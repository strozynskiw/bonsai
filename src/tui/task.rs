use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    Agent, AgentMode, AgentRunResult, QueuedUserMessage, QueuedUserMessageCommand, UserInput,
};
use crate::commands::{
    CommandMessageKind, CommandModalRequest, MemoryCommandRequest, handle_command_with_catalog,
    parse_memory_command,
};
use crate::completion_report::{
    CompletionReport, CompletionSessionEvidence, CompletionStatus, classify_completion_status,
};
use crate::model_catalog::ModelCatalog;
use crate::output::SharedSink;
use crate::plan::PlanDoc;
use crate::provider::ProviderRegistry;
use crate::session::{CredentialPersistence, SessionStore};
use crate::session_activity::{SessionActivity, SessionActivityGate};
use crate::storage::{SessionId, Storage};
use crate::subagent::SubagentRegistry;
use crate::tui::app::TranscriptItem;
use crate::tui::event::{
    CommandOutcomeEvent, CommandOutputEvent, ModalKind, ProviderRunSelection, RuntimeEvent, UiError,
};
use crate::tui::pickers::ModelOption;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    AgentRun,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewWorkflow {
    General,
    Security,
}

struct ActiveTask {
    kind: TaskKind,
    handle: JoinHandle<()>,
    cancellation: Option<CancellationToken>,
    queued_messages: Option<mpsc::UnboundedSender<QueuedUserMessageCommand>>,
    agent_persona: Option<crate::agent::ActivePersona>,
}

#[derive(Clone)]
struct SessionRuntimeBudget {
    storage: Storage,
    active_session_id: crate::tool::SharedActiveSessionId,
    max_active_duration: Option<Duration>,
    activity: SessionActivityGate,
    subagents: Arc<SubagentRegistry>,
}

#[derive(Clone)]
struct CompletionRunContext {
    agent: Arc<tokio::sync::Mutex<Agent>>,
    sink: SharedSink,
}

/// Runtime services required to execute a slash command task.
#[derive(Clone)]
pub(crate) struct CommandTaskDeps {
    agent: Arc<tokio::sync::Mutex<Agent>>,
    session_store: Arc<tokio::sync::Mutex<SessionStore>>,
    project_root: PathBuf,
    registry: Arc<ProviderRegistry>,
    model_catalog: Arc<ModelCatalog>,
    storage: Option<Storage>,
    credential_persistence: CredentialPersistence,
    /// The built-in mode active at dispatch, so `/model` in the command task
    /// records the selection against that mode's per-mode model entry.
    active_mode: Option<crate::agent::AgentMode>,
}

impl CommandTaskDeps {
    /// Bind a command task to one coherent runtime snapshot.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        agent: Arc<tokio::sync::Mutex<Agent>>,
        session_store: Arc<tokio::sync::Mutex<SessionStore>>,
        project_root: PathBuf,
        registry: Arc<ProviderRegistry>,
        model_catalog: Arc<ModelCatalog>,
        storage: Option<Storage>,
        credential_persistence: CredentialPersistence,
        active_mode: Option<crate::agent::AgentMode>,
    ) -> Self {
        Self {
            agent,
            session_store,
            project_root,
            registry,
            model_catalog,
            storage,
            credential_persistence,
            active_mode,
        }
    }
}

impl std::fmt::Debug for CommandTaskDeps {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommandTaskDeps")
            .field("agent", &"<shared>")
            .field("session_store", &"<shared>")
            .field("project_root", &self.project_root)
            .field("registry", &"<shared>")
            .field("model_catalog", &"<shared>")
            .field("has_storage", &self.storage.is_some())
            .field("credential_persistence", &self.credential_persistence)
            .finish()
    }
}

#[derive(Debug, Clone, Copy)]
struct CompletionEvidenceBaseline {
    verification_runs: usize,
    self_review_runs: usize,
    authorization_decision_id: Option<i64>,
}

/// Everything needed to resolve and apply a persona's model at run dispatch:
/// the per-mode `/model` entries live in the session store, a custom persona's
/// `model:` in the agent registry. Registry/catalog handles are replaced on
/// `/refresh` — the event loop re-installs fresh deps then.
#[derive(Clone)]
pub(crate) struct PersonaModelDeps {
    pub(crate) agent: Arc<tokio::sync::Mutex<Agent>>,
    pub(crate) session_store: Arc<tokio::sync::Mutex<SessionStore>>,
    pub(crate) registry: Arc<ProviderRegistry>,
    pub(crate) model_catalog: Arc<ModelCatalog>,
    pub(crate) custom_agents: crate::resource::agent::SharedAgentRegistry,
}

/// Apply the model the persona wants (per-mode entry or custom-agent `model:`)
/// to the session + agent when it differs from the current target, persisting
/// the session. Returns the applied selection for UI mirrors; `None` when
/// nothing changed (no entry, unresolvable, or already current). Lock order:
/// session_store before agent (the canonical order).
pub(crate) async fn apply_persona_model(
    deps: &PersonaModelDeps,
    persona: &crate::agent::ActivePersona,
) -> Option<ProviderRunSelection> {
    // One session acquisition covers both the desired-input read and the
    // apply, so the input can't go stale between a read and a re-lock.
    let mut session = deps.session_store.lock().await;
    let desired = crate::commands::model_switch::desired_persona_model_input(
        persona,
        &session,
        &deps.custom_agents,
    )?;
    let mut agent = deps.agent.lock().await;
    let (model, reasoning) = crate::commands::model_switch::apply_model_input_if_different(
        &deps.registry,
        &mut session,
        Some(&deps.model_catalog),
        &mut agent,
        &desired,
    )?;
    // Persist a clone after dropping the locks — the same idiom as
    // `switch_model_selection`. Safe because callers are serialized: the
    // dispatch path runs under the TaskController's single-active-task slot
    // and the idle path is busy-guarded, so nothing mutates the live session
    // between the clone and the save. Re-check if that invariant ever
    // relaxes.
    let snapshot = session.clone();
    drop(agent);
    drop(session);
    tracing::info!(persona = ?persona, model = %model, "applied persona model");
    if let Err(err) = snapshot.save_async().await {
        tracing::warn!(%err, "failed to persist persona model switch");
    }
    Some(ProviderRunSelection {
        provider: snapshot.provider_label().to_string(),
        model,
        reasoning,
    })
}

pub struct TaskController {
    active: Option<ActiveTask>,
    sender: mpsc::UnboundedSender<RuntimeEvent>,
    max_run_duration: Option<Duration>,
    session_runtime_budget: Option<SessionRuntimeBudget>,
    /// When set, every agent-run dispatch first applies the run persona's
    /// model (see [`apply_persona_model`]); `None` (tests) skips the step.
    persona_model_deps: Option<PersonaModelDeps>,
}

