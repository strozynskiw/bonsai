use std::sync::Arc;
use std::time::Duration;

use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

use super::CommandMessageKind;
use super::CommandModalRequest;
use super::metadata::{
    BusyCommandBehavior, busy_behavior_for, command_completion_preview, command_description,
    command_usage_hint, complete_command,
};
use super::providers::refresh_stale_model_cache;
use crate::agent::Agent;
use crate::background::BackgroundTaskRegistry;
use crate::output::SharedSink;
use crate::provider::{
    AuthInput, AuthorizeOutcome, Provider, ProviderFactory, ProviderMetadata, ProviderRegistry,
    StreamedResponse,
};
use crate::session::SessionStore;
use crate::tool::ToolRegistry;
use crate::tool::test_utils::TestFixture;

struct MockProvider;

#[async_trait]
impl Provider for MockProvider {
    async fn chat_stream(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        _cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        })
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec!["alpha".to_string(), "beta".to_string()])
    }
}

struct MockCodexFactory;

#[async_trait]
impl ProviderFactory for MockCodexFactory {
    fn metadata(&self) -> &ProviderMetadata {
        crate::provider::metadata_for("codex").unwrap()
    }

    async fn authorize(&self, _input: AuthInput) -> anyhow::Result<AuthorizeOutcome> {
        Ok(AuthorizeOutcome::default())
    }

    fn is_authorized(&self, session: &crate::session::ProviderSession) -> bool {
        !session.api_key.trim().is_empty() && !session.account_id.trim().is_empty()
    }

    fn clear_authorization(&self, session: &mut crate::session::ProviderSession) {
        session.api_key.clear();
        session.account_id.clear();
    }

    async fn list_models(
        &self,
        _session: &crate::session::ProviderSession,
    ) -> anyhow::Result<Vec<String>> {
        Ok(vec!["gpt-5.3-codex-spark".to_string()])
    }
}

fn agent() -> Agent {
    let fixture = TestFixture::new();
    Agent::new(
        Box::new(MockProvider),
        Arc::new(ToolRegistry::new()),
        Arc::new(ToolRegistry::new()),
        fixture.read_tracker,
        String::new(),
        fixture.project_root,
    )
    .unwrap()
}

fn registry() -> Arc<ProviderRegistry> {
    Arc::new(ProviderRegistry::default_registry())
}

fn session_store() -> SessionStore {
    let mut store = SessionStore::default();
    for id in ["opencode", "codex", "anthropic", "minimax-coding-plan"] {
        store.ensure_provider(id);
    }
    store
}

fn test_catalog() -> crate::model_catalog::ModelCatalog {
    crate::model_catalog::ModelCatalog::load_builtin().unwrap()
}

fn catalog_with_live_models(
    provider_id: &str,
    models: impl IntoIterator<Item = &'static str>,
) -> crate::model_catalog::ModelCatalog {
    let catalog = test_catalog();
    let connection_id = provider_id
        .parse::<crate::model_catalog::ConnectionId>()
        .unwrap();
    catalog
        .write_live_availability(
            &connection_id,
            crate::model_catalog::LiveModelAvailability::from_remote_ids(
                models.into_iter().map(str::to_string),
            ),
        )
        .unwrap();
    catalog
}

async fn handle_command(
    input: &str,
    agent: &mut Agent,
    session_store: &mut SessionStore,
    project_root: &std::path::Path,
    transcript: &[crate::tui::app::TranscriptItem],
    registry: Arc<ProviderRegistry>,
) -> anyhow::Result<super::CommandOutcome> {
    super::handle_command_with_catalog(
        input,
        agent,
        session_store,
        project_root,
        transcript,
        registry,
        super::CommandRuntimeContext {
            catalog: None,
            storage: None,
            active_mode: Some(crate::agent::AgentMode::Coding),
        },
    )
    .await
}

async fn handle_command_with_model_catalog(
    input: &str,
    agent: &mut Agent,
    session_store: &mut SessionStore,
    project_root: &std::path::Path,
    transcript: &[crate::tui::app::TranscriptItem],
    registry: Arc<ProviderRegistry>,
    catalog: &crate::model_catalog::ModelCatalog,
) -> anyhow::Result<super::CommandOutcome> {
    super::handle_command_with_catalog(
        input,
        agent,
        session_store,
        project_root,
        transcript,
        registry,
        super::CommandRuntimeContext {
            catalog: Some(catalog),
            storage: None,
            active_mode: Some(crate::agent::AgentMode::Coding),
        },
    )
    .await
}

#[tokio::test]
async fn help_opens_command_modal() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/help", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(outcome.opens_command_help());
    assert!(outcome.messages.is_empty());
}

#[tokio::test]
async fn perf_reports_empty_state_before_first_run() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/perf", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert_eq!(outcome.messages.len(), 1);
    assert_eq!(outcome.messages[0].kind, CommandMessageKind::Status);
    assert!(
        outcome.messages[0]
            .text
            .contains("No performance data yet. Run the agent once, then use /perf or /cost."),
        "{:?}",
        outcome.messages[0].text
    );
    assert!(
        outcome.messages[0].text.contains("Usage: session"),
        "{:?}",
        outcome.messages[0].text
    );
}

#[tokio::test]
async fn episodes_opens_lifecycle_modal() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/episodes",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(outcome.opens_episodes());
    assert_eq!(outcome.messages.len(), 1);
    assert_eq!(outcome.messages[0].kind, CommandMessageKind::Status);
}

#[tokio::test]
async fn cost_alias_reports_perf_usage() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/cost", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert_eq!(outcome.messages.len(), 1);
    assert_eq!(outcome.messages[0].kind, CommandMessageKind::Status);
    assert!(outcome.messages[0].text.contains("Usage: session"));
}

#[tokio::test]
async fn sandbox_status_opens_status_modal() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/sandbox status",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(
        outcome.opens_sandbox_status(),
        "sandbox status should open modal"
    );
    assert!(outcome.messages.is_empty());
}

#[tokio::test]
async fn mcp_opens_server_modal() {
    let mut agent = agent();
    agent
        .extensions()
        .upsert(crate::extension::status::ExtensionStatus {
            id: crate::extension::ExtensionId::McpServer("demo".to_string()),
            source: crate::config::ConfigSource::Project,
            capabilities: crate::config::DeclaredCapabilities {
                capabilities: vec![crate::config::Capability::Read],
                batching: crate::config::BatchingPolicy::Serialized,
            },
            state: crate::extension::status::ExtensionState::Enabled,
            detail: "1 tool(s)".to_string(),
            tools: vec![crate::extension::status::DiscoveredTool {
                name: "read_note".to_string(),
                description: "Read the demo note".to_string(),
            }],
        });
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/mcp", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    let rows = outcome
        .mcp_server_rows()
        .expect("/mcp should open the MCP modal");
    assert!(outcome.messages.is_empty());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name, "demo");
    assert_eq!(rows[0].tools[0].name, "read_note");
}

