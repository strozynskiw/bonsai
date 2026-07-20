//! Built-in agent personas (the "modes"). This is the data backing the hardcoded
//! [`AgentMode`], so persona-dependent behavior (system prompt, the self-review
//! and planning-budget rules, and — later — the UI view + the persona cycle) reads
//! from one source of truth instead of scattered `match mode` arms.

use super::AgentMode;

/// The UI surface a persona renders when run as a mode. Wired into the layout in
/// the view phase; carried here now so the persona is the single source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PersonaView {
    /// Plain conversation — no TODO panel, no canvas.
    Chat,
    /// Conversation + the TODO side panel (today's coding agent).
    Todo,
    /// The live plan canvas (today's planning mode).
    Canvas,
}

impl PersonaView {
    /// Parse a custom agent's `view:` field; unknown/absent defaults to `Chat`
    /// (custom personas are read-only conversational by default).
    pub(crate) fn parse(view: Option<&str>) -> Self {
        match view.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("todo") => Self::Todo,
            Some("canvas") => Self::Canvas,
            _ => Self::Chat,
        }
    }
}

/// The active main-loop persona: a built-in mode or a user-defined agent (by
/// name). Not `Copy` — it holds the custom agent's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActivePersona {
    Builtin(AgentMode),
    Custom(String),
}

impl Default for ActivePersona {
    fn default() -> Self {
        Self::Builtin(AgentMode::Coding)
    }
}

impl ActivePersona {
    /// The built-in mode, if this is a built-in persona (`None` for custom).
    pub(crate) fn builtin(&self) -> Option<AgentMode> {
        match self {
            Self::Builtin(mode) => Some(*mode),
            Self::Custom(_) => None,
        }
    }

    /// The custom agent name, if this is a custom persona.
    pub(crate) fn custom_name(&self) -> Option<&str> {
        match self {
            Self::Custom(name) => Some(name),
            Self::Builtin(_) => None,
        }
    }
}

/// A built-in persona: a named, prompted way the main loop can run.
pub(crate) struct ModePersona {
    pub(crate) mode: AgentMode,
    pub(crate) name: &'static str,
    pub(crate) view: PersonaView,
    /// Runs the completion-blocking self-review pass before finishing.
    pub(crate) self_review: bool,
    /// Subject to the planning research-turn budget (read-only research that must
    /// converge on the plan canvas).
    pub(crate) planning_budget: bool,
    /// Reachable by the persona switcher. `review` is excluded — it is
    /// launched with `/review` so its diff is seeded first.
    pub(crate) cyclable: bool,
    /// Accent color spec for the composer label (a palette name or `#rrggbb`).
    pub(crate) color: &'static str,
}

impl ModePersona {
    /// The static system prompt for this persona.
    pub(crate) fn system_prompt(&self) -> &'static str {
        super::prompts::system_prompt(self.mode)
    }
}

/// Built-in personas in switcher order.
pub(crate) const BUILTIN_PERSONAS: &[ModePersona] = &[
    ModePersona {
        mode: AgentMode::Coding,
        name: "coding",
        view: PersonaView::Todo,
        self_review: true,
        planning_budget: false,
        cyclable: true,
        color: "blue",
    },
    ModePersona {
        mode: AgentMode::Planning,
        name: "planning",
        view: PersonaView::Canvas,
        self_review: false,
        planning_budget: true,
        cyclable: true,
        color: "magenta",
    },
    ModePersona {
        mode: AgentMode::Review,
        name: "review",
        view: PersonaView::Chat,
        self_review: false,
        planning_budget: false,
        cyclable: false,
        color: "amber",
    },
];

/// The persona for a mode. Every [`AgentMode`] has exactly one built-in persona.
pub(crate) fn persona_for(mode: AgentMode) -> &'static ModePersona {
    BUILTIN_PERSONAS
        .iter()
        .find(|persona| persona.mode == mode)
        .expect("every AgentMode has a built-in persona")
}

/// The ordered cycle of switcher personas: the built-in cyclable ones followed by the
/// enabled, mode-capable custom agents (name order).
pub(crate) fn persona_cycle(custom: &crate::resource::agent::AgentRegistry) -> Vec<ActivePersona> {
    let mut cycle: Vec<ActivePersona> = BUILTIN_PERSONAS
        .iter()
        .filter(|persona| persona.cyclable)
        .map(|persona| ActivePersona::Builtin(persona.mode))
        .collect();
    for def in custom.iter() {
        // Built-in subagent ids stay delegated subagents even when a legacy
        // same-name Markdown settings file is present. They must never become
        // user-selectable custom personas merely because that file omitted
        // `surface:`.
        if def.enabled && def.allows_mode() && !crate::tool::is_builtin_agent(&def.name) {
            cycle.push(ActivePersona::Custom(def.name.clone()));
        }
    }
    cycle
}

