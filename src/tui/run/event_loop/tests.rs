use super::*;
use crate::background::{BackgroundTaskEvent, BackgroundTaskRegistry};
use crate::interaction::QuestionOption;
use crate::output::OutputSink;
use crate::plan::SharedPlanStore;
use crate::provider::{
    AuthInput, AuthorizeOutcome, Provider, ProviderFactory, ProviderMetadata, ReasoningSelection,
    StreamedResponse,
};
use crate::tool::test_utils::TestFixture;
use crate::tool::{Tool, ToolOutput, ToolRegistry};
use crate::tui::app::{
    DeferredCommand, DeferredCommandPayload, ModelPickerPane, QueuedInput, ToolActivity, ToolStatus,
};
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};

mod plan_execution;
use plan_execution::{drain_tasks, two_phase_plan};

fn smol_off() -> crate::smol::SmolProfile {
    crate::smol::SmolProfile::resolve(crate::smol::SmolPreference::Off, 128_000)
}

struct NullSink;

impl OutputSink for NullSink {}

struct BlockingProvider;

#[async_trait]
impl Provider for BlockingProvider {
    async fn chat_stream(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        cancellation_token: tokio_util::sync::CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        cancellation_token.cancelled().await;
        Ok(StreamedResponse::interrupted())
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
}

struct CompleteProvider;

#[async_trait]
impl Provider for CompleteProvider {
    async fn chat_stream(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        _cancellation_token: tokio_util::sync::CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        })
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedProviderRequest {
    provider_id: String,
    model: String,
    reasoning: ReasoningSelection,
    context_window: Option<u32>,
    conversation_cache_key: String,
}

struct RecordingProviderFactory {
    metadata: &'static ProviderMetadata,
    requests: Arc<Mutex<Vec<RecordedProviderRequest>>>,
}

impl RecordingProviderFactory {
    fn new(provider_id: &str, requests: Arc<Mutex<Vec<RecordedProviderRequest>>>) -> Self {
        Self {
            metadata: crate::provider::metadata_for(provider_id)
                .expect("recording provider metadata should exist"),
            requests,
        }
    }
}

#[async_trait]
impl ProviderFactory for RecordingProviderFactory {
    fn metadata(&self) -> &ProviderMetadata {
        self.metadata
    }

    fn build_target(
        &self,
        _session: &crate::session::ProviderSession,
        target: &crate::model_catalog::RunTarget,
    ) -> Box<dyn Provider> {
        Box::new(RecordingProvider {
            provider_id: self.metadata.id.to_string(),
            model: target.remote_model_id.to_string(),
            reasoning: target.reasoning,
            context_window: target.context_window,
            conversation_cache_key: String::new(),
            requests: self.requests.clone(),
        })
    }

    fn is_authorized(&self, _session: &crate::session::ProviderSession) -> bool {
        true
    }

    async fn list_models(
        &self,
        _session: &crate::session::ProviderSession,
    ) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

struct RecordingProvider {
    provider_id: String,
    model: String,
    reasoning: ReasoningSelection,
    context_window: Option<u32>,
    conversation_cache_key: String,
    requests: Arc<Mutex<Vec<RecordedProviderRequest>>>,
}

#[async_trait]
impl Provider for RecordingProvider {
    async fn chat_stream(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        _cancellation_token: tokio_util::sync::CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.requests.lock().await.push(RecordedProviderRequest {
            provider_id: self.provider_id.clone(),
            model: self.model.clone(),
            reasoning: self.reasoning,
            context_window: self.context_window,
            conversation_cache_key: self.conversation_cache_key.clone(),
        });
        Ok(StreamedResponse {
            content: "done".to_string(),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            ..StreamedResponse::default()
        })
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn set_conversation_cache_key(&mut self, key: &str) {
        self.conversation_cache_key = key.to_string();
    }
}

struct ModelListingCodexFactory {
    list_models_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl ProviderFactory for ModelListingCodexFactory {
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
        self.list_models_calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec!["gpt-new".to_string()])
    }
}

struct CapturingProvider {
    requests: Arc<Mutex<Vec<Vec<String>>>>,
}

impl CapturingProvider {
    fn new() -> (Self, Arc<Mutex<Vec<Vec<String>>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                requests: requests.clone(),
            },
            requests,
        )
    }
}

#[async_trait]
impl Provider for CapturingProvider {
    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        _cancellation_token: tokio_util::sync::CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        let user_messages = messages
            .iter()
            .filter(|message| matches!(message, ChatCompletionRequestMessage::User(_)))
            .map(message_content)
            .collect::<Vec<_>>();
        self.requests.lock().await.push(user_messages);
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        })
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
}

fn message_content(message: &ChatCompletionRequestMessage) -> String {
    let value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    value
        .get("content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn app() -> AppState {
    AppState::new(
        "codex",
        "test-model".to_string(),
        "workspace".to_string(),
        None,
    )
}

#[test]
fn safe_plan_start_confirmation_accepts_only_exact_scoped_words_once() {
    for input in ["go", "GO", " continue "] {
        let mut app = app();
        app.pending_safe_plan_start_confirmation = true;
        assert!(take_safe_plan_start_confirmation(&mut app, input));
        assert!(!app.pending_safe_plan_start_confirmation);
        assert!(
            !take_safe_plan_start_confirmation(&mut app, input),
            "confirmation authority must be one-shot"
        );
    }

    for input in [
        "yes",
        "go ahead and delete it",
        "continue with deployment",
        "allow",
        "approve",
    ] {
        let mut app = app();
        app.pending_safe_plan_start_confirmation = true;
        assert!(!take_safe_plan_start_confirmation(&mut app, input));
        assert!(!app.pending_safe_plan_start_confirmation);
    }
}

#[test]
fn safe_plan_start_confirmation_requires_prior_planning_completion() {
    let mut app = app();
    assert!(!take_safe_plan_start_confirmation(&mut app, "go"));
}

fn provider_command_event() -> RuntimeEvent {
    provider_command_event_with_generation(None)
}

fn provider_command_event_with_generation(generation: Option<u64>) -> RuntimeEvent {
    RuntimeEvent::CommandFinished(Box::new(CommandOutcomeEvent::Applied {
        generation,
        clear_transcript: false,
        messages: Vec::new(),
        provider: Some(Box::new(ProviderRunSelection {
            provider: "codex".to_string(),
            model: "test-model".to_string(),
            reasoning: ReasoningSelection::Default,
        })),
        context_report: None,
        quit: false,
        open_modal: None,
    }))
}

#[test]
fn first_run_transition_requires_a_submitted_model_selection() {
    let mut app = app();
    app.first_run_step = Some(crate::onboarding::FirstRunStep::Model);

    let unrelated = FirstRunRuntimeTransition::capture(&app, &provider_command_event());
    assert!(unrelated.provider_selection_applied);
    assert_eq!(unrelated.model_selection_pending, None);

    let generation = app.command_generation();
    app.first_run_model_selection_pending = Some(generation);
    let selection = FirstRunRuntimeTransition::capture(
        &app,
        &provider_command_event_with_generation(Some(generation)),
    );
    assert!(selection.command_finished);
    assert!(selection.provider_selection_applied);
    assert_eq!(
        selection.model_selection_pending,
        Some(app.command_generation())
    );
    assert_eq!(selection.command_generation, Some(generation));
}

#[test]
fn first_run_transition_does_not_correlate_an_unrelated_command() {
    let mut app = app();
    app.first_run_step = Some(crate::onboarding::FirstRunStep::Model);
    let generation = app.command_generation();
    app.first_run_model_selection_pending = Some(generation);

    let unrelated = FirstRunRuntimeTransition::capture(
        &app,
        &provider_command_event_with_generation(Some(generation.wrapping_add(1))),
    );

    assert_ne!(
        unrelated.model_selection_pending,
        unrelated.command_generation
    );
}

#[test]
fn first_run_transition_accepts_only_a_completed_initial_prompt() {
    let mut app = app();
    app.first_run_step = Some(crate::onboarding::FirstRunStep::FirstPrompt);

    let completed = FirstRunRuntimeTransition::capture(
        &app,
        &RuntimeEvent::AgentFinished(Ok(crate::tui::event::AgentRunOutcome::Completed)),
    );
    assert!(completed.agent_completed);

    let failed = FirstRunRuntimeTransition::capture(
        &app,
        &RuntimeEvent::AgentFinished(Ok(crate::tui::event::AgentRunOutcome::Failed)),
    );
    assert!(!failed.agent_completed);
}

fn shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn background_task_snapshot(
    id: &str,
    status: BackgroundTaskStatus,
    tool_call_id: Option<&str>,
    tail: &str,
) -> BackgroundTaskSnapshot {
    BackgroundTaskSnapshot {
        id: id.to_string(),
        incarnation: "test-task".to_string(),
        command: "sleep 30".to_string(),
        cwd: std::path::PathBuf::from("/tmp/project"),
        status,
        started_at: std::time::SystemTime::now(),
        finished_at: status.is_finished().then(std::time::SystemTime::now),
        exit_code: None,
        timeout_secs: 30,
        timed_out: false,
        tail: tail.to_string(),
        tail_truncated: false,
        total_output_chars: tail.chars().count(),
        version: 1,
        tool_call_id: tool_call_id.map(str::to_string),
    }
}

fn test_agent(provider: Box<dyn Provider>) -> Arc<Mutex<Agent>> {
    let fixture = TestFixture::new();
    Arc::new(Mutex::new(
        Agent::new(
            provider,
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .expect("test agent should build"),
    ))
}

/// Regression: `/self-review <mode>` used to change state silently, which was
/// indistinguishable from a no-op — the set must confirm like the headless
/// handler does.
#[tokio::test]
async fn self_review_set_updates_state_and_confirms_in_transcript() {
    let mut app = app();
    let agent = test_agent(Box::new(CompleteProvider));
    assert_eq!(
        app.self_review_mode,
        crate::self_review::SelfReviewMode::Auto
    );

    // The mode-set path ignores the picker handles, but the signature needs
    // them; build minimal valid ones.
    let catalog = test_model_catalog();
    let registry = Arc::new(crate::provider::ProviderRegistry::from_catalog(&catalog));
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let temp_dir = tempfile::tempdir().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    crate::tui::run::commands::apply_self_review_command(
        "/self-review off",
        &mut app,
        agent.clone(),
        &session_store,
        &registry,
        &catalog,
        &storage,
        true,
    )
    .await;

    assert_eq!(
        app.self_review_mode,
        crate::self_review::SelfReviewMode::Off
    );
    assert_eq!(
        agent.lock().await.self_review_mode(),
        crate::self_review::SelfReviewMode::Off,
        "sync_agent must mirror the mode into the live agent"
    );
    assert!(app.transcript.is_empty());
    assert!(
        app.session_toast
            .as_ref()
            .is_some_and(|toast| toast.text.starts_with("Self-review set to")),
        "the set must confirm through a transient toast"
    );
}

fn sample_plan(title: &str) -> crate::plan::PlanDoc {
    let mut plan = crate::plan::PlanDoc::default();
    plan.edit().set_title(title);
    plan.edit().set_section("Approach", "Do the work.");
    plan.edit().add_task("Ship it");
    plan
}

fn sample_verification_run(
    started_at_ms: i64,
    command: &str,
) -> crate::verification::VerificationRunRecord {
    crate::verification::VerificationRunRecord {
        kind: crate::verification::VerificationKind::Test,
        status: crate::verification::VerificationRunStatus::Passed,
        checks: vec![crate::verification::VerificationCheckRecord {
            name: "Rust tests".to_string(),
            command: command.to_string(),
            status: crate::verification::VerificationCheckStatus::Passed,
            tool_call_id: Some(format!("call-{started_at_ms}")),
            exit_code: Some(0),
            completed_at_ms: Some(started_at_ms + 10),
            attempt_count: 1,
            last_failure_signature: None,
            binding: None,
            delivered_binding: None,
            attempt_timestamps_ms: vec![started_at_ms + 10],
            failure_signatures: Vec::new(),
            terminal_reason_kind: None,
        }],
        started_at_ms,
        finished_at_ms: Some(started_at_ms + 20),
        observed_final_workspace: Some(true),
        workspace_changes_after_last_check: Vec::new(),
        repair_attempts: 0,
        reasoning_escalations: Vec::new(),
        terminal_reason: None,
        terminal_reason_kind: None,
        delivered_workspace_binding: None,
    }
}

fn sample_self_review_run(
    started_at_ms: i64,
    diff_line_count: u32,
) -> crate::self_review::SelfReviewRunRecord {
    crate::self_review::SelfReviewRunRecord {
        tool_call_id: Some(format!("self-review-{started_at_ms}")),
        started_at_ms,
        mode: crate::self_review::SelfReviewMode::Auto,
        scope: crate::self_review::SelfReviewScope::Scoped,
        diff_line_count,
        reviewer_duration_ms: 25,
        reviewer_prompt_tokens: 100,
        reviewer_completion_tokens: 20,
        reviewer_cost_micros: Some(5),
        status: crate::self_review::SelfReviewRunStatus::Succeeded,
        result: Some("No findings.".to_string()),
        findings: crate::self_review::SelfReviewFindingCounts::default(),
        disposition: Some(crate::self_review::SelfReviewDisposition::NoneNeeded),
    }
}

#[tokio::test]
async fn clear_rotation_persists_hard_boundary_and_isolates_episode_ledger() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let old_session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "test-model",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let agent = test_agent(Box::new(CompleteProvider));
    let episode_store = crate::episode::SharedEpisodeStore::default();
    let compaction_event = crate::agent::CompactionEvent {
        seq: 1,
        occurred_at_ms: 1_700_000_000_050,
        before_tokens: 90_000,
        after_tokens: 50_000,
        messages_omitted: 12,
        prefix_hash_before: Some("before-prefix".to_string()),
        prefix_hash_after: Some("after-prefix".to_string()),
        ..crate::agent::CompactionEvent::default()
    };
    {
        let mut agent = agent.lock().await;
        agent.set_episode_store(episode_store);
        agent
            .run(
                "old session task",
                tokio_util::sync::CancellationToken::new(),
                Arc::new(NullSink),
            )
            .await
            .unwrap();
        agent.restore_verification_runs(vec![sample_verification_run(
            1_700_000_000_000,
            "cargo test --locked",
        )]);
        agent.restore_self_review_runs(vec![sample_self_review_run(1_700_000_000_100, 4)]);
        agent.restore_compaction_events(vec![compaction_event.clone()]);
        agent.clear_for_session_rotation().await;
    }
    let command = CommandOutcomeEvent::Applied {
        generation: None,
        clear_transcript: true,
        messages: Vec::new(),
        provider: None,
        context_report: None,
        quit: false,
        open_modal: None,
    };
    let active_session_id = Arc::new(Mutex::new(Some(old_session_id)));
    let plan_store = Arc::new(Mutex::new(crate::plan::PlanDoc::default()));
    let mut app = app();
    let mut current_session_id = old_session_id;
    let mut signatures = PersistedSnapshotSignatures::default();
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let tasks = TaskController::new(sender);

    rotate_persisted_session_on_clear(
        &command,
        &tasks,
        &storage,
        &active_session_id,
        temp_dir.path(),
        &plan_store,
        &mut app,
        &mut current_session_id,
        &mut signatures,
        &agent,
    )
    .await;

    assert_ne!(current_session_id, old_session_id);
    let outgoing = storage.load_episodes(old_session_id).await.unwrap();
    assert_eq!(outgoing.len(), 1);
    assert_eq!(
        outgoing[0].close_reason(),
        Some(crate::episode::EpisodeCloseReason::HardBoundary)
    );
    assert_eq!(outgoing[0].status(), crate::episode::EpisodeStatus::Closed);
    let outgoing_snapshot = storage
        .load_session_snapshot(old_session_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outgoing_snapshot.verification_runs.len(), 1);
    assert_eq!(outgoing_snapshot.self_review_runs.len(), 1);
    assert_eq!(outgoing_snapshot.compaction_events, [compaction_event]);
    let resumed_agent = test_agent(Box::new(CompleteProvider));
    {
        let mut resumed = resumed_agent.lock().await;
        crate::session_persist::restore_agent_state(&mut resumed, &outgoing_snapshot).await;
        assert_eq!(
            resumed.compaction_events(),
            outgoing_snapshot.compaction_events
        );
        assert_eq!(
            resumed.context_report().compaction_events,
            outgoing_snapshot.compaction_events
        );
    }
    assert!(
        agent
            .lock()
            .await
            .episodes_snapshot()
            .expect("episodes remain wired")
            .is_empty(),
        "new persisted session must not inherit the outgoing archive"
    );
    assert!(
        storage
            .load_episodes(current_session_id)
            .await
            .unwrap()
            .is_empty()
    );
    persist_changed_snapshots(
        &storage,
        current_session_id,
        &mut app,
        agent.clone(),
        &mut signatures,
    )
    .await;
    persist_changed_snapshots(
        &storage,
        current_session_id,
        &mut app,
        agent.clone(),
        &mut signatures,
    )
    .await;
    let rotated = storage
        .load_session_snapshot(current_session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(rotated.verification_runs.is_empty());
    assert!(rotated.self_review_runs.is_empty());
    assert!(rotated.compaction_events.is_empty());
}

#[tokio::test]
async fn clear_rotation_flushes_dirty_edit_made_since_the_last_periodic_flush() {
    // N2 regression: a dirty transcript edit made after the last periodic
    // flush (signatures still default, so nothing has hit storage yet) must
    // not be stranded when `/clear` rotates to a new session before the next
    // 500ms `PERSISTENCE_FLUSH_INTERVAL` tick fires.
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, old_session_id) = storage_with_active_session(temp_dir.path()).await;
    let agent = test_agent(Box::new(CompleteProvider));
    let command = CommandOutcomeEvent::Applied {
        generation: None,
        clear_transcript: true,
        messages: Vec::new(),
        provider: None,
        context_report: None,
        quit: false,
        open_modal: None,
    };
    let active_session_id = Arc::new(Mutex::new(Some(old_session_id)));
    let plan_store = Arc::new(Mutex::new(crate::plan::PlanDoc::default()));
    let mut app = app();
    app.transcript.push(TranscriptItem::UserMessage {
        text: "unflushed edit before switch".to_string(),
    });
    let mut current_session_id = old_session_id;
    let mut signatures = PersistedSnapshotSignatures::default();
    let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let tasks = TaskController::new(sender);

    rotate_persisted_session_on_clear(
        &command,
        &tasks,
        &storage,
        &active_session_id,
        temp_dir.path(),
        &plan_store,
        &mut app,
        &mut current_session_id,
        &mut signatures,
        &agent,
    )
    .await;

    assert_ne!(current_session_id, old_session_id);
    let outgoing = storage
        .load_session_snapshot(old_session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            outgoing.transcript.as_slice(),
            [TranscriptItem::UserMessage { text }] if text == "unflushed edit before switch"
        ),
        "dirty transcript edit must be flushed to the outgoing session before rotation, \
         got {:?}",
        outgoing.transcript
    );
}

fn section_only_plan(title: &str) -> crate::plan::PlanDoc {
    let mut plan = crate::plan::PlanDoc::default();
    plan.edit().set_title(title);
    plan.edit()
        .set_section("Approach", "Do the work without checklist tasks.");
    plan
}

fn authorized_codex_store() -> SessionStore {
    let mut store = SessionStore::default();
    store.ensure_provider("opencode");
    store.ensure_provider("codex");
    store.session_mut("codex").api_key = "codex-token".to_string();
    store.session_mut("codex").account_id = "codex-account".to_string();
    store
}

fn test_model_catalog() -> Arc<crate::model_catalog::ModelCatalog> {
    let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
    let codex = "codex"
        .parse::<crate::model_catalog::ConnectionId>()
        .unwrap();
    catalog
        .write_live_availability(
            &codex,
            crate::model_catalog::LiveModelAvailability::from_remote_ids(["gpt-5.5".to_string()]),
        )
        .unwrap();
    Arc::new(catalog)
}

fn zero_signatures() -> PersistedSnapshotSignatures {
    PersistedSnapshotSignatures::default()
}

fn empty_repo_map_injector() -> RepoMapInjector {
    let (_sender, receiver) = tokio::sync::watch::channel(Some(String::new()));
    RepoMapInjector::new(receiver)
}

async fn storage_with_active_session(project_root: &std::path::Path) -> (Storage, SessionId) {
    let storage = Storage::open_at(project_root.join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            project_root,
            "codex",
            "test-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    (storage, session_id)
}

fn persistence_deps<'a>(
    storage: &'a Storage,
    project_root: &'a std::path::Path,
    current_session_id: SessionId,
) -> PersistenceCommandDeps<'a> {
    PersistenceCommandDeps {
        storage,
        agent: test_agent(Box::new(CompleteProvider)),
        memory: None,
        session_store: Arc::new(Mutex::new(SessionStore::default())),
        registry: Arc::new(ProviderRegistry::default_registry()),
        model_catalog: test_model_catalog(),
        todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
        plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
        project_root,
        active_session_id: Arc::new(Mutex::new(Some(current_session_id))),
    }
}

#[tokio::test]
async fn fresh_plan_protection_clears_an_empty_canvas_without_saving() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let plan_store: SharedPlanStore = Arc::new(Mutex::new(crate::plan::PlanDoc::default()));
    let mut app = app();

    protect_canvas_before_new_plan(&mut app, &storage, session_id, &plan_store)
        .await
        .unwrap();

    assert!(app.plan.is_empty());
    assert!(plan_store.lock().await.is_empty());
    assert!(app.active_saved_plan_session_id.is_none());
    assert!(
        storage
            .saved_plans_for_project(temp_dir.path(), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn fresh_plan_protection_saves_then_clears_an_unsaved_canvas() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let plan_store: SharedPlanStore = Arc::new(Mutex::new(sample_plan("Protect me")));
    let mut app = app();

    protect_canvas_before_new_plan(&mut app, &storage, session_id, &plan_store)
        .await
        .unwrap();

    assert!(app.plan.is_empty());
    assert!(plan_store.lock().await.is_empty());
    assert!(app.active_saved_plan_session_id.is_none());
    let saved = storage
        .saved_plans_for_project(temp_dir.path(), 10)
        .await
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].title, "Protect me");
}

#[tokio::test]
async fn fresh_plan_protection_refreshes_linked_plan_in_place() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let plan_store: SharedPlanStore = Arc::new(Mutex::new(sample_plan("First title")));
    let mut app = app();
    app.plan = plan_store.lock().await.clone();
    let saved = storage
        .save_plan_to_library(session_id, None, &app.plan, None)
        .await
        .unwrap();
    app.active_saved_plan_session_id = Some(saved.id);
    let saved_id = saved.id;
    plan_store.lock().await.edit().set_title("Refreshed title");

    protect_canvas_before_new_plan(&mut app, &storage, session_id, &plan_store)
        .await
        .unwrap();

    assert!(app.plan.is_empty());
    assert!(app.active_saved_plan_session_id.is_none());
    let saved = storage.load_saved_plan(saved_id).await.unwrap().unwrap();
    assert_eq!(saved.plan.title, "Refreshed title");
    assert_eq!(
        storage
            .saved_plans_for_project(temp_dir.path(), 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn fresh_plan_protection_keeps_an_invalid_canvas_intact() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut invalid = crate::plan::PlanDoc::default();
    invalid.edit().add_task("Untitled task");
    let plan_store: SharedPlanStore = Arc::new(Mutex::new(invalid.clone()));
    let mut app = app();

    let err = protect_canvas_before_new_plan(&mut app, &storage, session_id, &plan_store)
        .await
        .unwrap_err();

    assert!(err.to_string().contains("untitled"));
    assert_eq!(*plan_store.lock().await, invalid);
    assert_eq!(app.plan, invalid);
    assert!(app.active_saved_plan_session_id.is_none());
}

#[tokio::test]
async fn start_new_plan_save_failure_keeps_coding_and_skips_continuation() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut invalid = crate::plan::PlanDoc::default();
    invalid.edit().add_task("Untitled task");
    let plan_store: SharedPlanStore = Arc::new(Mutex::new(invalid.clone()));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let agent = test_agent(Box::new(CompleteProvider));
    let mut app = app();
    app.pending_start_new_plan = true;
    let mut repo_map = empty_repo_map_injector();

    let started = maybe_start_new_plan(
        &mut app,
        &mut tasks,
        agent,
        Arc::new(NullSink),
        &mut repo_map,
        &storage,
        session_id,
        plan_store.clone(),
    )
    .await;

    assert!(!started);
    assert_eq!(app.view, View::Agent);
    assert_eq!(app.active_mode(), AgentMode::Coding);
    assert_eq!(app.task_state, TaskState::Idle);
    assert!(!tasks.is_busy());
    assert!(!app.pending_start_new_plan);
    assert_eq!(app.plan, invalid);
    assert_eq!(*plan_store.lock().await, invalid);
}

fn runtime_action_deps<'a>(
    storage: &'a Storage,
    project_root: &'a std::path::Path,
    current_session_id: SessionId,
    session_store: Arc<Mutex<SessionStore>>,
    runtime_sender: mpsc::UnboundedSender<RuntimeEvent>,
) -> RuntimeActionDeps<'a> {
    let (interaction, _interaction_rx) = InteractionService::new();
    RuntimeActionDeps {
        interaction: Arc::new(interaction),
        runtime_sender,
        agent: test_agent(Box::new(CompleteProvider)),
        memory: None,
        yolo_mode: crate::yolo::YoloMode::new(),
        session_store,
        permissions: crate::permissions::PermissionManager::memory_only(),
        domain_permissions: crate::permissions::PermissionManager::memory_only_domains(),
        registry: Arc::new(ProviderRegistry::default_registry()),
        model_catalog: test_model_catalog(),
        storage,
        project_root,
        session_project_root: project_root,
        active_session_id: Arc::new(Mutex::new(Some(current_session_id))),
        todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
        plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
        sink: Arc::new(NullSink),
        background_tasks: Arc::new(BackgroundTaskRegistry::new()),
        terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
        peer_bus: None,
    }
}

fn runtime_action_deps_with_permissions<'a>(
    storage: &'a Storage,
    project_root: &'a std::path::Path,
    current_session_id: SessionId,
    session_store: Arc<Mutex<SessionStore>>,
    runtime_sender: mpsc::UnboundedSender<RuntimeEvent>,
    permissions: crate::permissions::PermissionManager,
    domain_permissions: crate::permissions::PermissionManager,
) -> RuntimeActionDeps<'a> {
    let (interaction, _interaction_rx) = InteractionService::new();
    RuntimeActionDeps {
        interaction: Arc::new(interaction),
        runtime_sender,
        agent: test_agent(Box::new(CompleteProvider)),
        memory: None,
        yolo_mode: crate::yolo::YoloMode::new(),
        session_store,
        permissions,
        domain_permissions,
        registry: Arc::new(ProviderRegistry::default_registry()),
        model_catalog: test_model_catalog(),
        storage,
        project_root,
        session_project_root: project_root,
        active_session_id: Arc::new(Mutex::new(Some(current_session_id))),
        todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
        plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
        sink: Arc::new(NullSink),
        background_tasks: Arc::new(BackgroundTaskRegistry::new()),
        terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
        peer_bus: None,
    }
}

