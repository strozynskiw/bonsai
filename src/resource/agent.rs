//! Custom-subagent registry for user-owned definitions in
//! `.bonsai/agents/<name>.md` (and `.claude`/`.agents`/global). It parses their
//! frontmatter and holds their bodies (the agent's prompt). Mirrors
//! [`crate::resource::skill`]. Distinct from [`crate::subagent::SubagentRegistry`],
//! which tracks *live runs* for `/subtasks`; this one holds *definitions*.
//!
//! A custom agent may scope its `tools:` to the registry granted by its caller.
//! `model`/`effort` are honored for subagent model selection; `permission` is
//! rejected until the runtime can enforce agent-scoped permission profiles.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::resource::cap_body;
use crate::resource::discovery::{
    DiscoveredResource, Provenance, ResourceKind, discover, discover_global_only,
};
use crate::resource::frontmatter::{AgentFrontmatter, Parsed, parse_frontmatter};

/// Hard safety ceiling for a custom subagent's `max_turns` override.
pub(crate) const CUSTOM_AGENT_MAX_TURNS: usize = 500;

/// A discovered, parsed custom subagent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentDef {
    pub name: String,
    pub description: String,
    /// The markdown body — the subagent's prompt/framing (capped).
    pub instructions: String,
    /// Declared scoped tool names. `None` means the full read-only set.
    pub tools: Option<Vec<String>>,
    /// Declared model selector: either a role (`cheap`/`fast`/`smart`) or a
    /// concrete model resolved at run time.
    pub model: Option<String>,
    /// Declared reasoning-effort override (parsed at resolve time).
    pub effort: Option<String>,
    /// Optional backup model selector tried after the primary assignment.
    pub fallback_model: Option<String>,
    /// Optional reasoning-effort override for the backup assignment.
    pub fallback_effort: Option<String>,
    /// Optional subagent model-turn budget. The runtime supplies a bounded
    /// custom-agent default when absent.
    pub max_turns: Option<usize>,
    /// Whether the agent is active. A disabled agent is discovered (so `/agents`
    /// can list it) but not advertised in the index or runnable via the tool.
    pub enabled: bool,
    /// UI surface when run as a switcher persona (`chat`/`todo`/`canvas`); `None`
    /// defaults to `chat`.
    pub view: Option<String>,
    /// How the agent may run (`mode` and/or `subagent`); omission defaults to a
    /// delegated subagent so loading a helper never adds it to the switcher.
    pub surface: Option<Vec<String>>,
    /// Accent color for the persona label (a palette name or `#rrggbb`).
    pub color: Option<String>,
    pub source_path: PathBuf,
    pub provenance: Provenance,
    pub truncated: bool,
}

impl AgentDef {
    /// Whether this agent came from the global `$BONSAI_HOME` (not the project).
    pub(crate) fn is_global(&self) -> bool {
        matches!(self.provenance, Provenance::Global { .. })
    }

    /// Whether this definition may run as a user-selectable main-loop persona.
    /// The mode surface must be explicit; omission defaults to subagent-only.
    pub(crate) fn allows_mode(&self) -> bool {
        self.has_surface("mode")
    }

    /// Whether this definition may run through the delegated `agent` tool.
    /// Omission defaults to delegated-subagent use for legacy definitions.
    pub(crate) fn allows_subagent(&self) -> bool {
        self.surface.is_none() || self.has_surface("subagent")
    }

    fn has_surface(&self, requested: &str) -> bool {
        self.surface.as_ref().is_some_and(|surfaces| {
            surfaces
                .iter()
                .any(|surface| surface.trim().eq_ignore_ascii_case(requested))
        })
    }
}

/// All custom agents discovered for a project, keyed by name (sorted, byte-stable).
#[derive(Debug, Default)]
pub(crate) struct AgentRegistry {
    agents: BTreeMap<String, AgentDef>,
}

impl AgentRegistry {
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    /// Discover and parse custom agents for `project_root`, resolving the global
    /// agents dir from `$BONSAI_HOME`. Malformed agents are skipped with a warning.
    pub(crate) fn load(project_root: &Path) -> Self {
        let home = crate::storage::BonsaiPaths::discover()
            .map(|paths| paths.home_dir().to_path_buf())
            .unwrap_or_default();
        Self::load_from(project_root, &home)
    }

    /// Load only global custom agents. Project-agent bodies become subagent
    /// prompts, so they stay inert until the repository root is trusted.
    pub(crate) fn load_untrusted(project_root: &Path) -> Self {
        let home = crate::storage::BonsaiPaths::discover()
            .map(|paths| paths.home_dir().to_path_buf())
            .unwrap_or_default();
        Self::load_from_with_project_resources(project_root, &home, false)
    }

