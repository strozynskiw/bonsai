use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::symbol::{
    SYMBOL_EXTENSIONS, SYMBOL_KINDS, Symbol, SymbolKind, extractor_for_path, parse_symbol_kind,
};
use crate::tool::digest_content;
use crate::tool::output::cap_text;
use crate::tool::schema::{
    bounded_integer_property, closed_object, parse_args, path_property, string_enum_property,
    string_property,
};
use crate::tool::{
    ProjectPathResolver, ReadCoverage, ReadEvidence, ReadTracker, ReadWindow, Tool, ToolOutput,
};

const MAX_REGION_LINES: usize = 1000;
const MAX_REGION_CHARS: usize = 30_000;
const LARGE_LINE_CHARS: usize = 4000;
const REGION_CAP_HINT: &str =
    "Use a narrower read_region range, read_symbol, grep, or symbol_search for targeted content.";
#[derive(Debug, Deserialize)]
struct ReadRegionArgs {
    path: String,
    start_line: usize,
    end_line: usize,
}

#[derive(Debug, Deserialize)]
struct ReadSymbolArgs {
    path: String,
    query: String,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct Region {
    start_line: usize,
    end_line: usize,
}

pub struct ReadRegionTool {
    project_root: PathBuf,
    read_tracker: ReadTracker,
}

impl ReadRegionTool {
    pub fn new(project_root: PathBuf, read_tracker: ReadTracker) -> Self {
        Self {
            project_root,
            read_tracker,
        }
    }
}

#[async_trait]
impl Tool for ReadRegionTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "read_region"
    }

    fn description(&self) -> &str {
        "Read an exact 1-based line range from a file. Use this after symbol_search when a full file read would be too large."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "path",
                    path_property("File path relative to project root, or absolute path inside it"),
                ),
                (
                    "start_line",
                    bounded_integer_property("First line to read, 1-based", Some(1), None),
                ),
                (
                    "end_line",
                    bounded_integer_property("Last line to read, inclusive", Some(1), None),
                ),
            ],
            &["path", "start_line", "end_line"],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: ReadRegionArgs = parse_args("read_region", args)?;
        let resolved = ProjectPathResolver::new(&self.project_root)
            .action("read files")
            .resolve_existing(&args.path)?;
        ensure_regular_file(resolved.canonical_path(), &args.path)?;
        read_file_region(
            resolved.canonical_path(),
            &args.path,
            Region {
                start_line: args.start_line,
                end_line: args.end_line,
            },
            &self.read_tracker,
            None,
        )
        .await
    }
}

pub struct ReadSymbolTool {
    project_root: PathBuf,
    read_tracker: ReadTracker,
}

impl ReadSymbolTool {
    pub fn new(project_root: PathBuf, read_tracker: ReadTracker) -> Self {
        Self {
            project_root,
            read_tracker,
        }
    }
}