#[tokio::test]
async fn permissions_manager_delete_removes_session_rule_and_rebuilds() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let permissions = crate::permissions::PermissionManager::memory_only();
    let domain_permissions = crate::permissions::PermissionManager::memory_only_domains();
    permissions.add_session_rule("make *", crate::permissions::Permission::Allow);
    permissions.add_session_rule("rm -rf *", crate::permissions::Permission::Deny);

    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current_session_id = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    crate::tui::runtime_actions::open_permissions_manager(
        &mut app,
        &permissions,
        &domain_permissions,
        String::new(),
        0,
    );

    let removed_pattern = match &app.modal {
        Some(ModalKind::Manager(crate::tui::event::ManagerModal::PermissionsManager {
            rows,
            ..
        })) => {
            assert_eq!(rows.len(), 2, "both session rules should be listed");
            rows[0].pattern.clone()
        }
        other => panic!("expected permissions manager, got {other:?}"),
    };

    let result = handle_runtime_action(
        AppAction::PermissionsManager(crate::tui::event::PermissionsManagerAction::Delete),
        &mut app,
        &mut tasks,
        runtime_action_deps_with_permissions(
            &storage,
            temp_dir.path(),
            session_id,
            session_store,
            runtime_tx,
            permissions.clone(),
            domain_permissions.clone(),
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    // The manager rebuilt in place with the deleted rule gone.
    match &app.modal {
        Some(ModalKind::Manager(crate::tui::event::ManagerModal::PermissionsManager {
            rows,
            ..
        })) => {
            assert_eq!(rows.len(), 1);
            assert!(rows.iter().all(|row| row.pattern != removed_pattern));
        }
        other => panic!("expected permissions manager, got {other:?}"),
    }
    // And the live manager no longer carries the session rule.
    assert!(
        permissions
            .user_rules()
            .iter()
            .all(|rule| rule.pattern != removed_pattern)
    );
}

fn context_report() -> crate::agent::ContextReport {
    crate::agent::ContextReport {
        budget_tokens: 120_000,
        entries: vec![crate::agent::ContextEntry {
            role: crate::agent::ContextRole::User,
            tokens: 12,
            text: "active context".to_string(),
        }],
        last_prompt_tokens: Some(12),
        last_completion_tokens: Some(3),
        session_prompt_tokens: 12,
        session_completion_tokens: 3,
        ..Default::default()
    }
}

fn context_report_with_ledger_rows(row_count: usize) -> crate::agent::ContextReport {
    let mut report = context_report();
    report.ledger = (0..row_count)
        .map(|index| crate::agent::ContextNode {
            id: format!("row-{index}").into(),
            kind: crate::agent::ContextNodeKind::ChatMessage,
            inclusion: crate::agent::ContextInclusion::Included,
            role: Some(crate::agent::ContextRole::User),
            label: format!("Message {index}"),
            tokens: 10,
            chars: 20,
            bytes: 20,
            source: crate::provider::TokenCounterKind::Heuristic,
            confidence: crate::provider::EstimateConfidence::Low,
            preview: String::new(),
            sources: Vec::new(),
            children: Vec::new(),
        })
        .collect();
    report
}

fn session_summary(id: i64) -> crate::storage::SessionSummary {
    crate::storage::SessionSummary {
        id: crate::storage::SessionId::from_raw(id),
        project_path: "/Users/wojtek/code/bonsai".to_string(),
        name: format!("Session {id}"),
        summary: "Work in progress".to_string(),
        provider_id: "minimax-coding-plan".to_string(),
        model: "MiniMax-M3".to_string(),
        reasoning: ReasoningSelection::default(),
        status: crate::storage::SessionStatus::Active,
        terminal_reason: None,
        latest_task: None,
        updated_at_ms: 1_000,
        message_count: 0,
        prompt_token_count: 0,
        completion_token_count: 0,
        cache_read_input_token_count: 0,
        cache_creation_input_token_count: 0,
        cache_measured_input_token_count: 0,
        cost_micros: 0,
        no_cache_cost_micros: 0,
        source_plan_id: None,
    }
}

#[test]
fn interrupted_session_notice_is_short() {
    let one = format_interrupted_sessions(&[session_summary(54)]);

    // Single line: this renders in a composer meta row / header pill, so a
    // newline would split the banner.
    assert_eq!(
        one,
        "Interrupted session found. Use /resume to bring it back."
    );
    assert!(!one.contains('\n'));
    assert!(!one.contains("minimax-coding-plan"));
    assert!(!one.contains("/forget"));

    let many = format_interrupted_sessions(&[session_summary(54), session_summary(55)]);
    assert_eq!(
        many,
        "Interrupted sessions found. Use /resume to bring back the latest one."
    );
    assert!(!many.contains('\n'));
}

#[test]
fn ctrl_c_idle_first_press_arms_and_second_press_exits() {
    let now = Instant::now();
    assert_eq!(
        next_ctrl_c_action(TaskState::Idle, false, None, now),
        CtrlCAction::ArmExit
    );
    let prompt = CtrlCExitPrompt {
        kind: CtrlCExitPromptKind::Exit,
        expires_at: now + CTRL_C_EXIT_WINDOW,
    };

    assert_eq!(
        next_ctrl_c_action(
            TaskState::Idle,
            false,
            Some(prompt),
            now + Duration::from_secs(1)
        ),
        CtrlCAction::ConfirmExit
    );
}

#[test]
fn termination_signal_marks_the_session_interrupted_and_resumable() {
    let mut app = app();
    app.current_session_status = crate::storage::SessionStatus::Active;
    app.current_terminal_reason =
        Some(crate::run_budget::RunBudgetExhaustion::MaxTurns { limit: 10 });

    prepare_termination_shutdown(&mut app);

    assert_eq!(
        app.current_session_status,
        crate::storage::SessionStatus::Interrupted
    );
    assert_eq!(app.current_terminal_reason, None);
    assert_eq!(app.task_state, TaskState::Exiting);
}

#[test]
fn ctrl_c_timeout_clears_armed_prompt() {
    let now = Instant::now();
    let mut app = app();
    let mut prompt = Some(CtrlCExitPrompt {
        kind: CtrlCExitPromptKind::Exit,
        expires_at: now,
    });
    app.reduce(AppAction::SetShutdownNotice(Some(
        CtrlCExitPromptKind::Exit.notice().to_string(),
    )));

    clear_expired_ctrl_c_prompt(&mut app, &mut prompt, now + Duration::from_millis(1));

    assert!(prompt.is_none());
    assert!(app.shutdown_notice.is_none());
    assert_eq!(
        next_ctrl_c_action(TaskState::Idle, false, prompt, now + Duration::from_secs(1)),
        CtrlCAction::ArmExit
    );
}

#[test]
fn ctrl_c_running_first_press_cancels_and_second_press_exits() {
    let now = Instant::now();
    assert_eq!(
        next_ctrl_c_action(TaskState::Running, false, None, now),
        CtrlCAction::CancelRunAndArm
    );
    let prompt = CtrlCExitPrompt {
        kind: CtrlCExitPromptKind::Cancelling,
        expires_at: now + CTRL_C_EXIT_WINDOW,
    };

    assert_eq!(
        next_ctrl_c_action(
            TaskState::Cancelling,
            false,
            Some(prompt),
            now + Duration::from_secs(1)
        ),
        CtrlCAction::ConfirmExit
    );
}

#[test]
fn ctrl_c_command_aborts_and_arms() {
    let now = Instant::now();
    assert_eq!(
        next_ctrl_c_action(TaskState::Command, false, None, now),
        CtrlCAction::CancelCommandAndArm
    );
}

#[test]
fn ctrl_c_cancelling_arms_exit() {
    let now = Instant::now();
    assert_eq!(
        next_ctrl_c_action(TaskState::Cancelling, false, None, now),
        CtrlCAction::ArmExit
    );
}

#[test]
fn ctrl_c_cancelling_after_escape_stops_remaining_subagents() {
    let now = Instant::now();
    assert_eq!(
        next_ctrl_c_action(TaskState::Cancelling, true, None, now),
        CtrlCAction::CancelSubagentsAndArm
    );
}

#[test]
fn ctrl_c_idle_with_running_subagents_cancels_them() {
    let now = Instant::now();
    assert_eq!(
        next_ctrl_c_action(TaskState::Idle, true, None, now),
        CtrlCAction::CancelSubagentsAndArm
    );
}

#[test]
fn command_cancellation_filters_only_the_stale_command_event() {
    let mut app = app();
    let stale_generation = app.command_generation();
    app.invalidate_pending_commands();
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    runtime_tx
        .send(RuntimeEvent::CommandFinished(Box::new(
            CommandOutcomeEvent::Applied {
                generation: Some(stale_generation),
                clear_transcript: false,
                messages: Vec::new(),
                provider: None,
                context_report: None,
                quit: true,
                open_modal: None,
            },
        )))
        .expect("send should succeed");
    runtime_tx
        .send(RuntimeEvent::PeerInboxChanged(2))
        .expect("send should succeed");
    runtime_tx
        .send(RuntimeEvent::BackgroundTaskRemovalFinished {
            task_id: "bg-1".to_string(),
            error: None,
        })
        .expect("send should succeed");

    let retained = std::iter::from_fn(|| runtime_rx.try_recv().ok())
        .filter(|event| !runtime_event_is_stale_command(&app, event))
        .collect::<Vec<_>>();

    assert_eq!(retained.len(), 2);
    assert!(matches!(retained[0], RuntimeEvent::PeerInboxChanged(2)));
    assert!(matches!(
        retained[1],
        RuntimeEvent::BackgroundTaskRemovalFinished { .. }
    ));
}

#[tokio::test]
async fn ui_peer_delivery_persists_transcript_and_ack_in_one_boundary() {
    let project = tempfile::TempDir::new().unwrap();
    let (storage, recipient) = storage_with_active_session(project.path()).await;
    let sender = storage
        .start_session(
            project.path(),
            "codex",
            "gpt-5.5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .send_peer_message(
            project.path(),
            sender,
            recipient,
            crate::storage::PeerMessageKind::Text,
            "durable UI handoff",
            0,
        )
        .await
        .unwrap();
    let mut deliveries = storage
        .claim_ui_undelivered_messages(recipient)
        .await
        .unwrap();
    let delivery = deliveries.pop().unwrap();
    let message_id = delivery.message.id;

    let mut app = app();
    app.reduce(AppAction::PeerMessage {
        source_message_id: Some(message_id),
        delivery_receipt: Some(delivery.receipt),
        session_id: sender.as_i64(),
        outgoing: false,
        text: delivery.message.body,
    });
    let agent = test_agent(Box::new(CompleteProvider));
    let mut signatures = PersistedSnapshotSignatures::default();
    persist_changed_snapshots(&storage, recipient, &mut app, agent, &mut signatures).await;

    let snapshot = storage
        .load_session_snapshot(recipient)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        snapshot.transcript.as_slice(),
        [TranscriptItem::PeerMessage {
            source_message_id: Some(id),
            text,
            ..
        }] if *id == message_id && text == "durable UI handoff"
    ));
    assert!(
        storage
            .claim_ui_undelivered_messages(recipient)
            .await
            .unwrap()
            .is_empty(),
        "the transcript commit must acknowledge the matching UI lease"
    );

    // A replayed runtime event with the same durable id cannot duplicate the
    // transcript entry even if it arrives before receipt cleanup.
    app.reduce(AppAction::PeerMessage {
        source_message_id: Some(message_id),
        delivery_receipt: None,
        session_id: sender.as_i64(),
        outgoing: false,
        text: "durable UI handoff".to_string(),
    });
    assert_eq!(app.transcript.len(), 1);
}

#[tokio::test]
async fn periodic_snapshot_flush_runs_in_background_and_applies_signatures() {
    let project = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(project.path()).await;
    let mut app = app();
    app.transcript.push(TranscriptItem::UserMessage {
        text: "background persistence".to_string(),
    });
    let agent = test_agent(Box::new(CompleteProvider));
    let mut signatures = PersistedSnapshotSignatures::default();

    let handle = spawn_changed_snapshot_flush(&storage, session_id, &app, &agent, signatures);
    let flush = handle.await.unwrap();
    apply_persistence_flush_result(flush, session_id, &mut app, &agent, &mut signatures);

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        snapshot.transcript.as_slice(),
        [TranscriptItem::UserMessage { text }] if text == "background persistence"
    ));
    assert_ne!(signatures.transcript, 0);
}

#[tokio::test]
async fn failed_snapshot_flush_surfaces_actionable_recovery_guidance() {
    let project = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(project.path()).await;
    let mut app = app();
    let agent = test_agent(Box::new(CompleteProvider));
    let mut signatures = PersistedSnapshotSignatures::default();
    let flush = failed_persistence_flush_for_tests(
        &storage,
        session_id,
        anyhow::anyhow!("database or disk is full"),
    );

    apply_persistence_flush_result(flush, session_id, &mut app, &agent, &mut signatures);

    assert_eq!(
        app.session_toast.as_ref().map(|toast| toast.text.as_str()),
        Some("Session changes could not be saved. Free disk space, then run /doctor.")
    );
    assert_eq!(signatures, PersistedSnapshotSignatures::default());
}

#[tokio::test]
async fn superseded_background_flush_cannot_overwrite_newer_snapshot() {
    let project = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(project.path()).await;
    let mut app = app();
    app.transcript.push(TranscriptItem::UserMessage {
        text: "older snapshot".to_string(),
    });
    let agent = test_agent(Box::new(CompleteProvider));
    let mut signatures = PersistedSnapshotSignatures::default();
    let older = spawn_changed_snapshot_flush(&storage, session_id, &app, &agent, signatures);

    app.transcript.push(TranscriptItem::UserMessage {
        text: "newer snapshot".to_string(),
    });
    persist_changed_snapshots(
        &storage,
        session_id,
        &mut app,
        agent.clone(),
        &mut signatures,
    )
    .await;
    let current_signature = signatures.transcript;

    let older = older.await.unwrap();
    apply_persistence_flush_result(older, session_id, &mut app, &agent, &mut signatures);

    let snapshot = storage
        .load_session_snapshot(session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        snapshot.transcript.as_slice(),
        [
            TranscriptItem::UserMessage { text: older },
            TranscriptItem::UserMessage { text: newer },
        ] if older == "older snapshot" && newer == "newer snapshot"
    ));
    assert_eq!(
        signatures.transcript, current_signature,
        "IMPORTANT snapshot invariant: a superseded task must not roll signatures back"
    );
}

#[test]
fn terminal_input_health_tolerates_normal_empty_polls() {
    let mut health = TerminalInputHealth::default();

    // Healthy idle polls: full-length waits that slept (cpu ≈ 0).
    for _ in 0..100 {
        assert!(!health.record_poll(false, EVENT_POLL_TIMEOUT, Duration::ZERO));
    }
}

#[test]
fn terminal_input_health_detects_repeated_fast_empty_polls() {
    let mut health = TerminalInputHealth::default();

    for _ in 0..TERMINAL_FAST_EMPTY_POLL_LIMIT.saturating_sub(1) {
        assert!(!health.record_poll(false, Duration::ZERO, Duration::ZERO));
    }

    assert!(health.record_poll(false, Duration::ZERO, Duration::ZERO));
}

#[test]
fn terminal_input_health_resets_after_real_event() {
    let mut health = TerminalInputHealth::default();

    for _ in 0..TERMINAL_FAST_EMPTY_POLL_LIMIT.saturating_sub(1) {
        assert!(!health.record_poll(false, Duration::ZERO, Duration::ZERO));
    }
    assert!(!health.record_poll(true, Duration::ZERO, Duration::ZERO));

    for _ in 0..TERMINAL_FAST_EMPTY_POLL_LIMIT.saturating_sub(1) {
        assert!(!health.record_poll(false, Duration::ZERO, Duration::ZERO));
    }
}

#[test]
fn terminal_input_health_detects_busy_spin_polls() {
    let mut health = TerminalInputHealth::default();

    // The dead-tty EOF spin: full-length empty polls whose CPU time tracks
    // wall time (crossterm hot-loops on a permanently-readable fd).
    for _ in 0..TERMINAL_BUSY_EMPTY_POLL_LIMIT.saturating_sub(1) {
        assert!(!health.record_poll(false, EVENT_POLL_TIMEOUT, EVENT_POLL_TIMEOUT));
    }

    assert!(health.record_poll(false, EVENT_POLL_TIMEOUT, EVENT_POLL_TIMEOUT));
}

#[test]
fn terminal_input_health_busy_spin_needs_dominant_cpu_share() {
    let mut health = TerminalInputHealth::default();

    // Half the wait on-CPU (scheduler noise, heavy frame) is below the 75%
    // spin threshold and must never accumulate toward an exit.
    for _ in 0..100 {
        assert!(!health.record_poll(false, EVENT_POLL_TIMEOUT, EVENT_POLL_TIMEOUT / 2));
    }
}

#[test]
fn terminal_input_health_busy_spin_resets_after_real_event() {
    let mut health = TerminalInputHealth::default();

    for _ in 0..TERMINAL_BUSY_EMPTY_POLL_LIMIT.saturating_sub(1) {
        assert!(!health.record_poll(false, EVENT_POLL_TIMEOUT, EVENT_POLL_TIMEOUT));
    }
    assert!(!health.record_poll(true, EVENT_POLL_TIMEOUT, EVENT_POLL_TIMEOUT));

    for _ in 0..TERMINAL_BUSY_EMPTY_POLL_LIMIT.saturating_sub(1) {
        assert!(!health.record_poll(false, EVENT_POLL_TIMEOUT, EVENT_POLL_TIMEOUT));
    }
}

#[test]
fn resize_event_requests_immediate_redraw() {
    assert!(terminal_event_requests_immediate_redraw(&Event::Resize(
        120, 40
    )));
    assert!(!terminal_event_requests_immediate_redraw(
        &Event::FocusGained
    ));
}

#[test]
fn pointer_motion_is_dropped_before_it_can_request_redraw() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    let mouse = |kind| {
        Event::Mouse(MouseEvent {
            kind,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })
    };

    // Bare motion floods events under all-motion capture and renders nothing;
    // it must not raise the refresh rate above the constant redraw cadence.
    assert!(terminal_event_is_pointer_motion(&mouse(
        MouseEventKind::Moved
    )));

    // State-changing pointer events keep their immediate redraw.
    assert!(!terminal_event_is_pointer_motion(&mouse(
        MouseEventKind::Down(MouseButton::Left)
    )));
    assert!(!terminal_event_is_pointer_motion(&mouse(
        MouseEventKind::Drag(MouseButton::Left)
    )));
    assert!(!terminal_event_is_pointer_motion(&mouse(
        MouseEventKind::ScrollDown
    )));
    assert!(!terminal_event_is_pointer_motion(&Event::Resize(120, 40)));
}

#[test]
fn redraw_interval_slows_down_idle_frames() {
    // "Active" is now any task in flight — the main agent, a background task, a
    // detached subagent, or a terminal (see `any_task_active`). At rest the
    // cadence drops to the 1s idle interval; with any work running it stays at
    // the 10 FPS active interval so on-screen spinners/timers animate smoothly.
    assert_eq!(redraw_interval(false), IDLE_REDRAW_INTERVAL);
    assert_eq!(redraw_interval(true), ACTIVE_REDRAW_INTERVAL);
}

#[test]
fn should_draw_frame_honors_requests_and_deadline() {
    let now = Instant::now();
    let later = now + Duration::from_secs(1);

    assert!(should_draw_frame(true, now, later));
    assert!(!should_draw_frame(false, now, later));
    assert!(should_draw_frame(false, later, later));
}

#[test]
fn hidden_subagent_activity_does_not_bypass_redraw_throttle() {
    assert!(!subagent_event_requests_redraw(
        crate::subagent::SubagentEvent::Activity,
        false,
    ));
    assert!(subagent_event_requests_redraw(
        crate::subagent::SubagentEvent::Activity,
        true,
    ));
    assert!(subagent_event_requests_redraw(
        crate::subagent::SubagentEvent::Finished,
        false,
    ));
}

#[test]
fn normal_key_clears_ctrl_c_prompt() {
    let now = Instant::now();
    let mut app = app();
    let mut prompt = None;
    set_ctrl_c_prompt(&mut app, &mut prompt, CtrlCExitPromptKind::Exit, now);

    assert!(clears_ctrl_c_prompt(&KeyIntent::Insert('x')));
    clear_ctrl_c_prompt(&mut app, &mut prompt);

    assert!(prompt.is_none());
    assert!(app.shutdown_notice.is_none());
    assert!(!clears_ctrl_c_prompt(&KeyIntent::CancelOrQuit));
}

async fn running_tasks() -> (TaskController, mpsc::UnboundedReceiver<RuntimeEvent>) {
    let (runtime_tx, runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    tasks
        .start_agent_run(
            test_agent(Box::new(BlockingProvider)),
            crate::agent::UserInput::from_text("initial"),
            Arc::new(NullSink),
            crate::agent::ActivePersona::Builtin(AgentMode::Coding),
        )
        .expect("agent run should start");
    tokio::task::yield_now().await;
    (tasks, runtime_rx)
}

#[tokio::test]
async fn running_enter_queues_normal_text_for_next_turn() {
    let (mut tasks, _runtime_rx) = running_tasks().await;
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("follow up".to_string());
    assert!(enqueue_running_follow_up(
        &mut app,
        &tasks,
        FollowUpDelivery::Queue,
        &crate::provider::ProviderRegistry::default_registry(),
        None,
    ));

    assert_eq!(app.input(), "");
    assert_eq!(app.composer.text, "");
    assert!(matches!(
        app.queued_inputs.as_slice(),
        [QueuedInput {
            id: 1,
            text,
            mode: AgentMode::Coding,
            delivery: FollowUpDelivery::Queue,
            ..
        }] if text == "follow up"
    ));
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::QueuedUserMessage { text, .. }) if text == "follow up"
    ));
    tasks.abort();
}

#[tokio::test]
async fn running_escape_interrupts_and_stages_immediate_steer() {
    let (mut tasks, mut runtime_rx) = running_tasks().await;
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("do this now".to_string());

    assert!(steer_active_run(
        &mut app,
        &tasks,
        &crate::provider::ProviderRegistry::default_registry(),
        &test_model_catalog(),
    ));

    assert_eq!(app.task_state, TaskState::Cancelling);
    assert!(matches!(
        app.queued_inputs.as_slice(),
        [QueuedInput {
            text,
            delivery: FollowUpDelivery::Steer,
            ..
        }] if text == "do this now"
    ));
    let finished = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(RuntimeEvent::AgentFinished(result)) = runtime_rx.recv().await {
                break result;
            }
        }
    })
    .await
    .expect("Esc should promptly stop the foreground run")
    .expect("foreground interruption should be a normal run outcome");
    assert!(matches!(
        finished,
        crate::tui::event::AgentRunOutcome::Interrupted
    ));
    assert!(tasks.poll_finished().await.is_some());
}

#[tokio::test]
async fn escape_steer_reduces_old_completion_before_starting_replacement() {
    let (mut tasks, mut runtime_rx) = running_tasks().await;
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("do this now".to_string());

    assert!(steer_active_run(
        &mut app,
        &tasks,
        &crate::provider::ProviderRegistry::default_registry(),
        &test_model_catalog(),
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if reap_finished_task(&mut app, &mut tasks).await {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("interrupted run should be reaped");

    assert!(!tasks.is_busy());
    assert!(matches!(
        app.queued_inputs.as_slice(),
        [QueuedInput {
            delivery: FollowUpDelivery::Steer,
            ..
        }]
    ));

    let old_result = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Some(RuntimeEvent::AgentFinished(result)) = runtime_rx.recv().await {
                break result;
            }
        }
    })
    .await
    .expect("old completion event should be queued");
    app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(old_result)));

    let mut repo_map = empty_repo_map_injector();
    assert!(
        start_pending_queued_run_if_idle(
            &mut app,
            &mut tasks,
            test_agent(Box::new(BlockingProvider)),
            Arc::new(NullSink),
            &mut repo_map,
            &ProviderRegistry::default_registry(),
            &test_model_catalog(),
        )
        .await
    );

    assert_eq!(app.task_state, TaskState::Running);
    assert!(tasks.is_busy());
    assert_ne!(
        crate::tui::view::status_dot_spans(&app)[0].content.as_ref(),
        "●",
        "the replacement run must keep the active spinner"
    );
    tasks.abort();
}

#[tokio::test]
async fn running_follow_up_does_not_accept_slash_commands() {
    let (mut tasks, _runtime_rx) = running_tasks().await;
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("/clear".to_string());
    assert!(!enqueue_running_follow_up(
        &mut app,
        &tasks,
        FollowUpDelivery::Steer,
        &crate::provider::ProviderRegistry::default_registry(),
        None,
    ));

    assert_eq!(app.input(), "/clear");
    assert_eq!(app.composer.text, "/clear");
    assert!(app.queued_inputs.is_empty());
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Error,
            ..
        })
    ));
    tasks.abort();
}

#[tokio::test]
async fn running_ctx_command_opens_modal_without_user_message() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.latest_context_report = Some(context_report());
    app.composer.set_text("/ctx".to_string());
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/ctx",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(app.input().is_empty());
    assert!(app.transcript.is_empty());
    assert!(matches!(
        app.modal,
        Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
            _
        )))
    ));
    assert_eq!(app.composer.history, vec!["/ctx".to_string()]);
}

#[tokio::test]
async fn running_model_command_opens_picker_without_user_message() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("/model".to_string());
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/model",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(app.input().is_empty());
    assert!(app.queued_inputs.is_empty());
    assert!(
        !app.transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::UserMessage { text } if text == "/model"))
    );
    assert!(matches!(
        app.modal,
        Some(ModalKind::Picker(
            crate::tui::event::PickerModal::ModelPicker { .. }
        ))
    ));
    assert!(!app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text,
        } if text.contains("queue it for after the current run")
    )));
}

#[tokio::test]
async fn running_model_selection_command_opens_queue_modal() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer
        .set_text("/model codex:openai/gpt-5.5".to_string());
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/model codex:openai/gpt-5.5",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(app.queued_inputs.is_empty());
    assert!(app.deferred_commands.is_empty());
    assert!(matches!(
        app.modal,
        Some(ModalKind::Detail(crate::tui::event::DetailModal::BusyCommand { ref input, ref rows, cursor: 0 }))
            if input == "/model codex:openai/gpt-5.5"
                && rows.first().is_some_and(|row| row.label == "Queue for next run")
    ));
}

#[tokio::test]
async fn running_perf_and_cost_open_cached_usage_modal() {
    for command in ["/perf", "/cost"] {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
            .await
            .unwrap();
        let current_id = storage
            .start_session(
                temp_dir.path(),
                "codex",
                "gpt-5",
                crate::provider::ReasoningSelection::default(),
            )
            .await
            .unwrap();
        let mut current_session_id = current_id;
        let mut signatures = zero_signatures();
        let mut state = PersistenceCommandState {
            current_session_id: &mut current_session_id,
            signatures: &mut signatures,
        };
        let mut app = app();
        app.task_state = TaskState::Running;
        app.latest_context_report = Some(context_report());
        let yolo_mode = crate::yolo::YoloMode::new();

        let handled = handle_running_slash_command(
            command,
            &mut app,
            PersistenceCommandDeps {
                storage: &storage,
                agent: test_agent(Box::new(BlockingProvider)),
                memory: None,
                session_store: Arc::new(Mutex::new(authorized_codex_store())),
                registry: Arc::new(ProviderRegistry::default_registry()),
                model_catalog: test_model_catalog(),
                todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
                plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
                project_root: temp_dir.path(),
                active_session_id: Arc::new(Mutex::new(Some(current_id))),
            },
            &yolo_mode,
            &mut state,
        )
        .await;

        assert!(handled);
        assert!(
            !app.transcript
                .iter()
                .any(|item| matches!(item, TranscriptItem::CommandOutput { .. }))
        );
        assert!(matches!(
            app.modal,
            Some(ModalKind::Detail(crate::tui::event::DetailModal::PerfReport {
                ref title,
                ref lines,
            })) if title == "Usage"
                && lines.iter().any(|line| line.contains("Usage: session"))
                && lines.iter().any(|line| line.contains("session "))
        ));
    }
}

