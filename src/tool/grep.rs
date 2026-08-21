use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use serde::Deserialize;

use crate::tool::arg_repair::{RepairAction, RepairNote};
use crate::tool::schema::{
    boolean_property, bounded_integer_property, closed_object, parse_args, path_property,
    string_enum_property, string_property,
};
use crate::tool::{
    ExistingProjectPath, PathEvidence, ProjectPathResolver, ReadTracker, Tool, ToolOutput,
    ToolPathError, output, search,
};

/// Default cap on grep's joined model-visible output, so one broad content
/// search can't flood the context window. Overridable per call via `max_chars`.
const MAX_GREP_OUTPUT_CHARS: usize = 30_000;

/// Upper bound on requested `context_lines`, keeping per-match context bounded.
const MAX_CONTEXT_LINES: usize = 20;

/// File types accepted by the `type` filter, paired with their extension.
const TYPE_EXTENSIONS: &[(&str, &str)] = &[
    ("rust", "rs"),
    ("python", "py"),
    ("javascript", "js"),
    ("typescript", "ts"),
    ("json", "json"),
    ("toml", "toml"),
    ("markdown", "md"),
    ("text", "txt"),
];

/// What grep returns, selected by the `mode` argument.
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
enum GrepMode {
    /// Default: one line per file that contains a match.
    #[default]
    FilesWithMatches,
    /// Matching lines prefixed with file and line number.
    Content,
    /// Match-line count per file.
    Count,
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default, rename = "type")]
    type_: Option<String>,
    #[serde(default)]
    mode: GrepMode,
    #[serde(default)]
    multiline: bool,
    #[serde(default = "default_limit")]
    limit: usize,
    /// Lines of surrounding context to include before/after each match
    /// (content mode only). Default 0.
    #[serde(default)]
    context_lines: usize,
    /// Cap on the joined output in characters. Default [`MAX_GREP_OUTPUT_CHARS`].
    #[serde(default = "default_max_chars")]
    max_chars: usize,
    #[serde(default)]
    recheck: bool,
}

fn default_limit() -> usize {
    100
}

fn default_max_chars() -> usize {
    MAX_GREP_OUTPUT_CHARS
}

/// String-valued grep argument aliases from Claude Code's Grep tool (and the
/// ripgrep-flavored schemas other harnesses import), mapped onto bonsai's
/// canonical field names. Models trained on those harnesses send these names
/// with identical semantics — `output_mode` carries bonsai's `mode` enum
/// values, `include` carries a `glob`, and `head_limit` carries `limit` (often
/// encoded as a numeric string). Without the mapping the call is bounced with a
/// rejected-fields error, wasting the turn and feeding the completion report's
/// failure evidence — and a model that keeps resending the same alias shape
/// trips the repeated-failure loop signal (see
/// `completion_report::classify_completion_status`). `output_mode` and
/// `include` were observed during live SWE-bench canary runs (mimo);
/// `head_limit` appeared in persisted coding sessions. Deliberate
/// coerce-or-guide accommodation of a cross-harness naming split — do not
/// "simplify" away. Mirrors `coerce_line_range_aliases` in read.rs.
const GREP_ALIASES: &[(&str, &str)] = &[("output_mode", "mode"), ("include", "glob")];

fn coerce_grep_aliases(args: &mut serde_json::Value) -> Vec<RepairNote> {
    let Some(object) = args.as_object_mut() else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    for (alias, canonical) in GREP_ALIASES {
        let Some(value) = object.get(*alias).cloned() else {
            continue;
        };
        // A non-string is left in place for rejected-fields guidance rather
        // than silently mapped.
        if !value.is_string() {
            continue;
        }
        object.remove(*alias);
        // The canonical field wins when both are present; the redundant alias
        // is still dropped so it can't bounce an otherwise-correct call.
        if !object.contains_key(*canonical) {
            object.insert((*canonical).to_string(), value);
        }
        notes.push(RepairNote {
            field: (*alias).to_string(),
            action: RepairAction::MappedAliasField,
        });
    }
    let head_limit = object.get("head_limit").and_then(|value| {
        value.as_u64().or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<u64>().ok())
        })
    });
    if let Some(head_limit) = head_limit {
        object.remove("head_limit");
        if !object.contains_key("limit") {
            object.insert("limit".to_string(), head_limit.into());
        }
        notes.push(RepairNote {
            field: "head_limit".to_string(),
            action: RepairAction::MappedAliasField,
        });
    }
    notes
}