#[tokio::test]
async fn self_review_command_sets_mode_and_reports_status() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let set = handle_command(
        "/self-review on",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        agent.self_review_mode(),
        crate::self_review::SelfReviewMode::On
    );
    assert!(set.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Status && message.text == "Self-review set to on."
    }));

    let status = handle_command(
        "/self-review status",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();
    assert!(
        status
            .messages
            .iter()
            .any(|message| message.text == "Self-review is on.")
    );

    let bad = handle_command(
        "/self-review nope",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();
    assert_eq!(
        agent.self_review_mode(),
        crate::self_review::SelfReviewMode::On,
        "an invalid mode must not change the setting"
    );
    assert!(
        bad.messages
            .iter()
            .any(|message| message.kind == CommandMessageKind::Error)
    );
}

#[tokio::test]
async fn yolo_command_toggles_and_reports_state() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let enabled = handle_command(
        "/yolo",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();
    assert!(agent.yolo_enabled());
    assert!(enabled.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Status && message.text == "Autonomy set to yolo."
    }));

    let status = handle_command(
        "/yolo status",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();
    assert!(agent.yolo_enabled());
    assert!(
        status
            .messages
            .iter()
            .any(|message| message.text == "Autonomy is yolo.")
    );

    let disabled = handle_command(
        "/yolo off",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();
    assert!(!agent.yolo_enabled());
    assert!(
        disabled
            .messages
            .iter()
            .any(|message| message.text == "Autonomy set to ask.")
    );
}

#[tokio::test]
async fn yolo_command_accepts_on_and_rejects_bad_args() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let enabled = handle_command(
        "/yolo on",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();
    let _ = enabled;
    assert!(agent.yolo_enabled());

    let bad = handle_command(
        "/yolo maybe",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();
    assert!(bad.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Error && message.text == "Usage: /yolo [on|off|status]"
    }));
    assert!(agent.yolo_enabled());
}

#[tokio::test]
async fn smol_command_sets_reports_and_rejects_bad_args() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let enabled = handle_command(
        "/smol on",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();
    assert!(agent.smol_mode());
    assert!(enabled.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Status
            && message
                .text
                .starts_with("SMOL preference set to on; effective profile: on")
    }));

    let status = handle_command(
        "/smol status",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();
    assert!(status.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Status
            && message
                .text
                .starts_with("SMOL preference: on; effective profile: on")
    }));

    let bad = handle_command(
        "/smol maybe",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();
    assert!(bad.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Error && message.text == "Usage: /smol [on|off|status]"
    }));
    assert!(agent.smol_mode());

    let disabled = handle_command(
        "/smol off",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();
    assert!(!agent.smol_mode());
    assert!(disabled.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Status
            && message
                .text
                .starts_with("SMOL preference set to off; effective profile: off")
    }));
}

#[test]
fn subagents_command_registered_with_subtasks_as_legacy_alias() {
    // Canonical name: registered, completable, read-only while busy.
    assert_eq!(
        command_description("/subagents"),
        Some("Show running subagents (live)")
    );
    assert_eq!(complete_command("/subag").as_deref(), Some("/subagents"));
    assert_eq!(
        busy_behavior_for("/subagents"),
        BusyCommandBehavior::ReadOnlyNow
    );

    // Legacy spelling still resolves (muscle memory), but completion never
    // suggests it — only the canonical name is advertised.
    assert_eq!(
        command_description("/subtasks"),
        Some("Show running subagents (live)")
    );
    assert_eq!(
        busy_behavior_for("/subtasks"),
        BusyCommandBehavior::ReadOnlyNow
    );
    assert_eq!(complete_command("/subta").as_deref(), None);
}

#[test]
fn smol_command_metadata_and_completion_are_registered() {
    assert_eq!(
        command_completion_preview("/smol").as_deref(),
        Some("Configure the small-model compact profile [on|off|status]")
    );
    assert_eq!(complete_command("/smo").as_deref(), Some("/smol"));
    // Bare `/smol` toggles (a state change): setting toggles apply mid-run
    // (persisted + app mirror now, agent sync when the run releases the lock),
    // while `/smol status` stays a read-only view.
    assert_eq!(busy_behavior_for("/smol"), BusyCommandBehavior::RunNow);
    assert_eq!(
        busy_behavior_for("/smol status"),
        BusyCommandBehavior::ReadOnlyNow
    );
    assert_eq!(busy_behavior_for("/smol on"), BusyCommandBehavior::RunNow);
}

#[test]
fn serenity_command_metadata_and_completion_are_registered() {
    assert_eq!(
        command_completion_preview("/serenity").as_deref(),
        Some("Use calm transcript presentation [on|off|status]")
    );
    assert_eq!(complete_command("/ser").as_deref(), Some("/serenity"));
    assert_eq!(busy_behavior_for("/serenity"), BusyCommandBehavior::RunNow);
    assert_eq!(
        busy_behavior_for("/serenity status"),
        BusyCommandBehavior::ReadOnlyNow
    );
    assert_eq!(
        busy_behavior_for("/serenity on"),
        BusyCommandBehavior::RunNow
    );
    assert_eq!(
        busy_behavior_for("/serenity maybe"),
        BusyCommandBehavior::Block
    );
}

#[tokio::test]
async fn tasks_command_lists_background_tasks() {
    let mut agent = agent();
    let background_tasks = Arc::new(BackgroundTaskRegistry::new());
    let dir = tempdir().unwrap();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let task = background_tasks
        .start(&shell, "printf listed-task", dir.path(), 5)
        .await
        .unwrap();
    background_tasks
        .wait_for_task(&task.id, Duration::from_secs(2))
        .await
        .unwrap();
    agent.set_background_tasks(background_tasks);
    let mut store = session_store();

    let outcome = handle_command(
        "/tasks",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();

    let message = outcome.messages.first().expect("status message");
    assert_eq!(message.kind, CommandMessageKind::Status);
    assert!(message.text.contains(&task.id), "text: {}", message.text);
    assert!(message.text.contains("succeeded"), "text: {}", message.text);
}

#[tokio::test]
async fn tasks_stop_command_stops_background_task() {
    let mut agent = agent();
    let background_tasks = Arc::new(BackgroundTaskRegistry::new());
    let dir = tempdir().unwrap();
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let task = background_tasks
        .start(&shell, "sleep 5", dir.path(), 30)
        .await
        .unwrap();
    agent.set_background_tasks(background_tasks);
    let mut store = session_store();

    let outcome = handle_command(
        &format!("/tasks stop {}", task.id),
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();

    let message = outcome.messages.first().expect("status message");
    assert_eq!(message.kind, CommandMessageKind::Status);
    assert!(message.text.contains("stopped"), "text: {}", message.text);
}

#[tokio::test]
async fn copy_command_returns_tui_handoff_message() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/copy", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert_eq!(
        outcome
            .messages
            .first()
            .map(|message| message.text.as_str()),
        Some(
            "/copy is handled by the TUI: selected text or focused row; /copy all copies the full transcript."
        )
    );
}

#[tokio::test]
async fn copy_all_command_returns_tui_handoff_message() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/copy all",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert_eq!(
        outcome
            .messages
            .first()
            .map(|message| message.text.as_str()),
        Some(
            "/copy is handled by the TUI: selected text or focused row; /copy all copies the full transcript."
        )
    );
}

#[tokio::test]
async fn compact_preview_reports_without_opening_context_modal() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/compact preview",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(!outcome.opens_context_report());
    assert!(outcome.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Status
            && message.text.contains("Context compaction preview")
    }));
}

#[tokio::test]
async fn compact_reports_without_opening_context_modal() {
    let mut agent = agent();
    // Compaction only acts when over target, so shrink the window and seed
    // enough history that the prompt is genuinely over the (50%) target.
    agent.set_context_budget_tokens(10_000);
    let history = (0..20)
        .map(|index| {
            (
                "user".to_string(),
                format!("message {index} {}", "x".repeat(4_000)),
            )
        })
        .collect::<Vec<_>>();
    agent.restore_text_history(&history).await.unwrap();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/compact",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(!outcome.opens_context_report());
    assert!(outcome.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Status && message.text.contains("Context compacted")
    }));
}

