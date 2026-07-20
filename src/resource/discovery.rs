//! Discovery of on-disk resource definitions (skills, subagents) from a project
//! and the global bonsai home.
//!
//! Mirrors the steering-file precedent in [`crate::context`] but scans fixed
//! `<kind>` sub-directories instead of walking up the tree. Project definitions
//! take precedence over global ones; within a project, `.bonsai` wins over
//! `.claude` wins over `.agents`. Only file references are returned here —
//! frontmatter parsing is a separate step (see [`super::frontmatter`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A kind of discoverable resource. Each kind maps to a `<kind>` sub-directory
/// name and an on-disk layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceKind {
    Skills,
    Agents,
    Themes,
}

impl ResourceKind {
    fn dir_name(self) -> &'static str {
        match self {
            ResourceKind::Skills => "skills",
            ResourceKind::Agents => "agents",
            ResourceKind::Themes => "themes",
        }
    }
}

/// Where a discovered resource came from — surfaced later by `/skills`/`/agents`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Provenance {
    /// A project-local definition (under `.bonsai`/`.claude`/`.agents`).
    Project { dir: PathBuf },
    /// A global definition under `$BONSAI_HOME`.
    Global { dir: PathBuf },
    /// A definition compiled into the binary (see [`crate::resource::builtin`]);
    /// never produced by [`discover`], lowest shadowing precedence.
    Builtin,
}

/// A discovered resource definition file (not yet parsed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredResource {
    pub name: String,
    /// The definition file: `<name>/SKILL.md` for skills, `<name>.md` for agents.
    pub path: PathBuf,
    pub provenance: Provenance,
}

/// Collect resource definitions of `kind`, first-occurrence-wins on name
/// collision, sorted by name (byte-stable).
///
/// Precedence (highest first): project `.bonsai/<kind>`, `.claude/<kind>`,
/// `.agents/<kind>`, then global `<bonsai_home>/<kind>`. A project definition
/// thus shadows a global one of the same name. `bonsai_home` is a parameter
/// (resolved by the caller via `BonsaiPaths`) so this stays pure and testable.
pub(crate) fn discover(
    project_root: &Path,
    bonsai_home: &Path,
    kind: ResourceKind,
) -> Vec<DiscoveredResource> {
    discover_with_project_resources(project_root, bonsai_home, kind, true)
}

/// Discover only user-global definitions, deliberately skipping every
/// project-owned resource root. Used while workspace trust is restricted so a
/// repository cannot inject a skill or subagent prompt merely by being opened.
pub(crate) fn discover_global_only(
    project_root: &Path,
    bonsai_home: &Path,
    kind: ResourceKind,
) -> Vec<DiscoveredResource> {
    discover_with_project_resources(project_root, bonsai_home, kind, false)
}

fn discover_with_project_resources(
    project_root: &Path,
    bonsai_home: &Path,
    kind: ResourceKind,
    include_project: bool,
) -> Vec<DiscoveredResource> {
    let mut found: BTreeMap<String, DiscoveredResource> = BTreeMap::new();
    for (root, is_global) in resource_roots(project_root, bonsai_home, kind, include_project) {
        for (name, path) in enumerate_root(&root, kind) {
            let provenance = if is_global {
                Provenance::Global { dir: root.clone() }
            } else {
                Provenance::Project { dir: root.clone() }
            };
            found.entry(name.clone()).or_insert(DiscoveredResource {
                name,
                path,
                provenance,
            });
        }
    }
    found.into_values().collect()
}

/// The `<base>/<kind>` roots scanned for `kind`, highest precedence first, each
/// tagged `is_global`. Shared by [`discover`] and [`disabled_names`] so both
/// agree on the exact set of directories (and the empty-home guard).
fn resource_roots(
    project_root: &Path,
    bonsai_home: &Path,
    kind: ResourceKind,
    include_project: bool,
) -> Vec<(PathBuf, bool)> {
    let dir = kind.dir_name();
    let mut roots = Vec::new();
    if include_project {
        roots.extend([
            (project_root.join(".bonsai").join(dir), false),
            (project_root.join(".claude").join(dir), false),
            (project_root.join(".agents").join(dir), false),
        ]);
    }
    // Only scan the global root when a real home resolved. An empty `bonsai_home`
    // would join to the relative path "<kind>", which `read_dir` resolves against
    // the process cwd — so a stray ./skills or ./agents would leak in as "global".
    if !bonsai_home.as_os_str().is_empty() {
        roots.push((bonsai_home.join(dir), true));
    }
    roots
}

