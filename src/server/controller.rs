use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use super::sink::{SessionEvent, SessionSink};
use crate::interaction::{InteractionOutcome, InteractionRequest, InteractionService};
use crate::output::{OutputSink, SharedSink};
use crate::runtime::{RuntimeBuildRequest, RuntimeBuilder, RuntimeCore, SurfaceKind};
use crate::storage::{SessionId, SessionRunSelection, Storage};

/// Operator-owned construction. Requests cannot change workspace or authority.
#[async_trait]
pub(crate) trait SessionFactory: Send + Sync {
    async fn build(
        &self,
        id: SessionId,
        interaction: Arc<InteractionService>,
        active: crate::tool::SharedActiveSessionId,
    ) -> Result<RuntimeCore>;
}

pub(crate) struct SessionRuntime {
    pub(crate) root: PathBuf,
    pub(crate) storage: Storage,
    pub(crate) catalog: Arc<crate::model_catalog::ModelCatalog>,
    pub(crate) registry: Arc<crate::provider::ProviderRegistry>,
    pub(crate) session: crate::session::SessionStore,
    pub(crate) budget: crate::run_budget::RunBudget,
    pub(crate) autonomy: crate::yolo::YoloMode,
    pub(crate) trust: crate::workspace_trust::WorkspaceTrustGate,
}

#[async_trait]
impl SessionFactory for SessionRuntime {
    async fn build(
        &self,
        id: SessionId,
        interaction: Arc<InteractionService>,
        active: crate::tool::SharedActiveSessionId,
    ) -> Result<RuntimeCore> {
        let snapshot = self
            .storage
            .load_runtime_session_snapshot(id)
            .await?
            .context("Missing session")?;
        let mut session = self.session.clone();
        session.ensure_provider(&snapshot.summary.provider_id);
        let selected = session.session_mut(&snapshot.summary.provider_id);
        selected.model = snapshot.summary.model.clone();
        selected.reasoning = snapshot.summary.reasoning;
        session.set_current_kind_id(&snapshot.summary.provider_id);
        RuntimeBuilder::new(RuntimeBuildRequest {
            surface: SurfaceKind::Server,
            storage: self.storage.clone(),
            project_root: self.root.clone(),
            session_project_root: self.root.clone(),
            model_catalog: self.catalog.clone(),
            registry: self.registry.clone(),
            session_store: Arc::new(Mutex::new(session)),
            run_budget: self.budget,
            interaction,
            active_session_id: active,
            yolo_mode: self.autonomy.clone(),
            current_session_id: Some(id),
            system_prompt_suffix: None,
            workspace_trust: self.trust.clone(),
        })
        .build()
        .await
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ControlSnapshot {
    pub(crate) session_id: SessionId,
    pub(crate) run_id: Option<String>,
    pub(crate) state: String,
    pub(crate) pending: Vec<InteractionRequest>,
    pub(crate) failure: Option<String>,
}

/// Shared domain data only; neither provider wire messages nor ratatui state.
#[derive(Debug, Clone)]
pub(crate) struct SessionSnapshot {
    pub(crate) control: ControlSnapshot,
    pub(crate) watermark: u64,
    pub(crate) transcript: Vec<crate::protocol::TranscriptItem>,
    pub(crate) plan: crate::plan::PlanDoc,
    pub(crate) todos: Vec<crate::todo::TodoItem>,
    pub(crate) goal: Option<crate::goal::GoalRecord>,
    pub(crate) goal_elapsed_ms: Option<u64>,
    pub(crate) budget: crate::run_budget::RunBudget,
    pub(crate) usage: (u64, u64, Option<u64>),
}

#[derive(Debug)]
pub(crate) enum GoalControl {
    Set(String),
    Pause,
    Resume,
    Clear,
}

#[derive(Debug, Clone, Copy)]
struct LiveGoalRun {
    task_id: crate::storage::TaskRunId,
    started_at: std::time::Instant,
}

type SharedGoalClock = Arc<std::sync::Mutex<Option<LiveGoalRun>>>;

fn live_goal_runtime(clock: &SharedGoalClock) -> Option<(crate::storage::TaskRunId, Duration)> {
    clock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|run| (run.task_id, run.started_at.elapsed()))
}

struct Loaded {
    id: SessionId,
    core: Arc<Mutex<RuntimeCore>>,
    sink: Arc<SessionSink>,
    interaction: Arc<InteractionService>,
    control: Arc<Mutex<ControlSnapshot>>,
    cancellation: CancellationToken,
    ownership: CancellationToken,
    services: Arc<SessionServices>,
    todo_store: crate::todo::SharedTodoStore,
    plan_store: crate::plan::SharedPlanStore,
    goal: crate::goal::GoalService,
    goal_clock: SharedGoalClock,
    run_budget: crate::run_budget::RunBudget,
    worker: Option<tokio::task::JoinHandle<()>>,
    followups: Option<ServerFollowups>,
    busy: crate::storage::SharedBusyFlag,
    storage: Storage,
    released: bool,
}

struct ServerFollowups {
    run: String,
    goal_id: String,
    next_id: u64,
    sender: mpsc::UnboundedSender<crate::agent::QueuedUserMessageCommand>,
    wake: Arc<tokio::sync::Notify>,
}

impl Drop for Loaded {
    fn drop(&mut self) {
        self.ownership.cancel();
        self.cancellation.cancel();
        if self.released {
            return;
        }
        // A cancelled caller or failed bounded shutdown does not abandon children.
        // The detached cleanup owns the OS claim until the actual futures settle.
        let core = self.core.clone();
        let services = self.services.clone();
        let storage = self.storage.clone();
        let id = self.id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let core = core.lock().await;
                loop {
                    core.agent.cancel_running_subagents();
                    let _ = core
                        .background_tasks
                        .stop_all_running(Duration::from_secs(1))
                        .await;
                    let _ = core
                        .terminals
                        .stop_all_running(Duration::from_secs(1))
                        .await;
                    if !core.agent.has_active_subagent_runs()
                        && !core.background_tasks.has_running().await
                        && core
                            .terminals
                            .list()
                            .await
                            .iter()
                            .all(|t| t.status.is_finished())
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                let _ = storage.finish_session_run(id).await;
                let _ = storage
                    .mark_session_status(id, crate::storage::SessionStatus::Interrupted)
                    .await;
                drop(services);
            });
        }
    }
}