#[async_trait]
impl Tool for ReadSymbolTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "read_symbol"
    }

    fn description(&self) -> &str {
        "Read the body/range for one Rust, TypeScript/JavaScript, Python, or Go symbol. Provide path plus an exact symbol name or signature fragment; use kind to disambiguate."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "path",
                    path_property(
                        "Source file path relative to project root, or absolute path inside it",
                    ),
                ),
                (
                    "query",
                    string_property("Exact symbol name or signature fragment to read"),
                ),
                (
                    "kind",
                    string_enum_property("Optional symbol kind filter", SYMBOL_KINDS),
                ),
            ],
            &["path", "query"],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: ReadSymbolArgs = parse_args("read_symbol", args)?;
        let resolved = ProjectPathResolver::new(&self.project_root)
            .action("read files")
            .resolve_existing(&args.path)?;
        ensure_regular_file(resolved.canonical_path(), &args.path)?;
        let kind_filter = parse_symbol_kind(args.kind.as_deref())?;
        let content = read_utf8_file(resolved.canonical_path(), &args.path).await?;
        let extractor = extractor_for_path(resolved.canonical_path()).with_context(|| {
            format!(
                "read_symbol does not support this file type; supported extensions: {}",
                SYMBOL_EXTENSIONS
                    .iter()
                    .map(|extension| format!(".{extension}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        let symbols = extractor.extract(resolved.canonical_path(), &content)?;
        let candidates = matching_symbols(&symbols, &args.query, kind_filter);
        let symbol = match select_symbol(&candidates, &args.query) {
            SymbolSelection::One(symbol) => symbol,
            SymbolSelection::None => {
                return Ok(ToolOutput::Text(format!(
                    "No symbol matching {:?} found in {}. Use symbol_search with path to list available symbols.",
                    args.query, args.path
                )));
            }
            SymbolSelection::Many(matches) => {
                return Ok(ToolOutput::Text(ambiguous_symbol_message(
                    &args.path, matches,
                )));
            }
        };

        read_file_region(
            resolved.canonical_path(),
            &args.path,
            Region {
                start_line: symbol.line,
                end_line: symbol.end_line,
            },
            &self.read_tracker,
            Some(symbol),
        )
        .await
    }
}

fn ensure_regular_file(path: &Path, display_path: &str) -> Result<()> {
    if path.is_dir() {
        anyhow::bail!("Path is a directory, not a file: {display_path}");
    }
    Ok(())
}

async fn read_utf8_file(path: &Path, display_path: &str) -> Result<String> {
    match fs::read_to_string(path).await {
        Ok(content) => Ok(content),
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
            anyhow::bail!(
                "File is not valid UTF-8: {display_path} (it appears to be binary or to use a non-UTF-8 text encoding)."
            );
        }
        Err(err) => Err(err).with_context(|| format!("Failed to read file: {display_path}")),
    }
}

async fn read_file_region(
    canonical_path: &Path,
    display_path: &str,
    region: Region,
    read_tracker: &ReadTracker,
    symbol: Option<&Symbol>,
) -> Result<ToolOutput> {
    if region.start_line == 0 {
        anyhow::bail!("start_line must be 1 or greater");
    }
    if region.end_line < region.start_line {
        anyhow::bail!("end_line must be greater than or equal to start_line");
    }

    let metadata = fs::metadata(canonical_path)
        .await
        .with_context(|| format!("Failed to read file: {display_path}"))?;
    let modified = metadata.modified().ok();
    let requested_line_count = region
        .end_line
        .saturating_sub(region.start_line)
        .saturating_add(1);
    let requested_was_line_capped = requested_line_count > MAX_REGION_LINES;
    let requested_end = region
        .end_line
        .min(region.start_line.saturating_add(MAX_REGION_LINES - 1));

    let mut text = String::new();
    if let Some(symbol) = symbol {
        let _ = writeln!(
            text,
            "[Symbol: {} {} lines {}-{}]",
            symbol.kind.label(),
            symbol.name,
            symbol.line,
            symbol.end_line
        );
    } else {
        let _ = writeln!(
            text,
            "[Region: {display_path} lines {}-{}]",
            region.start_line, region.end_line
        );
    }

    let file = fs::File::open(canonical_path)
        .await
        .with_context(|| format!("Failed to read file: {display_path}"))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut line_no = 0usize;
    let mut reached_eof = false;
    let mut visible_end_line = None;
    let mut line_truncated = false;
    let mut digest_bytes = (region.start_line == 1).then(Vec::new);

    while line_no < requested_end {
        line.clear();
        let bytes_read = read_region_line(&mut reader, &mut line, display_path).await?;
        if bytes_read == 0 {
            reached_eof = true;
            break;
        }
        line_no = line_no.saturating_add(1);
        push_digest_bytes(&mut digest_bytes, line.as_bytes());
        if line_no < region.start_line {
            continue;
        }

        let displayed = truncate_line(trim_line_ending(&line), LARGE_LINE_CHARS);
        if matches!(&displayed, std::borrow::Cow::Owned(_)) {
            line_truncated = true;
        }
        let _ = writeln!(text, "{}: {}", line_no, displayed);
        visible_end_line = Some(line_no);
    }

    if !reached_eof && !requested_was_line_capped {
        line.clear();
        let bytes_read = read_region_line(&mut reader, &mut line, display_path).await?;
        if bytes_read == 0 {
            reached_eof = true;
        } else {
            digest_bytes = None;
        }
    }

    let total_lines = reached_eof.then_some(line_no);
    if region.start_line > line_no
        && let Some(total_lines) = total_lines
    {
        let _ = writeln!(
            text,
            "[Region start line {} is past EOF. Total lines: {total_lines}.]",
            region.start_line
        );
    }

    let range_truncated = requested_was_line_capped && !reached_eof;
    if range_truncated {
        let _ = writeln!(
            text,
            "[Region truncated. Requested {} lines; showing at most {MAX_REGION_LINES}. Continue with start_line={}.]",
            requested_line_count,
            requested_end.saturating_add(1)
        );
    } else if let Some(total_lines) = total_lines
        && region.end_line > total_lines
    {
        let _ = writeln!(text, "[Region ended at EOF. Total lines: {total_lines}.]");
    }

    let char_truncated = text.chars().count() > MAX_REGION_CHARS;
    if char_truncated {
        text = cap_text(text, MAX_REGION_CHARS, REGION_CAP_HINT);
        visible_end_line = displayed_line_range(&text).map(|(_start, end)| end);
    }

    let full = region.start_line == 1
        && reached_eof
        && !range_truncated
        && !char_truncated
        && !line_truncated;
    let file_digest = if full {
        digest_bytes
            .filter(|bytes| bytes.len() as u64 == metadata.len())
            .map(|bytes| digest_content(&bytes))
    } else {
        None
    };
    if full {
        if let Some(file_digest) = file_digest {
            read_tracker
                .mark_read_with_file_digest(canonical_path, file_digest, true)
                .await;
        } else {
            read_tracker.mark_read(canonical_path).await;
        }
    } else {
        read_tracker.mark_read_partial(canonical_path).await;
    }

    let evidence = ReadEvidence::new(
        display_path.to_string(),
        canonical_path.to_path_buf(),
        ReadWindow {
            requested_offset: region.start_line,
            requested_limit: requested_line_count,
            start_line: region.start_line,
            end_line: visible_end_line,
            total_lines,
        },
        if full {
            ReadCoverage::Full
        } else {
            ReadCoverage::Partial
        },
        &text,
        modified,
        metadata.len(),
        file_digest,
    );

    Ok(ToolOutput::Read { text, evidence })
}

async fn read_region_line(
    reader: &mut BufReader<fs::File>,
    line: &mut String,
    display_path: &str,
) -> Result<usize> {
    match reader.read_line(line).await {
        Ok(bytes_read) => Ok(bytes_read),
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
            anyhow::bail!(
                "File is not valid UTF-8: {display_path} (it appears to be binary or to use a non-UTF-8 text encoding)."
            );
        }
        Err(err) => Err(err).with_context(|| format!("Failed to read file: {display_path}")),
    }
}

fn push_digest_bytes(digest_bytes: &mut Option<Vec<u8>>, bytes: &[u8]) {
    if let Some(buffer) = digest_bytes {
        if buffer.len().saturating_add(bytes.len()) <= MAX_REGION_CHARS {
            buffer.extend_from_slice(bytes);
        } else {
            *digest_bytes = None;
        }
    }
}

fn trim_line_ending(line: &str) -> &str {
    let without_newline = line.strip_suffix('\n').unwrap_or(line);
    without_newline
        .strip_suffix('\r')
        .unwrap_or(without_newline)
}

fn truncate_line(line: &str, max_chars: usize) -> std::borrow::Cow<'_, str> {
    match line.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => {
            std::borrow::Cow::Owned(format!("{}…[line truncated]", &line[..byte_idx]))
        }
        None => std::borrow::Cow::Borrowed(line),
    }
}