#[tokio::test]
async fn running_memory_command_opens_manager() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let home_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let memory = Arc::new(crate::memory::MemoryService::load(
        home_dir.path(),
        temp_dir.path(),
        storage.clone(),
        0,
    ));
    memory
        .store()
        .write(
            crate::memory::entry::MemoryTier::User,
            crate::memory::entry::MemoryEntryType::Preference,
            None,
            "tests run with quiet",
            "Use --quiet.",
            None,
        )
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("/memory".to_string());
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/memory",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(BlockingProvider)),
            memory: Some(memory),
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    // The manager opens mid-run instead of falling to the busy-command modal.
    assert!(matches!(
        app.modal,
        Some(ModalKind::Manager(crate::tui::event::ManagerModal::MemoryManager { ref rows, cursor: 0 }))
            if rows.len() == 1 && rows[0].name == "tests-run-with-quiet"
    ));
}

#[tokio::test]
async fn running_memory_command_without_service_reports_status() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("/memory".to_string());
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/memory",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(BlockingProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(app.modal.is_none());
    assert!(app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text,
        } if text.contains("Memory is unavailable")
    )));
}

/// Runtime-action deps with a live memory service, for the manager/wizard
/// mutation paths that intentionally bypass the agent lock.
fn runtime_action_deps_with_memory<'a>(
    storage: &'a Storage,
    project_root: &'a std::path::Path,
    current_session_id: SessionId,
    session_store: Arc<Mutex<SessionStore>>,
    runtime_sender: mpsc::UnboundedSender<RuntimeEvent>,
    memory: Arc<crate::memory::MemoryService>,
) -> RuntimeActionDeps<'a> {
    let mut deps = runtime_action_deps(
        storage,
        project_root,
        current_session_id,
        session_store,
        runtime_sender,
    );
    deps.memory = Some(memory);
    deps
}

fn open_memory_manager_modal(app: &mut AppState, memory: &crate::memory::MemoryService) {
    app.modal = Some(ModalKind::Manager(
        crate::tui::event::ManagerModal::MemoryManager {
            rows: memory.store().entries(),
            cursor: 0,
        },
    ));
}

#[tokio::test]
async fn memory_manager_edit_updates_entry_in_place() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let home_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let memory = Arc::new(crate::memory::MemoryService::load(
        home_dir.path(),
        temp_dir.path(),
        storage.clone(),
        0,
    ));
    memory
        .store()
        .write(
            crate::memory::entry::MemoryTier::Project,
            crate::memory::entry::MemoryEntryType::Project,
            None,
            "tests run with quiet",
            "Use --quiet.",
            None,
        )
        .unwrap();
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current_session_id = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    open_memory_manager_modal(&mut app, &memory);

    let result = handle_runtime_action(
        AppAction::MemoryManagerEdit,
        &mut app,
        &mut tasks,
        runtime_action_deps_with_memory(
            &storage,
            temp_dir.path(),
            session_id,
            session_store.clone(),
            runtime_tx.clone(),
            memory.clone(),
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    {
        let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard { state })) =
            app.modal.as_mut()
        else {
            panic!("edit should open the wizard, got {:?}", app.modal);
        };
        assert_eq!(
            state.editing,
            Some((
                crate::memory::entry::MemoryTier::Project,
                "tests-run-with-quiet".to_string()
            ))
        );
        assert_eq!(state.description, "tests run with quiet");
        // Stored bodies always end with one newline (normalize_body).
        assert_eq!(state.body.text, "Use --quiet.\n");
        // Simulate the user editing and reaching the review step.
        state.description = "tests always run quietly".to_string();
        state.body.set_text("Always pass --quiet.".to_string());
        state.step = crate::tui::memory_manager::MemoryWizardStep::Review;
    }

    let result = handle_runtime_action(
        AppAction::MemoryAddWizard(crate::tui::event::MemoryAddWizardAction::Submit),
        &mut app,
        &mut tasks,
        runtime_action_deps_with_memory(
            &storage,
            temp_dir.path(),
            session_id,
            session_store,
            runtime_tx,
            memory.clone(),
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    // Back on the manager with the same (unrenamed) entry reselected.
    assert!(matches!(
        app.modal,
        Some(ModalKind::Manager(crate::tui::event::ManagerModal::MemoryManager { ref rows, cursor: 0 })) if rows.len() == 1
    ));
    let entry = memory
        .store()
        .get_exact(
            crate::memory::entry::MemoryTier::Project,
            "tests-run-with-quiet",
        )
        .expect("edited entry keeps its identity");
    assert_eq!(entry.description, "tests always run quietly");
    assert_eq!(entry.body, "Always pass --quiet.\n");
    assert!(entry.path.exists());
}

#[tokio::test]
async fn memory_manager_edit_moves_entry_across_tiers() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let home_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let memory = Arc::new(crate::memory::MemoryService::load(
        home_dir.path(),
        temp_dir.path(),
        storage.clone(),
        0,
    ));
    let written = memory
        .store()
        .write(
            crate::memory::entry::MemoryTier::Project,
            crate::memory::entry::MemoryEntryType::Project,
            None,
            "tests run with quiet",
            "Use --quiet.",
            None,
        )
        .unwrap();
    let old_path = written.entry.path.clone();
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current_session_id = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    let mut wizard = crate::tui::memory_manager::MemoryAddWizardState::for_edit(&written.entry);
    wizard.tier = crate::memory::entry::MemoryTier::User;
    wizard.step = crate::tui::memory_manager::MemoryWizardStep::Review;
    app.modal = Some(ModalKind::Wizard(
        crate::tui::event::WizardModal::MemoryAddWizard {
            state: Box::new(wizard),
        },
    ));

    let result = handle_runtime_action(
        AppAction::MemoryAddWizard(crate::tui::event::MemoryAddWizardAction::Submit),
        &mut app,
        &mut tasks,
        runtime_action_deps_with_memory(
            &storage,
            temp_dir.path(),
            session_id,
            session_store,
            runtime_tx,
            memory.clone(),
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    let moved = memory
        .store()
        .get_exact(
            crate::memory::entry::MemoryTier::User,
            "tests-run-with-quiet",
        )
        .expect("entry moved to the user tier");
    assert!(moved.path.exists());
    assert!(!old_path.exists(), "old-tier file should be removed");
    assert!(
        memory
            .store()
            .get_exact(
                crate::memory::entry::MemoryTier::Project,
                "tests-run-with-quiet",
            )
            .is_none()
    );
}

#[tokio::test]
async fn memory_manager_edit_refuses_cross_tier_name_collision() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let home_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let memory = Arc::new(crate::memory::MemoryService::load(
        home_dir.path(),
        temp_dir.path(),
        storage.clone(),
        0,
    ));
    let project_entry = memory
        .store()
        .write(
            crate::memory::entry::MemoryTier::Project,
            crate::memory::entry::MemoryEntryType::Project,
            None,
            "tests run with quiet",
            "Project copy.",
            None,
        )
        .unwrap();
    memory
        .store()
        .write(
            crate::memory::entry::MemoryTier::User,
            crate::memory::entry::MemoryEntryType::Preference,
            Some("tests-run-with-quiet"),
            "tests run with quiet",
            "User copy.",
            None,
        )
        .unwrap();
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current_session_id = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    let mut wizard =
        crate::tui::memory_manager::MemoryAddWizardState::for_edit(&project_entry.entry);
    wizard.tier = crate::memory::entry::MemoryTier::User;
    wizard.step = crate::tui::memory_manager::MemoryWizardStep::Review;
    app.modal = Some(ModalKind::Wizard(
        crate::tui::event::WizardModal::MemoryAddWizard {
            state: Box::new(wizard),
        },
    ));

    let result = handle_runtime_action(
        AppAction::MemoryAddWizard(crate::tui::event::MemoryAddWizardAction::Submit),
        &mut app,
        &mut tasks,
        runtime_action_deps_with_memory(
            &storage,
            temp_dir.path(),
            session_id,
            session_store,
            runtime_tx,
            memory.clone(),
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    // The wizard stays open with an error; both tiers keep their copies.
    let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard { state })) =
        app.modal.as_ref()
    else {
        panic!("collision should keep the wizard open, got {:?}", app.modal);
    };
    assert!(
        state
            .error
            .as_deref()
            .is_some_and(|error| error.contains("already exists"))
    );
    for (tier, body) in [
        (crate::memory::entry::MemoryTier::Project, "Project copy.\n"),
        (crate::memory::entry::MemoryTier::User, "User copy.\n"),
    ] {
        let entry = memory
            .store()
            .get_exact(tier, "tests-run-with-quiet")
            .expect("both copies survive");
        assert_eq!(entry.body, body);
        assert!(entry.path.exists());
    }
}

#[tokio::test]
async fn running_sessions_command_opens_picker() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/sessions",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(matches!(
        app.modal,
        Some(ModalKind::Picker(crate::tui::event::PickerModal::SessionPicker { ref sessions, cursor: 0 })) if sessions.is_empty()
    ));
    assert!(app.queued_inputs.is_empty());
}

#[tokio::test]
async fn session_picker_delete_end_to_end() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, current_id) = storage_with_active_session(temp_dir.path()).await;
    // Create a second session to delete
    let session_to_delete = storage
        .start_session(
            temp_dir.path(),
            "anthropic",
            "claude-sonnet-4-5",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    // End the session so it's forgettable (not live)
    storage
        .mark_session_status(session_to_delete, crate::storage::SessionStatus::Completed)
        .await
        .unwrap();

    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let yolo_mode = crate::yolo::YoloMode::new();

    // Open the session picker
    let mut app = app();
    let handled = handle_running_slash_command(
        "/sessions",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: session_store.clone(),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;
    assert!(handled);
    // Picker shows 1 session (current active is filtered out)
    assert!(matches!(
        app.modal,
        Some(ModalKind::Picker(crate::tui::event::PickerModal::SessionPicker { ref sessions, cursor: 0 })) if sessions.len() == 1
    ));

    // Simulate Delete keypress: dispatch SessionPickerDeleteSelected
    let result = handle_runtime_action(
        AppAction::SessionPickerDeleteSelected,
        &mut app,
        &mut tasks,
        runtime_action_deps(
            &storage,
            temp_dir.path(),
            current_id,
            session_store.clone(),
            runtime_tx.clone(),
        ),
        &mut state,
    )
    .await;
    assert!(matches!(result, RuntimeActionResult::Handled));

    // Confirm modal should be open
    assert!(
        matches!(
            app.modal,
            Some(ModalKind::Confirm(
                crate::tui::event::ConfirmModal::SessionDelete { .. }
            ))
        ),
        "expected SessionDeleteConfirm modal, got {:?}",
        app.modal
    );

    // Simulate Enter/Y: dispatch SessionDeleteConfirmSubmit
    let result = handle_runtime_action(
        AppAction::SessionDeleteConfirmSubmit,
        &mut app,
        &mut tasks,
        runtime_action_deps(
            &storage,
            temp_dir.path(),
            current_id,
            session_store,
            runtime_tx,
        ),
        &mut state,
    )
    .await;
    assert!(matches!(result, RuntimeActionResult::Handled));

    // Picker refreshed with only the current session
    let (sessions_after, cursor_after) = match &app.modal {
        Some(ModalKind::Picker(crate::tui::event::PickerModal::SessionPicker {
            sessions,
            cursor,
        })) => (sessions.clone(), *cursor),
        other => panic!("expected SessionPicker after delete, got {other:?}"),
    };
    assert_eq!(sessions_after.len(), 0, "deleted session should be gone");
    assert_eq!(cursor_after, 0);
}

#[tokio::test]
async fn running_compact_command_is_blocked_without_mutation() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("/compact".to_string());
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/compact",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(app.input().is_empty());
    assert!(app.queued_inputs.is_empty());
    assert!(matches!(
        app.modal,
        Some(ModalKind::Detail(crate::tui::event::DetailModal::BusyCommand { ref input, ref rows, cursor: 0 }))
            if input == "/compact"
                && rows.iter().any(|row| row.label == "Cancel current run")
                && !rows.iter().any(|row| row.label == "Queue for next run")
    ));
    assert!(app.transcript.is_empty());
}

#[tokio::test]
async fn running_yolo_command_updates_state_without_user_message() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("/yolo".to_string());
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/yolo",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(yolo_mode.is_enabled());
    assert_eq!(app.approval_level, crate::tool::ApprovalLevel::Yolo);
    assert!(
        !app.transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::UserMessage { text } if text == "/yolo"))
    );
    assert!(!app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text,
        } if text == "Autonomy set to yolo."
    )));
}

/// Audit H1: `/sandbox off` typed while the agent is busy must apply (mutate
/// the live sandbox handle and persist), not bounce into a queue modal or a
/// read-only status view.
#[tokio::test]
async fn running_sandbox_off_applies_and_persists_mid_run() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    let sandbox = crate::sandbox::CommandSandbox::disabled();
    sandbox.set_enabled(true);
    app.sandbox = Some(sandbox.clone());
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/sandbox off",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(BlockingProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(
        !sandbox.is_enabled(),
        "mid-run /sandbox off must mutate the live sandbox handle"
    );
    assert_eq!(
        storage.sandbox_enabled().await.unwrap(),
        Some(false),
        "mid-run /sandbox off must persist"
    );
    assert!(
        !matches!(
            app.modal,
            Some(ModalKind::Detail(
                crate::tui::event::DetailModal::BusyCommand { .. }
            ))
        ),
        "setting toggles must not open the busy-command modal"
    );
}

/// Audit H1: `/smol on` typed mid-run persists the preference and flips the
/// app mirror immediately (the live agent syncs when the run releases the
/// agent lock).
#[tokio::test]
async fn running_smol_on_applies_preference_mid_run() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.smol_mode = false;
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/smol on",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(BlockingProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(
        app.smol_mode,
        "mid-run /smol on must flip the app mirror immediately"
    );
    assert_eq!(
        storage.smol_preference().await.unwrap(),
        crate::smol::SmolPreference::On,
        "mid-run /smol on must persist the preference"
    );
    assert!(
        !matches!(
            app.modal,
            Some(ModalKind::Detail(
                crate::tui::event::DetailModal::BusyCommand { .. }
            ))
        ),
        "setting toggles must not open the busy-command modal"
    );
    assert!(app.transcript.is_empty());
    assert!(
        app.session_toast
            .as_ref()
            .is_some_and(|toast| toast.text.starts_with("SMOL preference set to on"))
    );
}

/// Audit H1: `/self-review off` typed mid-run applies to the app mirror (the
/// authoritative value every run start pushes onto the agent).
#[tokio::test]
async fn running_self_review_off_applies_mid_run() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    assert_eq!(
        app.self_review_mode,
        crate::self_review::SelfReviewMode::Auto
    );
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/self-review off",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(BlockingProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert_eq!(
        app.self_review_mode,
        crate::self_review::SelfReviewMode::Off,
        "mid-run /self-review off must apply to the app mirror"
    );
    assert!(
        !matches!(
            app.modal,
            Some(ModalKind::Detail(
                crate::tui::event::DetailModal::BusyCommand { .. }
            ))
        ),
        "setting toggles must not open the busy-command modal"
    );
}

/// The busy-command modal's "Open read-only view" action routes through the
/// same executor as the running composer path, so e.g. `/sandbox status`
/// opens the same SandboxStatus modal on both surfaces (audit H1: the old
/// duplicate busy executor had drifted).
#[tokio::test]
async fn busy_modal_read_only_executor_matches_running_path() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.sandbox = Some(crate::sandbox::CommandSandbox::disabled());

    crate::tui::run::commands::apply_non_idle_read_only_command(
        "/sandbox status",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(BlockingProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &mut state,
    )
    .await;

    assert!(
        matches!(
            app.modal,
            Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::SandboxStatus { .. }
            ))
        ),
        "/sandbox status through the shared read-only executor must open the SandboxStatus modal"
    );
}

/// `/skills` and `/agents` must open while a run holds the agent lock — the
/// lock is held for a turn's entire duration, and the old lock-taking open
/// path froze the whole event loop until the turn finished. Rows now build
/// from the app's shared handles + mirrors, never the agent lock.
#[tokio::test]
async fn skills_and_agents_modals_open_while_agent_lock_is_held() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut app = app();
    app.task_state = TaskState::Running;

    let agent = test_agent(Box::new(BlockingProvider));
    // Simulate a running turn: the run task holds the guard until it finishes.
    let _running_turn_guard = agent.lock().await;

    for (input, expect_skill_manager) in [("/skills", true), ("/agents", false)] {
        app.modal = None;
        let mut state = PersistenceCommandState {
            current_session_id: &mut current_session_id,
            signatures: &mut signatures,
        };
        tokio::time::timeout(
            Duration::from_secs(5),
            crate::tui::run::commands::apply_non_idle_read_only_command(
                input,
                &mut app,
                PersistenceCommandDeps {
                    storage: &storage,
                    agent: agent.clone(),
                    memory: None,
                    session_store: Arc::new(Mutex::new(authorized_codex_store())),
                    registry: Arc::new(ProviderRegistry::default_registry()),
                    model_catalog: test_model_catalog(),
                    todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
                    plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
                    project_root: temp_dir.path(),
                    active_session_id: Arc::new(Mutex::new(Some(current_id))),
                },
                &mut state,
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{input} must not await the held agent lock"));
        if expect_skill_manager {
            assert!(
                matches!(
                    app.modal,
                    Some(ModalKind::Manager(
                        crate::tui::event::ManagerModal::SkillManager { .. }
                    ))
                ),
                "/skills must open its manager mid-run"
            );
        } else {
            assert!(
                matches!(
                    app.modal,
                    Some(ModalKind::Manager(
                        crate::tui::event::ManagerModal::AgentBrowser { .. }
                    ))
                ),
                "/agents must open its browser mid-run"
            );
        }
    }
}

/// Editing or toggling a built-in subagent from the `/agents` browser must not
/// take the agent lock: the browser opens mid-run, and a running turn holds
/// that lock for its whole duration. Both actions read the shared
/// `app.builtin_subagents` handle instead, so they complete with the lock held —
/// the old lock-taking path froze the TUI until the turn finished.
#[tokio::test]
async fn agent_browser_edit_and_toggle_do_not_block_on_held_agent_lock() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());

    let agent = test_agent(Box::new(BlockingProvider));
    // Simulate a running turn: the run task holds the guard until it finishes.
    let _running_turn_guard = agent.lock().await;

    // A built-in row is selected in the browser; its settings live in the shared
    // handle the app cloned at startup, not behind the agent lock.
    let rows = crate::tui::agent_composer::browser_rows_with_settings(
        &crate::resource::agent::AgentRegistry::empty(),
        &std::collections::BTreeMap::new(),
    );
    let builtin_id = rows[0].builtin_id.expect("first row is a built-in");

    let mut app = app();
    app.task_state = TaskState::Running;
    app.builtin_subagents = crate::subagent::SharedBuiltinSubagentSettings::default();
    app.modal = Some(ModalKind::Manager(
        crate::tui::event::ManagerModal::AgentBrowser {
            rows: rows.clone(),
            cursor: 0,
        },
    ));

    // Fresh deps per dispatch (each is consumed by value); all borrow the same
    // held-lock agent so a stray `agent.lock().await` would deadlock the test.
    let make_deps = || {
        let (interaction, _interaction_rx) = InteractionService::new();
        RuntimeActionDeps {
            interaction: Arc::new(interaction),
            runtime_sender: runtime_tx.clone(),
            agent: agent.clone(),
            memory: None,
            yolo_mode: crate::yolo::YoloMode::new(),
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            permissions: crate::permissions::PermissionManager::memory_only(),
            domain_permissions: crate::permissions::PermissionManager::memory_only_domains(),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            storage: &storage,
            project_root: temp_dir.path(),
            session_project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            sink: Arc::new(NullSink),
            background_tasks: Arc::new(BackgroundTaskRegistry::new()),
            terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
            peer_bus: None,
        }
    };

    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };

    // Toggle the built-in's enabled flag while the lock is held.
    tokio::time::timeout(
        Duration::from_secs(5),
        handle_runtime_action(
            AppAction::AgentBrowserToggleEnabled,
            &mut app,
            &mut tasks,
            make_deps(),
            &mut state,
        ),
    )
    .await
    .expect("toggling a built-in must not await the held agent lock");
    assert!(
        !app.builtin_subagents.get(builtin_id).unwrap().enabled,
        "toggle must flip the built-in in the shared handle, off the agent lock"
    );
    assert!(
        matches!(
            app.modal,
            Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::AgentBrowser { .. }
            ))
        ),
        "toggle must reopen the browser"
    );

    // Open the composer to edit the built-in while the lock is still held.
    tokio::time::timeout(
        Duration::from_secs(5),
        handle_runtime_action(
            AppAction::AgentBrowserEdit,
            &mut app,
            &mut tasks,
            make_deps(),
            &mut state,
        ),
    )
    .await
    .expect("editing a built-in must not await the held agent lock");
    assert!(
        matches!(
            app.modal,
            Some(ModalKind::Wizard(
                crate::tui::event::WizardModal::AgentComposer { .. }
            ))
        ),
        "edit must open the composer mid-run"
    );

    let expected_settings = {
        let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
            &mut app.modal
        else {
            panic!("built-in composer must remain open");
        };
        state.model.set_text("codex:gpt-5-live".to_string());
        state.effort_index = 4;
        state
            .fallback_model
            .set_text("anthropic:claude-live".to_string());
        state.fallback_effort_index = 2;
        state.step = crate::tui::agent_composer::AgentComposerStep::Review;
        state.builtin_subagent_settings().unwrap().1
    };
    tokio::time::timeout(
        Duration::from_secs(5),
        handle_runtime_action(
            AppAction::AgentComposer(crate::tui::event::AgentComposerAction::Submit),
            &mut app,
            &mut tasks,
            make_deps(),
            &mut state,
        ),
    )
    .await
    .expect("saving built-in settings must not await the held agent lock");
    assert_eq!(
        app.builtin_subagents.get(builtin_id).as_ref(),
        Some(&expected_settings),
        "successful persistence must publish the settings to later launches"
    );
    assert_eq!(
        storage
            .load_builtin_subagent_settings()
            .await
            .unwrap()
            .get(&builtin_id),
        Some(&expected_settings),
        "built-in settings must reach storage before publication"
    );

    let mut custom = crate::tui::agent_composer::AgentComposerState::new();
    custom.name.set_text("live custom".to_string());
    custom
        .description
        .set_text("saved during an active run".to_string());
    custom
        .prompt
        .set_text("Inspect the requested files.".to_string());
    custom.step = crate::tui::agent_composer::AgentComposerStep::Review;
    let custom_path =
        crate::tui::agent_composer::agent_file_path(&custom, storage.home_dir(), temp_dir.path());
    app.modal = Some(ModalKind::Wizard(
        crate::tui::event::WizardModal::AgentComposer {
            state: Box::new(custom),
        },
    ));
    tokio::time::timeout(
        Duration::from_secs(5),
        handle_runtime_action(
            AppAction::AgentComposer(crate::tui::event::AgentComposerAction::Submit),
            &mut app,
            &mut tasks,
            make_deps(),
            &mut state,
        ),
    )
    .await
    .expect("saving a custom agent must not await the held agent lock");
    let custom_markdown = std::fs::read_to_string(&custom_path).unwrap();
    assert!(custom_markdown.contains("name: \"live custom\""));
    assert!(custom_markdown.contains("Inspect the requested files."));
}

#[tokio::test]
async fn cached_model_picker_opens_without_command_task() {
    let registry = Arc::new(ProviderRegistry::default_registry());
    let store = authorized_codex_store();
    let session_store = Arc::new(Mutex::new(store));
    let mut app = app();

    open_cached_model_picker(&mut app, session_store, registry, test_model_catalog()).await;

    assert_eq!(app.task_state, TaskState::Idle);
    assert!(app.transcript.is_empty());
    let Some(ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker { entries })) =
        &app.modal
    else {
        panic!("model picker should be open");
    };
    assert!(entries.iter().any(|entry| {
        entry.provider_id == "codex" && entry.provider_label == "Codex" && entry.model == "gpt-5.5"
    }));
}

#[tokio::test]
async fn cached_model_picker_does_not_refresh_provider_models() {
    let list_models_calls = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(ProviderRegistry::new(vec![Arc::new(
        ModelListingCodexFactory {
            list_models_calls: list_models_calls.clone(),
        },
    )]));
    let session_store = Arc::new(Mutex::new(authorized_codex_store()));
    let mut app = app();

    open_cached_model_picker(
        &mut app,
        session_store,
        registry,
        Arc::new(crate::model_catalog::ModelCatalog::load_builtin().unwrap()),
    )
    .await;

    let Some(ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker { entries })) =
        &app.modal
    else {
        panic!("model picker should be open");
    };
    assert!(entries.iter().all(|entry| entry.model != "gpt-new"));
    assert_eq!(list_models_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn model_picker_shortcut_key_persists_and_toggles_selection() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut saved_store = authorized_codex_store();
    saved_store.set_current_kind_id("codex");
    storage
        .save_session_store_with_auth_policy(
            &saved_store,
            crate::session::SaveAuthPolicy::PreserveExisting,
        )
        .await
        .unwrap();
    let mut loaded_store = SessionStore::load_with_storage(&storage).await.unwrap();
    // Credentials no longer round-trip through SQLite. This test exercises
    // model-shortcut persistence, so inject the runtime Codex authorization
    // after loading the persistence-backed store.
    loaded_store.session_mut("codex").api_key = "codex-token".to_string();
    loaded_store.session_mut("codex").account_id = "codex-account".to_string();
    let session_store = Arc::new(Mutex::new(loaded_store));
    let registry = Arc::new(ProviderRegistry::default_registry());
    let model_catalog = test_model_catalog();
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current_session_id = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();

    open_cached_model_picker(&mut app, session_store.clone(), registry, model_catalog).await;
    app.model_picker.active_pane = ModelPickerPane::Reasoning;
    let selected_model = {
        let Some(ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker { entries })) =
            &app.modal
        else {
            panic!("model picker should be open");
        };
        app.model_picker_selected_model(entries)
            .expect("model picker should select a model")
            .model
            .clone()
    };
    let key = crate::model_role::ModelShortcutKey::new('z').unwrap();

    let assigned = handle_runtime_action(
        AppAction::ModelPicker(crate::tui::event::ModelPickerAction::AssignShortcut(key)),
        &mut app,
        &mut tasks,
        runtime_action_deps(
            &storage,
            temp_dir.path(),
            session_id,
            session_store.clone(),
            runtime_tx.clone(),
        ),
        &mut state,
    )
    .await;

    assert!(matches!(assigned, RuntimeActionResult::Handled));
    {
        let session = session_store.lock().await;
        let binding = session
            .model_shortcut_binding(key)
            .expect("shortcut key should persist selected model");
        assert_eq!(binding.provider_id.as_str(), "codex");
        assert_eq!(binding.model, selected_model);
    }
    let Some(ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker { entries })) =
        &app.modal
    else {
        panic!("model picker should stay open");
    };
    let entry = app
        .model_picker_selected_model(entries)
        .expect("model picker should keep a selected model");
    assert!(
        entry
            .shortcut_bindings
            .iter()
            .any(|(bound, _)| *bound == key)
    );

    let toggled = handle_runtime_action(
        AppAction::ModelPicker(crate::tui::event::ModelPickerAction::AssignShortcut(key)),
        &mut app,
        &mut tasks,
        runtime_action_deps(
            &storage,
            temp_dir.path(),
            session_id,
            session_store.clone(),
            runtime_tx,
        ),
        &mut state,
    )
    .await;

    assert!(matches!(toggled, RuntimeActionResult::Handled));
    assert!(
        session_store
            .lock()
            .await
            .model_shortcut_binding(key)
            .is_none()
    );
    let Some(ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker { entries })) =
        &app.modal
    else {
        panic!("model picker should stay open");
    };
    let entry = app
        .model_picker_selected_model(entries)
        .expect("model picker should keep a selected model");
    assert!(
        entry
            .shortcut_bindings
            .iter()
            .all(|(bound, _)| *bound != key)
    );
}

