use crate::tui::event::{AppAction, Focus, ModalKind};

use super::super::AppState;
use super::ActionResult;

pub(super) fn handle(app: &mut AppState, action: AppAction) -> ActionResult {
    match action {
        AppAction::OpenExecutionGroup { group_id } => {
            if let Some(index) = app.execution_group_index(group_id) {
                app.active_group_tool_selection = None;
                app.focus = Focus::Transcript;
                if app
                    .execution_group(group_id)
                    .is_some_and(|group| !group.tools.is_empty())
                {
                    app.set_execution_group_tool_selection(group_id, 0);
                } else {
                    app.set_transcript_focus(index);
                }
            }
        }
        AppAction::ToggleExecutionGroup { group_id } => {
            app.toggle_execution_group_expansion(group_id);
        }
        AppAction::ExecutionGroupMoveSelection { delta } => {
            let Some(selection) = app.active_group_tool_selection else {
                return ActionResult::Handled;
            };
            if let Some(next_selected) = app.move_execution_group_selection(
                selection.group_id,
                selection.selected_tool,
                delta,
            ) {
                app.set_execution_group_tool_selection(selection.group_id, next_selected);
            }
        }
        AppAction::ClearExecutionGroupSelection => {
            app.active_group_tool_selection = None;
        }
        AppAction::OpenSelectedExecutionGroupTool => {
            if let Some(tool_id) = app
                .selected_execution_group_tool()
                .map(|activity| activity.id.clone())
            {
                app.modal_scroll = 0;
                app.modal_return_focus = Some(Focus::Transcript);
                app.modal = Some(ModalKind::ToolDetail { tool_id });
                app.focus = Focus::Modal;
            }
        }
        AppAction::OpenSelectedExecutionGroupDiff => {
            if let Some(tool_id) = app
                .selected_execution_group_tool()
                .filter(|activity| activity.diff.is_some())
                .map(|activity| activity.id.clone())
            {
                app.modal_scroll = 0;
                app.modal_return_focus = Some(Focus::Transcript);
                app.modal = Some(ModalKind::DiffPreview { tool_id });
                app.focus = Focus::Modal;
            }
        }
        action => return ActionResult::unhandled(action),
    }
    ActionResult::Handled
}
