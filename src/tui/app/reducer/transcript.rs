use crate::tui::event::{AppAction, Focus, ModalKind};

use super::super::{
    AppState, ModalSelection, MouseArea, PlanPosition, PlanSelection, SelectionKind,
    TranscriptItem, next_mouse_click,
};
use super::ActionResult;

pub(super) fn handle(app: &mut AppState, action: AppAction) -> ActionResult {
    match action {
        AppAction::TranscriptClick {
            position,
            kind,
            extend,
            column,
            row,
        } => {
            if app.is_selectable_item(position.item) {
                app.focus = Focus::Transcript;
                app.active_group_tool_selection = None;
                app.transcript_focus = Some(position.item);
                app.transcript_autoscroll = false;
                app.scroll_transcript_focus_into_view = false;
                match kind {
                    SelectionKind::Word => app.apply_transcript_word_select(position),
                    SelectionKind::Line => app.apply_transcript_line_select(position),
                    _ => app.apply_transcript_click(position, extend),
                }
                // A pointer gesture has begun; the selection is copied once the
                // button is released (PointerSelectionEnd), not on every tick.
                app.pointer_selecting = true;
                app.last_mouse_click = Some(next_mouse_click(
                    app.last_mouse_click,
                    MouseArea::Transcript,
                    column,
                    row,
                ));
            }
        }
        AppAction::TranscriptDrag {
            position,
            scroll_delta,
        } => {
            if app.is_selectable_item(position.item) {
                app.focus = Focus::Transcript;
                app.active_group_tool_selection = None;
                app.transcript_focus = Some(position.item);
                app.transcript_autoscroll = false;
                app.scroll_transcript_focus_into_view = false;
                if scroll_delta != 0 {
                    app.transcript_scroll = crate::tui::app::reducer::scroll::clamped_scroll(
                        app.transcript_scroll,
                        scroll_delta,
                    );
                }
                app.apply_transcript_drag(position);
                app.pointer_selecting = true;
            }
        }
        AppAction::PlanClick {
            position,
            kind,
            column,
            row,
        } => {
            app.focus = Focus::Plan;
            app.transcript_selection = None;
            app.active_group_tool_selection = None;
            app.transcript_focus = None;
            match kind {
                SelectionKind::Word => {
                    app.plan_selection = crate::tui::widgets::plan::word_selection(app, position)
                }
                SelectionKind::Line => {
                    app.plan_selection = Some(PlanSelection {
                        anchor: PlanPosition {
                            line: position.line,
                            grapheme: 0,
                            width: position.width,
                        },
                        caret: crate::tui::widgets::plan::line_end_position(app, position),
                    });
                }
                SelectionKind::Position => {
                    app.plan_selection = Some(PlanSelection {
                        anchor: position,
                        caret: position,
                    });
                }
            }
            app.pointer_selecting = true;
            app.last_mouse_click = Some(next_mouse_click(
                app.last_mouse_click,
                MouseArea::Plan,
                column,
                row,
            ));
        }
        AppAction::PlanDrag {
            position,
            scroll_delta,
        } => {
            app.focus = Focus::Plan;
            app.transcript_selection = None;
            app.active_group_tool_selection = None;
            if scroll_delta != 0 {
                app.plan_scroll =
                    crate::tui::app::reducer::scroll::clamped_scroll(app.plan_scroll, scroll_delta);
            }
            let anchor = app
                .plan_selection
                .map(|selection| selection.anchor)
                .unwrap_or(position);
            app.plan_selection = Some(PlanSelection {
                anchor,
                caret: position,
            });
            app.pointer_selecting = true;
        }
        AppAction::ModalClick {
            offset,
            kind,
            column,
            row,
        } => {
            // Clear non-modal selections.
            app.transcript_selection = None;
            app.plan_selection = None;
            app.active_group_tool_selection = None;
            let body_lines = app.modal_body_lines.borrow();
            let plain = crate::tui::widgets::modal::modal_body_plain_text(&body_lines);
            match kind {
                SelectionKind::Word => {
                    let (start, end) = crate::tui::text_bounds::word_bounds_at(&plain, offset);
                    if start != end {
                        app.modal_selection = Some(ModalSelection {
                            anchor: start,
                            caret: end,
                        });
                    }
                }
                SelectionKind::Line => {
                    let (start, end) = crate::tui::text_bounds::line_bounds_at(&plain, offset);
                    if start != end {
                        app.modal_selection = Some(ModalSelection {
                            anchor: start,
                            caret: end,
                        });
                    }
                }
                SelectionKind::Position => {
                    app.modal_selection = Some(ModalSelection {
                        anchor: offset,
                        caret: offset,
                    });
                    app.pointer_selecting = true;
                }
            }
            app.last_mouse_click = Some(next_mouse_click(
                app.last_mouse_click,
                MouseArea::Modal,
                column,
                row,
            ));
            drop(body_lines);
            // Word and line selections copy immediately.
            if !app.pointer_selecting {
                app.copy_modal_selection();
            }
        }
        AppAction::ModalDrag { offset } => {
            if let Some(sel) = app.modal_selection {
                app.modal_selection = Some(ModalSelection {
                    anchor: sel.anchor,
                    caret: offset,
                });
                app.pointer_selecting = true;
            }
        }
        AppAction::PointerSelectionEnd => {
            // Copy the pointer-made selection exactly once, on button release,
            // so the notice reflects the final selection rather than a mid-drag
            // prefix. The flag keeps stray mouse-ups (clicks that opened a tool
            // card, scrollbar drags, …) from re-copying a stale selection.
            if app.pointer_selecting {
                app.pointer_selecting = false;
                if app.modal_selection.is_some() {
                    app.copy_modal_selection();
                } else {
                    app.auto_copy_text_selection();
                }
            }
        }
        AppAction::ExtendTranscriptCursor(delta) => {
            if matches!(app.focus, Focus::Transcript) {
                app.extend_transcript_cursor(delta);
                app.auto_copy_text_selection();
            }
        }
        AppAction::TranscriptSelectAll => {
            if matches!(app.focus, Focus::Transcript) {
                app.select_all_transcript();
                app.auto_copy_text_selection();
            }
        }
        AppAction::ClearSelection => {
            app.modal_selection = None;
            if matches!(app.focus, Focus::Input) {
                app.composer.clear_selection();
            } else if matches!(app.focus, Focus::Plan) {
                app.plan_selection = None;
            } else {
                app.transcript_selection = None;
                if matches!(app.focus, Focus::Transcript) {
                    app.active_group_tool_selection = None;
                    app.transcript_focus = None;
                    app.scroll_transcript_focus_into_view = false;
                }
            }
        }
        AppAction::MoveTranscriptFocus(delta) => {
            if matches!(app.focus, Focus::Transcript) {
                app.active_group_tool_selection = None;
                app.move_transcript_focus(delta);
            }
        }
        AppAction::FocusTranscriptFirst => {
            if matches!(app.focus, Focus::Transcript) && !app.transcript.is_empty() {
                app.active_group_tool_selection = None;
                app.set_transcript_focus(0);
            }
        }
        AppAction::FocusTranscriptLast => {
            if matches!(app.focus, Focus::Transcript)
                && let Some(index) = app.transcript.len().checked_sub(1)
            {
                app.active_group_tool_selection = None;
                app.set_transcript_focus(index);
            }
        }
        AppAction::OpenFocusedDetail => {
            if matches!(app.focus, Focus::Transcript)
                && let Some(index) = app
                    .transcript_focus
                    .filter(|index| *index < app.transcript.len())
            {
                let group_id = match &app.transcript[index] {
                    TranscriptItem::ExecutionGroup(group) => Some(group.id),
                    _ => None,
                };
                let tool_id = match &app.transcript[index] {
                    TranscriptItem::ToolActivity(activity) => Some(activity.id.clone()),
                    _ => None,
                };
                if let Some(group_id) = group_id {
                    if app.serenity_mode {
                        app.toggle_execution_group_expansion(group_id);
                    } else {
                        app.set_execution_group_tool_selection(group_id, 0);
                    }
                } else {
                    app.modal_scroll = 0;
                    app.modal_return_focus = Some(Focus::Transcript);
                    app.modal = if let Some(tool_id) = tool_id {
                        Some(ModalKind::ToolDetail { tool_id })
                    } else {
                        Some(ModalKind::BlockDetail { item_index: index })
                    };
                    app.focus = Focus::Modal;
                }
            }
        }
        AppAction::ClearTranscriptFocus => {
            if matches!(app.focus, Focus::Transcript) {
                app.active_group_tool_selection = None;
                app.transcript_focus = None;
                app.scroll_transcript_focus_into_view = false;
            }
        }
        AppAction::OpenPlanFindings => {
            if !app.plan.findings.is_empty() {
                app.modal_scroll = 0;
                app.modal_return_focus = Some(Focus::Plan);
                app.modal = Some(ModalKind::PlanFindingDetail { index: 0 });
                app.focus = Focus::Modal;
            }
        }
        AppAction::CyclePlanFinding(delta) => {
            if let Some(ModalKind::PlanFindingDetail { index }) = app.modal {
                let count = app.plan.findings.len();
                if count > 0 {
                    let next = (index as i64 + delta as i64).clamp(0, count as i64 - 1) as usize;
                    if next != index {
                        app.modal_scroll = 0;
                        app.modal = Some(ModalKind::PlanFindingDetail { index: next });
                    }
                }
            }
        }
        AppAction::OpenFindingEvidence => {
            if let Some(ModalKind::PlanFindingDetail { index }) = app.modal {
                // Jump to the first evidence id that still resolves to a tool
                // card in the live transcript (same-session best effort).
                let tool_id =
                    app.plan
                        .findings_in_severity_order()
                        .get(index)
                        .and_then(|finding| {
                            finding
                                .source_ids
                                .iter()
                                .find(|id| app.tool_activity(id).is_some())
                                .cloned()
                        });
                if let Some(tool_id) = tool_id {
                    app.modal_scroll = 0;
                    app.modal_return_focus = Some(Focus::Plan);
                    app.modal = Some(ModalKind::ToolDetail { tool_id });
                }
            }
        }
        AppAction::OpenToolDetail(tool_id) => {
            let focused_tool = app.focus_tool_for_activation(&tool_id);
            app.modal_scroll = 0;
            app.modal_return_focus = if focused_tool || app.inline_selection_matches_tool(&tool_id)
            {
                Some(Focus::Transcript)
            } else {
                None
            };
            app.modal = Some(ModalKind::ToolDetail { tool_id });
            app.focus = Focus::Modal;
        }
        AppAction::OpenDiffPreview(tool_id) => {
            if app
                .tool_activity(&tool_id)
                .and_then(|activity| activity.diff.clone())
                .is_some()
            {
                app.modal_scroll = 0;
                app.modal_return_focus = if app.inline_selection_matches_tool(&tool_id) {
                    Some(Focus::Transcript)
                } else {
                    None
                };
                app.modal = Some(ModalKind::DiffPreview { tool_id });
                app.focus = Focus::Modal;
            }
        }
        action => return ActionResult::unhandled(action),
    }
    ActionResult::Handled
}