pub struct GrepTool {
    project_root: PathBuf,
    read_tracker: ReadTracker,
    path_evidence: Option<PathEvidence>,
}

impl GrepTool {
    pub fn new(project_root: PathBuf, read_tracker: ReadTracker) -> Self {
        Self {
            project_root,
            read_tracker,
            path_evidence: None,
        }
    }

    pub(crate) fn with_path_evidence(mut self, path_evidence: PathEvidence) -> Self {
        self.path_evidence = Some(path_evidence);
        self
    }

    fn matches_type(path: &std::path::Path, type_: &Option<String>) -> bool {
        let Some(type_) = type_.as_deref() else {
            return true;
        };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        TYPE_EXTENSIONS
            .iter()
            .any(|(name, expected_ext)| *name == type_ && *expected_ext == ext)
    }

    fn should_skip(
        gitignore_path: &std::path::Path,
        glob_path: &std::path::Path,
        gitignore: Option<&ignore::gitignore::Gitignore>,
        respect_gitignore: bool,
        glob: &Option<glob::Pattern>,
        type_: &Option<String>,
    ) -> bool {
        if search::is_hidden_or_gitignored(gitignore_path, gitignore, respect_gitignore) {
            return true;
        }
        if let Some(pattern) = glob
            && !pattern.matches_path(glob_path)
        {
            return true;
        }
        if !Self::matches_type(glob_path, type_) {
            return true;
        }
        false
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with regex. Supports files_with_matches, content, count, glob scoping, and optional multiline matching."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                ("pattern", string_property("Regex pattern to search for")),
                (
                    "path",
                    path_property(
                        "File or directory to search in (default: project root). Several space-separated paths search them all, e.g. 'src tests'.",
                    ),
                ),
                (
                    "glob",
                    string_property("Optional glob to restrict searched files"),
                ),
                (
                    "type",
                    string_enum_property(
                        "Optional file type filter",
                        &TYPE_EXTENSIONS
                            .iter()
                            .map(|(name, _)| *name)
                            .collect::<Vec<_>>(),
                    ),
                ),
                (
                    "mode",
                    string_enum_property(
                        "Output mode (default: files_with_matches). files_with_matches returns file paths, content returns matching lines with file and line number, count returns match counts per file.",
                        &["files_with_matches", "content", "count"],
                    ),
                ),
                (
                    "multiline",
                    boolean_property("Enable multiline regex matching"),
                ),
                (
                    "limit",
                    bounded_integer_property(
                        "Maximum number of matched files to return",
                        Some(1),
                        None,
                    ),
                ),
                (
                    "context_lines",
                    bounded_integer_property(
                        "Lines of context to show before and after each match (content mode only)",
                        Some(0),
                        Some(MAX_CONTEXT_LINES as i64),
                    ),
                ),
                (
                    "max_chars",
                    bounded_integer_property(
                        "Cap on total output characters (default: 30000)",
                        Some(1),
                        None,
                    ),
                ),
                (
                    "recheck",
                    boolean_property(
                        "Bypass cached missing-path evidence and check the filesystem again",
                    ),
                ),
            ],
            &["pattern"],
        )
    }

    fn coerce_arguments(&self, args: &mut serde_json::Value) -> Vec<RepairNote> {
        coerce_grep_aliases(args)
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: GrepArgs = parse_args("grep tool", args)?;
        search::ensure_pattern_present(&args.pattern)?;
        search::ensure_limit_nonzero(args.limit)?;

        let resolved_roots = resolve_search_roots(
            &self.project_root,
            self.path_evidence.as_ref(),
            args.path.as_deref(),
            args.recheck,
        )?;
        if resolved_roots.roots.is_empty()
            && let Some(reused) = resolved_roots.reused_missing_paths.first()
        {
            return Ok(ToolOutput::MissingPathReuse {
                text: reused.render_reuse(),
            });
        }
        let skipped_missing_paths = resolved_roots.skipped_missing_paths;
        let reused_missing_paths = resolved_roots.reused_missing_paths;

        let respect_gitignore = search::respect_gitignore("BONSAI_GREP_RESPECT_GITIGNORE");
        let glob_pattern = search::compile_glob(args.glob.as_deref(), None)?;
        let matcher = RegexMatcherBuilder::new()
            .multi_line(args.multiline)
            .build(&args.pattern)
            .with_context(|| format!("Invalid regex pattern: {}", args.pattern))?;

        // Owned copies moved into the blocking closure so the walkdir + grep
        // search loop (CPU/IO-bound) runs off the async runtime thread. Each
        // requested root carries its own walk base, relative-display base, and
        // gitignore set.
        let walk_roots: Vec<(
            PathBuf,
            PathBuf,
            PathBuf,
            Option<ignore::gitignore::Gitignore>,
        )> = resolved_roots
            .roots
            .iter()
            .map(|root| search::walk_context(root, respect_gitignore))
            .collect();
        let type_ = args.type_.clone();
        let mode = args.mode;
        let multiline = args.multiline;
        let limit = args.limit;
        let context_lines = args.context_lines.min(MAX_CONTEXT_LINES);

        // `read_paths` collects the absolute path of every file whose content was
        // returned, so content-mode matches can satisfy read-before-edit.
        let (matches, truncated, read_paths): (Vec<String>, bool, Vec<PathBuf>) =
            tokio::task::spawn_blocking(move || {
                let mut searcher = SearcherBuilder::new()
                    .line_number(true)
                    .multi_line(multiline)
                    .before_context(context_lines)
                    .after_context(context_lines)
                    .build();

                let mut matches: Vec<String> = Vec::new();
                let mut read_paths: Vec<PathBuf> = Vec::new();
                let mut truncated = false;
                'roots: for (walk_root, display_base, glob_base, gitignore) in &walk_roots {
                    for file in search::walk_project_files(
                        walk_root,
                        display_base,
                        gitignore.as_ref(),
                        respect_gitignore,
                        display_base,
                    ) {
                        let relative_path = file.relative;
                        let glob_path = file
                            .absolute
                            .strip_prefix(glob_base)
                            .unwrap_or(relative_path.as_path());
                        if GrepTool::should_skip(
                            &relative_path,
                            glob_path,
                            gitignore.as_ref(),
                            respect_gitignore,
                            &glob_pattern,
                            &type_,
                        ) {
                            continue;
                        }

                        let mut file_result = String::new();
                        let mut count = 0usize;
                        let matched = searcher.search_path(
                            &matcher,
                            &file.absolute,
                            GrepContentSink {
                                mode,
                                relative_path: &relative_path,
                                out: &mut file_result,
                                match_count: &mut count,
                            },
                        );

                        if matched.is_err() {
                            continue;
                        }
                        if count == 0 {
                            continue;
                        }
                        if matches.len() >= limit {
                            truncated = true;
                            break 'roots;
                        }

                        match mode {
                            GrepMode::Count => {
                                matches.push(format!("{}:{}", relative_path.display(), count));
                            }
                            GrepMode::Content => {
                                let trimmed_len = file_result.trim_end().len();
                                file_result.truncate(trimmed_len);
                                matches.push(file_result);
                                read_paths.push(file.absolute);
                            }
                            GrepMode::FilesWithMatches => {
                                matches.push(relative_path.display().to_string());
                            }
                        }
                    }
                }
                (matches, truncated, read_paths)
            })
            .await?;

        // Only content mode returns file contents, so only it can satisfy
        // read-before-edit; file-name/count modes return no content. Coverage is
        // *partial* — grep shows matching lines and context, not the whole file —
        // so an edit (content-addressed) is allowed but a whole-file write is not
        // (P4), matching bash `grep` read-tracking.
        if mode == GrepMode::Content {
            for path in &read_paths {
                self.read_tracker.mark_read_partial(path).await;
            }
        }

        if matches.is_empty() {
            let mut result = "No matches found".to_string();
            append_skipped_missing_paths(&mut result, &skipped_missing_paths);
            append_reused_missing_paths(&mut result, &reused_missing_paths);
            return Ok(ToolOutput::Text(result));
        }
        let body = matches.join("\n");
        let mut result = output::cap_text(
            body,
            args.max_chars.max(1),
            "Narrow the pattern, lower limit, or add a path/glob filter.",
        );
        if truncated {
            result.push_str(&search::format_truncation(args.limit, None));
        }
        append_skipped_missing_paths(&mut result, &skipped_missing_paths);
        append_reused_missing_paths(&mut result, &reused_missing_paths);
        // Content mode is the only path that returns file *contents* — text the
        // agent did not deliberately open, swept from across the tree. Frame it
        // as untrusted data (P3) so the "not instructions" boundary rides along
        // with the payload and survives compaction, not just the standing system
        // prompt. Count / files-with-matches return paths and counts, not
        // content, so they stay plain text.
        if mode == GrepMode::Content {
            return Ok(ToolOutput::untrusted_context(
                format!("grep pattern={}", args.pattern),
                &result,
            ));
        }
        Ok(ToolOutput::Text(result))
    }
}

