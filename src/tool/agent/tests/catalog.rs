use super::*;

#[test]
fn builtins_and_custom_agents_have_distinct_budgets() {
    assert_eq!(
        builtin_agent("explore").map(|spec| spec.budget.limits()),
        Some(SubagentRunLimits {
            max_iterations: EXPLORE_MAX_ITERATIONS,
            timeout: Duration::from_secs(450),
            conclude_timeout: Duration::from_secs(180),
        })
    );
    assert_eq!(
        builtin_agent("review").map(|spec| spec.budget.limits()),
        builtin_agent("security-review").map(|spec| spec.budget.limits())
    );
    assert_eq!(
        builtin_agent("review").map(|spec| spec.budget.limits().max_iterations),
        Some(40)
    );
    assert_eq!(
        builtin_agent("research").map(|spec| spec.budget.limits().max_iterations),
        Some(40)
    );
    assert_eq!(
        custom_subagent_limits(None).max_iterations,
        CUSTOM_SUBAGENT_DEFAULT_MAX_ITERATIONS
    );
    assert_eq!(custom_subagent_limits(Some(72)).max_iterations, 72);
    assert_eq!(
        custom_subagent_limits(Some(usize::MAX)).max_iterations,
        crate::resource::agent::CUSTOM_AGENT_MAX_TURNS
    );
}

#[test]
fn frontmatter_model_effort_becomes_an_override() {
    let custom = custom_registry(&[(
        "fast",
        "---\nname: fast\ndescription: d\nmodel: f\neffort: low\n---\nprompt",
    )]);
    let tool = agent_tool_with(sub_registry_with(&["read"]), custom);
    let resolved = tool.resolve("fast").unwrap();
    let over = resolved
        .model_chain
        .primary
        .expect("frontmatter model -> override");
    assert_eq!(over.model, "f");
    assert_ne!(over.reasoning, ReasoningSelection::Default);
}

#[test]
fn frontmatter_full_model_becomes_an_override() {
    let custom = custom_registry(&[(
        "full",
        "---\nname: full\ndescription: d\nmodel: codex:openai/gpt-5.5\n---\nprompt",
    )]);
    let tool = agent_tool_with(sub_registry_with(&["read"]), custom);
    let resolved = tool.resolve("full").unwrap();
    let over = resolved
        .model_chain
        .primary
        .expect("frontmatter model -> override");
    assert_eq!(over.model, "codex:openai/gpt-5.5");
    assert_eq!(over.reasoning, ReasoningSelection::Default);
}

#[test]
fn frontmatter_backup_becomes_ordered_model_chain() {
    let custom = custom_registry(&[(
        "resilient",
        "---\nname: resilient\ndescription: d\nmodel: codex:openai/gpt-5.5\neffort: high\nfallback_model: anthropic:anthropic/claude-sonnet-4-6\nfallback_effort: medium\n---\nprompt",
    )]);
    let tool = agent_tool_with(sub_registry_with(&["read"]), custom);
    let resolved = tool.resolve("resilient").unwrap();

    assert_eq!(
        resolved
            .model_chain
            .primary
            .as_ref()
            .map(|over| over.model.as_str()),
        Some("codex:openai/gpt-5.5")
    );
    assert_eq!(
        resolved
            .model_chain
            .backup
            .as_ref()
            .map(|over| over.model.as_str()),
        Some("anthropic:anthropic/claude-sonnet-4-6")
    );
    assert_eq!(
        resolved
            .model_chain
            .backup
            .as_ref()
            .map(|over| over.reasoning),
        Some(ReasoningSelection::Medium)
    );
}

