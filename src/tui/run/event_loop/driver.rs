//! The interactive TUI frame loop, split out of `run` so the loop's state is a
//! named struct instead of ~60 locals.

use super::*;

macro_rules! runtime_action_deps {
    ($s:ident) => {
        RuntimeActionDeps {
            interaction: $s.interaction.clone(),
            runtime_sender: $s.runtime_sender.clone(),
            agent: $s.agent.clone(),
            memory: $s.memory.clone(),
            yolo_mode: $s.yolo_mode.clone(),
            session_store: $s.session_store.clone(),
            permissions: $s.permissions.clone(),
            domain_permissions: $s.domain_permissions.clone(),
            registry: $s.registry.clone(),
            model_catalog: $s.model_catalog.clone(),
            storage: &$s.storage,
            project_root: &$s.project_root,
            session_project_root: &$s.session_project_root,
            active_session_id: $s.active_session_id.clone(),
            todo_store: $s.todo_store.clone(),
            plan_store: $s.plan_store.clone(),
            sink: $s.sink.clone(),
            background_tasks: $s.background_tasks.clone(),
            terminals: $s.terminals.clone(),
            peer_bus: Some($s.peer_bus.clone()),
        }
    };
}

/// Why a frame ended.
pub(super) enum Flow {
    /// Run the next frame.
    Continue,
    /// Leave the loop and shut the TUI down.
    Exit,
}

/// Every handle and per-loop mutable value one frame needs.
pub(super) struct TuiLoop {
    pub(super) agent: Arc<Mutex<Agent>>,
    pub(super) session_store: Arc<Mutex<SessionStore>>,
    pub(super) registry: Arc<ProviderRegistry>,
    pub(super) todo_store: Arc<Mutex<crate::todo::TodoStore>>,
    pub(super) plan_store: Arc<Mutex<crate::plan::PlanDoc>>,
    pub(super) interaction_rx: mpsc::UnboundedReceiver<crate::interaction::InteractionRequest>,
    pub(super) interaction: Arc<crate::interaction::InteractionService>,
    pub(super) storage: Storage,
    pub(super) model_catalog: Arc<crate::model_catalog::ModelCatalog>,
    pub(super) active_session_id: Arc<Mutex<Option<crate::storage::SessionId>>>,
    pub(super) background_tasks: Arc<BackgroundTaskRegistry>,
    pub(super) terminals: Arc<TerminalRegistry>,
    pub(super) subagents: Arc<crate::subagent::SubagentRegistry>,
    pub(super) peer_bus: Arc<crate::peer::PeerBus>,
    pub(super) project_root: std::path::PathBuf,
    pub(super) session_project_root: std::path::PathBuf,
    pub(super) terminal: TerminalSession,
    pub(super) ui_receiver: mpsc::UnboundedReceiver<UiEvent>,
    pub(super) runtime_receiver: mpsc::UnboundedReceiver<RuntimeEvent>,
    pub(super) runtime_sender: mpsc::UnboundedSender<RuntimeEvent>,
    pub(super) background_spawns: tokio::task::JoinSet<()>,
    pub(super) update_config: crate::config::UpdateConfig,
    pub(super) app: AppState,
    pub(super) current_session_id: SessionId,
    pub(super) goal_service: crate::goal::GoalService,
    pub(super) busy_flag: crate::storage::SharedBusyFlag,
    pub(super) last_model_cache_signature: u64,
    pub(super) memory: Option<Arc<crate::memory::MemoryService>>,
    pub(super) sink: SharedSink,
    pub(super) tasks: TaskController,
    pub(super) yolo_mode: crate::yolo::YoloMode,
    pub(super) permissions: crate::permissions::PermissionManager,
    pub(super) domain_permissions: crate::permissions::PermissionManager,
    pub(super) termination_requested: Arc<AtomicBool>,
    pub(super) background_events: tokio::sync::broadcast::Receiver<BackgroundTaskEvent>,
    pub(super) terminal_events: tokio::sync::broadcast::Receiver<TerminalEvent>,
    pub(super) background_wake_events:
        tokio::sync::broadcast::Receiver<crate::background_wake::BackgroundWorkWake>,
    pub(super) pending_background_work_wakes: Vec<crate::background_wake::BackgroundWorkWake>,
    pub(super) subagent_events: tokio::sync::broadcast::Receiver<crate::subagent::SubagentEvent>,
    pub(super) pending_background_wake: Option<Instant>,
    pub(super) pending_peer_wake: Option<PendingPeerWake>,
    pub(super) pending_peer_wait_recheck: Option<PendingPeerWaitRecheck>,
    pub(super) repo_map: RepoMapInjector,
    pub(super) ctrl_c_prompt: Option<CtrlCExitPrompt>,
    pub(super) deferred_ui_event: Option<UiEvent>,
    pub(super) terminal_input: TerminalInput,
    pub(super) terminal_input_health: TerminalInputHealth,
    pub(super) tty_hangup_watch: TtyHangupWatch,
    pub(super) next_persistence_flush: Instant,
    pub(super) redraw_requested: bool,
    pub(super) next_redraw_at: Instant,
    pub(super) next_goal_refresh_at: Instant,
    pub(super) pending_selection_auto_scroll: Option<PendingSelectionAutoScroll>,
    pub(super) plan_edited_pending: bool,
    pub(super) last_active_persona: crate::agent::ActivePersona,
    pub(super) persisted_signatures: PersistedSnapshotSignatures,
    pub(super) pending_snapshot_flush: Option<tokio::task::JoinHandle<PersistenceFlushResult>>,
    pub(super) snapshot_flush_gate: SnapshotFlushGate,
}