/// Per-phase turn budget for plan-implementation runs. Phases are isolated
/// runs that may need more iterations than a typical conversation; this limit
/// is higher than the default conversation budget (375) to give phases room
/// to complete their work without hitting the ceiling.
const PLAN_PHASE_MAX_TURNS: usize = 500;

impl TaskController {
    pub fn new(sender: mpsc::UnboundedSender<RuntimeEvent>) -> Self {
        Self {
            active: None,
            sender,
            max_run_duration: None,
            session_runtime_budget: None,
            persona_model_deps: None,
        }
    }

    /// Install (or refresh, after `/refresh` swaps registry/catalog) the deps
    /// used to apply a persona's model at every agent-run dispatch.
    pub(crate) fn set_persona_model_deps(&mut self, deps: PersonaModelDeps) {
        self.persona_model_deps = Some(deps);
    }

    pub(crate) fn persona_model_deps(&self) -> Option<&PersonaModelDeps> {
        self.persona_model_deps.as_ref()
    }

    pub(crate) fn set_max_run_duration(&mut self, duration: Option<Duration>) {
        self.max_run_duration = duration;
    }

    pub(crate) fn configure_session_runtime_budget(
        &mut self,
        storage: Storage,
        active_session_id: crate::tool::SharedActiveSessionId,
        max_active_duration: Option<Duration>,
        subagents: Arc<SubagentRegistry>,
    ) {
        self.session_runtime_budget = Some(SessionRuntimeBudget {
            activity: SessionActivityGate::new(storage.clone()),
            storage,
            active_session_id,
            max_active_duration,
            subagents,
        });
    }

    pub(crate) fn set_max_session_active_duration(&mut self, duration: Option<Duration>) {
        if let Some(runtime) = self.session_runtime_budget.as_mut() {
            runtime.max_active_duration = duration;
        }
    }

    /// Reconcile the durable union activity segment after runtime/subagent
    /// events. Its result drives the header timer and peer busy heartbeat.
    pub(crate) async fn reconcile_session_activity(
        &self,
    ) -> anyhow::Result<Option<SessionActivity>> {
        let Some(runtime) = &self.session_runtime_budget else {
            return Ok(None);
        };
        runtime
            .activity
            .reconcile(runtime.subagents.has_running())
            .await
            .map(Some)
    }

    pub(crate) async fn session_activity_is_active(&self) -> bool {
        let Some(runtime) = &self.session_runtime_budget else {
            return false;
        };
        runtime.subagents.has_running() || runtime.activity.is_active().await
    }

    pub(crate) fn session_active_budget_exhausted(&self, active_run_ms: u64) -> bool {
        self.session_runtime_budget
            .as_ref()
            .and_then(|runtime| runtime.max_active_duration)
            .is_some_and(|limit| {
                active_run_ms >= u64::try_from(limit.as_millis()).unwrap_or(u64::MAX)
            })
    }

    pub fn runtime_sender(&self) -> mpsc::UnboundedSender<RuntimeEvent> {
        self.sender.clone()
    }

