use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::tool::schema::{
    bounded_integer_property, closed_object, parse_args, path_property, string_property,
};
use crate::tool::{PathEvidence, ProjectPathResolver, Tool, ToolOutput, ToolPathError, search};

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    recheck: bool,
}

fn default_limit() -> usize {
    100
}

pub struct GlobTool {
    project_root: PathBuf,
    path_evidence: Option<PathEvidence>,
}

impl GlobTool {
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            project_root,
            path_evidence: None,
        }
    }

    pub(crate) fn with_path_evidence(mut self, path_evidence: PathEvidence) -> Self {
        self.path_evidence = Some(path_evidence);
        self
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Fast file pattern matching tool that finds files by name and pattern. Supports ** for recursive matching. Results sorted by modification time (newest first)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "pattern",
                    string_property(
                        "Glob pattern to match files against (e.g., '*.rs', '**/*.md')",
                    ),
                ),
                (
                    "path",
                    path_property("Directory to search in (default: project root)"),
                ),
                (
                    "limit",
                    bounded_integer_property(
                        "Maximum number of results to return (default: 100)",
                        Some(1),
                        None,
                    ),
                ),
                (
                    "recheck",
                    crate::tool::schema::boolean_property(
                        "Bypass cached missing-path evidence and check the filesystem again",
                    ),
                ),
            ],
            &["pattern"],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: GlobArgs = parse_args("glob tool", args)?;

        search::ensure_pattern_present(&args.pattern)?;
        search::ensure_limit_nonzero(args.limit)?;

        let mut resolver = ProjectPathResolver::new(&self.project_root)
            .action("search")
            .recheck(args.recheck);
        if let Some(evidence) = self.path_evidence.as_ref() {
            resolver = resolver.path_evidence(evidence);
        }
        let search_path = match resolver.resolve_existing_or_project_root(args.path.as_deref()) {
            Err(ToolPathError::ReusedMissingPath { evidence }) => {
                return Ok(ToolOutput::MissingPathReuse {
                    text: evidence.render_reuse(),
                });
            }
            result => result?,
        };

        let respect_gitignore = search::respect_gitignore("BONSAI_GLOB_RESPECT_GITIGNORE");

        let pattern = search::compile_glob(Some(&args.pattern), None)?
            .expect("compile_glob returns Some when the pattern is Some");

        // Owned copies moved into the blocking closure so the walkdir + metadata
        // work (CPU/IO-bound) runs off the async runtime thread.
        let (walk_root, display_base, glob_base, gitignore) =
            search::walk_context(&search_path, respect_gitignore);

        let mut matches: Vec<(PathBuf, SystemTime)> = tokio::task::spawn_blocking(move || {
            let mut matches: Vec<(PathBuf, SystemTime)> = Vec::new();
            for file in search::walk_project_files(
                &walk_root,
                &display_base,
                gitignore.as_ref(),
                respect_gitignore,
                &display_base,
            ) {
                let glob_path = file
                    .absolute
                    .strip_prefix(&glob_base)
                    .unwrap_or(file.relative.as_path());
                if search::is_hidden_or_gitignored(
                    &file.relative,
                    gitignore.as_ref(),
                    respect_gitignore,
                ) {
                    continue;
                }

                if pattern.matches_path(glob_path) {
                    let mod_time = std::fs::metadata(&file.absolute)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .unwrap_or(SystemTime::UNIX_EPOCH);
                    matches.push((file.relative, mod_time));
                }
            }
            matches
        })
        .await?;

        matches.sort_by_key(|b| std::cmp::Reverse(b.1));

        let total = matches.len();
        let truncated = total > args.limit;
        if truncated {
            matches.truncate(args.limit);
        }

        if matches.is_empty() {
            return Ok(ToolOutput::Text("No files found".to_string()));
        }

        let mut result = matches
            .iter()
            .map(|(p, _)| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n");

        if truncated {
            result.push_str(&search::format_truncation(args.limit, Some(total)));
        }

        Ok(ToolOutput::Text(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_utils::TestFixture;
    use serde_json::json;

    #[tokio::test]
    async fn test_glob_simple_match() {
        let fixture = TestFixture::new();
        fixture.create_file("src/main.rs", "fn main() {}\n");
        fixture.create_file("src/lib.rs", "pub fn lib() {}\n");

        let tool = GlobTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({
                "pattern": "**/*.rs"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("src/main.rs"), "output: {output}");
        assert!(output.contains("src/lib.rs"), "output: {output}");
    }

    #[tokio::test]
    async fn test_glob_rejects_zero_limit() {
        let fixture = TestFixture::new();
        let tool = GlobTool::new(fixture.project_root.clone());

        let result = tool.execute(json!({"pattern": "**/*", "limit": 0})).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("limit"));
    }

    #[tokio::test]
    async fn test_glob_no_files_found() {
        let fixture = TestFixture::new();

        let tool = GlobTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({
                "pattern": "**/*.md"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert_eq!(output, "No files found", "output: {output}");
    }

    #[tokio::test]
    async fn test_glob_truncates_results() {
        let fixture = TestFixture::new();
        fixture.create_file("a.txt", "a");
        fixture.create_file("b.txt", "b");
        fixture.create_file("c.txt", "c");

        let tool = GlobTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({
                "pattern": "*.txt",
                "limit": 2
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(
            output.contains("[Truncated: showing 2 of 3 matches"),
            "output: {output}"
        );
    }

    #[tokio::test]
    async fn test_glob_skips_hidden_files() {
        let fixture = TestFixture::new();
        fixture.create_file(".hidden.txt", "secret");
        fixture.create_file("visible.txt", "visible");

        let tool = GlobTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({
                "pattern": "*.txt"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("visible.txt"), "output: {output}");
        assert!(!output.contains(".hidden.txt"), "output: {output}");
    }

    #[tokio::test]
    async fn test_glob_respects_gitignore() {
        let fixture = TestFixture::new();
        fixture.create_gitignore(&["ignored.txt"]);
        fixture.create_file("ignored.txt", "ignored");
        fixture.create_file("kept.txt", "kept");

        let tool = GlobTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({
                "pattern": "*.txt"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("kept.txt"), "output: {output}");
        assert!(!output.contains("ignored.txt"), "output: {output}");
    }

    #[tokio::test]
    async fn glob_scoped_path_respects_root_gitignore() {
        let fixture = TestFixture::new();
        fixture.create_gitignore(&["src/generated.rs"]);
        fixture.create_file("src/generated.rs", "ignored");
        fixture.create_file("src/kept.rs", "kept");

        let tool = GlobTool::new(fixture.project_root.clone());
        let output = match tool
            .execute(json!({
                "pattern": "*.rs",
                "path": "src"
            }))
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
    async fn test_glob_invalid_pattern() {
        let fixture = TestFixture::new();

        let tool = GlobTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({
                "pattern": "["
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Invalid glob pattern"));
    }

    #[tokio::test]
    async fn test_glob_rejects_outside_project_root() {
        let fixture = TestFixture::new();

        let tool = GlobTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({
                "pattern": "*.txt",
                "path": ".."
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Cannot search outside project root"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_glob_does_not_follow_symlinked_directories() {
        let fixture = TestFixture::new();
        let outside_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(outside_dir.path().join("escape.txt"), "escape").unwrap();

        let linked_dir = fixture.project_root.join("linked");
        std::os::unix::fs::symlink(outside_dir.path(), &linked_dir).unwrap();

        let tool = GlobTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({
                "pattern": "**/*.txt"
            }))
            .await
            .unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(!output.contains("escape.txt"), "output: {output}");
    }
}
