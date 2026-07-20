use super::*;

#[tokio::test]
async fn unknown_agent_guides_with_available() {
    let tool = agent_tool();
    let err = tool
        .execute(serde_json::json!({ "agent": "nope", "prompt": "x" }))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Unknown agent 'nope'"), "{err}");
    assert!(err.contains("explore") && err.contains("review"), "{err}");
}

#[test]
fn agent_tool_is_serialized_and_closed_schema() {
    let tool = agent_tool();
    assert_eq!(tool.parallel_policy(), ParallelPolicy::Serialized);
    assert_eq!(tool.parameters_schema()["additionalProperties"], false);
}

#[test]
fn reserved_builtin_id_ignores_custom_prompt_tools_and_turn_budget() {
    let custom = custom_registry(&[(
        "explore",
        "---\nname: explore\ndescription: my explore\ntools: [grep]\nmodel: f\nmax_turns: 72\n---\nCUSTOM EXPLORE PROMPT",
    )]);
    let tool = agent_tool_with(sub_registry_with(&["read", "grep"]), custom);
    let resolved = tool.resolve("explore").unwrap();
    assert_eq!(resolved.name, "explore");
    assert!(resolved.instructions.contains("read-only exploration"));
    assert!(!resolved.instructions.contains("CUSTOM EXPLORE PROMPT"));
    assert_eq!(resolved.limits.max_iterations, EXPLORE_MAX_ITERATIONS);
    assert!(resolved.registry.get("read").is_some());
    assert!(resolved.registry.get("grep").is_some());
    assert_eq!(
        resolved
            .model_chain
            .primary
            .as_ref()
            .map(|assignment| assignment.model.as_str()),
        Some("f")
    );
}

#[test]
fn persisted_builtin_settings_override_legacy_same_name_file() {
    let custom = custom_registry(&[(
        "explore",
        "---\nname: explore\ndescription: legacy\nmodel: legacy-model\nenabled: false\n---\nCUSTOM",
    )]);
    let settings = crate::subagent::BuiltinSubagentSettingsRegistry::from([(
        crate::subagent::BuiltinSubagentId::Explore,
        crate::subagent::BuiltinSubagentSettings {
            enabled: true,
            primary_model: Some("persisted-model".to_string()),
            primary_effort: Some("high".to_string()),
            fallback_model: None,
            fallback_effort: None,
        },
    )]);
    let tool = agent_tool_with_settings(sub_registry_with(&["read"]), custom, settings);

    let resolved = tool.resolve("explore").expect("persisted setting enables");
    let primary = resolved
        .model_chain
        .primary
        .expect("persisted model assignment");
    assert_eq!(primary.model, "persisted-model");
    assert_eq!(primary.reasoning, ReasoningSelection::High);
    assert!(resolved.instructions.contains("read-only exploration"));
}

#[tokio::test]
async fn persisted_builtin_settings_reload_into_fresh_tool_and_beat_legacy_file() {
    let fixture = crate::storage::test_utils::TestStorage::new().await;
    let id = crate::subagent::BuiltinSubagentId::Explore;
    let persisted = crate::subagent::BuiltinSubagentSettings {
        enabled: true,
        primary_model: Some("persisted-model".to_string()),
        primary_effort: Some("high".to_string()),
        fallback_model: None,
        fallback_effort: None,
    };
    fixture
        .storage
        .upsert_builtin_subagent_settings(id, &persisted)
        .await
        .unwrap();
    let loaded = fixture
        .storage
        .load_builtin_subagent_settings()
        .await
        .unwrap();
    let custom = custom_registry(&[(
        "explore",
        "---\nname: explore\ndescription: legacy\nmodel: legacy-model\neffort: low\nsurface: [mode]\n---\nCUSTOM PROMPT",
    )]);

    let tool = agent_tool_with_settings(sub_registry_with(&["read"]), custom, loaded);
    let resolved = tool.resolve("explore").unwrap();

    assert_eq!(
        resolved
            .model_chain
            .primary
            .as_ref()
            .map(|assignment| assignment.model.as_str()),
        Some("persisted-model")
    );
    assert_eq!(
        resolved
            .model_chain
            .primary
            .as_ref()
            .map(|assignment| assignment.reasoning),
        Some(crate::provider::ReasoningSelection::High)
    );
    assert!(resolved.instructions.contains("read-only exploration"));
    assert!(!resolved.instructions.contains("CUSTOM PROMPT"));
}