    /// Shared spawn machinery for every agent run: reject if a task is already
    /// active, wire the cancellation token and queued-message channel (pre-seeding
    /// `queued_messages`), spawn the run future, and translate its result into the
    /// `AgentFinished` runtime event. `run` builds the per-variant future from the
    /// cancellation token and the queue receiver.
    fn spawn_agent_task<F, Fut>(
        &mut self,
        agent_persona: crate::agent::ActivePersona,
        queued_messages: Vec<QueuedUserMessage>,
        completion: Option<CompletionRunContext>,
        run: F,
    ) -> Result<(), UiError>
    where
        F: FnOnce(CancellationToken, mpsc::UnboundedReceiver<QueuedUserMessageCommand>) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = anyhow::Result<AgentRunResult>> + Send + 'static,
    {
        if self.active.is_some() {
            return Err(UiError::new(
                "Task already running",
                "Wait for the current task to finish before starting another one.",
            ));
        }

        let cancellation = CancellationToken::new();
        let run_token = cancellation.clone();
        let (queue_sender, queue_receiver) = mpsc::unbounded_channel();
        for message in queued_messages {
            queue_sender
                .send(QueuedUserMessageCommand::Send(message))
                .map_err(|_err| {
                    UiError::new(
                        "Cannot queue message",
                        "The agent stopped accepting queued messages.",
                    )
                })?;
        }
        let sender = self.sender.clone();
        let max_run_duration = self.max_run_duration;
        let session_runtime_budget = self.session_runtime_budget.clone();
        let persona_model_deps = self.persona_model_deps.clone();
        let persona_for_model = agent_persona.clone();
        let _ = sender.send(RuntimeEvent::AgentStarted);
        let handle = tokio::spawn(async move {
            // Every run starts under the model its persona selected: the
            // per-mode `/model` entry (Coding/Planning) or a custom agent's
            // `model:`. Applied here — the single choke point for all agent
            // dispatch paths (submit, queued, retry, implement, phases,
            // continuations) — before the run future takes the agent lock.
            if let Some(deps) = &persona_model_deps
                && let Some(selection) = apply_persona_model(deps, &persona_for_model).await
            {
                let _ = sender.send(RuntimeEvent::PersonaModelApplied(Box::new(selection)));
            }
            let session_timing = match begin_session_timing(session_runtime_budget.as_ref()).await {
                Ok(timing) => timing,
                Err(err) => {
                    let _ = sender.send(RuntimeEvent::AgentFinished(Err(UiError::new(
                        "Persistence failed",
                        format!("{err:#}"),
                    ))));
                    return;
                }
            };
            let completion_baseline = match completion.as_ref() {
                Some(context) => {
                    context.sink.begin_completion_run();
                    Some(
                        capture_completion_baseline(
                            context,
                            session_runtime_budget.as_ref(),
                            session_timing.map(|(session_id, _)| session_id),
                        )
                        .await,
                    )
                }
                None => None,
            };
            let selected_timeout = crate::run_budget::select_budget_timeout(
                max_run_duration,
                session_runtime_budget
                    .as_ref()
                    .and_then(|runtime| runtime.max_active_duration),
                session_timing.map_or(0, |(_, active_run_ms)| active_run_ms),
            );
            let preflight_exhaustion = selected_timeout
                .filter(|(duration, _)| duration.is_zero())
                .map(|(_, exhaustion)| exhaustion);
            let run_time_exhausted = Arc::new(AtomicBool::new(false));
            let timeout_task = selected_timeout
                .filter(|(duration, _)| !duration.is_zero())
                .map(|(duration, _)| {
                    let timeout_token = run_token.clone();
                    let run_time_exhausted = run_time_exhausted.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(duration).await;
                        run_time_exhausted.store(true, Ordering::Release);
                        timeout_token.cancel();
                    })
                });
            let result = if preflight_exhaustion.is_some() {
                None
            } else {
                Some(run(run_token, queue_receiver).await)
            };
            if let Some(timeout_task) = timeout_task {
                timeout_task.abort();
            }
            let final_active_run_ms = match finish_session_timing(
                session_runtime_budget.as_ref(),
                session_timing.map(|(session_id, _)| session_id),
            )
            .await
            {
                Ok(active_run_ms) => active_run_ms,
                Err(err) => {
                    let _ = sender.send(RuntimeEvent::AgentFinished(Err(UiError::new(
                        "Persistence failed",
                        format!("{err:#}"),
                    ))));
                    return;
                }
            };
            let timeout_exhaustion = selected_timeout.map(|(_, exhaustion)| {
                crate::run_budget::refresh_session_time_exhaustion(exhaustion, final_active_run_ms)
            });
            let mut outcome = if let Some(exhaustion) = preflight_exhaustion {
                crate::tui::event::AgentRunOutcome::BudgetExhausted(
                    crate::run_budget::refresh_session_time_exhaustion(
                        exhaustion,
                        final_active_run_ms,
                    ),
                )
            } else if run_time_exhausted.load(Ordering::Acquire) {
                crate::tui::event::AgentRunOutcome::BudgetExhausted(timeout_exhaustion.unwrap_or(
                    crate::run_budget::RunBudgetExhaustion::RunTime {
                        limit_seconds:
                            max_run_duration.map_or(1, |duration| duration.as_secs().max(1)),
                    },
                ))
            } else {
                let Some(result) = result else {
                    let _ = sender.send(RuntimeEvent::AgentFinished(Err(UiError::new(
                        "Agent failed",
                        "Agent run did not produce a result.",
                    ))));
                    return;
                };
                match result {
                    Ok(AgentRunResult::Completed(_)) => {
                        crate::tui::event::AgentRunOutcome::Completed
                    }
                    Ok(AgentRunResult::Interrupted(_)) => {
                        crate::tui::event::AgentRunOutcome::Interrupted
                    }
                    Ok(AgentRunResult::Waiting(reason)) => {
                        crate::tui::event::AgentRunOutcome::Waiting(reason)
                    }
                    Err(err) => {
                        let Some(exhaustion) = err
                            .downcast_ref::<crate::run_budget::RunBudgetExhaustion>()
                            .copied()
                        else {
                            let detail = crate::provider::agent_failure_detail(&err);
                            let _ = sender.send(RuntimeEvent::AgentFinished(Err(UiError::new(
                                "Agent failed",
                                detail,
                            ))));
                            return;
                        };
                        crate::tui::event::AgentRunOutcome::BudgetExhausted(exhaustion)
                    }
                }
            };
            if let (Some(context), Some(baseline)) = (completion.as_ref(), completion_baseline)
                && let Some(report) = build_completion_report(
                    context,
                    baseline,
                    &outcome,
                    session_runtime_budget.as_ref(),
                    session_timing.map(|(session_id, _)| session_id),
                    final_active_run_ms,
                )
                .await
            {
                if report.status == CompletionStatus::Failed
                    && matches!(outcome, crate::tui::event::AgentRunOutcome::Completed)
                {
                    outcome = crate::tui::event::AgentRunOutcome::Failed;
                }
                let _ = sender.send(RuntimeEvent::CompletionReport(Box::new(report)));
            }
            let _ = sender.send(RuntimeEvent::AgentFinished(Ok(outcome)));
        });

        self.active = Some(ActiveTask {
            kind: TaskKind::AgentRun,
            handle,
            cancellation: Some(cancellation),
            queued_messages: Some(queue_sender),
            agent_persona: Some(agent_persona),
        });
        Ok(())
    }

    pub fn start_agent_run(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        input: UserInput,
        sink: SharedSink,
        persona: crate::agent::ActivePersona,
    ) -> Result<(), UiError> {
        self.start_agent_run_inner(agent, input, sink, persona, Vec::new())
    }

    pub fn start_focused_coding_run(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        prompt: String,
        sink: SharedSink,
    ) -> Result<(), UiError> {
        let completion = CompletionRunContext {
            agent: agent.clone(),
            sink: sink.clone(),
        };
        self.spawn_agent_task(
            crate::agent::ActivePersona::Builtin(AgentMode::Coding),
            Vec::new(),
            Some(completion),
            move |run_token, queue_receiver| async move {
                let mut guard = agent.lock().await;
                guard.begin_focused_coding_run(&prompt).await;
                sink.context_updated(guard.context_report());
                guard
                    .run_current_context_with_queue(run_token, sink, queue_receiver)
                    .await
            },
        )
    }

    pub fn start_verification_run(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        workflow: crate::verification::VerificationWorkflow,
        sink: SharedSink,
    ) -> Result<(), UiError> {
        let completion = CompletionRunContext {
            agent: agent.clone(),
            sink: sink.clone(),
        };
        self.spawn_agent_task(
            crate::agent::ActivePersona::Builtin(AgentMode::Coding),
            Vec::new(),
            Some(completion),
            move |run_token, queue_receiver| async move {
                let mut guard = agent.lock().await;
                guard
                    .begin_verification_run(workflow.kind, &workflow.checks, &workflow.prompt)
                    .await;
                sink.context_updated(guard.context_report());
                guard
                    .run_current_context_with_queue(run_token, sink, queue_receiver)
                    .await
            },
        )
    }

    /// Continue the latest human turn from the existing context without
    /// replaying its prompt.
    pub fn start_agent_retry(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        sink: SharedSink,
        persona: crate::agent::ActivePersona,
    ) -> Result<(), UiError> {
        let persona_for_task = persona.clone();
        let completion = CompletionRunContext {
            agent: agent.clone(),
            sink: sink.clone(),
        };
        self.spawn_agent_task(
            persona,
            Vec::new(),
            Some(completion),
            move |run_token, queue_receiver| async move {
                let mut guard = agent.lock().await;
                guard.set_persona(persona_for_task);
                guard.begin_retry_last_turn().await?;
                sink.context_updated(guard.context_report());
                guard
                    .run_current_context_with_queue(run_token, sink, queue_receiver)
                    .await
            },
        )
    }

    pub fn start_agent_run_with_queued(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        input: UserInput,
        sink: SharedSink,
        persona: crate::agent::ActivePersona,
        queued_messages: Vec<QueuedUserMessage>,
    ) -> Result<(), UiError> {
        self.start_agent_run_inner(agent, input, sink, persona, queued_messages)
    }

    /// Continue the current conversation under a newly selected persona after
    /// a completed planning transition. No new user turn is appended:
    /// `set_persona` swaps the registry and injects the
    /// transition system message, then the model generates from the existing
    /// context so it builds the plan on the canvas instead of re-grounding.
    pub fn start_persona_switch_continuation(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        sink: SharedSink,
        persona: crate::agent::ActivePersona,
    ) -> Result<(), UiError> {
        let persona_for_task = persona.clone();
        let completion = CompletionRunContext {
            agent: agent.clone(),
            sink: sink.clone(),
        };
        self.spawn_agent_task(
            persona,
            Vec::new(),
            Some(completion),
            move |run_token, queue_receiver| async move {
                let mut guard = agent.lock().await;
                guard.set_persona(persona_for_task);
                sink.context_updated(guard.context_report());
                guard
                    .run_current_context_with_queue(run_token, sink, queue_receiver)
                    .await
            },
        )
    }

    fn start_agent_run_inner(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        input: UserInput,
        sink: SharedSink,
        persona: crate::agent::ActivePersona,
        queued_messages: Vec<QueuedUserMessage>,
    ) -> Result<(), UiError> {
        let persona_for_task = persona.clone();
        let completion = CompletionRunContext {
            agent: agent.clone(),
            sink: sink.clone(),
        };
        self.spawn_agent_task(
            persona,
            queued_messages,
            Some(completion),
            move |run_token, queue_receiver| async move {
                let mut guard = agent.lock().await;
                guard.set_persona(persona_for_task);
                sink.context_updated(guard.context_report());
                guard
                    .run_with_queue(input, run_token, sink, queue_receiver)
                    .await
            },
        )
    }

    /// Start an "implement plan" run: pre-seeds the agent with the plan's
    /// todo list when the plan has tasks, resets the conversation to focus on
    /// execution, and ships the plan markdown as the only user message before
    /// dispatching the run loop.
    #[cfg(test)]
    pub fn start_implement_plan(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        plan: PlanDoc,
        sink: SharedSink,
        phase: Option<usize>,
    ) -> Result<(), UiError> {
        self.start_implement_plan_with_context(
            agent,
            plan,
            sink,
            phase,
            crate::agent::PlanContextMode::Clean,
        )
    }

    pub fn start_implement_plan_with_context(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        plan: PlanDoc,
        sink: SharedSink,
        phase: Option<usize>,
        context_mode: crate::agent::PlanContextMode,
    ) -> Result<(), UiError> {
        let completion = CompletionRunContext {
            agent: agent.clone(),
            sink: sink.clone(),
        };
        self.spawn_agent_task(
            crate::agent::ActivePersona::Builtin(AgentMode::Coding),
            Vec::new(),
            Some(completion),
            move |run_token, queue_receiver| async move {
                let mut guard = agent.lock().await;
                // Give plan-implementation runs a larger budget — phases may
                // need more turns than a typical conversation.
                guard.set_run_budget(crate::run_budget::RunBudget {
                    max_turns: Some(PLAN_PHASE_MAX_TURNS),
                    ..Default::default()
                });
                let has_plan = guard
                    .implement_plan_from_with_context(&plan, phase, context_mode)
                    .await;
                sink.context_updated(guard.context_report());
                if has_plan {
                    guard
                        .run_current_context_with_queue(run_token, sink, queue_receiver)
                        .await
                } else {
                    Ok(AgentRunResult::Completed(String::new()))
                }
            },
        )
    }

    /// Begin a "review pending changes" run. Mirrors plan implementation: seeds
    /// the agent with the diff for `scope` and
    /// a review instruction, then dispatches the run loop. The agent stays in
    /// `Review` mode with read-only tools. When the scope has no changes the
    /// run loop is skipped.
    pub async fn start_review(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        scope: crate::agent::ReviewScope,
        sink: SharedSink,
    ) -> Result<bool, UiError> {
        self.start_review_workflow(agent, scope, sink, ReviewWorkflow::General)
            .await
    }

    /// Begin the curated `/security-review` workflow in the same enforced
    /// read-only review persona.
    pub async fn start_security_review(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        scope: crate::agent::ReviewScope,
        sink: SharedSink,
    ) -> Result<bool, UiError> {
        self.start_review_workflow(agent, scope, sink, ReviewWorkflow::Security)
            .await
    }

    async fn start_review_workflow(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        scope: crate::agent::ReviewScope,
        sink: SharedSink,
        workflow: ReviewWorkflow,
    ) -> Result<bool, UiError> {
        if self.active.is_some() {
            return Err(UiError::new(
                "Task already running",
                "Wait for the current task to finish before starting another one.",
            ));
        }

        let mut guard = agent.lock().await;
        let has_changes = match workflow {
            ReviewWorkflow::General => guard.review_pending_changes(scope).await,
            ReviewWorkflow::Security => guard.security_review_pending_changes(scope).await,
        };
        sink.context_updated(guard.context_report());
        drop(guard);
        if !has_changes {
            return Ok(false);
        }

        let completion = CompletionRunContext {
            agent: agent.clone(),
            sink: sink.clone(),
        };
        self.spawn_agent_task(
            crate::agent::ActivePersona::Builtin(AgentMode::Review),
            Vec::new(),
            Some(completion),
            move |run_token, queue_receiver| async move {
                let mut guard = agent.lock().await;
                guard
                    .run_current_context_with_queue(run_token, sink, queue_receiver)
                    .await
            },
        )?;
        Ok(true)
    }

    pub fn start_agent_resume(
        &mut self,
        agent: Arc<tokio::sync::Mutex<Agent>>,
        sink: SharedSink,
        mode: AgentMode,
    ) -> Result<(), UiError> {
        let completion = CompletionRunContext {
            agent: agent.clone(),
            sink: sink.clone(),
        };
        self.spawn_agent_task(
            crate::agent::ActivePersona::Builtin(mode),
            Vec::new(),
            Some(completion),
            move |run_token, queue_receiver| async move {
                let mut guard = agent.lock().await;
                guard.set_mode(mode);
                sink.context_updated(guard.context_report());
                guard
                    .run_current_context_with_queue(run_token, sink, queue_receiver)
                    .await
            },
        )
    }

    pub fn start_command(
        &mut self,
        input: String,
        transcript: Vec<TranscriptItem>,
        generation: u64,
        deps: CommandTaskDeps,
    ) -> Result<(), UiError> {
        if self.active.is_some() {
            return Err(UiError::new(
                "Task already running",
                "Wait for the current task to finish before starting another one.",
            ));
        }

        let CommandTaskDeps {
            agent,
            session_store,
            project_root,
            registry,
            model_catalog,
            storage,
            credential_persistence,
            active_mode,
        } = deps;
        let sender = self.sender.clone();
        let handle = tokio::spawn(async move {
            let result = async {
                // Canonical lock order: session_store before agent. Every site
                // that holds both locks acquires in this order so a concurrent
                // command (e.g. an orphaned `/authorize` spawn) can't form an
                // agent↔session deadlock cycle. (This path previously locked
                // agent first — the lone inversion.)
                let mut session_guard = session_store.lock().await;
                let mut agent_guard = agent.lock().await;
                if let Some(hub) = agent_guard.mcp_hub() {
                    hub.set_credential_persistence(credential_persistence);
                }
                let outcome = handle_command_with_catalog(
                    &input,
                    &mut agent_guard,
                    &mut session_guard,
                    &project_root,
                    &transcript,
                    registry.clone(),
                    crate::commands::CommandRuntimeContext {
                        catalog: Some(&model_catalog),
                        storage: storage.as_ref(),
                        active_mode,
                    },
                )
                .await?;
                let context_report = agent_guard.context_report();
                let memory_modal = agent_guard
                    .memory()
                    .and_then(|memory| memory_command_modal(&input, memory));
                Ok::<_, anyhow::Error>((outcome, context_report, memory_modal))
            }
            .await;

            let event = match result {
                Ok((outcome, context_report, memory_modal)) => {
                    let session = session_store.lock().await;
                    let provider = Some(Box::new(ProviderRunSelection {
                        provider: session.provider_label().to_string(),
                        model: session.current_session().model.clone(),
                        reasoning: session.current_session().reasoning,
                    }));
                    let mut model_entries = Vec::new();
                    for (provider_id, provider_session) in &session.providers {
                        let Some(factory) = registry.lookup(provider_id.as_str()) else {
                            continue;
                        };
                        if !factory.is_authorized(provider_session) {
                            continue;
                        }
                        let metadata = factory.metadata();
                        let provider_label = metadata.display_name.to_string();
                        let models = crate::model_catalog::available_model_ids_for_provider(
                            Some(&model_catalog),
                            provider_id.as_str(),
                            metadata,
                            &provider_session.model,
                        );
                        for model in models {
                            model_entries.push(ModelOption::from_provider_model(
                                Some(&model_catalog),
                                provider_id.as_str(),
                                &provider_label,
                                &session,
                                metadata,
                                model,
                            ));
                        }
                    }
                    let perf_report_title = perf_report_title(&input);
                    let perf_report = perf_report_title.and_then(|title| {
                        outcome
                            .messages
                            .iter()
                            .find(|message| matches!(message.kind, CommandMessageKind::Status))
                            .map(|message| ModalKind::perf_report(title, &message.text))
                    });
                    let opens_api_key_prompt = matches!(
                        &outcome.modal,
                        Some(CommandModalRequest::ApiKeyPrompt { .. })
                    );
                    let opens_episodes =
                        matches!(&outcome.modal, Some(CommandModalRequest::Episodes(_)));
                    let memory_modal_open = memory_modal.is_some();
                    let open_modal = memory_modal.or(perf_report).or_else(|| {
                        outcome.modal.map(|modal| match modal {
                            CommandModalRequest::CommandHelp => {
                                ModalKind::Detail(crate::tui::event::DetailModal::CommandHelp)
                            }
                            CommandModalRequest::Keys => {
                                ModalKind::Detail(crate::tui::event::DetailModal::Help)
                            }
                            CommandModalRequest::ContextReport(report) => {
                                ModalKind::Detail(crate::tui::event::DetailModal::Context(report))
                            }
                            CommandModalRequest::Episodes(report) => {
                                ModalKind::Detail(crate::tui::event::DetailModal::Episodes {
                                    report,
                                    cursor: 0,
                                })
                            }
                            CommandModalRequest::ModelPicker => {
                                ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker {
                                    entries: model_entries,
                                })
                            }
                            CommandModalRequest::AuthorizeProviderPicker { providers } => {
                                ModalKind::Picker(
                                    crate::tui::event::PickerModal::AuthorizeProviderPicker {
                                        providers,
                                        query: String::new(),
                                        cursor: 0,
                                    },
                                )
                            }
                            CommandModalRequest::UnauthorizeProviderPicker { providers } => {
                                ModalKind::Picker(
                                    crate::tui::event::PickerModal::UnauthorizeProviderPicker {
                                        providers,
                                        query: String::new(),
                                        cursor: 0,
                                    },
                                )
                            }
                            CommandModalRequest::ApiKeyPrompt { provider_id } => {
                                let endpoint_form =
                                    registry.lookup(&provider_id).is_some_and(|factory| {
                                        factory.metadata().auth_requirement.uses_endpoint_setup()
                                    });
                                let origins = provider_id
                                    .parse::<crate::model_catalog::ConnectionId>()
                                    .ok()
                                    .and_then(|id| model_catalog.connection(&id))
                                    .map(|connection| connection.origins.clone())
                                    .unwrap_or_default();
                                let initial_form =
                                    Some(crate::tui::app::ProviderAuthForm::from_provider_session(
                                        session.session(&provider_id),
                                        credential_persistence,
                                        origins,
                                        endpoint_form,
                                    ));
                                ModalKind::Detail(crate::tui::event::DetailModal::ApiKeyPrompt {
                                    provider_id,
                                    initial_form,
                                })
                            }
                            CommandModalRequest::McpServers { rows } => {
                                ModalKind::Manager(crate::tui::event::ManagerModal::McpServers {
                                    rows,
                                    cursor: 0,
                                })
                            }
                            CommandModalRequest::SandboxStatus => {
                                ModalKind::Manager(crate::tui::event::ManagerModal::SandboxStatus {
                                    cursor: 0,
                                })
                            }
                            CommandModalRequest::Doctor(report) => {
                                ModalKind::Manager(crate::tui::event::ManagerModal::Doctor {
                                    report,
                                    cursor: 0,
                                })
                            }
                        })
                    });
                    let messages = if perf_report_title.is_some()
                        || opens_api_key_prompt
                        || opens_episodes
                        || memory_modal_open
                    {
                        Vec::new()
                    } else {
                        outcome
                            .messages
                            .into_iter()
                            .filter(|message| !suppress_redundant_tui_status(&input, message))
                            .map(|message| CommandOutputEvent {
                                kind: match message.kind {
                                    CommandMessageKind::Status => {
                                        crate::tui::event::CommandOutputKind::Status
                                    }
                                    CommandMessageKind::Error => {
                                        crate::tui::event::CommandOutputKind::Error
                                    }
                                },
                                text: message.text,
                            })
                            .collect()
                    };

                    RuntimeEvent::CommandFinished(Box::new(CommandOutcomeEvent::Applied {
                        // Aborting a task cannot retract an event it enqueued
                        // just before cancellation. The generation lets the
                        // event loop discard only that stale command outcome.
                        generation: Some(generation),
                        clear_transcript: outcome.clear_transcript,
                        messages,
                        provider,
                        context_report: Some(Box::new(context_report)),
                        quit: outcome.quit,
                        open_modal,
                    }))
                }
                Err(err) => {
                    RuntimeEvent::TaskPanicked(UiError::new("Task panicked", format!("{:#}", err)))
                }
            };
            let _ = sender.send(event);
        });

        self.active = Some(ActiveTask {
            kind: TaskKind::Command,
            handle,
            cancellation: None,
            queued_messages: None,
            agent_persona: None,
        });
        Ok(())
    }

    pub fn queue_agent_message(&self, message: QueuedUserMessage) -> Result<(), UiError> {
        let Some(active) = &self.active else {
            return Err(UiError::new(
                "No active agent run",
                "The agent finished before the message could be queued.",
            ));
        };
        if !matches!(active.kind, TaskKind::AgentRun) {
            return Err(UiError::new(
                "Cannot queue message",
                "Messages can only be queued while the agent is running.",
            ));
        }
        let Some(sender) = &active.queued_messages else {
            return Err(UiError::new(
                "Cannot queue message",
                "The active task is not accepting queued messages.",
            ));
        };
        sender
            .send(QueuedUserMessageCommand::Send(message))
            .map_err(|err| {
                let text = match err.0 {
                    QueuedUserMessageCommand::Send(message) => message.display_text,
                    QueuedUserMessageCommand::Cancel(id) => format!("cancel queued message {id}"),
                };
                UiError::new(
                    "Cannot queue message",
                    format!("The agent stopped accepting queued messages: {text}"),
                )
            })
    }

    pub fn cancel_agent_message(&self, id: u64) -> Result<(), UiError> {
        let Some(active) = &self.active else {
            return Ok(());
        };
        if !matches!(active.kind, TaskKind::AgentRun) {
            return Ok(());
        }
        let Some(sender) = &active.queued_messages else {
            return Ok(());
        };
        sender
            .send(QueuedUserMessageCommand::Cancel(id))
            .map_err(|_err| {
                UiError::new(
                    "Cannot cancel queued message",
                    "The agent stopped accepting queued message updates.",
                )
            })
    }

    pub fn cancel_agent_messages(&self, ids: &[u64]) -> Result<(), UiError> {
        for id in ids {
            self.cancel_agent_message(*id)?;
        }
        Ok(())
    }

    pub fn active_agent_persona(&self) -> Option<&crate::agent::ActivePersona> {
        self.active
            .as_ref()
            .and_then(|active| active.agent_persona.as_ref())
    }

    pub fn active_agent_mode(&self) -> Option<AgentMode> {
        self.active_agent_persona()
            .and_then(crate::agent::ActivePersona::builtin)
    }

    pub fn cancel(&self) {
        if let Some(active) = &self.active
            && let Some(token) = &active.cancellation
        {
            token.cancel();
        }
    }

    pub fn abort(&mut self) {
        if let Some(active) = self.active.take() {
            if let Some(token) = &active.cancellation {
                token.cancel();
            }
            active.handle.abort();
        }
    }

    pub async fn poll_finished(&mut self) -> Option<TaskKind> {
        let finished = self
            .active
            .as_ref()
            .is_some_and(|active| active.handle.is_finished());
        if !finished {
            return None;
        }

        let active = self.active.take()?;
        if let Err(err) = active.handle.await
            && let Err(send_err) = self.sender.send(RuntimeEvent::TaskPanicked(UiError::new(
                "Task panicked",
                format!("task panicked: {}", err),
            )))
        {
            tracing::warn!(%send_err, "failed to deliver task-panicked event");
        }
        Some(active.kind)
    }

    /// True while a spawned task is still running. Used by logging to record
    /// whether the app exited with a save mid-flight.
    pub fn is_busy(&self) -> bool {
        self.active.is_some()
    }
}