#[tokio::test]
async fn compact_accepts_target_and_summary_policy_arguments() {
    let mut agent = agent();
    // Seed enough history that the prompt is over the explicit 2k target so the
    // command actually compacts (deterministically, no provider call).
    let history = (0..10)
        .map(|index| {
            (
                "user".to_string(),
                format!("message {index} {}", "x".repeat(2_000)),
            )
        })
        .collect::<Vec<_>>();
    agent.restore_text_history(&history).await.unwrap();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/compact deterministic target=2k",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(outcome.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Status
            && message.text.contains("Context compacted")
            && message.text.contains("target 2.0k")
    }));
}

#[tokio::test]
async fn compact_rejects_unknown_arguments() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/compact now",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(outcome.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Error
            && message
                .text
                .contains("Usage: /compact [preview] [target=<tokens>]")
    }));
}

#[tokio::test]
async fn start_returns_tui_handoff_message_for_non_tui_callers() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/start", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("TUI plan-handoff"))
    );
}

#[tokio::test]
async fn commit_returns_tui_handoff_message_for_non_tui_callers() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/commit", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("TUI command"))
    );
}

#[tokio::test]
async fn pr_returns_tui_handoff_message_for_non_tui_callers() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/pr", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("TUI command"))
    );
}

#[tokio::test]
async fn every_listed_command_is_dispatched() {
    // `COMMANDS` is the single source of truth for `/help` and completion. Guard
    // that every entry actually resolves to a handler: a listed-but-undispatched
    // command silently falls through to the generic "Unknown command" error while
    // still being advertised. This must hold across the dispatch refactor.
    for command in super::metadata::COMMANDS {
        let mut agent = agent();
        let mut store = session_store();
        let dir = tempdir().unwrap();
        let registry = registry();

        let outcome = handle_command(
            command.name,
            &mut agent,
            &mut store,
            dir.path(),
            &[],
            registry,
        )
        .await
        .unwrap();

        assert!(
            !outcome
                .messages
                .iter()
                .any(|message| message.text.starts_with("Unknown command")),
            "{} is listed in COMMANDS but not dispatched",
            command.name
        );
    }
}

#[tokio::test]
async fn doctor_json_returns_versioned_redacted_report() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();

    let outcome = handle_command(
        "/doctor json",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();

    let report: serde_json::Value = serde_json::from_str(&outcome.messages[0].text).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert!(
        report["checks"]
            .as_array()
            .is_some_and(|checks| { checks.iter().any(|check| check["id"] == "provider_auth") })
    );
    assert!(report["checks"].as_array().is_some_and(|checks| {
        checks.iter().all(|check| {
            check["id"] != "provider_reachability" && check["id"] != "mcp_reachability"
        })
    }));
    assert!(!outcome.messages[0].text.contains("api_key"));
}

#[tokio::test]
async fn doctor_text_opens_summary_modal_instead_of_dumping_text() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();

    let outcome = handle_command(
        "/doctor",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();

    // Text mode is now a modal: no transcript spam, and the report rides along
    // so the TUI can render it.
    assert!(outcome.messages.is_empty());
    match outcome.modal {
        Some(CommandModalRequest::Doctor(report)) => {
            assert_eq!(report.schema_version, 1);
            assert!(
                report
                    .checks
                    .iter()
                    .any(|check| check.id == "provider_auth")
            );
        }
        other => panic!("expected a doctor modal, got {other:?}"),
    }
}

#[tokio::test]
async fn doctor_online_adds_explicit_reachability_checks() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();

    let outcome = handle_command(
        "/doctor online json",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();

    let report: serde_json::Value = serde_json::from_str(&outcome.messages[0].text).unwrap();
    let checks = report["checks"].as_array().unwrap();
    assert!(
        checks
            .iter()
            .any(|check| check["id"] == "provider_reachability")
    );
    assert!(checks.iter().any(|check| check["id"] == "mcp_reachability"));
    assert!(checks.iter().any(|check| check["id"] == "release"));
    assert!(!outcome.messages[0].text.contains("api_key"));
}

#[tokio::test]
async fn start_rejects_arguments() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/start now",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| matches!(message.kind, CommandMessageKind::Error)
                && message.text.contains("Usage: /start"))
    );
}

#[tokio::test]
async fn commit_rejects_arguments() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/commit now",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| matches!(message.kind, CommandMessageKind::Error)
                && message.text.contains("Usage: /commit"))
    );
}

#[tokio::test]
async fn pr_rejects_arguments() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/pr now", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| matches!(message.kind, CommandMessageKind::Error)
                && message.text.contains("Usage: /pr"))
    );
}

#[tokio::test]
async fn review_returns_tui_handoff_message_for_non_tui_callers() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/review", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("TUI command"))
    );
}

#[tokio::test]
async fn review_rejects_arguments() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/review now",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| matches!(message.kind, CommandMessageKind::Error)
                && message.text.contains("Usage: /review"))
    );
}

#[tokio::test]
async fn security_review_returns_tui_handoff_and_rejects_arguments() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/security-review",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();
    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("TUI command"))
    );

    let outcome = handle_command(
        "/security-review now",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();
    assert!(outcome.messages.iter().any(|message| {
        matches!(message.kind, CommandMessageKind::Error)
            && message.text.contains("Usage: /security-review")
    }));
}

#[tokio::test]
async fn wizard_returns_tui_handoff_message_for_non_tui_callers() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/wizard", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("TUI command"))
    );
}

#[tokio::test]
async fn wizard_with_argument_defers_to_tui() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/wizard my-local",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("TUI command"))
    );
}

#[tokio::test]
async fn clear_marks_transcript_for_reset() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/clear", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(outcome.clear_transcript);
    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("Conversation cleared"))
    );
}

#[tokio::test]
async fn new_marks_transcript_for_session_reset() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/new", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(outcome.clear_transcript);
    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("Started new session"))
    );
}

#[tokio::test]
async fn model_without_argument_opens_picker() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    store.session_mut("opencode").api_key = "sk-oc".to_string();

    let outcome = handle_command("/model", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(outcome.opens_model_picker());
    assert!(outcome.messages.is_empty());
}

#[tokio::test]
async fn model_blocks_when_no_authorized_provider() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/model", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(!outcome.opens_model_picker());
    assert!(
        outcome
            .messages
            .iter()
            .any(|message| matches!(message.kind, CommandMessageKind::Error))
    );
}

#[tokio::test]
async fn model_opens_picker_when_current_provider_authorized() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    store.session_mut("opencode").api_key = "sk-oc".to_string();
    store.set_current_kind_id("opencode");

    let outcome = handle_command("/model", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(outcome.opens_model_picker());
}

#[tokio::test]
async fn model_opens_picker_when_any_authorized_provider_exists() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    store.session_mut("minimax-coding-plan").api_key = "sk-mm".to_string();

    let outcome = handle_command("/model", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(outcome.opens_model_picker());
    assert!(outcome.messages.is_empty());
}

#[tokio::test]
async fn unknown_model_error_names_refresh_and_local_catalog_actions() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    store.set_current_kind_id("opencode");
    let outcome = handle_command(
        "/model missing-local-model",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    let message = outcome
        .messages
        .iter()
        .find(|message| message.kind == CommandMessageKind::Error)
        .expect("unknown model should return an error");
    assert!(message.text.contains("provider 'opencode'"));
    assert!(message.text.contains("/refresh"));
    assert!(message.text.contains("/providers"));
    assert!(message.text.contains(".bonsai/models"));
}