impl TuiLoop {
    /// Runs frames until one asks to exit. An `Err` skips the orderly
    /// shutdown, exactly as `?` inside the old inline loop did.
    pub(super) async fn drive(&mut self) -> Result<()> {
        loop {
            if matches!(self.tick().await?, Flow::Exit) {
                return Ok(());
            }
        }
    }

    async fn tick(&mut self) -> Result<Flow> {
        // Frame budget watch: everything between here and the event poll runs
        // on the UI thread, and a slow segment freezes the app — keypresses
        // (including the Ctrl+C exit dance) queue unread until the next poll.
        // Users report that as "the app hangs"; the warn below names the
        // culprit segment instead of leaving a dark zone in the log.
        let frame_work_started = Instant::now();
        let mut frame_persist = Duration::ZERO;
        let mut frame_needs_redraw = false;

        match self.terminal.ensure_raw_mode() {
            Ok(true) => {
                tracing::warn!(
                    "terminal raw mode was reset externally; restored it and discarded queued input"
                );
                frame_needs_redraw = true;
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "terminal raw mode was reset and could not be restored; disabled mouse capture"
                );
                if self.app.mouse_capture {
                    self.app.reduce(AppAction::ToggleMouseCapture);
                }
            }
        }

        if self
            .pending_snapshot_flush
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
            && let Some(handle) = self.pending_snapshot_flush.take()
        {
            match handle.await {
                Ok(flush) => apply_persistence_flush_result(
                    flush,
                    self.current_session_id,
                    &mut self.app,
                    &self.agent,
                    &mut self.persisted_signatures,
                ),
                Err(error) => tracing::warn!(
                    session_id = %self.current_session_id,
                    error = %error,
                    "periodic snapshot task failed"
                ),
            }
        }

        // A SIGTERM/SIGHUP (terminal window closed, `kill`, logout) lands here
        // within a frame and exits through bounded cleanup. It is still an
        // interruption, not a successful completion: preserve the snapshot but
        // leave the session explicitly resumable.
        if self.termination_requested.load(Ordering::Relaxed) {
            tracing::info!(
                session_id = %self.current_session_id,
                "received termination signal; exiting cleanly"
            );
            self.tasks.abort();
            prepare_termination_shutdown(&mut self.app);
            return Ok(Flow::Exit);
        }
        clear_expired_ctrl_c_prompt(&mut self.app, &mut self.ctrl_c_prompt, Instant::now());
        if let Some(pending) = self.pending_selection_auto_scroll
            && self.app.pointer_selecting
            && Instant::now() >= pending.due_at
        {
            self.pending_selection_auto_scroll = handle_mouse_event(
                pending.mouse,
                &mut self.app,
                &mut self.ctrl_c_prompt,
                &mut self.terminal,
                &self.agent,
                &self.tasks,
            )
            .await?
            .map(|mouse| PendingSelectionAutoScroll {
                mouse,
                due_at: Instant::now() + SELECTION_AUTO_SCROLL_INTERVAL,
            });
            frame_needs_redraw = true;
        } else if !self.app.pointer_selecting {
            self.pending_selection_auto_scroll = None;
        }
        let mut changed_files = Vec::new();
        frame_needs_redraw |= apply_ui_events_for_frame(
            &mut self.app,
            &mut self.ui_receiver,
            &mut self.deferred_ui_event,
            &mut changed_files,
        );
        if !changed_files.is_empty() {
            // Record off the frame path; the upsert is tiny and low-frequency
            // (one batch per finished edit/write tool).
            let storage = self.storage.clone();
            let session_id = self.current_session_id;
            tokio::spawn(async move {
                if let Err(panic) = std::panic::AssertUnwindSafe(async {
                    if let Err(err) = storage
                        .record_session_file_changes(session_id, &changed_files)
                        .await
                    {
                        tracing::debug!(%err, "file-change ledger write failed");
                    }
                })
                .catch_unwind()
                .await
                {
                    tracing::error!(?panic, "file-change ledger write panicked");
                }
            });
        }
        // Outgoing peer messages echo into this session's own chat as blue
        // `you → #N` messages at the send site.
        let peer_echoes = self.peer_bus.drain_echoes();
        if !peer_echoes.is_empty() {
            frame_needs_redraw = true;
        }
        for echo in peer_echoes {
            self.app.reduce(AppAction::PeerMessage {
                source_message_id: None,
                delivery_receipt: None,
                session_id: echo.to_session_id.as_i64(),
                outgoing: true,
                text: echo.body,
            });
        }