async fn capture_completion_baseline(
    context: &CompletionRunContext,
    runtime: Option<&SessionRuntimeBudget>,
    session_id: Option<SessionId>,
) -> CompletionEvidenceBaseline {
    let (verification_runs, self_review_runs) = {
        let agent = context.agent.lock().await;
        (
            agent.verification_runs().len(),
            agent.self_review_runs().len(),
        )
    };
    let authorization_decision_id = match (runtime, session_id) {
        (Some(runtime), Some(session_id)) => {
            match runtime
                .storage
                .recent_authorization_decisions(session_id, 1)
                .await
            {
                Ok(records) => Some(records.first().map_or(0, |record| record.id)),
                Err(err) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %format!("{err:#}"),
                        "failed to capture TUI authorization evidence baseline"
                    );
                    None
                }
            }
        }
        _ => None,
    };
    CompletionEvidenceBaseline {
        verification_runs,
        self_review_runs,
        authorization_decision_id,
    }
}

async fn build_completion_report(
    context: &CompletionRunContext,
    baseline: CompletionEvidenceBaseline,
    outcome: &crate::tui::event::AgentRunOutcome,
    runtime: Option<&SessionRuntimeBudget>,
    session_id: Option<SessionId>,
    active_run_ms: u64,
) -> Option<CompletionReport> {
    let (base_status, budget_exhaustion) = match outcome {
        crate::tui::event::AgentRunOutcome::Completed => (CompletionStatus::Completed, None),
        crate::tui::event::AgentRunOutcome::Failed => (CompletionStatus::Failed, None),
        crate::tui::event::AgentRunOutcome::Interrupted => (CompletionStatus::Interrupted, None),
        crate::tui::event::AgentRunOutcome::BudgetExhausted(reason) => {
            (CompletionStatus::BudgetExhausted, Some(*reason))
        }
        crate::tui::event::AgentRunOutcome::Waiting(_) => return None,
    };
    let (verification, review, usage, session_budget) = {
        let mut agent = context.agent.lock().await;
        agent
            .revalidate_verification_for_delivery(baseline.verification_runs)
            .await;
        (
            run_local_last(agent.verification_runs(), baseline.verification_runs),
            run_local_last(agent.self_review_runs(), baseline.self_review_runs),
            agent.usage_totals(),
            agent
                .session_budget_usage()
                .with_active_run_ms(active_run_ms),
        )
    };
    let authorization_decisions = match (runtime, session_id, baseline.authorization_decision_id) {
        (Some(runtime), Some(session_id), Some(baseline_id)) => {
            match runtime
                .storage
                .recent_authorization_decisions(session_id, 500)
                .await
            {
                Ok(records) => records
                    .into_iter()
                    .filter(|record| record.id > baseline_id)
                    .collect(),
                Err(err) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %format!("{err:#}"),
                        "failed to load TUI authorization completion evidence"
                    );
                    Vec::new()
                }
            }
        }
        _ => Vec::new(),
    };
    let evidence = context.sink.completion_evidence().unwrap_or_default();
    let status = classify_completion_status(base_status, &evidence, verification.as_ref());
    Some(CompletionReport::from_evidence(
        status,
        evidence,
        CompletionSessionEvidence {
            verification,
            review,
            authorization_decisions: &authorization_decisions,
            usage,
            session_budget,
            budget_exhaustion,
        },
    ))
}

