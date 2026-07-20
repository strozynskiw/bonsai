use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

pub(crate) fn env_flag_enabled(var: &str, default: bool) -> bool {
    std::env::var(var)
        .map(|value| value != "false" && value != "0")
        .unwrap_or(default)
}

pub(crate) fn build_gitignore(project_root: &Path, search_path: &Path) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(project_root);
    let search_dir = if search_path.is_dir() {
        search_path
    } else {
        search_path.parent().unwrap_or(search_path)
    };
    let mut dirs = Vec::new();
    let mut current = search_dir;
    while current.starts_with(project_root) {
        dirs.push(current.to_path_buf());
        if current == project_root {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        current = parent;
    }
    if dirs.first().map(PathBuf::as_path) != Some(project_root) {
        dirs.push(project_root.to_path_buf());
    }
    dirs.reverse();
    for dir in dirs {
        builder.add(dir.join(".gitignore"));
    }
    builder.build().ok()
}

/// Derive the `(walk_root, display_base, glob_base, gitignore)` bundle a
/// blocking file walk needs from a resolved search root. Returns owned copies so
/// the tuple can move into a `spawn_blocking` closure. `gitignore` is `None` when
/// `respect_gitignore` is off.
pub(crate) fn walk_context(
    root: &super::ExistingProjectPath,
    respect_gitignore: bool,
) -> (PathBuf, PathBuf, PathBuf, Option<Gitignore>) {
    let walk_root = root.canonical_path().to_path_buf();
    let display_base = root.canonical_root().to_path_buf();
    let glob_base = root.relative_base_for_file_or_directory().to_path_buf();
    let gitignore = if respect_gitignore {
        build_gitignore(root.canonical_root(), &walk_root)
    } else {
        None
    };
    (walk_root, display_base, glob_base, gitignore)
}

pub(crate) fn is_hidden_path(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str().to_string_lossy().starts_with('.'))
}

pub(crate) fn is_visible_walk_entry(entry: &walkdir::DirEntry, search_root: &Path) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let relative = entry
        .path()
        .strip_prefix(search_root)
        .unwrap_or(entry.path());
    !is_hidden_path(relative)
}

pub(crate) fn is_hidden_or_gitignored(
    path: &Path,
    gitignore: Option<&Gitignore>,
    respect_gitignore: bool,
) -> bool {
    is_hidden_or_gitignored_kind(path, gitignore, respect_gitignore, false)
}

/// Like [`is_hidden_or_gitignored`] but takes whether `path` is a directory, so
/// directory-only ignore patterns (e.g. `target/`) match correctly. Directory
/// listings need this; the file-only search walkers can use the `false` wrapper.
pub(crate) fn is_hidden_or_gitignored_kind(
    path: &Path,
    gitignore: Option<&Gitignore>,
    respect_gitignore: bool,
    is_dir: bool,
) -> bool {
    is_hidden_path(path)
        || (respect_gitignore
            && gitignore.is_some_and(|gitignore| gitignore.matched(path, is_dir).is_ignore()))
}

/// A regular file discovered while walking the project tree.
pub(crate) struct ProjectFile {
    /// Absolute path on disk, suitable for reading.
    pub absolute: PathBuf,
    /// Path relative to the strip base, suitable for display and pattern matching.
    pub relative: PathBuf,
}

/// Walk `walk_root` for regular files, pruning hidden directories from
/// traversal, never following symlinks, and yielding each file with its path
/// made relative to `strip_base`.
///
/// Callers layer their own filters (gitignore, glob, type) on top of the
/// returned `relative` path. For directory searches `walk_root` and
/// `strip_base` are the same directory; for single-file searches `strip_base`
/// is the file's parent so the relative path keeps the file name.
pub(crate) fn walk_project_files(
    walk_root: &Path,
    strip_base: &Path,
) -> impl Iterator<Item = ProjectFile> {
    let walk_root = walk_root.to_path_buf();
    let strip_base = strip_base.to_path_buf();
    walkdir::WalkDir::new(&walk_root)
        .follow_links(false)
        .into_iter()
        .filter_entry(move |entry| is_visible_walk_entry(entry, &walk_root))
        .filter_map(move |entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if !path.is_file() {
                return None;
            }
            let relative = path.strip_prefix(&strip_base).ok()?.to_path_buf();
            Some(ProjectFile {
                absolute: path.to_path_buf(),
                relative,
            })
        })
}

/// Compile an optional user-supplied glob, falling back to `default` when the
/// caller did not provide one. Returns `Ok(None)` only when both are absent, so
/// callers with a required pattern can `expect` a `Some`.
///
/// Centralizes the identical "compile glob with a friendly error" block that
/// previously lived in glob/grep/symbol_search.
pub(crate) fn compile_glob(
    pattern: Option<&str>,
    default: Option<&str>,
) -> Result<Option<glob::Pattern>> {
    match pattern.or(default) {
        Some(pattern) => {
            Ok(Some(glob::Pattern::new(pattern).with_context(|| {
                format!("Invalid glob pattern: {pattern}")
            })?))
        }
        None => Ok(None),
    }
}