#[tokio::test]
async fn model_picker_surfaces_cached_catalog_fallback() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();
    let catalog = test_catalog();
    catalog.record_models_dev_refresh_failure(
        &crate::model_catalog::CatalogError::ModelsDevHttpStatus {
            url: "https://models.example/catalog.json".to_string(),
            status: 503,
        },
    );
    store.session_mut("opencode").api_key = "sk-oc".to_string();
    store.set_current_kind_id("opencode");

    let outcome = handle_command_with_model_catalog(
        "/model",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
        &catalog,
    )
    .await
    .unwrap();

    assert!(outcome.opens_model_picker());
    assert!(outcome.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Status
            && message.text.contains("using cached model metadata")
    }));
}

#[tokio::test]
async fn refresh_updates_active_provider_model_cache() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = Arc::new(ProviderRegistry::new(vec![Arc::new(MockCodexFactory)]));
    let catalog = test_catalog();

    store.set_current_kind_id("codex");
    store.session_mut("codex").api_key = "codex-token".to_string();
    store.session_mut("codex").account_id = "codex-account".to_string();

    let outcome = handle_command_with_model_catalog(
        "/refresh",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
        &catalog,
    )
    .await
    .unwrap();

    let codex = "codex"
        .parse::<crate::model_catalog::ConnectionId>()
        .unwrap();
    assert_eq!(
        catalog.available_models_for_connection(&codex, Vec::new()),
        vec!["gpt-5.3-codex-spark".to_string()]
    );
    assert!(
        outcome
            .messages
            .iter()
            .any(|message| matches!(message.kind, CommandMessageKind::Status)
                && message.text.contains("Codex: 1 models"))
    );
}

#[tokio::test]
async fn refresh_requires_active_provider_authorization() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = Arc::new(ProviderRegistry::new(vec![Arc::new(MockCodexFactory)]));
    let catalog = test_catalog();

    store.set_current_kind_id("codex");

    let outcome = handle_command_with_model_catalog(
        "/refresh",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
        &catalog,
    )
    .await
    .unwrap();

    // /refresh now covers every authorized provider; with none authorized it
    // errors with the authorize hint instead of naming the active provider.
    assert!(
        outcome
            .messages
            .iter()
            .any(|message| matches!(message.kind, CommandMessageKind::Error)
                && message.text.contains("No authorized providers")),
        "unexpected outcome: {outcome:#?}"
    );
}

#[tokio::test]
async fn authorize_keyboard_returns_prompt_request() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/authorize opencode",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(outcome.api_key_prompt_provider().is_some());
    assert_eq!(outcome.api_key_prompt_provider(), Some("opencode"));
}

#[tokio::test]
async fn authorize_without_argument_opens_provider_picker() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();
    store.session_mut("opencode").api_key = "sk-oc".to_string();
    store.set_current_kind_id("opencode");

    let outcome = handle_command(
        "/authorize",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    let picker = outcome
        .authorize_provider_picker()
        .expect("provider picker should open");
    assert!(outcome.api_key_prompt_provider().is_none());
    assert!(outcome.messages.is_empty());
    assert!(
        picker
            .iter()
            .any(|provider| provider.provider_id == "opencode"
                && provider.authorized
                && provider.current)
    );
    assert!(
        picker
            .iter()
            .any(|provider| provider.provider_id == "codex")
    );
}

#[tokio::test]
async fn authorize_codex_does_not_prompt_for_key() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/authorize codex",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(outcome.api_key_prompt_provider().is_none());
}

#[tokio::test]
async fn authorize_accepts_numeric_provider() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/authorize 1",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(outcome.api_key_prompt_provider().is_some());
    assert_eq!(outcome.api_key_prompt_provider(), Some("opencode"));
}

#[tokio::test]
async fn removed_provider_commands_are_unknown() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/provider",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();

    assert!(outcome.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Error && message.text == "Unknown command: /provider"
    }));

    // `/providers` is a registered command again (the provider manager); only
    // the old singular `/provider` stays unknown.
    let outcome = handle_command(
        "/providers",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(!outcome.messages.iter().any(|message| {
        message.kind == CommandMessageKind::Error && message.text.starts_with("Unknown command")
    }));
    assert_eq!(store.current_kind_id(), "opencode");
}

#[test]
fn complete_command_extends_unique_prefix() {
    assert_eq!(complete_command("/n").as_deref(), Some("/new"));
    assert_eq!(complete_command("/sta").as_deref(), Some("/start"));
    assert_eq!(complete_command("/ref").as_deref(), Some("/refresh"));
    assert_eq!(complete_command("/comm").as_deref(), Some("/commit"));
    assert_eq!(complete_command("/pr").as_deref(), Some("/pr"));
}

#[test]
fn complete_command_returns_longest_common_prefix_for_ambiguous() {
    assert_eq!(complete_command("/c").as_deref(), Some("/c"));
    assert_eq!(complete_command("/co").as_deref(), Some("/co"));
    assert_eq!(complete_command("/com").as_deref(), Some("/com"));
}

#[test]
fn complete_command_matches_case_insensitively() {
    assert_eq!(complete_command("/REF").as_deref(), Some("/refresh"));
    assert_eq!(complete_command("/C").as_deref(), Some("/c"));
}

#[test]
fn model_shortcut_commands_are_hidden_and_defer_when_busy() {
    // One-letter model shortcuts are dynamic assignments, not static commands:
    // they should not appear in help/completion metadata, but they still switch
    // the live model and therefore defer mid-run.
    for cmd in ["/c", "/f", "/s", "/z"] {
        assert!(
            command_description(cmd).is_none(),
            "{cmd} should not be a registered command",
        );
        assert_eq!(
            busy_behavior_for(cmd),
            BusyCommandBehavior::DeferUntilIdle,
            "{cmd} should defer until idle like dynamic model shortcuts",
        );
    }
    assert_eq!(busy_behavior_for("/f extra"), BusyCommandBehavior::Block);
}