fn run_local_last<T: Clone>(records: &[T], baseline: usize) -> Option<T> {
    records.get(baseline..)?.last().cloned()
}

async fn begin_session_timing(
    runtime: Option<&SessionRuntimeBudget>,
) -> anyhow::Result<Option<(SessionId, u64)>> {
    let Some(runtime) = runtime else {
        return Ok(None);
    };
    let session_id = *runtime.active_session_id.lock().await;
    let Some(session_id) = session_id else {
        anyhow::bail!("No active session is available for run timing");
    };
    let active_run_ms = runtime.activity.begin_main(session_id).await?.active_run_ms;
    Ok(Some((session_id, active_run_ms)))
}

async fn finish_session_timing(
    runtime: Option<&SessionRuntimeBudget>,
    session_id: Option<SessionId>,
) -> anyhow::Result<u64> {
    let (Some(runtime), Some(session_id)) = (runtime, session_id) else {
        return Ok(0);
    };
    let activity = runtime
        .activity
        .finish_main(runtime.subagents.has_running())
        .await?;
    let _ = session_id;
    Ok(activity.active_run_ms)
}

pub(in crate::tui) fn memory_command_modal(
    input: &str,
    memory: &crate::memory::MemoryService,
) -> Option<ModalKind> {
    let request = parse_memory_command(input).ok()?;
    let rows = memory.store().entries();
    match request {
        MemoryCommandRequest::List => Some(ModalKind::Manager(
            crate::tui::event::ManagerModal::MemoryManager { rows, cursor: 0 },
        )),
        MemoryCommandRequest::View { name } => {
            let cursor = memory
                .store()
                .get(&name)
                .and_then(|entry| {
                    rows.iter()
                        .position(|row| row.tier == entry.tier && row.name == entry.name)
                })
                .unwrap_or_else(|| rows.len().saturating_sub(1));
            Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::MemoryManager { rows, cursor },
            ))
        }
        MemoryCommandRequest::Forget { .. } => None,
    }
}