#[tokio::test]
async fn model_picker_submit_while_running_queues_selected_model() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let registry = Arc::new(ProviderRegistry::default_registry());
    let session_store = Arc::new(Mutex::new(authorized_codex_store()));
    let model_catalog = test_model_catalog();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    open_cached_model_picker(
        &mut app,
        session_store.clone(),
        registry.clone(),
        model_catalog.clone(),
    )
    .await;
    let selected_reasoning = {
        let Some(ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker { entries })) =
            &app.modal
        else {
            panic!("model picker should be open");
        };
        let entry = app
            .model_picker_selected_model(entries)
            .expect("model picker should select a model");
        let choices = AppState::model_picker_reasoning_choices(entry);
        let reasoning = choices
            .iter()
            .copied()
            .find(|candidate| *candidate != ReasoningSelection::default())
            .expect("selected model should expose a non-default reasoning option");
        app.model_picker.reasoning_cursor = choices
            .iter()
            .position(|candidate| *candidate == reasoning)
            .unwrap_or(0);
        reasoning
    };
    let (interaction, _interaction_rx) = InteractionService::new();

    let result = handle_runtime_action(
        AppAction::ModelPicker(crate::tui::event::ModelPickerAction::Submit),
        &mut app,
        &mut tasks,
        RuntimeActionDeps {
            interaction: Arc::new(interaction),
            runtime_sender: runtime_tx,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            yolo_mode: crate::yolo::YoloMode::new(),
            session_store,
            permissions: crate::permissions::PermissionManager::memory_only(),
            domain_permissions: crate::permissions::PermissionManager::memory_only_domains(),
            registry,
            model_catalog,
            storage: &storage,
            project_root: temp_dir.path(),
            session_project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            sink: Arc::new(NullSink),
            background_tasks: Arc::new(BackgroundTaskRegistry::new()),
            terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
            peer_bus: None,
        },
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    assert!(app.modal.is_none());
    assert!(app.queued_inputs.is_empty());
    assert!(matches!(
        app.deferred_commands.as_slice(),
        [DeferredCommand {
            input,
            label,
            payload: DeferredCommandPayload::ModelSelection(selection),
            ..
        }]
            if input.starts_with("/model ")
                && label == input
                && selection.reasoning == selected_reasoning
    ));
    assert!(matches!(
        app.transcript.last(),
        Some(TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text,
        }) if text.starts_with("Queued /model ")
    ));
}

#[tokio::test]
async fn theme_picker_move_previews_and_cancel_restores_original_theme() {
    let _theme_guard = crate::tui::theme::TEST_LOCK.lock().await;
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    crate::tui::theme::set_theme("forest");
    let mut app = app();
    app.reduce(AppAction::OpenModal(ModalKind::Picker(
        crate::tui::event::PickerModal::ThemePicker {
            cursor: 0,
            original_theme: "forest".to_string(),
        },
    )));

    let result = handle_runtime_action(
        AppAction::Modal(crate::tui::event::ModalAction::ThemePicker(
            crate::tui::event::ThemePickerAction::Move(1),
        )),
        &mut app,
        &mut tasks,
        runtime_action_deps(
            &storage,
            temp_dir.path(),
            session_id,
            session_store.clone(),
            runtime_tx.clone(),
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    assert_eq!(crate::tui::theme::current_theme_name(), "ocean");
    assert!(matches!(
        app.modal,
        Some(ModalKind::Picker(
            crate::tui::event::PickerModal::ThemePicker { cursor: 1, .. }
        ))
    ));

    let result = handle_runtime_action(
        AppAction::Modal(crate::tui::event::ModalAction::ThemePicker(
            crate::tui::event::ThemePickerAction::Cancel,
        )),
        &mut app,
        &mut tasks,
        runtime_action_deps(
            &storage,
            temp_dir.path(),
            session_id,
            session_store,
            runtime_tx,
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    assert_eq!(crate::tui::theme::current_theme_name(), "forest");
    assert!(app.modal.is_none());
}

#[tokio::test]
async fn theme_picker_submit_persists_selected_theme() {
    let _theme_guard = crate::tui::theme::TEST_LOCK.lock().await;
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut saved_store = SessionStore::default();
    saved_store.ensure_provider("codex");
    saved_store.theme = "forest".to_string();
    storage
        .save_session_store_with_auth_policy(
            &saved_store,
            crate::session::SaveAuthPolicy::PreserveExisting,
        )
        .await
        .unwrap();
    let session_store = Arc::new(Mutex::new(
        SessionStore::load_with_storage(&storage).await.unwrap(),
    ));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    crate::tui::theme::set_theme("forest");
    let mut app = app();
    app.reduce(AppAction::OpenModal(ModalKind::Picker(
        crate::tui::event::PickerModal::ThemePicker {
            cursor: 2,
            original_theme: "forest".to_string(),
        },
    )));

    let result = handle_runtime_action(
        AppAction::Modal(crate::tui::event::ModalAction::ThemePicker(
            crate::tui::event::ThemePickerAction::Submit,
        )),
        &mut app,
        &mut tasks,
        runtime_action_deps(
            &storage,
            temp_dir.path(),
            session_id,
            session_store.clone(),
            runtime_tx,
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    assert_eq!(crate::tui::theme::current_theme_name(), "paper");
    assert!(app.modal.is_none());
    assert_eq!(session_store.lock().await.theme, "paper");
    let persisted = storage
        .load_session_store_raw()
        .await
        .unwrap()
        .expect("session store should persist");
    assert_eq!(persisted.theme, "paper");
    crate::tui::theme::set_theme("forest");
}

#[tokio::test]
async fn settings_cycle_persists_serenity_preference() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    let mut app = app();
    let rows = crate::tui::settings::seed_settings_rows(&app, smol_off());
    let cursor = rows
        .iter()
        .position(|row| row.id() == Some(crate::tui::event::SettingId::Serenity))
        .expect("settings should include the serenity row");
    app.reduce(AppAction::OpenModal(ModalKind::Manager(
        crate::tui::event::ManagerModal::Settings { rows, cursor },
    )));

    let result = handle_runtime_action(
        AppAction::SettingsCycle(1),
        &mut app,
        &mut tasks,
        runtime_action_deps(
            &storage,
            temp_dir.path(),
            session_id,
            session_store.clone(),
            runtime_tx,
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    assert!(app.serenity_mode, "cycle should flip serenity on");
    assert!(
        storage.serenity_mode().await.unwrap(),
        "settings cycle should persist the serenity preference"
    );
}

#[tokio::test]
async fn settings_cycle_persists_explicit_smol_preference() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    let mut app = app();
    let rows = crate::tui::settings::seed_settings_rows(&app, smol_off());
    let cursor = rows
        .iter()
        .position(|row| row.id() == Some(crate::tui::event::SettingId::Smol))
        .expect("settings should include the SMOL row");
    app.reduce(AppAction::OpenModal(ModalKind::Manager(
        crate::tui::event::ManagerModal::Settings { rows, cursor },
    )));

    let result = handle_runtime_action(
        AppAction::SettingsCycle(1),
        &mut app,
        &mut tasks,
        runtime_action_deps(
            &storage,
            temp_dir.path(),
            session_id,
            session_store,
            runtime_tx,
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    assert!(app.smol_mode);
    assert_eq!(
        storage.smol_preference().await.unwrap(),
        crate::smol::SmolPreference::On
    );
}

#[tokio::test]
async fn settings_cycle_opts_into_and_persists_a_run_budget() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    let mut app = app();
    let rows = crate::tui::settings::seed_settings_rows(&app, smol_off());
    let cursor = rows
        .iter()
        .position(|row| row.id() == Some(crate::tui::event::SettingId::BudgetTurns))
        .expect("settings should include the turn budget row");
    app.reduce(AppAction::OpenModal(ModalKind::Manager(
        crate::tui::event::ManagerModal::Settings { rows, cursor },
    )));

    let result = handle_runtime_action(
        AppAction::SettingsCycle(1),
        &mut app,
        &mut tasks,
        runtime_action_deps(
            &storage,
            temp_dir.path(),
            session_id,
            session_store,
            runtime_tx,
        ),
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    assert_eq!(app.run_budget.max_turns, Some(25));
    assert_eq!(storage.run_budget().await.unwrap().max_turns, Some(25));
}

#[tokio::test]
async fn theme_export_writes_starter_file_and_refuses_overwrite() {
    let _theme_guard = crate::tui::theme::TEST_LOCK.lock().await;
    crate::tui::theme::set_theme("forest");
    let project = tempfile::TempDir::new().unwrap();
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let mut app = app();

    apply_theme_command(
        "export mytheme",
        &mut app,
        project.path(),
        session_store.clone(),
    )
    .await;
    let path = project.path().join(".bonsai/themes/mytheme.toml");
    assert!(path.exists(), "export writes the theme file");
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(
        written.contains("bg ="),
        "exported file is a theme: {written}"
    );

    // A second export refuses to overwrite and leaves the file untouched.
    apply_theme_command("export mytheme", &mut app, project.path(), session_store).await;
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        written,
        "file unchanged"
    );
    crate::tui::theme::set_theme("forest");
}

#[tokio::test]
async fn model_picker_selection_preserves_agent_context() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let mut store = SessionStore::default();
    store.ensure_provider("codex");
    store.ensure_provider("anthropic");
    store.session_mut("codex").api_key = "codex-token".to_string();
    store.session_mut("codex").account_id = "codex-account".to_string();
    store.session_mut("anthropic").api_key = "anthropic-token".to_string();
    store.set_current_kind_id("codex");
    storage
        .save_session_store_with_auth_policy(
            &store,
            crate::session::SaveAuthPolicy::PreserveExisting,
        )
        .await
        .unwrap();
    let session_store = Arc::new(Mutex::new(
        SessionStore::load_with_storage(&storage).await.unwrap(),
    ));
    let agent = test_agent(Box::new(CompleteProvider));
    {
        let mut guard = agent.lock().await;
        guard
            .restore_text_history(&[
                (
                    "user".to_string(),
                    "keep this model switch context".to_string(),
                ),
                ("assistant".to_string(), "still here".to_string()),
            ])
            .await
            .unwrap();
    }
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let mut app = app();

    switch_model_selection(
        ModelSelection {
            provider_id: "anthropic".to_string(),
            connection_id: "anthropic".to_string(),
            model_id: Some("anthropic/claude-sonnet-4-5".to_string()),
            model: "claude-sonnet-4-5".to_string(),
            selection_input: Some("anthropic:claude-sonnet-4-5".to_string()),
            reasoning: crate::provider::ReasoningSelection::default(),
        },
        &mut app,
        agent.clone(),
        session_store,
        Arc::new(ProviderRegistry::default_registry()),
        Arc::new(crate::model_catalog::ModelCatalog::load_builtin().unwrap()),
        sender,
    );

    let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .unwrap()
        .expect("model switch should send a runtime event");
    app.reduce(AppAction::Runtime(event));

    assert_eq!(app.task_state, TaskState::Idle);
    assert_eq!(app.provider, "anthropic");
    assert_eq!(app.model, "anthropic/claude-sonnet-4-5");
    assert!(!app.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text,
        } if text.starts_with("Model set to ")
    )));
    let context_report = agent.lock().await.context_report();
    assert!(
        context_report
            .entries
            .iter()
            .any(|entry| { entry.text.contains("keep this model switch context") })
    );
    assert!(app.latest_context_report.is_some());
}

#[tokio::test]
async fn running_commit_command_is_blocked_without_user_message() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("/commit".to_string());
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/commit",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(app.input().is_empty());
    assert!(app.queued_inputs.is_empty());
    assert!(matches!(
        app.modal,
        Some(ModalKind::Detail(crate::tui::event::DetailModal::BusyCommand { ref input, ref rows, cursor: 0 }))
            if input == "/commit"
                && rows.iter().any(|row| row.label == "Cancel current run")
                && !rows.iter().any(|row| row.label == "Queue for next run")
    ));
    assert!(
        !app.transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::UserMessage { text } if text == "/commit"))
    );
}

#[tokio::test]
async fn running_pr_command_is_blocked_without_user_message() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("/pr".to_string());
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/pr",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(app.input().is_empty());
    assert!(app.queued_inputs.is_empty());
    assert!(matches!(
        app.modal,
        Some(ModalKind::Detail(crate::tui::event::DetailModal::BusyCommand { ref input, ref rows, cursor: 0 }))
            if input == "/pr"
                && rows.iter().any(|row| row.label == "Cancel current run")
                && !rows.iter().any(|row| row.label == "Queue for next run")
    ));
    assert!(
        !app.transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::UserMessage { text } if text == "/pr"))
    );
}

#[tokio::test]
async fn running_save_command_persists_plan_without_user_message() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "gpt-5",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id = current_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.task_state = TaskState::Running;
    app.plan = sample_plan("Runnable save");
    let yolo_mode = crate::yolo::YoloMode::new();

    let handled = handle_running_slash_command(
        "/save",
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_id))),
        },
        &yolo_mode,
        &mut state,
    )
    .await;

    assert!(handled);
    assert!(
        !app.transcript
            .iter()
            .any(|item| matches!(item, TranscriptItem::UserMessage { text } if text == "/save"))
    );
    let saved_id = app
        .active_saved_plan_session_id
        .expect("/save should bind the canvas to the new library entry");
    assert_eq!(
        storage
            .load_saved_plan(saved_id)
            .await
            .unwrap()
            .expect("plan should be saved")
            .plan
            .title,
        "Runnable save"
    );
}

#[tokio::test]
async fn exact_tasks_command_opens_modal_while_agent_is_running() {
    let registry = Arc::new(BackgroundTaskRegistry::new());
    let terminals = Arc::new(crate::terminal::TerminalRegistry::new());
    let mut app = app();
    app.task_state = TaskState::Running;
    app.composer.set_text("/tasks".to_string());
    let handled = open_tasks_command_if_exact("/tasks", &mut app, &registry, &terminals).await;

    assert!(handled);
    assert!(app.input().is_empty());
    assert!(app.transcript.is_empty());
    assert!(matches!(
        app.modal,
        Some(ModalKind::Manager(
            crate::tui::event::ManagerModal::TaskList { .. }
        ))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn task_list_includes_interactive_terminals() {
    let registry = Arc::new(BackgroundTaskRegistry::new());
    let terminals = Arc::new(crate::terminal::TerminalRegistry::new());
    let terminal = terminals
        .start("/bin/sh", "sleep 30", std::path::Path::new("/"), 60, None)
        .await
        .expect("PTY fixture should start");
    let mut app = app();

    refresh_background_task_list(&mut app, &registry, &terminals).await;
    app.reduce(AppAction::OpenTaskList);

    assert!(matches!(
        app.modal,
        Some(ModalKind::Manager(crate::tui::event::ManagerModal::TaskList { ref tasks, .. }))
            if tasks.iter().any(|task| task.id == terminal.id)
    ));
    let _ = terminals.stop(&terminal.id).await;
}

#[tokio::test]
async fn deleting_selected_running_task_stops_then_removes_it() {
    let registry = Arc::new(BackgroundTaskRegistry::new());
    let terminals = Arc::new(crate::terminal::TerminalRegistry::new());
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    let temp_dir = tempfile::TempDir::new().unwrap();
    let task = registry
        .start(&shell(), "sleep 5", temp_dir.path(), 30)
        .await
        .unwrap();
    let mut app = app();
    refresh_background_task_list(&mut app, &registry, &terminals).await;
    app.reduce(AppAction::OpenTaskList);

    start_delete_selected_background_task(
        &mut app,
        registry.clone(),
        terminals.clone(),
        runtime_tx,
    );

    assert!(matches!(
        app.task_list_status.as_deref(),
        Some(status) if status == format!("Stopping {}...", task.id)
    ));

    let event = tokio::time::timeout(Duration::from_secs(2), runtime_rx.recv())
        .await
        .expect("removal should finish")
        .expect("runtime event should be sent");
    assert!(matches!(
        &event,
        RuntimeEvent::BackgroundTaskRemovalFinished { task_id, error: None }
            if task_id == &task.id
    ));
    app.reduce(AppAction::Runtime(event));
    refresh_background_task_list(&mut app, &registry, &terminals).await;

    assert!(registry.list().await.is_empty());
    assert!(matches!(
        app.task_list_status.as_deref(),
        Some(status) if status == format!("Removed {}.", task.id)
    ));
    assert!(matches!(
        app.modal,
        Some(ModalKind::Manager(crate::tui::event::ManagerModal::TaskList { ref tasks, .. })) if tasks.is_empty()
    ));
}

#[test]
fn background_task_events_request_task_modal_refresh() {
    let mut app = app();

    let refresh = apply_background_task_event(
        &mut app,
        BackgroundTaskEvent::Removed {
            task_id: "bg-1".to_string(),
        },
    );

    assert_eq!(
        refresh,
        BackgroundTaskUiEffect {
            refresh_task_list: true,
            wake_candidate: false,
        }
    );
}

#[test]
fn background_task_finished_event_updates_attached_tool_and_refreshes_tasks() {
    let mut app = app();
    let now = Instant::now();
    app.reduce(AppAction::Agent(UiEvent::ToolStarted {
        id: "call-1".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"sleep 1","run_in_background":true}"#.to_string(),
        started_at: now,
    }));

    let refresh = apply_background_task_event(
        &mut app,
        BackgroundTaskEvent::Finished {
            task_id: "bg-1".to_string(),
            tool_call_id: Some("call-1".to_string()),
            status: BackgroundTaskStatus::Succeeded,
            summary: "done".to_string(),
            success: true,
            version: 2,
        },
    );

    assert_eq!(
        refresh,
        BackgroundTaskUiEffect {
            refresh_task_list: true,
            wake_candidate: true,
        }
    );
    assert!(matches!(
        app.tool_activity("call-1").map(|activity| activity.status),
        Some(crate::tui::app::ToolStatus::Succeeded)
    ));
}

#[test]
fn stopped_background_task_finish_does_not_request_wake() {
    let mut app = app();

    let effect = apply_background_task_event(
        &mut app,
        BackgroundTaskEvent::Finished {
            task_id: "bg-1".to_string(),
            tool_call_id: Some("call-1".to_string()),
            status: BackgroundTaskStatus::Stopped,
            summary: "stopped".to_string(),
            success: false,
            version: 2,
        },
    );

    assert_eq!(
        effect,
        BackgroundTaskUiEffect {
            refresh_task_list: true,
            wake_candidate: false,
        }
    );
}

#[test]
fn terminal_wait_and_finish_update_attached_bash_tool() {
    let mut app = app();
    app.reduce(AppAction::Agent(UiEvent::ToolStarted {
        id: "call-pty".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"read answer","interactive":true}"#.to_string(),
        started_at: Instant::now(),
    }));

    let waiting = apply_terminal_event(
        &mut app,
        TerminalEvent::WaitingForInput {
            terminal_id: "pty-1".to_string(),
            tool_call_id: Some("call-pty".to_string()),
            summary: "waiting for input".to_string(),
            version: 2,
        },
    );
    assert!(waiting.wake_candidate);
    let activity = app
        .tool_activity("call-pty")
        .expect("tool should remain visible");
    assert_eq!(activity.status, ToolStatus::Running);
    assert_eq!(activity.result.as_deref(), Some("waiting for input"));

    let finished = apply_terminal_event(
        &mut app,
        TerminalEvent::Finished {
            terminal_id: "pty-1".to_string(),
            tool_call_id: Some("call-pty".to_string()),
            status: TerminalStatus::Succeeded,
            summary: "terminal complete".to_string(),
            success: true,
            version: 2,
        },
    );
    assert!(finished.wake_candidate);
    let activity = app
        .tool_activity("call-pty")
        .expect("tool should remain visible");
    assert_eq!(activity.status, ToolStatus::Succeeded);
    assert_eq!(activity.result.as_deref(), Some("terminal complete"));
}

#[test]
fn resume_marks_process_local_interactive_terminal_as_lost() {
    let now = Instant::now();
    let mut items = vec![TranscriptItem::ToolActivity(ToolActivity {
        id: "call-pty".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"repl","interactive":true}"#.to_string(),
        delegated_model: None,
        status: ToolStatus::Running,
        result: Some("Started interactive terminal pty-1".to_string()),
        diff: None,
        started_at: now,
        finished_at: None,
    })];

    let lost = normalize_lost_interactive_terminals(&mut items);

    let TranscriptItem::ToolActivity(activity) = &items[0] else {
        panic!("expected tool activity");
    };
    assert_eq!(activity.status, ToolStatus::Failed);
    assert_eq!(lost, vec!["pty-1"]);
    assert!(activity.finished_at.is_some());
    assert!(
        activity
            .result
            .as_deref()
            .is_some_and(|result| result.contains("process-local"))
    );
}

#[test]
fn shutdown_background_task_snapshot_finishes_attached_tool_before_persistence() {
    let mut app = app();
    let started_at = Instant::now();
    app.reduce(AppAction::Agent(UiEvent::ToolStarted {
        id: "call-1".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"sleep 30","run_in_background":true}"#.to_string(),
        started_at,
    }));
    app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
        crate::tui::event::AgentRunOutcome::Completed,
    ))));
    assert!(matches!(
        app.tool_activity("call-1").map(|activity| activity.status),
        Some(ToolStatus::Running)
    ));
    let group_id = app.active_execution_group_id;
    app.task_state = TaskState::Exiting;

    let effect = apply_background_task_snapshot(
        &mut app,
        &background_task_snapshot(
            "bg-1",
            BackgroundTaskStatus::Stopped,
            Some("call-1"),
            "partial output",
        ),
    );

    assert_eq!(
        effect,
        BackgroundTaskUiEffect {
            refresh_task_list: true,
            wake_candidate: false,
        }
    );
    let activity = app.tool_activity("call-1").expect("tool remains visible");
    assert_eq!(activity.status, ToolStatus::Interrupted);
    let result = activity.result.as_deref().unwrap_or_default();
    assert!(result.contains("stopped"), "result: {result}");
    assert!(result.contains("partial output"), "result: {result}");
    assert!(app.active_tools.is_empty());
    assert!(app.active_execution_group_id.is_none());
    let group = app
        .execution_group(group_id.expect("background group stays active until shutdown cleanup"))
        .expect("group remains in transcript");
    assert!(group.finished_at.is_some());
}

#[test]
fn completed_detached_subagent_finishes_attached_agent_tool() {
    let mut app = app();
    let started_at = Instant::now();
    app.reduce(AppAction::Agent(UiEvent::ToolStarted {
        id: "call-1".to_string(),
        name: "agent".to_string(),
        arguments: r#"{"agent":"explore","prompt":"Review area","run_in_background":true}"#
            .to_string(),
        started_at,
    }));
    assert!(matches!(
        app.tool_activity("call-1").map(|activity| activity.status),
        Some(ToolStatus::Running)
    ));

    let subagents = crate::subagent::SubagentRegistry::new();
    let subtask_id = subagents.register("explore", "Review area", true);
    subagents
        .attach_tool_call(&subtask_id, "call-1")
        .expect("subagent should exist");
    subagents.finish(
        &subtask_id,
        crate::subagent::SubagentStatus::Succeeded,
        Some("all clear".to_string()),
    );

    assert!(apply_completed_subagent_tool_calls(&mut app, &subagents));
    let activity = app.tool_activity("call-1").expect("tool remains visible");
    assert_eq!(activity.status, ToolStatus::Succeeded);
    assert!(
        activity
            .result
            .as_deref()
            .is_some_and(|result| result.contains("all clear")),
        "result: {:?}",
        activity.result
    );
    assert!(app.active_tools.is_empty());
    assert!(!apply_completed_subagent_tool_calls(&mut app, &subagents));
}

#[test]
fn completed_subagent_reconciles_after_late_tool_started_event() {
    let mut app = app();
    let subagents = crate::subagent::SubagentRegistry::new();
    let subtask_id = subagents.register("explore", "Review area", true);
    subagents
        .attach_tool_call(&subtask_id, "call-1")
        .expect("subagent should exist");
    subagents.finish(
        &subtask_id,
        crate::subagent::SubagentStatus::Succeeded,
        Some("all clear".to_string()),
    );

    assert!(!apply_completed_subagent_tool_calls(&mut app, &subagents));
    app.reduce(AppAction::Agent(UiEvent::ToolStarted {
        id: "call-1".to_string(),
        name: "agent".to_string(),
        arguments: r#"{"agent":"explore","prompt":"Review area","run_in_background":true}"#
            .to_string(),
        started_at: Instant::now(),
    }));

    assert!(apply_completed_subagent_tool_calls(&mut app, &subagents));
    assert_eq!(
        app.tool_activity("call-1").map(|activity| activity.status),
        Some(ToolStatus::Succeeded)
    );
    assert!(app.active_tools.is_empty());
}

#[tokio::test]
async fn background_wake_starts_agent_with_completed_task_output() {
    let registry = Arc::new(BackgroundTaskRegistry::new());
    let temp_dir = tempfile::TempDir::new().unwrap();
    let task = registry
        .start(&shell(), "printf wake-output", temp_dir.path(), 5)
        .await
        .unwrap();
    registry.attach_tool_call(&task.id, "call-1").await.unwrap();
    registry
        .wait_for_task(&task.id, Duration::from_secs(2))
        .await
        .unwrap();
    assert!(registry.agent_wake_ready().await);

    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, requests) = CapturingProvider::new();
    let agent = test_agent(Box::new(provider));
    agent.lock().await.set_background_tasks(registry.clone());
    let mut app = app();
    let mut pending = Some(Instant::now() - Duration::from_millis(1));
    let subagents = Arc::new(crate::subagent::SubagentRegistry::new());
    let terminals = Arc::new(crate::terminal::TerminalRegistry::new());

    let peer_bus = Arc::new(crate::peer::PeerBus::new(
        crate::storage::Storage::open_at(temp_dir.path().join("t.db"))
            .await
            .unwrap(),
        Arc::new(tokio::sync::Mutex::new(None)),
        temp_dir.path().to_path_buf(),
    ));
    maybe_start_background_wake(
        &mut pending,
        &mut app,
        &mut tasks,
        agent,
        Arc::new(NullSink),
        &registry,
        &terminals,
        &subagents,
        &peer_bus,
    )
    .await;

    assert!(pending.is_none());
    assert_eq!(app.task_state, TaskState::Running);
    assert!(tasks.is_busy());

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background wake run should finish");

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("wake-output")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(!registry.agent_wake_ready().await);
}