#[tokio::test]
async fn model_shortcut_commands_accept_bare_and_model_forms_only() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();
    let key = crate::model_role::ModelShortcutKey::new('f').unwrap();
    store.set_current_kind_id("opencode");
    store.session_mut("opencode").model = "alpha".to_string();
    store.set_model_shortcut_binding(
        key,
        crate::model_role::ModelShortcutBinding {
            provider_id: "opencode".to_string(),
            connection_id: None,
            model_id: None,
            model: "beta".to_string(),
            reasoning: crate::provider::ReasoningSelection::Default,
        },
    );

    let rejected = handle_command(
        "/f extra",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();

    assert_eq!(store.session("opencode").model, "alpha");
    assert!(rejected.messages.iter().any(|message| {
        matches!(message.kind, CommandMessageKind::Error)
            && message.text.contains("Unknown command: /f")
    }));

    let direct = handle_command(
        "/f",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry.clone(),
    )
    .await
    .unwrap();

    assert_eq!(store.session("opencode").model, "beta");
    assert!(direct.messages.iter().any(|message| {
        matches!(message.kind, CommandMessageKind::Status)
            && message.text.contains("Model set to beta (/f")
    }));

    store.session_mut("opencode").model = "alpha".to_string();
    let via_model = handle_command(
        "/model f",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert_eq!(store.session("opencode").model, "beta");
    assert!(via_model.messages.iter().any(|message| {
        matches!(message.kind, CommandMessageKind::Status)
            && message.text.contains("Model set to beta (/f")
    }));
}

#[test]
fn complete_command_rejects_non_command_input() {
    assert_eq!(complete_command("hello"), None);
}

#[test]
fn command_metadata_builds_completion_preview() {
    assert_eq!(command_description("/MODEL"), Some("Switch model"));
    assert_eq!(
        command_usage_hint("/model"),
        Some("<number|model|connection:model>")
    );
    assert_eq!(
        command_completion_preview("/model").as_deref(),
        Some("Switch model [<number|model|connection:model>]")
    );
    assert_eq!(
        command_completion_preview("/help").as_deref(),
        Some("List commands")
    );
    assert_eq!(
        command_completion_preview("/perf").as_deref(),
        Some("Show performance and usage")
    );
    assert_eq!(
        command_completion_preview("/cost").as_deref(),
        Some("Show performance and usage")
    );
    assert_eq!(
        command_completion_preview("/yolo").as_deref(),
        Some("Shortcut for /autonomy yolo (no guardrails) [on|off|status]")
    );
    assert_eq!(
        command_completion_preview("/compact").as_deref(),
        Some(
            "Compact context or preview the compaction plan [preview target=<tokens> provider|deterministic]"
        )
    );
    assert_eq!(
        command_completion_preview("/commit").as_deref(),
        Some("Commit pending changes")
    );
    assert_eq!(
        command_completion_preview("/pr").as_deref(),
        Some("Create or update a pull request")
    );
    assert_eq!(
        command_completion_preview("/wizard").as_deref(),
        Some("Alias for /providers add (create or edit a custom provider) [<provider-id>]")
    );
    assert_eq!(command_usage_hint("/export"), Some("<path>"));
    assert_eq!(
        command_completion_preview("/export").as_deref(),
        Some("Export the current plan to a file [<path>]")
    );
}

#[test]
fn busy_behavior_classifies_commands_by_run_state_policy() {
    use BusyCommandBehavior::{Block, DeferUntilIdle, ReadOnlyNow, RunNow};

    let cases = [
        ("/ctx", ReadOnlyNow),
        ("/perf", ReadOnlyNow),
        ("/cost", ReadOnlyNow),
        ("/usage", ReadOnlyNow),
        ("/sessions", ReadOnlyNow),
        ("/model", ReadOnlyNow),
        ("/model gpt-5.3", DeferUntilIdle),
        // Setting toggles apply mid-run (audit H1): a user typing
        // `/sandbox off` while the agent is busy means it.
        ("/sandbox status", ReadOnlyNow),
        ("/sandbox on", RunNow),
        ("/sandbox net on", RunNow),
        ("/sandbox bogus", Block),
        ("/self-review status", ReadOnlyNow),
        ("/self-review on", RunNow),
        // Parser-accepted aliases behave like their canonical forms (the old
        // hand-written match wrongly blocked these).
        ("/self-review no", RunNow),
        ("/self-review true", RunNow),
        ("/self-review bogus", Block),
        ("/smol on", RunNow),
        ("/smol", RunNow),
        ("/smol status", ReadOnlyNow),
        ("/serenity on", RunNow),
        ("/serenity status", ReadOnlyNow),
        // `/settings` seeds from the live agent profile (needs the agent
        // lock), so unlike `/mode` it cannot open mid-run.
        ("/settings", Block),
        ("/remember use tabs", RunNow),
        ("/memory", ReadOnlyNow),
        ("/memory some-entry", ReadOnlyNow),
        ("/memory forget some-entry", RunNow),
        ("/memory forget some-entry extra", Block),
        ("/commit", Block),
        ("/pr", Block),
        ("/security-review", Block),
        ("/compact", Block),
        ("/clear", Block),
        ("/does-not-exist", Block),
    ];

    for (input, expected) in cases {
        assert_eq!(busy_behavior_for(input), expected, "input: {input}");
    }
}

#[test]
fn every_registered_command_declares_its_busy_behavior() {
    use BusyCommandBehavior::{Block, DeferUntilIdle, ReadOnlyNow, RunNow};

    // Bare-form non-idle behavior for every registered command, consumed by
    // both the running composer dispatcher and the busy-command modal.
    // Exhaustive by construction: registering a command without deciding its
    // mid-run behavior here fails the coverage assertion below, so non-idle
    // behavior stays a conscious declaration instead of a silent fall-through.
    let expected = [
        ("/help", ReadOnlyNow),
        ("/keys", ReadOnlyNow),
        ("/select", ReadOnlyNow),
        ("/ctx", ReadOnlyNow),
        ("/episodes", ReadOnlyNow),
        ("/perf", ReadOnlyNow),
        ("/cost", ReadOnlyNow),
        ("/usage", ReadOnlyNow),
        ("/compact", Block),
        ("/autonomy", RunNow),
        ("/yolo", RunNow),
        // Bare toggle commands: `/self-review` and `/sandbox` parse bare as
        // status (read-only); `/smol` and `/serenity` parse bare as a toggle
        // (a set, which applies mid-run).
        ("/self-review", ReadOnlyNow),
        ("/smol", RunNow),
        ("/serenity", RunNow),
        ("/sandbox", ReadOnlyNow),
        ("/permissions", RunNow),
        ("/config", Block),
        ("/doctor", Block),
        ("/mcp", Block),
        ("/hooks", Block),
        ("/mode", RunNow),
        ("/settings", Block),
        ("/tasks", ReadOnlyNow),
        ("/init", Block),
        ("/skills", ReadOnlyNow),
        ("/skill", Block),
        ("/agents", ReadOnlyNow),
        ("/remember", RunNow),
        ("/memory", ReadOnlyNow),
        ("/peers", ReadOnlyNow),
        ("/subagents", ReadOnlyNow),
        ("/start", Block),
        ("/continue", Block),
        ("/test", Block),
        ("/build", Block),
        ("/retry", Block),
        ("/save", RunNow),
        ("/discard", Block),
        ("/export", RunNow),
        ("/plans", RunNow),
        ("/sessions", ReadOnlyNow),
        ("/resume", Block),
        ("/forget", Block),
        ("/search", RunNow),
        ("/authorize", RunNow),
        ("/unauthorize", Block),
        ("/model", ReadOnlyNow),
        ("/refresh", Block),
        ("/providers", ReadOnlyNow),
        ("/wizard", Block),
        ("/new", Block),
        ("/new-plan", Block),
        ("/clear", Block),
        ("/copy", RunNow),
        ("/commit", Block),
        ("/pr", Block),
        ("/review", RunNow),
        ("/security-review", Block),
        ("/theme", RunNow),
        ("/bonsai", Block),
        ("/quit", Block),
        ("/bug", DeferUntilIdle),
        ("/update", RunNow),
    ];

    for (name, behavior) in expected {
        assert_eq!(busy_behavior_for(name), behavior, "command: {name}");
    }

    let declared: std::collections::BTreeSet<_> = expected.iter().map(|(name, _)| *name).collect();
    let registered: std::collections::BTreeSet<_> = crate::commands::COMMANDS
        .iter()
        .map(|command| command.name)
        .collect();
    assert_eq!(
        declared, registered,
        "the busy-behavior table must cover exactly the registered commands"
    );
}

#[tokio::test]
async fn model_numeric_argument_uses_current_provider_model() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();
    let catalog = catalog_with_live_models(
        "opencode",
        ["model-one", "model-two", "model-three", "model-four"],
    );

    store.session_mut("minimax-coding-plan").api_key = "sk-mm".to_string();

    let outcome = handle_command_with_model_catalog(
        "/model 4",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
        &catalog,
    )
    .await
    .unwrap();

    assert!(!outcome.opens_model_picker());
    assert_eq!(store.current_kind_id(), "opencode");
    assert_eq!(store.session("opencode").model, "opencode/model-four");
    assert_eq!(
        store.active_connection_id().map(|id| id.as_str()),
        Some("opencode")
    );
    assert_eq!(
        store.active_model_id().map(|id| id.as_str()),
        Some("opencode/model-four")
    );
}