fn displayed_line_range(rendered: &str) -> Option<(usize, usize)> {
    let mut first = None;
    let mut last = None;
    for line in rendered.lines() {
        let Some((number, _rest)) = line.split_once(':') else {
            continue;
        };
        let Ok(line_no) = number.trim().parse::<usize>() else {
            continue;
        };
        first.get_or_insert(line_no);
        last = Some(line_no);
    }
    first.zip(last)
}

fn matching_symbols<'a>(
    symbols: &'a [Symbol],
    query: &str,
    kind_filter: Option<SymbolKind>,
) -> Vec<&'a Symbol> {
    symbols
        .iter()
        .filter(|symbol| kind_filter.is_none_or(|kind| symbol.kind == kind))
        .filter(|symbol| {
            symbol.name == query || symbol.name.contains(query) || symbol.signature.contains(query)
        })
        .collect()
}

enum SymbolSelection<'a> {
    One(&'a Symbol),
    Many(Vec<&'a Symbol>),
    None,
}

fn select_symbol<'a>(candidates: &[&'a Symbol], query: &str) -> SymbolSelection<'a> {
    if candidates.is_empty() {
        return SymbolSelection::None;
    }
    let exact = candidates
        .iter()
        .copied()
        .filter(|symbol| symbol.name == query)
        .collect::<Vec<_>>();
    let narrowed = if exact.is_empty() {
        candidates.to_vec()
    } else {
        exact
    };
    match narrowed.as_slice() {
        [symbol] => SymbolSelection::One(symbol),
        symbols => SymbolSelection::Many(symbols.to_vec()),
    }
}

