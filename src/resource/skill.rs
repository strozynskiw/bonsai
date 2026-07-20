//! The skill registry: discover `SKILL.md` definitions, parse their
//! frontmatter, and hold their bodies for on-demand loading. On-disk skills
//! merge with [`crate::resource::builtin`] skills (project > global >
//! built-in), and every skill's `activation:` rule is evaluated against the
//! project root at load — an inactive skill never enters the registry, so a
//! non-Rust project never sees `rust-writer`. The byte-stable
//! [`SkillRegistry::index_section`] advertises what exists in the cacheable
//! prompt prefix; full bodies load only when the model calls the `skill` tool or
//! the user runs `/skill <name>`, so dynamism stays in the conversation tail.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::resource::activation::{ActivationProbe, ActivationRule};
use crate::resource::builtin::builtin_skill_defs;
use crate::resource::discovery::{
    DiscoveredResource, Provenance, ResourceKind, disabled_names, disabled_names_global_only,
    discover, discover_global_only,
};
use crate::resource::frontmatter::{Parsed, SkillFrontmatter, parse_frontmatter};
use crate::resource::{cap_body, render_index_section};

/// Cap the advertised index so a directory full of skills can't bloat the
/// always-on prefix; the remainder collapses into a `…and N more` line.
const SKILLS_INDEX_MAX: usize = 50;
/// Heading + how-to for the cacheable skills index. Static so the index stays
/// byte-stable across sessions.
const SKILLS_INDEX_HEADING: &str = "## Skills\nFocused instruction sets available on demand. When one is relevant, call the `skill` tool with its name to load its full instructions before doing the work.";

/// A discovered, parsed skill: its advertised name/description plus the body that
/// loads on demand, with provenance for trust attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillDef {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source_path: PathBuf,
    pub provenance: Provenance,
    pub truncated: bool,
}

impl SkillDef {
    /// Whether this skill came from the global `$BONSAI_HOME` (not the project's
    /// git history) — used to attach a trust note when its body loads.
    pub(crate) fn is_global(&self) -> bool {
        matches!(self.provenance, Provenance::Global { .. })
    }

    /// Whether this skill ships inside the bonsai binary (see
    /// [`crate::resource::builtin`]).
    pub(crate) fn is_builtin(&self) -> bool {
        matches!(self.provenance, Provenance::Builtin)
    }

    /// Render the skill body for loading into the conversation, framed as trusted
    /// project instructions (skills are repo-trusted, like `AGENTS.md`). A global
    /// or built-in skill gets a one-line provenance note since it isn't in the
    /// project tree.
    pub(crate) fn render_for_context(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# Skill: {}\n", self.name));
        match self.provenance {
            Provenance::Global { .. } => out.push_str(
                "(Loaded from your global ~/.bonsai skills — not part of this project's git history.)\n",
            ),
            Provenance::Builtin => out.push_str(
                "(Built-in bonsai skill, active because this project matches its activation rules.)\n",
            ),
            Provenance::Project { .. } => {}
        }
        out.push_str(
            "(Trusted project instructions loaded on demand. Follow them for this task.)\n\n",
        );
        out.push_str(&self.body);
        if self.truncated {
            out.push_str("\n…(skill body truncated)");
        }
        out
    }
}

/// Why a built-in skill is or isn't advertised for this project. Only `Active`
/// built-ins enter the prompt index and the `skill` tool; the rest are surfaced
/// by `/skills` so the user can see the full set and its dispositions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinSkillState {
    /// Activation conditions met and not disabled — advertised and loadable.
    Active,
    /// Activation conditions not met for this project (e.g. no Rust files).
    Inactive,
    /// Turned off by the user via a `.disabled` list.
    Disabled,
    /// Replaced by a project/global skill of the same name.
    Shadowed,
}

impl BuiltinSkillState {
    /// Whether this built-in is advertised to the model (conditions met, not
    /// disabled or shadowed).
    pub(crate) fn is_active(self) -> bool {
        matches!(self, BuiltinSkillState::Active)
    }

    /// The lowercase word `/skills` prints in parentheses for this state.
    pub(crate) fn label(self) -> &'static str {
        match self {
            BuiltinSkillState::Active => "active",
            BuiltinSkillState::Inactive => "inactive",
            BuiltinSkillState::Disabled => "disabled",
            BuiltinSkillState::Shadowed => "shadowed by a project/global skill",
        }
    }
}