struct SessionServices {
    _owner: crate::storage::SessionOwnerGuard,
    heartbeat: tokio::task::JoinHandle<()>,
    prompts: tokio::task::JoinHandle<()>,
    domain: std::sync::Mutex<Option<AbortTask>>,
}

impl Drop for SessionServices {
    fn drop(&mut self) {
        self.heartbeat.abort();
        self.prompts.abort();
    }
}

struct AbortTask(tokio::task::JoinHandle<()>);
impl Drop for AbortTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct State {
    loaded: Option<Loaded>,
    closed: bool,
}

/// Commands serialize under `state`; the worker never holds that mutex.
/// A loaded session retains ownership until explicit drain, including children.
pub(crate) struct Controller {
    root: PathBuf,
    storage: Storage,
    factory: Arc<dyn SessionFactory>,
    state: Mutex<State>,
}

impl Controller {
    pub(crate) fn new(
        root: &Path,
        storage: Storage,
        factory: Arc<dyn SessionFactory>,
    ) -> Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
            storage,
            factory,
            state: Mutex::new(State {
                loaded: None,
                closed: false,
            }),
        })
    }

    /// Create durable state only. It does not acquire an execution slot or run a model.
    pub(crate) async fn create(&self, selection: &SessionRunSelection) -> Result<SessionId> {
        let state = self.state.lock().await;
        anyhow::ensure!(!state.closed, "Controller is shut down");
        let id = self
            .storage
            .start_session(
                &self.root,
                &selection.provider_id,
                &selection.model,
                selection.reasoning,
            )
            .await?;
        self.storage
            .mark_session_status(id, crate::storage::SessionStatus::Interrupted)
            .await?;
        Ok(id)
    }

    /// Claim and restore without calling the model. Another loaded session must
    /// first be drained; idle foreground does not imply idle children.
    pub(crate) async fn resume(
        &self,
        id: SessionId,
        selection: &SessionRunSelection,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        anyhow::ensure!(!state.closed, "Controller is shut down");
        if let Some(loaded) = &state.loaded {
            anyhow::ensure!(loaded.id == id, "Another session owns the execution slot");
            anyhow::ensure!(
                loaded.control.lock().await.failure.is_none(),
                "Session requires recovery"
            );
            return Ok(());
        }
        let owner = self
            .storage
            .try_session_owner(id)?
            .context("Session has a live runtime owner")?;
        anyhow::ensure!(
            self.storage
                .claim_session_with_owner(&self.root, id, Some(selection), &owner)
                .await?
                == crate::storage::ResumeSessionOutcome::Resumed,
            "Session cannot be claimed"
        );
        let result = tokio::time::timeout(Duration::from_secs(30), self.load(id, owner))
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Session initialization timed out")));
        match result {
            Ok(loaded) => {
                state.loaded = Some(loaded);
                Ok(())
            }
            Err(error) => {
                self.storage
                    .mark_session_status(id, crate::storage::SessionStatus::Interrupted)
                    .await?;
                Err(error)
            }
        }
    }

    async fn load(
        &self,
        id: SessionId,
        owner: crate::storage::SessionOwnerGuard,
    ) -> Result<Loaded> {
        self.storage.interrupt_abandoned_server_commands(id).await?;
        let snapshot = self
            .storage
            .load_runtime_session_snapshot(id)
            .await?
            .context("Session disappeared after claim")?;
        let sink = SessionSink::new(snapshot.transcript.clone());
        let (interaction, mut receiver) = InteractionService::new();
        let interaction = Arc::new(interaction);
        let control = Arc::new(Mutex::new(ControlSnapshot {
            session_id: id,
            run_id: None,
            state: "idle".into(),
            pending: Vec::new(),
            failure: None,
        }));
        // Install the receiver before runtime construction: hooks can ask trust
        // questions while building. Unattended bootstrap must fail closed.
        let (boot_tx, mut boot_rx) = mpsc::channel(1);
        let prompts = tokio::spawn({
            let control = control.clone();
            let sink = sink.clone();
            let interaction = interaction.clone();
            async move {
                let mut booting = true;
                loop {
                    tokio::select! {
                        signal = boot_rx.recv(), if booting => {
                            if signal.is_none() { break; }
                            booting = false;
                        }
                        request = receiver.recv() => {
                            let Some(request) = request else { break; };
                            if booting {
                                let _ = interaction.respond(request_id(&request), deny(&request)).await;
                            } else {
                                let mut state = control.lock().await;
                                state.pending.push(request.clone());
                                sink.publish(SessionEvent::Interaction(request));
                            }
                        }
                    }
                }
            }
        });
        let active = Arc::new(Mutex::new(Some(id)));
        let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ownership_cancel = CancellationToken::new();
        let heartbeat = tokio::spawn({
            let storage = self.storage.clone();
            let control = control.clone();
            let cancel = ownership_cancel.clone();
            let busy = busy.clone();
            async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(3));
                loop {
                    ticker.tick().await;
                    if let Err(error) = storage
                        .record_session_heartbeat(
                            id,
                            busy.load(std::sync::atomic::Ordering::Acquire),
                        )
                        .await
                    {
                        cancel.cancel();
                        let mut state = control.lock().await;
                        state.failure = Some(format!(
                            "Session heartbeat lost: {}",
                            crate::redact::redact(&error.to_string())
                        ));
                        state.state = "failed".into();
                        break;
                    }
                }
            }
        });
        let services = Arc::new(SessionServices {
            _owner: owner,
            heartbeat,
            prompts,
            domain: std::sync::Mutex::new(None),
        });
        let mut core = self
            .factory
            .build(id, interaction.clone(), active.clone())
            .await?;
        crate::runtime::session::restore(
            &mut core.agent,
            &self.storage,
            id,
            &core.todo_store,
            &core.plan_store,
            Some(&snapshot),
        )
        .await?;
        // Reattach the durable goal before exposing this runtime. Binding
        // pauses an interrupted active goal; it never starts autonomous work.
        core.agent.goal.bind(self.storage.clone(), id).await?;
        core.agent.defer_goal_finalization = true;
        let goal = core.agent.goal.clone();
        goal.set_change_listener({
            let sink = sink.clone();
            Arc::new(move |record| sink.goal_updated(record))
        })
        .await;
        sink.context_updated(core.agent.context_report());
        let _ = boot_tx.send(()).await;
        crate::storage::activate_session_heartbeat(&self.storage, &active, id).await?;
        anyhow::ensure!(
            !ownership_cancel.is_cancelled(),
            "Session heartbeat lost during initialization"
        );
        let domain = AbortTask(tokio::spawn({
            let plan = core.plan_store.clone();
            let todos = core.todo_store.clone();
            let sink = sink.clone();
            async move {
                let mut timer = tokio::time::interval(Duration::from_millis(50));
                loop {
                    timer.tick().await;
                    let plan = plan.lock().await;
                    let todos = todos.lock().await;
                    sink.domain_updated(&plan, todos.todos());
                }
            }
        }));
        *services
            .domain
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(domain);
        let todo_store = core.todo_store.clone();
        let plan_store = core.plan_store.clone();
        let run_budget = core.run_budget;
        let goal_clock = Arc::new(std::sync::Mutex::new(None));
        Ok(Loaded {
            id,
            core: Arc::new(Mutex::new(core)),
            sink,
            interaction,
            control,
            cancellation: ownership_cancel.child_token(),
            ownership: ownership_cancel,
            services,
            todo_store,
            plan_store,
            goal,
            goal_clock,
            run_budget,
            worker: None,
            followups: None,
            busy,
            storage: self.storage.clone(),
            released: false,
        })
    }

    pub(crate) async fn message(
        &self,
        id: SessionId,
        request: &str,
        prompt: &str,
    ) -> Result<crate::storage::ServerCommand> {
        anyhow::ensure!(
            !request.is_empty() && request.len() <= 128 && !prompt.trim().is_empty(),
            "Invalid message or request ID"
        );
        let mut state = self.state.lock().await;
        anyhow::ensure!(!state.closed, "Controller is shut down");
        let loaded = state.loaded.as_mut().context("Resume the session first")?;
        anyhow::ensure!(loaded.id == id, "Another session owns the execution slot");
        if let Some(command) = self.storage.server_command(id, request, prompt).await? {
            return Ok(command);
        }
        if loaded
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
        {
            let active = loaded.goal.snapshot().await;
            let queue = loaded.followups.as_mut().context("Session is busy")?;
            anyhow::ensure!(
                !loaded.cancellation.is_cancelled()
                    && active.as_ref().is_some_and(
                        |g| g.state == crate::goal::GoalState::Active && g.id == queue.goal_id
                    ),
                "Session is busy"
            );
            anyhow::ensure!(queue.next_id < 64, "Goal follow-up queue is full");
            let message_id = queue.next_id;
            queue.next_id += 1;
            self.storage
                .admit_server_command(id, request, prompt, &queue.run)
                .await?;
            let registered = loaded
                .sink
                .register_followup(&queue.run, message_id, request);
            let sent = registered
                && queue
                    .sender
                    .send(crate::agent::QueuedUserMessageCommand::Send(
                        crate::agent::QueuedUserMessage {
                            id: message_id,
                            display_text: prompt.into(),
                            transcript_text: prompt.into(),
                            input: crate::agent::UserInput::from_text(prompt),
                        },
                    ))
                    .is_ok();
            if sent {
                queue.wake.notify_one();
            } else {
                self.storage
                    .interrupt_server_command(id, request, &queue.run)
                    .await?;
            }
            return Ok(crate::storage::ServerCommand {
                run_id: queue.run.clone(),
                state: if sent { "accepted" } else { "interrupted" }.into(),
            });
        }
        anyhow::ensure!(
            loaded.control.lock().await.failure.is_none(),
            "Session requires recovery after failed persistence"
        );
        let goal = loaded.goal.active_run_lease().await;
        let mut control = loaded.control.lock().await;
        let run = uuid::Uuid::now_v7().to_string();
        self.storage
            .admit_server_command(id, request, prompt, &run)
            .await?;
        loaded.cancellation = loaded.ownership.child_token();
        loaded
            .busy
            .store(true, std::sync::atomic::Ordering::Release);
        control.run_id = Some(run.clone());
        control.state = "running".into();
        drop(control);
        loaded.sink.user_message(prompt);
        loaded.sink.publish(SessionEvent::Lifecycle {
            run_id: Some(run.clone()),
            state: "running".into(),
        });
        let (sender, receiver) = mpsc::unbounded_channel();
        let wake = Arc::new(tokio::sync::Notify::new());
        loaded.followups = goal.as_ref().map(|goal| {
            loaded.sink.begin_followups(&run);
            ServerFollowups {
                run: run.clone(),
                goal_id: goal.record.id.clone(),
                next_id: 0,
                sender,
                wake: wake.clone(),
            }
        });
        loaded.worker = Some(tokio::spawn(execute(
            loaded.core.clone(),
            loaded.sink.clone(),
            loaded.control.clone(),
            self.storage.clone(),
            id,
            run.clone(),
            prompt.to_owned(),
            request.to_owned(),
            goal,
            receiver,
            wake,
            loaded.goal_clock.clone(),
            loaded.cancellation.clone(),
            loaded.busy.clone(),
            loaded.services.clone(),
        )));
        Ok(crate::storage::ServerCommand {
            run_id: run,
            state: "accepted".into(),
        })
    }

    pub(crate) async fn snapshot(&self) -> Result<(ControlSnapshot, Arc<SessionSink>)> {
        let state = self.state.lock().await;
        let loaded = state.loaded.as_ref().context("No loaded session")?;
        Ok((loaded.control.lock().await.clone(), loaded.sink.clone()))
    }

    pub(crate) async fn subscribe(
        &self,
    ) -> Result<(
        SessionSnapshot,
        tokio::sync::broadcast::Receiver<super::sink::OrderedEvent>,
    )> {
        let (snapshot, receiver, _) = self.subscribe_stream().await?;
        Ok((snapshot, receiver))
    }

    /// Obtain the replay owner under the same session fence as the snapshot.
    pub(crate) async fn subscribe_stream(
        &self,
    ) -> Result<(
        SessionSnapshot,
        tokio::sync::broadcast::Receiver<super::sink::OrderedEvent>,
        Arc<SessionSink>,
    )> {
        let state = self.state.lock().await;
        let loaded = state.loaded.as_ref().context("No loaded session")?;
        let control = loaded.control.lock().await;
        // Match the publisher's lock order and retain domain guards through
        // subscription so no domain event can overtake these snapshot values.
        let plan = loaded.plan_store.lock().await;
        let todos = loaded.todo_store.lock().await;
        loaded.sink.domain_updated(&plan, todos.todos());
        let (goal, watermark, transcript, events) = loaded
            .goal
            .with_snapshot(|goal| {
                let goal = goal.cloned();
                loaded.sink.goal_updated(goal.clone());
                let (watermark, transcript, events) = loaded.sink.subscribe();
                (goal, watermark, transcript, events)
            })
            .await;
        let live = live_goal_runtime(&loaded.goal_clock);
        let sink = loaded.sink.clone();
        let mut snapshot = SessionSnapshot {
            control: control.clone(),
            watermark,
            transcript,
            plan: plan.clone(),
            todos: todos.todos().to_vec(),
            goal,
            goal_elapsed_ms: None,
            budget: loaded.run_budget,
            usage: loaded.sink.usage(),
        };
        drop(todos);
        drop(plan);
        drop(control);
        drop(state);
        if let Some(goal) = &snapshot.goal {
            snapshot.goal_elapsed_ms = Some(
                self.storage
                    .goal_elapsed_ms(snapshot.control.session_id, &goal.id, live)
                    .await?,
            );
        }
        Ok((snapshot, events, sink))
    }

    /// Read recorded goal time and, when available, the verified live run.
    pub(crate) async fn goal_elapsed_ms(&self, id: SessionId, goal_id: &str) -> Result<u64> {
        let live = {
            let state = self.state.lock().await;
            state
                .loaded
                .as_ref()
                .filter(|loaded| loaded.id == id)
                .and_then(|loaded| live_goal_runtime(&loaded.goal_clock))
        };
        self.storage.goal_elapsed_ms(id, goal_id, live).await
    }

    /// Release only an actually idle runtime; never cancel another token's work.
    pub(crate) async fn release_idle(&self) -> Result<()> {
        {
            let state = self.state.lock().await;
            if let Some(loaded) = &state.loaded {
                anyhow::ensure!(
                    loaded.worker.as_ref().is_none_or(|w| w.is_finished()),
                    "Session is busy"
                );
                let core = loaded.core.lock().await;
                anyhow::ensure!(
                    !core.agent.has_active_subagent_runs()
                        && !core.background_tasks.has_running().await
                        && core
                            .terminals
                            .list()
                            .await
                            .iter()
                            .all(|t| t.status.is_finished()),
                    "Children are busy"
                );
            }
        }
        self.release(Duration::from_secs(5)).await
    }

    pub(crate) async fn interrupt(&self, id: SessionId) -> Result<()> {
        let state = self.state.lock().await;
        let loaded = state.loaded.as_ref().context("No loaded session")?;
        anyhow::ensure!(loaded.id == id, "Wrong session");
        loaded.cancellation.cancel();
        deny_pending(loaded).await;
        Ok(())
    }

    /// Change goal intent without starting a run. Replacing or pausing a goal
    /// cancels any foreground work that could still be pursuing the old intent.
    pub(crate) async fn control_goal(
        &self,
        id: SessionId,
        action: GoalControl,
    ) -> Result<Option<crate::goal::GoalRecord>> {
        use crate::goal::GoalState;
        if let GoalControl::Set(objective) = &action {
            anyhow::ensure!(
                !objective.trim().is_empty(),
                "Goal objective must not be empty"
            );
            anyhow::ensure!(
                objective.trim().chars().count() <= 16_384,
                "Goal objective exceeds 16384 characters"
            );
        }
        let state = self.state.lock().await;
        anyhow::ensure!(!state.closed, "Controller is shut down");
        let loaded = state.loaded.as_ref().context("Resume the session first")?;
        anyhow::ensure!(loaded.id == id, "Wrong session");
        anyhow::ensure!(
            loaded.control.lock().await.failure.is_none(),
            "Session requires recovery after failed persistence"
        );
        let current = loaded.goal.snapshot().await;
        let busy = loaded
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished());
        let cancel = match action {
            GoalControl::Set(objective) => {
                loaded.goal.set(&objective).await?;
                busy
            }
            GoalControl::Pause => {
                let goal = current.as_ref().context("No goal to pause")?;
                anyhow::ensure!(goal.state == GoalState::Active, "Goal is not active");
                loaded
                    .goal
                    .transition(&goal.id, GoalState::Paused, Some("Paused by user."))
                    .await?;
                anyhow::ensure!(
                    loaded.goal.snapshot().await.is_some_and(
                        |record| record.id == goal.id && record.state == GoalState::Paused
                    ),
                    "Goal changed before it could be paused"
                );
                busy
            }
            GoalControl::Resume => {
                anyhow::ensure!(!busy, "Wait for the current run to stop before resuming");
                let goal = current.as_ref().context("No goal to resume")?;
                anyhow::ensure!(
                    matches!(goal.state, GoalState::Paused | GoalState::Blocked),
                    "Goal cannot be resumed from its current state"
                );
                loaded
                    .goal
                    .transition(&goal.id, GoalState::Active, None)
                    .await?;
                false
            }
            GoalControl::Clear => {
                if current.is_some() {
                    loaded.goal.clear().await?;
                }
                busy && current.is_some_and(|goal| goal.state == GoalState::Active)
            }
        };
        if cancel {
            loaded.cancellation.cancel();
            deny_pending(loaded).await;
        }
        Ok(loaded.goal.snapshot().await)
    }

    pub(crate) async fn respond(
        &self,
        id: SessionId,
        run: &str,
        request: u64,
        outcome: InteractionOutcome,
    ) -> Result<()> {
        let state = self.state.lock().await;
        let loaded = state.loaded.as_ref().context("No loaded session")?;
        anyhow::ensure!(
            loaded.id == id && !loaded.cancellation.is_cancelled(),
            "Wrong or cancelled session"
        );
        let mut control = loaded.control.lock().await;
        anyhow::ensure!(control.run_id.as_deref() == Some(run), "Stale run");
        let index = control
            .pending
            .iter()
            .position(|r| request_id(r) == request)
            .context("Stale interaction")?;
        anyhow::ensure!(
            loaded.interaction.is_pending(request).await,
            "Stale interaction"
        );
        validate_answer(&control.pending[index], &outcome)?;
        loaded
            .interaction
            .respond(request, outcome)
            .await
            .map_err(|_| anyhow::anyhow!("Interaction cancelled"))?;
        control.pending.remove(index);
        loaded
            .sink
            .publish(SessionEvent::InteractionResolved(request));
        Ok(())
    }

    /// Drain before relinquishing ownership. On timeout the slot and claim are
    /// retained: a failed shutdown is never permission to start another session.
    pub(crate) async fn shutdown(&self, timeout: Duration) -> Result<()> {
        self.state.lock().await.closed = true;
        self.release(timeout).await
    }

    /// Explicitly relinquish an idle or cancelled session before switching.
    pub(crate) async fn release(&self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, self.drain())
            .await
            .context("Session drain timed out; ownership retained")?
    }

    async fn drain(&self) -> Result<()> {
        let timeout = Duration::from_secs(5);
        let mut state = self.state.lock().await;
        let Some(loaded) = state.loaded.as_mut() else {
            return Ok(());
        };
        loaded.cancellation.cancel();
        deny_pending(loaded).await;
        if let Some(worker) = &mut loaded.worker {
            tokio::time::timeout(timeout, &mut *worker)
                .await
                .context("Session drain timed out; ownership retained")??;
        }
        loaded.worker = None;
        let core = loaded.core.lock().await;
        core.agent.cancel_running_subagents();
        let _ = core.background_tasks.stop_all_running(timeout).await;
        let _ = core.terminals.stop_all_running(timeout).await;
        anyhow::ensure!(
            !core.agent.has_active_subagent_runs()
                && !core.background_tasks.has_running().await
                && core
                    .terminals
                    .list()
                    .await
                    .iter()
                    .all(|terminal| terminal.status.is_finished()),
            "Session children did not drain"
        );
        if loaded.control.lock().await.failure.is_none() {
            crate::runtime::session::persist(
                &self.storage,
                loaded.id,
                &core.agent,
                &core.todo_store,
                &core.plan_store,
                &loaded.sink.transcript(),
            )
            .await?;
        }
        self.storage
            .mark_session_status(loaded.id, crate::storage::SessionStatus::Interrupted)
            .await?;
        drop(core);
        loaded.released = true;
        state.loaded = None;
        Ok(())
    }
}