#[tokio::test]
async fn background_wake_resumes_under_the_active_persona_mode() {
    // A wake must not flip the session's persona: with the plan view active, a
    // hardcoded Coding resume turned the read-only planner into a mutating
    // coding agent, which then implemented the freshly drafted plan without
    // `/start` — bypassing phased todo seeding and canvas progress.
    let registry = Arc::new(BackgroundTaskRegistry::new());
    let temp_dir = tempfile::TempDir::new().unwrap();
    let task = registry
        .start(&shell(), "printf wake-output", temp_dir.path(), 5)
        .await
        .unwrap();
    registry.attach_tool_call(&task.id, "call-1").await.unwrap();
    registry
        .wait_for_task(&task.id, Duration::from_secs(2))
        .await
        .unwrap();

    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, _requests) = CapturingProvider::new();
    let agent = test_agent(Box::new(provider));
    agent.lock().await.set_background_tasks(registry.clone());
    let mut app = app();
    app.active_persona = crate::agent::ActivePersona::Builtin(AgentMode::Planning);
    let mut pending = Some(Instant::now() - Duration::from_millis(1));
    let subagents = Arc::new(crate::subagent::SubagentRegistry::new());
    let terminals = Arc::new(crate::terminal::TerminalRegistry::new());
    let peer_bus = Arc::new(crate::peer::PeerBus::new(
        crate::storage::Storage::open_at(temp_dir.path().join("t.db"))
            .await
            .unwrap(),
        Arc::new(tokio::sync::Mutex::new(None)),
        temp_dir.path().to_path_buf(),
    ));
    maybe_start_background_wake(
        &mut pending,
        &mut app,
        &mut tasks,
        agent.clone(),
        Arc::new(NullSink),
        &registry,
        &terminals,
        &subagents,
        &peer_bus,
    )
    .await;
    assert!(tasks.is_busy());

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("background wake run should finish");

    // `set_mode` returns None when the mode already matches: the resume must
    // have run under Planning, not silently switched the agent to Coding.
    assert!(
        agent.lock().await.set_mode(AgentMode::Planning).is_none(),
        "wake resume must preserve the active planning persona"
    );
}

#[tokio::test]
async fn implement_plan_task_runs_section_only_plan() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, requests) = CapturingProvider::new();

    tasks
        .start_implement_plan(
            test_agent(Box::new(provider)),
            section_only_plan("Saved section-only plan"),
            Arc::new(NullSink),
            None,
        )
        .expect("section-only plan should start implementation");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("implement plan run should finish");

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("The plan on the canvas is ready")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("Saved section-only plan")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("Do the work without checklist tasks.")),
        "request messages: {:?}",
        requests[0]
    );
}

#[tokio::test]
async fn commit_handoff_starts_coding_agent_with_commit_workflow_prompt() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, requests) = CapturingProvider::new();
    let agent = test_agent(Box::new(provider));
    let mut app = app();

    let started = commit_changes(&mut app, &mut tasks, agent.clone(), Arc::new(NullSink)).await;

    assert!(started, "commit command should start a focused agent run");
    assert_eq!(app.view, View::Agent);
    assert_eq!(app.task_state, TaskState::Running);
    assert_eq!(app.active_mode(), AgentMode::Coding);
    assert!(tasks.is_busy());

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("commit workflow run should finish");

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("Create a git commit")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("staged diff has changes")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("git add -A")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("Conventional Commit")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("git commit")),
        "request messages: {:?}",
        requests[0]
    );
}

#[tokio::test]
async fn init_handoff_starts_coding_agent_with_project_aware_workflow_prompt() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, requests) = CapturingProvider::new();
    let agent = test_agent(Box::new(provider));
    let mut app = app();

    let started = initialize_agents_md(&mut app, &mut tasks, agent, Arc::new(NullSink)).await;

    assert!(started, "init command should start a focused agent run");
    assert_eq!(app.view, View::Agent);
    assert_eq!(app.task_state, TaskState::Running);
    assert_eq!(app.active_mode(), AgentMode::Coding);
    assert!(tasks.is_busy());

    tokio::time::timeout(Duration::from_secs(1), async {
        while tasks.poll_finished().await.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("init workflow run should finish");

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("project-aware root `AGENTS.md`")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("exactly two choices")),
        "request messages: {:?}",
        requests[0]
    );
}

#[tokio::test]
async fn pr_handoff_starts_coding_agent_with_pr_workflow_prompt() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, requests) = CapturingProvider::new();
    let agent = test_agent(Box::new(provider));
    let mut app = app();

    let started =
        create_pull_request(&mut app, &mut tasks, agent.clone(), Arc::new(NullSink)).await;

    assert!(started, "pr command should start a focused agent run");
    assert_eq!(app.view, View::Agent);
    assert_eq!(app.task_state, TaskState::Running);
    assert_eq!(app.active_mode(), AgentMode::Coding);
    assert!(tasks.is_busy());

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("pr workflow run should finish");

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("Create or update a GitHub pull request")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("non-default feature branch")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("uncommitted changes")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("gh pr create")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("gh pr view --comments")),
        "request messages: {:?}",
        requests[0]
    );
}

#[tokio::test]
async fn test_handoff_uses_detected_profile_and_bounded_workflow_prompt() {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='verify-me'\n",
    )
    .unwrap();
    std::fs::write(root.path().join("Cargo.lock"), "").unwrap();
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, requests) = CapturingProvider::new();
    let agent = test_agent(Box::new(provider));
    let mut app = app();

    let started =
        run_test_profile(&mut app, &mut tasks, agent, Arc::new(NullSink), root.path()).await;

    assert!(started);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("verification workflow should finish");
    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    let prompt = requests[0].join("\n");
    assert!(prompt.contains("cargo test --locked"));
    assert!(prompt.contains("Run each command once"));
    assert!(prompt.contains("Before a failure, do not edit files"));
    assert!(prompt.contains("typed recovery event"));
    assert!(prompt.contains("final git status"));
}

#[tokio::test]
async fn start_handoff_seeds_visible_todos_and_sends_plan_context() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, requests) = CapturingProvider::new();
    let agent = test_agent(Box::new(provider));
    let todo_store = Arc::new(Mutex::new(crate::todo::TodoStore::new()));
    let plan_store = Arc::new(Mutex::new(sample_plan("Start handoff plan")));
    let mut app = app();

    let started = implement_plan(
        &mut app,
        &mut tasks,
        agent.clone(),
        Arc::new(NullSink),
        todo_store.clone(),
        plan_store,
    )
    .await;

    assert!(started, "non-empty plan should start implementation");
    assert_eq!(app.view, View::Agent);
    assert_eq!(
        app.current_session_summary, "Start handoff plan",
        "/start should move the plan title to the session title"
    );
    assert_eq!(app.todo.len(), 1);
    assert_eq!(app.todo[0].content, "Ship it");
    assert_eq!(app.todo[0].status, crate::todo::TodoStatus::InProgress);
    let stored_todos = todo_store.lock().await.todos().to_vec();
    assert_eq!(stored_todos, app.todo);

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("implement plan run should finish");

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("Start handoff plan")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("Ship it")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("Use todowrite explicitly")),
        "request messages: {:?}",
        requests[0]
    );
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("[in_progress] Ship it")),
        "request messages: {:?}",
        requests[0]
    );
}

#[tokio::test]
async fn start_handoff_uses_markdown_checklist_fallback_as_todos() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, requests) = CapturingProvider::new();
    let agent = test_agent(Box::new(provider));
    let todo_store = Arc::new(Mutex::new(crate::todo::TodoStore::new()));
    let mut plan = crate::plan::PlanDoc::default();
    plan.edit().set_title("Markdown checklist plan");
    plan.edit().set_section(
        "Implementation",
        "- [ ] Extract todos from markdown\n- [ ] Send prompt context",
    );
    let plan_store = Arc::new(Mutex::new(plan));
    let mut app = app();

    let started = implement_plan(
        &mut app,
        &mut tasks,
        agent.clone(),
        Arc::new(NullSink),
        todo_store.clone(),
        plan_store,
    )
    .await;

    assert!(
        started,
        "markdown-checklist plan should start implementation"
    );
    assert_eq!(app.todo.len(), 2);
    assert_eq!(app.todo[0].content, "Extract todos from markdown");
    assert_eq!(app.todo[0].status, crate::todo::TodoStatus::InProgress);
    assert_eq!(app.todo[1].content, "Send prompt context");
    assert_eq!(app.todo[1].status, crate::todo::TodoStatus::Pending);

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("implement plan run should finish");

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .iter()
            .any(|message| message.contains("[in_progress] Extract todos from markdown")),
        "request messages: {:?}",
        requests[0]
    );
}

#[tokio::test]
async fn cancel_queued_action_removes_message_from_active_task_queue() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, requests) = CapturingProvider::new();
    let agent = test_agent(Box::new(provider));
    let agent_guard = agent.lock().await;
    tasks
        .start_agent_run(
            agent.clone(),
            crate::agent::UserInput::from_text("initial"),
            Arc::new(NullSink),
            crate::agent::ActivePersona::Builtin(AgentMode::Coding),
        )
        .expect("agent run should start");

    let mut app = app();
    app.task_state = TaskState::Running;
    for (id, text) in [(1, "remove me"), (2, "keep me")] {
        tasks
            .queue_agent_message(QueuedUserMessage {
                id,
                display_text: text.to_string(),
                transcript_text: text.to_string(),
                input: crate::agent::UserInput::from_text(text),
            })
            .expect("queued message should send to active task");
        app.reduce(AppAction::SteerInput {
            id,
            text: text.to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });
    }

    apply_action_with_task_side_effects(AppAction::CancelQueuedInput { id: 1 }, &mut app, &tasks);
    drop(agent_guard);

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("agent run should finish");

    let requests = requests.lock().await;
    assert_eq!(
        requests.as_slice(),
        &[vec!["initial".to_string(), "keep me".to_string()]]
    );
    assert!(matches!(
        app.queued_inputs.as_slice(),
        [QueuedInput { id: 2, text, .. }] if text == "keep me"
    ));
}

#[tokio::test]
async fn withdraw_queued_action_cancels_and_restores_most_recent_message() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let (provider, requests) = CapturingProvider::new();
    let agent = test_agent(Box::new(provider));
    let agent_guard = agent.lock().await;
    tasks
        .start_agent_run(
            agent.clone(),
            crate::agent::UserInput::from_text("initial"),
            Arc::new(NullSink),
            crate::agent::ActivePersona::Builtin(AgentMode::Coding),
        )
        .expect("agent run should start");

    let mut app = app();
    app.task_state = TaskState::Running;
    for (id, text) in [(1, "tell me more"), (2, "keep me")] {
        tasks
            .queue_agent_message(QueuedUserMessage {
                id,
                display_text: text.to_string(),
                transcript_text: text.to_string(),
                input: crate::agent::UserInput::from_text(text),
            })
            .expect("queued message should send to active task");
        app.reduce(AppAction::SteerInput {
            id,
            text: text.to_string(),
            content: crate::tui::app::ComposerContent {
                text: text.to_string(),
                chips: Vec::new(),
            },
            mode: AgentMode::Coding,
        });
    }

    apply_action_with_task_side_effects(AppAction::WithdrawQueuedInput, &mut app, &tasks);
    drop(agent_guard);

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("agent run should finish");

    let requests = requests.lock().await;
    assert_eq!(
        requests.as_slice(),
        &[vec!["initial".to_string(), "tell me more".to_string()]]
    );
    assert_eq!(app.input(), "keep me");
    assert_eq!(app.composer.text, "keep me");
    assert!(matches!(
        app.queued_inputs.as_slice(),
        [QueuedInput { id: 1, text, .. }] if text == "tell me more"
    ));
}

#[tokio::test]
async fn pending_queued_input_dispatches_when_idle() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let mut app = app();
    app.reduce(AppAction::QueueNextInput {
        id: 3,
        text: "run me next".to_string(),
        content: crate::tui::app::ComposerContent::default(),
        mode: AgentMode::Planning,
    });
    app.reduce(AppAction::SetTaskState(TaskState::Idle));
    let mut repo_map = empty_repo_map_injector();

    start_pending_queued_run_if_idle(
        &mut app,
        &mut tasks,
        test_agent(Box::new(CompleteProvider)),
        Arc::new(NullSink),
        &mut repo_map,
        &ProviderRegistry::default_registry(),
        &test_model_catalog(),
    )
    .await;

    assert!(app.queued_inputs.is_empty());
    assert_eq!(app.task_state, TaskState::Running);
    assert!(tasks.is_busy());
    assert!(matches!(
        app.transcript.as_slice(),
        [TranscriptItem::UserMessage { text }] if text == "run me next"
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queued fallback run should finish");
}

/// Regression: the primary queued message opens the run via `begin_run`, so it
/// must be excluded from the pre-seeded queue channel — leaving it in delivered
/// it twice (duplicate transcript row + duplicate model input).
#[tokio::test]
async fn pending_queued_run_does_not_resend_primary_message() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let mut app = app();
    for (id, text) in [(1, "primary"), (2, "follow-up")] {
        app.reduce(AppAction::QueueNextInput {
            id,
            text: text.to_string(),
            content: crate::tui::app::ComposerContent {
                text: text.to_string(),
                chips: Vec::new(),
            },
            mode: AgentMode::Coding,
        });
    }
    app.reduce(AppAction::SetTaskState(TaskState::Idle));
    let mut repo_map = empty_repo_map_injector();
    let (provider, requests) = CapturingProvider::new();

    start_pending_queued_run_if_idle(
        &mut app,
        &mut tasks,
        test_agent(Box::new(provider)),
        Arc::new(NullSink),
        &mut repo_map,
        &ProviderRegistry::default_registry(),
        &test_model_catalog(),
    )
    .await;

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queued run should finish");

    let requests = requests.lock().await;
    for user_messages in requests.iter() {
        assert_eq!(
            user_messages
                .iter()
                .filter(|text| text.as_str() == "primary")
                .count(),
            1,
            "primary queued message must reach the model exactly once, got {user_messages:?}"
        );
    }
}

#[tokio::test]
async fn pending_steer_runs_before_and_separately_from_enter_queue() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let mut app = app();
    app.reduce(AppAction::QueueNextInput {
        id: 1,
        text: "later work".to_string(),
        content: crate::tui::app::ComposerContent {
            text: "later work".to_string(),
            chips: Vec::new(),
        },
        mode: AgentMode::Coding,
    });
    app.reduce(AppAction::SteerInput {
        id: 2,
        text: "urgent correction".to_string(),
        content: crate::tui::app::ComposerContent {
            text: "urgent correction".to_string(),
            chips: Vec::new(),
        },
        mode: AgentMode::Coding,
    });
    app.reduce(AppAction::SetTaskState(TaskState::Idle));
    let mut repo_map = empty_repo_map_injector();
    let (provider, requests) = CapturingProvider::new();

    assert!(
        start_pending_queued_run_if_idle(
            &mut app,
            &mut tasks,
            test_agent(Box::new(provider)),
            Arc::new(NullSink),
            &mut repo_map,
            &ProviderRegistry::default_registry(),
            &test_model_catalog(),
        )
        .await
    );

    assert!(matches!(
        app.queued_inputs.as_slice(),
        [QueuedInput {
            id: 1,
            text,
            delivery: FollowUpDelivery::Queue,
            ..
        }] if text == "later work"
    ));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("steer run should finish");
    assert_eq!(
        requests.lock().await.as_slice(),
        &[vec!["urgent correction".to_string()]]
    );
}

#[tokio::test]
async fn pending_queued_image_is_rechecked_before_idle_run() {
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let mut composer = crate::tui::app::Composer::default();
    composer.insert_image_chip(crate::tui::image_paste::EncodedImage {
        base64: "aW1hZ2U=".to_string(),
        byte_len: 5,
        width: 1,
        height: 1,
    });
    let content = composer.content();
    let display_text = content.submission().display_text;
    let mut app = app();
    app.provider = "nonvision".to_string();
    app.model = "text-only".to_string();
    app.reduce(AppAction::SteerInput {
        id: 9,
        text: display_text,
        content,
        mode: AgentMode::Coding,
    });
    app.reduce(AppAction::SetTaskState(TaskState::Idle));
    let mut repo_map = empty_repo_map_injector();
    let registry = ProviderRegistry::default_registry();
    let model_catalog = test_model_catalog();

    start_pending_queued_run_if_idle(
        &mut app,
        &mut tasks,
        test_agent(Box::new(CompleteProvider)),
        Arc::new(NullSink),
        &mut repo_map,
        &registry,
        &model_catalog,
    )
    .await;

    assert!(app.queued_inputs.is_empty());
    assert_eq!(app.task_state, TaskState::Idle);
    assert!(!tasks.is_busy());
    assert!(matches!(
        app.transcript.as_slice(),
        [TranscriptItem::CommandOutput { kind: CommandOutputKind::Error, text }]
            if text == "This model can't see images — pending image message not sent."
    ));
}

#[tokio::test]
async fn deferred_model_command_applies_after_idle_drain() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let mut session_store = SessionStore::load_with_storage(&storage).await.unwrap();
    session_store.ensure_provider("codex");
    session_store.session_mut("codex").api_key = "codex-token".to_string();
    let session_store = Arc::new(Mutex::new(session_store));
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let registry = Arc::new(ProviderRegistry::default_registry());
    let model_catalog = test_model_catalog();
    let mut app = app();
    app.model = "before-run".to_string();
    app.task_state = TaskState::Running;
    app.reduce(AppAction::QueueDeferredCommand {
        input: "/model codex:openai/gpt-5.5".to_string(),
        label: "/model codex:openai/gpt-5.5".to_string(),
    });

    assert_eq!(app.model, "before-run");
    assert_eq!(app.deferred_commands.len(), 1);

    app.task_state = TaskState::Idle;
    let started = start_next_deferred_command_if_idle(
        &mut app,
        &mut tasks,
        test_agent(Box::new(CompleteProvider)),
        session_store,
        temp_dir.path(),
        DeferredCommandDeps {
            registry,
            model_catalog,
            storage: Some(&storage),
        },
    )
    .await;

    assert!(started);
    assert!(app.deferred_commands.is_empty());
    assert_eq!(app.task_state, TaskState::Command);
    assert_eq!(app.model, "before-run");

    let event = tokio::time::timeout(Duration::from_secs(1), runtime_rx.recv())
        .await
        .expect("deferred command should send a runtime event")
        .expect("runtime event should be present");
    app.reduce(AppAction::Runtime(event));

    assert_eq!(app.task_state, TaskState::Idle);
    assert_eq!(app.provider, "codex");
    assert_eq!(app.model, "openai/gpt-5.5");
    drain_tasks(&mut tasks).await;
}

#[tokio::test]
async fn deferred_command_waits_for_fresh_plan_transition() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let plan_store: SharedPlanStore = Arc::new(Mutex::new(sample_plan("Protect this plan")));
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let agent = test_agent(Box::new(CompleteProvider));
    let mut app = app();
    app.pending_start_new_plan = true;
    app.reduce(AppAction::QueueDeferredCommand {
        input: "/new-plan".to_string(),
        label: "/new-plan".to_string(),
    });

    let started_deferred = start_next_deferred_command_if_idle(
        &mut app,
        &mut tasks,
        agent.clone(),
        Arc::new(Mutex::new(SessionStore::default())),
        temp_dir.path(),
        DeferredCommandDeps {
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            storage: Some(&storage),
        },
    )
    .await;

    assert!(!started_deferred);
    assert_eq!(app.task_state, TaskState::Idle);
    assert!(!tasks.is_busy());
    assert_eq!(app.deferred_commands.len(), 1);

    let mut repo_map = empty_repo_map_injector();
    let started_new_plan = maybe_start_new_plan(
        &mut app,
        &mut tasks,
        agent,
        Arc::new(NullSink),
        &mut repo_map,
        &storage,
        session_id,
        plan_store,
    )
    .await;

    assert!(started_new_plan);
    assert_eq!(
        storage
            .saved_plans_for_project(temp_dir.path(), 10)
            .await
            .unwrap()[0]
            .title,
        "Protect this plan"
    );
}

#[tokio::test]
async fn deferred_model_selection_preserves_reasoning_after_idle_drain() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let mut session_store = SessionStore::load_with_storage(&storage).await.unwrap();
    session_store.ensure_provider("codex");
    session_store.session_mut("codex").api_key = "codex-token".to_string();
    let session_store = Arc::new(Mutex::new(session_store));
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let registry = Arc::new(ProviderRegistry::default_registry());
    let model_catalog = test_model_catalog();
    let mut app = app();
    app.reasoning = ReasoningSelection::default();
    app.reduce(AppAction::QueueDeferredModelSelection {
        input: "/model codex:gpt-5.5".to_string(),
        label: "/model codex:gpt-5.5".to_string(),
        selection: ModelSelection {
            provider_id: "codex".to_string(),
            connection_id: "codex".to_string(),
            model_id: Some("openai/gpt-5.5".to_string()),
            model: "gpt-5.5".to_string(),
            selection_input: Some("codex:gpt-5.5".to_string()),
            reasoning: ReasoningSelection::High,
        },
    });
    app.reduce(AppAction::SetTaskState(TaskState::Idle));

    let started = start_next_deferred_command_if_idle(
        &mut app,
        &mut tasks,
        test_agent(Box::new(CompleteProvider)),
        session_store,
        temp_dir.path(),
        DeferredCommandDeps {
            registry,
            model_catalog,
            storage: Some(&storage),
        },
    )
    .await;

    assert!(started);
    assert!(app.deferred_commands.is_empty());
    assert_eq!(app.task_state, TaskState::Command);
    assert!(!tasks.is_busy());

    let event = tokio::time::timeout(Duration::from_secs(1), runtime_rx.recv())
        .await
        .expect("deferred model selection should send a runtime event")
        .expect("runtime event should be present");
    app.reduce(AppAction::Runtime(event));

    assert_eq!(app.task_state, TaskState::Idle);
    assert_eq!(app.provider, "codex");
    assert_eq!(app.model, "openai/gpt-5.5");
    assert_eq!(app.reasoning, ReasoningSelection::High);
}

#[tokio::test]
async fn command_provider_selection_persists_active_session_model() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "old-model",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut app = app();
    let event = RuntimeEvent::CommandFinished(Box::new(CommandOutcomeEvent::Applied {
        generation: None,
        clear_transcript: false,
        messages: Vec::new(),
        provider: Some(Box::new(ProviderRunSelection {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            reasoning: ReasoningSelection::High,
        })),
        context_report: None,
        quit: false,
        open_modal: None,
    }));

    let selection = provider_selection_to_persist(&app, &event);
    app.reduce(AppAction::Runtime(event));
    persist_provider_selection(&storage, session_id, selection).await;

    let summary = storage.session_summary(session_id).await.unwrap().unwrap();
    assert_eq!(summary.provider_id, "anthropic");
    assert_eq!(summary.model, "claude-sonnet-4-5");
    assert_eq!(summary.reasoning, ReasoningSelection::High);
}

#[tokio::test]
async fn stale_provider_selection_does_not_persist_active_session_model() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "old-model",
            ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut app = app();
    let stale_generation = app.command_generation().wrapping_add(1);
    let event = RuntimeEvent::CommandFinished(Box::new(CommandOutcomeEvent::Applied {
        generation: Some(stale_generation),
        clear_transcript: false,
        messages: Vec::new(),
        provider: Some(Box::new(ProviderRunSelection {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-5".to_string(),
            reasoning: ReasoningSelection::High,
        })),
        context_report: None,
        quit: false,
        open_modal: None,
    }));

    let selection = provider_selection_to_persist(&app, &event);
    app.reduce(AppAction::Runtime(event));
    persist_provider_selection(&storage, session_id, selection).await;

    let summary = storage.session_summary(session_id).await.unwrap().unwrap();
    assert_eq!(summary.provider_id, "codex");
    assert_eq!(summary.model, "old-model");
    assert_eq!(summary.reasoning, ReasoningSelection::default());
}

#[tokio::test]
async fn deferred_command_drains_before_queued_user_message() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let mut app = app();
    app.reduce(AppAction::QueueDeferredCommand {
        input: "/model codex:openai/gpt-5.5".to_string(),
        label: "/model codex:openai/gpt-5.5".to_string(),
    });
    app.reduce(AppAction::SteerInput {
        id: 7,
        text: "queued user prompt".to_string(),
        content: crate::tui::app::ComposerContent::default(),
        mode: AgentMode::Coding,
    });
    app.reduce(AppAction::SetTaskState(TaskState::Idle));

    let started = start_next_deferred_command_if_idle(
        &mut app,
        &mut tasks,
        test_agent(Box::new(CompleteProvider)),
        Arc::new(Mutex::new(authorized_codex_store())),
        temp_dir.path(),
        DeferredCommandDeps {
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            storage: None,
        },
    )
    .await;

    assert!(started);
    assert_eq!(app.task_state, TaskState::Command);
    assert!(tasks.is_busy());
    assert!(matches!(
        app.queued_inputs.as_slice(),
        [QueuedInput { id: 7, text, .. }] if text == "queued user prompt"
    ));
    drain_tasks(&mut tasks).await;
}

#[tokio::test]
async fn deferred_command_drains_before_phase_auto_advance() {
    use crate::tui::app::{PhaseAdvance, PlanExecution};

    let temp_dir = tempfile::TempDir::new().unwrap();
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let mut app = app();
    app.plan = two_phase_plan();
    app.plan_execution = Some(PlanExecution { phase_index: 0 });
    app.phase_advance = Some(PhaseAdvance::Continue);
    app.reduce(AppAction::QueueDeferredCommand {
        input: "/model codex:openai/gpt-5.5".to_string(),
        label: "/model codex:openai/gpt-5.5".to_string(),
    });
    app.reduce(AppAction::SetTaskState(TaskState::Idle));

    let started = start_next_deferred_command_if_idle(
        &mut app,
        &mut tasks,
        test_agent(Box::new(CompleteProvider)),
        Arc::new(Mutex::new(authorized_codex_store())),
        temp_dir.path(),
        DeferredCommandDeps {
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            storage: None,
        },
    )
    .await;

    assert!(started);
    assert_eq!(app.task_state, TaskState::Command);
    assert_eq!(app.phase_advance, Some(PhaseAdvance::Continue));
    assert_eq!(app.plan_execution, Some(PlanExecution { phase_index: 0 }));
    drain_tasks(&mut tasks).await;
}

#[tokio::test]
async fn authorize_command_task_opens_provider_picker() {
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let registry = Arc::new(ProviderRegistry::default_registry());
    let mut session = SessionStore::default();
    for factory in registry.all() {
        session.ensure_provider(&factory.metadata().id);
    }
    let session_store = Arc::new(Mutex::new(session));
    let temp_dir = tempfile::TempDir::new().expect("temp dir should be created");

    tasks
        .start_command(
            "/authorize".to_string(),
            Vec::new(),
            0,
            CommandTaskDeps::new(
                test_agent(Box::new(CompleteProvider)),
                session_store,
                temp_dir.path().to_path_buf(),
                registry,
                Arc::new(crate::model_catalog::ModelCatalog::load_builtin().unwrap()),
                None,
                crate::session::CredentialPersistence::File,
                Some(crate::agent::AgentMode::Coding),
            ),
        )
        .expect("command should start");

    let event = tokio::time::timeout(Duration::from_secs(1), runtime_rx.recv())
        .await
        .expect("command should produce an event")
        .expect("runtime event should be present");
    let RuntimeEvent::CommandFinished(event) = event else {
        panic!("expected authorize provider picker event");
    };
    let CommandOutcomeEvent::Applied {
        open_modal:
            Some(ModalKind::Picker(crate::tui::event::PickerModal::AuthorizeProviderPicker {
                providers,
                cursor: 0,
                ..
            })),
        ..
    } = *event
    else {
        panic!("expected authorize provider picker event");
    };
    assert!(
        providers
            .iter()
            .any(|provider| provider.provider_id == "codex")
    );
}