/// A built-in skill's disposition for this project, for the `/skills` listing.
/// Retained for every built-in regardless of state (unlike the active `skills`
/// map, which holds only advertised skills).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuiltinSkillStatus {
    pub name: String,
    pub description: String,
    pub state: BuiltinSkillState,
    /// How to activate it when [`state`](Self::state) is `Inactive`; empty
    /// otherwise (or when the rule is unconditional).
    pub activation_hint: String,
}

/// All skills discovered for a project, keyed by name (sorted, byte-stable),
/// plus the disposition of every built-in for the `/skills` listing.
#[derive(Debug, Default)]
pub(crate) struct SkillRegistry {
    skills: BTreeMap<String, SkillDef>,
    builtin_status: Vec<BuiltinSkillStatus>,
}

impl SkillRegistry {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Discover and parse skills for `project_root`, resolving the global skills
    /// dir from `$BONSAI_HOME`. Malformed skills are skipped with a warning.
    pub(crate) fn load(project_root: &Path) -> Self {
        let home = crate::storage::BonsaiPaths::discover()
            .map(|paths| paths.home_dir().to_path_buf())
            .unwrap_or_default();
        Self::load_from(project_root, &home)
    }

    /// Load only global/built-in skills. Project-owned skill bodies are
    /// instructions, so an untrusted workspace must not advertise or load
    /// them merely because the repository was opened.
    pub(crate) fn load_untrusted(project_root: &Path) -> Self {
        let home = crate::storage::BonsaiPaths::discover()
            .map(|paths| paths.home_dir().to_path_buf())
            .unwrap_or_default();
        Self::load_from_with_project_resources(project_root, &home, false)
    }

    /// Pure variant of [`load`] taking the global home explicitly, for tests.
    ///
    /// Precedence on a name collision: project > global > built-in. A skill
    /// whose `activation:` rule doesn't match `project_root` is skipped
    /// entirely — not advertised, not loadable — regardless of provenance. A
    /// built-in named in a `.disabled` list is suppressed even when active.
    pub(crate) fn load_from(project_root: &Path, bonsai_home: &Path) -> Self {
        Self::load_from_with_project_resources(project_root, bonsai_home, true)
    }

    fn load_from_with_project_resources(
        project_root: &Path,
        bonsai_home: &Path,
        include_project_resources: bool,
    ) -> Self {
        let mut probe = ActivationProbe::new(project_root);
        let disabled = if include_project_resources {
            disabled_names(project_root, bonsai_home, ResourceKind::Skills)
        } else {
            disabled_names_global_only(project_root, bonsai_home, ResourceKind::Skills)
        };
        let mut skills = BTreeMap::new();
        let resources = if include_project_resources {
            discover(project_root, bonsai_home, ResourceKind::Skills)
        } else {
            discover_global_only(project_root, bonsai_home, ResourceKind::Skills)
        };
        for resource in resources {
            match parse_skill(&resource) {
                Ok((def, activation)) => {
                    if !is_active(activation.as_ref(), &mut probe) {
                        tracing::debug!(skill = %def.name, "skill inactive for this project");
                        continue;
                    }
                    skills.insert(def.name.clone(), def);
                }
                Err(err) => {
                    tracing::warn!(
                        skill = %resource.name,
                        path = %resource.path.display(),
                        error = %err,
                        "skipping malformed skill"
                    );
                }
            }
        }
        // Merge built-ins (lowest precedence) and record every one's state, so
        // `/skills` can show the full set — active, inactive, disabled, shadowed.
        let mut builtin_status = Vec::new();
        for (def, activation) in builtin_skill_defs() {
            let state = if skills.contains_key(&def.name) {
                BuiltinSkillState::Shadowed
            } else if disabled.contains(&def.name) {
                BuiltinSkillState::Disabled
            } else if is_active(activation.as_ref(), &mut probe) {
                BuiltinSkillState::Active
            } else {
                BuiltinSkillState::Inactive
            };
            if state == BuiltinSkillState::Active {
                skills.insert(def.name.clone(), def.clone());
            }
            builtin_status.push(BuiltinSkillStatus {
                name: def.name,
                description: def.description,
                state,
                activation_hint: match (state, activation) {
                    (BuiltinSkillState::Inactive, Some(rule)) => rule.hint(),
                    _ => String::new(),
                },
            });
        }
        builtin_status.sort_by(|a, b| a.name.cmp(&b.name));
        Self {
            skills,
            builtin_status,
        }
    }