async fn deny_pending(loaded: &Loaded) {
    let pending = std::mem::take(&mut loaded.control.lock().await.pending);
    for request in pending {
        let _ = loaded
            .interaction
            .respond(request_id(&request), deny(&request))
            .await;
        loaded
            .sink
            .publish(SessionEvent::InteractionResolved(request_id(&request)));
    }
}

fn request_id(request: &InteractionRequest) -> u64 {
    match request {
        InteractionRequest::Permission { request_id, .. }
        | InteractionRequest::SandboxEscalation { request_id, .. }
        | InteractionRequest::WebDomain { request_id, .. }
        | InteractionRequest::Question { request_id, .. }
        | InteractionRequest::ExtensionTool { request_id, .. }
        | InteractionRequest::HookTrust { request_id, .. } => *request_id,
    }
}

fn deny(request: &InteractionRequest) -> InteractionOutcome {
    match request {
        InteractionRequest::SandboxEscalation { .. } => {
            InteractionOutcome::SandboxEscalation(crate::interaction::EscalationDecision::Deny)
        }
        InteractionRequest::Question { .. } => InteractionOutcome::Question(None),
        _ => InteractionOutcome::Permission(crate::interaction::PermissionDecision::Deny),
    }
}

fn validate_answer(request: &InteractionRequest, answer: &InteractionOutcome) -> Result<()> {
    let valid = match (request, answer) {
        (
            InteractionRequest::SandboxEscalation { .. },
            InteractionOutcome::SandboxEscalation(_),
        ) => true,
        (
            InteractionRequest::Question {
                options, multiple, ..
            },
            InteractionOutcome::Question(Some(crate::interaction::InteractionAnswer::Choices(
                indices,
            ))),
        ) => {
            !indices.is_empty()
                && (*multiple || indices.len() == 1)
                && indices.iter().all(|i| *i < options.len())
                && indices
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == indices.len()
        }
        (InteractionRequest::Question { .. }, InteractionOutcome::Question(None)) => true,
        (
            InteractionRequest::Permission { .. }
            | InteractionRequest::WebDomain { .. }
            | InteractionRequest::ExtensionTool { .. }
            | InteractionRequest::HookTrust { .. },
            InteractionOutcome::Permission(_),
        ) => true,
        _ => false,
    };
    anyhow::ensure!(valid, "Response does not match interaction");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    core: Arc<Mutex<RuntimeCore>>,
    sink: Arc<SessionSink>,
    control: Arc<Mutex<ControlSnapshot>>,
    storage: Storage,
    id: SessionId,
    run: String,
    prompt: String,
    request: String,
    initial_goal: Option<crate::goal::GoalRunLease>,
    mut queued_messages: mpsc::UnboundedReceiver<crate::agent::QueuedUserMessageCommand>,
    wake: Arc<tokio::sync::Notify>,
    goal_clock: SharedGoalClock,
    cancel: CancellationToken,
    busy: crate::storage::SharedBusyFlag,
    _services: Arc<SessionServices>,
) {
    let mut core = core.lock().await;
    let result = execute_turn(
        &mut core,
        &sink,
        &storage,
        id,
        ServerTurn {
            run: &run,
            prompt: &prompt,
            request: &request,
            goal: initial_goal.as_ref(),
            queued_messages: &mut queued_messages,
            wake: &wake,
        },
        &goal_clock,
        cancel.clone(),
    )
    .await;
    *goal_clock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    if result.is_err() {
        queued_messages.close();
        let (delivered, pending) = sink.close_followups(&run);
        for request in delivered.iter().chain(pending.iter()) {
            if let Err(error) = storage.interrupt_server_command(id, request, &run).await {
                tracing::warn!(%error, "failed to interrupt follow-up");
            }
        }
        for request_id in pending {
            sink.publish(SessionEvent::MessageDelivery {
                run_id: run.clone(),
                request_id,
                state: "interrupted".into(),
            });
        }
        cancel.cancel();
        core.agent.cancel_running_subagents();
        let _ = core
            .background_tasks
            .stop_all_running(Duration::from_secs(5))
            .await;
        let _ = core
            .terminals
            .stop_all_running(Duration::from_secs(5))
            .await;
        let _ = storage.finish_session_run(id).await;
        if let Some(goal) = &initial_goal
            && let Err(error) = core
                .agent
                .goal
                .transition_for_run(
                    goal,
                    crate::goal::GoalState::Blocked,
                    Some("Server execution or persistence failed."),
                )
                .await
        {
            tracing::warn!(%error, "failed to reconcile goal after server failure");
        }
    }
    busy.store(false, std::sync::atomic::Ordering::Release);
    let mut state = control.lock().await;
    state.pending.clear();
    match result {
        Ok(()) if state.failure.is_none() => {
            state.state = "idle".into();
        }
        Ok(()) => {
            state.state = "failed".into();
        }
        Err(error) => {
            state.state = "failed".into();
            state.failure = Some(crate::redact::redact(&format!("{error:#}")).into_owned());
        }
    }
    sink.publish(SessionEvent::Lifecycle {
        run_id: Some(run),
        state: state.state.clone(),
    });
}