#[tokio::test]
async fn authorize_command_task_opens_key_prompt_without_status_message() {
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let registry = Arc::new(ProviderRegistry::default_registry());
    let mut session = SessionStore::default();
    for factory in registry.all() {
        session.ensure_provider(&factory.metadata().id);
    }
    let session_store = Arc::new(Mutex::new(session));
    let temp_dir = tempfile::TempDir::new().expect("temp dir should be created");

    tasks
        .start_command(
            "/authorize opencode".to_string(),
            Vec::new(),
            0,
            CommandTaskDeps::new(
                test_agent(Box::new(CompleteProvider)),
                session_store,
                temp_dir.path().to_path_buf(),
                registry,
                Arc::new(crate::model_catalog::ModelCatalog::load_builtin().unwrap()),
                None,
                crate::session::CredentialPersistence::File,
                Some(crate::agent::AgentMode::Coding),
            ),
        )
        .expect("command should start");

    let event = tokio::time::timeout(Duration::from_secs(1), runtime_rx.recv())
        .await
        .expect("command should produce an event")
        .expect("runtime event should be present");
    let RuntimeEvent::CommandFinished(event) = event else {
        panic!("expected authorize key prompt event");
    };
    let CommandOutcomeEvent::Applied {
        messages,
        open_modal:
            Some(ModalKind::Detail(crate::tui::event::DetailModal::ApiKeyPrompt {
                provider_id,
                initial_form: Some(initial_form),
            })),
        ..
    } = *event
    else {
        panic!("expected authorize key prompt event");
    };
    assert_eq!(provider_id, "opencode");
    assert!(initial_form.origins.is_empty());
    assert!(messages.is_empty());
}

#[tokio::test]
async fn perf_command_task_opens_report_modal_without_status_message() {
    let (runtime_tx, mut runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx);
    let registry = Arc::new(ProviderRegistry::default_registry());
    let mut store = authorized_codex_store();
    store.set_current_kind_id("codex");
    let session_store = Arc::new(Mutex::new(store));
    let temp_dir = tempfile::TempDir::new().expect("temp dir should be created");

    tasks
        .start_command(
            "/perf".to_string(),
            Vec::new(),
            0,
            CommandTaskDeps::new(
                test_agent(Box::new(CompleteProvider)),
                session_store,
                temp_dir.path().to_path_buf(),
                registry,
                Arc::new(crate::model_catalog::ModelCatalog::load_builtin().unwrap()),
                None,
                crate::session::CredentialPersistence::File,
                Some(crate::agent::AgentMode::Coding),
            ),
        )
        .expect("command should start");

    let event = tokio::time::timeout(Duration::from_secs(1), runtime_rx.recv())
        .await
        .expect("command should produce an event")
        .expect("runtime event should be present");
    let RuntimeEvent::CommandFinished(event) = event else {
        panic!("expected performance report event");
    };
    let CommandOutcomeEvent::Applied {
        messages,
        open_modal:
            Some(ModalKind::Detail(crate::tui::event::DetailModal::PerfReport { title, lines })),
        ..
    } = *event
    else {
        panic!("expected performance report modal");
    };
    assert!(messages.is_empty());
    assert_eq!(title, "Performance");
    assert!(
        lines
            .iter()
            .any(|line| line.contains("Performance: last model call"))
    );
    assert!(lines.iter().any(|line| line.contains("Usage: session")));
}

#[tokio::test]
async fn sessions_command_opens_project_picker_empty_state() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let registry = Arc::new(ProviderRegistry::default_registry());
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let current_session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "test-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let active_session_id = Arc::new(Mutex::new(Some(current_session_id)));
    let mut current_session_id_mut = current_session_id;
    let mut signatures = PersistedSnapshotSignatures {
        transcript: 0,
        plan: 0,
        todo: 0,
        context: 0,
        context_controls: 0,
        usage: 0,
        ..Default::default()
    };
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id_mut,
        signatures: &mut signatures,
    };
    let mut app = app();

    apply_persistence_command(
        PersistenceCommand::Sessions,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store,
            registry,
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id,
        },
        &mut state,
    )
    .await
    .unwrap();

    assert!(matches!(
        app.modal,
        Some(ModalKind::Picker(crate::tui::event::PickerModal::SessionPicker { ref sessions, cursor: 0 })) if sessions.is_empty()
    ));
}

#[tokio::test]
async fn save_and_plans_commands_persist_and_open_project_picker() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let registry = Arc::new(ProviderRegistry::default_registry());
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let current_session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "test-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let active_session_id = Arc::new(Mutex::new(Some(current_session_id)));
    let mut current_session_id_mut = current_session_id;
    let mut signatures = PersistedSnapshotSignatures {
        transcript: 0,
        plan: 0,
        todo: 0,
        context: 0,
        context_controls: 0,
        usage: 0,
        ..Default::default()
    };
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id_mut,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.plan = sample_plan("Plan library");

    apply_persistence_command(
        PersistenceCommand::SavePlan,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: session_store.clone(),
            registry: registry.clone(),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: active_session_id.clone(),
        },
        &mut state,
    )
    .await
    .unwrap();

    let saved_id = app
        .active_saved_plan_session_id
        .expect("saving should bind the canvas to the new library entry");
    assert_eq!(
        storage
            .load_saved_plan(saved_id)
            .await
            .unwrap()
            .expect("plan should be saved")
            .plan
            .title,
        "Plan library"
    );

    apply_persistence_command(
        PersistenceCommand::Plans,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store,
            registry,
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id,
        },
        &mut state,
    )
    .await
    .unwrap();

    assert!(matches!(
        app.modal,
        Some(ModalKind::Picker(crate::tui::event::PickerModal::PlanPicker { ref plans, cursor: 0, .. }))
            if plans.len() == 1 && plans[0].title == "Plan library"
    ));
}

#[tokio::test]
async fn save_command_rejects_untitled_or_empty_plans() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let current_session_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "test-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let mut current_session_id_mut = current_session_id;
    let mut signatures = PersistedSnapshotSignatures {
        transcript: 0,
        plan: 0,
        todo: 0,
        context: 0,
        context_controls: 0,
        usage: 0,
        ..Default::default()
    };
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id_mut,
        signatures: &mut signatures,
    };
    let mut app = app();

    let err = apply_persistence_command(
        PersistenceCommand::SavePlan,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(SessionStore::default())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_session_id))),
        },
        &mut state,
    )
    .await
    .unwrap_err();

    assert!(err.to_string().contains("untitled plan"));
}

#[tokio::test]
async fn export_command_writes_plan_markdown_to_relative_path() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, current_session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut current_session_id_mut = current_session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id_mut,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.view = View::Plan;
    app.plan = sample_plan("Exported plan");

    assert_eq!(
        persistence_command("/export plans/current.md"),
        Some(PersistenceCommand::ExportPlan("plans/current.md"))
    );
    apply_persistence_command(
        PersistenceCommand::ExportPlan("plans/current.md"),
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(SessionStore::default())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(current_session_id))),
        },
        &mut state,
    )
    .await
    .unwrap();

    let exported = tokio::fs::read_to_string(temp_dir.path().join("plans/current.md"))
        .await
        .unwrap();
    assert_eq!(exported, format!("{}\n", app.plan.to_markdown()));
    assert!(app.transcript.is_empty());
    assert_eq!(
        app.session_toast.as_ref().map(|toast| toast.text.as_str()),
        Some("Exported plan to plans/current.md.")
    );
    assert_eq!(app.active_saved_plan_session_id, None);
}

#[tokio::test]
async fn isolated_export_writes_to_workspace_but_uses_source_session_identity() {
    let root = tempfile::TempDir::new().unwrap();
    let source_root = root.path().join("source");
    let workspace_root = root.path().join("worktree");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::create_dir_all(&workspace_root).unwrap();
    let (storage, current_session_id) = storage_with_active_session(&source_root).await;
    let mut current_session_id_mut = current_session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id_mut,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.view = View::Plan;
    app.plan = sample_plan("Isolated export");
    app.project_root = workspace_root.clone();

    apply_persistence_command(
        PersistenceCommand::ExportPlan("plans/current.md"),
        &mut app,
        persistence_deps(&storage, &source_root, current_session_id),
        &mut state,
    )
    .await
    .unwrap();

    assert!(workspace_root.join("plans/current.md").is_file());
    assert!(!source_root.join("plans/current.md").exists());
    assert_eq!(
        storage
            .session_summary(current_session_id)
            .await
            .unwrap()
            .unwrap()
            .project_path,
        crate::storage::canonical_project_path(&source_root)
    );
}

#[tokio::test]
async fn export_command_rejects_unsafe_or_existing_targets() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, current_session_id) = storage_with_active_session(temp_dir.path()).await;
    tokio::fs::write(temp_dir.path().join("existing.md"), "keep")
        .await
        .unwrap();
    let mut current_session_id_mut = current_session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id_mut,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.view = View::Plan;
    app.plan = sample_plan("Export guards");

    let err = apply_persistence_command(
        PersistenceCommand::ExportPlan(""),
        &mut app,
        persistence_deps(&storage, temp_dir.path(), current_session_id),
        &mut state,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("Usage: /export <path>"));

    let err = apply_persistence_command(
        PersistenceCommand::ExportPlan("../outside.md"),
        &mut app,
        persistence_deps(&storage, temp_dir.path(), current_session_id),
        &mut state,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("inside the workdir"));

    let absolute = temp_dir.path().join("absolute.md").display().to_string();
    let err = apply_persistence_command(
        PersistenceCommand::ExportPlan(&absolute),
        &mut app,
        persistence_deps(&storage, temp_dir.path(), current_session_id),
        &mut state,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("relative to the workdir"));

    let err = apply_persistence_command(
        PersistenceCommand::ExportPlan("existing.md"),
        &mut app,
        persistence_deps(&storage, temp_dir.path(), current_session_id),
        &mut state,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("already exists"));
    assert_eq!(
        tokio::fs::read_to_string(temp_dir.path().join("existing.md"))
            .await
            .unwrap(),
        "keep"
    );

    #[cfg(unix)]
    {
        let outside = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), temp_dir.path().join("linked")).unwrap();
        let err = apply_persistence_command(
            PersistenceCommand::ExportPlan("linked/plan.md"),
            &mut app,
            persistence_deps(&storage, temp_dir.path(), current_session_id),
            &mut state,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("inside the workdir"));
    }
}

#[tokio::test]
async fn export_command_requires_plan_view_and_non_empty_plan() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, current_session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut current_session_id_mut = current_session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id_mut,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.plan = sample_plan("Wrong view");

    let err = apply_persistence_command(
        PersistenceCommand::ExportPlan("plan.md"),
        &mut app,
        persistence_deps(&storage, temp_dir.path(), current_session_id),
        &mut state,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("Switch to Plan view"));

    app.view = View::Plan;
    app.plan = crate::plan::PlanDoc::default();
    let err = apply_persistence_command(
        PersistenceCommand::ExportPlan("plan.md"),
        &mut app,
        persistence_deps(&storage, temp_dir.path(), current_session_id),
        &mut state,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("empty plan"));
}

#[tokio::test]
async fn resume_without_argument_restores_latest_prior_project_session() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let registry = Arc::new(ProviderRegistry::default_registry());
    let mut store = SessionStore::default();
    store.ensure_provider("codex");
    store.session_mut("codex").api_key = "codex-token".to_string();
    store.session_mut("codex").account_id = "codex-account".to_string();
    let session_store = Arc::new(Mutex::new(store));
    let prior_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "prior-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .replace_transcript_snapshot(
            prior_id,
            &[TranscriptItem::UserMessage {
                text: "restore me".to_string(),
            }],
        )
        .await
        .unwrap();
    let prior_verification = sample_verification_run(1_700_000_001_000, "cargo test --workspace");
    let prior_review = sample_self_review_run(1_700_000_001_100, 8);
    storage
        .replace_verification_runs_snapshot(prior_id, std::slice::from_ref(&prior_verification))
        .await
        .unwrap();
    storage
        .replace_self_review_runs_snapshot(prior_id, std::slice::from_ref(&prior_review))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "current-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let active_session_id = Arc::new(Mutex::new(Some(current_id)));
    let mut current_session_id = current_id;
    let mut signatures = PersistedSnapshotSignatures {
        transcript: 0,
        plan: 0,
        todo: 0,
        context: 0,
        context_controls: 0,
        usage: 42,
        verification: 0,
        self_review: 0,
        episodes: 0,
    };
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    let agent = test_agent(Box::new(CompleteProvider));
    let outgoing_verification = sample_verification_run(1_700_000_002_000, "cargo test --locked");
    let outgoing_review = sample_self_review_run(1_700_000_002_100, 4);
    {
        let mut agent = agent.lock().await;
        agent.restore_verification_runs(vec![outgoing_verification.clone()]);
        agent.restore_self_review_runs(vec![outgoing_review.clone()]);
    }

    apply_persistence_command(
        PersistenceCommand::Resume(""),
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: agent.clone(),
            memory: None,
            session_store,
            registry,
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: active_session_id.clone(),
        },
        &mut state,
    )
    .await
    .unwrap();

    assert_eq!(*state.current_session_id, prior_id);
    assert_eq!(*active_session_id.lock().await, Some(prior_id));
    assert_eq!(
        storage
            .session_summary(current_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
    assert!(storage.is_session_live(prior_id).await.unwrap());
    assert_eq!(state.signatures.usage, 0);
    assert_eq!(app.model, "prior-model");
    assert!(app.transcript.iter().any(|item| {
        matches!(item, TranscriptItem::UserMessage { text } if text == "restore me")
    }));
    {
        let agent = agent.lock().await;
        assert_eq!(
            agent.verification_runs(),
            std::slice::from_ref(&prior_verification)
        );
        assert_eq!(
            agent.self_review_runs(),
            std::slice::from_ref(&prior_review)
        );
    }
    persist_changed_snapshots(
        &storage,
        *state.current_session_id,
        &mut app,
        agent.clone(),
        state.signatures,
    )
    .await;
    let resumed = storage
        .load_session_snapshot(prior_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.verification_runs, [prior_verification]);
    assert_eq!(resumed.self_review_runs, [prior_review]);
    let outgoing = storage
        .load_session_snapshot(current_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outgoing.verification_runs, [outgoing_verification]);
    assert_eq!(outgoing.self_review_runs, [outgoing_review]);
    // The resume confirmation is now an ephemeral toast, not a persisted
    // transcript row — so resumes don't accumulate "Resumed session #N" lines.
    assert_eq!(
        app.session_toast.as_ref().map(|toast| toast.text.as_str()),
        Some(format!("Resumed session #{prior_id}.").as_str())
    );
    assert!(!app.transcript.iter().any(|item| {
        matches!(
            item,
            TranscriptItem::CommandOutput {
                kind: CommandOutputKind::Status,
                text,
            } if text == &format!("Resumed session #{prior_id}.")
        )
    }));
}

#[tokio::test]
async fn resume_uses_pinned_provider_for_first_coding_request() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let model_catalog = test_model_catalog();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(ProviderRegistry::new(vec![
        Arc::new(RecordingProviderFactory::new("opencode", requests.clone())),
        Arc::new(RecordingProviderFactory::new("codex", requests.clone())),
    ]));

    let target_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "openai/gpt-5.5",
            ReasoningSelection::High,
        )
        .await
        .unwrap();
    storage
        .mark_session_status(target_id, SessionStatus::Completed)
        .await
        .unwrap();
    let target_cache_key = storage.conversation_cache_key(target_id).await.unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "opencode",
            "opencode/qwen3.7-max",
            ReasoningSelection::Default,
        )
        .await
        .unwrap();

    let mut global_store =
        SessionStore::load_with_storage_and_catalog(&storage, Some(&model_catalog))
            .await
            .unwrap();
    global_store.ensure_provider("opencode");
    global_store.ensure_provider("codex");
    global_store.session_mut("opencode").model = "opencode/qwen3.7-max".to_string();
    global_store.set_current_kind_id("opencode");
    global_store
        .mode_models
        .insert("coding".to_string(), "opencode:qwen3.7-max".to_string());
    global_store.save_async().await.unwrap();
    let session_store = Arc::new(Mutex::new(global_store));
    let agent = test_agent(Box::new(CompleteProvider));
    let active_session_id = Arc::new(Mutex::new(Some(current_id)));
    let mut current_session_id = current_id;
    let mut signatures = PersistedSnapshotSignatures::default();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = AppState::new(
        "opencode",
        "opencode/qwen3.7-max".to_string(),
        "workspace".to_string(),
        None,
    );

    resume_session(
        target_id,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: agent.clone(),
            memory: None,
            session_store: session_store.clone(),
            registry: registry.clone(),
            model_catalog: model_catalog.clone(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id,
        },
        &mut state,
    )
    .await
    .unwrap();

    assert_eq!(app.provider, "codex");
    assert_eq!(app.model, "openai/gpt-5.5");
    assert_eq!(app.reasoning, ReasoningSelection::High);
    let expected_context_window = {
        let resumed_store = session_store.lock().await;
        assert_eq!(resumed_store.current_kind_id(), "codex");
        assert_eq!(resumed_store.current_session().model, "openai/gpt-5.5");
        assert_eq!(
            resumed_store.current_session().reasoning,
            ReasoningSelection::High
        );
        assert!(resumed_store.mode_model("coding").is_none());
        crate::model_resolution::context_window_for_current_model_with_catalog(
            &registry,
            &resumed_store,
            Some(&model_catalog),
        )
    };
    let resumed_report = agent.lock().await.context_report();
    assert_eq!(
        resumed_report.budget_tokens,
        expected_context_window as usize
    );

    let (runtime_sender, _runtime_receiver) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_sender);
    tasks.set_persona_model_deps(crate::tui::task::PersonaModelDeps {
        agent: agent.clone(),
        session_store: session_store.clone(),
        registry,
        model_catalog: model_catalog.clone(),
        custom_agents: crate::resource::agent::shared_registry(
            crate::resource::agent::AgentRegistry::empty(),
        ),
    });
    tasks
        .start_agent_run(
            agent,
            crate::agent::UserInput::from_text("continue the pinned session"),
            Arc::new(NullSink),
            crate::agent::ActivePersona::Builtin(AgentMode::Coding),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if tasks.poll_finished().await.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("resumed turn should finish");

    let first = requests
        .lock()
        .await
        .first()
        .cloned()
        .expect("first provider request should run");
    assert_eq!(first.provider_id, "codex");
    assert_eq!(first.model, "gpt-5.5");
    assert_eq!(first.reasoning, ReasoningSelection::High);
    assert_eq!(first.context_window, Some(expected_context_window));
    assert_eq!(first.conversation_cache_key, target_cache_key);

    let persisted_target = storage
        .session_summary(target_id)
        .await
        .unwrap()
        .expect("resumed session should remain persisted");
    assert_eq!(persisted_target.provider_id, "codex");
    assert_eq!(persisted_target.model, "openai/gpt-5.5");
    assert_eq!(persisted_target.reasoning, ReasoningSelection::High);
    let global_default =
        SessionStore::load_with_storage_and_catalog(&storage, Some(&model_catalog))
            .await
            .unwrap();
    assert_eq!(global_default.current_kind_id(), "opencode");
}

#[tokio::test]
async fn resume_requires_current_global_provider_authorization() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let registry = Arc::new(ProviderRegistry::default_registry());
    let session_store = Arc::new(Mutex::new(SessionStore::default()));
    let prior_id = storage
        .start_session(
            temp_dir.path(),
            "minimax-coding-plan",
            "MiniMax-M3",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2)).await;
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "current-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let active_session_id = Arc::new(Mutex::new(Some(current_id)));
    let mut current_session_id = current_id;
    let mut signatures = PersistedSnapshotSignatures {
        transcript: 0,
        plan: 0,
        todo: 0,
        context: 0,
        context_controls: 0,
        usage: 0,
        ..Default::default()
    };
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    let prior_arg = prior_id.to_string();

    apply_persistence_command(
        PersistenceCommand::Resume(&prior_arg),
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store,
            registry,
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: active_session_id.clone(),
        },
        &mut state,
    )
    .await
    .unwrap();

    assert_eq!(*state.current_session_id, current_id);
    assert_eq!(*active_session_id.lock().await, Some(current_id));
    assert!(app.transcript.iter().any(|item| {
        matches!(
            item,
            TranscriptItem::CommandOutput {
                kind: CommandOutputKind::Error,
                text,
            } if text.contains("Authorize MiniMax Coding Plan before resuming session")
        )
    }));
}

#[tokio::test]
async fn clean_saved_plan_open_creates_new_empty_session_with_loaded_plan() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let source_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "test-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let plan = sample_plan("Clean open plan");
    let saved = storage
        .save_plan_to_library(source_id, None, &plan, Some("main"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "test-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let active_session_id = Arc::new(Mutex::new(Some(current_id)));
    let plan_store = Arc::new(Mutex::new(crate::plan::PlanDoc::default()));
    let mut current_session_id = current_id;
    let mut signatures = PersistedSnapshotSignatures {
        transcript: 99,
        plan: 99,
        todo: 99,
        context: 99,
        context_controls: 99,
        usage: 99,
        verification: 99,
        self_review: 99,
        episodes: 99,
    };
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.transcript.push(TranscriptItem::UserMessage {
        text: "/plans".to_string(),
    });

    open_saved_plan_clean(
        saved.id,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(SessionStore::default())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: plan_store.clone(),
            project_root: temp_dir.path(),
            active_session_id: active_session_id.clone(),
        },
        &mut state,
    )
    .await
    .unwrap();

    assert_ne!(*state.current_session_id, current_id);
    assert_eq!(
        *active_session_id.lock().await,
        Some(*state.current_session_id)
    );
    assert_eq!(app.active_saved_plan_session_id, Some(saved.id));
    assert_eq!(app.view, View::Plan);
    assert!(app.transcript.is_empty());
    assert_eq!(app.plan.title, "Clean open plan");
    assert_eq!(plan_store.lock().await.title, "Clean open plan");
    let opened = storage
        .load_session_snapshot(*state.current_session_id)
        .await
        .unwrap()
        .expect("clean-open session should persist");
    assert_eq!(opened.summary.source_plan_id, Some(saved.id));
    assert_eq!(state.signatures.transcript, 0);
}

#[tokio::test]
async fn resume_session_restores_active_saved_plan_association() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let registry = Arc::new(ProviderRegistry::default_registry());
    let session_store = Arc::new(Mutex::new(authorized_codex_store()));
    let plan_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "test-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let saved = storage
        .save_plan_to_library(plan_id, None, &sample_plan("Resume link"), Some("main"))
        .await
        .unwrap();
    let execution_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "test-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .set_session_source_plan(execution_id, Some(saved.id))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "current-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let active_session_id = Arc::new(Mutex::new(Some(current_id)));
    let mut current_session_id = current_id;
    let mut signatures = PersistedSnapshotSignatures {
        transcript: 0,
        plan: 0,
        todo: 0,
        context: 0,
        context_controls: 0,
        usage: 0,
        ..Default::default()
    };
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();

    resume_session(
        execution_id,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store,
            registry,
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id,
        },
        &mut state,
    )
    .await
    .unwrap();

    assert_eq!(app.active_saved_plan_session_id, Some(saved.id));
    assert_eq!(*state.current_session_id, execution_id);
}

#[tokio::test]
async fn resume_session_rejects_live_target_without_mutating_active_session() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let target_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "target-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .record_session_heartbeat(target_id, false)
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "current-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .record_session_heartbeat(current_id, false)
        .await
        .unwrap();
    let active_session_id = Arc::new(Mutex::new(Some(current_id)));
    let mut current_session_id = current_id;
    let mut signatures = PersistedSnapshotSignatures::default();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();

    resume_session(
        target_id,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store: Arc::new(Mutex::new(authorized_codex_store())),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: active_session_id.clone(),
        },
        &mut state,
    )
    .await
    .unwrap();

    assert_eq!(*state.current_session_id, current_id);
    assert_eq!(*active_session_id.lock().await, Some(current_id));
    assert_eq!(
        storage
            .session_summary(current_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Active
    );
}

#[tokio::test]
async fn resume_session_provider_preparation_failure_preserves_runtime_state() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let target_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "target-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .mark_session_status(target_id, SessionStatus::Completed)
        .await
        .unwrap();
    storage
        .set_session_run_selection(
            target_id,
            "missing-provider",
            "target-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "current-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let active_session_id = Arc::new(Mutex::new(Some(current_id)));
    let agent = test_agent(Box::new(CompleteProvider));
    agent
        .lock()
        .await
        .restore_text_history(&[("user".to_string(), "current context".to_string())])
        .await
        .unwrap();
    let current_context = agent.lock().await.context_message_snapshot();
    let session_store = Arc::new(Mutex::new(authorized_codex_store()));
    {
        let mut session = session_store.lock().await;
        session.set_current_kind_id("codex");
        session.session_mut("codex").model = "current-model".to_string();
    }
    let mut current_session_id = current_id;
    let mut signatures = PersistedSnapshotSignatures::default();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.set_session_identity(current_id, "Current".to_string(), "keep me".to_string());
    app.transcript.push(TranscriptItem::UserMessage {
        text: "current transcript".to_string(),
    });

    let error = resume_session(
        target_id,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: agent.clone(),
            memory: None,
            session_store: session_store.clone(),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: active_session_id.clone(),
        },
        &mut state,
    )
    .await
    .unwrap_err();

    assert!(format!("{error:#}").contains("Unknown provider 'missing-provider'"));
    assert_eq!(*state.current_session_id, current_id);
    assert_eq!(*active_session_id.lock().await, Some(current_id));
    assert_eq!(app.current_session_id, Some(current_id));
    assert_eq!(app.current_session_name, "Current");
    assert_eq!(app.current_session_summary, "keep me");
    assert_eq!(app.transcript.len(), 1);
    assert_eq!(
        agent.lock().await.context_message_snapshot().messages,
        current_context.messages
    );
    {
        let session = session_store.lock().await;
        assert_eq!(session.current_kind_id(), "codex");
        assert_eq!(session.current_session().model, "current-model");
    }
    assert_eq!(
        storage
            .session_summary(current_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Active
    );
    assert_eq!(
        storage
            .session_summary(target_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
}

#[tokio::test]
async fn resume_session_storage_failure_preserves_all_runtime_owners() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let database_path = temp_dir.path().join("bonsai.db");
    let storage = Storage::open_at(&database_path).await.unwrap();
    let target_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "target-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .mark_session_status(target_id, SessionStatus::Completed)
        .await
        .unwrap();
    let recovery_id = crate::storage::RecoveryId::parse("tui-atomic-resume").unwrap();
    storage
        .insert_recovery_point(crate::storage::NewRecoveryPoint {
            id: &recovery_id,
            project_path: temp_dir.path(),
            repository_path: temp_dir.path(),
            worktree_path: temp_dir.path(),
            baseline_ref: "refs/bonsai/recovery/tui-test",
            source_index_tree: "test-tree",
        })
        .await
        .unwrap();
    let failure_pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", database_path.display()))
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_tui_recovery_attachment BEFORE UPDATE OF session_id ON recovery_points \
         BEGIN SELECT RAISE(FAIL, 'injected TUI recovery failure'); END",
    )
    .execute(&failure_pool)
    .await
    .unwrap();

    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "current-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let active_session_id = Arc::new(Mutex::new(Some(current_id)));
    let agent = test_agent(Box::new(CompleteProvider));
    agent
        .lock()
        .await
        .restore_text_history(&[("user".to_string(), "current context".to_string())])
        .await
        .unwrap();
    let current_context = agent.lock().await.context_message_snapshot();
    let session_store = Arc::new(Mutex::new(authorized_codex_store()));
    {
        let mut session = session_store.lock().await;
        session.set_current_kind_id("codex");
        session.session_mut("codex").model = "current-model".to_string();
    }
    let mut current_session_id = current_id;
    let mut signatures = PersistedSnapshotSignatures::default();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.set_session_identity(current_id, "Current".to_string(), "keep me".to_string());
    app.transcript.push(TranscriptItem::UserMessage {
        text: "current transcript".to_string(),
    });

    let error = resume_session(
        target_id,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: agent.clone(),
            memory: None,
            session_store: session_store.clone(),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: active_session_id.clone(),
        },
        &mut state,
    )
    .await
    .unwrap_err();

    assert!(format!("{error:#}").contains("injected TUI recovery failure"));
    assert_eq!(*state.current_session_id, current_id);
    assert_eq!(*active_session_id.lock().await, Some(current_id));
    assert_eq!(app.current_session_id, Some(current_id));
    assert_eq!(app.current_session_name, "Current");
    assert_eq!(app.current_session_summary, "keep me");
    assert_eq!(app.transcript.len(), 1);
    assert_eq!(
        agent.lock().await.context_message_snapshot().messages,
        current_context.messages
    );
    {
        let session = session_store.lock().await;
        assert_eq!(session.current_kind_id(), "codex");
        assert_eq!(session.current_session().model, "current-model");
    }
    assert_eq!(
        storage
            .session_summary(current_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Active
    );
    assert_eq!(
        storage
            .session_summary(target_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Completed
    );
    assert_eq!(
        storage
            .recovery_point(&recovery_id)
            .await
            .unwrap()
            .session_id,
        None
    );
}

#[tokio::test]
async fn saved_plan_source_resume_restores_source_session_and_tracks_plan() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let registry = Arc::new(ProviderRegistry::default_registry());
    let session_store = Arc::new(Mutex::new(authorized_codex_store()));
    let source_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "source-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    storage
        .replace_transcript_snapshot(
            source_id,
            &[TranscriptItem::UserMessage {
                text: "source context".to_string(),
            }],
        )
        .await
        .unwrap();
    let saved = storage
        .save_plan_to_library(source_id, None, &sample_plan("Resume source"), Some("main"))
        .await
        .unwrap();
    let current_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "current-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let active_session_id = Arc::new(Mutex::new(Some(current_id)));
    let mut current_session_id = current_id;
    let mut signatures = PersistedSnapshotSignatures {
        transcript: 0,
        plan: 0,
        todo: 0,
        context: 0,
        context_controls: 0,
        usage: 0,
        ..Default::default()
    };
    let mut state = PersistenceCommandState {
        current_session_id: &mut current_session_id,
        signatures: &mut signatures,
    };
    let mut app = app();

    let saved_id = saved.id;
    open_saved_plan(
        saved,
        PlanOpenMode::ResumeSourceSession,
        &mut app,
        PersistenceCommandDeps {
            storage: &storage,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            session_store,
            registry,
            model_catalog: test_model_catalog(),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            project_root: temp_dir.path(),
            active_session_id: active_session_id.clone(),
        },
        &mut state,
    )
    .await
    .unwrap();

    assert_eq!(*state.current_session_id, source_id);
    assert_eq!(app.active_saved_plan_session_id, Some(saved_id));
    assert_eq!(app.view, View::Plan);
    assert!(app.transcript.iter().any(|item| {
        matches!(item, TranscriptItem::UserMessage { text } if text == "source context")
    }));
}

