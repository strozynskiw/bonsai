use super::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct BackgroundTaskUiEffect {
    pub(super) refresh_task_list: bool,
    pub(super) wake_candidate: bool,
}

pub(super) fn apply_background_task_event(
    app: &mut AppState,
    event: BackgroundTaskEvent,
) -> BackgroundTaskUiEffect {
    let mut effect = BackgroundTaskUiEffect {
        refresh_task_list: true,
        wake_candidate: false,
    };
    match event {
        BackgroundTaskEvent::Started { .. } => {}
        BackgroundTaskEvent::Output {
            tool_call_id: Some(id),
            tail,
            ..
        } => app.reduce(AppAction::Agent(UiEvent::ToolOutput {
            id,
            output: tail,
            updated_at: Instant::now(),
        })),
        BackgroundTaskEvent::Finished {
            tool_call_id: Some(id),
            status,
            summary,
            success,
            ..
        } => {
            effect.wake_candidate = !matches!(status, BackgroundTaskStatus::Stopped);
            app.reduce(AppAction::Agent(UiEvent::ToolFinished {
                id,
                result: summary,
                status: if matches!(status, BackgroundTaskStatus::Stopped) {
                    crate::output::ToolExecutionStatus::Interrupted
                } else if success {
                    crate::output::ToolExecutionStatus::Succeeded
                } else {
                    crate::output::ToolExecutionStatus::Failed
                },
                finished_at: Instant::now(),
            }));
        }
        BackgroundTaskEvent::Output { .. }
        | BackgroundTaskEvent::Finished { .. }
        | BackgroundTaskEvent::Removed { .. } => {}
    }
    effect
}

pub(super) fn apply_background_task_snapshot(
    app: &mut AppState,
    task: &BackgroundTaskSnapshot,
) -> BackgroundTaskUiEffect {
    let mut effect = BackgroundTaskUiEffect {
        refresh_task_list: true,
        wake_candidate: false,
    };

    let Some(tool_call_id) = task.tool_call_id.as_ref() else {
        return effect;
    };

    if !task.tail.is_empty() {
        app.reduce(AppAction::Agent(UiEvent::ToolOutput {
            id: tool_call_id.clone(),
            output: task.tail.clone(),
            updated_at: Instant::now(),
        }));
    }

    if task.status.is_finished() {
        effect.wake_candidate = !matches!(task.status, BackgroundTaskStatus::Stopped);
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: tool_call_id.clone(),
            result: task.detail(),
            status: if matches!(task.status, BackgroundTaskStatus::Stopped) {
                crate::output::ToolExecutionStatus::Interrupted
            } else if task.status.is_success() {
                crate::output::ToolExecutionStatus::Succeeded
            } else {
                crate::output::ToolExecutionStatus::Failed
            },
            finished_at: Instant::now(),
        }));
    }

    effect
}

pub(super) fn apply_terminal_event(
    app: &mut AppState,
    event: TerminalEvent,
) -> BackgroundTaskUiEffect {
    let _terminal_id = event.terminal_id();
    let mut effect = BackgroundTaskUiEffect {
        refresh_task_list: true,
        wake_candidate: false,
    };
    match event {
        TerminalEvent::Started { .. } | TerminalEvent::Resized { .. } => {}
        TerminalEvent::Output {
            tool_call_id: Some(id),
            output,
            ..
        } => app.reduce(AppAction::Agent(UiEvent::ToolOutput {
            id,
            output,
            updated_at: Instant::now(),
        })),
        TerminalEvent::WaitingForInput {
            tool_call_id: Some(id),
            summary,
            ..
        } => {
            effect.wake_candidate = true;
            app.reduce(AppAction::Agent(UiEvent::ToolOutput {
                id,
                output: summary,
                updated_at: Instant::now(),
            }));
        }
        TerminalEvent::Finished {
            tool_call_id: Some(id),
            status,
            summary,
            success,
            ..
        } => {
            effect.wake_candidate = !matches!(status, TerminalStatus::Stopped);
            app.reduce(AppAction::Agent(UiEvent::ToolFinished {
                id,
                result: summary,
                status: if matches!(status, TerminalStatus::Stopped) {
                    crate::output::ToolExecutionStatus::Interrupted
                } else if success {
                    crate::output::ToolExecutionStatus::Succeeded
                } else {
                    crate::output::ToolExecutionStatus::Failed
                },
                finished_at: Instant::now(),
            }));
        }
        TerminalEvent::Output { .. }
        | TerminalEvent::WaitingForInput { .. }
        | TerminalEvent::Finished { .. }
        | TerminalEvent::Removed { .. } => {}
    }
    effect
}

