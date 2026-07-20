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
                success,
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
            success: task.status.is_success(),
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
        TerminalEvent::Started { .. } => {}
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
                success,
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
            success: terminal.status.is_success(),
            finished_at: Instant::now(),
        }));
    }
    effect
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
            success: matches!(
                completion.snapshot.status,
                crate::subagent::SubagentStatus::Succeeded
            ),
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
        let result = if task_id.starts_with("pty-") {
            terminals.remove(&task_id).await.map(|_| ())
        } else {
            background_tasks.remove(&task_id).await.map(|_| ())
        };
        let error = result
            .err()
            .map(|err| UiError::new("Background task removal failed", format!("{err:#}")));
        let _ = sender.send(RuntimeEvent::BackgroundTaskRemovalFinished { task_id, error });
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
        tool_call_id: terminal.tool_call_id.clone(),
    }
}
