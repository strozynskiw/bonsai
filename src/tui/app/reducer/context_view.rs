use crate::tui::event::{AppAction, Focus, ModalKind};

use super::super::AppState;
use super::ActionResult;

pub(super) fn handle(app: &mut AppState, action: AppAction) -> ActionResult {
    match action {
        AppAction::OpenContextModal => {
            if let Some(report) = app.latest_context_report.clone() {
                app.modal_scroll = 0;
                app.init_context_view_state(&report);
                app.pending_question_visibility = false;
                app.modal_return_focus = Some(app.focus);
                app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
                    Box::new(report),
                )));
                app.focus = Focus::Modal;
            }
        }
        AppAction::OpenEpisodesModal => {
            if let Some(report) = app.latest_context_report.clone() {
                app.modal_scroll = 0;
                app.pending_question_visibility = false;
                app.modal_return_focus = Some(app.focus);
                app.modal = Some(ModalKind::Detail(
                    crate::tui::event::DetailModal::Episodes {
                        report: Box::new(report),
                        cursor: 0,
                    },
                ));
                app.focus = Focus::Modal;
            }
        }
        AppAction::OpenContextPreviewModal(report) => {
            app.modal_scroll = 0;
            app.init_context_view_state(&report);
            app.pending_question_visibility = false;
            app.modal_return_focus = Some(app.focus);
            app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
                Box::new(report),
            )));
            app.focus = Focus::Modal;
        }
        AppAction::RefreshContextModal(report) => {
            app.latest_context_report = Some(report.clone());
            if matches!(
                app.modal,
                Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
                    _
                )))
            ) {
                app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
                    Box::new(report),
                )));
                app.clamp_context_cursor();
                app.clamp_context_wire_cursor();
                app.clamp_context_turns_cursor();
            }
        }
        AppAction::ContextMove(delta) => {
            app.context_state.manual_scroll = false;
            if app.context_state.view_mode.is_wire() {
                app.move_context_wire_cursor(delta);
            } else if app.context_state.view_mode.is_turns() {
                app.move_context_turns_cursor(delta);
            } else {
                app.move_context_cursor(delta);
            }
        }
        AppAction::ContextToggleSelected => {
            if app.context_state.view_mode.is_wire() {
                if let Some(id) = app.selected_context_wire_row_id() {
                    app.context_state.manual_scroll = false;
                    if !app.context_state.wire_expanded.remove(&id) {
                        app.context_state.wire_expanded.insert(id);
                        app.context_state.reveal_expanded = true;
                    }
                    app.clamp_context_wire_cursor();
                }
            } else if app.context_state.view_mode.is_turns() {
                if let Some(seq) = app.selected_context_turn_seq() {
                    app.context_state.manual_scroll = false;
                    if !app.context_state.turns_expanded.remove(&seq) {
                        app.context_state.turns_expanded.insert(seq);
                        app.context_state.reveal_expanded = true;
                    }
                }
            } else if let Some(id) = app.selected_context_node_id() {
                app.context_state.manual_scroll = false;
                if !app.context_state.expanded.remove(&id) {
                    app.context_state.expanded.insert(id);
                    app.context_state.reveal_expanded = true;
                }
                app.clamp_context_cursor();
            }
        }
        AppAction::ContextClick { row_index } => {
            if app.context_state.view_mode.is_wire() {
                let max = app.visible_context_wire_row_ids().len().saturating_sub(1);
                app.context_state.wire_cursor = row_index.min(max);
                app.context_state.manual_scroll = false;
                if let Some(id) = app.selected_context_wire_row_id() {
                    if !app.context_state.wire_expanded.remove(&id) {
                        app.context_state.wire_expanded.insert(id);
                        app.context_state.reveal_expanded = true;
                    }
                    app.clamp_context_wire_cursor();
                }
                return ActionResult::Handled;
            }
            if app.context_state.view_mode.is_turns() {
                let max = app.visible_context_turn_seqs().len().saturating_sub(1);
                app.context_state.turns_cursor = row_index.min(max);
                app.context_state.manual_scroll = false;
                if let Some(seq) = app.selected_context_turn_seq()
                    && !app.context_state.turns_expanded.remove(&seq)
                {
                    app.context_state.turns_expanded.insert(seq);
                    app.context_state.reveal_expanded = true;
                }
                return ActionResult::Handled;
            }
            let max = app.visible_context_node_ids().len().saturating_sub(1);
            app.context_state.cursor = row_index.min(max);
            app.context_state.manual_scroll = false;
            if let Some(id) = app.selected_context_node_id() {
                if !app.context_state.expanded.remove(&id) {
                    app.context_state.expanded.insert(id);
                    app.context_state.reveal_expanded = true;
                }
                app.clamp_context_cursor();
            }
        }
        AppAction::ContextExpandSelected => {
            if app.context_state.view_mode.is_wire() {
                if let Some(id) = app.selected_context_wire_row_id() {
                    app.context_state.manual_scroll = false;
                    app.context_state.wire_expanded.insert(id);
                    app.context_state.reveal_expanded = true;
                }
            } else if app.context_state.view_mode.is_turns() {
                if let Some(seq) = app.selected_context_turn_seq() {
                    app.context_state.manual_scroll = false;
                    app.context_state.turns_expanded.insert(seq);
                    app.context_state.reveal_expanded = true;
                }
            } else if let Some(id) = app.selected_context_node_id() {
                app.context_state.manual_scroll = false;
                app.context_state.expanded.insert(id);
                app.context_state.reveal_expanded = true;
            }
        }
        AppAction::ContextCollapseSelected => {
            if app.context_state.view_mode.is_wire() {
                if let Some(id) = app.selected_context_wire_row_id() {
                    app.context_state.manual_scroll = false;
                    app.context_state.wire_expanded.remove(&id);
                    app.clamp_context_wire_cursor();
                }
            } else if app.context_state.view_mode.is_turns() {
                if let Some(seq) = app.selected_context_turn_seq() {
                    app.context_state.manual_scroll = false;
                    app.context_state.turns_expanded.remove(&seq);
                }
            } else if let Some(id) = app.selected_context_node_id() {
                app.context_state.manual_scroll = false;
                app.context_state.expanded.remove(&id);
                app.clamp_context_cursor();
            }
        }
        AppAction::ContextToggleView => {
            if matches!(
                app.modal,
                Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
                    _
                )))
            ) {
                app.context_state.view_mode = app.context_state.view_mode.cycled();
                app.modal_scroll = 0;
                app.context_state.manual_scroll = false;
            }
        }
        AppAction::ContextPinSelected
        | AppAction::ContextDropSelected
        | AppAction::ContextStubSelected
        | AppAction::ContextRestoreSelected => {}
        action => return ActionResult::unhandled(action),
    }
    ActionResult::Handled
}