/// Drain the background-task and terminal channels for one frame: apply each
/// event to `app`, refresh the `/tasks` list if it's open, and arm a
/// background-agent wake when one just became ready. Returns whether anything
/// changed (i.e. the frame needs a redraw). One of `run`'s per-frame phases,
/// extracted so the loop body reads as named steps.
pub(super) async fn drain_background_and_terminal_channels(
    background_events: &mut tokio::sync::broadcast::Receiver<BackgroundTaskEvent>,
    terminal_events: &mut tokio::sync::broadcast::Receiver<TerminalEvent>,
    app: &mut AppState,
    background_tasks: &Arc<BackgroundTaskRegistry>,
    terminals: &Arc<TerminalRegistry>,
    pending_background_wake: &mut Option<Instant>,
) -> bool {
    let mut changed = false;
    let mut refresh_task_list = false;
    let mut wake_candidate = false;
    while let Ok(event) = background_events.try_recv() {
        changed = true;
        let effect = apply_background_task_event(app, event);
        refresh_task_list |= effect.refresh_task_list;
        wake_candidate |= effect.wake_candidate;
    }
    while let Ok(event) = terminal_events.try_recv() {
        changed = true;
        let effect = apply_terminal_event(app, event);
        refresh_task_list |= effect.refresh_task_list;
        wake_candidate |= effect.wake_candidate;
    }
    if refresh_task_list && app.is_task_list_open() {
        refresh_background_task_list(app, background_tasks, terminals).await;
    }
    if wake_candidate
        && (background_tasks.agent_wake_ready().await || terminals.agent_wake_ready().await)
    {
        *pending_background_wake = Some(Instant::now() + BACKGROUND_AGENT_WAKE_DELAY);
    }
    changed
}

/// Drain the subagent activity channel for one frame: keep the `/subtasks`
/// snapshot fresh while it's open, adopt each subagent's model onto its
/// launching `agent` tool call, and arm a background-agent wake when a subagent
/// just finished. Returns whether the frame needs a redraw. One of `run`'s
/// per-frame phases.
pub(super) fn drain_subagent_channel(
    subagent_events: &mut tokio::sync::broadcast::Receiver<crate::subagent::SubagentEvent>,
    app: &mut AppState,
    subagents: &crate::subagent::SubagentRegistry,
    pending_background_wake: &mut Option<Instant>,
) -> bool {
    let mut changed = false;
    let mut subagents_changed = false;
    let mut subagent_wake_candidate = false;
    loop {
        match subagent_events.try_recv() {
            Ok(event) => {
                subagents_changed = true;
                // Activity only changes the /subtasks snapshot. Marking the main
                // view dirty for every streamed token bypasses the active redraw
                // throttle and can overwhelm slower terminals when several
                // subagents stream concurrently. The normal active-frame deadline
                // still advances timers at 10 FPS.
                changed |= subagent_event_requests_redraw(event, app.is_subtask_list_open());
                if matches!(event, crate::subagent::SubagentEvent::Finished) {
                    subagent_wake_candidate = true;
                }
            }
            // A burst that overran the 256-slot buffer still means the list
            // changed; treat the lag as a change so we don't skip the refresh.
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                subagents_changed = true;
                subagent_wake_candidate = true;
                changed |= app.is_subtask_list_open();
            }
            Err(_) => break,
        }
    }
    if subagents_changed {
        // Runs adopt their model onto the launching `agent` tool call via a
        // cheap ids+models projection; the full snapshot list (which clones every
        // run's activity) is still only taken while /subagents is open.
        changed |= app.adopt_subagent_models(&subagents.tool_call_models());
        if app.is_subtask_list_open() {
            app.reduce(AppAction::RefreshSubtaskList {
                subtasks: subagents.list(),
            });
        }
    }
    if subagent_wake_candidate && subagents.agent_wake_ready() {
        *pending_background_wake = Some(Instant::now() + BACKGROUND_AGENT_WAKE_DELAY);
    }
    changed
}