struct ServerTurn<'a> {
    run: &'a str,
    prompt: &'a str,
    request: &'a str,
    goal: Option<&'a crate::goal::GoalRunLease>,
    queued_messages: &'a mut mpsc::UnboundedReceiver<crate::agent::QueuedUserMessageCommand>,
    wake: &'a tokio::sync::Notify,
}

async fn execute_turn(
    core: &mut RuntimeCore,
    sink: &Arc<SessionSink>,
    storage: &Storage,
    id: SessionId,
    turn: ServerTurn<'_>,
    goal_clock: &SharedGoalClock,
    cancel: CancellationToken,
) -> Result<()> {
    use crate::agent::{AgentRunResult, WaitReason};
    use crate::output::OutputSink;
    use crate::storage::{TaskOutcome, TaskTerminalReason, TaskTerminalReasonCode};
    let ServerTurn {
        run,
        prompt,
        request,
        goal,
        queued_messages,
        wake,
    } = turn;
    let task = match goal {
        Some(goal) => {
            storage
                .start_task_run_with_goal_id(id, None, &goal.record.id, &goal.record.objective)
                .await?
        }
        None => storage.start_task_run(id, None, prompt).await?,
    };
    let active_ms = storage.begin_session_run(id).await?;
    let selected = crate::run_budget::select_runtime_timeout(
        None,
        core.run_budget.max_run_duration(),
        core.run_budget.max_session_active_duration(),
        active_ms,
    );
    let budget_hit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _timer = selected.map(|(duration, _)| {
        AbortTask(tokio::spawn({
            let cancel = cancel.clone();
            let hit = budget_hit.clone();
            async move {
                tokio::time::sleep(duration).await;
                hit.store(true, std::sync::atomic::Ordering::Release);
                cancel.cancel();
            }
        }))
    });
    let verification_before = core.agent.verification_runs().len();
    let review_before = core.agent.self_review_runs().len();
    let authorization_before = storage
        .recent_authorization_decisions(id, 1)
        .await?
        .first()
        .map(|decision| decision.id)
        .unwrap_or(0);
    sink.begin_completion_run();
    core.agent.begin_timing_run();
    if goal.is_some() {
        *goal_clock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(LiveGoalRun {
            task_id: task.id,
            started_at: std::time::Instant::now(),
        });
    }
    let shared_sink: SharedSink = sink.clone();
    // Subscribe before execution so a fast background completion cannot race registration.
    let mut wakes = core.background_wakes.subscribe();
    let mut result = if cancel.is_cancelled() {
        Ok(AgentRunResult::Interrupted(String::new()))
    } else if selected.is_some_and(|(duration, _)| duration.is_zero()) {
        Err(anyhow::Error::new(
            selected
                .and_then(|(_, reason)| reason)
                .context("Missing budget reason")?,
        ))
    } else {
        core.agent
            .run_with_borrowed_queue(
                crate::agent::UserInput::from_text(prompt),
                cancel.clone(),
                shared_sink.clone(),
                queued_messages,
            )
            .await
    };
    while let Ok(AgentRunResult::Waiting(wait)) = &result {
        let waiting = wait.clone();
        let ready: Result<()> = loop {
            tokio::select! {
                () = cancel.cancelled() => { break Ok(()); }
                () = wake.notified() => { if sink.has_pending_followups(run) { break Ok(()); } }
                ready = async {
                    match waiting.clone() {
                        WaitReason::BackgroundWork(wait) => loop {
                            let wake = wakes.recv().await.context("Background wake stream lost; explicit recovery required")?;
                            if wake.wait == wait {
                                core.agent.push_background_work_wake(&wake);
                                break;
                            }
                        },
                        WaitReason::Subagents(_) => { while core.subagents.has_active_detached() && !core.subagents.agent_wake_ready() {
                            core.subagents.wait_for_any_detached(Duration::from_millis(100)).await;
                        } },
                        WaitReason::Peer(wait) => {
                            while !core.peer_bus.recheck_wake_subscription(wait).await? && core.peer_bus.pending_inbox_count().await? == 0 {
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        },
                        WaitReason::StartNewPlan => bail!("Planning surface handoff requires explicit user action"),
                    }
                    Ok(())
                } => break ready,
            }
        };
        if cancel.is_cancelled() {
            break;
        }
        if let Err(error) = ready {
            result = Err(error);
            break;
        }
        if !core.agent.goal.automatic_work_allowed().await {
            break;
        }
        result = core
            .agent
            .continue_with_borrowed_queue(cancel.clone(), shared_sink.clone(), queued_messages)
            .await;
    }
    queued_messages.close();
    let (mut delivered, pending) = sink.close_followups(run);
    delivered.push(request.to_owned());
    // Retain slot until the actual futures and processes have settled.
    core.agent.cancel_running_subagents();
    let background = core
        .background_tasks
        .stop_all_running(Duration::from_secs(5))
        .await;
    let terminals = core
        .terminals
        .stop_all_running(Duration::from_secs(5))
        .await;
    let drain = tokio::time::timeout(Duration::from_secs(5), async {
        while core.agent.has_active_subagent_runs() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    anyhow::ensure!(
        drain.is_ok()
            && background.iter().all(|b| b.status.is_finished())
            && terminals.iter().all(|t| t.status.is_finished()),
        "Children did not drain; session retains ownership"
    );
    let active_ms = storage.finish_session_run(id).await?;
    let exhaustion = if budget_hit.load(std::sync::atomic::Ordering::Acquire) {
        selected.and_then(|(_, reason)| reason)
    } else {
        result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<crate::run_budget::RunBudgetExhaustion>())
            .copied()
    };
    let status = if exhaustion.is_some() {
        crate::completion_report::CompletionStatus::BudgetExhausted
    } else if cancel.is_cancelled() || matches!(&result, Ok(AgentRunResult::Interrupted(_))) {
        crate::completion_report::CompletionStatus::Interrupted
    } else {
        match &result {
            Ok(AgentRunResult::Completed(_)) => {
                crate::completion_report::CompletionStatus::Completed
            }
            Ok(AgentRunResult::Incomplete { failure, .. }) => match failure.outcome {
                crate::agent::CompletionFailureOutcome::Unverified => {
                    crate::completion_report::CompletionStatus::Unverified
                }
                crate::agent::CompletionFailureOutcome::Blocked => {
                    crate::completion_report::CompletionStatus::Blocked
                }
                crate::agent::CompletionFailureOutcome::Failed => {
                    crate::completion_report::CompletionStatus::Failed
                }
                crate::agent::CompletionFailureOutcome::Cancelled => {
                    crate::completion_report::CompletionStatus::Interrupted
                }
            },
            Ok(AgentRunResult::Waiting(_)) => crate::completion_report::CompletionStatus::Blocked,
            _ => crate::completion_report::CompletionStatus::Failed,
        }
    };
    {
        let plan = core.plan_store.lock().await;
        let todos = core.todo_store.lock().await;
        sink.domain_updated(&plan, todos.todos());
    }
    let evidence = sink.completion_evidence();
    let verification = core
        .agent
        .verification_runs()
        .get(verification_before..)
        .and_then(|runs| runs.last())
        .cloned();
    let review = core
        .agent
        .self_review_runs()
        .get(review_before..)
        .and_then(|runs| runs.last())
        .cloned();
    let status = crate::completion_report::classify_completion_status(
        status,
        &evidence,
        verification.as_ref(),
    );
    let decisions = storage
        .recent_authorization_decisions(id, 500)
        .await?
        .into_iter()
        .filter(|decision| decision.id > authorization_before)
        .collect::<Vec<_>>();
    let report = crate::completion_report::CompletionReport::from_evidence(
        status,
        evidence,
        crate::completion_report::CompletionSessionEvidence {
            completion_guard: core.agent.completion_guard_trace(),
            verification,
            review,
            authorization_decisions: &decisions,
            usage: core.agent.usage_totals(),
            session_budget: core
                .agent
                .session_budget_usage()
                .with_active_run_ms(active_ms),
            budget_exhaustion: exhaustion,
        },
    );
    let mut transcript = sink.transcript();
    transcript.push(crate::protocol::TranscriptItem::CompletionReport(Box::new(
        report.clone(),
    )));
    crate::runtime::session::persist(
        storage,
        id,
        &core.agent,
        &core.todo_store,
        &core.plan_store,
        &transcript,
    )
    .await?;
    let outcome = match status {
        crate::completion_report::CompletionStatus::Completed => TaskOutcome::Succeeded,
        crate::completion_report::CompletionStatus::Interrupted => TaskOutcome::Cancelled,
        crate::completion_report::CompletionStatus::Blocked
        | crate::completion_report::CompletionStatus::BudgetExhausted => TaskOutcome::Blocked,
        _ => TaskOutcome::Failed,
    };
    let reason = outcome.requires_reason().then(|| {
        TaskTerminalReason::new(
            if exhaustion.is_some() {
                TaskTerminalReasonCode::BudgetExhausted
            } else if outcome == TaskOutcome::Cancelled {
                TaskTerminalReasonCode::UserCancelled
            } else {
                TaskTerminalReasonCode::ExecutionFailure
            },
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .as_deref()
                .unwrap_or("Task did not satisfy its completion contract"),
        )
    });
    let timing = core.agent.take_timing_checkpoint();
    if !timing.is_empty()
        && let Err(error) = storage.checkpoint_task_run_timing(task.id, &timing).await
    {
        tracing::warn!(task_run_id = %task.id, error = %format!("{error:#}"), "failed to persist task timing");
    }
    storage
        .finish_task_run(task.id, outcome, reason.as_ref())
        .await?;
    storage.finish_server_commands(id, run, &delivered).await?;
    for request_id in pending {
        sink.publish(SessionEvent::MessageDelivery {
            run_id: run.into(),
            request_id,
            state: "interrupted".into(),
        });
    }
    sink.forget_followups(run);
    storage
        .api_audit(
            None,
            Some(id.as_i64()),
            Some(run),
            run,
            "completion",
            match outcome {
                TaskOutcome::Succeeded => "succeeded",
                TaskOutcome::Cancelled => "cancelled",
                TaskOutcome::Blocked => "blocked",
                _ => "failed",
            },
        )
        .await?;
    super::artifacts::Artifacts::new(storage.home_dir())?
        .publish_completion(storage, id.as_i64(), run, &report)
        .await?;
    if let Some(goal) = goal {
        let (state, reason) = crate::goal::terminal_from_completion_guarded(
            status,
            exhaustion,
            report.completion_guard.as_ref(),
        );
        core.agent
            .goal
            .transition_for_run(goal, state, reason.as_deref())
            .await?;
    }
    sink.completion(&report);
    Ok(())
}
