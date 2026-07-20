//! Suggest existing project files when a tool path argument does not resolve.
//!
//! A bare "Path not found" forces the model to guess the path again, burning a
//! turn. When a `read`/`grep`/`glob`/`symbol_search` path misses, these helpers
//! name the few existing files whose name is closest to what was asked for,
//! ranked so a near sibling beats a far namesake. They only read the tree to
//! describe it — they never create or mutate anything — and they run only on the
//! (cold) not-found error path, with a bounded, gitignore-aware walk.

use std::path::Path;

use ignore::WalkBuilder;
use strsim::normalized_levenshtein;

/// Most suggestions surfaced in one hint.
const MAX_SUGGESTIONS: usize = 5;

/// Fuzzy basename similarity floor: a name below this is too different to suggest.
const NAME_SIMILARITY_FLOOR: f64 = 0.6;

/// Upper bound on files scanned while looking for suggestions. The walk already
/// prunes hidden and gitignored directories, so this only guards against an
/// enormous tracked tree; it keeps the missing-path path bounded regardless.
const MAX_FILES_SCANNED: usize = 20_000;

/// Render the " Did you mean: …" tail appended to a path-not-found error, or an
/// empty string when no existing file is close enough. The leading `. ` lets the
/// caller concatenate it directly onto `Path not found: {path}`.
pub(crate) fn nearest_path_hint(project_root: &Path, raw_path: &str) -> String {
    let suggestions = nearest_paths(project_root, raw_path);
    if suggestions.is_empty() {
        return String::new();
    }
    format!(". Did you mean: {}?", suggestions.join(", "))
}

/// A scored candidate file, kept relative to the project root for display.
struct Scored {
    relative: String,
    score: f64,
    proximity: usize,
}

/// The relative paths of the existing files closest to `raw_path`'s name, best
/// first (capped at [`MAX_SUGGESTIONS`]). Empty when nothing clears the floor.
fn nearest_paths(project_root: &Path, raw_path: &str) -> Vec<String> {
    let want = Path::new(raw_path);
    let Some(want_name) = file_name_string(want) else {
        return Vec::new();
    };
    let want_stem = file_stem_string(want).unwrap_or_else(|| want_name.clone());
    let want_dir = path_components(want.parent().unwrap_or_else(|| Path::new("")));

    let mut scored: Vec<Scored> = Vec::new();
    let mut scanned = 0usize;
    let walker = WalkBuilder::new(project_root)
        .follow_links(false)
        .hidden(true)
        .ignore(true)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .parents(true)
        .build();

    for entry in walker.filter_map(Result::ok) {
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        scanned += 1;
        if scanned > MAX_FILES_SCANNED {
            break;
        }
        let relative = entry
            .path()
            .strip_prefix(project_root)
            .unwrap_or(entry.path());
        let Some(name) = file_name_string(relative) else {
            continue;
        };
        let Some(score) = name_score(
            &want_name,
            &want_stem,
            &name,
            file_stem_string(relative).as_deref(),
        ) else {
            continue;
        };
        let proximity = shared_prefix_len(
            &want_dir,
            &path_components(relative.parent().unwrap_or_else(|| Path::new(""))),
        );
        scored.push(Scored {
            relative: relative.to_string_lossy().replace('\\', "/"),
            score,
            proximity,
        });
    }

    // Most similar first; break ties by directory proximity (a same-folder
    // namesake beats a distant one), then by path for a stable order.
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then(b.proximity.cmp(&a.proximity))
            .then(a.relative.cmp(&b.relative))
    });
    scored.truncate(MAX_SUGGESTIONS);
    scored.into_iter().map(|s| s.relative).collect()
}

/// Similarity of a candidate file's name to the wanted one. An exact basename
/// wins, then a shared stem with a different extension, then a fuzzy basename
/// above the floor. `None` means too different to suggest.
fn name_score(want_name: &str, want_stem: &str, name: &str, stem: Option<&str>) -> Option<f64> {
    if name == want_name {
        return Some(1.0);
    }
    if stem == Some(want_stem) {
        return Some(0.85);
    }
    let similarity = normalized_levenshtein(want_name, name);
    (similarity >= NAME_SIMILARITY_FLOOR).then_some(similarity)
}

fn file_name_string(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
}

fn file_stem_string(path: &Path) -> Option<String> {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
}

/// The `Normal` components of a path, as strings (ignoring `.`/`..`/root), for
/// proximity scoring.
fn path_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy().to_string()),
            _ => None,
        })
        .collect()
}

/// Number of leading directory components two paths share.
fn shared_prefix_len(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(root: &Path, relative: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    #[test]
    fn suggests_exact_basename_found_elsewhere() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "src/a/config.rs");
        touch(dir.path(), "src/b/other.rs");

        let hint = nearest_path_hint(dir.path(), "src/c/config.rs");
        assert!(hint.contains("Did you mean:"), "{hint}");
        assert!(hint.contains("src/a/config.rs"), "{hint}");
        assert!(!hint.contains("other.rs"), "{hint}");
    }

    #[test]
    fn suggests_same_stem_different_extension() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "src/data.json");

        let paths = nearest_paths(dir.path(), "src/data.yaml");
        assert_eq!(paths, vec!["src/data.json".to_string()]);
    }

    #[test]
    fn ranks_nearest_directory_first() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "config.rs");
        touch(dir.path(), "src/sub/config.rs");

        // Both are exact-name matches (score 1.0); proximity to the requested
        // `src/sub/...` must put the same-folder file first.
        let paths = nearest_paths(dir.path(), "src/sub/deep/config.rs");
        assert_eq!(
            paths,
            vec!["src/sub/config.rs".to_string(), "config.rs".to_string()]
        );
    }

    #[test]
    fn excludes_gitignored_files() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), ".gitignore");
        fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
        touch(dir.path(), "build/config.rs");
        touch(dir.path(), "src/config.rs");

        let paths = nearest_paths(dir.path(), "missing/config.rs");
        assert_eq!(paths, vec!["src/config.rs".to_string()]);
    }

    #[test]
    fn excludes_files_ignored_by_nested_gitignore() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/.gitignore"), "generated.rs\n").unwrap();
        touch(dir.path(), "src/generated.rs");
        touch(dir.path(), "src/generator.rs");

        let paths = nearest_paths(dir.path(), "src/generate.rs");

        assert!(
            !paths.iter().any(|path| path == "src/generated.rs"),
            "nested ignored files must not be suggested: {paths:?}"
        );
        assert_eq!(paths, vec!["src/generator.rs".to_string()]);
    }

    #[test]
    fn no_suggestion_when_nothing_is_close() {
        let dir = TempDir::new().unwrap();
        touch(dir.path(), "src/completely_unrelated.rs");

        assert_eq!(nearest_path_hint(dir.path(), "notes.md"), "");
    }

    #[test]
    fn caps_suggestion_count() {
        let dir = TempDir::new().unwrap();
        for i in 0..(MAX_SUGGESTIONS + 4) {
            touch(dir.path(), &format!("dir{i}/mod.rs"));
        }

        let paths = nearest_paths(dir.path(), "elsewhere/mod.rs");
        assert_eq!(paths.len(), MAX_SUGGESTIONS);
    }
}