    /// Whether any skill is *advertised* (active) — built-ins recorded as
    /// inactive/disabled/shadowed don't count. Only tests query this now; the
    /// `/skills` view renders from [`user_skills`](Self::user_skills) and
    /// [`builtin_status`](Self::builtin_status) directly.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&SkillDef> {
        self.skills.get(name)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &SkillDef> {
        self.skills.values()
    }

    /// Whether any user-provided (non-built-in) skill is registered — gates the
    /// `skill` tool's presence in the SMOL registry.
    pub(crate) fn has_user_skills(&self) -> bool {
        self.skills.values().any(|skill| !skill.is_builtin())
    }

    /// User-provided (project/global) skills only — the built-ins are surfaced
    /// separately via [`builtin_status`](Self::builtin_status).
    pub(crate) fn user_skills(&self) -> impl Iterator<Item = &SkillDef> {
        self.skills.values().filter(|skill| !skill.is_builtin())
    }

    /// The disposition of every built-in skill for this project (active,
    /// inactive, disabled, or shadowed), sorted by name — the data `/skills`
    /// renders so the user sees which built-ins apply and how to change that.
    pub(crate) fn builtin_status(&self) -> &[BuiltinSkillStatus] {
        &self.builtin_status
    }

    /// The byte-stable cacheable-prefix section advertising available skills, or
    /// an empty string when there are none (so plain projects pay nothing).
    pub(crate) fn index_section(&self) -> String {
        Self::render_index(self.skills.values())
    }

    /// Like [`index_section`](Self::index_section) but listing only
    /// user-provided (project/global) skills — built-ins excluded. This is the
    /// index SMOL mode advertises: the user opted into their own skills, while
    /// built-in guidance would burn the deliberately tiny SMOL prompt budget.
    pub(crate) fn user_index_section(&self) -> String {
        Self::render_index(self.skills.values().filter(|skill| !skill.is_builtin()))
    }

    fn render_index<'a>(skills: impl Iterator<Item = &'a SkillDef>) -> String {
        let entries: Vec<(String, String)> = skills
            .map(|skill| (skill.name.clone(), skill.description.clone()))
            .collect();
        if entries.is_empty() {
            return String::new();
        }
        render_index_section(SKILLS_INDEX_HEADING, &entries, SKILLS_INDEX_MAX)
    }
}

/// A hot-swappable handle to the skill registry, mirroring
/// [`crate::resource::agent::SharedAgentRegistry`]. The agent and the `skill`
/// tool share one handle, so enabling/disabling a skill mid-session (which
/// rewrites `.disabled` and calls [`reload_into`]) is visible to both — the
/// tool stops offering a disabled skill and the agent re-renders the prompt
/// index — without a relaunch.
#[derive(Clone, Debug)]
pub(crate) struct SharedSkillRegistry {
    registry: Arc<RwLock<Arc<SkillRegistry>>>,
    include_project_resources: bool,
}

/// Wrap a freshly loaded registry in a shared, swappable handle.
pub(crate) fn shared_registry(registry: SkillRegistry) -> SharedSkillRegistry {
    shared_registry_with_project_resources(registry, true)
}

/// Wrap a registry that must remain global/built-in only for the whole
/// session. Reloads preserve this boundary until the user trusts the workspace
/// and restarts.
pub(crate) fn shared_registry_untrusted(registry: SkillRegistry) -> SharedSkillRegistry {
    shared_registry_with_project_resources(registry, false)
}

fn shared_registry_with_project_resources(
    registry: SkillRegistry,
    include_project_resources: bool,
) -> SharedSkillRegistry {
    SharedSkillRegistry {
        registry: Arc::new(RwLock::new(Arc::new(registry))),
        include_project_resources,
    }
}

