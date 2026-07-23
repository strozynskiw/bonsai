//! A custom persona's tool set is derived from its `view:` — a `view: canvas`
//! persona additionally receives the plan-canvas (`plan_*`) tools so it can drive
//! the plan its view renders, while other views stay read-only.

use std::path::Path;
use std::sync::Arc;

use crate::agent::persona::ActivePersona;
use crate::resource::agent::{AgentRegistry, shared_registry};
use crate::tool::ToolRegistry;
use crate::tool::test_utils::TestFixture;

use super::Agent;
use super::mocks::{MockProvider, MockTool};

fn registry_with(names: &[&'static str]) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for name in names {
        registry.register(Arc::new(MockTool::new(name, "ok")));
    }
    Arc::new(registry)
}

fn custom_agents(dir: &Path, files: &[(&str, &str)]) -> AgentRegistry {
    for (name, contents) in files {
        let path = dir.join(format!(".bonsai/agents/{name}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    AgentRegistry::load_from(dir, &dir.join("home"))
}

#[tokio::test]
async fn canvas_persona_gets_plan_tools_but_chat_does_not() {
    let fixture = TestFixture::new();
    let temp = tempfile::TempDir::new().unwrap();
    let custom = custom_agents(
        temp.path(),
        &[
            (
                "canvas-agent",
                "---\nname: canvas-agent\ndescription: builds the plan\nview: canvas\nsurface: [mode]\n---\nYou plan.",
            ),
            (
                "chat-agent",
                "---\nname: chat-agent\ndescription: chats\nview: chat\nsurface: [mode]\n---\nYou chat.",
            ),
        ],
    );

    let mut agent = Agent::builder(
        MockProvider::empty(),
        // coding_registry — `read_only_registry` is derived from this.
        registry_with(&["read"]),
        // planning_registry — carries the plan-canvas tools.
        registry_with(&["plan_add_task", "read"]),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .custom_agents(shared_registry(custom))
    .build()
    .unwrap();

    agent.set_persona(ActivePersona::Custom("canvas-agent".to_string()));
    assert!(
        agent.tool_registry.get("plan_add_task").is_some(),
        "a canvas persona should receive the plan-canvas tools"
    );
    assert!(
        agent.tool_registry.get("read").is_some(),
        "a canvas persona keeps its read-only tools"
    );

    agent.set_persona(ActivePersona::Custom("chat-agent".to_string()));
    assert!(
        agent.tool_registry.get("plan_add_task").is_none(),
        "a non-canvas persona must not receive plan-canvas tools"
    );
    assert!(
        agent.tool_registry.get("read").is_some(),
        "a chat persona keeps its read-only tools"
    );
}

#[tokio::test]
async fn persona_can_be_granted_mutating_tools() {
    let fixture = TestFixture::new();
    let temp = tempfile::TempDir::new().unwrap();
    let custom = custom_agents(
        temp.path(),
        &[(
            "fixer",
            "---\nname: fixer\ndescription: edits code\ntools: [read, write]\nsurface: [mode]\n---\nYou fix.",
        )],
    );

    let mut agent = Agent::builder(
        MockProvider::empty(),
        // coding_registry carries the full tool set a persona's `tools:` scopes from.
        registry_with(&["read", "write", "bash", "agent"]),
        registry_with(&["plan_add_task", "read"]),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .custom_agents(shared_registry(custom))
    .build()
    .unwrap();

    agent.set_persona(ActivePersona::Custom("fixer".to_string()));
    assert!(
        agent.tool_registry.get("write").is_some(),
        "a persona declaring write must be granted it from the coding registry"
    );
    assert!(agent.tool_registry.get("read").is_some());
    assert!(
        agent.tool_registry.get("agent").is_some(),
        "user-facing agents retain delegated-subagent access"
    );
    assert!(
        agent.tool_registry.get("bash").is_none(),
        "undeclared tools stay excluded"
    );
}

#[tokio::test]
async fn direct_persona_selection_rejects_subagent_only_and_reserved_builtin_ids() {
    let fixture = TestFixture::new();
    let temp = tempfile::TempDir::new().unwrap();
    let custom = custom_agents(
        temp.path(),
        &[
            (
                "helper",
                "---\nname: helper\ndescription: delegated only\nsurface: [subagent]\n---\nYou help.",
            ),
            (
                "explore",
                "---\nname: explore\ndescription: legacy builtin settings\n---\nCUSTOM EXPLORE",
            ),
        ],
    );

    let mut agent = Agent::builder(
        MockProvider::empty(),
        registry_with(&["read"]),
        registry_with(&["plan_add_task", "read"]),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .custom_agents(shared_registry(custom))
    .build()
    .unwrap();

    for name in ["helper", "explore"] {
        agent.set_persona(ActivePersona::Custom(name.to_string()));
        assert_eq!(
            agent.active_persona,
            ActivePersona::Builtin(super::AgentMode::Coding),
            "{name} must not become a custom main-loop persona"
        );
    }
}

#[tokio::test]
async fn pure_builtin_persona_has_empty_tool_registry() {
    let fixture = TestFixture::new();
    let coding_registry = registry_with(&["read", "write", "bash"]);
    let planning_registry = registry_with(&["plan_add_task", "read"]);

    let mut agent = Agent::builder(
        MockProvider::empty(),
        coding_registry,
        planning_registry,
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .build()
    .unwrap();

    // Set up pure mode and force registry update via mode switch.
    agent.set_persona(ActivePersona::Builtin(super::AgentMode::Planning));
    // Use the setter so mutual exclusion with smol and registry/system
    // message rebuild are exercised.
    agent.set_pure_mode(true);
    agent.set_persona(ActivePersona::Builtin(super::AgentMode::Coding));

    // Pure persona should have zero tools.
    assert!(
        agent.tool_registry.get("read").is_none(),
        "pure persona must have no tools, not even read"
    );
    assert!(
        agent.tool_registry.get("write").is_none(),
        "pure persona must have no mutating tools"
    );

    // Explicit names list should be empty too.
    assert!(
        agent.tool_registry.names().count() == 0,
        "pure tool registry should list zero tool names"
    );
}

#[tokio::test]
async fn pure_mode_survives_context_budget_change() {
    let fixture = TestFixture::new();
    let coding_registry = registry_with(&["read", "write", "bash"]);
    let planning_registry = registry_with(&["plan_add_task", "read"]);

    let mut agent = Agent::builder(
        MockProvider::empty(),
        coding_registry,
        planning_registry,
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .build()
    .unwrap();

    // Give smol a reason to activate.
    agent.set_smol_mode(true);
    assert!(agent.smol_mode(), "smol should be on before pure");

    // Enable pure — should disable smol.
    agent.set_pure_mode(true);
    assert!(agent.pure_mode(), "pure should be on");
    assert!(!agent.smol_mode(), "pure should disable smol");

    // Simulate a model switch: set_context_budget_tokens calls
    // refresh_effective_smol_profile internally. set_pure_mode(true)
    // overrides smol_preference to Off, so the budget change must not
    // re-enable smol.
    agent.set_context_budget_tokens(100_000);
    assert!(
        agent.pure_mode(),
        "pure mode should survive a budget change"
    );
    assert!(
        !agent.smol_mode(),
        "smol must not re-enable behind pure's back"
    );

    // Explicit smol activation should still override pure.
    agent.set_smol_mode(true);
    assert!(agent.smol_mode(), "explicit smol activation should work");
    assert!(!agent.pure_mode(), "explicit smol should disable pure");
}

#[tokio::test]
async fn pure_mode_blocks_internal_smol_reactivation() {
    let fixture = TestFixture::new();
    let coding_registry = registry_with(&["read", "write", "bash"]);
    let planning_registry = registry_with(&["plan_add_task", "read"]);

    let mut agent = Agent::builder(
        MockProvider::empty(),
        coding_registry,
        planning_registry,
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .build()
    .unwrap();

    // Enable smol first.
    agent.set_smol_mode(true);
    assert!(agent.smol_mode());

    // Enable pure — must disable smol and set smol_preference to Off.
    agent.set_pure_mode(true);
    assert!(agent.pure_mode());
    assert!(!agent.smol_mode());

    // Internal budget change must NOT re-enable smol behind pure's back.
    // refresh_effective_smol_profile sees pure_mode==true and returns false.
    agent.set_context_budget_tokens(10_000);
    assert!(agent.pure_mode(), "pure must survive budget change");
    assert!(!agent.smol_mode(), "smol must stay off");

    // Explicit smol activation overrides pure.
    agent.set_smol_mode(true);
    assert!(agent.smol_mode(), "explicit smol must activate");
    assert!(!agent.pure_mode(), "explicit smol must disable pure");
}