/// Names the user has opted to disable, read from a `.disabled` file in any
/// `<base>/<kind>` root (project or global). One name per line; blank lines and
/// `#` comments are ignored. The union across roots is returned — a name
/// disabled in either scope is disabled. Used to suppress built-in skills the
/// user does not want (their frontmatter can't be edited since they're
/// embedded).
pub(crate) fn disabled_names(
    project_root: &Path,
    bonsai_home: &Path,
    kind: ResourceKind,
) -> BTreeSet<String> {
    disabled_names_with_project_resources(project_root, bonsai_home, kind, true)
}

/// Like [`disabled_names`] but ignores a repository's `.disabled` file along
/// with its resource definitions while the workspace is untrusted.
pub(crate) fn disabled_names_global_only(
    project_root: &Path,
    bonsai_home: &Path,
    kind: ResourceKind,
) -> BTreeSet<String> {
    disabled_names_with_project_resources(project_root, bonsai_home, kind, false)
}

fn disabled_names_with_project_resources(
    project_root: &Path,
    bonsai_home: &Path,
    kind: ResourceKind,
    include_project: bool,
) -> BTreeSet<String> {
    let mut disabled = BTreeSet::new();
    for (root, _) in resource_roots(project_root, bonsai_home, kind, include_project) {
        let Ok(contents) = std::fs::read_to_string(root.join(".disabled")) else {
            continue;
        };
        for line in contents.lines() {
            let name = line.split('#').next().unwrap_or("").trim();
            if !name.is_empty() {
                disabled.insert(name.to_string());
            }
        }
    }
    disabled
}

/// Add or remove `name` from the project-scoped `.disabled` list
/// (`<project_root>/.bonsai/<kind>/.disabled`), the counterpart to
/// [`disabled_names`]. Returns whether the file actually changed (disabling an
/// already-listed name, or enabling an absent one, is a no-op). Other lines —
/// including `#` comments — are preserved verbatim; a name is matched as the
/// non-comment token of a line.
///
/// # Errors
///
/// Propagates filesystem and UTF-8 errors while reading or atomically replacing
/// the file. Only a missing file is treated as an empty list.
pub(crate) fn set_disabled(
    project_root: &Path,
    name: &str,
    disable: bool,
    kind: ResourceKind,
) -> std::io::Result<bool> {
    let path = project_root
        .join(".bonsai")
        .join(kind.dir_name())
        .join(".disabled");
    super::repository::update_text(&path, |existing| {
        let already = existing
            .lines()
            .any(|line| line.split('#').next().unwrap_or("").trim() == name);
        if disable == already {
            return Ok((None, false));
        }

        let new_contents = if disable {
            let mut out = existing.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(name);
            out.push('\n');
            out
        } else {
            let kept: Vec<&str> = existing
                .lines()
                .filter(|line| line.split('#').next().unwrap_or("").trim() != name)
                .collect();
            let mut out = kept.join("\n");
            if !out.is_empty() {
                out.push('\n');
            }
            out
        };
        Ok((Some(new_contents), true))
    })
}