#[tokio::test]
async fn model_command_records_selection_against_the_active_mode() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();
    let catalog = catalog_with_live_models("opencode", ["model-one", "model-two"]);

    let previous = store.current_model_selection_input();

    // The test harness dispatches with active_mode = Coding: the switch lands
    // in coding's entry and planning is seeded with the pre-switch selection.
    handle_command_with_model_catalog(
        "/model 2",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
        &catalog,
    )
    .await
    .unwrap();

    assert_eq!(
        store.mode_model("coding"),
        Some("opencode:opencode/model-two")
    );
    assert_eq!(store.mode_model("planning"), Some(previous.as_str()));
}

#[tokio::test]
async fn model_canonical_argument_switches_to_matching_connection() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();
    let catalog = test_catalog();

    store.set_current_kind_id("opencode");
    store.session_mut("codex").api_key = "codex-token".to_string();
    store.session_mut("codex").account_id = "codex-account".to_string();

    let outcome = handle_command_with_model_catalog(
        "/model openai/gpt-5.5",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
        &catalog,
    )
    .await
    .unwrap();

    assert!(!outcome.opens_model_picker());
    assert_eq!(store.current_kind_id(), "codex");
    assert_eq!(store.session("codex").model, "openai/gpt-5.5");
    assert_eq!(
        store.active_connection_id().map(|id| id.as_str()),
        Some("codex")
    );
    assert_eq!(
        store.active_model_id().map(|id| id.as_str()),
        Some("openai/gpt-5.5")
    );
    assert!(outcome.messages.iter().any(|message| {
        matches!(message.kind, CommandMessageKind::Status)
            && message.text.contains("Model set to openai/gpt-5.5")
    }));
}

#[tokio::test]
async fn model_connection_prefixed_argument_selects_connection_target() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();
    let catalog = test_catalog();

    store.set_current_kind_id("opencode");

    let outcome = handle_command_with_model_catalog(
        "/model opencode:opencode/glm-5.2",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
        &catalog,
    )
    .await
    .unwrap();

    assert!(!outcome.opens_model_picker());
    assert_eq!(store.current_kind_id(), "opencode");
    assert_eq!(store.session("opencode").model, "opencode/glm-5.2");
    assert_eq!(
        store.active_connection_id().map(|id| id.as_str()),
        Some("opencode")
    );
    assert_eq!(
        store.active_model_id().map(|id| id.as_str()),
        Some("opencode/glm-5.2")
    );
}

#[test]
fn model_display_alias_argument_selects_remote_target() {
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let provider_dir = dir.path().join("providers");
    let model_dir = dir.path().join("models");
    std::fs::create_dir_all(&provider_dir).unwrap();
    std::fs::create_dir_all(&model_dir).unwrap();
    // `openai-compatible` is a user catalog entry now (what the fold-in
    // migration writes), so the test defines the connection alongside the
    // aliased target.
    std::fs::write(
        provider_dir.join("openai-compatible.toml"),
        r#"
            [[connections]]
            id = "openai-compatible"
            display_name = "OpenAI Compatible"
            auth = "optional-api-key"
            transport = "openai-chat"
            default_base_url = "http://localhost:1234/v1"
        "#,
    )
    .unwrap();
    std::fs::write(
        model_dir.join("qwen.toml"),
        r#"
            [[targets]]
            connection = "openai-compatible"
            model = "Qwen"
            remote_model = "qwen/qwen3.6-35b-a3b"
            context_window = 131072
        "#,
    )
    .unwrap();
    let catalog = crate::model_catalog::ModelCatalog::from_spec(
        crate::model_catalog::load_catalog_with_user_dirs(&provider_dir, &model_dir).unwrap(),
    );
    let registry = std::sync::Arc::new(crate::provider::ProviderRegistry::from_catalog(&catalog));

    store.ensure_provider("openai-compatible");
    store.set_current_kind_id("openai-compatible");

    let selection =
        super::resolve_model_selection(registry.as_ref(), &store, Some(&catalog), "Qwen").unwrap();
    assert_eq!(selection.model, "Qwen");
    super::apply_model_selection(
        registry.as_ref(),
        &mut store,
        Some(&catalog),
        &selection,
        crate::provider::ReasoningSelection::Default,
    );

    let run_target = crate::model_resolution::run_target_for_current_model_with_catalog(
        registry.as_ref(),
        &store,
        Some(&catalog),
    );

    assert_eq!(store.current_kind_id(), "openai-compatible");
    assert_eq!(
        store.session("openai-compatible").model,
        "openai-compatible/Qwen"
    );
    assert_eq!(
        store.active_connection_id().map(|id| id.as_str()),
        Some("openai-compatible")
    );
    assert_eq!(
        store.active_model_id().map(|id| id.as_str()),
        Some("openai-compatible/Qwen")
    );
    assert_eq!(run_target.remote_model_id.as_ref(), "qwen/qwen3.6-35b-a3b");
}

#[tokio::test]
async fn model_opens_picker_without_switching_when_current_provider_unauthorized() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    store.set_current_kind_id("minimax-coding-plan");
    store.session_mut("minimax-coding-plan").api_key = "sk-mm".to_string();
    store.session_mut("codex").api_key = "codex-token".to_string();
    store.session_mut("codex").account_id = "codex-account".to_string();

    let outcome = handle_command("/model", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(outcome.opens_model_picker());
    assert_eq!(store.current_kind_id(), "minimax-coding-plan");
    assert!(
        crate::model_catalog::available_model_ids_for_provider(
            Some(&test_catalog()),
            "minimax-coding-plan",
            crate::provider::metadata_for("minimax-coding-plan").unwrap(),
            &store.session("minimax-coding-plan").model,
        )
        .contains(&"MiniMax-M3".to_string())
    );
}

#[tokio::test]
async fn model_command_uses_live_availability_catalog() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();
    let catalog = catalog_with_live_models("minimax-coding-plan", ["MiniMax-M3", "MiniMax-M2.7"]);

    store.set_current_kind_id("minimax-coding-plan");

    let outcome = handle_command_with_model_catalog(
        "/model MiniMax-M2.7",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
        &catalog,
    )
    .await
    .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("Model set to MiniMax-M2.7"))
    );
    assert_eq!(
        store.session("minimax-coding-plan").model,
        "minimax-coding-plan/MiniMax-M2.7"
    );
    assert_eq!(
        store.active_connection_id().map(|id| id.as_str()),
        Some("minimax-coding-plan")
    );
    assert_eq!(
        store.active_model_id().map(|id| id.as_str()),
        Some("minimax-coding-plan/MiniMax-M2.7")
    );
}

#[tokio::test]
async fn refresh_all_provider_models_covers_every_authorized_provider() {
    let registry = ProviderRegistry::new(vec![Arc::new(MockCodexFactory)]);
    let mut store = session_store();
    let catalog = test_catalog();
    store.session_mut("codex").api_key = "token".to_string();
    store.session_mut("codex").account_id = "account".to_string();

    let report =
        crate::commands::providers::refresh_all_provider_models(&registry, &store, &catalog)
            .await
            .unwrap();

    assert!(report.contains("Codex: 1 models"), "report: {report}");
    let codex = "codex"
        .parse::<crate::model_catalog::ConnectionId>()
        .unwrap();
    assert_eq!(
        catalog.available_models_for_connection(&codex, Vec::new()),
        vec!["gpt-5.3-codex-spark".to_string()]
    );
}