pub(super) fn apply_terminal_snapshot(
    app: &mut AppState,
    terminal: &TerminalSnapshot,
) -> BackgroundTaskUiEffect {
    let mut effect = BackgroundTaskUiEffect {
        refresh_task_list: true,
        wake_candidate: false,
    };
    let Some(tool_call_id) = terminal.tool_call_id.as_ref() else {
        return effect;
    };
    if !terminal.tail.is_empty() {
        app.reduce(AppAction::Agent(UiEvent::ToolOutput {
            id: tool_call_id.clone(),
            output: terminal.tail.clone(),
            updated_at: Instant::now(),
        }));
    }
    if terminal.status.is_finished() {
        effect.wake_candidate = !matches!(terminal.status, TerminalStatus::Stopped);
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: tool_call_id.clone(),
            result: terminal.detail(),
            status: if matches!(terminal.status, TerminalStatus::Stopped) {
                crate::output::ToolExecutionStatus::Interrupted
            } else if terminal.status.is_success() {
                crate::output::ToolExecutionStatus::Succeeded
            } else {
                crate::output::ToolExecutionStatus::Failed
            },
            finished_at: Instant::now(),
        }));
    }
    effect
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn drain_background_work_wakes(
    wakes: &mut tokio::sync::broadcast::Receiver<crate::background_wake::BackgroundWorkWake>,
    pending_wakes: &mut Vec<crate::background_wake::BackgroundWorkWake>,
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    pending_background_wake: &mut Option<Instant>,
    background_tasks: &Arc<BackgroundTaskRegistry>,
    terminals: &Arc<TerminalRegistry>,
) -> bool {
    while let Ok(wake) = wakes.try_recv() {
        pending_wakes.push(wake);
    }
    if app.task_state.is_busy() || tasks.is_busy() {
        return false;
    }
    let Some(wake_index) = pending_wakes.iter().position(|wake| {
        app.waiting_for_background_work
            .as_ref()
            .is_some_and(|waiting| {
                waiting.subscription_id == wake.wait.subscription_id
                    && waiting.requester_generation == wake.wait.requester_generation
                    && waiting.target_kind == wake.wait.target_kind
                    && waiting.target_id == wake.wait.target_id
                    && waiting.target_incarnation == wake.wait.target_incarnation
            })
    }) else {
        return false;
    };
    let wake = pending_wakes.remove(wake_index);
    app.waiting_for_background_work = None;
    *pending_background_wake = None;
    match wake.wait.target_kind {
        crate::storage::BackgroundWakeTargetKind::BackgroundTask => {
            background_tasks
                .acknowledge_exact_wake(&wake.wait.target_id, &wake.wait.target_incarnation)
                .await;
        }
        crate::storage::BackgroundWakeTargetKind::Terminal => {
            terminals
                .acknowledge_exact_wake(&wake.wait.target_id, &wake.wait.target_incarnation)
                .await;
        }
    }
    agent.lock().await.push_background_work_wake(&wake);
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(Instant::now());
    let truncation = if wake.output_truncated {
        "\n(Output delta was truncated.)"
    } else {
        ""
    };
    app.reduce(AppAction::Agent(UiEvent::Thinking(format!(
        "{} woke at version {} ({}){}",
        wake.wait.target_id, wake.wake_version, wake.reason, truncation
    ))));
    if let Err(error) = tasks.start_agent_resume(agent.clone(), sink.clone(), app.active_mode()) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(error)));
    }
    true
}

pub(super) fn apply_completed_subagent_tool_calls(
    app: &mut AppState,
    subagents: &crate::subagent::SubagentRegistry,
) -> bool {
    let mut changed = false;
    for completion in subagents.completed_tool_calls() {
        if !matches!(
            app.tool_activity(&completion.tool_call_id)
                .map(|activity| activity.status),
            Some(crate::tui::app::ToolStatus::Running)
        ) {
            continue;
        }

        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: completion.tool_call_id,
            result: completed_subagent_tool_result(&completion.snapshot),
            status: match completion.snapshot.status {
                crate::subagent::SubagentStatus::Succeeded => {
                    crate::output::ToolExecutionStatus::Succeeded
                }
                crate::subagent::SubagentStatus::Cancelled => {
                    crate::output::ToolExecutionStatus::Interrupted
                }
                crate::subagent::SubagentStatus::Running
                | crate::subagent::SubagentStatus::Failed
                | crate::subagent::SubagentStatus::TimedOut => {
                    crate::output::ToolExecutionStatus::Failed
                }
            },
            finished_at: Instant::now(),
        }));
        changed = true;
    }
    changed
}