fn ambiguous_symbol_message(path: &str, matches: Vec<&Symbol>) -> String {
    let mut out = format!("Multiple symbols matched in {path}; pass kind or a more exact query:\n");
    for symbol in matches.into_iter().take(20) {
        let _ = writeln!(
            out,
            "- {} {} lines {}-{}",
            symbol.kind.label(),
            symbol.name,
            symbol.line,
            symbol.end_line
        );
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::read_evidence::ReadFreshness;
    use crate::tool::test_utils::TestFixture;
    use serde_json::json;

    fn read_output(output: ToolOutput) -> (String, ReadEvidence) {
        match output {
            ToolOutput::Read { text, evidence } => (text, evidence),
            other => panic!("expected read output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_region_returns_exact_range_and_partial_evidence() {
        let fixture = TestFixture::new();
        fixture.create_file("src/lib.rs", "one\ntwo\nthree\nfour\n");
        let tool = ReadRegionTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let (text, evidence) = read_output(
            tool.execute(json!({
                "path": "src/lib.rs",
                "start_line": 2,
                "end_line": 3
            }))
            .await
            .unwrap(),
        );

        assert!(text.contains("2: two"), "{text}");
        assert!(text.contains("3: three"), "{text}");
        assert!(!text.contains("1: one"), "{text}");
        assert_eq!(evidence.coverage(), ReadCoverage::Partial);
        assert_eq!(evidence.window().start_line, 2);
        assert_eq!(evidence.window().end_line, Some(3));
        let path = fixture
            .project_root
            .join("src/lib.rs")
            .canonicalize()
            .unwrap();
        assert!(fixture.read_tracker.is_read(&path).await);
        assert!(!fixture.read_tracker.was_fully_read(&path).await);
    }

    #[tokio::test]
    async fn read_region_remains_exact_for_small_files() {
        let fixture = TestFixture::new();
        let content = (1..=250)
            .map(|line| format!("line {line}: compact"))
            .collect::<Vec<_>>()
            .join("\n");
        fixture.create_file("compact.txt", &content);
        let tool = ReadRegionTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let output = tool
            .execute(json!({
                "path": "compact.txt",
                "start_line": 1,
                "end_line": 80
            }))
            .await
            .unwrap();
        let ToolOutput::Read { text, evidence } = output else {
            panic!("expected read output");
        };

        assert!(text.contains("80: line 80: compact"), "{text}");
        assert!(!text.contains("81: line 81: compact"), "{text}");
        assert_eq!(evidence.coverage(), ReadCoverage::Partial);
        assert_eq!(evidence.window().end_line, Some(80));
    }

    #[tokio::test]
    async fn read_region_marks_full_when_whole_file_is_visible() {
        let fixture = TestFixture::new();
        fixture.create_file("src/lib.rs", "one\ntwo\n");
        let tool = ReadRegionTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let (_text, evidence) = read_output(
            tool.execute(json!({
                "path": "src/lib.rs",
                "start_line": 1,
                "end_line": 2
            }))
            .await
            .unwrap(),
        );

        assert_eq!(evidence.coverage(), ReadCoverage::Full);
        assert!(
            fixture
                .read_tracker
                .was_fully_read(
                    &fixture
                        .project_root
                        .join("src/lib.rs")
                        .canonicalize()
                        .unwrap(),
                )
                .await
        );
    }

    #[tokio::test]
    async fn read_region_marks_full_when_range_extends_past_eof() {
        let fixture = TestFixture::new();
        let file = fixture.create_file("src/lib.rs", "one\ntwo\n");
        let tool = ReadRegionTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let (text, evidence) = read_output(
            tool.execute(json!({
                "path": "src/lib.rs",
                "start_line": 1,
                "end_line": 999_999
            }))
            .await
            .unwrap(),
        );

        assert!(text.contains("1: one"), "{text}");
        assert!(text.contains("2: two"), "{text}");
        assert!(text.contains("Region ended at EOF"), "{text}");
        assert!(!text.contains("Region truncated"), "{text}");
        assert_eq!(evidence.coverage(), ReadCoverage::Full);
        assert_eq!(evidence.window().total_lines, Some(2));
        assert!(
            fixture
                .read_tracker
                .was_fully_read(&file.canonicalize().unwrap())
                .await
        );
    }

    #[tokio::test]
    async fn read_region_marks_empty_file_as_full_when_eof_is_visible() {
        let fixture = TestFixture::new();
        let file = fixture.create_file("empty.txt", "");
        let tool = ReadRegionTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let (text, evidence) = read_output(
            tool.execute(json!({
                "path": "empty.txt",
                "start_line": 1,
                "end_line": 50
            }))
            .await
            .unwrap(),
        );

        assert!(text.contains("Region start line 1 is past EOF"), "{text}");
        assert_eq!(evidence.coverage(), ReadCoverage::Full);
        assert_eq!(evidence.window().end_line, None);
        assert_eq!(evidence.window().total_lines, Some(0));
        assert!(
            fixture
                .read_tracker
                .was_fully_read(&file.canonicalize().unwrap())
                .await
        );
    }

    #[tokio::test]
    async fn read_symbol_reads_matching_rust_symbol_body() {
        let fixture = TestFixture::new();
        fixture.create_file(
            "src/lib.rs",
            "fn helper() {}\n\npub fn target() {\n    helper();\n}\n\nfn other() {}\n",
        );
        let tool = ReadSymbolTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let (text, evidence) = read_output(
            tool.execute(json!({
                "path": "src/lib.rs",
                "query": "target",
                "kind": "function"
            }))
            .await
            .unwrap(),
        );

        assert!(text.contains("[Symbol: fn target lines 3-5]"), "{text}");
        assert!(text.contains("3: pub fn target() {"), "{text}");
        assert!(text.contains("4:     helper();"), "{text}");
        assert!(!text.contains("1: fn helper"), "{text}");
        assert_eq!(evidence.coverage(), ReadCoverage::Partial);
        assert_eq!(evidence.window().start_line, 3);
        assert_eq!(evidence.freshness(), ReadFreshness::Fresh);
    }

    #[tokio::test]
    async fn read_symbol_reads_decorated_python_method() {
        let fixture = TestFixture::new();
        fixture.create_file(
            "client.py",
            "class Client:\n    @classmethod\n    def build(cls):\n        return cls()\n",
        );
        let tool = ReadSymbolTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let (text, evidence) = read_output(
            tool.execute(json!({
                "path": "client.py",
                "query": "build",
                "kind": "method"
            }))
            .await
            .unwrap(),
        );

        assert!(text.contains("[Symbol: method build lines 2-4]"), "{text}");
        assert!(text.contains("2:     @classmethod"), "{text}");
        assert!(text.contains("4:         return cls()"), "{text}");
        assert_eq!(evidence.window().start_line, 2);
    }

    #[tokio::test]
    async fn read_symbol_reads_exported_typescript_interface() {
        let fixture = TestFixture::new();
        fixture.create_file(
            "store.ts",
            "export interface Store {\n  get(id: string): string;\n}\n",
        );
        let tool = ReadSymbolTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let (text, evidence) = read_output(
            tool.execute(json!({
                "path": "store.ts",
                "query": "Store",
                "kind": "interface"
            }))
            .await
            .unwrap(),
        );

        assert!(
            text.contains("[Symbol: interface Store lines 1-3]"),
            "{text}"
        );
        assert!(text.contains("1: export interface Store"), "{text}");
        assert_eq!(evidence.window().end_line, Some(3));
    }

    #[tokio::test]
    async fn read_symbol_reads_go_method() {
        let fixture = TestFixture::new();
        fixture.create_file(
            "server.go",
            "package server\n\ntype Server struct{}\n\nfunc (Server) Serve() {\n    run()\n}\n",
        );
        let tool = ReadSymbolTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let (text, evidence) = read_output(
            tool.execute(json!({
                "path": "server.go",
                "query": "Serve",
                "kind": "method"
            }))
            .await
            .unwrap(),
        );

        assert!(text.contains("[Symbol: method Serve lines 5-7]"), "{text}");
        assert!(text.contains("6:     run()"), "{text}");
        assert_eq!(evidence.window().start_line, 5);
    }

    #[tokio::test]
    async fn read_symbol_reports_ambiguous_matches() {
        let fixture = TestFixture::new();
        fixture.create_file("src/lib.rs", "fn target_one() {}\nfn target_two() {}\n");
        let tool = ReadSymbolTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());

        let output = tool
            .execute(json!({
                "path": "src/lib.rs",
                "query": "target"
            }))
            .await
            .unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("expected text output");
        };

        assert!(text.contains("Multiple symbols matched"), "{text}");
        assert!(text.contains("target_one"), "{text}");
        assert!(text.contains("target_two"), "{text}");
    }
}