fn suppress_redundant_tui_status(input: &str, message: &crate::commands::CommandMessage) -> bool {
    if message.kind != CommandMessageKind::Status {
        return false;
    }

    match input.split_whitespace().next() {
        Some("/model") => message.text.starts_with("Model set to "),
        Some("/autonomy") => {
            matches!(
                crate::commands::parse_autonomy_command(input),
                Ok(crate::commands::AutonomyCommandRequest::Set(_))
            ) && message.text.starts_with("Autonomy set to ")
        }
        Some("/yolo") => {
            matches!(
                crate::commands::parse_yolo_command(input),
                Ok(crate::commands::YoloCommandRequest::Set(_)
                    | crate::commands::YoloCommandRequest::Toggle)
            ) && message.text.starts_with("Autonomy set to ")
        }
        Some("/self-review") => {
            matches!(
                crate::commands::parse_self_review_command(input),
                Ok(crate::commands::SelfReviewCommandRequest::Set(_))
            ) && message.text.starts_with("Self-review set to ")
        }
        Some("/sandbox") => {
            matches!(
                crate::commands::parse_sandbox_command(input),
                Ok(crate::commands::SandboxCommandRequest::Set(_)
                    | crate::commands::SandboxCommandRequest::SetNet(_))
            ) && is_redundant_sandbox_status(&message.text)
        }
        _ => false,
    }
}