#[tokio::test]
async fn refresh_all_provider_models_requires_an_authorized_provider() {
    let registry = ProviderRegistry::new(vec![Arc::new(MockCodexFactory)]);
    let store = session_store();
    let catalog = test_catalog();

    let err = crate::commands::providers::refresh_all_provider_models(&registry, &store, &catalog)
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("No authorized providers"),
        "err: {err:#}"
    );
}

#[tokio::test]
async fn refresh_stale_model_cache_replaces_codex_seed_cache() {
    let registry = ProviderRegistry::new(vec![Arc::new(MockCodexFactory)]);
    let mut store = session_store();
    let catalog = test_catalog();
    store.session_mut("codex").api_key = "token".to_string();
    store.session_mut("codex").account_id = "account".to_string();

    let session = store.session("codex").clone();
    let changed = refresh_stale_model_cache(&registry, "codex", &session, Some(&catalog))
        .await
        .unwrap();

    assert!(changed, "an empty live cache should be refreshed");
    let codex = "codex"
        .parse::<crate::model_catalog::ConnectionId>()
        .unwrap();
    assert_eq!(
        catalog.available_models_for_connection(&codex, Vec::new()),
        vec!["gpt-5.3-codex-spark".to_string()]
    );
}

#[tokio::test]
async fn refresh_stale_model_cache_keeps_a_fresh_codex_cache() {
    let registry = ProviderRegistry::new(vec![Arc::new(MockCodexFactory)]);
    let mut store = session_store();
    let catalog = test_catalog();
    store.session_mut("codex").api_key = "token".to_string();
    store.session_mut("codex").account_id = "account".to_string();
    let codex = "codex"
        .parse::<crate::model_catalog::ConnectionId>()
        .unwrap();
    catalog
        .write_live_availability(
            &codex,
            crate::model_catalog::LiveModelAvailability::from_remote_ids(["gpt-fresh".to_string()]),
        )
        .unwrap();

    let session = store.session("codex").clone();
    let changed = refresh_stale_model_cache(&registry, "codex", &session, Some(&catalog))
        .await
        .unwrap();

    assert!(
        !changed,
        "a fresh live cache should avoid a network refresh"
    );
}

fn agent_with_skills(skills: crate::resource::skill::SkillRegistry) -> Agent {
    let fixture = TestFixture::new();
    Agent::builder(
        Box::new(MockProvider),
        Arc::new(ToolRegistry::new()),
        Arc::new(ToolRegistry::new()),
        fixture.read_tracker,
        fixture.project_root,
    )
    .skills(crate::resource::skill::shared_registry(skills))
    .build()
    .unwrap()
}

fn skills_with_deploy() -> crate::resource::skill::SkillRegistry {
    let dir = tempdir().unwrap();
    let path = dir.path().join(".bonsai/skills/deploy/SKILL.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        "---\nname: deploy\ndescription: Ship the release\n---\nRun deploy.sh",
    )
    .unwrap();
    // Bodies are read into memory during load, so the tempdir can drop after.
    crate::resource::skill::SkillRegistry::load_from(dir.path(), &dir.path().join("home"))
}

#[tokio::test]
async fn skills_lists_available_skills() {
    let mut agent = agent_with_skills(skills_with_deploy());
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/skills", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("deploy")
                && message.text.contains("Ship the release")
                && message.text.contains("project"))
    );
}

#[tokio::test]
async fn skills_shows_builtin_active_inactive_and_disabled() {
    // A Rust project (rust-writer active), with go-writer inactive and
    // shell-writer disabled via a .disabled list.
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
    let skills_dir = dir.path().join(".bonsai/skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(skills_dir.join(".disabled"), "shell-writer\n").unwrap();
    let skills =
        crate::resource::skill::SkillRegistry::load_from(dir.path(), &dir.path().join("home"));

    let mut agent = agent_with_skills(skills);
    let mut store = session_store();
    let registry = registry();

    let outcome = handle_command("/skills", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();
    let text = outcome
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(text.contains("Built-in skills:"), "{text}");
    assert!(
        text.contains("rust-writer") && text.contains("(active)"),
        "{text}"
    );
    assert!(
        text.contains("go-writer") && text.contains("activate with"),
        "{text}"
    );
    assert!(
        text.contains("shell-writer") && text.contains("(disabled)"),
        "{text}"
    );
    assert!(text.contains(".bonsai/skills/.disabled"), "{text}");
}

#[tokio::test]
async fn skills_disable_and_enable_edit_the_disabled_file() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
    let skills =
        crate::resource::skill::SkillRegistry::load_from(dir.path(), &dir.path().join("home"));
    let mut agent = agent_with_skills(skills);
    let mut store = session_store();

    // Disable a real built-in: writes the file and applies live (no relaunch).
    let out = handle_command(
        "/skills disable rust-writer",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    assert!(out.messages.iter().any(|message| {
        message
            .text
            .contains("Disabled built-in skill 'rust-writer'")
            && !message.text.contains("Relaunch")
    }));
    let disabled_path = dir.path().join(".bonsai/skills/.disabled");
    assert_eq!(
        std::fs::read_to_string(&disabled_path).unwrap(),
        "rust-writer\n"
    );

    // Re-enabling removes it from the file.
    let out = handle_command(
        "/skills enable rust-writer",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    assert!(out.messages.iter().any(|message| {
        message
            .text
            .contains("Re-enabled built-in skill 'rust-writer'")
    }));
    assert_eq!(std::fs::read_to_string(&disabled_path).unwrap(), "");
}

#[tokio::test]
async fn skills_disable_rejects_unknown_name() {
    let dir = tempdir().unwrap();
    let mut agent = agent_with_skills(skills_with_deploy());
    let mut store = session_store();

    let out = handle_command(
        "/skills disable not-a-skill",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    assert!(
        out.messages
            .iter()
            .any(|message| message.text.contains("is not a built-in skill"))
    );
    assert!(!dir.path().join(".bonsai/skills/.disabled").exists());
}

#[tokio::test]
async fn skills_reports_empty_state() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/skills", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("No project skills"))
    );
}

#[tokio::test]
async fn skill_loads_named_skill_into_context() {
    let mut agent = agent_with_skills(skills_with_deploy());
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/skill deploy",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(outcome.messages.iter().any(|message| matches!(
        message.kind,
        CommandMessageKind::Status
    )
        && message.text.contains("Loaded skill 'deploy'")));
}

#[tokio::test]
async fn skill_unknown_name_guides_with_available() {
    let mut agent = agent_with_skills(skills_with_deploy());
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command(
        "/skill nope",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry,
    )
    .await
    .unwrap();

    assert!(outcome.messages.iter().any(|message| matches!(
        message.kind,
        CommandMessageKind::Error
    )
        && message.text.contains("Unknown skill 'nope'")
        && message.text.contains("deploy")));
}

#[tokio::test]
async fn skill_without_argument_shows_usage() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/skill", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| matches!(message.kind, CommandMessageKind::Error)
                && message.text.contains("Usage: /skill"))
    );
}

#[tokio::test]
async fn agents_lists_builtin_subagents() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/agents", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();

    assert!(
        outcome
            .messages
            .iter()
            .any(|message| message.text.contains("explore") && message.text.contains("review"))
    );
}

