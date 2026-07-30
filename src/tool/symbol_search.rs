use std::path::PathBuf;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::symbol::{
    SYMBOL_EXTENSIONS, SYMBOL_KINDS, SYMBOL_LANGUAGES, Symbol, extractor_for, language_for_path,
    parse_symbol_kind,
};
use crate::tool::output::cap_text;
use crate::tool::schema::{
    bounded_integer_property, closed_object, parse_args, string_enum_property, string_property,
};
use crate::tool::{ProjectPathResolver, Tool, ToolOutput, search};

const MAX_OUTPUT_CHARS: usize = 20_000;

#[derive(Deserialize)]
struct SymbolSearchArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default = "default_language")]
    language: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_language() -> String {
    "auto".to_string()
}

fn default_limit() -> usize {
    100
}

#[derive(Debug)]
struct SymbolMatch {
    relative_path: PathBuf,
    symbol: Symbol,
}

pub struct SymbolSearchTool {
    project_root: PathBuf,
}

impl SymbolSearchTool {
    pub fn new(project_root: PathBuf) -> Self {
        Self { project_root }
    }
}

#[async_trait]
impl Tool for SymbolSearchTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "symbol_search"
    }

    fn description(&self) -> &str {
        "Find Rust, TypeScript/JavaScript, Python, and Go symbols using built-in tree-sitter parsers. \
         Returns definitions with kind, name, signature, and location. Language defaults to \
         auto-detection; use path=<file> as a cheap outline before reading or editing it."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "query",
                    string_property("Optional substring filter for symbol names"),
                ),
                (
                    "kind",
                    string_enum_property("Optional symbol kind filter", SYMBOL_KINDS),
                ),
                (
                    "path",
                    string_property("Directory or file to search in (default: project root)"),
                ),
                ("glob", string_property("Optional glob to restrict files")),
                (
                    "language",
                    string_enum_property(
                        "Language to search; auto detects each file by extension",
                        SYMBOL_LANGUAGES,
                    ),
                ),
                (
                    "limit",
                    bounded_integer_property("Maximum number of symbols to return", Some(1), None),
                ),
            ],
            &[],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: SymbolSearchArgs = parse_args("symbol search", args)?;

        if !SYMBOL_LANGUAGES.contains(&args.language.as_str()) {
            anyhow::bail!("Unsupported language: {}", args.language);
        }
        search::ensure_limit_nonzero(args.limit)?;

        let kind_filter = parse_symbol_kind(args.kind.as_deref())?;

        let search_path = ProjectPathResolver::new(&self.project_root)
            .action("search")
            .resolve_existing_or_project_root(args.path.as_deref())?;

        let single_file = search_path.canonical_path().is_file();
        let single_file_language = single_file
            .then(|| language_for_path(search_path.canonical_path()))
            .flatten();
        if single_file && single_file_language.is_none() {
            let extension = search_path
                .canonical_path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| format!(".{ext}"))
                .unwrap_or_else(|| "without an extension".to_string());
            return Ok(ToolOutput::Text(format!(
                "symbol_search: unsupported file type {extension}; currently supports: {}\n\
                 Use read to view the file directly.",
                SYMBOL_EXTENSIONS
                    .iter()
                    .map(|extension| format!(".{extension}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        if let Some(file_language) = single_file_language
            && args.language != "auto"
            && args.language != file_language
        {
            return Ok(ToolOutput::Text(format!(
                "No symbols found: {} is a {file_language} file, but language={} was requested.",
                search_path.canonical_path().display(),
                args.language
            )));
        }

        let glob_pattern = search::compile_glob(args.glob.as_deref(), None)?;

        let respect_gitignore = search::respect_gitignore("BONSAI_SYMBOL_RESPECT_GITIGNORE");

        // Owned copies moved into the blocking closure so the walkdir + file
        // read + tree-sitter extraction (the dominant CPU cost) runs off the
        // async runtime thread.
        let walk_root = search_path.canonical_path().to_path_buf();
        let display_base = search_path.canonical_root().to_path_buf();
        let glob_base = search_path
            .relative_base_for_file_or_directory()
            .to_path_buf();
        let gitignore = if respect_gitignore {
            search::build_gitignore(search_path.canonical_root(), &walk_root)
        } else {
            None
        };
        let query = args.query.clone();
        let limit = args.limit;
        let requested_language = args.language;

        let (results, truncated): (Vec<SymbolMatch>, bool) =
            tokio::task::spawn_blocking(move || -> Result<(Vec<SymbolMatch>, bool)> {
                let mut results: Vec<SymbolMatch> = Vec::new();
                let mut truncated = false;
                'walk: for file in search::walk_project_files(
                    &walk_root,
                    &display_base,
                    gitignore.as_ref(),
                    respect_gitignore,
                    &display_base,
                ) {
                    let relative_path = file.relative;
                    let glob_path = file
                        .absolute
                        .strip_prefix(&glob_base)
                        .unwrap_or(relative_path.as_path());
                    if search::is_hidden_or_gitignored(
                        &relative_path,
                        gitignore.as_ref(),
                        respect_gitignore,
                    ) {
                        continue;
                    }
                    if let Some(pattern) = &glob_pattern
                        && !pattern.matches_path(glob_path)
                    {
                        continue;
                    }

                    let path = file.absolute.as_path();
                    let Some(file_language) = language_for_path(path) else {
                        continue;
                    };
                    if requested_language != "auto" && requested_language != file_language {
                        continue;
                    }
                    let Some(extractor) = extractor_for(file_language) else {
                        continue;
                    };
                    let source = std::fs::read_to_string(path)
                        .with_context(|| format!("Failed to read file: {}", path.display()))?;
                    for symbol in extractor.extract(path, &source)? {
                        if let Some(kind) = kind_filter
                            && symbol.kind != kind
                        {
                            continue;
                        }
                        if let Some(query) = query.as_deref()
                            && !symbol.name.contains(query)
                        {
                            continue;
                        }
                        if results.len() >= limit {
                            truncated = true;
                            break 'walk;
                        }
                        results.push(SymbolMatch {
                            relative_path: relative_path.clone(),
                            symbol,
                        });
                    }
                }
                Ok((results, truncated))
            })
            .await??;

        if results.is_empty() {
            return Ok(ToolOutput::Text("No symbols found".to_string()));
        }

        let mut output = format_matches(&results, single_file);
        if truncated {
            output.push_str(&search::format_truncation(limit, None));
        }
        Ok(ToolOutput::Text(cap_text(
            output,
            MAX_OUTPUT_CHARS,
            "Use path, query, kind, glob, or limit to narrow symbol_search.",
        )))
    }
}

fn format_matches(matches: &[SymbolMatch], single_file: bool) -> String {
    if single_file {
        return format_single_file_matches(matches);
    }

    let mut out = String::new();
    for symbol_match in matches {
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "{}:{}-{}:{} {} {}",
            symbol_match.relative_path.display(),
            symbol_match.symbol.line,
            symbol_match.symbol.end_line,
            symbol_match.symbol.column,
            symbol_match.symbol.kind.label(),
            symbol_summary(&symbol_match.symbol)
        );
    }
    out.trim_end().to_string()
}

fn format_single_file_matches(matches: &[SymbolMatch]) -> String {
    let mut out = String::new();
    if let Some(first) = matches.first() {
        use std::fmt::Write as _;
        let _ = writeln!(out, "symbols in {}", first.relative_path.display());
        let _ = writeln!(out, "{:<7} {:>9}  signature", "kind", "lines");
    }

    for symbol_match in matches {
        use std::fmt::Write as _;
        let _ = writeln!(
            out,
            "{:<7} {:>4}-{:>4}  {}",
            symbol_match.symbol.kind.label(),
            symbol_match.symbol.line,
            symbol_match.symbol.end_line,
            symbol_summary(&symbol_match.symbol)
        );
    }
    out.trim_end().to_string()
}

fn symbol_summary(symbol: &Symbol) -> &str {
    let signature = symbol.signature.trim();
    if signature.is_empty() {
        symbol.name.as_str()
    } else {
        signature
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_utils::TestFixture;
    use serde_json::json;

    #[tokio::test]
    async fn finds_rust_symbols() {
        let fixture = TestFixture::new();
        fixture.create_file(
            "src/lib.rs",
            "pub struct Foo;\nimpl Foo { pub fn new() -> Self { Self } }\nfn helper() {}\n",
        );
        let tool = SymbolSearchTool::new(fixture.project_root.clone());
        let result = tool.execute(json!({})).await.unwrap();
        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(output.contains("Foo"), "output: {output}");
        assert!(output.contains("helper"), "output: {output}");
    }

    #[tokio::test]
    async fn filters_by_kind_and_query() {
        let fixture = TestFixture::new();
        fixture.create_file("src/lib.rs", "pub struct Foo;\nfn helper() {}\n");
        let tool = SymbolSearchTool::new(fixture.project_root.clone());
        let result = tool
            .execute(json!({"kind": "struct", "query": "Foo"}))
            .await
            .unwrap();
        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(output.contains("struct"), "output: {output}");
        assert!(output.contains("Foo"), "output: {output}");
        assert!(!output.contains("helper"), "output: {output}");
    }

    #[tokio::test]
    async fn supports_file_path() {
        let fixture = TestFixture::new();
        fixture.create_file("src/lib.rs", "pub struct Foo;\nfn helper() {}\n");
        let tool = SymbolSearchTool::new(fixture.project_root.clone());

        let result = tool.execute(json!({"path": "src/lib.rs"})).await.unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(output.contains("symbols in src/lib.rs"), "output: {output}");
        assert!(output.contains("kind"), "output: {output}");
        assert!(output.contains("line"), "output: {output}");
        assert!(output.contains("struct"), "output: {output}");
        assert!(output.contains("fn"), "output: {output}");
        assert!(output.contains("src/lib.rs"), "output: {output}");
        assert!(output.contains("Foo"), "output: {output}");
        assert!(output.contains("helper"), "output: {output}");
        assert!(output.contains("1"), "output: {output}");
        assert!(output.contains("2"), "output: {output}");
    }

    #[tokio::test]
    async fn symbol_search_scoped_path_respects_root_gitignore() {
        let fixture = TestFixture::new();
        fixture.create_gitignore(&["src/generated.rs"]);
        fixture.create_file("src/generated.rs", "pub struct Ignored;\n");
        fixture.create_file("src/kept.rs", "pub struct Kept;\n");
        let tool = SymbolSearchTool::new(fixture.project_root.clone());

        let output = match tool
            .execute(json!({"path": "src", "glob": "*.rs"}))
            .await
            .unwrap()
        {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("src/kept.rs"), "output: {output}");
        assert!(output.contains("Kept"), "output: {output}");
        assert!(!output.contains("Ignored"), "output: {output}");
    }

    #[tokio::test]
    async fn rejects_zero_limit() {
        let fixture = TestFixture::new();
        let tool = SymbolSearchTool::new(fixture.project_root.clone());

        let result = tool.execute(json!({"limit": 0})).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("limit"));
    }

    #[tokio::test]
    async fn rejects_unsupported_language() {
        let fixture = TestFixture::new();
        let tool = SymbolSearchTool::new(fixture.project_root.clone());
        let result = tool.execute(json!({"language": "java"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_outside_project_root() {
        let fixture = TestFixture::new();
        let tool = SymbolSearchTool::new(fixture.project_root.clone());
        let result = tool.execute(json!({"path": ".."})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn reports_unsupported_single_file_extension() {
        let fixture = TestFixture::new();
        fixture.create_file("README.md", "# Project\n");
        let tool = SymbolSearchTool::new(fixture.project_root.clone());

        let result = tool.execute(json!({"path": "README.md"})).await.unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(
            output.contains("unsupported file type .md"),
            "output: {output}"
        );
        assert!(output.contains("Use read"), "output: {output}");
    }

    #[tokio::test]
    async fn auto_detects_typescript_javascript_and_python() {
        let fixture = TestFixture::new();
        fixture.create_file("web/store.ts", "export interface Store {}\n");
        fixture.create_file("web/index.js", "export function boot() {}\n");
        fixture.create_file("pkg/client.py", "class Client:\n    pass\n");
        fixture.create_file("cmd/main.go", "package main\nfunc Run() {}\n");
        let tool = SymbolSearchTool::new(fixture.project_root.clone());

        let output = match tool.execute(json!({})).await.unwrap() {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("Store"), "output: {output}");
        assert!(output.contains("boot"), "output: {output}");
        assert!(output.contains("Client"), "output: {output}");
        assert!(output.contains("Run"), "output: {output}");
    }

    #[tokio::test]
    async fn explicit_language_filters_mixed_projects() {
        let fixture = TestFixture::new();
        fixture.create_file("src/lib.rs", "pub struct RustOnly;\n");
        fixture.create_file("pkg/client.py", "class PythonOnly:\n    pass\n");
        let tool = SymbolSearchTool::new(fixture.project_root.clone());

        let output = match tool.execute(json!({"language": "python"})).await.unwrap() {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };

        assert!(output.contains("PythonOnly"), "output: {output}");
        assert!(!output.contains("RustOnly"), "output: {output}");
    }

    #[tokio::test]
    async fn empty_file_returns_no_symbols_message() {
        let fixture = TestFixture::new();
        fixture.create_file("src/lib.rs", "");
        let tool = SymbolSearchTool::new(fixture.project_root.clone());

        let result = tool.execute(json!({"path": "src/lib.rs"})).await.unwrap();

        let output = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("Expected Text output"),
        };
        assert!(output.contains("No symbols"), "output: {output}");
    }

    #[tokio::test]
    async fn rejects_invalid_glob() {
        let fixture = TestFixture::new();
        let tool = SymbolSearchTool::new(fixture.project_root.clone());
        let result = tool.execute(json!({"glob": "["})).await;
        assert!(result.is_err());
    }
}