#[tokio::test]
async fn mark_started_saved_plan_links_current_execution_session() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let storage = Storage::open_at(temp_dir.path().join("bonsai.db"))
        .await
        .unwrap();
    let plan_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "test-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let execution_id = storage
        .start_session(
            temp_dir.path(),
            "codex",
            "test-model",
            crate::provider::ReasoningSelection::default(),
        )
        .await
        .unwrap();
    let saved = storage
        .save_plan_to_library(plan_id, None, &sample_plan("Start link"), Some("main"))
        .await
        .unwrap();
    let mut app = app();
    app.active_saved_plan_session_id = Some(saved.id);

    mark_started_saved_plan(&mut app, &storage, execution_id).await;

    let summary = storage
        .load_saved_plan(saved.id)
        .await
        .unwrap()
        .expect("saved plan should load")
        .summary;
    assert_eq!(summary.status, crate::storage::SavedPlanStatus::Started);
    assert_eq!(summary.execution_session_id, Some(execution_id));
    assert!(app.transcript.is_empty());
}

#[test]
fn question_move_keeps_selected_option_visible_in_compact_modal() {
    let mut app = app();
    let options = (0..16)
        .map(|idx| QuestionOption {
            label: format!("Option {idx} with a long label that wraps on compact widths"),
            description: format!(
                "Description {idx} also wraps so every choice consumes several rows"
            ),
            preselected: false,
        })
        .collect::<Vec<_>>();
    app.modal = Some(ModalKind::Detail(
        crate::tui::event::DetailModal::QuestionPrompt {
            request_id: 1,
            prompt: "Pick one".to_string(),
            header: Some("Need input".to_string()),
            options,
            multiple: false,
            origin: None,
            cursor: 0,
            selected: vec![false; 16],
        },
    ));

    for _ in 0..12 {
        app.reduce(AppAction::QuestionMove(1));
    }
    assert!(app.pending_question_visibility);

    let area = Rect::new(0, 0, 80, 18);
    clamp_scrolls(&mut app, area);

    assert!(
        app.modal_scroll > 0,
        "moving to a lower option should scroll the compact question body"
    );
    assert!(!app.pending_question_visibility);

    let Some(ModalKind::Detail(crate::tui::event::DetailModal::QuestionPrompt {
        prompt,
        origin,
        options,
        multiple,
        cursor,
        ..
    })) = app.modal.as_ref()
    else {
        panic!("question modal should still be open");
    };
    let modal_area = crate::tui::widgets::modal::question_prompt_area(area);
    let metrics = crate::tui::widgets::modal::question_prompt_metrics(
        modal_area,
        prompt,
        origin.as_deref(),
        options,
        *multiple,
        *cursor,
    );
    let visible_bottom = app.modal_scroll.saturating_add(metrics.body_height);
    assert!(
        metrics.selected_end <= visible_bottom,
        "selected option bottom should stay visible"
    );
}

#[test]
fn question_move_keeps_last_option_visible_in_tiny_modal() {
    let mut app = app();
    let options = (0..20)
        .map(|idx| QuestionOption {
            label: format!("Option {idx}"),
            description: format!("Description {idx}"),
            preselected: false,
        })
        .collect::<Vec<_>>();
    app.modal = Some(ModalKind::Detail(
        crate::tui::event::DetailModal::QuestionPrompt {
            request_id: 1,
            prompt: "Pick one".to_string(),
            header: Some("Need input".to_string()),
            options,
            multiple: false,
            origin: None,
            cursor: 0,
            selected: vec![false; 20],
        },
    ));

    // Move all the way to the last option in a very short terminal.
    for _ in 0..19 {
        app.reduce(AppAction::QuestionMove(1));
    }
    assert!(app.pending_question_visibility);

    let area = Rect::new(0, 0, 60, 10);
    clamp_scrolls(&mut app, area);

    let Some(ModalKind::Detail(crate::tui::event::DetailModal::QuestionPrompt {
        prompt,
        origin,
        options,
        multiple,
        cursor,
        ..
    })) = app.modal.as_ref()
    else {
        panic!("question modal should still be open");
    };
    let modal_area = crate::tui::widgets::modal::question_prompt_area(area);
    let metrics = crate::tui::widgets::modal::question_prompt_metrics(
        modal_area,
        prompt,
        origin.as_deref(),
        options,
        *multiple,
        *cursor,
    );
    let visible_bottom = app.modal_scroll.saturating_add(metrics.body_height);
    assert!(
        metrics.selected_end <= visible_bottom,
        "last option (end {}) should stay visible above viewport bottom {}; scroll={}",
        metrics.selected_end,
        visible_bottom,
        app.modal_scroll
    );
    assert!(
        app.modal_scroll > 0,
        "scrolling to the last option should advance modal_scroll past 0"
    );
}

#[test]
fn clamp_scrolls_keeps_context_cursor_visible_below_header_lines() {
    let mut app = app();
    app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
        Box::new(context_report_with_ledger_rows(48)),
    )));
    app.focus = Focus::Modal;
    app.context_state.cursor = 42;

    let area = Rect::new(0, 0, 90, 18);
    clamp_scrolls(&mut app, area);

    let metrics = crate::tui::widgets::modal::context_modal_metrics(&app, area)
        .expect("context modal should report metrics");
    let selected_line = metrics
        .selected_line
        .expect("context ledger row should be selected");
    let visible_bottom = app
        .modal_scroll
        .saturating_add(metrics.body_height.saturating_sub(1));
    assert!(
        selected_line >= app.modal_scroll && selected_line <= visible_bottom,
        "selected line {selected_line} should be within {}..={visible_bottom}",
        app.modal_scroll
    );
}

#[test]
fn clamp_scrolls_preserves_manual_context_modal_page_scroll() {
    let mut app = app();
    app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
        Box::new(context_report_with_ledger_rows(48)),
    )));
    app.focus = Focus::Modal;
    app.context_state.cursor = 0;

    app.reduce(AppAction::ScrollModal(8));
    let manual_scroll = app.modal_scroll;
    assert!(manual_scroll > 0, "fixture should scroll manually");

    clamp_scrolls(&mut app, Rect::new(0, 0, 90, 18));

    assert_eq!(app.modal_scroll, manual_scroll);
    assert!(app.context_state.manual_scroll);
}

#[test]
fn clamp_scrolls_clears_todo_focus_when_sidebar_is_hidden() {
    let mut app = app();
    app.focus = Focus::Todo;
    app.todo_focus_available = true;

    clamp_scrolls(&mut app, Rect::new(0, 0, 80, 24));

    assert_eq!(app.focus, Focus::Input);
    assert!(!app.todo_focus_available);
}

#[test]
fn clamp_scrolls_marks_todo_focus_available_when_sidebar_is_visible() {
    let mut app = app();

    clamp_scrolls(&mut app, Rect::new(0, 0, 120, 32));

    assert!(app.todo_focus_available);
}

#[test]
fn clamp_scrolls_consumes_todo_scroll_request_and_reaches_in_progress() {
    use crate::todo::{TodoItem, TodoStatus};

    let mut app = app();
    // A long list with the in-progress item far below the viewport.
    app.todo = (0..80)
        .map(|idx| TodoItem {
            content: format!("Task {idx}"),
            status: if idx == 60 {
                TodoStatus::InProgress
            } else {
                TodoStatus::Pending
            },
        })
        .collect();
    app.scroll_todo_in_progress_into_view = true;

    clamp_scrolls(&mut app, Rect::new(0, 0, 120, 32));

    // The request is consumed and the view scrolled down toward the item.
    assert!(!app.scroll_todo_in_progress_into_view);
    assert!(app.sidebar_scroll > 0);
    assert!(app.sidebar_scroll <= 60);
}

#[test]
fn clamp_scrolls_clears_todo_scroll_request_when_sidebar_hidden() {
    let mut app = app();
    app.scroll_todo_in_progress_into_view = true;

    // 80 cols hides the sidebar (see the focus test above).
    clamp_scrolls(&mut app, Rect::new(0, 0, 80, 24));

    // The request is dropped rather than lingering for a later frame.
    assert!(!app.scroll_todo_in_progress_into_view);
    assert_eq!(app.sidebar_scroll, 0);
}

#[test]
fn tool_detail_modal_scroll_is_clamped_to_rendered_content() {
    let mut app = app();
    let now = Instant::now();
    let result = (1..=40)
        .map(|line| format!("{line}: {}", "long wrapped output ".repeat(8)))
        .collect::<Vec<_>>()
        .join("\n");
    app.transcript
        .push(TranscriptItem::ToolActivity(ToolActivity {
            id: "call-1".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"src/main.rs"}"#.to_string(),
            delegated_model: None,
            status: ToolStatus::Succeeded,
            result: Some(result),
            diff: None,
            started_at: now,
            finished_at: Some(now),
        }));
    app.modal = Some(ModalKind::Detail(
        crate::tui::event::DetailModal::ToolDetail {
            tool_id: "call-1".to_string(),
        },
    ));

    for _ in 0..20 {
        app.reduce(AppAction::ScrollModal(8));
    }

    let area = Rect::new(0, 0, 80, 18);
    let max_scroll = crate::tui::widgets::modal::max_modal_scroll(&app, area)
        .expect("tool detail should report a max scroll");
    assert!(max_scroll > 0, "fixture should be scrollable");
    assert!(app.modal_scroll > max_scroll, "fixture should overscroll");

    clamp_scrolls(&mut app, area);

    assert_eq!(app.modal_scroll, max_scroll);
}

#[test]
fn interaction_requests_wait_while_permission_prompt_is_open() {
    let mut app = app();
    let (tx, mut rx) = mpsc::unbounded_channel();
    tx.send(InteractionRequest::Permission {
        request_id: 1,
        command: "sleep 1".to_string(),
        origin: None,
    })
    .expect("first permission request should enqueue");
    tx.send(InteractionRequest::Permission {
        request_id: 2,
        command: "sleep 2".to_string(),
        origin: None,
    })
    .expect("second permission request should enqueue");

    apply_interaction_request(&mut app, &mut rx);
    assert!(matches!(
        app.modal,
        Some(ModalKind::Detail(
            crate::tui::event::DetailModal::PermissionPrompt { request_id: 1, .. }
        ))
    ));

    apply_interaction_request(&mut app, &mut rx);
    assert!(
        matches!(
            app.modal,
            Some(ModalKind::Detail(
                crate::tui::event::DetailModal::PermissionPrompt { request_id: 1, .. }
            ))
        ),
        "an active permission prompt must not be replaced by the next queued request"
    );

    app.modal = None;
    apply_interaction_request(&mut app, &mut rx);
    assert!(matches!(
        app.modal,
        Some(ModalKind::Detail(
            crate::tui::event::DetailModal::PermissionPrompt { request_id: 2, .. }
        ))
    ));
}

#[test]
fn question_request_seeds_multi_select_checkboxes_from_preselected() {
    let mut app = app();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut checked = crate::interaction::QuestionOption::new("Doctor report", "diagnostics");
    checked.preselected = true;
    let unchecked = crate::interaction::QuestionOption::new("Log tail", "raw-ish, off by default");
    tx.send(InteractionRequest::Question {
        request_id: 9,
        prompt: "Include in the bundle?".to_string(),
        header: Some("Review".to_string()),
        options: vec![checked, unchecked],
        multiple: true,
        origin: None,
    })
    .expect("question request should enqueue");

    apply_interaction_request(&mut app, &mut rx);
    let Some(ModalKind::Detail(crate::tui::event::DetailModal::QuestionPrompt {
        selected, ..
    })) = &app.modal
    else {
        panic!("question modal should open");
    };
    assert_eq!(
        selected,
        &vec![true, false],
        "checkboxes must open in each option's declared default state"
    );
}

#[test]
fn web_domain_request_opens_prompt_and_waits_behind_an_open_modal() {
    let mut app = app();
    let (tx, mut rx) = mpsc::unbounded_channel();
    tx.send(InteractionRequest::WebDomain {
        request_id: 7,
        url: "https://example.com/page".to_string(),
        host: "example.com".to_string(),
        redirected_from: None,
        origin: None,
    })
    .expect("web domain request should enqueue");
    tx.send(InteractionRequest::WebDomain {
        request_id: 8,
        url: "https://other.test/".to_string(),
        host: "other.test".to_string(),
        redirected_from: Some("example.com".to_string()),
        origin: None,
    })
    .expect("second request should enqueue");

    apply_interaction_request(&mut app, &mut rx);
    assert!(matches!(
        app.modal,
        Some(ModalKind::Detail(
            crate::tui::event::DetailModal::WebDomainPrompt { request_id: 7, .. }
        ))
    ));

    // A second request must not clobber the open prompt.
    apply_interaction_request(&mut app, &mut rx);
    assert!(matches!(
        app.modal,
        Some(ModalKind::Detail(
            crate::tui::event::DetailModal::WebDomainPrompt { request_id: 7, .. }
        ))
    ));

    // Once answered, the redirect-hop request surfaces with its warning context.
    app.modal = None;
    apply_interaction_request(&mut app, &mut rx);
    assert!(matches!(
        &app.modal,
        Some(ModalKind::Detail(crate::tui::event::DetailModal::WebDomainPrompt { request_id: 8, redirected_from: Some(from), .. }))
            if from == "example.com"
    ));
}

#[test]
fn ui_event_drain_renders_started_tools_before_queued_finishes() {
    let mut app = app();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    tx.send(UiEvent::ToolStarted {
        id: "call-1".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"sleep 1"}"#.to_string(),
        started_at: now,
    })
    .expect("tool start should enqueue");
    tx.send(UiEvent::ToolStarted {
        id: "call-2".to_string(),
        name: "read".to_string(),
        arguments: r#"{"path":"src/main.rs"}"#.to_string(),
        started_at: now,
    })
    .expect("second tool start should enqueue");
    tx.send(UiEvent::ToolFinished {
        id: "call-1".to_string(),
        result: "ok".to_string(),
        status: crate::output::ToolExecutionStatus::Succeeded,
        finished_at: now,
    })
    .expect("tool finish should enqueue");

    let mut deferred = None;
    apply_ui_events_for_frame(&mut app, &mut rx, &mut deferred, &mut Vec::new());

    let group = app.execution_group(1).expect("group should exist");
    assert_eq!(group.tools.len(), 2);
    assert!(
        group
            .tools
            .iter()
            .all(|activity| matches!(activity.status, crate::tui::app::ToolStatus::Running)),
        "started tools should get one rendered frame before queued finishes apply"
    );
    assert!(matches!(deferred, Some(UiEvent::ToolFinished { .. })));

    apply_ui_events_for_frame(&mut app, &mut rx, &mut deferred, &mut Vec::new());
    let group = app.execution_group(1).expect("group should still exist");
    let first = group
        .tools
        .iter()
        .find(|activity| activity.id == "call-1")
        .expect("first tool should exist");
    assert!(matches!(
        first.status,
        crate::tui::app::ToolStatus::Succeeded
    ));
}

#[tokio::test]
async fn self_review_delivery_barrier_precedes_parent_finalization() {
    let mut app = app();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let now = Instant::now();
    let (barrier, marker) = crate::output::OutputDeliveryBarrier::pair();
    tx.send(UiEvent::ToolStarted {
        id: "self-review-race".to_string(),
        name: "agent".to_string(),
        arguments: r#"{"agent":"self-review"}"#.to_string(),
        started_at: now,
    })
    .expect("self-review start should enqueue");
    tx.send(UiEvent::ToolFinished {
        id: "self-review-race".to_string(),
        result: "Major: deterministic finding".to_string(),
        status: crate::output::ToolExecutionStatus::Succeeded,
        finished_at: now + Duration::from_millis(1),
    })
    .expect("self-review finish should enqueue");
    tx.send(UiEvent::OutputDeliveryBarrier(marker))
        .expect("delivery marker should enqueue");

    let wait = barrier.wait();
    tokio::pin!(wait);
    let mut deferred = None;
    apply_ui_events_for_frame(&mut app, &mut rx, &mut deferred, &mut Vec::new());

    assert!(
        tokio::time::timeout(Duration::from_millis(1), &mut wait)
            .await
            .is_err(),
        "the parent fence must stay closed while the finish event is deferred"
    );
    let activity = app
        .tool_activity("self-review-race")
        .expect("self-review card should exist");
    assert_eq!(activity.status, ToolStatus::Running);
    assert!(activity.result.is_none());

    apply_ui_events_for_frame(&mut app, &mut rx, &mut deferred, &mut Vec::new());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut wait)
            .await
            .expect("delivery barrier should be acknowledged")
    );
    let activity = app
        .tool_activity("self-review-race")
        .expect("self-review card should remain attached");
    assert_eq!(activity.status, ToolStatus::Succeeded);
    assert_eq!(
        activity.result.as_deref(),
        Some("Major: deterministic finding")
    );

    app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
        crate::tui::event::AgentRunOutcome::Completed,
    ))));
    let activity = app
        .tool_activity("self-review-race")
        .expect("finalization must not orphan the self-review card");
    assert_eq!(activity.status, ToolStatus::Succeeded);
    assert_eq!(
        activity.result.as_deref(),
        Some("Major: deterministic finding")
    );
}

/// A minimal tool whose only job is to occupy a registry slot so
/// `to_openai_tools` produces a schema and the context report's tool-schema
/// count reflects the registry size. Mirrors `MockTool` in `agent.rs`.
struct StubTool {
    name: &'static str,
}

impl StubTool {
    fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "stub tool"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::Text("ok".to_string()))
    }
}

fn stub_registry(names: &[&'static str]) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for name in names {
        registry.register(Arc::new(StubTool::new(name)));
    }
    Arc::new(registry)
}