        frame_needs_redraw |= drain_background_and_terminal_channels(
            &mut self.background_events,
            &mut self.terminal_events,
            &mut self.app,
            &self.background_tasks,
            &self.terminals,
            &mut self.pending_background_wake,
        )
        .await;
        frame_needs_redraw |= drain_background_work_wakes(
            &mut self.background_wake_events,
            &mut self.pending_background_work_wakes,
            &mut self.app,
            &mut self.tasks,
            self.agent.clone(),
            self.sink.clone(),
            &mut self.pending_background_wake,
            &self.background_tasks,
            &self.terminals,
        )
        .await;

        // Subagent activity: while `/subagents` is open, keep its snapshot fresh
        // so it updates live. Each snapshot deep-clones the run's activity and
        // result (up to ~18 KB), so the work stays gated on the modal being open.
        frame_needs_redraw |= drain_subagent_channel(
            &mut self.subagent_events,
            &mut self.app,
            &self.subagents,
            &mut self.pending_background_wake,
        );

        let registry_before_drain = Arc::as_ptr(&self.registry);
        let catalog_before_drain = Arc::as_ptr(&self.model_catalog);
        frame_needs_redraw |= drain_runtime_events_for_frame(
            &mut self.runtime_receiver,
            &mut self.app,
            &self.tasks,
            &mut self.current_session_id,
            &mut self.persisted_signatures,
            &mut self.model_catalog,
            &mut self.registry,
            &mut self.last_model_cache_signature,
            &mut self.pending_peer_wake,
            &mut self.pending_peer_wait_recheck,
            &mut self.repo_map,
            &mut self.background_spawns,
            RuntimeEventDrainDeps {
                storage: &self.storage,
                agent: &self.agent,
                active_session_id: &self.active_session_id,
                project_root: &self.session_project_root,
                plan_store: &self.plan_store,
                session_store: &self.session_store,
                background_tasks: &self.background_tasks,
                terminals: &self.terminals,
                peer_bus: &self.peer_bus,
                memory: &self.memory,
            },
        )
        .await;
        // A `/refresh` swapped the registry/catalog handles; re-install them in
        // the dispatcher's persona-model deps so future runs resolve models
        // against the fresh catalog.
        if Arc::as_ptr(&self.registry) != registry_before_drain
            || Arc::as_ptr(&self.model_catalog) != catalog_before_drain
        {
            self.tasks
                .set_persona_model_deps(crate::tui::task::PersonaModelDeps {
                    agent: self.agent.clone(),
                    session_store: self.session_store.clone(),
                    registry: self.registry.clone(),
                    model_catalog: self.model_catalog.clone(),
                    custom_agents: self.app.custom_agents.clone(),
                });
        }
        // ToolStarted and SubagentEvent travel over separate channels. Reconcile
        // after every runtime drain so a completion observed on an earlier frame
        // still closes a transcript row that arrives later.
        frame_needs_redraw |= apply_completed_subagent_tool_calls(&mut self.app, &self.subagents);
        // Inject the async-built repo map as soon as the agent is free.
        if let Some(report) = self.repo_map.try_apply_when_idle(&self.agent) {
            self.app.latest_context_report = Some(report);
            frame_needs_redraw = true;
        }
        // Exit the loop as soon as a runtime event (e.g. a CommandFinished
        // with quit:true from /quit, or the runtime-reducer arm) flips us to
        // Exiting, so we skip one final draw before breaking below.
        if matches!(self.app.task_state, TaskState::Exiting) {
            tracing::info!(
                pending_task = self.tasks.is_busy(),
                "TUI exiting; run loop broken"
            );
            return Ok(Flow::Exit);
        }
        // Drive phase auto-advance from this unconditional per-frame site, not
        // only the poll_finished site. The AgentFinished reducer sets
        // phase_advance + Idle at the top-of-loop drain, which can land a frame
        // *after* poll_finished already reaped (take()s) the run handle — so a
        // poll_finished-only driver would miss the signal and the phased plan
        // would stall after the first phase. maybe_advance self-gates on
        // idle + !is_busy + phase_advance.take(), so it is a cheap no-op
        // otherwise. Deferred commands run before phase advancement at both
        // dispatch sites; queued user input remains last in the idle slot.
        // Starting a new plan protects and clears the previous canvas before
        // any deferred command can act on it. It takes priority over phase
        // advancement and queued work.
        let started_new_plan = maybe_start_new_plan(
            &mut self.app,
            &mut self.tasks,
            self.agent.clone(),
            self.sink.clone(),
            &mut self.repo_map,
            &self.storage,
            self.current_session_id,
            self.plan_store.clone(),
        )
        .await;
        frame_needs_redraw |= started_new_plan;
        frame_needs_redraw |=
            maybe_report_empty_fresh_plan(&mut self.app, &mut self.tasks, &self.plan_store).await;
        if !started_new_plan {
            let started_deferred = start_next_deferred_command_if_idle(
                &mut self.app,
                &mut self.tasks,
                self.agent.clone(),
                self.session_store.clone(),
                &self.project_root,
                DeferredCommandDeps {
                    registry: self.registry.clone(),
                    model_catalog: self.model_catalog.clone(),
                    storage: Some(&self.storage),
                },
            )
            .await;
            frame_needs_redraw |= started_deferred;
            let advanced = !started_deferred
                && maybe_advance_plan_phase(
                    &mut self.app,
                    &mut self.tasks,
                    self.agent.clone(),
                    self.sink.clone(),
                    self.todo_store.clone(),
                    self.plan_store.clone(),
                    &mut self.repo_map,
                )
                .await;
            frame_needs_redraw |= advanced;
            if !started_deferred && !advanced {
                frame_needs_redraw |= start_pending_queued_run_if_idle(
                    &mut self.app,
                    &mut self.tasks,
                    self.agent.clone(),
                    self.sink.clone(),
                    &mut self.repo_map,
                    &self.registry,
                    &self.model_catalog,
                )
                .await;
            }
        }
        frame_needs_redraw |= maybe_start_background_wake(
            &mut self.pending_background_wake,
            &mut self.app,
            &mut self.tasks,
            self.agent.clone(),
            self.sink.clone(),
            &self.background_tasks,
            &self.terminals,
            &self.subagents,
            &self.peer_bus,
        )
        .await;
        frame_needs_redraw |= maybe_start_peer_wake(
            &mut self.pending_peer_wake,
            &mut self.app,
            &mut self.tasks,
            self.agent.clone(),
            self.sink.clone(),
            &self.peer_bus,
        )
        .await;
        maybe_recheck_peer_wait(
            &mut self.pending_peer_wait_recheck,
            &self.app,
            &self.tasks,
            &self.peer_bus,
        )
        .await;