fn is_redundant_sandbox_status(text: &str) -> bool {
    text == "Sandbox disabled."
        || text.starts_with("Sandbox enabled (")
        || text == "Network egress denied."
        || text == "Network egress allowed."
}

fn perf_report_title(input: &str) -> Option<&'static str> {
    match input.split_whitespace().next() {
        Some("/perf") => Some("Performance"),
        Some("/cost") => Some("Usage"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopProvider;

    #[async_trait::async_trait]
    impl crate::provider::Provider for NoopProvider {
        async fn chat_stream(
            &self,
            _messages: &[async_openai::types::chat::ChatCompletionRequestMessage],
            _tools: &[async_openai::types::chat::ChatCompletionTool],
            _cancellation_token: CancellationToken,
            _sink: SharedSink,
        ) -> crate::provider::ProviderResult<crate::provider::StreamedResponse> {
            Err(crate::provider::ProviderFailure::configuration(
                "noop provider must not be invoked",
            ))
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct CompletionSink {
        evidence: crate::completion_report::CompletionEvidenceCollector,
    }

    impl crate::output::OutputSink for CompletionSink {
        fn begin_completion_run(&self) {
            self.evidence.begin_run();
        }

        fn completion_evidence(
            &self,
        ) -> Option<crate::completion_report::CompletionEvidenceSnapshot> {
            Some(self.evidence.snapshot())
        }

        fn tool_started(&self, id: &str, name: &str, arguments: &str) {
            self.evidence.record_tool_started(id, name, arguments);
        }

        fn tool_finished(
            &self,
            id: &str,
            result: &str,
            status: crate::output::ToolExecutionStatus,
        ) {
            self.evidence.record_tool_result(id, result, status, None);
        }
    }

    fn completion_context() -> (CompletionRunContext, SharedSink) {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let agent = Agent::new(
            Box::new(NoopProvider),
            Arc::new(crate::tool::ToolRegistry::new()),
            Arc::new(crate::tool::ToolRegistry::new()),
            fixture.read_tracker,
            String::new(),
            fixture.project_root,
        )
        .unwrap();
        let sink: SharedSink = Arc::new(CompletionSink::default());
        (
            CompletionRunContext {
                agent: Arc::new(tokio::sync::Mutex::new(agent)),
                sink: sink.clone(),
            },
            sink,
        )
    }

    fn status(text: &str) -> crate::commands::CommandMessage {
        crate::commands::CommandMessage {
            kind: CommandMessageKind::Status,
            text: text.to_string(),
        }
    }

    fn error(text: &str) -> crate::commands::CommandMessage {
        crate::commands::CommandMessage {
            kind: CommandMessageKind::Error,
            text: text.to_string(),
        }
    }

    #[test]
    fn tui_status_filter_suppresses_state_switch_confirmations() {
        assert!(suppress_redundant_tui_status(
            "/model 1",
            &status("Model set to gpt-5.5")
        ));
        assert!(suppress_redundant_tui_status(
            "/yolo",
            &status("Autonomy set to yolo.")
        ));
        assert!(suppress_redundant_tui_status(
            "/autonomy balanced",
            &status("Autonomy set to balanced.")
        ));
        assert!(suppress_redundant_tui_status(
            "/self-review off",
            &status("Self-review set to off.")
        ));
        assert!(suppress_redundant_tui_status(
            "/sandbox net on",
            &status("Network egress denied.")
        ));
    }

    #[test]
    fn tui_status_filter_keeps_explicit_reports_warnings_and_errors() {
        assert!(!suppress_redundant_tui_status(
            "/autonomy status",
            &status("Autonomy is balanced.")
        ));
        assert!(!suppress_redundant_tui_status(
            "/model 1",
            &status("Context window reduced from 200k to 128k tokens.")
        ));
        assert!(!suppress_redundant_tui_status(
            "/model nope",
            &error("Unknown model 'nope'.")
        ));
        assert!(!suppress_redundant_tui_status(
            "/sandbox net on",
            &status(
                "Network egress set to denied, but the active backend (none) can't enforce it."
            )
        ));
    }

    #[test]
    fn provider_auth_failure_detail_includes_recovery_action() {
        let error =
            anyhow::Error::new(crate::provider::ProviderFailure::http(401, "expired", None));

        let detail = crate::provider::agent_failure_detail(&error);

        assert!(detail.contains("provider HTTP error (401)"));
        assert!(detail.contains("Action:"));
        assert!(detail.contains("/authorize"));
    }

    #[tokio::test]
    async fn run_budget_cancels_a_foreground_task() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut tasks = TaskController::new(sender);
        tasks.set_max_run_duration(Some(Duration::from_millis(10)));
        let (completion, _) = completion_context();
        tasks
            .spawn_agent_task(
                crate::agent::ActivePersona::Builtin(AgentMode::Coding),
                Vec::new(),
                Some(completion),
                |token, _queue| async move {
                    token.cancelled().await;
                    Ok(AgentRunResult::Interrupted(String::new()))
                },
            )
            .unwrap();

        let started = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(started, RuntimeEvent::AgentStarted));
        let report_event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let RuntimeEvent::CompletionReport(report) = report_event else {
            panic!("expected completion report before terminal event");
        };
        assert_eq!(
            report.status,
            crate::completion_report::CompletionStatus::BudgetExhausted
        );
        assert!(
            report
                .render_compact()
                .contains("Budget · exhausted: run exhausted")
        );
        let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            RuntimeEvent::AgentFinished(Ok(crate::tui::event::AgentRunOutcome::BudgetExhausted(
                crate::run_budget::RunBudgetExhaustion::RunTime { .. }
            )))
        ));
    }

    #[tokio::test]
    async fn repeated_tool_failure_emits_failed_report_and_terminal_outcome() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut tasks = TaskController::new(sender);
        let (completion, sink) = completion_context();
        tasks
            .spawn_agent_task(
                crate::agent::ActivePersona::Builtin(AgentMode::Coding),
                Vec::new(),
                Some(completion),
                move |_token, _queue| async move {
                    // The demotion needs a detected loop (the same invocation
                    // failing repeatedly) — a single adaptive failure now stays
                    // a Completed-with-caveat, not a failure.
                    for call in ["call-1", "call-2", "call-3"] {
                        sink.tool_started(call, "bash", r#"{"command":"cargo test"}"#);
                        sink.tool_finished(
                            call,
                            "tests failed",
                            crate::output::ToolExecutionStatus::Failed,
                        );
                    }
                    Ok(AgentRunResult::Completed(String::new()))
                },
            )
            .unwrap();

        assert!(matches!(
            receiver.recv().await,
            Some(RuntimeEvent::AgentStarted)
        ));
        let Some(RuntimeEvent::CompletionReport(report)) = receiver.recv().await else {
            panic!("expected typed completion report");
        };
        assert_eq!(
            report.status,
            crate::completion_report::CompletionStatus::Failed
        );
        assert!(report.caveats.iter().any(|caveat| caveat.contains("tool")));
        assert!(matches!(
            receiver.recv().await,
            Some(RuntimeEvent::AgentFinished(Ok(
                crate::tui::event::AgentRunOutcome::Failed
            )))
        ));
    }

    #[tokio::test]
    async fn interrupted_run_emits_interrupted_completion_caveat() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut tasks = TaskController::new(sender);
        let (completion, _) = completion_context();
        tasks
            .spawn_agent_task(
                crate::agent::ActivePersona::Builtin(AgentMode::Coding),
                Vec::new(),
                Some(completion),
                |_token, _queue| async move { Ok(AgentRunResult::Interrupted(String::new())) },
            )
            .unwrap();

        assert!(matches!(
            receiver.recv().await,
            Some(RuntimeEvent::AgentStarted)
        ));
        let Some(RuntimeEvent::CompletionReport(report)) = receiver.recv().await else {
            panic!("expected typed completion report");
        };
        assert_eq!(report.status, CompletionStatus::Interrupted);
        assert_eq!(
            report.caveats,
            vec!["Run was interrupted; partial work may remain."]
        );
        assert!(matches!(
            receiver.recv().await,
            Some(RuntimeEvent::AgentFinished(Ok(
                crate::tui::event::AgentRunOutcome::Interrupted
            )))
        ));
    }

    #[test]
    fn cumulative_session_timeout_wins_when_less_time_remains() {
        assert_eq!(
            crate::run_budget::select_budget_timeout(
                Some(Duration::from_secs(10)),
                Some(Duration::from_secs(60)),
                55_000,
            ),
            Some((
                Duration::from_secs(5),
                crate::run_budget::RunBudgetExhaustion::SessionTime {
                    limit_seconds: 60,
                    used_seconds: 60,
                }
            ))
        );
    }

    #[tokio::test]
    async fn exhausted_session_time_prevents_another_foreground_run() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let session_id = fixture.start_session().await;
        fixture.storage.begin_session_run(session_id).await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        fixture
            .storage
            .finish_session_run(session_id)
            .await
            .unwrap();
        let active_session_id = Arc::new(tokio::sync::Mutex::new(Some(session_id)));
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut tasks = TaskController::new(sender);
        tasks.configure_session_runtime_budget(
            fixture.storage.clone(),
            active_session_id,
            Some(Duration::from_millis(1)),
            Arc::new(SubagentRegistry::new()),
        );
        let invoked = Arc::new(AtomicBool::new(false));
        let invoked_by_run = invoked.clone();

        tasks
            .spawn_agent_task(
                crate::agent::ActivePersona::Builtin(AgentMode::Coding),
                Vec::new(),
                None,
                move |_token, _queue| async move {
                    invoked_by_run.store(true, Ordering::Release);
                    Ok(AgentRunResult::Completed(String::new()))
                },
            )
            .unwrap();

        assert!(matches!(
            receiver.recv().await,
            Some(RuntimeEvent::AgentStarted)
        ));
        let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            event,
            RuntimeEvent::AgentFinished(Ok(crate::tui::event::AgentRunOutcome::BudgetExhausted(
                crate::run_budget::RunBudgetExhaustion::SessionTime { .. }
            )))
        ));
        assert!(!invoked.load(Ordering::Acquire));
    }
}