    /// Pure variant of [`load`] taking the global home explicitly, for tests.
    pub(crate) fn load_from(project_root: &Path, bonsai_home: &Path) -> Self {
        Self::load_from_with_project_resources(project_root, bonsai_home, true)
    }

    fn load_from_with_project_resources(
        project_root: &Path,
        bonsai_home: &Path,
        include_project_resources: bool,
    ) -> Self {
        let mut agents = BTreeMap::new();
        let resources = if include_project_resources {
            discover(project_root, bonsai_home, ResourceKind::Agents)
        } else {
            discover_global_only(project_root, bonsai_home, ResourceKind::Agents)
        };
        for resource in resources {
            match parse_agent(&resource) {
                Ok(def) => {
                    agents.insert(def.name.clone(), def);
                }
                Err(err) => {
                    tracing::warn!(
                        agent = %resource.name,
                        path = %resource.path.display(),
                        error = %err,
                        "skipping malformed custom agent"
                    );
                }
            }
        }
        Self { agents }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&AgentDef> {
        self.agents.get(name)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &AgentDef> {
        self.agents.values()
    }
}

/// A hot-swappable handle to the custom-agent registry. The TUI agent composer
/// reloads it after writing or deleting an agent so the running `AgentTool` (and
/// `/agents`) observe the change without a relaunch. Every holder shares one
/// `Arc`, so a write through any handle is visible to all readers.
#[derive(Clone, Debug)]
pub(crate) struct SharedAgentRegistry {
    registry: Arc<RwLock<Arc<AgentRegistry>>>,
    include_project_resources: bool,
}

/// Wrap a freshly loaded registry in a shared, swappable handle.
pub(crate) fn shared_registry(registry: impl Into<Arc<AgentRegistry>>) -> SharedAgentRegistry {
    shared_registry_with_project_resources(registry, true)
}

/// Wrap an untrusted-workspace registry; reloads retain the global-only
/// boundary until a restart after the user grants workspace trust.
pub(crate) fn shared_registry_untrusted(registry: AgentRegistry) -> SharedAgentRegistry {
    shared_registry_with_project_resources(registry, false)
}

fn shared_registry_with_project_resources(
    registry: impl Into<Arc<AgentRegistry>>,
    include_project_resources: bool,
) -> SharedAgentRegistry {
    SharedAgentRegistry {
        registry: Arc::new(RwLock::new(registry.into())),
        include_project_resources,
    }
}

/// Cheaply snapshot the current registry for reading; never holds the lock across
/// the returned value's lifetime.
pub(crate) fn snapshot(handle: &SharedAgentRegistry) -> Arc<AgentRegistry> {
    handle
        .registry
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

/// Reload the registry from disk and swap it into the shared handle in place.
pub(crate) fn reload_into(handle: &SharedAgentRegistry, project_root: &Path) {
    let reloaded = if handle.include_project_resources {
        AgentRegistry::load(project_root)
    } else {
        AgentRegistry::load_untrusted(project_root)
    };
    *handle
        .registry
        .write()
        .unwrap_or_else(|poison| poison.into_inner()) = Arc::new(reloaded);
}

fn parse_agent(resource: &DiscoveredResource) -> anyhow::Result<AgentDef> {
    let content = std::fs::read_to_string(&resource.path)?;
    let Parsed { frontmatter, body } = parse_frontmatter::<AgentFrontmatter>(&content)?;
    frontmatter.validate_supported_capabilities()?;
    if let Some(max_turns) = frontmatter.max_turns
        && !(1..=CUSTOM_AGENT_MAX_TURNS).contains(&max_turns)
    {
        anyhow::bail!(
            "agent max_turns must be between 1 and {CUSTOM_AGENT_MAX_TURNS}, got {max_turns}"
        );
    }
    if let Some(surfaces) = frontmatter.surface.as_deref() {
        anyhow::ensure!(
            !surfaces.is_empty(),
            "agent surface must contain `mode`, `subagent`, or both"
        );
        for surface in surfaces {
            anyhow::ensure!(
                matches!(
                    surface.trim().to_ascii_lowercase().as_str(),
                    "mode" | "subagent"
                ),
                "unsupported agent surface `{surface}`; expected `mode` or `subagent`"
            );
        }
    }
    let (instructions, truncated) = cap_body(body);
    Ok(AgentDef {
        name: frontmatter.name,
        description: frontmatter.description,
        instructions,
        tools: frontmatter.tools,
        model: frontmatter.model,
        effort: frontmatter.effort,
        fallback_model: frontmatter.fallback_model,
        fallback_effort: frontmatter.fallback_effort,
        max_turns: frontmatter.max_turns,
        enabled: frontmatter.enabled,
        view: frontmatter.view,
        surface: frontmatter.surface,
        color: frontmatter.color,
        source_path: resource.path.clone(),
        provenance: resource.provenance.clone(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn loads_and_parses_project_agent() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/agents/api-explorer.md"),
            "---\nname: api-explorer\ndescription: maps routes\ntools: [read, grep]\n---\nYou map HTTP routes.",
        );

        let registry = AgentRegistry::load_from(root.path(), home.path());
        let agent = registry.get("api-explorer").expect("agent");
        assert_eq!(agent.description, "maps routes");
        assert_eq!(
            agent.tools,
            Some(vec!["read".to_string(), "grep".to_string()])
        );
        assert_eq!(agent.instructions, "You map HTTP routes.");
        assert!(!agent.is_global());
    }

    #[test]
    fn restricted_workspace_skips_project_agent_prompts() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/agents/project.md"),
            "---\nname: project\ndescription: p\n---\nproject instructions",
        );
        write(
            &home.path().join("agents/global.md"),
            "---\nname: global\ndescription: g\n---\nglobal instructions",
        );

        let registry =
            AgentRegistry::load_from_with_project_resources(root.path(), home.path(), false);
        assert!(registry.get("project").is_none());
        assert!(registry.get("global").is_some());
    }

    #[test]
    fn enabled_defaults_true_and_model_effort_parse() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/agents/fast.md"),
            "---\nname: fast\ndescription: cheap explorer\nmodel: anthropic:haiku\neffort: low\nfallback_model: codex:mini\nfallback_effort: medium\nenabled: false\n---\nbody",
        );
        write(
            &root.path().join(".bonsai/agents/plain.md"),
            "---\nname: plain\ndescription: defaults\n---\nbody",
        );
        let registry = AgentRegistry::load_from(root.path(), home.path());

        let fast = registry.get("fast").expect("fast");
        assert!(!fast.enabled, "enabled: false should parse");
        assert_eq!(fast.model.as_deref(), Some("anthropic:haiku"));
        assert_eq!(fast.effort.as_deref(), Some("low"));
        assert_eq!(fast.fallback_model.as_deref(), Some("codex:mini"));
        assert_eq!(fast.fallback_effort.as_deref(), Some("medium"));

        let plain = registry.get("plain").expect("plain");
        assert!(plain.enabled, "enabled defaults to true when absent");
        assert_eq!(plain.model, None);
        assert_eq!(plain.effort, None);
        assert_eq!(plain.fallback_model, None);
        assert_eq!(plain.fallback_effort, None);
        assert_eq!(plain.max_turns, None);
    }