        if let Some(ModalKind::Detail(crate::tui::event::DetailModal::QuestionPrompt {
            request_id,
            ..
        })) = self.app.modal.as_ref()
            && !self.interaction.is_pending(*request_id).await
        {
            self.app.modal = None;
            self.app.focus = Focus::Input;
            push_command_message(
                &mut self.app,
                CommandOutputKind::Status,
                "Question resolved; work continues. Use /questions for pending questions.",
            );
            frame_needs_redraw = true;
        }
        frame_needs_redraw |= apply_interaction_request(&mut self.app, &mut self.interaction_rx);
        frame_needs_redraw |= super::goal::maybe_start_goal(
            &mut self.app,
            &mut self.tasks,
            &self.goal_service,
            &self.agent,
            &self.sink,
        )
        .await;

        if Instant::now() >= self.next_goal_refresh_at {
            frame_needs_redraw |= super::goal::sync_goal_snapshot(
                &mut self.app,
                &self.tasks,
                &self.goal_service,
                &self.storage,
            )
            .await;
            self.next_goal_refresh_at = Instant::now() + Duration::from_secs(1);
        }

        self.app.reduce(AppAction::Tick);
        // A growing bonsai animates at the poll cadence; without this the
        // idle loop repaints once a second and the growth would stutter.
        if self.app.bonsai_growing() {
            frame_needs_redraw = true;
        }
        frame_needs_redraw |= sync_todo_snapshot(&mut self.app, &self.todo_store);
        if let Ok(plan) = self.plan_store.try_lock() {
            frame_needs_redraw |= sync_plan_store_snapshot(
                &mut self.app,
                &plan,
                self.tasks.active_agent_mode(),
                &mut self.plan_edited_pending,
            );
        }
        let plan_run_busy = self.app.task_state.is_busy() || self.tasks.is_busy();
        frame_needs_redraw |= show_plan_top_when_ready(
            &mut self.app,
            self.tasks.active_agent_mode(),
            plan_run_busy,
            &mut self.plan_edited_pending,
        );