/// The next persona the switcher selects, cycling built-in + enabled custom
/// personas. Computed against a live registry snapshot so composer edits change
/// membership without a relaunch.
pub(crate) fn next_persona(
    current: &ActivePersona,
    custom: &crate::resource::agent::AgentRegistry,
) -> ActivePersona {
    let cycle = persona_cycle(custom);
    match cycle.iter().position(|candidate| candidate == current) {
        Some(index) => cycle[(index + 1) % cycle.len()].clone(),
        None => cycle.into_iter().next().unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_has_a_persona() {
        for mode in [AgentMode::Coding, AgentMode::Planning, AgentMode::Review] {
            assert_eq!(persona_for(mode).mode, mode);
        }
    }

    #[test]
    fn persona_cycle_includes_builtin_personas_and_skips_review() {
        let empty = crate::resource::agent::AgentRegistry::empty();
        let coding = ActivePersona::Builtin(AgentMode::Coding);
        let planning = ActivePersona::Builtin(AgentMode::Planning);
        assert_eq!(next_persona(&coding, &empty), planning);
        assert_eq!(next_persona(&planning, &empty), coding);
        // Review is launched via /review, not cyclable; it steps to the first
        // cyclable persona rather than re-selecting itself.
        assert_eq!(
            next_persona(&ActivePersona::Builtin(AgentMode::Review), &empty),
            coding
        );
        assert!(!persona_for(AgentMode::Review).cyclable);
    }

    #[test]
    fn enabled_custom_agents_join_the_cycle() {
        let root = tempfile::TempDir::new().unwrap();
        let home = tempfile::TempDir::new().unwrap();
        let write = |name: &str, body: &str| {
            let path = root.path().join(format!(".bonsai/agents/{name}.md"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write(
            "api",
            "---\nname: api\ndescription: d\nsurface: [mode, subagent]\n---\np",
        );
        write(
            "legacy-helper",
            "---\nname: legacy-helper\ndescription: d\n---\np",
        );
        write(
            "off",
            "---\nname: off\ndescription: d\nenabled: false\n---\np",
        );
        write(
            "helper",
            "---\nname: helper\ndescription: d\nsurface: [subagent]\n---\np",
        );
        write(
            "mode-only",
            "---\nname: mode-only\ndescription: d\nsurface: [mode]\n---\np",
        );
        write(
            "explore",
            "---\nname: explore\ndescription: legacy builtin settings\n---\np",
        );
        write(
            "review",
            "---\nname: review\ndescription: legacy builtin settings\n---\np",
        );
        let registry = crate::resource::agent::AgentRegistry::load_from(root.path(), home.path());
        let cycle = persona_cycle(&registry);
        // Built-ins first, then the enabled mode-capable custom agent only.
        assert_eq!(
            cycle.first(),
            Some(&ActivePersona::Builtin(AgentMode::Coding))
        );
        assert!(cycle.contains(&ActivePersona::Custom("api".to_string())));
        assert!(cycle.contains(&ActivePersona::Custom("mode-only".to_string())));
        assert!(!cycle.contains(&ActivePersona::Custom("off".to_string())));
        assert!(!cycle.contains(&ActivePersona::Custom("helper".to_string())));
        assert!(!cycle.contains(&ActivePersona::Custom("legacy-helper".to_string())));
        assert!(!cycle.contains(&ActivePersona::Custom("explore".to_string())));
        assert!(!cycle.contains(&ActivePersona::Custom("review".to_string())));
        // From the last built-in, the switcher steps into the custom agent.
        assert_eq!(
            next_persona(&ActivePersona::Builtin(AgentMode::Planning), &registry),
            ActivePersona::Custom("api".to_string())
        );
    }

    #[test]
    fn only_coding_self_reviews_and_only_planning_has_budget() {
        assert!(persona_for(AgentMode::Coding).self_review);
        assert!(persona_for(AgentMode::Planning).planning_budget);
        assert!(!persona_for(AgentMode::Review).self_review);
        assert!(!persona_for(AgentMode::Review).planning_budget);
    }
}
