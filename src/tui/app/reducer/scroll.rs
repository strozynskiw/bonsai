use crate::tui::event::{AppAction, Focus, ModalKind};

use super::super::{AppState, move_index};
use super::ActionResult;

pub(super) fn handle(app: &mut AppState, action: AppAction) -> ActionResult {
    match action {
        AppAction::ComposerPage(delta) => {
            if app.focus == Focus::Input {
                app.composer_follow = false;
                app.pending_composer_page = Some(delta);
            }
        }
        AppAction::SetComposerScroll(scroll) => {
            if app.focus == Focus::Input {
                app.composer_scroll = scroll;
                app.composer_follow = false;
            }
        }
        AppAction::ExtendComposerByPage(delta) => {
            if app.focus == Focus::Input {
                app.composer_follow = false;
                app.pending_composer_page = Some(delta);
                app.pending_composer_extend = Some(delta);
            }
        }
        AppAction::ScrollCurrent(delta) => {
            app.transcript_autoscroll = false;
            app.transcript_scroll = clamped_scroll(app.transcript_scroll, delta);
        }
        AppAction::SetCurrentScroll(scroll) => {
            app.transcript_scroll = scroll;
            app.transcript_autoscroll = false;
        }
        AppAction::ScrollTop => {
            app.transcript_scroll = 0;
            app.transcript_autoscroll = false;
        }
        AppAction::ScrollBottom => {
            app.transcript_autoscroll = true;
        }
        AppAction::ScrollSidebar(delta) => {
            app.sidebar_scroll = clamped_scroll(app.sidebar_scroll, delta);
        }
        AppAction::SetSidebarScroll(scroll) => {
            app.sidebar_scroll = scroll;
        }
        AppAction::MoveTodoPhase(delta) => {
            let total = app.plan.phases.len();
            if total > 1 {
                let current = app.resolved_todo_phase().unwrap_or(0);
                app.todo_phase_view = Some(move_index(current, delta, total - 1));
                // Land on the newly viewed phase's in-progress item (resolved in
                // `clamp_scrolls` once the sidebar size is known).
                app.scroll_todo_in_progress_into_view = true;
            }
        }
        AppAction::ScrollPlan(delta) => {
            app.plan_scroll = clamped_scroll(app.plan_scroll, delta);
        }
        AppAction::SetPlanScroll(scroll) => {
            app.plan_scroll = scroll;
        }
        AppAction::ScrollModal(delta) => {
            app.modal_scroll = clamped_scroll(app.modal_scroll, delta);
            if delta != 0 {
                app.modal_selection = None;
                if matches!(app.modal, Some(ModalKind::Context(_))) {
                    app.context_state.manual_scroll = true;
                }
            }
        }
        action => return ActionResult::unhandled(action),
    }
    ActionResult::Handled
}

pub(crate) fn clamped_scroll(current: u16, delta: i16) -> u16 {
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as u16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PlanDoc, PlanPhase};

    fn app_with_phases(count: usize) -> AppState {
        let mut app = AppState::new("codex", "model".to_string(), "ws".to_string(), None);
        app.plan = PlanDoc {
            phases: (0..count)
                .map(|i| PlanPhase {
                    name: format!("p{i}"),
                    tasks: Vec::new(),
                })
                .collect(),
            ..Default::default()
        };
        app
    }

    #[test]
    fn move_todo_phase_parks_clamps_and_requests_scroll() {
        let mut app = app_with_phases(3);
        app.focus = Focus::Todo;

        app.reduce(AppAction::MoveTodoPhase(1));
        assert_eq!(app.todo_phase_view, Some(1));
        assert!(app.scroll_todo_in_progress_into_view);

        // Jump to last, then no wrap past the end.
        app.reduce(AppAction::MoveTodoPhase(i16::MAX));
        assert_eq!(app.todo_phase_view, Some(2));
        app.reduce(AppAction::MoveTodoPhase(1));
        assert_eq!(app.todo_phase_view, Some(2));

        // Jump to first, then no wrap before the start.
        app.reduce(AppAction::MoveTodoPhase(i16::MIN));
        assert_eq!(app.todo_phase_view, Some(0));
        app.reduce(AppAction::MoveTodoPhase(-1));
        assert_eq!(app.todo_phase_view, Some(0));
    }

    #[test]
    fn move_todo_phase_inert_for_flat_plan() {
        let mut app = AppState::new("codex", "model".to_string(), "ws".to_string(), None);
        app.reduce(AppAction::MoveTodoPhase(1));
        assert_eq!(app.todo_phase_view, None);
        assert!(!app.scroll_todo_in_progress_into_view);
    }
}