        frame_needs_redraw |= refresh_cached_model_choices(
            &mut self.app,
            &self.session_store,
            &self.registry,
            &self.model_catalog,
            &mut self.last_model_cache_signature,
        );

        let now = Instant::now();
        if now >= self.next_persistence_flush && self.pending_snapshot_flush.is_none() {
            // `is_busy` covers a run holding the agent lock (agent state can
            // change under it); the gate's bounded heartbeat covers anything
            // that settles just after a run releases it.
            if self.snapshot_flush_gate.needs_flush(
                self.current_session_id,
                &self.app,
                self.tasks.is_busy(),
            ) {
                let persist_started = Instant::now();
                self.pending_snapshot_flush = Some(spawn_changed_snapshot_flush(
                    &self.storage,
                    self.current_session_id,
                    &self.app,
                    &self.agent,
                    self.persisted_signatures,
                ));
                frame_persist = persist_started.elapsed();
            }
            self.next_persistence_flush = now + PERSISTENCE_FLUSH_INTERVAL;
        }

        let activity = self.tasks.reconcile_session_activity().await?;
        if let Some(activity) = activity {
            if activity.active
                && !self.tasks.is_busy()
                && self
                    .tasks
                    .session_active_budget_exhausted(activity.active_run_ms)
            {
                self.agent.lock().await.cancel_running_subagents();
                frame_needs_redraw = true;
            }
            if activity.active {
                self.app.mark_run_started(Instant::now());
            } else {
                self.app.mark_run_finished(Instant::now());
            }
        }
        // The live session clock rides this same tick: the ledger is shared with
        // the interaction service and a snapshot is a lock-free read, so the
        // header can show what the run is charging without a new timer, a new
        // storage read, or the agent lock a live run holds. Only a live run gets
        // a reading: the ledger keeps the finished run's intervals until the next
        // run re-anchors, so an unguarded snapshot would keep advertising compute
        // beside the frozen run timer.
        let session_clock = if self.app.run_started_at.is_some() {
            session_clock_label(self.interaction.timing().map(|ledger| ledger.snapshot()))
        } else {
            None
        };
        if self.app.session_clock_label != session_clock {
            self.app.session_clock_label = session_clock;
            frame_needs_redraw = true;
        }
        if mirror_run_status(&mut self.app, &self.tasks, &self.subagents, &self.busy_flag) {
            self.redraw_requested = true;
        }

        if frame_needs_redraw {
            self.redraw_requested = true;
        }
        let mut frame_draw = Duration::ZERO;
        let now = Instant::now();
        if should_draw_frame(self.redraw_requested, now, self.next_redraw_at) {
            let draw_started = Instant::now();
            draw_tui_frame(&mut self.terminal, &mut self.app)?;
            frame_draw = draw_started.elapsed();
            self.redraw_requested = false;
            self.next_redraw_at = Instant::now()
                + redraw_interval(
                    any_task_active(
                        &self.app,
                        &self.tasks,
                        &self.subagents,
                        &self.background_tasks,
                        &self.terminals,
                    )
                    .await,
                );
        }

        if matches!(self.app.task_state, TaskState::Exiting) {
            tracing::info!(
                pending_task = self.tasks.is_busy(),
                "TUI exiting; run loop broken"
            );
            return Ok(Flow::Exit);
        }

        // Liveness-check the tty BEFORE entering the crossterm poll: with the
        // terminal dead, `event::poll` can spin on EOF internally and never
        // return, so this is the last point where the loop can still exit.
        if self.tty_hangup_watch.poll_hangup() {
            tracing::warn!(
                consecutive_hangups = self.tty_hangup_watch.consecutive_hangups,
                "controlling terminal hung up; exiting TUI"
            );
            self.tasks.abort();
            prepare_termination_shutdown(&mut self.app);
            return Ok(Flow::Exit);
        }

