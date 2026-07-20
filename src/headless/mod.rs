use std::io::{self, IsTerminal, Read};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use thiserror::Error;
use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentRunResult};
use crate::interaction::InteractionService;
use crate::output::{OutputSink, SharedSink};
use crate::plan::PlanDoc;
use crate::provider::ProviderRegistry;
use crate::recovery::{RecoveryMode, RecoveryWorkspace};
use crate::session::{DEFAULT_PROVIDER_ID, SessionStore};
use crate::storage::{RecoveryId, SessionId, SessionStatus, Storage};
use crate::todo::TodoStore;
use crate::tool::ApprovalLevel;
use crate::tool::SharedActiveSessionId;

mod sink;
mod transcript;

pub(crate) use sink::HeadlessSink;
use transcript::TranscriptRecorder;

const BACKGROUND_TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

impl OutputFormat {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            "stream-json" => Some(Self::StreamJson),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptSource {
    Inline(String),
    Stdin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeadlessConfig {
    pub prompt: PromptSource,
    pub output_format: OutputFormat,
    pub resume: Option<crate::cli::ResumeTarget>,
    pub autonomy: Option<ApprovalLevel>,
    pub model: Option<String>,
    pub reasoning: Option<crate::provider::ReasoningSelection>,
    pub max_turns: Option<usize>,
    /// Optional wall-time ceiling for each provider generation attempt.
    pub max_generation_duration: Option<Duration>,
    /// Optional combined reasoning + assistant output cap per provider attempt.
    pub max_streamed_chars: Option<usize>,
    /// Optional wall-time ceiling for each individual tool execution.
    pub max_tool_duration: Option<Duration>,
    pub append_system_prompt: Option<String>,
    pub timeout: Option<Duration>,
    pub isolation: RecoveryMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeadlessRunOutcome {
    exit_code: i32,
    session_id: SessionId,
}

impl HeadlessRunOutcome {
    fn from_status(status: HeadlessStatus, session_id: SessionId) -> Self {
        Self {
            exit_code: status.exit_code(),
            session_id,
        }
    }

    pub(crate) const fn exit_code(self) -> i32 {
        self.exit_code
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HeadlessStatus {
    Completed,
    Failed,
    BudgetExhausted,
    Interrupted,
    Terminated,
    TimedOut,
}

impl HeadlessStatus {
    pub(crate) const fn exit_code(self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Failed | Self::BudgetExhausted => 1,
            Self::Interrupted => 130,
            Self::Terminated => 143,
            Self::TimedOut => 124,
        }
    }

    const fn storage_status(self) -> SessionStatus {
        match self {
            Self::Completed => SessionStatus::Completed,
            Self::Failed => SessionStatus::Failed,
            Self::BudgetExhausted | Self::Interrupted | Self::Terminated | Self::TimedOut => {
                SessionStatus::Interrupted
            }
        }
    }

    const fn completion_status(self) -> crate::completion_report::CompletionStatus {
        match self {
            Self::Completed => crate::completion_report::CompletionStatus::Completed,
            Self::Failed => crate::completion_report::CompletionStatus::Failed,
            Self::BudgetExhausted => crate::completion_report::CompletionStatus::BudgetExhausted,
            Self::Interrupted | Self::Terminated | Self::TimedOut => {
                crate::completion_report::CompletionStatus::Interrupted
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeadlessEvidenceBaseline {
    verification_runs: usize,
    self_review_runs: usize,
    authorization_decision_id: Option<i64>,
}

impl HeadlessEvidenceBaseline {
    async fn capture(agent: &Agent, storage: &Storage, session_id: SessionId) -> Self {
        let authorization_decision_id =
            match storage.recent_authorization_decisions(session_id, 1).await {
                Ok(records) => Some(records.first().map_or(0, |record| record.id)),
                Err(err) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %format!("{err:#}"),
                        "failed to capture authorization evidence baseline"
                    );
                    None
                }
            };
        Self {
            verification_runs: agent.verification_runs().len(),
            self_review_runs: agent.self_review_runs().len(),
            authorization_decision_id,
        }
    }
}

fn run_local_last<T: Clone>(records: &[T], baseline: usize) -> Option<T> {
    records.get(baseline..)?.last().cloned()
}

fn classify_headless_status(
    run_status: HeadlessStatus,
    evidence: &crate::completion_report::CompletionEvidenceSnapshot,
    verification: Option<&crate::verification::VerificationRunRecord>,
) -> HeadlessStatus {
    match crate::completion_report::classify_completion_status(
        run_status.completion_status(),
        evidence,
        verification,
    ) {
        crate::completion_report::CompletionStatus::Failed => HeadlessStatus::Failed,
        _ => run_status,
    }
}

#[derive(Debug, Error)]
pub(crate) enum HeadlessError {
    #[error("{0}")]
    AuthConfig(String),
    #[error(transparent)]
    Runtime(#[from] anyhow::Error),
}

impl HeadlessError {
    pub(crate) const fn exit_code(&self) -> i32 {
        match self {
            Self::AuthConfig(_) => 3,
            Self::Runtime(_) => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderSelection {
    pub provider: String,
    pub model: String,
}

pub(crate) async fn run(config: HeadlessConfig) -> Result<HeadlessRunOutcome, HeadlessError> {
    let sink = Arc::new(HeadlessSink::stdio(config.output_format));
    let result = run_with_recovery(config, sink.clone()).await;
    match result {
        Ok(outcome) => Ok(outcome),
        Err(err) => {
            sink.error(&err.to_string());
            sink.finish_pending_writes()?;
            Err(err)
        }
    }
}

async fn run_with_recovery(
    config: HeadlessConfig,
    sink: Arc<HeadlessSink>,
) -> Result<HeadlessRunOutcome, HeadlessError> {
    let autonomy = resolve_autonomy(
        config.autonomy,
        std::env::var("BONSAI_AUTONOMY").ok().as_deref(),
    )?;
    let storage = Storage::open().await?;
    // Arm the opt-in lifecycle log: the JSONL layer is already installed by
    // init_headless_tracing; the persisted preference just opens the gate.
    crate::logging::set_support_log_enabled(storage.support_log_enabled().await.unwrap_or(false));
    let source_path = std::env::current_dir().context("Failed to determine current directory")?;
    let mut recovery =
        RecoveryWorkspace::prepare(storage.clone(), &source_path, config.isolation, autonomy)
            .await?;
    if let Some(workspace) = recovery.as_ref() {
        sink.status(&format!(
            "Recovery {}: running in isolated worktree {}",
            workspace.id(),
            workspace.execution_path().display()
        ));
    }
    let project_root = recovery
        .as_ref()
        .map(|workspace| workspace.execution_path().to_path_buf())
        .unwrap_or_else(|| source_path.clone());
    let session_project_root = recovery
        .as_ref()
        .map(|workspace| workspace.source_path().to_path_buf())
        .unwrap_or(source_path);
    let recovery_id = recovery.as_ref().map(|workspace| workspace.id().clone());
    let run_result = run_inner(
        config,
        sink.clone(),
        storage,
        autonomy,
        project_root,
        session_project_root,
        recovery_id,
    )
    .await;
    if let Some(workspace) = recovery.as_mut() {
        if let Err(finalize_err) = workspace.finalize().await {
            if run_result.is_ok() {
                return Err(HeadlessError::Runtime(finalize_err));
            }
            tracing::warn!(error = %finalize_err, "failed to finalize recovery point after run failure");
        } else {
            sink.status(&format!(
                "Recovery {} ready. Inspect with: bonsai recovery inspect {}",
                workspace.id(),
                workspace.id()
            ));
        }
    }
    run_result
}

async fn run_inner(
    config: HeadlessConfig,
    sink: Arc<HeadlessSink>,
    storage: Storage,
    autonomy: ApprovalLevel,
    project_root: std::path::PathBuf,
    session_project_root: std::path::PathBuf,
    recovery_id: Option<RecoveryId>,
) -> Result<HeadlessRunOutcome, HeadlessError> {
    run_inner_with_provider_runtime(
        config,
        sink,
        storage,
        autonomy,
        project_root,
        session_project_root,
        recovery_id,
        None,
    )
    .await
}

struct HeadlessProviderRuntime {
    model_catalog: Arc<crate::model_catalog::ModelCatalog>,
    registry: Arc<ProviderRegistry>,
    session_store: SessionStore,
    retry_backoff: crate::agent::RetryBackoff,
}

#[allow(clippy::too_many_arguments)]
async fn run_inner_with_provider_runtime(
    config: HeadlessConfig,
    sink: Arc<HeadlessSink>,
    storage: Storage,
    autonomy: ApprovalLevel,
    project_root: std::path::PathBuf,
    session_project_root: std::path::PathBuf,
    recovery_id: Option<RecoveryId>,
    provider_runtime: Option<HeadlessProviderRuntime>,
) -> Result<HeadlessRunOutcome, HeadlessError> {
    let prompt = resolve_prompt(&config.prompt).await?;
    let mut run_budget = storage.run_budget().await?;
    if let Some(max_turns) = config.max_turns {
        run_budget.max_turns = Some(max_turns);
    }
    if let Some(max_duration) = config.max_generation_duration {
        run_budget.max_generation_seconds = Some(max_duration.as_secs());
    }
    if let Some(max_streamed_chars) = config.max_streamed_chars {
        run_budget.max_output_chars = Some(max_streamed_chars);
    }
    if let Some(max_duration) = config.max_tool_duration {
        run_budget.max_tool_seconds = Some(max_duration.as_secs());
    }
    let project_id = storage.ensure_project(&session_project_root).await?;
    // Headless cannot answer a trust prompt, so project catalog/config files
    // stay inert until an interactive session has trusted this workspace.
    let workspace_trust =
        crate::workspace_trust::WorkspaceTrustGate::load(storage.clone(), project_id).await?;
    let workspace_trusted = matches!(
        workspace_trust.state(),
        crate::workspace_trust::WorkspaceTrust::Trusted
    );
    let provider_runtime = match provider_runtime {
        Some(runtime) => runtime,
        None => {
            let model_catalog = Arc::new(
                match crate::model_catalog::load_catalog_from_home_and_project_with_refresh(
                    storage.home_dir(),
                    workspace_trusted.then_some(project_root.as_path()),
                )
                .await
                {
                    Ok(catalog) => catalog,
                    Err(err) => {
                        tracing::warn!(
                            home = %storage.home_dir().display(),
                            error = %err,
                            "failed to load user model catalog for headless run; falling back to built-in catalog"
                        );
                        crate::model_catalog::ModelCatalog::load_builtin()
                            .map_err(anyhow::Error::from)?
                    }
                },
            );
            let registry = Arc::new(ProviderRegistry::from_catalog(&model_catalog));
            let session_store =
                SessionStore::load_with_storage_and_catalog(&storage, Some(&model_catalog)).await?;
            HeadlessProviderRuntime {
                model_catalog,
                registry,
                session_store,
                retry_backoff: crate::agent::RetryBackoff::Exponential,
            }
        }
    };
    let HeadlessProviderRuntime {
        model_catalog,
        registry,
        mut session_store,
        retry_backoff,
    } = provider_runtime;
    let resume_snapshot = match config.resume {
        Some(target) => {
            Some(resolve_resume_snapshot(&storage, &session_project_root, target).await?)
        }
        None => None,
    };
    let mut transcript_prefix = Vec::new();
    let (selection, current_session_id) = if let Some(snapshot) = resume_snapshot.as_ref() {
        let mut selection =
            select_resumed_provider(&registry, &mut session_store, &model_catalog, snapshot)?;
        if let Some(model) = config.model.as_deref() {
            selection.model = apply_model_override(
                &mut session_store,
                &selection.provider,
                &model_catalog,
                model,
            );
        }
        if let Some(reasoning) = config.reasoning {
            apply_reasoning_override(
                &registry,
                &mut session_store,
                &model_catalog,
                &selection,
                reasoning,
            )?;
        }
        match storage
            .claim_session_for_resume(&session_project_root, snapshot.summary.id)
            .await?
        {
            crate::storage::ResumeSessionOutcome::Resumed => {}
            crate::storage::ResumeSessionOutcome::Live => {
                return Err(HeadlessError::Runtime(anyhow::anyhow!(
                    "Session #{} is live in another bonsai process.",
                    snapshot.summary.id
                )));
            }
            crate::storage::ResumeSessionOutcome::NotFound => {
                return Err(HeadlessError::Runtime(anyhow::anyhow!(
                    "No session #{}.",
                    snapshot.summary.id
                )));
            }
            crate::storage::ResumeSessionOutcome::DifferentProject => {
                return Err(HeadlessError::Runtime(anyhow::anyhow!(
                    "Session #{} belongs to a different project.",
                    snapshot.summary.id
                )));
            }
        }
        storage
            .set_session_run_selection(
                snapshot.summary.id,
                &selection.provider,
                &selection.model,
                session_store.session(&selection.provider).reasoning,
            )
            .await?;
        transcript_prefix = snapshot.transcript.clone();
        (selection, snapshot.summary.id)
    } else {
        let mut selection = select_provider(
            &registry,
            &mut session_store,
            std::env::var("BONSAI_PROVIDER").ok().as_deref(),
        )?;
        if let Some(model) = config.model.as_deref() {
            selection.model = apply_model_override(
                &mut session_store,
                &selection.provider,
                &model_catalog,
                model,
            );
        }
        if let Some(reasoning) = config.reasoning {
            apply_reasoning_override(
                &registry,
                &mut session_store,
                &model_catalog,
                &selection,
                reasoning,
            )?;
        }
        let selected_session = session_store.session(&selection.provider).clone();
        let session_id = storage
            .start_session(
                &session_project_root,
                &selection.provider,
                &selection.model,
                selected_session.reasoning,
            )
            .await?;
        if let Some(title) = crate::session_persist::derive_session_title(&prompt)
            && let Err(err) = storage.set_session_summary(session_id, &title).await
        {
            tracing::warn!(
                session_id = %session_id,
                error = %err,
                "failed to seed headless session title"
            );
        }
        // Correlates this process's log file (keyed by pid + start-millis) with
        // the DB session id, so `bonsai.db` forensics and log tailing meet.
        tracing::info!(
            target: "bonsai::session",
            session_id = %session_id,
            "session attached to this process log"
        );
        (selection, session_id)
    };
    if let Some(recovery_id) = recovery_id.as_ref() {
        storage
            .attach_recovery_session(recovery_id, current_session_id)
            .await?;
        storage
            .record_session_heartbeat(current_session_id, false)
            .await?;
    }
    let interaction = Arc::new(InteractionService::noninteractive());
    let active_session_id: SharedActiveSessionId = Arc::new(Mutex::new(None));
    crate::storage::activate_session_heartbeat(&storage, &active_session_id, current_session_id)
        .await?;
    // Liveness heartbeat: a headless run can work for minutes; peers in the
    // same project root should see it as live rather than a crash leftover.
    // A headless run is working for its whole life (one prompt, then exit), so
    // the busy flag is pinned on.
    let busy_flag: crate::storage::SharedBusyFlag =
        Arc::new(std::sync::atomic::AtomicBool::new(true));
    crate::storage::spawn_heartbeat_writer(storage.clone(), active_session_id.clone(), busy_flag);
    let yolo_mode = crate::yolo::YoloMode::with_level(autonomy);
    let session_store = Arc::new(Mutex::new(session_store));
    let runtime = crate::runtime::RuntimeBuilder::new(crate::runtime::RuntimeBuildRequest {
        surface: crate::runtime::SurfaceKind::Headless,
        storage,
        project_root: project_root.clone(),
        session_project_root: session_project_root.clone(),
        model_catalog,
        registry,
        session_store,
        run_budget,
        interaction,
        active_session_id,
        yolo_mode,
        current_session_id: Some(current_session_id),
        system_prompt_suffix: config.append_system_prompt.clone(),
        workspace_trust,
    })
    .build()
    .await?;
    let crate::runtime::RuntimeCore {
        mut agent,
        storage,
        todo_store,
        plan_store,
        background_tasks,
        terminals,
        peer_bus,
        hooks,
        run_budget,
        ..
    } = runtime;
    agent.set_retry_backoff(retry_backoff);
    if let Some(snapshot) = resume_snapshot.as_ref() {
        todo_store.lock().await.set_todos(snapshot.todos.clone());
        *plan_store.lock().await = snapshot.plan.clone();
    }
    let (terminal_event_stop, terminal_event_task) =
        spawn_terminal_event_forwarder(terminals.subscribe(), sink.clone());
    let conversation_cache_key = storage.conversation_cache_key(current_session_id).await?;
    agent.set_conversation_cache_key(&conversation_cache_key);
    if let Some(snapshot) = resume_snapshot.as_ref() {
        crate::session_persist::restore_agent_state(&mut agent, snapshot).await;
        if config.append_system_prompt.is_some() {
            agent.refresh_system_context_message();
        }
    }

    let active_run_ms_before = storage.begin_session_run(current_session_id).await?;
    let selected_timeout = crate::run_budget::select_runtime_timeout(
        config.timeout,
        run_budget.max_run_duration(),
        run_budget.max_session_active_duration(),
        active_run_ms_before,
    );
    let run_timeout = selected_timeout.map(|(duration, _)| duration);
    let run_timeout_exhaustion = selected_timeout.and_then(|(_, exhaustion)| exhaustion);
    let preflight_exhaustion = selected_timeout
        .filter(|(duration, _)| duration.is_zero())
        .and_then(|(_, exhaustion)| exhaustion);
    let shared_sink: SharedSink = sink.clone();
    let cancellation_token = CancellationToken::new();
    let (cancellation_task, cancel_reason) = spawn_cancellation_watchers(
        cancellation_token.clone(),
        run_timeout,
        run_timeout_exhaustion,
    );

    let verification = crate::verification::resolve_slash_command(
        &prompt,
        &project_root,
        &agent.config().verification,
    )
    .map_err(|message| HeadlessError::Runtime(anyhow::anyhow!(message)))?;
    let evidence_baseline =
        HeadlessEvidenceBaseline::capture(&agent, &storage, current_session_id).await;
    sink.begin_completion_run();
    sink.user_message(&prompt);
    let run_result = if let Some(exhaustion) = preflight_exhaustion {
        Err(anyhow::Error::new(exhaustion))
    } else if let Some(workflow) = verification {
        agent
            .begin_verification_run(workflow.kind, &workflow.checks, &workflow.prompt)
            .await;
        agent
            .run_current_context(cancellation_token, shared_sink)
            .await
    } else {
        agent.run(&prompt, cancellation_token, shared_sink).await
    };
    cancellation_task.abort();
    let active_run_ms = storage.finish_session_run(current_session_id).await?;
    let _ = background_tasks
        .stop_all_running(BACKGROUND_TASK_SHUTDOWN_TIMEOUT)
        .await;
    let _ = terminals
        .stop_all_running(BACKGROUND_TASK_SHUTDOWN_TIMEOUT)
        .await;
    let _ = terminal_event_stop.send(());
    let _ = terminal_event_task.await;

    let (run_status, output, budget_exhaustion) = match run_result {
        Ok(AgentRunResult::Completed(output)) => (HeadlessStatus::Completed, output, None),
        Ok(AgentRunResult::Interrupted(output)) => match cancel_reason
            .get()
            .copied()
            .unwrap_or(CancelReason::Interrupted)
        {
            CancelReason::BudgetExhausted(reason) => {
                (HeadlessStatus::BudgetExhausted, output, Some(reason))
            }
            reason => (reason.status(), output, None),
        },
        Ok(AgentRunResult::Waiting(reason)) => {
            return Err(HeadlessError::Runtime(anyhow::anyhow!(
                "headless agent entered nonterminal wait state: {reason:?}"
            )));
        }
        Err(err) => {
            if let Some(reason) = err
                .downcast_ref::<crate::run_budget::RunBudgetExhaustion>()
                .copied()
            {
                (HeadlessStatus::BudgetExhausted, String::new(), Some(reason))
            } else {
                let agent_failure = crate::provider::agent_failure_detail(&err);
                let persistence_result = persist_headless_snapshots(
                    &storage,
                    current_session_id,
                    &agent,
                    &sink,
                    &todo_store,
                    &plan_store,
                    &transcript_prefix,
                )
                .await;
                fire_session_end_hooks(&hooks, current_session_id).await;
                fire_headless_wake_subscriptions(&peer_bus).await;
                mark_headless_session_status(
                    &storage,
                    current_session_id,
                    SessionStatus::Failed,
                    None,
                )
                .await;
                if let Err(persistence_error) = persistence_result {
                    return Err(HeadlessError::Runtime(anyhow::anyhow!(
                        "agent failed: {agent_failure}; session persistence also failed: {persistence_error:#}"
                    )));
                }
                return Err(HeadlessError::Runtime(anyhow::anyhow!(agent_failure)));
            }
        }
    };
    if let Some(reason) = budget_exhaustion {
        sink.status(&format!(
            "Budget exhausted: {reason}. Partial work and session state were preserved."
        ));
    }
    let usage_totals = agent.usage_totals();
    let verification = run_local_last(
        agent.verification_runs(),
        evidence_baseline.verification_runs,
    );
    let review = run_local_last(agent.self_review_runs(), evidence_baseline.self_review_runs);
    let completion_evidence = sink.completion_evidence();
    let status = classify_headless_status(run_status, &completion_evidence, verification.as_ref());
    let authorization_decisions =
        if let Some(baseline_id) = evidence_baseline.authorization_decision_id {
            match storage
                .recent_authorization_decisions(current_session_id, 500)
                .await
            {
                Ok(decisions) => decisions
                    .into_iter()
                    .filter(|record| record.id > baseline_id)
                    .collect(),
                Err(err) => {
                    tracing::warn!(
                        session_id = %current_session_id,
                        error = %format!("{err:#}"),
                        "failed to load authorization evidence for completion report"
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
    let completion_report = crate::completion_report::CompletionReport::from_evidence(
        status.completion_status(),
        completion_evidence,
        crate::completion_report::CompletionSessionEvidence {
            verification: verification.clone(),
            review,
            authorization_decisions: &authorization_decisions,
            usage: usage_totals,
            session_budget: agent
                .session_budget_usage()
                .with_active_run_ms(active_run_ms),
            budget_exhaustion,
        },
    );
    let mut final_output = HeadlessFinalOutput {
        status,
        output,
        provider: selection.provider,
        model: selection.model,
        session_id: current_session_id.as_i64(),
        usage: HeadlessUsage::from_totals(usage_totals),
        persistence_duration_ms: 0,
        budget_exhaustion,
        verification,
        completion_report,
    };
    sink.record_completion_report(&final_output.completion_report);
    let persistence_started_at = Instant::now();
    if let Err(error) = persist_headless_snapshots(
        &storage,
        current_session_id,
        &agent,
        &sink,
        &todo_store,
        &plan_store,
        &transcript_prefix,
    )
    .await
    {
        fire_session_end_hooks(&hooks, current_session_id).await;
        fire_headless_wake_subscriptions(&peer_bus).await;
        mark_headless_session_status(
            &storage,
            current_session_id,
            SessionStatus::Failed,
            budget_exhaustion,
        )
        .await;
        return Err(HeadlessError::Runtime(error));
    }
    final_output.persistence_duration_ms =
        u64::try_from(persistence_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    let outcome = HeadlessRunOutcome::from_status(status, current_session_id);
    let storage_status = status.storage_status();
    fire_session_end_hooks(&hooks, current_session_id).await;
    fire_headless_wake_subscriptions(&peer_bus).await;
    mark_headless_session_status(
        &storage,
        current_session_id,
        storage_status,
        budget_exhaustion,
    )
    .await;
    sink.finish(&final_output)?;

    Ok(outcome)
}

fn spawn_terminal_event_forwarder(
    mut events: tokio::sync::broadcast::Receiver<crate::terminal::TerminalEvent>,
    sink: Arc<HeadlessSink>,
) -> (oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let (stop_sender, mut stop_receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(event) => forward_terminal_event(&sink, event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = &mut stop_receiver => {
                    while let Ok(event) = events.try_recv() {
                        forward_terminal_event(&sink, event);
                    }
                    break;
                }
            }
        }
    });
    (stop_sender, task)
}

fn forward_terminal_event(sink: &HeadlessSink, event: crate::terminal::TerminalEvent) {
    match event {
        crate::terminal::TerminalEvent::Output {
            tool_call_id: Some(id),
            output,
            ..
        } => sink.tool_output(&id, &output),
        crate::terminal::TerminalEvent::WaitingForInput {
            tool_call_id: Some(id),
            summary,
            ..
        } => sink.tool_output(&id, &summary),
        crate::terminal::TerminalEvent::Finished {
            tool_call_id: Some(id),
            status,
            summary,
            ..
        } => {
            let status = match status {
                crate::terminal::TerminalStatus::Succeeded => {
                    crate::output::ToolExecutionStatus::Succeeded
                }
                crate::terminal::TerminalStatus::Stopped => {
                    crate::output::ToolExecutionStatus::Interrupted
                }
                crate::terminal::TerminalStatus::Failed
                | crate::terminal::TerminalStatus::TimedOut => {
                    crate::output::ToolExecutionStatus::Failed
                }
                crate::terminal::TerminalStatus::Running => {
                    crate::output::ToolExecutionStatus::Started
                }
            };
            sink.tool_finished(&id, &summary, status);
        }
        crate::terminal::TerminalEvent::Started { .. }
        | crate::terminal::TerminalEvent::Output { .. }
        | crate::terminal::TerminalEvent::WaitingForInput { .. }
        | crate::terminal::TerminalEvent::Finished { .. }
        | crate::terminal::TerminalEvent::Removed { .. } => {}
    }
}

async fn fire_headless_wake_subscriptions(peer_bus: &crate::peer::PeerBus) {
    match peer_bus.fire_own_wake_subscriptions().await {
        Ok(requesters) if !requesters.is_empty() => {
            tracing::debug!(
                count = requesters.len(),
                "fired headless peer wake subscriptions"
            );
        }
        Ok(_) => {}
        Err(err) => {
            tracing::debug!(error = %format!("{err:#}"), "headless wake-subscription firing failed");
        }
    }
}

async fn persist_headless_snapshots(
    storage: &Storage,
    session_id: SessionId,
    agent: &Agent,
    sink: &HeadlessSink,
    todo_store: &Arc<Mutex<TodoStore>>,
    plan_store: &Arc<Mutex<PlanDoc>>,
    transcript_prefix: &[crate::tui::app::TranscriptItem],
) -> Result<()> {
    let agent_snapshot = crate::session_persist::AgentStateSnapshot::capture(agent);
    let mut transcript = transcript_prefix.to_vec();
    transcript.extend(sink.take_transcript());
    let todos = todo_store.lock().await.todos().to_vec();
    let plan = plan_store.lock().await.clone();
    let agent_peer_delivery_receipts = agent_snapshot.peer_delivery_receipts().to_vec();
    let snapshot = crate::session_persist::SessionSnapshotData {
        transcript: &transcript,
        plan: &plan,
        todos: &todos,
        agent: Some(&agent_snapshot),
        fallback_usage: None,
        ui_peer_delivery_receipts: &[],
        agent_peer_delivery_receipts: &agent_peer_delivery_receipts,
    };
    crate::session_persist::SessionSnapshotWriter::new(storage, session_id)
        .persist(
            snapshot,
            crate::session_persist::SessionSnapshotSignatures::default(),
        )
        .await
        .context("Failed to persist headless session snapshot")?;
    Ok(())
}

/// SessionEnd, capped independently of any hook's own configured
/// timeout — a one-shot `-p` run must still exit promptly.
async fn fire_session_end_hooks(hooks: &Arc<crate::hooks::HookEngine>, session_id: SessionId) {
    const SESSION_END_HOOK_TIMEOUT: Duration = Duration::from_secs(5);
    match tokio::time::timeout(
        SESSION_END_HOOK_TIMEOUT,
        hooks.fire(
            crate::hooks::HookEvent::SessionEnd,
            crate::hooks::HookContext::default(),
        ),
    )
    .await
    {
        Ok(outcome) => {
            for warning in &outcome.warnings {
                tracing::warn!(session_id = %session_id, %warning, "SessionEnd hook warning");
            }
        }
        Err(_) => {
            tracing::warn!(
                session_id = %session_id,
                "SessionEnd hooks did not finish within the shutdown cap"
            );
        }
    }
}

async fn mark_headless_session_status(
    storage: &Storage,
    session_id: SessionId,
    status: SessionStatus,
    reason: Option<crate::run_budget::RunBudgetExhaustion>,
) {
    if let Err(err) = storage
        .mark_session_termination(session_id, status.clone(), reason)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            status = status.as_db_str(),
            error = %err,
            "failed to mark headless session status"
        );
    }
}

async fn resolve_prompt(source: &PromptSource) -> Result<String, HeadlessError> {
    match source {
        PromptSource::Inline(prompt) => Ok(prompt.clone()),
        PromptSource::Stdin => tokio::task::spawn_blocking(|| -> Result<String> {
            let mut stdin = io::stdin();
            if stdin.is_terminal() {
                anyhow::bail!(
                    "-p without a prompt reads stdin, but stdin is a terminal. Pass a prompt, pipe input, or use --print=<text> for prompts starting with '-'."
                );
            }
            let mut input = String::new();
            stdin
                .read_to_string(&mut input)
                .context("Failed to read prompt from stdin")?;
            if input.trim().is_empty() {
                anyhow::bail!("stdin prompt is empty; pass a prompt or pipe non-empty input.");
            }
            Ok(input)
        })
        .await
        .context("Failed to read prompt from stdin")?
        .map_err(HeadlessError::Runtime),
    }
}

pub(crate) fn resolve_autonomy(
    flag: Option<ApprovalLevel>,
    env: Option<&str>,
) -> Result<ApprovalLevel, HeadlessError> {
    if let Some(level) = flag {
        return Ok(level);
    }
    let Some(value) = env.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(ApprovalLevel::Ask);
    };
    ApprovalLevel::parse(value).ok_or_else(|| {
        HeadlessError::AuthConfig(format!(
            "Invalid BONSAI_AUTONOMY '{value}'. Expected one of: ask, conservative, balanced, auto-accept, yolo."
        ))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelReason {
    Interrupted,
    Terminated,
    TimedOut,
    BudgetExhausted(crate::run_budget::RunBudgetExhaustion),
}

impl CancelReason {
    const fn status(self) -> HeadlessStatus {
        match self {
            Self::Interrupted => HeadlessStatus::Interrupted,
            Self::Terminated => HeadlessStatus::Terminated,
            Self::TimedOut => HeadlessStatus::TimedOut,
            Self::BudgetExhausted(_) => HeadlessStatus::BudgetExhausted,
        }
    }
}

fn spawn_cancellation_watchers(
    cancellation_token: CancellationToken,
    timeout: Option<Duration>,
    timeout_exhaustion: Option<crate::run_budget::RunBudgetExhaustion>,
) -> (tokio::task::JoinHandle<()>, Arc<OnceLock<CancelReason>>) {
    let reason = Arc::new(OnceLock::new());
    let task_reason = reason.clone();
    let handle = tokio::spawn(async move {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if result.is_ok() {
                    let _ = task_reason.set(CancelReason::Interrupted);
                    cancellation_token.cancel();
                }
            }
            () = wait_for_sigterm() => {
                let _ = task_reason.set(CancelReason::Terminated);
                cancellation_token.cancel();
            }
            () = wait_for_timeout(timeout) => {
                let reason = timeout_exhaustion
                    .map_or(CancelReason::TimedOut, CancelReason::BudgetExhausted);
                let _ = task_reason.set(reason);
                cancellation_token.cancel();
            }
        }
    });
    (handle, reason)
}

async fn wait_for_timeout(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending::<()>().await,
    }
}

#[cfg(unix)]
async fn wait_for_sigterm() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut signal) => {
            let _ = signal.recv().await;
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to install SIGTERM handler");
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_sigterm() {
    std::future::pending::<()>().await;
}

async fn resolve_resume_snapshot(
    storage: &Storage,
    project_root: &std::path::Path,
    target: crate::cli::ResumeTarget,
) -> Result<crate::storage::SessionSnapshot, HeadlessError> {
    let session_id = match target {
        crate::cli::ResumeTarget::Latest => storage
            .recent_sessions_for_project(project_root, 1)
            .await?
            .into_iter()
            .next()
            .map(|summary| summary.id)
            .ok_or_else(|| {
                HeadlessError::Runtime(anyhow::anyhow!(
                    "No prior session for this project; use -p without -c to start one."
                ))
            })?,
        crate::cli::ResumeTarget::Session(id) => SessionId::from_raw(id),
    };
    if storage.is_session_live(session_id).await? {
        return Err(HeadlessError::Runtime(anyhow::anyhow!(
            "Session #{session_id} is live in another bonsai process."
        )));
    }
    let snapshot = storage
        .load_runtime_session_snapshot(session_id)
        .await?
        .ok_or_else(|| HeadlessError::Runtime(anyhow::anyhow!("No session #{session_id}.")))?;
    let current_project = crate::storage::canonical_project_path(project_root);
    if snapshot.summary.project_path != current_project {
        return Err(HeadlessError::Runtime(anyhow::anyhow!(
            "Session #{} belongs to a different project ({}); resume it from there.",
            session_id,
            snapshot.summary.project_path
        )));
    }
    Ok(snapshot)
}

fn select_resumed_provider(
    registry: &ProviderRegistry,
    session_store: &mut SessionStore,
    catalog: &crate::model_catalog::ModelCatalog,
    snapshot: &crate::storage::SessionSnapshot,
) -> Result<ProviderSelection, HeadlessError> {
    let summary = &snapshot.summary;
    let factory = registry.get(&summary.provider_id).ok_or_else(|| {
        HeadlessError::AuthConfig(format!(
            "Provider '{}' saved in session #{} is no longer available. Restore its provider configuration, or start a new session and choose an available provider with /provider.",
            summary.provider_id, summary.id
        ))
    })?;
    let metadata = factory.metadata();
    session_store.ensure_provider(&summary.provider_id);
    if !factory.is_authorized(session_store.session(&summary.provider_id)) {
        return Err(HeadlessError::AuthConfig(format!(
            "Provider '{}' is not authorized; {}",
            summary.provider_id,
            authorization_hint(metadata)
        )));
    }
    let resolved = crate::model_resolution::resolved_model_for_provider_model(
        Some(catalog),
        &summary.provider_id,
        &summary.model,
    );
    session_store.set_active_model_target(
        &summary.provider_id,
        resolved.as_ref().map(|model| model.connection_id.clone()),
        resolved.as_ref().map(|model| model.model_id.clone()),
        &summary.model,
    );
    let current_model = session_store.session(&summary.provider_id).model.clone();
    session_store
        .session_mut(&summary.provider_id)
        .set_model_reasoning(metadata, &current_model, summary.reasoning);
    Ok(ProviderSelection {
        provider: summary.provider_id.clone(),
        model: current_model,
    })
}

pub(crate) fn select_provider(
    registry: &ProviderRegistry,
    session_store: &mut SessionStore,
    provider_override: Option<&str>,
) -> Result<ProviderSelection, HeadlessError> {
    let factory = match provider_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(provider_id) => registry.get(provider_id).ok_or_else(|| {
            HeadlessError::AuthConfig(format!(
                "Unknown provider '{provider_id}' from BONSAI_PROVIDER"
            ))
        })?,
        None => registry
            .get(session_store.current_kind_id())
            .or_else(|| registry.get(DEFAULT_PROVIDER_ID))
            .ok_or_else(|| {
                HeadlessError::AuthConfig(format!(
                    "Default provider '{DEFAULT_PROVIDER_ID}' is not registered"
                ))
            })?,
    };

    let metadata = factory.metadata();
    session_store.ensure_provider(&metadata.id);
    session_store.set_current_kind_id(metadata.id.as_ref());
    let session = session_store.session(&metadata.id);
    if !factory.is_authorized(session) {
        return Err(HeadlessError::AuthConfig(format!(
            "Provider '{}' is not authorized; {}",
            metadata.id,
            authorization_hint(metadata)
        )));
    }

    Ok(ProviderSelection {
        provider: metadata.id.to_string(),
        model: session.model.clone(),
    })
}

pub(crate) fn apply_model_override(
    session_store: &mut SessionStore,
    provider_id: &str,
    catalog: &crate::model_catalog::ModelCatalog,
    model: &str,
) -> String {
    let resolved = crate::model_resolution::resolved_model_for_provider_model(
        Some(catalog),
        provider_id,
        model,
    );
    session_store.set_active_model_target(
        provider_id,
        resolved.as_ref().map(|model| model.connection_id.clone()),
        resolved.as_ref().map(|model| model.model_id.clone()),
        model,
    );
    session_store.session(provider_id).model.clone()
}

fn apply_reasoning_override(
    registry: &ProviderRegistry,
    session_store: &mut SessionStore,
    catalog: &crate::model_catalog::ModelCatalog,
    selection: &ProviderSelection,
    reasoning: crate::provider::ReasoningSelection,
) -> Result<(), HeadlessError> {
    let factory = registry.get(&selection.provider).ok_or_else(|| {
        HeadlessError::AuthConfig(format!("Unknown provider '{}'.", selection.provider))
    })?;
    // Catalog-aware validation: a catalog-defined provider (DeepSeek, GLM, Kimi,
    // …) declares its reasoning options per model in the catalog, but the flat
    // provider metadata reports NO_REASONING (capabilities_for_model is
    // model-agnostic). The metadata-only path would wrongly reject a valid
    // effort like DeepSeek's `high`, so resolve support through the catalog and
    // fall back to metadata only when the catalog has nothing.
    let applied = crate::model_resolution::normalize_reasoning_for_provider_model(
        Some(catalog),
        &selection.provider,
        &selection.model,
        factory.metadata(),
        reasoning,
    );
    if applied != reasoning {
        return Err(HeadlessError::AuthConfig(format!(
            "Model '{}' on provider '{}' does not support reasoning effort '{}'.",
            selection.model, selection.provider, reasoning
        )));
    }
    session_store
        .session_mut(&selection.provider)
        .store_model_reasoning(&selection.model, applied);
    Ok(())
}

fn authorization_hint(metadata: &crate::provider::ProviderMetadata) -> String {
    if let Some(var) = metadata.env_var_api_key.as_deref() {
        return format!("set {var} or run /authorize {}", metadata.id);
    }
    format!("run /authorize {} in the TUI", metadata.id)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct HeadlessFinalOutput {
    pub status: HeadlessStatus,
    pub output: String,
    pub provider: String,
    pub model: String,
    pub session_id: i64,
    pub usage: HeadlessUsage,
    /// Wall-clock time spent durably writing the final shared session snapshot.
    pub persistence_duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_exhaustion: Option<crate::run_budget::RunBudgetExhaustion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<crate::verification::VerificationRunRecord>,
    pub completion_report: crate::completion_report::CompletionReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct HeadlessUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cost_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_cache: Option<HeadlessInputCacheUsage>,
}

impl HeadlessUsage {
    fn from_totals(totals: crate::agent::UsageTotals) -> Self {
        let prompt_tokens = totals.prompt_tokens;
        let completion_tokens = totals.completion_tokens;
        let total_tokens = prompt_tokens.saturating_add(completion_tokens);
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cost_micros: if total_tokens == 0 {
                None
            } else {
                totals.cost_micros
            },
            input_cache: totals.input_cache.map(HeadlessInputCacheUsage::from),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct HeadlessInputCacheUsage {
    pub read_tokens: u64,
    pub creation_tokens: u64,
    pub total_input_tokens: u64,
    pub hit_rate_percent: Option<u64>,
}

impl From<crate::provider::InputCacheUsage> for HeadlessInputCacheUsage {
    fn from(value: crate::provider::InputCacheUsage) -> Self {
        Self {
            read_tokens: value.read_tokens,
            creation_tokens: value.creation_tokens,
            total_input_tokens: value.total_input_tokens,
            hit_rate_percent: value.hit_rate_percent(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{LazyLock, Mutex as StdMutex};

    use async_trait::async_trait;

    use super::*;
    use crate::provider::{
        AuthInput, AuthorizeOutcome, NO_PARAMETERS, NO_REASONING, Protocol, Provider,
        ProviderCapabilities, ProviderFactory, ProviderFailure, ProviderMetadata, StreamedResponse,
    };
    use crate::session::{ProviderSession, SessionStore};
    use crate::storage::{PeerMessageKind, test_utils::TestStorage};

    static TEST_A_METADATA: LazyLock<ProviderMetadata> = LazyLock::new(|| {
        ProviderMetadata::new(
            "provider-a",
            "Provider A",
            "model-a",
            "https://a.example/v1",
            Some("PROVIDER_A_API_KEY"),
            Some("PROVIDER_A_MODEL"),
            Some("PROVIDER_A_BASE_URL"),
            &["model-a"],
            Protocol::OpenAiChat,
            ProviderCapabilities::new(NO_REASONING, NO_PARAMETERS),
            "chat/completions",
        )
    });

    static TEST_B_METADATA: LazyLock<ProviderMetadata> = LazyLock::new(|| {
        ProviderMetadata::new(
            "provider-b",
            "Provider B",
            "model-b",
            "https://b.example/v1",
            Some("PROVIDER_B_API_KEY"),
            Some("PROVIDER_B_MODEL"),
            Some("PROVIDER_B_BASE_URL"),
            &["model-b"],
            Protocol::OpenAiChat,
            ProviderCapabilities::new(NO_REASONING, NO_PARAMETERS),
            "chat/completions",
        )
    });

    struct TestFactory {
        metadata: &'static ProviderMetadata,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ProviderScenario {
        ExpiredAuthorization,
        Offline,
        DisconnectAfterOutput,
        RemovedTarget,
        Success,
    }

    struct ScenarioFactory {
        scenario: Arc<StdMutex<ProviderScenario>>,
        attempts: Arc<AtomicUsize>,
    }

    struct ScenarioProvider {
        scenario: Arc<StdMutex<ProviderScenario>>,
        attempts: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for ScenarioProvider {
        async fn chat_stream(
            &self,
            _messages: &[async_openai::types::chat::ChatCompletionRequestMessage],
            _tools: &[async_openai::types::chat::ChatCompletionTool],
            _cancellation_token: CancellationToken,
            sink: SharedSink,
        ) -> crate::provider::ProviderResult<StreamedResponse> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let scenario = *self
                .scenario
                .lock()
                .expect("provider scenario lock should remain available");
            match scenario {
                ProviderScenario::ExpiredAuthorization => Err(ProviderFailure::Http {
                    status: 401,
                    message: "credential expired".to_string(),
                    retry_after_secs: None,
                }),
                ProviderScenario::Offline => Err(ProviderFailure::transport("offline")),
                ProviderScenario::DisconnectAfterOutput => {
                    sink.assistant_delta("discarded partial response");
                    Err(ProviderFailure::transport("connection lost mid-stream"))
                }
                ProviderScenario::RemovedTarget => Err(ProviderFailure::Http {
                    status: 404,
                    message: "selected model not found".to_string(),
                    retry_after_secs: None,
                }),
                ProviderScenario::Success => {
                    sink.assistant_delta("recovered response");
                    sink.assistant_done();
                    Ok(StreamedResponse {
                        content: "recovered response".to_string(),
                        terminal: crate::provider::StreamTerminal::Completed(
                            crate::provider::FinishReason::Stop,
                        ),
                        ..StreamedResponse::default()
                    })
                }
            }
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl ProviderFactory for ScenarioFactory {
        fn metadata(&self) -> &ProviderMetadata {
            crate::provider::metadata_for("opencode")
                .expect("built-in OpenCode metadata should exist")
        }

        fn build_target(
            &self,
            _session: &ProviderSession,
            _target: &crate::model_catalog::RunTarget,
        ) -> Box<dyn Provider> {
            Box::new(ScenarioProvider {
                scenario: self.scenario.clone(),
                attempts: self.attempts.clone(),
            })
        }

        async fn authorize(&self, _input: AuthInput) -> anyhow::Result<AuthorizeOutcome> {
            Ok(AuthorizeOutcome::new("token"))
        }

        fn is_authorized(&self, _session: &ProviderSession) -> bool {
            true
        }

        fn clear_authorization(&self, session: &mut ProviderSession) {
            session.api_key.clear();
        }

        async fn list_models(&self, _session: &ProviderSession) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl ProviderFactory for TestFactory {
        fn metadata(&self) -> &ProviderMetadata {
            self.metadata
        }

        async fn authorize(&self, _input: AuthInput) -> anyhow::Result<AuthorizeOutcome> {
            Ok(AuthorizeOutcome::new("token"))
        }

        fn is_authorized(&self, session: &ProviderSession) -> bool {
            !session.api_key.trim().is_empty()
        }

        fn clear_authorization(&self, session: &mut ProviderSession) {
            session.api_key.clear();
        }

        async fn list_models(&self, _session: &ProviderSession) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }

        async fn list_available_models(
            &self,
            _session: &ProviderSession,
        ) -> anyhow::Result<crate::model_catalog::LiveModelAvailability> {
            Ok(crate::model_catalog::LiveModelAvailability::default())
        }
    }

    fn test_registry() -> ProviderRegistry {
        ProviderRegistry::new(vec![
            Arc::new(TestFactory {
                metadata: &TEST_A_METADATA,
            }),
            Arc::new(TestFactory {
                metadata: &TEST_B_METADATA,
            }),
        ])
    }

    fn test_store() -> SessionStore {
        let mut providers = HashMap::new();
        providers.insert(
            "provider-a".to_string(),
            ProviderSession::new(
                "sk-a".to_string(),
                TEST_A_METADATA.default_base_url.to_string(),
                TEST_A_METADATA.default_model.to_string(),
            ),
        );
        providers.insert(
            "provider-b".to_string(),
            ProviderSession::new(
                "sk-b".to_string(),
                TEST_B_METADATA.default_base_url.to_string(),
                TEST_B_METADATA.default_model.to_string(),
            ),
        );
        SessionStore::from_persistent_parts(
            "provider-a".to_string(),
            None,
            None,
            providers,
            Default::default(),
            Default::default(),
            String::new(),
        )
    }

    fn scenario_factory(
        scenario: ProviderScenario,
    ) -> (
        Arc<ScenarioFactory>,
        Arc<StdMutex<ProviderScenario>>,
        Arc<AtomicUsize>,
    ) {
        let scenario = Arc::new(StdMutex::new(scenario));
        let attempts = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(ScenarioFactory {
                scenario: scenario.clone(),
                attempts: attempts.clone(),
            }),
            scenario,
            attempts,
        )
    }

    fn scenario_runtime(factory: Arc<ScenarioFactory>) -> HeadlessProviderRuntime {
        let model_catalog = Arc::new(
            crate::model_catalog::ModelCatalog::load_builtin()
                .expect("built-in model catalog should load"),
        );
        let registry = Arc::new(ProviderRegistry::new(vec![factory]));
        let mut session_store = SessionStore::default();
        session_store.ensure_provider("opencode");
        session_store.session_mut("opencode").api_key = "test-key".to_string();
        session_store.set_current_kind_id("opencode");
        HeadlessProviderRuntime {
            model_catalog,
            registry,
            session_store,
            retry_backoff: crate::agent::RetryBackoff::Immediate,
        }
    }

    fn headless_test_config(
        prompt: &str,
        resume: Option<crate::cli::ResumeTarget>,
    ) -> HeadlessConfig {
        HeadlessConfig {
            prompt: PromptSource::Inline(prompt.to_string()),
            output_format: OutputFormat::Json,
            resume,
            autonomy: None,
            model: None,
            reasoning: None,
            max_turns: None,
            max_generation_duration: None,
            max_streamed_chars: None,
            max_tool_duration: None,
            append_system_prompt: None,
            timeout: None,
            isolation: RecoveryMode::Off,
        }
    }

    async fn run_headless_scenario(
        fixture: &TestStorage,
        config: HeadlessConfig,
        runtime: HeadlessProviderRuntime,
    ) -> Result<HeadlessRunOutcome, HeadlessError> {
        let sink = Arc::new(HeadlessSink::new(
            OutputFormat::Json,
            Box::new(std::io::sink()),
            Box::new(std::io::sink()),
        ));
        let project_root = fixture.project_path().to_path_buf();
        run_inner_with_provider_runtime(
            config,
            sink,
            fixture.storage.clone(),
            ApprovalLevel::Ask,
            project_root.clone(),
            project_root,
            None,
            Some(runtime),
        )
        .await
    }

    #[test]
    fn select_provider_uses_bonsai_provider_override() {
        let registry = test_registry();
        let mut store = test_store();

        let selected = select_provider(&registry, &mut store, Some("provider-b")).unwrap();

        assert_eq!(selected.provider, "provider-b");
        assert_eq!(selected.model, "model-b");
        assert_eq!(store.current_kind_id(), "provider-b");
    }

    #[test]
    fn select_provider_falls_back_to_persisted_current_provider() {
        let registry = test_registry();
        let mut store = test_store();

        let selected = select_provider(&registry, &mut store, None).unwrap();

        assert_eq!(selected.provider, "provider-a");
        assert_eq!(selected.model, "model-a");
    }

    #[test]
    fn select_provider_rejects_unauthorized_provider() {
        let registry = test_registry();
        let mut store = test_store();
        store.session_mut("provider-b").api_key.clear();

        let err = select_provider(&registry, &mut store, Some("provider-b")).unwrap_err();

        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("not authorized"));
    }

    #[tokio::test]
    async fn resumed_session_with_removed_provider_has_actionable_recovery_guidance() {
        let fixture = TestStorage::new().await;
        let session_id = fixture
            .start_session_with("removed-provider", "removed-model")
            .await;
        let snapshot = fixture
            .storage
            .load_session_snapshot(session_id)
            .await
            .unwrap()
            .expect("persisted session should load");
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let mut store = test_store();

        let error =
            select_resumed_provider(&test_registry(), &mut store, &catalog, &snapshot).unwrap_err();
        let message = error.to_string();

        assert_eq!(error.exit_code(), 3);
        assert!(message.contains("removed-provider"));
        assert!(message.contains("no longer available"));
        assert!(message.contains("/provider"));
    }

    #[tokio::test]
    async fn resumed_session_preserves_unlisted_model_for_provider_resolution() {
        let fixture = TestStorage::new().await;
        let session_id = fixture
            .start_session_with("provider-a", "retired-model")
            .await;
        let snapshot = fixture
            .storage
            .load_session_snapshot(session_id)
            .await
            .unwrap()
            .expect("persisted session should load");
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let mut store = test_store();

        let selection =
            select_resumed_provider(&test_registry(), &mut store, &catalog, &snapshot).unwrap();

        assert_eq!(selection.provider, "provider-a");
        assert_eq!(selection.model, "retired-model");
    }

    #[tokio::test]
    async fn removed_provider_and_model_fail_without_mutating_committed_selection() {
        let removed_provider_fixture = TestStorage::new().await;
        let removed_provider_session = removed_provider_fixture
            .start_session_with("removed-provider", "removed-model")
            .await;
        removed_provider_fixture
            .storage
            .mark_session_termination(removed_provider_session, SessionStatus::Completed, None)
            .await
            .unwrap();
        let (factory, _scenario, attempts) = scenario_factory(ProviderScenario::Success);

        let provider_error = run_headless_scenario(
            &removed_provider_fixture,
            headless_test_config(
                "resume removed provider",
                Some(crate::cli::ResumeTarget::Session(
                    removed_provider_session.as_i64(),
                )),
            ),
            scenario_runtime(factory),
        )
        .await
        .unwrap_err();

        assert_eq!(provider_error.exit_code(), 3);
        assert!(provider_error.to_string().contains("no longer available"));
        assert!(provider_error.to_string().contains("/provider"));
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
        let provider_snapshot = removed_provider_fixture
            .storage
            .load_session_snapshot(removed_provider_session)
            .await
            .unwrap()
            .expect("original session should remain committed");
        assert_eq!(provider_snapshot.summary.status, SessionStatus::Completed);
        assert_eq!(provider_snapshot.summary.provider_id, "removed-provider");
        assert_eq!(provider_snapshot.summary.model, "removed-model");

        let removed_model_fixture = TestStorage::new().await;
        let (factory, scenario, attempts) = scenario_factory(ProviderScenario::RemovedTarget);
        let mut config = headless_test_config("use removed model", None);
        config.model = Some("retired-model".to_string());

        let model_error = run_headless_scenario(
            &removed_model_fixture,
            config,
            scenario_runtime(factory.clone()),
        )
        .await
        .unwrap_err();

        assert_eq!(model_error.exit_code(), 1);
        assert!(model_error.to_string().contains("selected model not found"));
        assert!(model_error.to_string().contains("/model"));
        assert!(model_error.to_string().contains("/provider"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let summaries = removed_model_fixture
            .storage
            .recent_sessions_for_project(removed_model_fixture.project_path(), 10)
            .await
            .unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].status, SessionStatus::Failed);
        assert_eq!(summaries[0].model, "retired-model");

        *scenario
            .lock()
            .expect("provider scenario lock should remain available") = ProviderScenario::Success;
        let mut recovery_config = headless_test_config(
            "retry with replacement model",
            Some(crate::cli::ResumeTarget::Session(summaries[0].id.as_i64())),
        );
        recovery_config.model = Some(
            crate::provider::metadata_for("opencode")
                .expect("built-in OpenCode metadata should exist")
                .default_model
                .to_string(),
        );
        let outcome = run_headless_scenario(
            &removed_model_fixture,
            recovery_config,
            scenario_runtime(factory),
        )
        .await
        .unwrap();

        assert_eq!(outcome.session_id, summaries[0].id);
        assert_eq!(outcome.exit_code(), 0);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn expired_authorization_failure_persists_and_resumes_successfully() {
        let fixture = TestStorage::new().await;
        let (factory, scenario, attempts) =
            scenario_factory(ProviderScenario::ExpiredAuthorization);

        let error = run_headless_scenario(
            &fixture,
            headless_test_config("first prompt", None),
            scenario_runtime(factory.clone()),
        )
        .await
        .unwrap_err();

        assert_eq!(error.exit_code(), 1);
        assert!(error.to_string().contains("credential expired"));
        assert!(error.to_string().contains("/authorize"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let summaries = fixture
            .storage
            .recent_sessions_for_project(fixture.project_path(), 10)
            .await
            .unwrap();
        assert_eq!(summaries.len(), 1);
        let session_id = summaries[0].id;
        assert_eq!(summaries[0].status, SessionStatus::Failed);
        let failed_snapshot = fixture
            .storage
            .load_session_snapshot(session_id)
            .await
            .unwrap()
            .expect("failed session should remain resumable");
        assert!(failed_snapshot.transcript.iter().any(|item| matches!(
            item,
            crate::tui::app::TranscriptItem::UserMessage { text } if text == "first prompt"
        )));

        *scenario
            .lock()
            .expect("provider scenario lock should remain available") = ProviderScenario::Success;
        let outcome = run_headless_scenario(
            &fixture,
            headless_test_config(
                "retry prompt",
                Some(crate::cli::ResumeTarget::Session(session_id.as_i64())),
            ),
            scenario_runtime(factory),
        )
        .await
        .unwrap();

        assert_eq!(outcome.exit_code(), 0);
        assert_eq!(outcome.session_id, session_id);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        let resumed_snapshot = fixture
            .storage
            .load_session_snapshot(session_id)
            .await
            .unwrap()
            .expect("resumed session should load");
        assert_eq!(resumed_snapshot.summary.status, SessionStatus::Completed);
        assert!(resumed_snapshot.transcript.iter().any(|item| matches!(
            item,
            crate::tui::app::TranscriptItem::AssistantMessage { text }
                if text == "recovered response"
        )));
    }

    #[tokio::test]
    async fn offline_and_midstream_failures_retry_boundedly_without_committing_partial_output() {
        for scenario in [
            ProviderScenario::Offline,
            ProviderScenario::DisconnectAfterOutput,
        ] {
            let fixture = TestStorage::new().await;
            let (factory, scenario_state, attempts) = scenario_factory(scenario);

            let error = run_headless_scenario(
                &fixture,
                headless_test_config("preserve this prompt", None),
                scenario_runtime(factory.clone()),
            )
            .await
            .unwrap_err();

            assert_eq!(error.exit_code(), 1);
            assert!(error.to_string().contains("bounded retries"));
            assert_eq!(attempts.load(Ordering::SeqCst), 4);
            let summaries = fixture
                .storage
                .recent_sessions_for_project(fixture.project_path(), 10)
                .await
                .unwrap();
            assert_eq!(summaries.len(), 1);
            assert_eq!(summaries[0].status, SessionStatus::Failed);
            let snapshot = fixture
                .storage
                .load_session_snapshot(summaries[0].id)
                .await
                .unwrap()
                .expect("failed session should remain persisted");
            assert!(snapshot.transcript.iter().any(|item| matches!(
                item,
                crate::tui::app::TranscriptItem::UserMessage { text }
                    if text == "preserve this prompt"
            )));
            assert!(!snapshot.transcript.iter().any(|item| matches!(
                item,
                crate::tui::app::TranscriptItem::AssistantMessage { text }
                    if text.contains("discarded partial response")
            )));

            *scenario_state
                .lock()
                .expect("provider scenario lock should remain available") =
                ProviderScenario::Success;
            let outcome = run_headless_scenario(
                &fixture,
                headless_test_config(
                    "resume after provider recovery",
                    Some(crate::cli::ResumeTarget::Session(summaries[0].id.as_i64())),
                ),
                scenario_runtime(factory),
            )
            .await
            .unwrap();

            assert_eq!(outcome.session_id, summaries[0].id);
            assert_eq!(outcome.exit_code(), 0);
            assert_eq!(attempts.load(Ordering::SeqCst), 5);
        }
    }

    #[test]
    fn provider_auth_failure_detail_includes_recovery_action() {
        let error = anyhow::Error::new(crate::provider::ProviderFailure::Http {
            status: 401,
            message: "expired".to_string(),
            retry_after_secs: None,
        });

        let detail = crate::provider::agent_failure_detail(&error);

        assert!(detail.contains("provider HTTP error (401)"));
        assert!(detail.contains("Action:"));
        assert!(detail.contains("/authorize"));
    }

    #[test]
    fn resolve_autonomy_prefers_flag_then_env_then_ask() {
        assert_eq!(
            resolve_autonomy(Some(ApprovalLevel::AutoAccept), Some("balanced")).unwrap(),
            ApprovalLevel::AutoAccept
        );
        assert_eq!(
            resolve_autonomy(None, Some("balanced")).unwrap(),
            ApprovalLevel::Balanced
        );
        assert_eq!(resolve_autonomy(None, None).unwrap(), ApprovalLevel::Ask);
    }

    #[test]
    fn resolve_autonomy_rejects_invalid_env() {
        let err = resolve_autonomy(None, Some("wild")).unwrap_err();

        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("Invalid BONSAI_AUTONOMY"));
    }

    #[test]
    fn headless_status_exit_codes_match_shell_conventions() {
        assert_eq!(HeadlessStatus::Completed.exit_code(), 0);
        assert_eq!(HeadlessStatus::Failed.exit_code(), 1);
        assert_eq!(HeadlessStatus::BudgetExhausted.exit_code(), 1);
        assert_eq!(HeadlessStatus::Interrupted.exit_code(), 130);
        assert_eq!(HeadlessStatus::TimedOut.exit_code(), 124);
        assert_eq!(HeadlessStatus::Terminated.exit_code(), 143);
    }

    #[test]
    fn terminal_status_is_single_source_for_process_and_storage_outcomes() {
        let cases = [
            (HeadlessStatus::Completed, 0, SessionStatus::Completed),
            (HeadlessStatus::Failed, 1, SessionStatus::Failed),
            (
                HeadlessStatus::BudgetExhausted,
                1,
                SessionStatus::Interrupted,
            ),
            (HeadlessStatus::Interrupted, 130, SessionStatus::Interrupted),
            (HeadlessStatus::TimedOut, 124, SessionStatus::Interrupted),
            (HeadlessStatus::Terminated, 143, SessionStatus::Interrupted),
        ];

        for (status, exit_code, storage_status) in cases {
            let outcome = HeadlessRunOutcome::from_status(status, SessionId::from_raw(1));
            assert_eq!(outcome.exit_code(), exit_code);
            assert_eq!(status.storage_status(), storage_status);
        }
    }

    #[test]
    fn run_local_evidence_does_not_reuse_historical_records() {
        let records = vec!["historical verification"];

        assert_eq!(run_local_last(&records, records.len()), None);
    }

    #[test]
    fn cumulative_session_timeout_competes_with_run_and_cli_timeouts() {
        let selected = crate::run_budget::select_runtime_timeout(
            Some(Duration::from_secs(20)),
            Some(Duration::from_secs(10)),
            Some(Duration::from_secs(60)),
            55_000,
        );

        assert_eq!(
            selected,
            Some((
                Duration::from_secs(5),
                Some(crate::run_budget::RunBudgetExhaustion::SessionTime {
                    limit_seconds: 60,
                    used_seconds: 60,
                })
            ))
        );
    }

    #[test]
    fn exhausted_session_timeout_is_a_zero_duration_preflight() {
        let selected = crate::run_budget::select_runtime_timeout(
            None,
            Some(Duration::from_secs(10)),
            Some(Duration::from_secs(60)),
            61_001,
        );

        assert_eq!(
            selected,
            Some((
                Duration::ZERO,
                Some(crate::run_budget::RunBudgetExhaustion::SessionTime {
                    limit_seconds: 60,
                    used_seconds: 62,
                })
            ))
        );
    }

    #[tokio::test]
    async fn saved_run_timeout_reports_typed_budget_exhaustion() {
        let cancellation = CancellationToken::new();
        let expected = crate::run_budget::RunBudgetExhaustion::RunTime { limit_seconds: 300 };
        let (watcher, reason) = spawn_cancellation_watchers(
            cancellation.clone(),
            Some(Duration::from_millis(1)),
            Some(expected),
        );

        tokio::time::timeout(Duration::from_secs(1), cancellation.cancelled())
            .await
            .unwrap();
        watcher.await.unwrap();
        assert_eq!(reason.get(), Some(&CancelReason::BudgetExhausted(expected)));
    }

    #[tokio::test]
    async fn headless_wake_firing_delivers_done_notice() {
        let fixture = TestStorage::new().await;
        let requester = fixture.start_session().await;
        let target = fixture.start_session().await;
        fixture
            .storage
            .add_wake_subscription(fixture.project_path(), requester, target, "validate", 0)
            .await
            .unwrap();
        let active_session_id: SharedActiveSessionId = Arc::new(Mutex::new(Some(target)));
        let bus = crate::peer::PeerBus::new(
            fixture.storage.clone(),
            active_session_id,
            fixture.project_path().to_path_buf(),
        );

        fire_headless_wake_subscriptions(&bus).await;

        let inbox = fixture
            .storage
            .claim_ui_undelivered_messages(requester)
            .await
            .unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].kind, PeerMessageKind::DoneNotice);
        assert_eq!(inbox[0].from_session_id, target);
    }
}