#[test]
fn reserved_builtin_keeps_compiled_limits_and_legacy_model_assignment() {
    let custom = custom_registry(&[(
        "explore",
        "---\nname: explore\ndescription: edited\nmodel: f\neffort: low\nmax_turns: 44\n---\nCUSTOM PROMPT",
    )]);
    let resolved = agent_tool_with(sub_registry_with(&["read"]), custom)
        .resolve("explore")
        .unwrap();

    assert_eq!(resolved.limits.max_iterations, EXPLORE_MAX_ITERATIONS);
    assert_eq!(resolved.limits.timeout, Duration::from_secs(450));
    assert_eq!(resolved.limits.conclude_timeout, Duration::from_secs(180));
    assert!(resolved.instructions.contains("read-only exploration"));
    assert!(!resolved.instructions.contains("CUSTOM PROMPT"));
    let model = resolved
        .model_chain
        .primary
        .expect("legacy model assignment remains active");
    assert_eq!(model.model, "f");
    assert_eq!(model.reasoning, ReasoningSelection::Low);
}

#[test]
fn agents_index_merges_custom_subagents_without_shadowing_builtins() {
    let custom = custom_registry(&[
        (
            "mapper",
            "---\nname: mapper\ndescription: maps routes\n---\np",
        ),
        (
            "explore",
            "---\nname: explore\ndescription: overridden explore\n---\np",
        ),
        (
            "interactive",
            "---\nname: interactive\ndescription: mode only\nsurface: [mode]\n---\np",
        ),
    ]);
    let section = agents_index_section(&custom);
    assert!(section.starts_with("## Subagents"));
    assert!(section.contains("- explore — Read-only codebase exploration"));
    assert!(!section.contains("overridden explore"));
    assert!(section.contains("- review —")); // built-in still present
    assert!(section.contains("- mapper — maps routes")); // custom added
    assert!(!section.contains("interactive")); // mode-only custom excluded
}

#[test]
fn agents_index_excludes_disabled_agents() {
    let custom = custom_registry(&[
        (
            "mapper",
            "---\nname: mapper\ndescription: maps routes\nenabled: false\n---\np",
        ),
        (
            "explore",
            "---\nname: explore\ndescription: disabled override\nenabled: false\n---\np",
        ),
    ]);
    let section = agents_index_section(&custom);
    assert!(
        !section.contains("mapper"),
        "disabled custom excluded: {section}"
    );
    // Legacy same-name files retain only the built-in's enable setting.
    assert!(
        !section.contains("- explore"),
        "disabled built-in excluded: {section}"
    );
    assert!(!section.contains("disabled override"), "{section}");
}

#[test]
fn agents_index_applies_persisted_builtin_enabled_setting_before_legacy_file() {
    let custom = custom_registry(&[(
        "explore",
        "---\nname: explore\ndescription: legacy\nenabled: false\n---\np",
    )]);
    let settings = crate::subagent::BuiltinSubagentSettingsRegistry::from([(
        crate::subagent::BuiltinSubagentId::Explore,
        crate::subagent::BuiltinSubagentSettings::default(),
    )]);

    let section = agents_index_section_with_settings(&custom, &settings);

    assert!(section.contains("- explore — Read-only codebase exploration"));
    assert!(!section.contains("legacy"));
}

#[test]
fn agents_index_reserves_slots_for_builtins() {
    let dir = tempfile::TempDir::new().unwrap();
    let agents_dir = dir.path().join(".bonsai/agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    for index in 0..AGENTS_INDEX_MAX {
        let name = format!("a{index:02}");
        let contents = format!("---\nname: {name}\ndescription: custom {index}\n---\np");
        std::fs::write(agents_dir.join(format!("{name}.md")), contents).unwrap();
    }
    let custom = AgentRegistry::load_from(dir.path(), &dir.path().join("home"));

    let section = agents_index_section(&custom);

    assert!(section.contains("- explore —"), "{section}");
    assert!(section.contains("- research —"), "{section}");
    assert!(section.contains("- review —"), "{section}");
    assert!(section.contains("- security-review —"), "{section}");
    assert!(section.contains("- …and 4 more"), "{section}");
}

#[test]
fn agents_index_has_builtins_when_no_custom() {
    let section = agents_index_section(&AgentRegistry::empty());
    assert!(section.contains("- explore —"));
    assert!(section.contains("- research —"));
    assert!(section.contains("- review —"));
    assert!(section.contains("- security-review —"));
}