fn completed_subagent_tool_result(snapshot: &crate::subagent::SubagentSnapshot) -> String {
    format!(
        "{} {} as '{}'\n\n{}",
        snapshot.id,
        snapshot.status.label(),
        snapshot.agent,
        snapshot.detail()
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn maybe_start_background_wake(
    pending_background_wake: &mut Option<Instant>,
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    background_tasks: &Arc<BackgroundTaskRegistry>,
    terminals: &Arc<TerminalRegistry>,
    subagents: &Arc<crate::subagent::SubagentRegistry>,
    peer_bus: &Arc<crate::peer::PeerBus>,
) -> bool {
    let Some(deadline) = *pending_background_wake else {
        return false;
    };
    if app.waiting_for_background_work.is_some() {
        return false;
    }
    if Instant::now() < deadline {
        return false;
    }
    if !background_tasks.agent_wake_ready().await
        && !terminals.agent_wake_ready().await
        && !subagents.agent_wake_ready()
    {
        *pending_background_wake = None;
        return false;
    }
    if app.task_state.is_busy() || tasks.is_busy() {
        return false;
    }

    *pending_background_wake = None;
    // A background-task wake is not a peer hop — reset the anti-loop chain so
    // sends from this turn count as fresh (peers P3).
    peer_bus.begin_turn(crate::peer::TurnOrigin::Human);
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(Instant::now());
    app.reduce(AppAction::Agent(UiEvent::Thinking(
        "Background work finished; resuming agent".to_string(),
    )));
    // Resume under the session's live persona, not a hardcoded Coding. A
    // hardcoded mode let a detached subagent finishing in Plan view resume the
    // read-only planner AS the coding agent (`start_agent_resume` calls
    // `set_mode`), which then implemented the freshly drafted plan without
    // `/start` — bypassing phased todo seeding and canvas progress entirely
    // (observed live: a whole multi-phase plan ran outside the runner).
    if let Err(err) = tasks.start_agent_resume(agent, sink, app.active_mode()) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
    }
    true
}

pub(super) async fn open_tasks_command_if_exact(
    input: &str,
    app: &mut AppState,
    background_tasks: &Arc<BackgroundTaskRegistry>,
    terminals: &Arc<TerminalRegistry>,
) -> bool {
    if input.trim() != "/tasks" {
        return false;
    }
    app.reduce(AppAction::SubmitCommandInput(input.trim().to_string()));
    open_background_task_list(app, background_tasks, terminals).await;
    true
}

pub(in crate::tui) async fn open_background_task_list(
    app: &mut AppState,
    background_tasks: &Arc<BackgroundTaskRegistry>,
    terminals: &Arc<TerminalRegistry>,
) {
    refresh_background_task_list(app, background_tasks, terminals).await;
    app.reduce(AppAction::OpenTaskList);
}

pub(super) async fn refresh_background_task_list(
    app: &mut AppState,
    background_tasks: &Arc<BackgroundTaskRegistry>,
    terminals: &Arc<TerminalRegistry>,
) {
    let (mut tasks, terminal_tasks) = tokio::join!(background_tasks.list(), terminals.list());
    tasks.extend(terminal_tasks.iter().map(terminal_as_background_snapshot));
    app.reduce(AppAction::RefreshTaskList { tasks });
}

pub(in crate::tui) fn start_delete_selected_background_task(
    app: &mut AppState,
    background_tasks: Arc<BackgroundTaskRegistry>,
    terminals: Arc<TerminalRegistry>,
    sender: mpsc::UnboundedSender<RuntimeEvent>,
) {
    let Some(task) = app.selected_background_task().cloned() else {
        return;
    };
    if matches!(
        task.status,
        crate::background::BackgroundTaskStatus::Running
    ) {
        app.reduce(AppAction::SetTaskListStatus(Some(format!(
            "Stopping {}...",
            task.id
        ))));
    } else {
        app.reduce(AppAction::SetTaskListStatus(Some(format!(
            "Removing {}...",
            task.id
        ))));
    }
    let task_id = task.id.clone();
    tokio::spawn(async move {
        if let Err(panic) = std::panic::AssertUnwindSafe(async {
            let result = if task_id.starts_with("pty-") {
                terminals.remove(&task_id).await.map(|_| ())
            } else {
                background_tasks.remove(&task_id).await.map(|_| ())
            };
            let error = result
                .err()
                .map(|err| UiError::new("Background task removal failed", format!("{err:#}")));
            let _ = sender.send(RuntimeEvent::BackgroundTaskRemovalFinished { task_id, error });
        })
        .catch_unwind()
        .await
        {
            tracing::error!(?panic, "background task removal panicked");
        }
    });
}

fn terminal_as_background_snapshot(terminal: &TerminalSnapshot) -> BackgroundTaskSnapshot {
    let status = match terminal.status {
        TerminalStatus::Running => BackgroundTaskStatus::Running,
        TerminalStatus::Succeeded => BackgroundTaskStatus::Succeeded,
        TerminalStatus::Failed => BackgroundTaskStatus::Failed,
        TerminalStatus::TimedOut => BackgroundTaskStatus::TimedOut,
        TerminalStatus::Stopped => BackgroundTaskStatus::Stopped,
    };
    BackgroundTaskSnapshot {
        id: terminal.id.clone(),
        incarnation: terminal.incarnation.clone(),
        command: terminal.command.clone(),
        cwd: terminal.cwd.clone(),
        status,
        started_at: terminal.started_at,
        finished_at: terminal.finished_at,
        exit_code: terminal.exit_code,
        timeout_secs: terminal.timeout_secs,
        timed_out: matches!(terminal.status, TerminalStatus::TimedOut),
        tail: terminal.live_output(),
        tail_truncated: terminal.tail_truncated,
        total_output_chars: terminal.total_output_chars,
        version: terminal.version,
        tool_call_id: terminal.tool_call_id.clone(),
    }
}