        // Name the culprit when a frame's work starved the input path. 500ms is
        // 10 poll periods — users feel it as a hang, and a queued Ctrl+C exit
        // sits unread the whole time.
        let frame_work = frame_work_started.elapsed();
        if frame_work >= SLOW_FRAME_WARN_THRESHOLD {
            tracing::warn!(
                frame_work_ms = frame_work.as_millis() as u64,
                persist_ms = frame_persist.as_millis() as u64,
                draw_ms = frame_draw.as_millis() as u64,
                task_state = ?self.app.task_state,
                "slow UI frame; input (incl. Ctrl+C) was starved this long"
            );
        }

        let poll_started_at = Instant::now();
        // CPU-time bracket around the blocking poll (no await between the two
        // readings, so both observe the same OS thread).
        let poll_cpu_started = thread_cpu_time();
        let has_terminal_event = match self.terminal_input.poll(EVENT_POLL_TIMEOUT) {
            Ok(has_event) => has_event,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => false,
            Err(err) => {
                tracing::warn!(error = %err, "terminal event poll failed; exiting TUI");
                self.tasks.abort();
                prepare_termination_shutdown(&mut self.app);
                return Ok(Flow::Exit);
            }
        };
        let poll_cpu_spent = match (poll_cpu_started, thread_cpu_time()) {
            (Some(started), Some(ended)) => ended.saturating_sub(started),
            _ => Duration::ZERO,
        };
        if self.terminal_input_health.record_poll(
            has_terminal_event,
            poll_started_at.elapsed(),
            poll_cpu_spent,
        ) {
            tracing::warn!(
                fast_empty_polls = self.terminal_input_health.fast_empty_polls,
                busy_empty_polls = self.terminal_input_health.busy_empty_polls,
                poll_timeout_ms = EVENT_POLL_TIMEOUT.as_millis() as u64,
                "terminal input appears disconnected; exiting TUI"
            );
            self.tasks.abort();
            prepare_termination_shutdown(&mut self.app);
            return Ok(Flow::Exit);
        }