    #[test]
    fn surface_eligibility_defaults_to_subagent_and_honors_explicit_scope() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        for (name, surface) in [
            ("both-default", ""),
            ("mode-only", "surface: [mode]\n"),
            ("subagent-only", "surface: [subagent]\n"),
            ("both-explicit", "surface: [MODE, SubAgent]\n"),
        ] {
            write(
                &root.path().join(format!(".bonsai/agents/{name}.md")),
                &format!("---\nname: {name}\ndescription: d\n{surface}---\nprompt"),
            );
        }

        let registry = AgentRegistry::load_from(root.path(), home.path());
        let default = registry.get("both-default").expect("default agent");
        assert!(!default.allows_mode());
        assert!(default.allows_subagent());

        let mode = registry.get("mode-only").expect("mode agent");
        assert!(mode.allows_mode());
        assert!(!mode.allows_subagent());

        let subagent = registry.get("subagent-only").expect("subagent agent");
        assert!(!subagent.allows_mode());
        assert!(subagent.allows_subagent());

        let both = registry.get("both-explicit").expect("both agent");
        assert!(both.allows_mode());
        assert!(both.allows_subagent());
    }

    #[test]
    fn empty_or_unknown_surface_is_rejected_instead_of_mislabeled() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        for (name, surface) in [("empty", "[]"), ("unknown", "[worker]")] {
            write(
                &root.path().join(format!(".bonsai/agents/{name}.md")),
                &format!("---\nname: {name}\ndescription: d\nsurface: {surface}\n---\nprompt"),
            );
        }

        let registry = AgentRegistry::load_from(root.path(), home.path());
        assert!(registry.get("empty").is_none());
        assert!(registry.get("unknown").is_none());
    }

    #[test]
    fn fallback_assignment_round_trips_from_project_and_global_files() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let markdown = "---\nname: resilient\ndescription: d\nmodel: codex:openai/gpt-5.5\nfallback_model: anthropic:anthropic/claude-sonnet-4-6\nfallback_effort: medium\n---\nprompt";
        write(&home.path().join("agents/global.md"), markdown);
        write(
            &root.path().join(".bonsai/agents/project.md"),
            &markdown.replace("name: resilient", "name: project-resilient"),
        );

        let registry = AgentRegistry::load_from(root.path(), home.path());
        for name in ["resilient", "project-resilient"] {
            let agent = registry.get(name).expect("agent");
            assert_eq!(
                agent.fallback_model.as_deref(),
                Some("anthropic:anthropic/claude-sonnet-4-6")
            );
            assert_eq!(agent.fallback_effort.as_deref(), Some("medium"));
        }
        assert!(registry.get("resilient").is_some_and(AgentDef::is_global));
        assert!(
            registry
                .get("project-resilient")
                .is_some_and(|agent| !agent.is_global())
        );
    }

    #[test]
    fn custom_turn_budget_parses_and_rejects_out_of_range_values() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/agents/bounded.md"),
            "---\nname: bounded\ndescription: bounded\nmax_turns: 72\n---\nbody",
        );
        write(
            &root.path().join(".bonsai/agents/zero.md"),
            "---\nname: zero\ndescription: invalid\nmax_turns: 0\n---\nbody",
        );
        write(
            &root.path().join(".bonsai/agents/huge.md"),
            "---\nname: huge\ndescription: invalid\nmaxTurns: 501\n---\nbody",
        );

        let registry = AgentRegistry::load_from(root.path(), home.path());
        assert_eq!(
            registry.get("bounded").and_then(|def| def.max_turns),
            Some(72)
        );
        assert!(registry.get("zero").is_none());
        assert!(registry.get("huge").is_none());
    }

    #[test]
    fn malformed_agent_is_skipped() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        // Missing required `description`.
        write(
            &root.path().join(".bonsai/agents/broken.md"),
            "---\nname: broken\n---\nbody",
        );
        write(
            &root.path().join(".bonsai/agents/ok.md"),
            "---\nname: ok\ndescription: fine\n---\nbody",
        );

        let registry = AgentRegistry::load_from(root.path(), home.path());
        assert!(registry.get("broken").is_none());
        assert!(registry.get("ok").is_some());
    }

    #[test]
    fn unsupported_permission_declaration_skips_agent() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/agents/privileged.md"),
            "---\nname: privileged\ndescription: unsupported\npermission: allow-write\n---\nbody",
        );

        let registry = AgentRegistry::load_from(root.path(), home.path());
        assert!(registry.get("privileged").is_none());
    }

    #[test]
    fn project_agent_shadows_global() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/agents/x.md"),
            "---\nname: x\ndescription: project\n---\nb",
        );
        write(
            &home.path().join("agents/x.md"),
            "---\nname: x\ndescription: global\n---\nb",
        );
        write(
            &home.path().join("agents/y.md"),
            "---\nname: y\ndescription: global\n---\nb",
        );

        let registry = AgentRegistry::load_from(root.path(), home.path());
        assert_eq!(registry.get("x").unwrap().description, "project");
        assert!(!registry.get("x").unwrap().is_global());
        assert!(registry.get("y").unwrap().is_global());
    }

    #[test]
    fn restricted_workspace_loads_only_global_agents() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/agents/project.md"),
            "---\nname: project\ndescription: project-owned\n---\nbody",
        );
        write(
            &home.path().join("agents/global.md"),
            "---\nname: global\ndescription: user-owned\n---\nbody",
        );

        let registry =
            AgentRegistry::load_from_with_project_resources(root.path(), home.path(), false);
        assert!(registry.get("project").is_none());
        assert!(registry.get("global").is_some_and(AgentDef::is_global));

        let shared = shared_registry_untrusted(registry);
        assert!(!shared.include_project_resources);
    }

    #[test]
    fn tools_default_to_none_when_absent() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        write(
            &root.path().join(".bonsai/agents/plain.md"),
            "---\nname: plain\ndescription: d\n---\nprompt",
        );
        let registry = AgentRegistry::load_from(root.path(), home.path());
        assert_eq!(registry.get("plain").unwrap().tools, None);
    }

    #[test]
    fn oversized_body_is_capped() {
        let big = "x".repeat(crate::resource::MAX_RESOURCE_BODY_BYTES + 500);
        let (capped, truncated) = cap_body(big);
        assert!(truncated);
        assert!(capped.len() <= crate::resource::MAX_RESOURCE_BODY_BYTES);
    }
}
