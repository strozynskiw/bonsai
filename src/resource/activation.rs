//! Declarative per-skill activation rules. A skill's frontmatter can
//! scope it to projects that match — so a built-in like `rust-writer` only
//! surfaces in Rust projects and plain projects pay nothing in the prefix:
//!
//! ```text
//! ---
//! name: rust-writer
//! description: …
//! activation:
//!   markers: [Cargo.toml, rust-toolchain.toml]
//!   extensions: [rs]
//! ---
//! ```
//!
//! Semantics: the skill activates when **any** marker exists at the project
//! root **or** any file with a listed extension exists in the tree. A missing
//! or empty `activation` block means always active, so existing skills keep
//! their behavior. Rules apply uniformly to built-in, project, and global
//! skills — evaluation happens once at registry load, through a shared
//! [`ActivationProbe`] so many extension-gated skills cost one tree walk, not
//! one each.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Deserialize;

/// Bound on directory entries visited by the extension scan, so activation
/// stays in the same "cheap at startup" cost class as skill discovery even in
/// huge trees. Markers are checked first and never walk.
const MAX_SCAN_ENTRIES: usize = 10_000;

/// A skill's `activation:` frontmatter block. Fields compose with OR: any
/// matching marker or extension activates the skill.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct ActivationRule {
    /// Project-root-relative files or directories whose presence activates the
    /// skill (e.g. `Cargo.toml`, `go.mod`, `.github/workflows`).
    #[serde(default)]
    pub markers: Vec<String>,
    /// File extensions (with or without the leading dot) that activate the
    /// skill when any matching file exists in the project tree.
    #[serde(default)]
    pub extensions: Vec<String>,
}

impl ActivationRule {
    /// A short human hint describing what activates this rule, e.g.
    /// `Cargo.toml, rust-toolchain.toml, or *.rs files`. Empty when the rule is
    /// unconditional (always active). Shown by `/skills` next to an inactive
    /// built-in so the user knows how to turn it on.
    pub(crate) fn hint(&self) -> String {
        let mut parts: Vec<String> = self.markers.clone();
        parts.extend(
            self.extensions
                .iter()
                .map(|ext| format!("*.{}", ext.trim_start_matches('.'))),
        );
        parts.join(", ")
    }

    /// Whether the skill should be active for the probed project. An empty
    /// rule is vacuously active. Markers are O(1) existence checks and are
    /// tried before the probe's (shared, bounded) extension scan.
    pub(crate) fn is_active(&self, probe: &mut ActivationProbe) -> bool {
        if self.markers.is_empty() && self.extensions.is_empty() {
            return true;
        }
        if self
            .markers
            .iter()
            .any(|marker| probe.root.join(marker).exists())
        {
            return true;
        }
        !self.extensions.is_empty() && probe.has_any_extension(&self.extensions)
    }
}

/// Shared project facts for evaluating many rules against one root. The
/// extension inventory is computed by a single gitignore-aware walk on first
/// use and reused for every subsequent rule, so registry load does at most one
/// walk regardless of how many skills are extension-gated.
pub(crate) struct ActivationProbe<'a> {
    root: &'a Path,
    /// Lowercased extensions present under `root`; `None` until first needed.
    extensions: Option<BTreeSet<String>>,
}

impl<'a> ActivationProbe<'a> {
    pub(crate) fn new(root: &'a Path) -> Self {
        Self {
            root,
            extensions: None,
        }
    }

    fn has_any_extension(&mut self, wanted: &[String]) -> bool {
        let found = self
            .extensions
            .get_or_insert_with(|| scan_extensions(self.root));
        wanted
            .iter()
            .any(|want| found.contains(&want.trim_start_matches('.').to_ascii_lowercase()))
    }
}

/// Inventory the file extensions under `root` (gitignore-aware, hidden entries
/// skipped), visiting at most [`MAX_SCAN_ENTRIES`] entries. A capped scan is a
/// partial inventory — affected skills stay inactive, and markers remain the
/// reliable signal for very large trees.
fn scan_extensions(root: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut visited = 0usize;
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        visited += 1;
        if visited > MAX_SCAN_ENTRIES {
            tracing::debug!(
                root = %root.display(),
                "activation extension scan hit its entry cap; inventory is partial"
            );
            break;
        }
        if entry.file_type().is_some_and(|ft| ft.is_file())
            && let Some(ext) = entry.path().extension().and_then(|ext| ext.to_str())
        {
            found.insert(ext.to_ascii_lowercase());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn rule(markers: &[&str], extensions: &[&str]) -> ActivationRule {
        ActivationRule {
            markers: markers.iter().map(|s| s.to_string()).collect(),
            extensions: extensions.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn active(rule: &ActivationRule, root: &Path) -> bool {
        rule.is_active(&mut ActivationProbe::new(root))
    }

    #[test]
    fn empty_rule_is_always_active() {
        let root = tempfile::TempDir::new().unwrap();
        assert!(active(&ActivationRule::default(), root.path()));
    }

    #[test]
    fn marker_presence_activates() {
        let root = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");
        assert!(active(&rule(&["Cargo.toml"], &[]), root.path()));
        assert!(!active(&rule(&["go.mod"], &[]), root.path()));
    }

    #[test]
    fn nested_marker_path_is_root_relative() {
        let root = tempfile::TempDir::new().unwrap();
        write(&root.path().join(".github/workflows/ci.yml"), "on: push");
        assert!(active(&rule(&[".github/workflows"], &[]), root.path()));
    }

    #[test]
    fn extension_match_activates_with_and_without_dot() {
        let root = tempfile::TempDir::new().unwrap();
        write(&root.path().join("src/main.rs"), "fn main() {}");
        assert!(active(&rule(&[], &["rs"]), root.path()));
        assert!(active(&rule(&[], &[".rs"]), root.path()));
        assert!(!active(&rule(&[], &["go"]), root.path()));
    }

    #[test]
    fn extension_compare_is_case_insensitive() {
        let root = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Lib.RS"), "");
        assert!(active(&rule(&[], &["rs"]), root.path()));
    }

    #[test]
    fn marker_short_circuits_before_extension_scan() {
        // Both signals present; either alone suffices, together still active.
        let root = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");
        write(&root.path().join("src/lib.rs"), "");
        assert!(active(&rule(&["Cargo.toml"], &["rs"]), root.path()));
        // Marker absent but extension present: the OR still activates.
        assert!(active(&rule(&["go.mod"], &["rs"]), root.path()));
    }

    #[test]
    fn no_signal_means_inactive() {
        let root = tempfile::TempDir::new().unwrap();
        write(&root.path().join("README.md"), "# hi");
        assert!(!active(&rule(&["Cargo.toml"], &["rs"]), root.path()));
    }

    #[test]
    fn one_probe_serves_many_rules_with_one_scan() {
        let root = tempfile::TempDir::new().unwrap();
        write(&root.path().join("app.py"), "");
        write(&root.path().join("index.html"), "");

        let mut probe = ActivationProbe::new(root.path());
        assert!(rule(&[], &["py"]).is_active(&mut probe));
        assert!(rule(&[], &["html"]).is_active(&mut probe));
        assert!(!rule(&[], &["rs"]).is_active(&mut probe));
        // The inventory was computed once and memoized.
        assert!(probe.extensions.is_some());
    }
}