#[derive(Debug)]
struct ResolvedSearchRoots {
    roots: Vec<ExistingProjectPath>,
    skipped_missing_paths: Vec<String>,
    reused_missing_paths: Vec<crate::tool::MissingPathEvidence>,
}

/// Resolve the `path` argument into one or more existing search roots.
///
/// Accepts the usual single path, but also tolerates several whitespace-separated
/// paths in one string — the muscle memory from CLI ripgrep (`rg pattern src tests`).
/// The whole string is tried as a single path first, so a real path containing
/// spaces still works. If that fails, every existing whitespace-separated token
/// becomes a search root and missing tokens are reported in the tool output. If
/// no token resolves, the original single-path error is surfaced.
fn resolve_search_roots(
    project_root: &Path,
    path_evidence: Option<&PathEvidence>,
    raw: Option<&str>,
    recheck: bool,
) -> Result<ResolvedSearchRoots> {
    let resolver = || {
        let resolver = ProjectPathResolver::new(project_root)
            .action("search")
            .recheck(recheck);
        path_evidence
            .map(|evidence| resolver.path_evidence(evidence))
            .unwrap_or(resolver)
    };

    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(ResolvedSearchRoots {
            roots: vec![resolver().resolve_existing_or_project_root(None)?],
            skipped_missing_paths: Vec::new(),
            reused_missing_paths: Vec::new(),
        });
    };

    let single_err = match resolver().resolve_existing(raw) {
        Ok(path) => {
            return Ok(ResolvedSearchRoots {
                roots: vec![path],
                skipped_missing_paths: Vec::new(),
                reused_missing_paths: Vec::new(),
            });
        }
        Err(err) => err,
    };

    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.len() > 1 {
        let mut roots = Vec::with_capacity(tokens.len());
        let mut skipped_missing_paths = Vec::new();
        let mut reused_missing_paths = Vec::new();
        for token in tokens {
            match resolver().resolve_existing(token) {
                Ok(path) => roots.push(path),
                Err(ToolPathError::ReusedMissingPath { evidence }) => {
                    reused_missing_paths.push(evidence);
                }
                Err(_) => skipped_missing_paths.push(token.to_string()),
            }
        }
        if !roots.is_empty() || !reused_missing_paths.is_empty() {
            return Ok(ResolvedSearchRoots {
                roots,
                skipped_missing_paths,
                reused_missing_paths,
            });
        }
    }

    Err(single_err.into())
}

