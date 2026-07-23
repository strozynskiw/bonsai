//! Repository map: a ranked, token-budgeted outline of the key symbols per
//! source file, injected into the system prompt's cacheable prefix so the agent
//! can orient on a large codebase at low token cost.
//!
//! The map is built once at startup and is byte-stable for the session, so it
//! sits inside the Anthropic prompt-cache prefix (see
//! [`crate::context::ProjectContextSnapshot::cacheable_prefix`]). Byte-stability
//! is why every step here is fully ordered: files are sorted by path before
//! ranking, the rank comparator breaks ties by path, and the chosen set is
//! re-sorted by path for output — `walkdir` yields filesystem order, which is
//! not deterministic across machines.

use std::path::Path;

use ignore::gitignore::Gitignore;

use crate::symbol::{Symbol, SymbolKind, extractor_for_path, language_for_path};
use crate::tool::search;

/// Approximate token budget for the rendered map, converted to a character
/// budget (~4 chars/token) for the greedy fill. The map is a coarse orientation
/// aid, so a heuristic budget is fine — no need to invoke a real tokenizer here.
const TOKEN_BUDGET: usize = 1_500;
const CHARS_PER_TOKEN: usize = 4;

/// Most symbols listed per file before the rest collapse into `· +N more`.
const PER_FILE_SYMBOL_CAP: usize = 12;

/// Stop scanning after this many source files so a pathologically large repo
/// cannot stall startup. When hit, the footer says so (no silent cap).
const MAX_FILES_SCANNED: usize = 5_000;

const HEADING: &str = "## Repository map";

/// Build the repository map for `root`, or an empty string when the project has
/// no extractable Rust, TypeScript/JavaScript, Python, or Go symbols. Callers omit
/// the section entirely on an empty result.
pub(crate) fn build_repo_map(root: &Path) -> String {
    build_with_budget(root, TOKEN_BUDGET * CHARS_PER_TOKEN)
}

/// One file's contribution to the map: its display path, ranking score, and the
/// significant symbols (already ordered public-first, then by source line).
struct FileOutline {
    relative: String,
    score: i64,
    symbols: Vec<Symbol>,
}