fn agent_with_custom_agents(custom: crate::resource::agent::AgentRegistry) -> Agent {
    let fixture = TestFixture::new();
    Agent::builder(
        Box::new(MockProvider),
        Arc::new(ToolRegistry::new()),
        Arc::new(ToolRegistry::new()),
        fixture.read_tracker,
        fixture.project_root,
    )
    .custom_agents(crate::resource::agent::shared_registry(custom))
    .build()
    .unwrap()
}

fn custom_agents_with(files: &[(&str, &str)]) -> crate::resource::agent::AgentRegistry {
    let dir = tempdir().unwrap();
    for (name, contents) in files {
        let path = dir.path().join(format!(".bonsai/agents/{name}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    crate::resource::agent::AgentRegistry::load_from(dir.path(), &dir.path().join("home"))
}

#[tokio::test]
async fn agents_lists_custom_with_provenance_and_invalid_note() {
    let custom = custom_agents_with(&[
        (
            "api-explorer",
            "---\nname: api-explorer\ndescription: maps routes\ntools: [read, grep]\nmax_turns: 72\n---\nprompt",
        ),
        (
            "danger",
            "---\nname: danger\ndescription: nope\ntools: [frobnicate]\n---\nprompt",
        ),
    ]);
    let mut agent = agent_with_custom_agents(custom);
    let mut store = session_store();
    let dir = tempdir().unwrap();
    let registry = registry();

    let outcome = handle_command("/agents", &mut agent, &mut store, dir.path(), &[], registry)
        .await
        .unwrap();
    let text = outcome
        .messages
        .iter()
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        text.contains("api-explorer")
            && text.contains("(project · subagent)")
            && text.contains("max turns: 72"),
        "{text}"
    );
    assert!(
        text.contains("explore") && text.contains("(built-in · subagent)"),
        "{text}"
    );
    // A custom agent declaring an unknown tool has it flagged as ignored (bash,
    // by contrast, is grantable now and would not be flagged).
    assert!(
        text.contains("danger") && text.contains("frobnicate"),
        "{text}"
    );
}

#[tokio::test]
async fn agents_lists_disabled_agent_with_marker_and_model() {
    let custom = custom_agents_with(&[(
        "fast",
        "---\nname: fast\ndescription: cheap\nmodel: haiku\neffort: low\nenabled: false\n---\nprompt",
    )]);
    let mut agent = agent_with_custom_agents(custom);
    let mut store = session_store();
    let dir = tempdir().unwrap();

    let outcome = handle_command(
        "/agents",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    let text = outcome
        .messages
        .iter()
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        text.contains("fast") && text.contains("(disabled)"),
        "{text}"
    );
    assert!(
        text.contains("model: haiku") && text.contains("effort low"),
        "{text}"
    );
}

#[tokio::test]
async fn agents_list_separates_mode_agents_and_applies_builtin_settings() {
    let custom = custom_agents_with(&[
        (
            "interactive",
            "---\nname: interactive\ndescription: mode only\nsurface: [mode]\n---\nprompt",
        ),
        (
            "helper",
            "---\nname: helper\ndescription: delegated\nsurface: [subagent]\n---\nprompt",
        ),
        (
            "explore",
            "---\nname: explore\ndescription: legacy shadow\nmodel: legacy-model\n---\nCUSTOM",
        ),
    ]);
    let mut agent = agent_with_custom_agents(custom);
    agent.builtin_subagent_settings_handle().upsert(
        crate::subagent::BuiltinSubagentId::Explore,
        crate::subagent::BuiltinSubagentSettings {
            enabled: false,
            primary_model: Some("persisted-model".to_string()),
            primary_effort: Some("high".to_string()),
            fallback_model: None,
            fallback_effort: None,
        },
    );
    let mut store = session_store();
    let dir = tempdir().unwrap();

    let outcome = handle_command(
        "/agents",
        &mut agent,
        &mut store,
        dir.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    let text = outcome
        .messages
        .iter()
        .map(|message| message.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        text.contains("interactive — mode only (project · agent)"),
        "{text}"
    );
    assert!(
        text.contains("helper — delegated (project · subagent)"),
        "{text}"
    );
    assert_eq!(text.matches("  explore —").count(), 1, "{text}");
    assert!(text.contains("(built-in · subagent)  (disabled)"), "{text}");
    assert!(
        text.contains("model: persisted-model, effort high"),
        "{text}"
    );
    assert!(!text.contains("legacy shadow"), "{text}");
    assert!(!text.contains("legacy-model"), "{text}");
}

#[tokio::test]
async fn agents_init_writes_disabled_example() {
    let mut agent = agent();
    let mut store = session_store();
    let project = tempdir().unwrap();

    let outcome = handle_command(
        "/agents init",
        &mut agent,
        &mut store,
        project.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    assert!(outcome.messages.iter().any(|m| m.text.contains("Created")));

    let written =
        std::fs::read_to_string(project.path().join(".bonsai/agents/example.md")).unwrap();
    assert!(written.contains("enabled: false"), "{written}");

    // Second run must not overwrite.
    let again = handle_command(
        "/agents init",
        &mut agent,
        &mut store,
        project.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    assert!(
        again
            .messages
            .iter()
            .any(|m| m.text.contains("already exists"))
    );
}

#[tokio::test]
async fn memory_commands_error_without_memory_service() {
    let mut agent = agent();
    let mut store = session_store();
    let dir = tempdir().unwrap();

    for input in ["/memory", "/remember use tabs"] {
        let outcome = handle_command(input, &mut agent, &mut store, dir.path(), &[], registry())
            .await
            .unwrap();
        assert_eq!(outcome.messages.len(), 1, "input: {input}");
        assert_eq!(outcome.messages[0].kind, CommandMessageKind::Error);
        assert!(
            outcome.messages[0].text.contains("unavailable"),
            "input: {input}: {}",
            outcome.messages[0].text
        );
    }
}

#[tokio::test]
async fn remember_list_view_forget_round_trip() {
    let mut agent = agent();
    let mut store = session_store();
    let home = tempdir().unwrap();
    let project = tempdir().unwrap();
    let db_dir = tempdir().unwrap();
    let storage = crate::storage::Storage::open_at(db_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    agent.set_memory(Arc::new(crate::memory::MemoryService::load(
        home.path(),
        project.path(),
        storage,
        0,
    )));

    let outcome = handle_command(
        "/remember use --no-verify for partial commits",
        &mut agent,
        &mut store,
        project.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    assert_eq!(outcome.messages[0].kind, CommandMessageKind::Status);
    assert!(
        outcome.messages[0].text.starts_with("remembered: "),
        "{}",
        outcome.messages[0].text
    );
    let name = "use-no-verify-for-partial-commits";
    assert!(home.path().join(format!("memory/{name}.md")).is_file());

    let listing = handle_command(
        "/memory",
        &mut agent,
        &mut store,
        project.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    assert!(listing.messages[0].text.contains(name));
    assert!(listing.messages[0].text.contains("Stores:"));

    let view = handle_command(
        &format!("/memory {name}"),
        &mut agent,
        &mut store,
        project.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    assert!(view.messages[0].text.contains("type: preference"));

    let forgotten = handle_command(
        &format!("/memory forget {name}"),
        &mut agent,
        &mut store,
        project.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    assert_eq!(forgotten.messages[0].kind, CommandMessageKind::Status);
    assert!(!home.path().join(format!("memory/{name}.md")).exists());

    let unknown = handle_command(
        "/memory forget nothing-here",
        &mut agent,
        &mut store,
        project.path(),
        &[],
        registry(),
    )
    .await
    .unwrap();
    assert_eq!(unknown.messages[0].kind, CommandMessageKind::Error);
}
