use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use super::controller::{Controller, SessionFactory, SessionRuntime};
use crate::interaction::InteractionService;
use crate::output::SharedSink;
use crate::provider::{Provider, ProviderFactory, ProviderMetadata, StreamedResponse};
use crate::runtime::RuntimeCore;
use crate::storage::{SessionId, SessionRunSelection, SessionStatus};

#[derive(Default)]
struct Mock {
    calls: AtomicUsize,
    immediate: AtomicBool,
    ignore_cancel: AtomicBool,
    release: Notify,
}

struct MockProvider(Arc<Mock>);

#[async_trait]
impl Provider for MockProvider {
    async fn chat_stream(
        &self,
        _messages: &[async_openai::types::chat::ChatCompletionRequestMessage],
        _tools: &[async_openai::types::chat::ChatCompletionTool],
        cancel: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        if self.0.ignore_cancel.load(Ordering::SeqCst) {
            self.0.release.notified().await;
        } else if !self.0.immediate.load(Ordering::SeqCst) {
            tokio::select! {
                () = self.0.release.notified() => {},
                () = cancel.cancelled() => return Ok(StreamedResponse::interrupted()),
            }
        }
        sink.assistant_delta("Test answer.");
        sink.assistant_done();
        Ok(StreamedResponse {
            content: "Test answer.".into(),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: Some(crate::provider::TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 3,
                input_cache: None,
            }),
            ..StreamedResponse::default()
        })
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

struct MockFactory(Arc<Mock>);
#[async_trait]
impl ProviderFactory for MockFactory {
    fn metadata(&self) -> &ProviderMetadata {
        crate::provider::metadata_for("opencode").unwrap()
    }
    fn build_target(
        &self,
        _: &crate::session::ProviderSession,
        _: &crate::model_catalog::RunTarget,
    ) -> Box<dyn Provider> {
        Box::new(MockProvider(self.0.clone()))
    }
    fn is_authorized(&self, _: &crate::session::ProviderSession) -> bool {
        true
    }
    async fn list_models(
        &self,
        _: &crate::session::ProviderSession,
    ) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[derive(Default)]
struct Handles {
    interaction: Option<Arc<InteractionService>>,
    background: Option<Arc<crate::background::BackgroundTaskRegistry>>,
    plan: Option<crate::plan::SharedPlanStore>,
    todos: Option<crate::todo::SharedTodoStore>,
}

struct Factory {
    runtime: SessionRuntime,
    builds: AtomicUsize,
    handles: Mutex<Handles>,
}
#[async_trait]
impl SessionFactory for Factory {
    async fn build(
        &self,
        id: SessionId,
        interaction: Arc<InteractionService>,
        active: crate::tool::SharedActiveSessionId,
    ) -> anyhow::Result<RuntimeCore> {
        self.builds.fetch_add(1, Ordering::SeqCst);
        let core = self.runtime.build(id, interaction.clone(), active).await?;
        *self.handles.lock().await = Handles {
            interaction: Some(interaction),
            background: Some(core.background_tasks.clone()),
            plan: Some(core.plan_store.clone()),
            todos: Some(core.todo_store.clone()),
        };
        Ok(core)
    }
}

mod api_tests;

struct Fixture {
    db: crate::storage::test_utils::TestStorage,
    controller: Controller,
    factory: Arc<Factory>,
    mock: Arc<Mock>,
    id: SessionId,
    selection: SessionRunSelection,
}

impl Fixture {
    async fn new() -> Self {
        let db = crate::storage::test_utils::TestStorage::new().await;
        let mock = Arc::new(Mock::default());
        let catalog = Arc::new(crate::model_catalog::ModelCatalog::load_builtin().unwrap());
        let registry = Arc::new(crate::provider::ProviderRegistry::new(vec![Arc::new(
            MockFactory(mock.clone()),
        )]));
        let mut session = crate::session::SessionStore::default();
        session.ensure_provider("opencode");
        session.session_mut("opencode").api_key = "mock-key".into();
        session.set_current_kind_id("opencode");
        let model = session.session_mut("opencode").model.clone();
        let id = db.start_session_with("opencode", &model).await;
        db.storage
            .mark_session_status(id, SessionStatus::Interrupted)
            .await
            .unwrap();
        let project = db.storage.ensure_project(db.project_path()).await.unwrap();
        let trust = crate::workspace_trust::WorkspaceTrustGate::load(db.storage.clone(), project)
            .await
            .unwrap();
        let factory = Arc::new(Factory {
            runtime: SessionRuntime {
                root: db.project_path().canonicalize().unwrap(),
                storage: db.storage.clone(),
                catalog,
                registry,
                session,
                budget: crate::run_budget::RunBudget::default(),
                autonomy: crate::yolo::YoloMode::new(),
                trust,
            },
            builds: AtomicUsize::new(0),
            handles: Mutex::new(Handles::default()),
        });
        let controller =
            Controller::new(db.project_path(), db.storage.clone(), factory.clone()).unwrap();
        let selection = SessionRunSelection {
            provider_id: "opencode".into(),
            model,
            reasoning: Default::default(),
        };
        Self {
            db,
            controller,
            factory,
            mock,
            id,
            selection,
        }
    }
    async fn resume(&self) {
        self.controller
            .resume(self.id, &self.selection)
            .await
            .unwrap();
    }
    async fn wait_calls(&self, n: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.mock.calls.load(Ordering::SeqCst) < n {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    async fn wait_idle(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (snapshot, _) = self.controller.snapshot().await.unwrap();
                assert!(snapshot.failure.is_none(), "{:?}", snapshot);
                if snapshot.state == "idle" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn resume_does_not_run_model_and_live_claim_is_rejected() {
    let f = Fixture::new().await;
    f.resume().await;
    assert_eq!(f.mock.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        f.db.storage
            .claim_session_for_resume(f.db.project_path(), f.id)
            .await
            .unwrap(),
        crate::storage::ResumeSessionOutcome::Live
    );
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn concurrent_messages_deduplicate_and_conflict_without_queue() {
    let f = Fixture::new().await;
    f.resume().await;
    let (a, b) = tokio::join!(
        f.controller.message(f.id, "same", "Hello"),
        f.controller.message(f.id, "same", "Hello")
    );
    assert_eq!(a.unwrap().run_id, b.unwrap().run_id);
    f.wait_calls(1).await;
    assert!(
        f.controller
            .message(f.id, "same", "Different")
            .await
            .is_err()
    );
    assert!(
        f.controller
            .message(f.id, "another", "Hello")
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(1), f.controller.interrupt(f.id))
        .await
        .unwrap()
        .unwrap();
    f.wait_idle().await;
    assert_eq!(f.mock.calls.load(Ordering::SeqCst), 1);
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn repeated_turns_reuse_runtime_and_persist_transcript() {
    let f = Fixture::new().await;
    f.mock.immediate.store(true, Ordering::SeqCst);
    f.resume().await;
    f.controller.message(f.id, "one", "Hello").await.unwrap();
    f.wait_idle().await;
    f.controller
        .message(f.id, "two", "Another question")
        .await
        .unwrap();
    f.wait_idle().await;
    assert_eq!(f.factory.builds.load(Ordering::SeqCst), 1);
    let saved =
        f.db.storage
            .load_runtime_session_snapshot(f.id)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        saved
            .transcript
            .iter()
            .filter(|i| matches!(i, crate::protocol::TranscriptItem::UserMessage { .. }))
            .count(),
        2
    );
    assert_eq!(
        saved
            .transcript
            .iter()
            .filter(|i| matches!(i, crate::protocol::TranscriptItem::CompletionReport(_)))
            .count(),
        2
    );
    assert_eq!(f.mock.calls.load(Ordering::SeqCst), 2);
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn slot_retained_for_background_children_and_switch_requires_drain() {
    let f = Fixture::new().await;
    f.resume().await;
    let second = f.controller.create(&f.selection).await.unwrap();
    let background = f.factory.handles.lock().await.background.clone().unwrap();
    let child = background
        .start("/bin/sh", "sleep 30", f.db.project_path(), 60)
        .await
        .unwrap();
    assert!(f.controller.resume(second, &f.selection).await.is_err());
    assert_eq!(f.mock.calls.load(Ordering::SeqCst), 0);
    f.controller.release(Duration::from_secs(5)).await.unwrap();
    assert!(
        background
            .snapshot(&child.id)
            .await
            .unwrap()
            .status
            .is_finished()
    );
    f.controller.resume(second, &f.selection).await.unwrap();
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn approval_is_responsive_correlated_and_consumed_once() {
    use crate::interaction::{InteractionOutcome, InteractionRequest, PermissionDecision};
    let f = Fixture::new().await;
    f.resume().await;
    let command = f.controller.message(f.id, "one", "Hello").await.unwrap();
    f.wait_calls(1).await;
    let interaction = f.factory.handles.lock().await.interaction.clone().unwrap();
    let waiter = tokio::spawn(async move {
        interaction
            .request(|request_id| InteractionRequest::Permission {
                request_id,
                command: "test command".into(),
                origin: None,
            })
            .await
    });
    let request = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let (snapshot, _) = f.controller.snapshot().await.unwrap();
            if let Some(InteractionRequest::Permission { request_id, .. }) =
                snapshot.pending.first()
            {
                break *request_id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        f.controller
            .respond(
                f.id,
                "old-run",
                request,
                InteractionOutcome::Permission(PermissionDecision::AllowOnce)
            )
            .await
            .is_err()
    );
    assert!(
        f.controller
            .respond(
                f.id,
                &command.run_id,
                request,
                InteractionOutcome::Question(None)
            )
            .await
            .is_err()
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        f.controller.respond(
            f.id,
            &command.run_id,
            request,
            InteractionOutcome::Permission(PermissionDecision::AllowOnce),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        waiter.await.unwrap().unwrap(),
        InteractionOutcome::Permission(PermissionDecision::AllowOnce)
    ));
    assert!(
        f.controller
            .respond(
                f.id,
                &command.run_id,
                request,
                InteractionOutcome::Permission(PermissionDecision::AllowOnce)
            )
            .await
            .is_err()
    );
    f.controller.interrupt(f.id).await.unwrap();
    f.wait_idle().await;
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn stale_heartbeat_cannot_steal_os_owned_session() {
    let f = Fixture::new().await;
    f.resume().await;
    f.db.storage
        .set_session_heartbeat_for_tests(f.id, 0)
        .await
        .unwrap();
    assert_eq!(
        f.db.storage
            .claim_session_for_resume(f.db.project_path(), f.id)
            .await
            .unwrap(),
        crate::storage::ResumeSessionOutcome::Live
    );
    let other =
        Controller::new(f.db.project_path(), f.db.storage.clone(), f.factory.clone()).unwrap();
    assert!(other.resume(f.id, &f.selection).await.is_err());
    f.controller.release(Duration::from_secs(5)).await.unwrap();
    other.resume(f.id, &f.selection).await.unwrap();
    other.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn timed_out_shutdown_and_drop_keep_claim_until_execution_settles() {
    let f = Fixture::new().await;
    f.mock.ignore_cancel.store(true, Ordering::SeqCst);
    f.resume().await;
    f.controller.message(f.id, "one", "Hello").await.unwrap();
    f.wait_calls(1).await;
    assert!(
        f.controller
            .shutdown(Duration::from_millis(1))
            .await
            .is_err()
    );
    let Fixture {
        controller,
        db,
        mock,
        ..
    } = f;
    drop(controller);
    db.storage
        .set_session_heartbeat_for_tests(SessionId::from_raw(1), 0)
        .await
        .unwrap();
    assert!(
        db.storage
            .try_session_owner(SessionId::from_raw(1))
            .unwrap()
            .is_none()
    );
    mock.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if db
                .storage
                .try_session_owner(SessionId::from_raw(1))
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn failure_pool(f: &Fixture) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(f.db.temp_dir.path().join("bonsai.db")),
    )
    .await
    .unwrap()
}

async fn wait_failed(f: &Fixture) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let (state, _) = f.controller.snapshot().await.unwrap();
            if state.state == "failed" {
                assert!(state.failure.is_some());
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn admission_failure_never_starts_execution() {
    let f = Fixture::new().await;
    f.resume().await;
    let pool = failure_pool(&f).await;
    sqlx::query("CREATE TRIGGER fail_admission BEFORE INSERT ON server_commands BEGIN SELECT RAISE(FAIL, 'admission unavailable'); END")
        .execute(&pool).await.unwrap();
    assert!(f.controller.message(f.id, "one", "Hello").await.is_err());
    assert_eq!(f.mock.calls.load(Ordering::SeqCst), 0);
    assert!(
        f.db.storage
            .server_command(f.id, "one", "Hello")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.controller.snapshot().await.unwrap().0.state, "idle");
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn failed_terminal_bookkeeping_preserves_committed_report_without_publication() {
    for table in ["task_runs", "server_commands"] {
        let f = Fixture::new().await;
        f.resume().await;
        let (_, mut events) = f.controller.subscribe().await.unwrap();
        f.controller.message(f.id, "one", "Hello").await.unwrap();
        f.wait_calls(1).await;
        let pool = failure_pool(&f).await;
        sqlx::query(match table {
            "task_runs" => "CREATE TRIGGER fail_terminal BEFORE UPDATE ON task_runs BEGIN SELECT RAISE(FAIL, 'terminal unavailable'); END",
            "server_commands" => "CREATE TRIGGER fail_terminal BEFORE UPDATE ON server_commands BEGIN SELECT RAISE(FAIL, 'terminal unavailable'); END",
            _ => unreachable!("fixed test table"),
        })
            .execute(&pool).await.unwrap();
        f.mock.release.notify_one();
        wait_failed(&f).await;
        let before =
            f.db.storage
                .load_runtime_session_snapshot(f.id)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            before
                .transcript
                .iter()
                .filter(|item| matches!(item, crate::protocol::TranscriptItem::CompletionReport(_)))
                .count(),
            1,
            "{table}"
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event.event, super::sink::SessionEvent::Completion(_)),
                "terminal evidence was published before bookkeeping committed"
            );
        }
        assert!(f.controller.message(f.id, "two", "Again").await.is_err());
        f.controller.release(Duration::from_secs(5)).await.unwrap();
        let after =
            f.db.storage
                .load_runtime_session_snapshot(f.id)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            serde_json::to_value(&after.transcript).unwrap(),
            serde_json::to_value(&before.transcript).unwrap(),
            "release overwrote committed evidence after {table} failure"
        );
    }
}

#[tokio::test]
async fn snapshot_failure_never_publishes_completion_or_finishes_command() {
    let f = Fixture::new().await;
    f.resume().await;
    let (_, mut events) = f.controller.subscribe().await.unwrap();
    f.controller.message(f.id, "one", "Hello").await.unwrap();
    f.wait_calls(1).await;
    let pool = failure_pool(&f).await;
    sqlx::query("CREATE TRIGGER fail_snapshot BEFORE INSERT ON transcript_blocks BEGIN SELECT RAISE(FAIL, 'snapshot unavailable'); END")
        .execute(&pool).await.unwrap();
    f.mock.release.notify_one();
    wait_failed(&f).await;
    assert_eq!(
        f.db.storage
            .server_command(f.id, "one", "Hello")
            .await
            .unwrap()
            .unwrap()
            .state,
        "accepted"
    );
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(
            event.event,
            super::sink::SessionEvent::Completion(_)
        ));
    }
    f.controller.release(Duration::from_secs(5)).await.unwrap();
}

#[test]
fn snapshot_plus_events_reconstructs_user_and_assistant_transcript() {
    use super::sink::{SessionEvent, SessionSink};
    use crate::output::OutputSink;
    use crate::protocol::TranscriptItem;

    let sink = SessionSink::new(vec![TranscriptItem::UserMessage {
        text: "Earlier message".into(),
    }]);
    sink.assistant_delta("Earlier response");
    sink.assistant_done();
    let (watermark, mut reconstructed, mut events) = sink.subscribe();

    sink.user_message("Nowa wiadomość\nfrom another client");
    sink.assistant_delta("First ");
    sink.assistant_delta("response");
    sink.assistant_done();
    sink.user_message("Next message");
    sink.assistant_delta("Still streaming");
    let (fresh_watermark, fresh_transcript, _) = sink.subscribe();

    // A client applies only the public events, not the sink's recorder logic.
    let mut assistant = String::new();
    for sequence in watermark + 1..=fresh_watermark {
        let event = events.try_recv().unwrap();
        assert_eq!(event.sequence, sequence);
        match event.event {
            SessionEvent::UserMessage(text) => {
                assert!(assistant.is_empty(), "previous response must be complete");
                reconstructed.push(TranscriptItem::UserMessage { text });
            }
            SessionEvent::AssistantDelta(text) => assistant.push_str(&text),
            SessionEvent::AssistantDone => {
                reconstructed.push(TranscriptItem::AssistantMessage {
                    text: std::mem::take(&mut assistant),
                });
            }
            event => panic!("unexpected event {event:?}"),
        }
    }
    if !assistant.is_empty() {
        reconstructed.push(TranscriptItem::AssistantMessage { text: assistant });
    }
    assert_eq!(
        serde_json::to_value(reconstructed).unwrap(),
        serde_json::to_value(fresh_transcript).unwrap(),
    );
}

#[test]
fn sink_retracts_attempts_preserves_typed_results_and_orders_subscription() {
    use crate::output::{OutputSink, ToolExecutionStatus};
    let sink = super::sink::SessionSink::new(Vec::new());
    sink.assistant_delta("retained");
    sink.assistant_done();
    sink.attempt_started();
    sink.thinking("discarded reasoning");
    sink.assistant_delta("discarded response");
    sink.attempt_discarded();
    sink.tool_started("call", "read", "{}");
    sink.tool_output("call", "transient");
    let mut result =
        crate::protocol::ToolResult::legacy("full result".into(), ToolExecutionStatus::Succeeded);
    result.background_task_id = Some("background-id".into());
    sink.tool_result("call", result.clone());
    let (watermark, transcript, mut events) = sink.subscribe();
    let json = serde_json::to_string(&transcript).unwrap();
    assert!(json.contains("retained"));
    assert!(json.contains("background-id"));
    assert!(!json.contains("discarded response"));
    assert!(!json.contains("discarded reasoning"));
    sink.tool_result("call", result.clone());
    let event = events.try_recv().unwrap();
    assert_eq!(event.sequence, watermark + 1);
    match event.event {
        super::sink::SessionEvent::ToolResult { result: actual, .. } => {
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(result).unwrap()
            );
        }
        event => panic!("unexpected event {event:?}"),
    }
}

#[tokio::test]
async fn heartbeat_failure_cancels_run_and_blocks_new_work() {
    let f = Fixture::new().await;
    f.resume().await;
    f.controller.message(f.id, "one", "Hello").await.unwrap();
    f.wait_calls(1).await;
    let pool = failure_pool(&f).await;
    sqlx::query("CREATE TRIGGER fail_heartbeat BEFORE UPDATE OF last_heartbeat_ms ON sessions BEGIN SELECT RAISE(FAIL, 'heartbeat unavailable'); END")
        .execute(&pool).await.unwrap();
    wait_failed(&f).await;
    assert!(f.controller.message(f.id, "two", "Again").await.is_err());
    assert!(f.db.storage.try_session_owner(f.id).unwrap().is_none());
    sqlx::query("DROP TRIGGER fail_heartbeat")
        .execute(&pool)
        .await
        .unwrap();
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn bridge_forwards_every_interaction_and_interrupt_denies_pending_requests() {
    use crate::interaction::{
        InteractionOutcome as O, InteractionRequest as R, PermissionDecision as P,
    };
    let f = Fixture::new().await;
    f.resume().await;
    let run = f.controller.message(f.id, "one", "Hello").await.unwrap();
    f.wait_calls(1).await;
    let interaction = f.factory.handles.lock().await.interaction.clone().unwrap();
    let requests = [
        R::Permission {
            request_id: 0,
            command: "command".into(),
            origin: None,
        },
        R::WebDomain {
            request_id: 0,
            url: "https://example.com".into(),
            host: "example.com".into(),
            redirected_from: None,
            origin: None,
        },
        R::ExtensionTool {
            request_id: 0,
            id: "mcp.test.tool".into(),
            server: "test".into(),
            capabilities: vec![],
            args_preview: "{}".into(),
        },
        R::HookTrust {
            request_id: 0,
            name: "test".into(),
            event: "PreToolUse".into(),
            action_kind: "shell".into(),
            action_preview: "command".into(),
        },
        R::SandboxEscalation {
            request_id: 0,
            command: "command".into(),
            origin: None,
            kind: crate::interaction::SandboxEscalationKind::SandboxOnly,
        },
        R::Question {
            request_id: 0,
            prompt: "Choose".into(),
            header: None,
            options: vec![],
            multiple: false,
            origin: None,
        },
    ];
    for template in requests {
        let interaction = interaction.clone();
        let waiter = tokio::spawn(async move {
            interaction
                .request(|id| {
                    let mut request = template;
                    match &mut request {
                        R::Permission { request_id, .. }
                        | R::WebDomain { request_id, .. }
                        | R::ExtensionTool { request_id, .. }
                        | R::HookTrust { request_id, .. }
                        | R::SandboxEscalation { request_id, .. }
                        | R::Question { request_id, .. } => *request_id = id,
                    }
                    request
                })
                .await
        });
        let request = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let (snapshot, _) = f.controller.subscribe().await.unwrap();
                if let Some(request) = snapshot.control.pending.first() {
                    break request.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (id, answer) = match request {
            R::SandboxEscalation { request_id, .. } => (
                request_id,
                O::SandboxEscalation(crate::interaction::EscalationDecision::Deny),
            ),
            R::Question { request_id, .. } => (request_id, O::Question(None)),
            R::Permission { request_id, .. }
            | R::WebDomain { request_id, .. }
            | R::ExtensionTool { request_id, .. }
            | R::HookTrust { request_id, .. } => (request_id, O::Permission(P::Deny)),
        };
        assert!(
            !waiter.is_finished(),
            "bridge must never grant unattended permission"
        );
        f.controller
            .respond(f.id, &run.run_id, id, answer)
            .await
            .unwrap();
        assert!(waiter.await.unwrap().is_ok());
    }
    f.controller.interrupt(f.id).await.unwrap();
    f.wait_idle().await;
    assert!(
        f.controller
            .subscribe()
            .await
            .unwrap()
            .0
            .control
            .pending
            .is_empty()
    );
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn domain_snapshot_retains_plan_guard_until_subscription() {
    let f = Fixture::new().await;
    f.resume().await;
    let handles = f.factory.handles.lock().await;
    let plan = handles.plan.clone().unwrap();
    let todos = handles.todos.clone().unwrap();
    drop(handles);
    let todo_guard = todos.lock().await;
    let mut subscribing = Box::pin(f.controller.subscribe());
    assert!(futures::poll!(&mut subscribing).is_pending());
    // The subscriber has captured the plan but is blocked on todos. An update
    // must not publish a newer plan before the subscription watermark is taken.
    assert!(plan.try_lock().is_err());
    drop(todo_guard);
    let (snapshot, mut events) = subscribing.await.unwrap();
    let (_, sink) = f.controller.snapshot().await.unwrap();
    let mut updated = plan.lock().await;
    updated.title = "updated after snapshot".into();
    updated.revision += 1;
    let mut updated_todos = todos.lock().await;
    updated_todos.set_todos(vec![crate::todo::TodoItem {
        content: "new task".into(),
        status: crate::todo::TodoStatus::Pending,
    }]);
    sink.domain_updated(&updated, updated_todos.todos());
    drop(updated_todos);
    drop(updated);
    let mut reconstructed_plan = snapshot.plan;
    let mut reconstructed_todos = snapshot.todos;
    while let Ok(event) = events.try_recv() {
        assert!(event.sequence > snapshot.watermark);
        match event.event {
            super::sink::SessionEvent::Plan(value) => reconstructed_plan = *value,
            super::sink::SessionEvent::Todos(value) => reconstructed_todos = value,
            _ => {}
        }
    }
    let (fresh, _) = f.controller.subscribe().await.unwrap();
    assert_eq!(
        serde_json::to_value(reconstructed_plan).unwrap(),
        serde_json::to_value(fresh.plan).unwrap()
    );
    assert_eq!(reconstructed_todos, fresh.todos);
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[test]
fn domain_events_are_ordered_and_coalesced_without_another_ledger() {
    let sink = super::sink::SessionSink::new(Vec::new());
    let mut plan = crate::plan::PlanDoc::default();
    sink.domain_updated(&plan, &[]);
    let (watermark, _, mut events) = sink.subscribe();
    plan.title = "Plan".into();
    plan.revision += 1;
    let todos = vec![crate::todo::TodoItem {
        content: "Task".into(),
        status: crate::todo::TodoStatus::Pending,
    }];
    sink.domain_updated(&plan, &todos);
    let first = events.try_recv().unwrap();
    let second = events.try_recv().unwrap();
    assert_eq!(
        (first.sequence, second.sequence),
        (watermark + 1, watermark + 2)
    );
    assert!(
        matches!(first.event, super::sink::SessionEvent::Plan(ref value) if value.title == "Plan")
    );
    assert!(matches!(second.event, super::sink::SessionEvent::Todos(ref value) if value == &todos));
    sink.domain_updated(&plan, &todos);
    assert!(matches!(
        events.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn usage_is_cumulative_across_turns_and_resume() {
    let f = Fixture::new().await;
    f.mock.immediate.store(true, Ordering::SeqCst);
    f.resume().await;
    for request in ["one", "two"] {
        f.controller.message(f.id, request, "Hello").await.unwrap();
        f.wait_idle().await;
    }
    let (snapshot, _) = f.controller.subscribe().await.unwrap();
    assert_eq!((snapshot.usage.0, snapshot.usage.1), (20, 6));
    f.controller.release(Duration::from_secs(5)).await.unwrap();
    f.resume().await;
    let (snapshot, _) = f.controller.subscribe().await.unwrap();
    assert_eq!((snapshot.usage.0, snapshot.usage.1), (20, 6));
    f.controller.message(f.id, "three", "Hello").await.unwrap();
    f.wait_idle().await;
    let (snapshot, _) = f.controller.subscribe().await.unwrap();
    assert_eq!((snapshot.usage.0, snapshot.usage.1), (30, 9));
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn forgetting_completed_server_session_removes_only_its_artifacts() {
    let f = Fixture::new().await;
    f.mock.immediate.store(true, Ordering::SeqCst);
    f.resume().await;
    f.controller.message(f.id, "one", "Hello").await.unwrap();
    f.wait_idle().await;
    f.controller.release(Duration::from_secs(5)).await.unwrap();
    let root = f.db.project_path().canonicalize().unwrap();
    let artifacts = super::artifacts::Artifacts::new(f.db.storage.home_dir()).unwrap();
    let records = artifacts
        .list(&f.db.storage, &root.to_string_lossy(), f.id.as_i64())
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    let other = f.controller.create(&f.selection).await.unwrap();
    f.controller.resume(other, &f.selection).await.unwrap();
    f.controller.message(other, "two", "Hello").await.unwrap();
    f.wait_idle().await;
    f.controller.release(Duration::from_secs(5)).await.unwrap();
    let other_records = artifacts
        .list(&f.db.storage, &root.to_string_lossy(), other.as_i64())
        .await
        .unwrap();
    assert_eq!(other_records.len(), 1);
    assert_eq!(
        f.db.storage.forget_session(&root, f.id).await.unwrap(),
        crate::storage::ForgetSessionOutcome::Forgotten
    );
    assert!(
        artifacts
            .list(&f.db.storage, &root.to_string_lossy(), f.id.as_i64())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !f.db
            .storage
            .home_dir()
            .join("server-artifacts")
            .join(&records[0].id)
            .exists()
    );
    assert!(
        artifacts
            .download(
                &f.db.storage,
                &root.to_string_lossy(),
                other.as_i64(),
                &other_records[0].id
            )
            .await
            .is_ok()
    );
    assert!(
        f.db.storage
            .load_runtime_session_snapshot(other)
            .await
            .unwrap()
            .is_some()
    );
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test]
async fn restart_marks_accepted_work_interrupted_without_reexecution() {
    let f = Fixture::new().await;
    f.db.storage
        .admit_server_command(f.id, "abandoned", "Hello", "old-run")
        .await
        .unwrap();
    f.resume().await;
    let retry = f
        .controller
        .message(f.id, "abandoned", "Hello")
        .await
        .unwrap();
    assert_eq!(retry.run_id, "old-run");
    assert_eq!(retry.state, "interrupted");
    assert_eq!(f.mock.calls.load(Ordering::SeqCst), 0);
    f.controller.shutdown(Duration::from_secs(5)).await.unwrap();
}