        if has_terminal_event {
            let terminal_event = match self.terminal_input.read() {
                Ok(event) => Some(event),
                Err(err) if err.kind() == io::ErrorKind::Interrupted => None,
                Err(err) => {
                    tracing::warn!(error = %err, "terminal event read failed; exiting TUI");
                    self.tasks.abort();
                    prepare_termination_shutdown(&mut self.app);
                    return Ok(Flow::Exit);
                }
            };
            // Pointer motion is discarded wholesale (see
            // `terminal_event_is_pointer_motion`): it must not request a
            // redraw, clear the Ctrl+C prompt, or pay the terminal-size ioctl.
            let terminal_event = terminal_event.filter(|event| {
                self.pending_selection_auto_scroll.is_some()
                    || !terminal_event_is_pointer_motion(event)
            });
            if let Some(terminal_event) = terminal_event {
                self.redraw_requested = true;
                if terminal_event_requests_immediate_redraw(&terminal_event) {
                    let draw_started = Instant::now();
                    draw_tui_frame(&mut self.terminal, &mut self.app)?;
                    let frame_draw = draw_started.elapsed();
                    self.redraw_requested = false;
                    self.next_redraw_at = Instant::now()
                        + redraw_interval(
                            any_task_active(
                                &self.app,
                                &self.tasks,
                                &self.subagents,
                                &self.background_tasks,
                                &self.terminals,
                            )
                            .await,
                        );
                    if frame_draw >= SLOW_FRAME_WARN_THRESHOLD {
                        tracing::warn!(
                            draw_ms = frame_draw.as_millis() as u64,
                            task_state = ?self.app.task_state,
                            "slow immediate resize redraw"
                        );
                    }
                }
                match terminal_event {
                    Event::Key(key) => {
                        let intent = map_key(key, &self.app);
                        if clears_ctrl_c_prompt(&intent) {
                            clear_ctrl_c_prompt(&mut self.app, &mut self.ctrl_c_prompt);
                        }
                        match intent {
                            KeyIntent::Action(action) => {
                                if matches!(action, AppAction::BudgetWarningSubmit) {
                                    if let Some(submission) =
                                        take_budget_warning_submission(&mut self.app)
                                    {
                                        let input = super::goal::command_input(&submission);
                                        let mut persistence = PersistenceCommandState {
                                            current_session_id: &mut self.current_session_id,
                                            signatures: &mut self.persisted_signatures,
                                        };
                                        if handle_toplevel_submit_command(
                                            &input,
                                            &mut self.app,
                                            &mut self.tasks,
                                            runtime_action_deps!(self),
                                            &mut persistence,
                                            &self.subagents,
                                            &self.peer_bus,
                                            &self.update_config,
                                            &self.goal_service,
                                        )
                                        .await
                                        {
                                            return Ok(Flow::Continue);
                                        }
                                        if let Some(command) = idle_slash_command(&input) {
                                            self.app.reduce(AppAction::ScrollBottom);
                                            self.app.reduce(AppAction::SubmitCommandInput(
                                                input.clone(),
                                            ));
                                            let mut persistence = PersistenceCommandState {
                                                current_session_id: &mut self.current_session_id,
                                                signatures: &mut self.persisted_signatures,
                                            };
                                            handle_idle_slash_command(
                                                command,
                                                &input,
                                                &mut self.app,
                                                &mut self.tasks,
                                                runtime_action_deps!(self),
                                                &mut persistence,
                                            )
                                            .await;
                                        } else {
                                            let _ = submit_and_start_run(
                                                submission,
                                                &input,
                                                &mut self.app,
                                                &mut self.tasks,
                                                runtime_action_deps!(self),
                                                &mut self.repo_map,
                                            )
                                            .await;
                                        }
                                    }
                                    return Ok(Flow::Continue);
                                }
                                // The Alt+S keybind opens `/subagents` from the cached
                                // snapshot; refresh it first so the modal opens to
                                // current data even though live refresh (above) is
                                // gated on the modal being open.
                                if matches!(action, AppAction::OpenSubtaskList) {
                                    self.app.reduce(AppAction::RefreshSubtaskList {
                                        subtasks: self.subagents.list(),
                                    });
                                }
                                let mut persistence = PersistenceCommandState {
                                    current_session_id: &mut self.current_session_id,
                                    signatures: &mut self.persisted_signatures,
                                };
                                let result = handle_runtime_action(
                                    action,
                                    &mut self.app,
                                    &mut self.tasks,
                                    runtime_action_deps!(self),
                                    &mut persistence,
                                )
                                .await;
                                if let RuntimeActionResult::Unhandled(action) = result {
                                    apply_action_with_task_side_effects(
                                        *action,
                                        &mut self.app,
                                        &self.tasks,
                                    );
                                }
                            }
                            KeyIntent::Quit => {
                                self.tasks.abort();
                                clear_ctrl_c_prompt(&mut self.app, &mut self.ctrl_c_prompt);
                                self.app.reduce(AppAction::SetTaskState(TaskState::Exiting));
                                return Ok(Flow::Exit);
                            }
                            KeyIntent::CancelOrQuit => {
                                if handle_ctrl_c(
                                    &mut self.app,
                                    &mut self.tasks,
                                    &mut self.ctrl_c_prompt,
                                    &self.subagents,
                                    &self.agent,
                                )
                                .await
                                {
                                    return Ok(Flow::Exit);
                                }
                            }
                            KeyIntent::Steer => {
                                steer_active_run(&mut self.app, &self.tasks);
                            }
                            intent @ (KeyIntent::Submit | KeyIntent::SubmitReplacement(_)) => {
                                if let KeyIntent::SubmitReplacement(replacement) = &intent {
                                    self.app
                                        .reduce(AppAction::CompleteInputTo(replacement.clone()));
                                }
                                // Snapshot the composer once: `input` is the
                                // placeholder display text used for slash-command
                                // detection and the transcript, while
                                // `submission.input` carries the expanded model
                                // payload (chip text spliced in, images attached).
                                let submission = self.app.composer.submission();
                                let input = super::goal::command_input(&submission);
                                if input.is_empty() {
                                    return Ok(Flow::Continue);
                                }
                                if matches!(self.app.task_state, TaskState::Idle)
                                    && submitted_input_dispatches_model_work(&input)
                                    && open_budget_warning_if_reached(
                                        &mut self.app,
                                        &self.tasks,
                                        &self.agent,
                                        submission.clone(),
                                    )
                                    .await
                                {
                                    return Ok(Flow::Continue);
                                }
                                let mut persistence = PersistenceCommandState {
                                    current_session_id: &mut self.current_session_id,
                                    signatures: &mut self.persisted_signatures,
                                };
                                if handle_toplevel_submit_command(
                                    &input,
                                    &mut self.app,
                                    &mut self.tasks,
                                    runtime_action_deps!(self),
                                    &mut persistence,
                                    &self.subagents,
                                    &self.peer_bus,
                                    &self.update_config,
                                    &self.goal_service,
                                )
                                .await
                                {
                                    return Ok(Flow::Continue);
                                }
                                if let Some(command) = idle_slash_command(&input) {
                                    // Every inline command echoes into the
                                    // transcript the same way; the arms below do
                                    // only their distinctive work.
                                    self.app.reduce(AppAction::ScrollBottom);
                                    self.app
                                        .reduce(AppAction::SubmitCommandInput(input.clone()));
                                    let mut persistence = PersistenceCommandState {
                                        current_session_id: &mut self.current_session_id,
                                        signatures: &mut self.persisted_signatures,
                                    };
                                    handle_idle_slash_command(
                                        command,
                                        &input,
                                        &mut self.app,
                                        &mut self.tasks,
                                        runtime_action_deps!(self),
                                        &mut persistence,
                                    )
                                    .await;
                                    return Ok(Flow::Continue);
                                }

                                if submit_and_start_run(
                                    submission,
                                    &input,
                                    &mut self.app,
                                    &mut self.tasks,
                                    runtime_action_deps!(self),
                                    &mut self.repo_map,
                                )
                                .await
                                {
                                    return Ok(Flow::Continue);
                                }
                            }
                            KeyIntent::Insert(ch) => self.app.reduce(AppAction::InputChar(ch)),
                            KeyIntent::Noop => {}
                        }
                    }
                    Event::Mouse(mouse) => {
                        // While edge auto-scroll is armed, all-motion events update
                        // its stored pointer position. The timer above applies the
                        // drag at a fixed cadence, rather than scrolling at the
                        // terminal's potentially much higher motion-event rate.
                        if matches!(mouse.kind, crossterm::event::MouseEventKind::Moved)
                            && let Some(pending) = self.pending_selection_auto_scroll.as_mut()
                        {
                            pending.mouse = crossterm::event::MouseEvent {
                                kind: crossterm::event::MouseEventKind::Drag(
                                    crossterm::event::MouseButton::Left,
                                ),
                                ..mouse
                            };
                        } else {
                            self.pending_selection_auto_scroll = handle_mouse_event(
                                mouse,
                                &mut self.app,
                                &mut self.ctrl_c_prompt,
                                &mut self.terminal,
                                &self.agent,
                                &self.tasks,
                            )
                            .await?
                            .map(|mouse| PendingSelectionAutoScroll {
                                mouse,
                                due_at: Instant::now() + SELECTION_AUTO_SCROLL_INTERVAL,
                            });
                        }
                    }
                    Event::Paste(text) => {
                        handle_paste_event(
                            text,
                            &mut self.app,
                            &mut self.ctrl_c_prompt,
                            &self.registry,
                            &self.model_catalog,
                        );
                    }
                    _ => {}
                }
            }
        }

