use std::time::{Duration, Instant};

use crate::tui::app::AppState;
use crate::tui::event::{AppAction, TaskState};
use crate::tui::keymap::KeyIntent;

pub(super) const CTRL_C_EXIT_WINDOW: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CtrlCExitPrompt {
    pub(super) kind: CtrlCExitPromptKind,
    pub(super) expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CtrlCExitPromptKind {
    Exit,
    Cancelling,
}

impl CtrlCExitPromptKind {
    pub(super) fn notice(self) -> &'static str {
        match self {
            Self::Exit => "Ctrl+C again to exit",
            Self::Cancelling => "Cancelling... Ctrl+C again to exit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CtrlCAction {
    ArmExit,
    CancelRunAndArm,
    CancelSubagentsAndArm,
    CancelCommandAndArm,
    ConfirmExit,
}

pub(super) fn next_ctrl_c_action(
    task_state: TaskState,
    has_running_subagents: bool,
    prompt: Option<CtrlCExitPrompt>,
    now: Instant,
) -> CtrlCAction {
    if prompt.is_some_and(|prompt| prompt.expires_at > now) {
        return CtrlCAction::ConfirmExit;
    }

    match task_state {
        TaskState::Running => CtrlCAction::CancelRunAndArm,
        TaskState::Command => CtrlCAction::CancelCommandAndArm,
        TaskState::Idle if has_running_subagents => CtrlCAction::CancelSubagentsAndArm,
        TaskState::Idle | TaskState::Cancelling => CtrlCAction::ArmExit,
        TaskState::Exiting => CtrlCAction::ConfirmExit,
    }
}

pub(super) fn set_ctrl_c_prompt(
    app: &mut AppState,
    prompt: &mut Option<CtrlCExitPrompt>,
    kind: CtrlCExitPromptKind,
    now: Instant,
) {
    *prompt = Some(CtrlCExitPrompt {
        kind,
        expires_at: now + CTRL_C_EXIT_WINDOW,
    });
    app.reduce(AppAction::SetShutdownNotice(Some(
        kind.notice().to_string(),
    )));
}

pub(super) fn clear_ctrl_c_prompt(app: &mut AppState, prompt: &mut Option<CtrlCExitPrompt>) {
    if prompt.take().is_some() || app.shutdown_notice.is_some() {
        app.reduce(AppAction::SetShutdownNotice(None));
    }
}

pub(super) fn clear_expired_ctrl_c_prompt(
    app: &mut AppState,
    prompt: &mut Option<CtrlCExitPrompt>,
    now: Instant,
) {
    if prompt.is_some_and(|prompt| prompt.expires_at <= now) {
        clear_ctrl_c_prompt(app, prompt);
    }
}

/// A keystroke intent that should disarm the armed "Ctrl+C again to exit"
/// prompt. `CancelOrQuit` extends the window (it's the next Ctrl+C itself) and
/// `Noop` is an unmapped keypress, which we deliberately ignore so random keys
/// don't silently cancel a user's pending exit.
pub(super) fn clears_ctrl_c_prompt(intent: &KeyIntent) -> bool {
    !matches!(intent, KeyIntent::CancelOrQuit | KeyIntent::Noop)
}