/// Build an agent whose coding/planning registries differ in size so a
/// view change produces a visibly different tool-schema count.
fn agent_with_distinct_registries() -> Arc<Mutex<Agent>> {
    let fixture = TestFixture::new();
    Arc::new(Mutex::new(
        Agent::new(
            Box::new(CompleteProvider),
            stub_registry(&["read"]),
            stub_registry(&["read", "glob"]),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .expect("test agent should build"),
    ))
}

fn tool_schema_entry(report: &crate::agent::ContextReport) -> Option<&crate::agent::ContextEntry> {
    report
        .entries
        .iter()
        .find(|entry| entry.role == crate::agent::ContextRole::ToolSchema)
}

#[test]
fn agent_mode_for_view_maps_correctly() {
    assert_eq!(agent_mode_for_view(View::Agent), AgentMode::Coding);
    assert_eq!(agent_mode_for_view(View::Plan), AgentMode::Planning);
}

#[test]
fn toggle_view_cycles_personas_and_syncs_view() {
    // The switcher cycles the cyclable personas; `view` follows (only planning uses the
    // canvas, so every other persona maps to the agent view).
    let mut app = app();
    assert_eq!(app.view, View::Agent);
    assert_eq!(app.active_mode(), AgentMode::Coding);

    app.reduce(AppAction::ToggleView);
    assert_eq!(app.active_mode(), AgentMode::Planning);
    assert_eq!(app.view, View::Plan);

    app.reduce(AppAction::ToggleView);
    assert_eq!(app.active_mode(), AgentMode::Coding);
    assert_eq!(app.view, View::Agent);
}

#[test]
fn set_view_syncs_active_mode() {
    let mut app = app();
    app.reduce(AppAction::SetView(View::Plan));
    assert_eq!(app.active_mode(), AgentMode::Planning);

    app.reduce(AppAction::SetView(View::Agent));
    assert_eq!(app.active_mode(), AgentMode::Coding);
}

fn review_finding(issue: &str) -> crate::plan::Finding {
    crate::plan::Finding {
        severity: crate::plan::Severity::Major,
        file: Some("src/foo.rs".to_string()),
        line: Some(42),
        issue: issue.to_string(),
        required_fix: "fix it".to_string(),
        acceptance_tests: vec!["test passes".to_string()],
        source_ids: vec!["call-1".to_string()],
        task: None,
        resolved: false,
    }
}

#[test]
fn sync_plan_store_snapshot_auto_opens_new_review_findings() {
    let mut app = app();
    app.view = View::Agent;
    app.focus = Focus::Input;
    app.active_persona = crate::agent::ActivePersona::Builtin(AgentMode::Review);
    app.task_state = TaskState::Running;

    let mut plan = app.plan.clone();
    plan.edit().add_finding(review_finding("bug"));

    sync_plan_store_snapshot(&mut app, &plan, Some(AgentMode::Review));

    assert_eq!(app.view, View::Plan);
    assert_eq!(app.focus, Focus::Plan);
    assert_eq!(app.active_mode(), AgentMode::Planning);
    assert_eq!(app.task_state, TaskState::Running);
    assert_eq!(app.plan.findings.len(), 1);
    assert_eq!(app.plan_scroll, u16::MAX);
}

#[test]
fn auto_opened_findings_keep_review_run_busy_while_planning_is_next_idle_mode() {
    let mut app = app();
    app.view = View::Agent;
    app.active_persona = crate::agent::ActivePersona::Builtin(AgentMode::Review);
    app.task_state = TaskState::Running;

    let mut plan = app.plan.clone();
    plan.edit().add_finding(review_finding("bug"));

    sync_plan_store_snapshot(&mut app, &plan, Some(AgentMode::Review));

    assert_eq!(app.view, View::Plan);
    assert_eq!(app.active_mode(), AgentMode::Planning);
    assert_eq!(app.task_state, TaskState::Running);
}

#[test]
fn sync_plan_store_snapshot_ignores_non_finding_plan_edits() {
    let mut app = app();
    app.view = View::Agent;

    let mut plan = app.plan.clone();
    plan.edit().set_section("Approach", "Do it.");

    sync_plan_store_snapshot(&mut app, &plan, Some(AgentMode::Review));

    assert_eq!(app.view, View::Agent);
    assert_eq!(app.focus, Focus::Input);
    assert_eq!(app.plan.sections.len(), 1);
}

#[test]
fn sync_plan_store_snapshot_ignores_findings_when_already_in_plan() {
    let mut app = app();
    app.reduce(AppAction::SetView(View::Plan));
    app.reduce(AppAction::SetFocus(Focus::Input));

    let mut plan = app.plan.clone();
    plan.edit().add_finding(review_finding("bug"));

    sync_plan_store_snapshot(&mut app, &plan, Some(AgentMode::Review));

    assert_eq!(app.view, View::Plan);
    assert_eq!(app.focus, Focus::Input);
    assert_eq!(app.plan.findings.len(), 1);
}

#[test]
fn sync_plan_store_snapshot_ignores_findings_outside_review_runs() {
    let mut app = app();
    app.view = View::Agent;

    let mut plan = app.plan.clone();
    plan.edit().add_finding(review_finding("bug"));

    sync_plan_store_snapshot(&mut app, &plan, Some(AgentMode::Coding));

    assert_eq!(app.view, View::Agent);
    assert_eq!(app.focus, Focus::Input);
    assert_eq!(app.plan.findings.len(), 1);
}

#[tokio::test]
async fn sync_agent_mode_and_refresh_updates_latest_report() {
    let mut app = app();
    let agent = agent_with_distinct_registries();

    app.view = View::Agent;
    app.active_persona = crate::agent::ActivePersona::Builtin(AgentMode::Coding);
    sync_agent_mode_and_refresh(&mut app, agent.clone()).await;
    let coding_report = app
        .latest_context_report
        .clone()
        .expect("coding report should be stored");
    let coding_entry =
        tool_schema_entry(&coding_report).expect("coding report has a tool-schema entry");
    assert!(
        coding_entry.text.contains("1 active tool definitions"),
        "coding entry text: {}",
        coding_entry.text
    );

    app.view = View::Plan;
    app.active_persona = crate::agent::ActivePersona::Builtin(AgentMode::Planning);
    sync_agent_mode_and_refresh(&mut app, agent.clone()).await;
    let planning_report = app
        .latest_context_report
        .clone()
        .expect("planning report should be stored");
    let planning_entry =
        tool_schema_entry(&planning_report).expect("planning report has a tool-schema entry");
    assert!(
        planning_entry.text.contains("2 active tool definitions"),
        "planning entry text: {}",
        planning_entry.text
    );
    assert_ne!(
        coding_entry.text, planning_entry.text,
        "tool-schema count should change with the view"
    );
}

#[tokio::test]
async fn sync_agent_mode_and_refresh_skips_when_busy() {
    let mut app = app();
    let agent = agent_with_distinct_registries();
    app.view = View::Plan;
    app.active_persona = crate::agent::ActivePersona::Builtin(AgentMode::Planning);
    app.latest_context_report = Some(context_report());

    app.reduce(AppAction::SetTaskState(TaskState::Running));
    sync_agent_mode_and_refresh(&mut app, agent.clone()).await;

    let stored = app
        .latest_context_report
        .as_ref()
        .expect("pre-existing report should be untouched");
    assert_eq!(
        stored.budget_tokens, 120_000,
        "report should be the unchanged pre-existing value"
    );
}

#[tokio::test]
async fn apply_persona_model_switches_to_the_modes_recorded_entry() {
    let agent = test_agent(Box::new(CompleteProvider));
    let mut store = authorized_codex_store();
    // Planning has its own recorded model on another provider; the session is
    // currently on opencode.
    store
        .mode_models
        .insert("planning".to_string(), "codex:gpt-5.5".to_string());
    let deps = crate::tui::task::PersonaModelDeps {
        agent: agent.clone(),
        session_store: Arc::new(Mutex::new(store)),
        registry: Arc::new(ProviderRegistry::default_registry()),
        model_catalog: test_model_catalog(),
        custom_agents: crate::resource::agent::shared_registry(
            crate::resource::agent::AgentRegistry::empty(),
        ),
    };

    // Modes without an entry (Coding here) and Review keep the current model.
    let coding = crate::agent::ActivePersona::Builtin(AgentMode::Coding);
    assert!(
        crate::tui::task::apply_persona_model(&deps, &coding)
            .await
            .is_none(),
        "no recorded entry should keep the current model"
    );
    let review = crate::agent::ActivePersona::Builtin(AgentMode::Review);
    assert!(
        crate::tui::task::apply_persona_model(&deps, &review)
            .await
            .is_none(),
        "review follows the current model"
    );

    // Planning applies its entry: session + agent switch to codex/gpt-5.5.
    let planning = crate::agent::ActivePersona::Builtin(AgentMode::Planning);
    let selection = crate::tui::task::apply_persona_model(&deps, &planning)
        .await
        .expect("planning's recorded model should apply");
    // The session persists the canonical model id the catalog resolves.
    assert_eq!(selection.model, "openai/gpt-5.5");
    {
        let session = deps.session_store.lock().await;
        assert_eq!(session.current_kind_id(), "codex");
        assert_eq!(session.current_session().model, "openai/gpt-5.5");
    }

    // Idempotent: a second apply for the same persona is a no-op.
    assert!(
        crate::tui::task::apply_persona_model(&deps, &planning)
            .await
            .is_none(),
        "an already-current model should not reapply"
    );
}

#[tokio::test]
async fn sync_agent_mode_and_refresh_applies_selection_after_run_ends() {
    // Regression (session 7): a persona selected mid-run — e.g. the findings
    // auto-open flipping Review → Planning — must never cancel the run; it is
    // applied by the post-completion sync once the run ends.
    let mut app = app();
    let agent = agent_with_distinct_registries();

    // Mid-run: selection changes, but the busy guard leaves the agent alone.
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.view = View::Plan;
    app.active_persona = crate::agent::ActivePersona::Builtin(AgentMode::Planning);
    sync_agent_mode_and_refresh(&mut app, agent.clone()).await;

    // Run finished (completed or interrupted): the selection lands.
    app.reduce(AppAction::SetTaskState(TaskState::Idle));
    sync_agent_mode_and_refresh(&mut app, agent.clone()).await;
    assert_eq!(
        agent.lock().await.active_persona(),
        &crate::agent::ActivePersona::Builtin(AgentMode::Planning),
        "selection made mid-run should apply once the run ends"
    );
}

use crate::agent::ContextReport;
use crate::provider::{EstimateConfidence, TokenCounterKind};

#[test]
fn session_end_usage_summary_includes_tokens_and_cost() {
    let report = ContextReport {
        budget_tokens: 120_000,
        entries: Vec::new(),
        ledger: Vec::new(),
        estimate_source: TokenCounterKind::Heuristic,
        estimate_confidence: EstimateConfidence::Low,
        prompt_estimate_tokens: 0,
        tool_schema_tokens: 0,
        last_prompt_tokens: None,
        last_completion_tokens: None,
        last_input_cache: None,
        last_turn_cost_micros: None,
        session_prompt_tokens: 12_500,
        session_completion_tokens: 1_200,
        session_input_cache: None,
        session_cost_micros: Some(42_000),
        ..Default::default()
    };

    let summary = super::session_end_usage_summary(
        crate::storage::SessionId::from_raw(7),
        "codex",
        "gpt-5.5",
        &report,
    )
    .expect("usage should produce a session summary");

    assert_eq!(
        summary,
        "bonsai session #7 complete: codex/gpt-5.5 · sent 12,500 tok · received 1,200 tok · cost $0.0420"
    );
}

#[test]
fn derive_session_title_collapses_whitespace_and_keeps_short_prompts() {
    assert_eq!(
        super::derive_session_title("  fix\tthe   resume\nhandling  "),
        Some("fix the resume handling".to_string())
    );
}

#[test]
fn derive_session_title_truncates_long_prompts_on_a_word_boundary() {
    let input = "Refactor the session picker so the title column reflects the AI-generated label instead of the project name";
    let title = super::derive_session_title(input).expect("non-blank input yields a title");
    assert!(title.ends_with('…'), "expected an ellipsis: {title}");
    assert!(
        title.chars().count() <= 61,
        "title should stay bounded: {title}"
    );
    assert!(
        !title.trim_end_matches('…').ends_with(' '),
        "no trailing space before the ellipsis: {title}"
    );
    assert!(title.starts_with("Refactor the session picker"));
}

#[test]
fn derive_session_title_rejects_blank_input() {
    assert_eq!(super::derive_session_title("   \n\t  "), None);
}

#[test]
fn resume_hint_names_the_session_id() {
    let hint = super::format_resume_hint(crate::storage::SessionId::from_raw(42));
    assert_eq!(
        hint,
        "Resume this session: /resume 42  (or relaunch with `bonsai -c 42`)"
    );
}

#[test]
fn session_end_usage_summary_skips_empty_sessions() {
    let report = ContextReport {
        budget_tokens: 120_000,
        entries: Vec::new(),
        ledger: Vec::new(),
        estimate_source: TokenCounterKind::Heuristic,
        estimate_confidence: EstimateConfidence::Low,
        prompt_estimate_tokens: 0,
        tool_schema_tokens: 0,
        last_prompt_tokens: None,
        last_completion_tokens: None,
        last_input_cache: None,
        last_turn_cost_micros: None,
        session_prompt_tokens: 0,
        session_completion_tokens: 0,
        session_input_cache: None,
        session_cost_micros: None,
        ..Default::default()
    };

    assert!(
        super::session_end_usage_summary(
            crate::storage::SessionId::from_raw(7),
            "codex",
            "gpt-5.5",
            &report
        )
        .is_none()
    );
}

#[test]
fn persistence_command_parses_discard() {
    assert_eq!(
        persistence_command("/discard"),
        Some(PersistenceCommand::DiscardPlan)
    );
}

#[test]
fn persistence_command_parses_new_plan() {
    assert_eq!(
        persistence_command("/new-plan"),
        Some(PersistenceCommand::NewPlan)
    );
}

#[tokio::test]
async fn new_plan_clears_canvas_but_keeps_transcript() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.plan = sample_plan("Reset me");
    app.transcript.push(TranscriptItem::UserMessage {
        text: "keep this chat".to_string(),
    });

    apply_persistence_command(
        PersistenceCommand::NewPlan,
        &mut app,
        persistence_deps(&storage, temp_dir.path(), session_id),
        &mut state,
    )
    .await
    .unwrap();

    assert!(app.plan.is_empty(), "the canvas should clear");
    assert!(
        app.active_saved_plan_session_id.is_none(),
        "the saved-plan binding should drop"
    );
    assert!(
        !app.transcript.is_empty(),
        "the transcript must survive /new-plan"
    );
    assert!(app.modal.is_none(), "/new-plan never opens a modal");
}

#[tokio::test]
async fn new_plan_unbinds_saved_plan_without_deleting_record() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.plan = sample_plan("Saved one");

    // Save → links the canvas to a library record.
    apply_persistence_command(
        PersistenceCommand::SavePlan,
        &mut app,
        persistence_deps(&storage, temp_dir.path(), session_id),
        &mut state,
    )
    .await
    .unwrap();
    let saved_id = app
        .active_saved_plan_session_id
        .expect("plan should be saved");

    // /new-plan resets the canvas but leaves the saved record intact.
    apply_persistence_command(
        PersistenceCommand::NewPlan,
        &mut app,
        persistence_deps(&storage, temp_dir.path(), session_id),
        &mut state,
    )
    .await
    .unwrap();

    assert!(app.plan.is_empty(), "the canvas should clear");
    assert!(
        app.active_saved_plan_session_id.is_none(),
        "the saved-plan binding should drop without a confirm modal"
    );
    assert!(app.modal.is_none(), "/new-plan never opens a confirm modal");
    assert!(
        storage.load_saved_plan(saved_id).await.unwrap().is_some(),
        "the saved record must survive /new-plan"
    );
}

#[tokio::test]
async fn new_plan_is_noop_when_busy() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.plan = sample_plan("Busy");
    app.task_state = TaskState::Running;

    apply_persistence_command(
        PersistenceCommand::NewPlan,
        &mut app,
        persistence_deps(&storage, temp_dir.path(), session_id),
        &mut state,
    )
    .await
    .unwrap();

    assert!(
        !app.plan.is_empty(),
        "a busy /new-plan must not clear the canvas"
    );
    assert!(app.modal.is_none());
}

#[tokio::test]
async fn discard_unsaved_plan_clears_canvas_without_prompt() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.plan = sample_plan("Throwaway");
    assert!(app.active_saved_plan_session_id.is_none());

    apply_persistence_command(
        PersistenceCommand::DiscardPlan,
        &mut app,
        persistence_deps(&storage, temp_dir.path(), session_id),
        &mut state,
    )
    .await
    .unwrap();

    assert!(app.plan.is_empty(), "an unsaved canvas should clear");
    assert!(app.modal.is_none(), "an unsaved discard should not prompt");
    assert!(app.active_saved_plan_session_id.is_none());
}

#[tokio::test]
async fn discard_saved_plan_opens_confirm_without_deleting() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.plan = sample_plan("Saved one");

    apply_persistence_command(
        PersistenceCommand::SavePlan,
        &mut app,
        persistence_deps(&storage, temp_dir.path(), session_id),
        &mut state,
    )
    .await
    .unwrap();
    let saved_id = app
        .active_saved_plan_session_id
        .expect("plan should be saved");

    apply_persistence_command(
        PersistenceCommand::DiscardPlan,
        &mut app,
        persistence_deps(&storage, temp_dir.path(), session_id),
        &mut state,
    )
    .await
    .unwrap();

    assert!(
        matches!(
            app.modal,
            Some(ModalKind::Confirm(crate::tui::event::ConfirmModal::PlanDiscard { saved_plan_id, .. })) if saved_plan_id == saved_id
        ),
        "discarding a saved plan should open the confirm modal"
    );
    assert!(!app.plan.is_empty(), "nothing should clear before confirm");
    assert!(
        storage.load_saved_plan(saved_id).await.unwrap().is_some(),
        "the saved record must survive until confirm"
    );
}

#[tokio::test]
async fn discard_is_noop_when_empty_or_busy() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };

    // Empty canvas → nothing to discard.
    let mut empty_app = app();
    apply_persistence_command(
        PersistenceCommand::DiscardPlan,
        &mut empty_app,
        persistence_deps(&storage, temp_dir.path(), session_id),
        &mut state,
    )
    .await
    .unwrap();
    assert!(empty_app.modal.is_none());

    // Busy → refused even with a plan on the canvas.
    let mut busy_app = app();
    busy_app.plan = sample_plan("Busy");
    busy_app.task_state = TaskState::Running;
    apply_persistence_command(
        PersistenceCommand::DiscardPlan,
        &mut busy_app,
        persistence_deps(&storage, temp_dir.path(), session_id),
        &mut state,
    )
    .await
    .unwrap();
    assert!(
        !busy_app.plan.is_empty(),
        "a busy discard must not clear the plan"
    );
    assert!(busy_app.modal.is_none());
}

#[tokio::test]
async fn discard_confirm_deletes_saved_record_and_clears_canvas() {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let (storage, session_id) = storage_with_active_session(temp_dir.path()).await;
    let (runtime_tx, _runtime_rx) = mpsc::unbounded_channel();
    let mut tasks = TaskController::new(runtime_tx.clone());
    let mut current = session_id;
    let mut signatures = zero_signatures();
    let mut state = PersistenceCommandState {
        current_session_id: &mut current,
        signatures: &mut signatures,
    };
    let mut app = app();
    app.plan = sample_plan("To delete");

    apply_persistence_command(
        PersistenceCommand::SavePlan,
        &mut app,
        persistence_deps(&storage, temp_dir.path(), session_id),
        &mut state,
    )
    .await
    .unwrap();
    let saved_id = app
        .active_saved_plan_session_id
        .expect("plan should be saved");
    app.modal = Some(ModalKind::Confirm(
        crate::tui::event::ConfirmModal::PlanDiscard {
            saved_plan_id: saved_id,
            title: app.plan.title.clone(),
        },
    ));

    let (interaction, _interaction_rx) = InteractionService::new();
    let result = handle_runtime_action(
        AppAction::PlanDiscardConfirmSubmit,
        &mut app,
        &mut tasks,
        RuntimeActionDeps {
            interaction: Arc::new(interaction),
            runtime_sender: runtime_tx,
            agent: test_agent(Box::new(CompleteProvider)),
            memory: None,
            yolo_mode: crate::yolo::YoloMode::new(),
            session_store: Arc::new(Mutex::new(SessionStore::default())),
            permissions: crate::permissions::PermissionManager::memory_only(),
            domain_permissions: crate::permissions::PermissionManager::memory_only_domains(),
            registry: Arc::new(ProviderRegistry::default_registry()),
            model_catalog: test_model_catalog(),
            storage: &storage,
            project_root: temp_dir.path(),
            session_project_root: temp_dir.path(),
            active_session_id: Arc::new(Mutex::new(Some(session_id))),
            todo_store: Arc::new(Mutex::new(crate::todo::TodoStore::new())),
            plan_store: Arc::new(Mutex::new(crate::plan::PlanDoc::default())),
            sink: Arc::new(NullSink),
            background_tasks: Arc::new(BackgroundTaskRegistry::new()),
            terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
            peer_bus: None,
        },
        &mut state,
    )
    .await;

    assert!(matches!(result, RuntimeActionResult::Handled));
    assert!(app.modal.is_none(), "the modal should close after confirm");
    assert!(app.plan.is_empty(), "the canvas should clear after confirm");
    assert!(app.active_saved_plan_session_id.is_none());
    assert!(
        storage.load_saved_plan(saved_id).await.unwrap().is_none(),
        "the saved record should be deleted"
    );
}

fn snapshot_agent() -> Arc<Mutex<Agent>> {
    let fixture = TestFixture::new();
    let context = crate::context::ProjectContextSnapshot {
        environment: "## Environment\n- cwd: /x".to_string(),
        volatile_state: String::new(),
        steering_files: Vec::new(),
        repo_map: String::new(),
        skills_index: String::new(),
        smol_skills_index: String::new(),
        agents_index: String::new(),
        memory_index: String::new(),
        stale_read_advisory: String::new(),
        peer_status: String::new(),
    };
    Arc::new(Mutex::new(
        Agent::builder(
            Box::new(CompleteProvider),
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            fixture.project_root.clone(),
        )
        .project_context_snapshot(context)
        .build()
        .expect("snapshot agent should build"),
    ))
}

const TEST_REPO_MAP: &str = "## Repository map\nsrc/lib.rs\n  fn entry";

// F2 regression: a map stashed from `RepoMapReady` must land in the agent at a
// turn-start site, before the run task takes the lock — not only from the idle
// poll — so turn 1's cacheable prefix already includes it.
#[tokio::test]
async fn repo_map_injector_applies_stashed_map_at_turn_start() {
    let agent = snapshot_agent();
    let before = agent.lock().await.context_report().cacheable_prefix_tokens;
    let mut injector = empty_repo_map_injector();
    injector.stash(TEST_REPO_MAP.to_string());

    let report = injector.apply_before_turn(&agent).await;

    let report = report.expect("a stashed map should apply and refresh the report");
    assert!(
        report.cacheable_prefix_tokens > before,
        "the applied map should enlarge the cacheable prefix"
    );
    assert!(
        injector.apply_before_turn(&agent).await.is_none(),
        "later turn-starts skip the already-applied map"
    );
}

// F2: the very first turn bound-waits for an in-flight build, so a submit that
// beats the async tree walk still gets the map before the first request.
#[tokio::test]
async fn repo_map_injector_first_turn_waits_for_inflight_build() {
    let agent = snapshot_agent();
    let before = agent.lock().await.context_report().cacheable_prefix_tokens;
    let (sender, receiver) = tokio::sync::watch::channel(None);
    let mut injector = RepoMapInjector::new(receiver);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = sender.send(Some(TEST_REPO_MAP.to_string()));
    });

    let report = injector.apply_before_turn(&agent).await;

    let report = report.expect("the bounded wait should pick up the finished build");
    assert!(report.cacheable_prefix_tokens > before);
}

// F2: a build slower than the bound must not stall the first turn; the map
// applies later (one prefix shift, as before the drain existed). Pays the real
// 750ms bound once — tokio's test-util (paused time) is not enabled.
#[tokio::test]
async fn repo_map_injector_proceeds_without_map_when_build_is_slow() {
    let agent = snapshot_agent();
    let (sender, receiver) = tokio::sync::watch::channel(None);
    let mut injector = RepoMapInjector::new(receiver);

    assert!(
        injector.apply_before_turn(&agent).await.is_none(),
        "a still-running build must not block the turn past the bound"
    );

    // The build finishes later; the next idle poll applies it.
    let _ = sender.send(Some(TEST_REPO_MAP.to_string()));
    injector.stash(TEST_REPO_MAP.to_string());
    assert!(
        injector.try_apply_when_idle(&agent).is_some(),
        "the late map still applies from the idle poll"
    );
}

// F2: a repo with no map (empty build result) resolves the watch so the first
// turn neither waits nor applies anything.
#[tokio::test]
async fn repo_map_injector_skips_empty_map() {
    let agent = snapshot_agent();
    let before = agent.lock().await.context_report().cacheable_prefix_tokens;
    let mut injector = empty_repo_map_injector();

    assert!(injector.apply_before_turn(&agent).await.is_none());
    assert_eq!(
        agent.lock().await.context_report().cacheable_prefix_tokens,
        before,
        "an empty map must not touch the system message"
    );
}

// Peer wake gate: a solicited wake (done notice for a park this session
// entered itself) resumes in-flight work, so it bypasses the autonomy gate,
// the hop cap, and the budget; unsolicited wakes keep every brake.
#[test]
fn peer_wake_gate_solicited_bypasses_policy_brakes() {
    use crate::tool::ApprovalLevel;

    // Unsolicited: gated on autonomy and hop.
    assert!(peer_wake_gate_allows(false, ApprovalLevel::Balanced, 0));
    assert!(!peer_wake_gate_allows(
        false,
        ApprovalLevel::Conservative,
        0
    ));
    assert!(!peer_wake_gate_allows(
        false,
        ApprovalLevel::Balanced,
        crate::peer::MAX_PEER_HOPS
    ));

    // Solicited: resumes regardless of autonomy or hop depth.
    assert!(peer_wake_gate_allows(true, ApprovalLevel::Conservative, 0));
    assert!(peer_wake_gate_allows(
        true,
        ApprovalLevel::Conservative,
        crate::peer::MAX_PEER_HOPS
    ));
}

#[test]
fn peer_wake_authority_is_message_bound_and_keeps_the_maximum_hop() {
    use crate::agent::PeerWait;
    use crate::storage::{PeerMessage, PeerMessageKind, SessionId};

    let peer = SessionId::from_raw(45);
    let recipient = SessionId::from_raw(7);
    let message = |id, kind, hop_count, wake_subscription_id| PeerMessage {
        id,
        from_session_id: peer,
        to_session_id: recipient,
        kind,
        body: "peer update".to_string(),
        hop_count,
        created_at_ms: 0,
        wake_subscription_id,
    };
    let wait = Some(PeerWait {
        session_id: peer,
        subscription_id: 99,
    });

    let exact_done = message(1, PeerMessageKind::DoneNotice, 1, Some(99));
    assert_eq!(peer_wake_authority(&[exact_done], wait), Some((1, true)));

    // Once that exact done notice is absent from the current durable inbox, a
    // later ordinary message cannot inherit its solicited bypass.
    let ordinary = message(2, PeerMessageKind::Text, 0, None);
    assert_eq!(
        peer_wake_authority(std::slice::from_ref(&ordinary), wait),
        Some((0, false))
    );

    let wrong_done = message(3, PeerMessageKind::DoneNotice, 0, Some(100));
    // A stale completion is informational only; it cannot be downgraded into
    // an unsolicited automatic wake.
    assert_eq!(peer_wake_authority(&[wrong_done], wait), None);

    // Merging a lower-hop message never lowers an earlier pending cap.
    let capped = message(4, PeerMessageKind::Text, crate::peer::MAX_PEER_HOPS, None);
    assert_eq!(
        peer_wake_authority(&[capped, ordinary], None),
        Some((crate::peer::MAX_PEER_HOPS, false))
    );
}

#[test]
fn peer_wait_recheck_tracks_only_the_current_parked_subscription() {
    use crate::agent::{PeerWait, WaitReason};
    use crate::storage::SessionId;
    use crate::tui::event::AgentRunOutcome;

    let first = PeerWait {
        session_id: SessionId::from_raw(45),
        subscription_id: 99,
    };
    let second = PeerWait {
        session_id: SessionId::from_raw(46),
        subscription_id: 100,
    };
    let now = Instant::now();
    let mut pending = None;

    update_peer_wait_recheck(
        &mut pending,
        &RuntimeEvent::AgentFinished(Ok(AgentRunOutcome::Waiting(WaitReason::Peer(first)))),
        now,
    );
    let scheduled = pending.expect("peer park schedules a recheck");
    assert_eq!(scheduled.wait, first);
    assert_eq!(scheduled.deadline, now + PEER_WAIT_RECHECK_DELAY);

    update_peer_wait_recheck(
        &mut pending,
        &RuntimeEvent::AgentFinished(Ok(AgentRunOutcome::Waiting(WaitReason::Peer(second)))),
        now,
    );
    assert_eq!(
        pending.map(|recheck| recheck.wait),
        Some(second),
        "a later park must replace the old subscription identity"
    );

    update_peer_wait_recheck(&mut pending, &RuntimeEvent::AgentStarted, now);
    assert!(
        pending.is_none(),
        "starting another run abandons the parked wait timer"
    );
}

// Stranded-waiter sweep: a missing target expires immediately; an idle target
// expires only on the second consecutive idle observation; a working target
// resets its counter; resolved waits drop stale counters.
#[test]
fn wake_sweep_expires_missing_immediately_and_idle_on_second_tick() {
    use crate::storage::SessionId;
    use std::collections::HashMap;

    let dead = SessionId::from_raw(1);
    let idle = SessionId::from_raw(2);
    let busy = SessionId::from_raw(3);
    let waiting_on = vec![dead, idle, busy];
    let mut live_working = HashMap::new();
    live_working.insert(idle, false);
    live_working.insert(busy, true);
    let mut idle_ticks = HashMap::new();

    // Tick 1: only the dead target expires; the idle one starts counting.
    let expired = expired_wake_targets(&waiting_on, &live_working, &mut idle_ticks);
    assert_eq!(expired, vec![dead]);
    assert_eq!(idle_ticks.get(&idle), Some(&1));

    // Tick 2: still idle → expires; the working target never counted.
    let waiting_on = vec![idle, busy];
    let expired = expired_wake_targets(&waiting_on, &live_working, &mut idle_ticks);
    assert_eq!(expired, vec![idle]);
    assert!(!idle_ticks.contains_key(&idle));
    assert!(!idle_ticks.contains_key(&busy));

    // A target that resumes working between ticks resets its counter.
    let waiting_on = vec![busy];
    let mut flapping = HashMap::new();
    flapping.insert(busy, false);
    let expired = expired_wake_targets(&waiting_on, &flapping, &mut idle_ticks);
    assert!(expired.is_empty());
    assert_eq!(idle_ticks.get(&busy), Some(&1));
    flapping.insert(busy, true);
    let expired = expired_wake_targets(&waiting_on, &flapping, &mut idle_ticks);
    assert!(expired.is_empty());
    assert!(
        !idle_ticks.contains_key(&busy),
        "a working observation must reset the idle counter"
    );

    // A wait that resolved on its own drops its stale counter.
    idle_ticks.insert(idle, 1);
    let expired = expired_wake_targets(&[], &flapping, &mut idle_ticks);
    assert!(expired.is_empty());
    assert!(idle_ticks.is_empty());
}

#[tokio::test]
async fn halt_folds_completed_todos_into_plan_so_continue_resumes_in_place() {
    use crate::todo::{TodoItem, TodoStatus};

    // Session 239 regression: a phased run halts after finishing its phase's
    // todos; the plan canvas must absorb those completions so the
    // first-pending-phase scan does not re-run finished work.
    let mut doc = crate::plan::PlanDoc::default();
    {
        let mut editor = doc.edit();
        editor.add_phase("One");
        editor.add_phase("Two");
        editor
            .add_task_to_phase("One", "Implement Vec3 module")
            .unwrap();
        editor
            .add_task_to_phase("One", "Implement renderer")
            .unwrap();
        editor.add_task_to_phase("Two", "Write docs").unwrap();
    }
    let plan_store: SharedPlanStore = Arc::new(tokio::sync::Mutex::new(doc));

    let mut todos = crate::todo::TodoStore::new();
    todos.set_todos(vec![
        TodoItem {
            content: "Implement Vec3 module".to_string(),
            status: TodoStatus::Completed,
        },
        TodoItem {
            content: "Implement renderer".to_string(),
            status: TodoStatus::InProgress,
        },
    ]);
    let todo_store: SharedTodoStore = Arc::new(tokio::sync::Mutex::new(todos));

    let plan = fold_completed_todos_into_plan(&todo_store, &plan_store)
        .await
        .expect("completed todos should update the plan");

    // Completed todo is now done on the canvas; unfinished work stays pending,
    // so the resume position is phase One's second task — not the phase start.
    let items = plan.phase_todo_items(0);
    assert_eq!(items[0].status, TodoStatus::Completed);
    assert_ne!(items[1].status, TodoStatus::Completed);
    assert_eq!(plan.next_phase_with_pending(None), Some(0));

    // With every todo of the phase completed, the phase no longer registers as
    // pending and /continue moves on to phase Two.
    {
        let mut todos = todo_store.lock().await;
        todos.set_todos(vec![TodoItem {
            content: "Implement renderer".to_string(),
            status: TodoStatus::Completed,
        }]);
    }
    let plan = fold_completed_todos_into_plan(&todo_store, &plan_store)
        .await
        .expect("second fold should update the plan");
    assert_eq!(plan.next_phase_with_pending(None), Some(1));
}