        if reap_finished_task(&mut self.app, &mut self.tasks).await {
            self.redraw_requested = true;
            // Do not dispatch a replacement here. `AgentFinished` may still be
            // queued; the next frame drains it before the unconditional idle
            // dispatch site starts plans, commands, phases, or queued input.
            // A persona selected while the run was in flight was deliberately
            // not applied mid-run (the busy guard skipped it). Apply it now
            // that the run ended — completed, failed, or interrupted alike.
            // When one of the dispatchers above already started a follow-up
            // run, its busy guard makes this a no-op (each dispatch sets its
            // own persona anyway).
            sync_agent_mode_and_refresh(&mut self.app, self.agent.clone()).await;
            apply_idle_persona_model(&mut self.app, &self.tasks, &mut self.redraw_requested).await;
        }

        // After all event processing for the frame, a view change (Tab, `1`,
        // `2`, mouse, implement_plan, review_changes) means the selected agent
        // persona changed, so the tool-schema count in the persisted context
        // report is stale. Regenerate it before the next draw.
        //
        // A persona change never cancels an in-flight run: the running turn
        // captured its persona at dispatch and keeps it — the user may just be
        // peeking at the plan canvas mid-implementation, and the review agent's
        // first finding auto-opens that canvas under the reviewer's feet
        // (regression: session 7, the finding's own auto-open cancelled the
        // review 228µs after it was recorded). While busy this sync is a no-op
        // (its internal guard); the post-completion sync in the poll_finished
        // block applies the selection once the run ends, and every dispatch
        // path sets its persona explicitly, so the next user-sent run always
        // starts under the newly selected persona.
        if self.app.active_persona != self.last_active_persona {
            self.last_active_persona = self.app.active_persona.clone();
            sync_agent_mode_and_refresh(&mut self.app, self.agent.clone()).await;
            // Idle persona switch: apply the new persona's model right away so
            // the meta line names the model the next run will actually use.
            // While busy this skips; dispatch applies it when the run starts.
            apply_idle_persona_model(&mut self.app, &self.tasks, &mut self.redraw_requested).await;
            self.redraw_requested = true;
        }
        Ok(Flow::Continue)
    }
}