fn build_with_budget(root: &Path, char_budget: usize) -> String {
    let respect_gitignore = search::env_flag_enabled("BONSAI_REPO_MAP_RESPECT_GITIGNORE", true);
    let gitignore = if respect_gitignore {
        search::build_gitignore(root, root)
    } else {
        None
    };

    // Collect candidate source files and sort by path for a deterministic
    // ranking and a byte-stable map (walkdir yields filesystem order).
    let mut files: Vec<_> =
        search::walk_project_files(root, root, gitignore.as_ref(), respect_gitignore, root)
            .filter(|file| {
                language_for_path(&file.relative).is_some()
                    && !is_excluded(&file.relative, gitignore.as_ref(), respect_gitignore)
            })
            .collect();
    files.sort_by(|a, b| a.relative.cmp(&b.relative));

    // Bound the scan so a pathologically large repo can't stall startup.
    let hit_scan_cap = files.len() > MAX_FILES_SCANNED;
    files.truncate(MAX_FILES_SCANNED);

    let mut outlines: Vec<FileOutline> = Vec::new();
    for file in files {
        let Some(extractor) = extractor_for_path(&file.absolute) else {
            continue;
        };
        let Ok(source) = std::fs::read_to_string(&file.absolute) else {
            continue;
        };
        let Ok(symbols) = extractor.extract(&file.absolute, &source) else {
            continue;
        };
        let symbols = select_significant(symbols);
        if symbols.is_empty() {
            continue;
        }
        let relative = file.relative.to_string_lossy().replace('\\', "/");
        let score = file_score(&relative, &symbols);
        outlines.push(FileOutline {
            relative,
            score,
            symbols,
        });
    }

    if outlines.is_empty() {
        return String::new();
    }

    // Rank by score, ties broken by path so the order is total and stable.
    outlines.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.relative.cmp(&b.relative))
    });

    // Greedily take the highest-ranked files until the budget is spent; every
    // per-file block is bounded by PER_FILE_SYMBOL_CAP, so the first file always
    // fits and the map is never empty when symbols exist.
    let mut chosen: Vec<&FileOutline> = Vec::new();
    let mut used = 0usize;
    for outline in &outlines {
        let block_len = render_file(outline).len();
        if !chosen.is_empty() && used + block_len > char_budget {
            break;
        }
        used += block_len;
        chosen.push(outline);
    }
    let omitted = outlines.len() - chosen.len();

    // Re-sort the chosen set by path so the map reads top-down and stays
    // byte-stable regardless of ranking ties.
    chosen.sort_by(|a, b| a.relative.cmp(&b.relative));

    let mut out = String::new();
    out.push_str(HEADING);
    out.push('\n');
    out.push_str(
        "Key symbols per file (ranked, truncated to a token budget). \
         Use `symbol_search` with `path` for one file's full structure or `query` to locate a symbol.\n\n",
    );
    for outline in &chosen {
        out.push_str(&render_file(outline));
    }
    if omitted > 0 || hit_scan_cap {
        out.push('\n');
        if hit_scan_cap {
            out.push_str(&format!(
                "(scan capped at {MAX_FILES_SCANNED} files; {omitted}+ more not shown — use glob/grep to explore.)\n"
            ));
        } else {
            out.push_str(&format!(
                "(+{omitted} more file{} not shown — use glob/grep to explore.)\n",
                if omitted == 1 { "" } else { "s" }
            ));
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// Render a single file block: the path, then its symbols on one indented line
/// joined by ` · `, with overflow past the per-file cap collapsed into `+N more`.
fn render_file(outline: &FileOutline) -> String {
    let shown = outline.symbols.len().min(PER_FILE_SYMBOL_CAP);
    let mut parts: Vec<String> = outline.symbols[..shown]
        .iter()
        .map(|symbol| format!("{} {}", symbol.kind.label(), symbol.name))
        .collect();
    if outline.symbols.len() > shown {
        parts.push(format!("+{} more", outline.symbols.len() - shown));
    }
    format!("{}\n  {}\n", outline.relative, parts.join(" · "))
}

/// Keep symbols worth surfacing in a high-level map: the public API anywhere in
/// the file, plus top-level definitions (column 1) even when private, since they
/// form the file's structure. Drop `impl` headers — their public methods already
/// surface on their own. Order public-first then by source line, so the per-file
/// cap keeps the most informative symbols.
fn select_significant(symbols: Vec<Symbol>) -> Vec<Symbol> {
    let mut significant: Vec<Symbol> = symbols.into_iter().filter(is_significant).collect();
    significant.sort_by(|a, b| {
        b.is_public
            .cmp(&a.is_public)
            .then_with(|| a.line.cmp(&b.line))
    });
    significant
}

fn is_significant(symbol: &Symbol) -> bool {
    if symbol.kind == SymbolKind::Impl {
        return false;
    }
    if symbol.kind == SymbolKind::Method {
        return symbol.is_public;
    }
    symbol.is_public || symbol.column == 1
}

/// Whether a file should be skipped. Unlike the shared file-level gitignore
/// check, this matches against *every ancestor* so a generated file nested under
/// an ignored directory (e.g. `target/…/bindgen.rs` under `/target`) is excluded
/// — `Gitignore::matched` alone only tests the literal path, missing those.
fn is_excluded(relative: &Path, gitignore: Option<&Gitignore>, respect_gitignore: bool) -> bool {
    search::is_hidden_path(relative)
        || (respect_gitignore
            && gitignore.is_some_and(|gitignore| {
                gitignore
                    .matched_path_or_any_parents(relative, false)
                    .is_ignore()
            }))
}

/// Rank a file by how much public surface it exposes, with crate roots pinned to
/// the top and module hubs nudged up. Higher is more important.
fn file_score(relative: &str, symbols: &[Symbol]) -> i64 {
    let public = symbols.iter().filter(|symbol| symbol.is_public).count() as i64;
    public + (symbols.len() as i64 / 4) + entrypoint_bonus(relative)
}

fn entrypoint_bonus(relative: &str) -> i64 {
    match relative.rsplit('/').next().unwrap_or(relative) {
        "main.rs" | "lib.rs" | "main.py" | "__init__.py" | "index.ts" | "index.tsx"
        | "index.js" | "index.jsx" | "main.go" => 1_000,
        "mod.rs" => 20,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Minimal on-disk project fixture: `build_repo_map` only reads files, so a
    /// bare temp dir (no tokio runtime, no interaction service) is enough.
    fn project(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (path, body) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, body).unwrap();
        }
        dir
    }

    #[test]
    fn lists_public_symbols_grouped_by_file() {
        let dir = project(&[(
            "src/widget.rs",
            "pub struct Widget;\npub fn make() -> Widget { Widget }\nfn private_helper() {}\n",
        )]);
        let map = build_repo_map(dir.path());
        assert!(map.contains(HEADING), "{map}");
        assert!(map.contains("src/widget.rs"), "{map}");
        assert!(map.contains("struct Widget"), "{map}");
        assert!(map.contains("fn make"), "{map}");
    }

    #[test]
    fn pins_crate_root_above_other_files() {
        let dir = project(&[
            ("src/zzz_module.rs", "pub fn zzz() {}\npub fn zzz2() {}\n"),
            ("src/lib.rs", "pub fn entry() {}\n"),
        ]);
        // Tiny budget so only the top-ranked file survives: the entrypoint bonus
        // must keep lib.rs in over the alphabetically-later module.
        let map = build_with_budget(dir.path(), 30);
        assert!(
            map.contains("fn entry"),
            "lib.rs should win the budget: {map}"
        );
        assert!(!map.contains("fn zzz"), "{map}");
    }

    #[test]
    fn deterministic_across_runs() {
        let dir = project(&[
            ("src/a.rs", "pub fn a() {}\n"),
            ("src/b.rs", "pub fn b() {}\n"),
            ("src/c.rs", "pub struct C;\n"),
        ]);
        assert_eq!(build_repo_map(dir.path()), build_repo_map(dir.path()));
    }

    #[test]
    fn respects_gitignore() {
        let dir = project(&[
            (".gitignore", "generated.rs\n"),
            ("src/keep.rs", "pub fn keep() {}\n"),
            ("generated.rs", "pub fn generated() {}\n"),
        ]);
        let map = build_repo_map(dir.path());
        assert!(map.contains("fn keep"), "{map}");
        assert!(!map.contains("fn generated"), "{map}");
    }

    #[test]
    fn empty_for_project_without_supported_source() {
        let dir = project(&[("README.md", "# hi\n"), ("config.toml", "value = 1\n")]);
        assert_eq!(build_repo_map(dir.path()), "");
    }

    #[test]
    fn includes_typescript_javascript_and_python_outlines() {
        let dir = project(&[
            ("web/index.ts", "export interface Store {}\n"),
            ("web/runtime.js", "export function boot() {}\n"),
            (
                "pkg/client.py",
                "class Client:\n    def request(self):\n        pass\n",
            ),
        ]);

        let map = build_repo_map(dir.path());

        assert!(map.contains("interface Store"), "{map}");
        assert!(map.contains("fn boot"), "{map}");
        assert!(map.contains("class Client"), "{map}");
        assert!(map.contains("method request"), "{map}");
    }

    #[test]
    fn includes_go_outline_and_omits_unexported_methods() {
        let dir = project(&[(
            "cmd/main.go",
            "package main\n\ntype Server struct{}\nfunc NewServer() Server { return Server{} }\nfunc (Server) Serve() {}\nfunc (Server) helper() {}\n",
        )]);

        let map = build_repo_map(dir.path());

        assert!(map.contains("struct Server"), "{map}");
        assert!(map.contains("fn NewServer"), "{map}");
        assert!(map.contains("method Serve"), "{map}");
        assert!(!map.contains("method helper"), "{map}");
    }

    #[test]
    fn caps_symbols_per_file_with_overflow_marker() {
        let body: String = (0..PER_FILE_SYMBOL_CAP + 5)
            .map(|i| format!("pub fn f{i}() {{}}\n"))
            .collect();
        let dir = project(&[("src/big.rs", &body)]);
        let map = build_repo_map(dir.path());
        assert!(map.contains("more"), "expected overflow marker in: {map}");
    }

    #[test]
    fn truncates_to_budget_with_more_files_footer() {
        let files: Vec<(String, &str)> = (0..40)
            .map(|i| (format!("src/m{i:02}.rs"), "pub fn f() {}\npub struct S;\n"))
            .collect();
        let owned: Vec<(&str, &str)> = files.iter().map(|(p, b)| (p.as_str(), *b)).collect();
        let dir = project(&owned);
        // A tiny budget forces most files out; the footer must report the count.
        let map = build_with_budget(dir.path(), 200);
        assert!(
            map.contains("more file"),
            "expected truncation footer: {map}"
        );
    }

    #[test]
    fn excludes_impl_headers_but_keeps_public_methods() {
        let dir = project(&[(
            "src/svc.rs",
            "pub struct Svc;\nimpl Svc {\n    pub fn run(&self) {}\n}\n",
        )]);
        let map = build_repo_map(dir.path());
        assert!(map.contains("fn run"), "public method should appear: {map}");
        assert!(
            !map.contains("impl Svc"),
            "impl header should be dropped: {map}"
        );
    }

    #[test]
    fn excludes_files_nested_under_an_ignored_directory() {
        // `/target` ignores the directory; generated files nested below it must
        // not leak into the map (a plain file-level gitignore check misses them).
        let dir = project(&[
            (".gitignore", "/target\n"),
            ("src/keep.rs", "pub fn keep() {}\n"),
            ("target/build/gen.rs", "pub fn generated_symbol() {}\n"),
        ]);
        let map = build_repo_map(dir.path());
        assert!(map.contains("fn keep"), "{map}");
        assert!(!map.contains("generated_symbol"), "{map}");
        assert!(!map.contains("target/"), "{map}");
    }
}