fn append_reused_missing_paths(
    result: &mut String,
    reused_missing_paths: &[crate::tool::MissingPathEvidence],
) {
    if reused_missing_paths.is_empty() {
        return;
    }
    let paths = reused_missing_paths
        .iter()
        .map(|evidence| evidence.project_relative_path().display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!(
        "[reused missing-path evidence] Skipped missing search roots: {paths}. Pass recheck: true after creating or restoring them."
    );
    let _ = write!(
        result,
        "\n\n{}",
        crate::tool::wrap_untrusted_content("project path evidence", &body)
    );
}

fn append_skipped_missing_paths(result: &mut String, skipped_missing_paths: &[String]) {
    if skipped_missing_paths.is_empty() {
        return;
    }
    let suffix = if skipped_missing_paths.len() == 1 {
        ""
    } else {
        "s"
    };
    let _ = write!(
        result,
        "\n\nSkipped missing search path{suffix}: {}",
        skipped_missing_paths.join(", ")
    );
}

/// Per-file sink for the grep search loop. Counts matches (so files with none
/// are skipped) and, in content mode, renders matched lines as `path:line:text`
/// and context lines as `path-line-text` — the `:` vs `-` separator marks which
/// is which, mirroring ripgrep.
struct GrepContentSink<'a> {
    mode: GrepMode,
    relative_path: &'a Path,
    out: &'a mut String,
    match_count: &'a mut usize,
}