#[test]
fn custom_surface_controls_delegated_resolution_and_advertising() {
    let custom = custom_registry(&[
        (
            "interactive",
            "---\nname: interactive\ndescription: mode only\nsurface: [mode]\n---\nprompt",
        ),
        (
            "helper",
            "---\nname: helper\ndescription: delegated\nsurface: [subagent]\n---\nprompt",
        ),
    ]);
    let tool = agent_tool_with(sub_registry_with(&["read"]), custom);

    let error = match tool.resolve("interactive") {
        Ok(_) => panic!("mode-only definition must not resolve as a subagent"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("not available as a delegated subagent"),
        "{error}"
    );
    assert!(tool.resolve("helper").is_ok());

    let unknown = match tool.resolve("missing") {
        Ok(_) => panic!("missing definition must fail"),
        Err(error) => error.to_string(),
    };
    assert!(!unknown.contains("interactive"), "{unknown}");
    assert!(unknown.contains("helper"), "{unknown}");
}

#[test]
fn custom_agent_tools_scope_the_registry() {
    let custom = custom_registry(&[(
        "reader",
        "---\nname: reader\ndescription: d\ntools: [read]\n---\nprompt",
    )]);
    let tool = agent_tool_with(sub_registry_with(&["read", "grep", "git"]), custom);
    let resolved = tool.resolve("reader").unwrap();
    assert!(resolved.registry.get("read").is_some());
    assert!(
        resolved.registry.get("grep").is_none(),
        "undeclared tools must be excluded"
    );
}

#[test]
fn legacy_disabled_file_disables_builtin_without_shadowing_identity() {
    let custom = custom_registry(&[(
        "explore",
        "---\nname: explore\ndescription: my explore\nenabled: false\n---\nCUSTOM PROMPT",
    )]);
    let tool = agent_tool_with(sub_registry_with(&["read"]), custom);
    // Legacy same-name files retain the built-in enabled setting.
    let err = match tool.resolve("explore") {
        Ok(_) => panic!("disabled built-in should not resolve"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("disabled"), "{err}");
    // And the masked built-in disappears from the advertised index.
    let index = agents_index_section(&custom_registry(&[(
        "explore",
        "---\nname: explore\ndescription: my explore\nenabled: false\n---\nCUSTOM PROMPT",
    )]));
    assert!(
        !index.contains("explore"),
        "masked built-in stays hidden: {index}"
    );
}

#[test]
fn custom_agent_tools_normalize_claude_aliases() {
    let custom = custom_registry(&[(
        "reader",
        "---\nname: reader\ndescription: d\ntools: [ProjectInfo, Read, ReadRegion, ReadSymbol, Grep, Glob, SymbolSearch, Git]\n---\nprompt",
    )]);
    let tool = agent_tool_with(
        sub_registry_with(&[
            "project_info",
            "read",
            "read_region",
            "read_symbol",
            "grep",
            "glob",
            "symbol_search",
            "git",
        ]),
        custom,
    );
    let resolved = tool.resolve("reader").unwrap();

    for name in [
        "project_info",
        "read",
        "read_region",
        "read_symbol",
        "grep",
        "glob",
        "symbol_search",
        "git",
    ] {
        assert!(
            resolved.registry.get(name).is_some(),
            "expected alias to resolve to {name}"
        );
    }
}

#[test]
fn custom_agent_can_be_granted_mutating_tools() {
    // A custom agent may now declare write/edit/bash; the tool is granted
    // from the full registry and prompts under the current policy at run time.
    let custom = custom_registry(&[(
        "fixer",
        "---\nname: fixer\ndescription: d\ntools: [Read, Bash]\n---\nprompt",
    )]);
    let tool = agent_tool_with(sub_registry_with(&["read", "bash", "grep"]), custom);
    let resolved = tool.resolve("fixer").unwrap();
    assert!(resolved.registry.get("bash").is_some(), "bash is grantable");
    assert!(resolved.registry.get("read").is_some());
    assert!(
        resolved.registry.get("grep").is_none(),
        "undeclared tools stay excluded"
    );
}

#[test]
fn builtin_only_agent_tool_ignores_custom_agents_and_mutating_grants() {
    let custom = custom_registry(&[
        (
            "explore",
            "---\nname: explore\ndescription: d\ntools: [bash]\nmodel: f\neffort: low\n---\nCUSTOM EXPLORE",
        ),
        (
            "fixer",
            "---\nname: fixer\ndescription: d\ntools: [bash]\n---\nCUSTOM FIXER",
        ),
        (
            "security-review",
            "---\nname: security-review\ndescription: d\ntools: [bash]\n---\nCUSTOM SECURITY",
        ),
    ]);
    let runner = test_runner_with_full_registry(
        sub_registry_with(&["read", "grep"]),
        sub_registry_with(&["read", "grep", "bash"]),
    );
    let tool = AgentTool::new_builtin_only(
        "agent",
        runner,
        crate::resource::agent::shared_registry(custom),
    );

    let resolved = tool.resolve("explore").unwrap();

    assert!(resolved.instructions.contains("read-only exploration"));
    assert!(
        !resolved.instructions.contains("CUSTOM EXPLORE"),
        "review delegation must not let custom agents shadow built-ins"
    );
    assert!(resolved.registry.get("read").is_some());
    assert!(resolved.registry.get("grep").is_some());
    assert_eq!(
        resolved
            .model_chain
            .primary
            .as_ref()
            .map(|assignment| assignment.model.as_str()),
        Some("f"),
        "built-in-only review tools still honor legacy model settings"
    );
    assert!(
        resolved.registry.get("bash").is_none(),
        "review delegation must keep built-in subagents read-only"
    );

    let security = tool.resolve("security-review").unwrap();
    assert!(
        security
            .instructions
            .contains("read-only security reviewer")
    );
    assert!(!security.instructions.contains("CUSTOM SECURITY"));
    assert!(security.registry.get("bash").is_none());

    let err = match tool.resolve("fixer") {
        Ok(_) => panic!("custom-only agent should not resolve in built-in-only mode"),
        Err(err) => err.to_string(),
    };
    assert!(err.contains("Unknown agent 'fixer'"), "{err}");
    assert!(err.contains("explore"), "{err}");
    assert!(err.contains("research"), "{err}");
    assert!(err.contains("review"), "{err}");
    assert!(err.contains("security-review"), "{err}");
}

#[test]
fn custom_agent_unknown_tool_is_skipped_not_fatal() {
    // An unrecognized tool name is dropped (surfaced separately by `/agents`),
    // not a hard error that would sink the whole delegation.
    let custom = custom_registry(&[(
        "reader",
        "---\nname: reader\ndescription: d\ntools: [read, bogus_tool]\n---\nprompt",
    )]);
    let tool = agent_tool_with(sub_registry_with(&["read", "grep"]), custom);
    let resolved = tool.resolve("reader").unwrap();
    assert!(resolved.registry.get("read").is_some());
    assert!(resolved.registry.get("bogus_tool").is_none());
}

#[tokio::test]
async fn custom_agent_runs_and_returns_conclusion() {
    let custom = custom_registry(&[(
        "mapper",
        "---\nname: mapper\ndescription: maps things\ntools: [read]\n---\nMap it.",
    )]);
    let tool = agent_tool_with(sub_registry_with(&["read", "grep"]), custom);
    let result = tool
        .execute(serde_json::json!({ "agent": "mapper", "prompt": "go" }))
        .await
        .unwrap();
    let ToolOutput::TextWithUsage { text, .. } = result else {
        panic!("agent tool should return text with usage");
    };
    assert_eq!(text, "Found it at src/lib.rs:1.");
}

#[tokio::test]
async fn mutating_custom_agent_returns_unscoped_effect() {
    let custom = custom_registry(&[(
        "fixer",
        "---\nname: fixer\ndescription: fixes things\ntools: [bash]\n---\nFix it.",
    )]);
    let tool = agent_tool_with(sub_registry_with(&["bash"]), custom);
    let result = tool
        .execute(serde_json::json!({ "agent": "fixer", "prompt": "go" }))
        .await
        .unwrap();

    let ToolOutput::TextWithUsage {
        workspace_effect, ..
    } = result
    else {
        panic!("agent tool should return text with usage");
    };
    assert_eq!(workspace_effect, crate::tool::ToolWorkspaceEffect::Unscoped);
}