/// Cheaply snapshot the current registry for reading; never holds the lock
/// across the returned value's lifetime.
pub(crate) fn snapshot(handle: &SharedSkillRegistry) -> Arc<SkillRegistry> {
    handle
        .registry
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

/// Reload the registry from disk (picking up `.disabled` edits and any new/
/// removed skill files) and swap it into the shared handle in place.
pub(crate) fn reload_into(handle: &SharedSkillRegistry, project_root: &Path) {
    let reloaded = if handle.include_project_resources {
        SkillRegistry::load(project_root)
    } else {
        SkillRegistry::load_untrusted(project_root)
    };
    *handle
        .registry
        .write()
        .unwrap_or_else(|poison| poison.into_inner()) = Arc::new(reloaded);
}

fn is_active(activation: Option<&ActivationRule>, probe: &mut ActivationProbe) -> bool {
    activation.is_none_or(|rule| rule.is_active(probe))
}

fn parse_skill(
    resource: &DiscoveredResource,
) -> anyhow::Result<(SkillDef, Option<ActivationRule>)> {
    let content = std::fs::read_to_string(&resource.path)?;
    let Parsed { frontmatter, body } = parse_frontmatter::<SkillFrontmatter>(&content)?;
    frontmatter.validate_supported_capabilities()?;
    let (body, truncated) = cap_body(body);
    let def = SkillDef {
        name: frontmatter.name,
        description: frontmatter.description,
        body,
        source_path: resource.path.clone(),
        provenance: resource.provenance.clone(),
        truncated,
    };
    Ok((def, frontmatter.activation))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn loads_and_parses_project_skill() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: Ship the release\n---\nRun the deploy script.",
        );

        let registry = SkillRegistry::load_from(root.path(), home.path());
        let skill = registry.get("deploy").expect("deploy skill");
        assert_eq!(skill.description, "Ship the release");
        assert_eq!(skill.body, "Run the deploy script.");
        assert!(!skill.is_global());
    }

    #[test]
    fn restricted_workspace_skips_project_skill_bodies() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/project/SKILL.md"),
            "---\nname: project\ndescription: p\n---\nproject instructions",
        );
        write(
            &home.path().join("skills/global/SKILL.md"),
            "---\nname: global\ndescription: g\n---\nglobal instructions",
        );

        let registry =
            SkillRegistry::load_from_with_project_resources(root.path(), home.path(), false);
        assert!(registry.get("project").is_none());
        assert!(registry.get("global").is_some());
    }

    #[test]
    fn malformed_skill_is_skipped_not_fatal() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        // Missing required `description`.
        write(
            &root.path().join(".bonsai/skills/broken/SKILL.md"),
            "---\nname: broken\n---\nbody",
        );
        write(
            &root.path().join(".bonsai/skills/ok/SKILL.md"),
            "---\nname: ok\ndescription: fine\n---\nbody",
        );

        let registry = SkillRegistry::load_from(root.path(), home.path());
        assert!(registry.get("broken").is_none());
        assert!(registry.get("ok").is_some());
    }

    #[test]
    fn unsupported_capability_declarations_skip_skills() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        for (name, declaration) in [
            ("tool-scoped", "allowed-tools: [read]"),
            ("model-scoped", "model: codex:gpt-5.4"),
            ("effort-scoped", "effort: high"),
        ] {
            write(
                &root.path().join(format!(".bonsai/skills/{name}/SKILL.md")),
                &format!("---\nname: {name}\ndescription: unsupported\n{declaration}\n---\nbody"),
            );
        }

        let registry = SkillRegistry::load_from(root.path(), home.path());
        for name in ["tool-scoped", "model-scoped", "effort-scoped"] {
            assert!(registry.get(name).is_none(), "{name} must be rejected");
        }
    }

    #[test]
    fn plain_project_advertises_only_product_skills() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let registry = SkillRegistry::load_from(root.path(), home.path());
        // A project matching no ecosystem rule still advertises the
        // always-active product skills — and nothing else.
        let names: Vec<&str> = registry.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, crate::resource::builtin::ALWAYS_ACTIVE_BUILTINS);
        assert!(registry.index_section().starts_with("## Skills"));

        // Disabling them restores the original no-entries → no-bytes contract.
        write(
            &root.path().join(".bonsai/skills/.disabled"),
            &crate::resource::builtin::ALWAYS_ACTIVE_BUILTINS.join("\n"),
        );
        let registry = SkillRegistry::load_from(root.path(), home.path());
        assert!(registry.is_empty());
        assert_eq!(registry.index_section(), "");
    }

    #[test]
    fn index_section_lists_skills_sorted() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/zebra/SKILL.md"),
            "---\nname: zebra\ndescription: z\n---\nb",
        );
        write(
            &root.path().join(".bonsai/skills/alpha/SKILL.md"),
            "---\nname: alpha\ndescription: a\n---\nb",
        );

        let section = SkillRegistry::load_from(root.path(), home.path()).index_section();
        assert!(section.starts_with("## Skills"));
        let alpha = section.find("- alpha").unwrap();
        let zebra = section.find("- zebra").unwrap();
        assert!(alpha < zebra);
    }

    #[test]
    fn project_skill_shadows_global_of_same_name() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/x/SKILL.md"),
            "---\nname: x\ndescription: project\n---\nproject body",
        );
        write(
            &home.path().join("skills/x/SKILL.md"),
            "---\nname: x\ndescription: global\n---\nglobal body",
        );

        let registry = SkillRegistry::load_from(root.path(), home.path());
        let skill = registry.get("x").unwrap();
        assert_eq!(skill.description, "project");
        assert!(!skill.is_global());
    }

    #[test]
    fn restricted_workspace_loads_only_global_and_builtin_skills() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");
        write(
            &root.path().join(".bonsai/skills/project/SKILL.md"),
            "---\nname: project\ndescription: project-owned\n---\nbody",
        );
        write(
            &home.path().join("skills/global/SKILL.md"),
            "---\nname: global\ndescription: user-owned\n---\nbody",
        );

        let registry =
            SkillRegistry::load_from_with_project_resources(root.path(), home.path(), false);
        assert!(registry.get("project").is_none());
        assert!(registry.get("global").is_some_and(SkillDef::is_global));
        assert!(
            registry
                .get("rust-writer")
                .is_some_and(SkillDef::is_builtin)
        );

        let shared = shared_registry_untrusted(registry);
        assert!(!shared.include_project_resources);
    }

    #[test]
    fn global_skill_renders_with_provenance_note() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &home.path().join("skills/g/SKILL.md"),
            "---\nname: g\ndescription: global\n---\nthe body",
        );

        let registry = SkillRegistry::load_from(root.path(), home.path());
        let skill = registry.get("g").unwrap();
        assert!(skill.is_global());
        let rendered = skill.render_for_context();
        assert!(rendered.contains("global ~/.bonsai skills"));
        assert!(rendered.contains("the body"));
    }

    #[test]
    fn builtin_rust_writer_surfaces_in_rust_project() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");

        let registry = SkillRegistry::load_from(root.path(), home.path());
        let skill = registry.get("rust-writer").expect("built-in rust-writer");
        assert!(skill.is_builtin());
        assert!(registry.index_section().contains("- rust-writer"));
    }

    #[test]
    fn builtin_stays_hidden_in_non_matching_project() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("README.md"), "# plain project");

        let registry = SkillRegistry::load_from(root.path(), home.path());
        assert!(registry.get("rust-writer").is_none());
        // Only the always-active product skills remain; no ecosystem writer
        // leaks into a project it doesn't match.
        assert!(
            registry
                .iter()
                .all(|skill| !skill.name.ends_with("-writer"))
        );
        assert!(registry.get("customize").is_some());
    }

    #[test]
    fn project_skill_shadows_builtin_of_same_name() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");
        write(
            &root.path().join(".bonsai/skills/rust-writer/SKILL.md"),
            "---\nname: rust-writer\ndescription: our own\n---\nproject body",
        );

        let registry = SkillRegistry::load_from(root.path(), home.path());
        let skill = registry.get("rust-writer").unwrap();
        assert!(!skill.is_builtin());
        assert_eq!(skill.description, "our own");
    }

    fn builtin_state(registry: &SkillRegistry, name: &str) -> BuiltinSkillState {
        registry
            .builtin_status()
            .iter()
            .find(|status| status.name == name)
            .unwrap_or_else(|| panic!("no builtin status for '{name}'"))
            .state
    }

    #[test]
    fn builtin_status_records_active_and_inactive() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");

        let registry = SkillRegistry::load_from(root.path(), home.path());
        // Every built-in is accounted for, sorted by name.
        assert!(!registry.builtin_status().is_empty());
        assert_eq!(
            builtin_state(&registry, "rust-writer"),
            BuiltinSkillState::Active
        );
        // A non-Rust built-in is present but inactive, with an activation hint.
        assert_eq!(
            builtin_state(&registry, "go-writer"),
            BuiltinSkillState::Inactive
        );
        let go = registry
            .builtin_status()
            .iter()
            .find(|status| status.name == "go-writer")
            .unwrap();
        assert!(go.activation_hint.contains("go.mod"));
    }

    #[test]
    fn disabled_builtin_is_suppressed_and_marked() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");
        write(
            &root.path().join(".bonsai/skills/.disabled"),
            "rust-writer\n",
        );

        let registry = SkillRegistry::load_from(root.path(), home.path());
        // Suppressed from the advertised set and the `skill` tool.
        assert!(registry.get("rust-writer").is_none());
        assert!(!registry.index_section().contains("rust-writer"));
        // But surfaced in the status list as disabled.
        assert_eq!(
            builtin_state(&registry, "rust-writer"),
            BuiltinSkillState::Disabled
        );
    }

    #[test]
    fn shadowed_builtin_is_marked_shadowed() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");
        write(
            &root.path().join(".bonsai/skills/rust-writer/SKILL.md"),
            "---\nname: rust-writer\ndescription: our own\n---\nbody",
        );

        let registry = SkillRegistry::load_from(root.path(), home.path());
        assert_eq!(
            builtin_state(&registry, "rust-writer"),
            BuiltinSkillState::Shadowed
        );
    }

    #[test]
    fn disabled_wins_over_shadowed_is_not_the_case_shadow_takes_precedence() {
        // A project skill of the same name AND a disable entry: the project
        // skill is what actually loads, so the built-in reads as Shadowed, not
        // Disabled (shadow is checked first).
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");
        write(
            &root.path().join(".bonsai/skills/rust-writer/SKILL.md"),
            "---\nname: rust-writer\ndescription: our own\n---\nbody",
        );
        write(
            &root.path().join(".bonsai/skills/.disabled"),
            "rust-writer\n",
        );

        let registry = SkillRegistry::load_from(root.path(), home.path());
        assert!(
            registry.get("rust-writer").is_some(),
            "the project skill loads"
        );
        assert_eq!(
            builtin_state(&registry, "rust-writer"),
            BuiltinSkillState::Shadowed
        );
    }

    #[test]
    fn builtin_renders_with_provenance_note() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");

        let registry = SkillRegistry::load_from(root.path(), home.path());
        let rendered = registry.get("rust-writer").unwrap().render_for_context();
        assert!(rendered.contains("Built-in bonsai skill"));
        assert!(rendered.contains("cargo check"));
    }

    #[test]
    fn user_index_section_excludes_builtins() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");
        write(
            &root.path().join(".bonsai/skills/deploy/SKILL.md"),
            "---\nname: deploy\ndescription: Ship it\n---\nbody",
        );

        let registry = SkillRegistry::load_from(root.path(), home.path());
        assert!(registry.has_user_skills());
        let full = registry.index_section();
        assert!(full.contains("- rust-writer"));
        assert!(full.contains("- deploy"));

        let user_only = registry.user_index_section();
        assert!(user_only.contains("- deploy"));
        assert!(!user_only.contains("rust-writer"));
    }

    #[test]
    fn user_index_section_is_empty_with_only_builtins() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(&root.path().join("Cargo.toml"), "[package]");

        let registry = SkillRegistry::load_from(root.path(), home.path());
        assert!(!registry.is_empty());
        assert!(!registry.has_user_skills());
        assert_eq!(registry.user_index_section(), "");
    }

    #[test]
    fn on_disk_skill_with_activation_rule_is_gated() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/skills/go-deploy/SKILL.md"),
            "---\nname: go-deploy\ndescription: d\nactivation:\n  markers: [go.mod]\n---\nbody",
        );

        let registry = SkillRegistry::load_from(root.path(), home.path());
        assert!(
            registry.get("go-deploy").is_none(),
            "no go.mod, so the skill must stay inactive"
        );

        write(&root.path().join("go.mod"), "module demo");
        let registry = SkillRegistry::load_from(root.path(), home.path());
        assert!(registry.get("go-deploy").is_some());
    }

    #[test]
    fn oversized_body_is_capped() {
        let big = "x".repeat(crate::resource::MAX_RESOURCE_BODY_BYTES + 1_000);
        let (capped, truncated) = cap_body(big);
        assert!(truncated);
        assert!(capped.len() <= crate::resource::MAX_RESOURCE_BODY_BYTES);
    }
}