impl Sink for GrepContentSink<'_> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        *self.match_count += 1;
        if self.mode == GrepMode::Content {
            let line_num = mat.line_number().unwrap_or(0);
            // `bytes()` already carries the line terminator.
            let text = String::from_utf8_lossy(mat.bytes());
            let _ = write!(
                self.out,
                "{}:{}:{}",
                self.relative_path.display(),
                line_num,
                text
            );
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, std::io::Error> {
        if self.mode == GrepMode::Content {
            let line_num = ctx.line_number().unwrap_or(0);
            let text = String::from_utf8_lossy(ctx.bytes());
            let _ = write!(
                self.out,
                "{}-{}-{}",
                self.relative_path.display(),
                line_num,
                text
            );
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_utils::TestFixture;
    use serde_json::json;

    /// Model-visible body of a grep result. Content mode returns the payload
    /// framed as untrusted (P3); every other mode returns plain text. Both carry
    /// the match lines verbatim, so `contains` assertions read the same body
    /// either way.
    fn grep_body(output: ToolOutput) -> String {
        match output {
            ToolOutput::Text(text) => text,
            ToolOutput::UntrustedContext { content, .. } => content,
            other => panic!("Expected grep text/untrusted output, got {other:?}"),
        }
    }

    #[test]
    fn output_mode_alias_maps_onto_mode() {
        let mut args = json!({"pattern": "needle", "output_mode": "content"});
        let notes = coerce_grep_aliases(&mut args);
        assert_eq!(notes.len(), 1);
        assert_eq!(args, json!({"pattern": "needle", "mode": "content"}));
    }

    #[test]
    fn include_alias_maps_onto_glob() {
        let mut args = json!({"pattern": "needle", "include": "*.py"});
        let notes = coerce_grep_aliases(&mut args);
        assert_eq!(notes.len(), 1);
        assert_eq!(args, json!({"pattern": "needle", "glob": "*.py"}));
    }

    #[test]
    fn both_grep_aliases_map_together() {
        let mut args = json!({"pattern": "n", "output_mode": "content", "include": "*.rs"});
        let notes = coerce_grep_aliases(&mut args);
        assert_eq!(notes.len(), 2);
        assert_eq!(
            args,
            json!({"pattern": "n", "mode": "content", "glob": "*.rs"})
        );
    }

    #[test]
    fn head_limit_alias_maps_numeric_string_onto_limit() {
        let mut args = json!({"pattern": "needle", "head_limit": "25"});
        let notes = coerce_grep_aliases(&mut args);
        assert_eq!(notes.len(), 1);
        assert_eq!(args, json!({"pattern": "needle", "limit": 25}));
    }

    #[test]
    fn canonical_grep_fields_win_over_aliases() {
        // Canonical fields win; the redundant aliases are dropped so they can't
        // bounce an otherwise-correct call on rejected-fields.
        let mut args = json!({"pattern": "n", "mode": "count", "output_mode": "content", "glob": "*.rs", "include": "*.py", "limit": 7, "head_limit": "25"});
        coerce_grep_aliases(&mut args);
        assert_eq!(
            args,
            json!({"pattern": "n", "mode": "count", "glob": "*.rs", "limit": 7})
        );
    }

    #[test]
    fn non_string_grep_alias_is_left_for_guidance() {
        let mut args = json!({"pattern": "n", "output_mode": 3});
        let notes = coerce_grep_aliases(&mut args);
        assert!(notes.is_empty());
        assert_eq!(args, json!({"pattern": "n", "output_mode": 3}));
    }

    #[tokio::test]
    async fn test_grep_returns_matches_with_line_numbers() {
        let fixture = TestFixture::new();
        fixture.create_file("src/main.rs", "fn main() { println!(\"hello\"); }\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({"pattern": "hello", "mode": "content"}))
            .await
            .unwrap();
        let output = grep_body(result);
        assert!(output.contains("src/main.rs:1:"), "output: {output}");
    }

    #[tokio::test]
    async fn test_grep_content_mode_outputs_line_numbers() {
        let fixture = TestFixture::new();
        fixture.create_file("src/main.rs", "fn main() { println!(\"hello\"); }\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let result = tool
            .execute(json!({"pattern": "hello", "mode": "content"}))
            .await
            .unwrap();

        let output = grep_body(result);
        assert!(output.contains("src/main.rs:1:"), "output: {output}");
    }

    #[tokio::test]
    async fn grep_content_mode_frames_results_as_untrusted() {
        // P3: content-mode results carry file text swept from the tree, so they
        // are framed as untrusted data with the boundary embedded in the payload
        // (survives compaction), while the match lines remain intact.
        let fixture = TestFixture::new();
        fixture.create_file("src/main.rs", "fn main() { needle(); }\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let result = tool
            .execute(json!({"pattern": "needle", "mode": "content"}))
            .await
            .unwrap();

        let ToolOutput::UntrustedContext { content, .. } = &result else {
            panic!("content mode must frame results as untrusted, got {result:?}");
        };
        assert!(
            content.contains("UNTRUSTED external data"),
            "missing untrusted frame: {content}"
        );
        assert!(
            content.contains("src/main.rs:1:"),
            "match line lost inside frame: {content}"
        );
    }

    #[tokio::test]
    async fn grep_non_content_modes_stay_plain_text() {
        // Count / files-with-matches return paths and counts, not file content,
        // so they must not be wrapped in the untrusted frame.
        let fixture = TestFixture::new();
        fixture.create_file("a.txt", "needle\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        for mode in ["files_with_matches", "count"] {
            let result = tool
                .execute(json!({"pattern": "needle", "mode": mode}))
                .await
                .unwrap();
            assert!(
                matches!(result, ToolOutput::Text(_)),
                "{mode} mode should stay plain text, got {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_grep_rejects_zero_limit() {
        let fixture = TestFixture::new();
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let result = tool.execute(json!({"pattern": "hello", "limit": 0})).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("limit"));
    }

    #[tokio::test]
    async fn grep_schema_bounds_limit_and_is_closed() {
        let fixture = TestFixture::new();
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let schema = tool.parameters_schema();

        assert_eq!(
            schema["additionalProperties"],
            serde_json::Value::Bool(false)
        );
        assert_eq!(schema["properties"]["limit"]["minimum"], 1);
        assert_eq!(schema["properties"]["context_lines"]["minimum"], 0);
        assert_eq!(schema["properties"]["max_chars"]["minimum"], 1);
    }

    #[tokio::test]
    async fn grep_content_mode_marks_file_read() {
        let fixture = TestFixture::new();
        let file = fixture.create_file("src/main.rs", "fn main() { needle(); }\n");
        let canonical = file.canonicalize().unwrap();
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        tool.execute(json!({"pattern": "needle", "mode": "content"}))
            .await
            .unwrap();

        assert!(fixture.read_tracker.is_read(&canonical).await);
    }

    #[tokio::test]
    async fn grep_content_mode_marks_only_partial_coverage() {
        // P4: grep shows matching lines, not the whole file, so it satisfies
        // read-before-edit (is_read) but must not satisfy the whole-file write
        // guard (was_fully_read stays false).
        let fixture = TestFixture::new();
        let file = fixture.create_file("src/main.rs", "fn main() { needle(); }\n");
        let canonical = file.canonicalize().unwrap();
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        tool.execute(json!({"pattern": "needle", "mode": "content"}))
            .await
            .unwrap();

        assert!(fixture.read_tracker.is_read(&canonical).await);
        assert!(
            !fixture.read_tracker.was_fully_read(&canonical).await,
            "grep content is a partial view, not full coverage"
        );
    }

    #[tokio::test]
    async fn grep_non_content_modes_do_not_mark_read() {
        let fixture = TestFixture::new();
        let file = fixture.create_file("src/main.rs", "fn main() { needle(); }\n");
        let canonical = file.canonicalize().unwrap();
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        tool.execute(json!({"pattern": "needle"})).await.unwrap(); // files_with_matches
        assert!(!fixture.read_tracker.is_read(&canonical).await);

        tool.execute(json!({"pattern": "needle", "mode": "count"}))
            .await
            .unwrap();
        assert!(!fixture.read_tracker.is_read(&canonical).await);
    }

    #[tokio::test]
    async fn grep_context_lines_include_surrounding_lines() {
        let fixture = TestFixture::new();
        fixture.create_file("a.txt", "alpha\nbeta\nGAMMA\ndelta\nepsilon\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let output = grep_body(
            tool.execute(json!({"pattern": "GAMMA", "mode": "content", "context_lines": 1}))
                .await
                .unwrap(),
        );

        // Match lines use ':'; context lines use '-'.
        assert!(output.contains("a.txt:3:GAMMA"), "{output}");
        assert!(output.contains("a.txt-2-beta"), "{output}");
        assert!(output.contains("a.txt-4-delta"), "{output}");
    }

    #[tokio::test]
    async fn grep_caps_output_with_footer() {
        let fixture = TestFixture::new();
        fixture.create_file("big.txt", &"needle\n".repeat(500));
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let output = grep_body(
            tool.execute(json!({"pattern": "needle", "mode": "content", "max_chars": 100}))
                .await
                .unwrap(),
        );

        assert!(output.contains("[Truncated:"), "{output}");
        assert!(output.contains("of"), "{output}");
        assert!(output.contains("chars"), "{output}");
    }

    #[tokio::test]
    async fn test_grep_count_mode() {
        let fixture = TestFixture::new();
        fixture.create_file("a.txt", "one two one\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({"pattern": "one", "mode": "count"}))
            .await
            .unwrap();
        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(output.contains("a.txt:1"), "output: {output}");
    }

    #[tokio::test]
    async fn test_grep_files_with_matches() {
        let fixture = TestFixture::new();
        fixture.create_file("a.txt", "one\n");
        fixture.create_file("b.txt", "two\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({"pattern": "one|two", "mode": "files_with_matches"}))
            .await
            .unwrap();
        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(output.contains("a.txt"), "output: {output}");
        assert!(output.contains("b.txt"), "output: {output}");
    }

    #[tokio::test]
    async fn test_grep_default_is_files_with_matches() {
        let fixture = TestFixture::new();
        fixture.create_file("a.txt", "one\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool.execute(json!({"pattern": "one"})).await.unwrap();
        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert_eq!(output.trim(), "a.txt");
    }

    #[tokio::test]
    async fn test_grep_content_mode() {
        let fixture = TestFixture::new();
        fixture.create_file("a.txt", "one\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({"pattern": "one", "mode": "content"}))
            .await
            .unwrap();
        let output = grep_body(result);
        assert!(output.contains("a.txt:1:one"), "output: {output}");
    }

    #[tokio::test]
    async fn test_grep_mode_selects_output_shape() {
        let fixture = TestFixture::new();
        fixture.create_file("a.txt", "one\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        // Default mode -> file paths only.
        let default_out = match tool.execute(json!({"pattern": "one"})).await.unwrap() {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert_eq!(default_out.trim(), "a.txt", "output: {default_out}");

        // count -> "<path>:<match-line-count>".
        let count_out = match tool
            .execute(json!({"pattern": "one", "mode": "count"}))
            .await
            .unwrap()
        {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert_eq!(count_out.trim(), "a.txt:1", "output: {count_out}");

        // content -> "<path>:<line>:<text>".
        let content_out = grep_body(
            tool.execute(json!({"pattern": "one", "mode": "content"}))
                .await
                .unwrap(),
        );
        assert!(content_out.contains("a.txt:1:one"), "output: {content_out}");
    }

    #[tokio::test]
    async fn test_grep_type_filter() {
        let fixture = TestFixture::new();
        fixture.create_file("keep.rs", "needle\n");
        fixture.create_file("skip.txt", "needle\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({"pattern": "needle", "type": "rust"}))
            .await
            .unwrap();
        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(output.contains("keep.rs"), "output: {output}");
        assert!(!output.contains("skip.txt"), "output: {output}");
    }

    #[tokio::test]
    async fn test_grep_multiline_matches_across_lines() {
        let fixture = TestFixture::new();
        fixture.create_file("multi.txt", "foo\nbar\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        // With multiline enabled the pattern can span the two lines.
        let multi = tool
            .execute(json!({"pattern": "foo\\nbar", "mode": "content", "multiline": true}))
            .await
            .unwrap();
        let multi = grep_body(multi);
        assert!(multi.contains("multi.txt"), "output: {multi}");
    }

    #[tokio::test]
    async fn test_grep_respects_gitignore_and_glob() {
        let fixture = TestFixture::new();
        fixture.create_gitignore(&["ignored.txt"]);
        fixture.create_file("ignored.txt", "needle\n");
        fixture.create_file("kept.rs", "needle\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({"pattern": "needle", "glob": "**/*.rs"}))
            .await
            .unwrap();
        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(output.contains("kept.rs"), "output: {output}");
        assert!(!output.contains("ignored.txt"), "output: {output}");
    }

    #[tokio::test]
    async fn grep_scoped_path_respects_root_gitignore() {
        let fixture = TestFixture::new();
        fixture.create_gitignore(&["src/generated.rs"]);
        fixture.create_file("src/generated.rs", "needle\n");
        fixture.create_file("src/kept.rs", "needle\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let output = match tool
            .execute(json!({"pattern": "needle", "path": "src", "glob": "*.rs"}))
            .await
            .unwrap()
        {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("src/kept.rs"), "output: {output}");
        assert!(!output.contains("generated.rs"), "output: {output}");
    }

    #[tokio::test]
    async fn test_grep_invalid_regex() {
        let fixture = TestFixture::new();
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool.execute(json!({"pattern": "["})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_grep_rejects_outside_project_root() {
        let fixture = TestFixture::new();
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool.execute(json!({"pattern": "x", "path": ".."})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn grep_searches_multiple_space_separated_paths() {
        let fixture = TestFixture::new();
        fixture.create_file("src/a.rs", "needle here\n");
        fixture.create_file("lib/b.rs", "needle there\n");
        fixture.create_file("docs/c.md", "needle elsewhere\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        // Mirrors `rg needle src lib`: both requested roots are searched, the
        // unrequested docs/ tree is not.
        let output = match tool
            .execute(json!({"pattern": "needle", "path": "src lib"}))
            .await
            .unwrap()
        {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(output.contains("src/a.rs"), "output: {output}");
        assert!(output.contains("lib/b.rs"), "output: {output}");
        assert!(!output.contains("c.md"), "output: {output}");
    }

    #[tokio::test]
    async fn grep_multi_path_with_a_missing_token_searches_existing_paths() {
        let fixture = TestFixture::new();
        fixture.create_file("src/a.rs", "needle\n");
        fixture.create_file("docs/c.md", "needle elsewhere\n");
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        // Models sometimes include a stale path in an otherwise useful path
        // list. Search the existing roots and report what was skipped.
        let output = match tool
            .execute(json!({"pattern": "needle", "path": "src nope"}))
            .await
            .unwrap()
        {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(output.contains("src/a.rs"), "output: {output}");
        assert!(!output.contains("docs/c.md"), "output: {output}");
        assert!(
            output.contains("Skipped missing search path: nope"),
            "output: {output}"
        );
    }

    #[tokio::test]
    async fn grep_single_missing_path_still_errors_clearly() {
        let fixture = TestFixture::new();
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let result = tool
            .execute(json!({"pattern": "x", "path": "does/not/exist"}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn grep_mixed_roots_keeps_results_when_missing_evidence_is_reused() {
        let fixture = TestFixture::new();
        fixture.create_file("src/a.rs", "needle\n");
        let evidence = crate::tool::PathEvidence::new(&fixture.project_root).unwrap();
        let tool = GrepTool::new(fixture.project_root.clone(), fixture.read_tracker.clone())
            .with_path_evidence(evidence);

        let first = tool
            .execute(json!({"pattern": "needle", "path": "src stale"}))
            .await
            .unwrap();
        assert!(grep_body(first).contains("Skipped missing search path: stale"));

        let second = tool
            .execute(json!({"pattern": "needle", "path": "src ./stale"}))
            .await
            .unwrap();
        let output = grep_body(second);
        assert!(output.contains("src/a.rs"), "output: {output}");
        assert!(
            output.contains("[reused missing-path evidence]"),
            "output: {output}"
        );
    }
}
