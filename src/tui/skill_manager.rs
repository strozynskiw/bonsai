//! Row model for the **skills manager** modal (`/skills`): a snapshot of every
//! skill the project knows about — user (project/global) skills plus the
//! built-ins with their per-project disposition — annotated with whether each
//! has been loaded into the conversation this session and whether it can be
//! toggled off. Pure data + the builder here; rendering lives in
//! `crate::tui::widgets::modal::skill_manager`, disk I/O and load-into-context
//! in `crate::tui::runtime_actions`.
//!
//! Mirrors [`crate::tui::agent_composer::browser_rows`]: the modal snapshots
//! owned rows at open time and never re-locks the agent per frame.

use std::collections::BTreeSet;

use crate::resource::builtin::builtin_skill_defs;
use crate::resource::skill::{BuiltinSkillState, SkillRegistry};

/// Where a row's skill comes from and, for built-ins, its current disposition.
/// Drives the status tag and whether the row is toggleable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillRowKind {
    /// A project-local skill (`.bonsai/skills/` etc.).
    Project,
    /// A skill from the global `~/.bonsai/skills` dir.
    Global,
    /// A built-in, with its per-project state.
    Builtin(BuiltinSkillState),
}

impl SkillRowKind {
    /// The lowercase status word shown in the list and detail pane.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SkillRowKind::Project => "project",
            SkillRowKind::Global => "global",
            SkillRowKind::Builtin(state) => state.label(),
        }
    }

    /// Whether a row of this kind is advertised to the model right now (in the
    /// prompt index / loadable via the `skill` tool). Only user skills and
    /// active built-ins are.
    pub(crate) fn is_advertised(self) -> bool {
        !matches!(
            self,
            SkillRowKind::Builtin(
                BuiltinSkillState::Inactive
                    | BuiltinSkillState::Disabled
                    | BuiltinSkillState::Shadowed
            )
        )
    }
}

/// One row in the skills manager — an owned snapshot, safe to hold in modal
/// state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillRow {
    pub name: String,
    pub description: String,
    pub kind: SkillRowKind,
    /// Loaded into the conversation this session (via the `skill` tool or
    /// `/skill`).
    pub loaded: bool,
    /// True for built-ins: they can be enabled/disabled from the modal. User
    /// skills are managed by their files, so they are not toggleable here.
    pub disablable: bool,
    /// Currently disabled by the user (only meaningful when `disablable`).
    pub disabled: bool,
    /// How to activate it, when the built-in is inactive; empty otherwise.
    pub activation_hint: String,
    /// A short human source label for the detail pane (`built-in`, or the path).
    pub source: String,
    /// The full SKILL.md body, for the review pane.
    pub body: String,
}

/// Snapshot the registry into manager rows: user (project/global) skills first,
/// then every built-in with its disposition. `loaded` names mark rows whose body
/// is already in the conversation. Built-in bodies are resolved from the
/// embedded set so an inactive/disabled built-in is still reviewable.
pub(crate) fn skill_rows(registry: &SkillRegistry, loaded: &BTreeSet<String>) -> Vec<SkillRow> {
    let builtin_bodies: std::collections::BTreeMap<String, String> = builtin_skill_defs()
        .into_iter()
        .map(|(def, _)| (def.name, def.body))
        .collect();

    let mut rows = Vec::new();
    for skill in registry.user_skills() {
        rows.push(SkillRow {
            name: skill.name.clone(),
            description: skill.description.clone(),
            kind: if skill.is_global() {
                SkillRowKind::Global
            } else {
                SkillRowKind::Project
            },
            loaded: loaded.contains(&skill.name),
            disablable: false,
            disabled: false,
            activation_hint: String::new(),
            source: skill.source_path.display().to_string(),
            body: skill.body.clone(),
        });
    }
    for status in registry.builtin_status() {
        rows.push(SkillRow {
            name: status.name.clone(),
            description: status.description.clone(),
            kind: SkillRowKind::Builtin(status.state),
            loaded: loaded.contains(&status.name),
            disablable: true,
            disabled: status.state == BuiltinSkillState::Disabled,
            activation_hint: status.activation_hint.clone(),
            source: "built-in".to_string(),
            body: builtin_bodies
                .get(&status.name)
                .cloned()
                .unwrap_or_default(),
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn find<'a>(rows: &'a [SkillRow], name: &str) -> &'a SkillRow {
        rows.iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("no row for '{name}'"))
    }

    #[test]
    fn rows_cover_user_skills_and_all_builtins() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");
        write(
            &root.path().join(".bonsai/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: Ship it\n---\nrun deploy",
        );
        let registry = SkillRegistry::load_from(root.path(), home.path());

        let loaded = BTreeSet::from(["deploy".to_string()]);
        let rows = skill_rows(&registry, &loaded);

        // The user skill: project, loaded, not disablable, carries its body.
        let deploy = find(&rows, "deploy");
        assert_eq!(deploy.kind, SkillRowKind::Project);
        assert!(deploy.loaded);
        assert!(!deploy.disablable);
        assert_eq!(deploy.body, "run deploy");

        // rust-writer is an active built-in in a Rust project.
        let rust = find(&rows, "rust-writer");
        assert_eq!(rust.kind, SkillRowKind::Builtin(BuiltinSkillState::Active));
        assert!(rust.disablable);
        assert!(!rust.disabled);
        assert!(rust.kind.is_advertised());
        assert!(!rust.loaded);
        assert!(rust.body.contains("cargo"), "built-in body resolved");

        // go-writer is inactive here; still reviewable with a hint.
        let go = find(&rows, "go-writer");
        assert_eq!(go.kind, SkillRowKind::Builtin(BuiltinSkillState::Inactive));
        assert!(!go.kind.is_advertised());
        assert!(go.activation_hint.contains("go.mod"));
        assert!(!go.body.is_empty());
    }

    #[test]
    fn disabled_builtin_row_reflects_state() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");
        write(
            &root.path().join(".bonsai/skills/.disabled"),
            "rust-writer\n",
        );
        let registry = SkillRegistry::load_from(root.path(), home.path());

        let rows = skill_rows(&registry, &BTreeSet::new());
        let rust = find(&rows, "rust-writer");
        assert_eq!(
            rust.kind,
            SkillRowKind::Builtin(BuiltinSkillState::Disabled)
        );
        assert!(rust.disabled);
        assert!(rust.disablable);
        // Reviewable even while disabled.
        assert!(!rust.body.is_empty());
    }
}