/// Reject a zero limit with the shared error message used by every search tool.
pub(crate) fn ensure_limit_nonzero(limit: usize) -> Result<()> {
    if limit == 0 {
        anyhow::bail!("limit must be greater than 0");
    }
    Ok(())
}

/// Reject an empty required pattern with the shared error message used by glob
/// and grep.
pub(crate) fn ensure_pattern_present(pattern: &str) -> Result<()> {
    if pattern.is_empty() {
        anyhow::bail!("pattern is required");
    }
    Ok(())
}

/// Build the standard truncation notice appended when results are capped.
///
/// When `total` is known (all matches were collected before truncating) the
/// message reports "showing X of Y"; otherwise it reports "showing first X" for
/// tools that stop walking as soon as the limit is reached. Shares the bracketed
/// footer vocabulary with [`crate::tool::output::cap_text`]; the returned string
/// includes its own leading blank line so callers can `push_str` it directly.
pub(crate) fn format_truncation(limit: usize, total: Option<usize>) -> String {
    let body = match total {
        Some(total) => format!("showing {limit} of {total} matches. Try a more specific filter."),
        None => format!("showing first {limit} matches. Try a more specific filter."),
    };
    crate::tool::output::truncation_note(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, "x").unwrap();
    }

    #[test]
    fn walk_yields_files_relative_to_strip_base() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        touch(&root.join("a.txt"));
        touch(&root.join("nested/b.txt"));

        let mut relatives: Vec<String> = walk_project_files(root, root)
            .map(|f| f.relative.to_string_lossy().replace('\\', "/"))
            .collect();
        relatives.sort();
        assert_eq!(relatives, vec!["a.txt", "nested/b.txt"]);
    }

    #[test]
    fn walk_skips_hidden_and_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        touch(&root.join("visible.txt"));
        touch(&root.join(".hidden/secret.txt"));
        std::fs::create_dir_all(root.join("emptydir")).unwrap();

        let relatives: Vec<String> = walk_project_files(root, root)
            .map(|f| f.relative.to_string_lossy().to_string())
            .collect();
        assert_eq!(relatives, vec!["visible.txt"]);
    }

    #[test]
    fn walk_single_file_keeps_name_via_parent_strip_base() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let file = root.join("only.txt");
        touch(&file);

        let files: Vec<ProjectFile> = walk_project_files(&file, root).collect();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].relative.to_string_lossy(), "only.txt");
        assert_eq!(files[0].absolute, file);
    }

    #[test]
    fn walk_nonexistent_root_yields_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert_eq!(walk_project_files(&missing, &missing).count(), 0);
    }

    #[test]
    fn gitignore_includes_parent_and_scoped_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "src/generated.rs\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/.gitignore"), "local.rs\n").unwrap();

        let gitignore = build_gitignore(root, &root.join("src")).unwrap();

        assert!(gitignore.matched("src/generated.rs", false).is_ignore());
        assert!(gitignore.matched("src/local.rs", false).is_ignore());
        assert!(!gitignore.matched("src/kept.rs", false).is_ignore());
    }

    #[test]
    fn compile_glob_prefers_pattern_then_default_then_none() {
        let from_pattern = compile_glob(Some("*.rs"), Some("**/*.md")).unwrap();
        assert!(from_pattern.unwrap().matches("lib.rs"));

        let from_default = compile_glob(None, Some("**/*.rs")).unwrap();
        assert!(from_default.unwrap().matches("src/lib.rs"));

        assert!(compile_glob(None, None).unwrap().is_none());
    }

    #[test]
    fn compile_glob_reports_invalid_pattern() {
        let err = compile_glob(Some("["), None).unwrap_err().to_string();
        assert!(err.contains("Invalid glob pattern"), "{err}");
    }

    #[test]
    fn ensure_limit_nonzero_rejects_zero() {
        assert!(ensure_limit_nonzero(0).is_err());
        assert!(ensure_limit_nonzero(1).is_ok());
    }

    #[test]
    fn ensure_pattern_present_rejects_empty() {
        assert!(ensure_pattern_present("").is_err());
        assert!(ensure_pattern_present("x").is_ok());
    }

    #[test]
    fn format_truncation_reports_total_when_known() {
        let with_total = format_truncation(2, Some(5));
        assert!(
            with_total.contains("showing 2 of 5 matches"),
            "{with_total}"
        );

        let without_total = format_truncation(2, None);
        assert!(
            without_total.contains("showing first 2 matches"),
            "{without_total}"
        );
    }
}