/// Yield `(name, definition_file_path)` for each resource in `root` per the
/// kind's layout. A missing or unreadable root yields nothing.
fn enumerate_root(root: &Path, kind: ResourceKind) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue; // skip non-UTF-8 names
        };
        if file_name.starts_with('.') {
            continue; // skip hidden entries
        }
        match kind {
            ResourceKind::Skills => {
                // A skill is a directory containing SKILL.md.
                let path = entry.path();
                let manifest = path.join("SKILL.md");
                if path.is_dir() && manifest.is_file() {
                    out.push((file_name, manifest));
                }
            }
            ResourceKind::Agents => {
                // A subagent is a flat <name>.md file.
                let path = entry.path();
                if path.is_file()
                    && let Some(stem) = file_name.strip_suffix(".md")
                    && !stem.is_empty()
                {
                    out.push((stem.to_string(), path));
                }
            }
            ResourceKind::Themes => {
                // A theme is a flat <name>.toml file; the name normalizes to
                // lowercase so precedence and dedup match regardless of casing.
                let path = entry.path();
                if path.is_file()
                    && let Some(stem) = file_name.strip_suffix(".toml")
                    && !stem.is_empty()
                {
                    out.push((stem.to_ascii_lowercase(), path));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn discovers_skill_directory_with_manifest() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: d\n---\nbody",
        );
        // A skill directory without SKILL.md is ignored.
        std::fs::create_dir_all(root.path().join(".bonsai/skills/empty")).unwrap();

        let found = discover(root.path(), home.path(), ResourceKind::Skills);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "deploy");
        assert!(found[0].path.ends_with("deploy/SKILL.md"));
        assert!(matches!(found[0].provenance, Provenance::Project { .. }));
    }

    #[test]
    fn global_only_discovery_skips_project_resources() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/project-only/SKILL.md"),
            "---\nname: project-only\ndescription: d\n---\nbody",
        );
        write(
            &home.path().join("skills/global-only/SKILL.md"),
            "---\nname: global-only\ndescription: d\n---\nbody",
        );

        let found = discover_global_only(root.path(), home.path(), ResourceKind::Skills);
        let names: Vec<&str> = found
            .iter()
            .map(|resource| resource.name.as_str())
            .collect();
        assert_eq!(names, vec!["global-only"]);
        assert!(matches!(found[0].provenance, Provenance::Global { .. }));
    }

    #[test]
    fn discovers_flat_agent_files_and_skips_non_md() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/agents/fixer.md"),
            "---\nname: fixer\ndescription: d\n---\nbody",
        );
        write(&root.path().join(".bonsai/agents/notes.txt"), "ignored");

        let found = discover(root.path(), home.path(), ResourceKind::Agents);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "fixer");
    }

    #[test]
    fn discovers_flat_theme_files_and_lowercases_names() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/themes/MyTheme.toml"),
            "bg = \"#101010\"",
        );
        write(&root.path().join(".bonsai/themes/notes.md"), "ignored");

        let found = discover(root.path(), home.path(), ResourceKind::Themes);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "mytheme", "name normalizes to lowercase");
        assert!(found[0].path.ends_with("MyTheme.toml"));
    }

    #[test]
    fn project_bonsai_wins_over_claude() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/x/SKILL.md"),
            "---\nname: x\ndescription: from-bonsai\n---\nb",
        );
        write(
            &root.path().join(".claude/skills/x/SKILL.md"),
            "---\nname: x\ndescription: from-claude\n---\nb",
        );

        let found = discover(root.path(), home.path(), ResourceKind::Skills);
        assert_eq!(found.len(), 1);
        assert!(found[0].path.to_string_lossy().contains(".bonsai"));
    }

    #[test]
    fn project_shadows_global() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/x/SKILL.md"),
            "---\nname: x\ndescription: project\n---\nb",
        );
        write(
            &home.path().join("skills/x/SKILL.md"),
            "---\nname: x\ndescription: global\n---\nb",
        );
        // A global-only skill is still discovered, tagged Global.
        write(
            &home.path().join("skills/y/SKILL.md"),
            "---\nname: y\ndescription: global\n---\nb",
        );

        let found = discover(root.path(), home.path(), ResourceKind::Skills);
        let by_name: BTreeMap<_, _> = found.iter().map(|r| (r.name.as_str(), r)).collect();
        assert!(matches!(
            by_name["x"].provenance,
            Provenance::Project { .. }
        ));
        assert!(matches!(by_name["y"].provenance, Provenance::Global { .. }));
    }

    #[test]
    fn output_is_sorted_by_name() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        for n in ["zebra", "alpha", "mike"] {
            write(
                &root.path().join(format!(".bonsai/skills/{n}/SKILL.md")),
                "---\nname: n\ndescription: d\n---\nb",
            );
        }
        let found = discover(root.path(), home.path(), ResourceKind::Skills);
        let names: Vec<_> = found.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["alpha", "mike", "zebra"]);
    }

    #[test]
    fn missing_dirs_yield_empty() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        assert!(discover(root.path(), home.path(), ResourceKind::Skills).is_empty());
        assert!(discover(root.path(), home.path(), ResourceKind::Agents).is_empty());
    }

    #[test]
    fn disabled_names_unions_project_and_global_and_ignores_comments() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/.disabled"),
            "rust-writer\n# a comment\n\n  go-writer  # trailing\n",
        );
        write(&home.path().join("skills/.disabled"), "shell-writer\n");

        let disabled = disabled_names(root.path(), home.path(), ResourceKind::Skills);
        assert!(disabled.contains("rust-writer"));
        assert!(disabled.contains("go-writer"));
        assert!(disabled.contains("shell-writer"));
        assert_eq!(disabled.len(), 3);
    }

    #[test]
    fn disabled_names_empty_without_file() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        assert!(disabled_names(root.path(), home.path(), ResourceKind::Skills).is_empty());
    }

    #[test]
    fn set_disabled_round_trips_and_preserves_comments() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let path = root.path().join(".bonsai/skills/.disabled");
        write(&path, "# keep me\nother-skill\n");

        // Disabling adds the name and leaves the comment + existing entry intact.
        assert!(set_disabled(root.path(), "rust-writer", true, ResourceKind::Skills).unwrap());
        let disabled = disabled_names(root.path(), home.path(), ResourceKind::Skills);
        assert!(disabled.contains("rust-writer"));
        assert!(disabled.contains("other-skill"));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# keep me")
        );

        // Disabling again is a no-op.
        assert!(!set_disabled(root.path(), "rust-writer", true, ResourceKind::Skills).unwrap());

        // Enabling removes only that name.
        assert!(set_disabled(root.path(), "rust-writer", false, ResourceKind::Skills).unwrap());
        let disabled = disabled_names(root.path(), home.path(), ResourceKind::Skills);
        assert!(!disabled.contains("rust-writer"));
        assert!(disabled.contains("other-skill"));
        // Enabling an absent name is a no-op.
        assert!(!set_disabled(root.path(), "rust-writer", false, ResourceKind::Skills).unwrap());
    }

    #[test]
    fn set_disabled_creates_file_when_absent() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        assert!(set_disabled(root.path(), "go-writer", true, ResourceKind::Skills).unwrap());
        assert!(
            disabled_names(root.path(), home.path(), ResourceKind::Skills).contains("go-writer")
        );
    }

    #[test]
    fn set_disabled_rejects_invalid_utf8_without_replacing_the_file() {
        let root = tempfile::TempDir::new().unwrap();
        let path = root.path().join(".bonsai/skills/.disabled");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = b"keep\n\xff\n";
        std::fs::write(&path, original).unwrap();

        let error =
            set_disabled(root.path(), "rust-writer", true, ResourceKind::Skills).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn set_disabled_propagates_permission_denied_without_replacing_the_file() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::TempDir::new().unwrap();
        let path = root.path().join(".bonsai/skills/.disabled");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "keep\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = set_disabled(root.path(), "rust-writer", true, ResourceKind::Skills);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_to_string(path).unwrap(), "keep\n");
    }

    #[test]
    fn empty_home_skips_global_root() {
        // An empty `bonsai_home` must not be joined to a relative "<kind>" path
        // (which read_dir would resolve against the process cwd). Only project
        // resources should surface.
        let root = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/local/SKILL.md"),
            "---\nname: local\ndescription: d\n---\nb",
        );
        let found = discover(root.path(), Path::new(""), ResourceKind::Skills);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "local");
        assert!(matches!(found[0].provenance, Provenance::Project { .. }));
    }
}
