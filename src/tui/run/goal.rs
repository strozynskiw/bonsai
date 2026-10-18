use super::*;
use crate::goal::{GoalCommand, GoalService, GoalState};

pub(super) fn command_input(submission: &crate::tui::app::ComposerSubmission) -> String {
    let display = submission.display_text.trim();
    if GoalCommand::parse(display).is_some() {
        submission.input.text.trim().to_string()
    } else {
        display.to_string()
    }
}

pub(super) async fn handle_goal_command(
    command: GoalCommand<'_>,
    app: &mut AppState,
    tasks: &TaskController,
    goal: &GoalService,
) -> Result<()> {
    match command {
        GoalCommand::Status => {
            let text = match goal.snapshot().await {
                None => "No session goal. Use /goal <objective>.".to_string(),
                Some(record) => {
                    let progress = app.latest_context_report.as_ref().map(|report| format!(
                        "\nSession usage: {} input + {} output tokens; cost {}.\nProgress: {} pending todo item(s). See /ctx, /cost and /tasks for evidence and limits.",
                        report.session_prompt_tokens, report.session_completion_tokens,
                        crate::tui::widgets::sidebar::optional_cost_micros(report.session_cost_micros),
                        app.todo.iter().filter(|item| !matches!(item.status, crate::todo::TodoStatus::Completed | crate::todo::TodoStatus::Cancelled)).count(),
                    )).unwrap_or_default();
                    let budget = app.run_budget;
                    let limit = |value: Option<u64>| {
                        value.map_or_else(|| "none".to_string(), |value| value.to_string())
                    };
                    let limits = format!(
                        "\nLimits: run turns {}; run time {}s; session turns {}; tokens {}; time {}s; cost {} µUSD.",
                        budget
                            .max_turns
                            .map_or_else(|| "default".into(), |value| value.to_string()),
                        limit(budget.max_run_seconds),
                        limit(budget.max_session_turns.map(|value| value as u64)),
                        limit(budget.max_session_billed_tokens),
                        limit(budget.max_session_active_seconds),
                        limit(budget.max_session_cost_micros)
                    );
                    format!(
                        "Goal {} · {}\n{}{}{}{}\nQuestion timeout: {}s; /questions reopens pending questions.",
                        record.id,
                        record.state.label(),
                        record.objective,
                        record
                            .reason
                            .as_ref()
                            .map(|reason| format!("\nStop reason: {reason}"))
                            .unwrap_or_default(),
                        progress,
                        limits,
                        record.question_timeout_seconds,
                    )
                }
            };
            push_command_message(app, CommandOutputKind::Status, &text);
        }
        GoalCommand::Set(objective) => {
            let record = goal.set(objective).await?;
            retire_queued_goal_input(app, tasks);
            if tasks.is_busy() {
                tasks.interrupt_foreground();
            }
            goal.request_start();
            push_command_message(
                app,
                CommandOutputKind::Status,
                &format!("Goal set: {}", record.objective),
            );
        }
        GoalCommand::Pause | GoalCommand::Clear => {
            goal.cancel_start();
            if let Some(record) = goal.snapshot().await {
                if matches!(command, GoalCommand::Pause) && record.state != GoalState::Active {
                    push_command_message(
                        app,
                        CommandOutputKind::Status,
                        &format!("Goal remains {}.", record.state.label()),
                    );
                    return Ok(());
                }
                goal.transition(&record.id, GoalState::Paused, Some("Paused by user."))
                    .await?;
                if record.state == GoalState::Active && tasks.is_busy() {
                    tasks.interrupt_foreground();
                }
            }
            if matches!(command, GoalCommand::Clear) {
                goal.clear().await?;
                retire_queued_goal_input(app, tasks);
            }
            push_command_message(
                app,
                CommandOutputKind::Status,
                if matches!(command, GoalCommand::Clear) {
                    "Goal cleared; history retained."
                } else {
                    "Goal paused."
                },
            );
        }
        GoalCommand::Resume => {
            let record = goal
                .snapshot()
                .await
                .context("No goal to resume. Use /goal <objective>.")?;
            if record.state == GoalState::Active && tasks.is_busy() {
                push_command_message(app, CommandOutputKind::Status, "Goal is already running.");
                return Ok(());
            }
            goal.transition(&record.id, GoalState::Active, None).await?;
            goal.request_start();
            push_command_message(app, CommandOutputKind::Status, "Goal resumed.");
        }
    }
    Ok(())
}

fn retire_queued_goal_input(app: &mut AppState, tasks: &TaskController) {
    let queued = app
        .queued_inputs
        .iter()
        .map(|input| (input.id, input.content.submission().transcript_text))
        .collect::<Vec<_>>();
    for (id, text) in queued {
        let _ = tasks.cancel_agent_message(id);
        app.reduce(AppAction::CancelQueuedInput { id });
        push_command_message(
            app,
            CommandOutputKind::Status,
            &format!("Queued message retired with the previous goal: {text}"),
        );
    }
}

/// Starts only an explicit set/resume request, once the previous foreground run
/// has released its lock. Opening a session alone never arms this path.
pub(super) async fn maybe_start_goal(
    app: &mut AppState,
    tasks: &mut TaskController,
    goal: &GoalService,
    agent: &Arc<Mutex<Agent>>,
    sink: &SharedSink,
) -> bool {
    if tasks.is_busy() || app.task_state.is_busy() {
        return false;
    }
    let Some(record) = goal.take_requested_start().await else {
        return false;
    };
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(std::time::Instant::now());
    app.reduce(AppAction::Agent(UiEvent::Thinking(
        "Working toward session goal".into(),
    )));
    let queued = app
        .queued_inputs
        .iter()
        .map(|input| {
            let submission = input.content.submission();
            QueuedUserMessage {
                id: input.id,
                display_text: input.text.clone(),
                transcript_text: submission.transcript_text,
                input: submission.input,
            }
        })
        .collect();
    if let Err(error) = tasks.start_agent_run_with_queued(
        agent.clone(),
        crate::agent::UserInput::from_text(&record.objective),
        sink.clone(),
        app.active_persona.clone(),
        queued,
    ) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(error)));
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_objective_uses_expanded_paste_payload() {
        let submission = crate::tui::app::ComposerSubmission {
            display_text: "/goal [Text 1]".into(),
            transcript_text: "/goal Implement export\nPreserve CSV and JSON formats".into(),
            input: crate::agent::UserInput::from_text(
                "/goal Implement export\nPreserve CSV and JSON formats",
            ),
        };
        assert_eq!(command_input(&submission), submission.input.text);
    }
}
