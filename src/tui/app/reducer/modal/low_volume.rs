use crate::tui::event::{
    BusyCommandModalAction, McpServersAction, ModalAction, ModalKind, ModePickerAction,
    SandboxAction, ThemePickerAction,
};

use super::super::super::{AppState, move_index};
use super::{ActionResult, move_mode_cursor};

pub(super) fn handle(app: &mut AppState, action: ModalAction) -> ActionResult {
    match action {
        ModalAction::McpServers(McpServersAction::Move(delta)) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::McpServers {
                rows,
                cursor,
            })) = &mut app.modal
            {
                let max = rows.len().saturating_sub(1);
                let next = move_index(*cursor, delta, max);
                if next != *cursor {
                    *cursor = next;
                    app.modal_scroll = 0;
                }
            }
        }
        ModalAction::McpServers(
            McpServersAction::Toggle | McpServersAction::Reload | McpServersAction::Authorize,
        ) => {
            // Runtime effect: handled by `tui::runtime_actions`.
        }
        ModalAction::Sandbox(SandboxAction::Move(delta)) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::SandboxStatus {
                cursor,
            })) = &mut app.modal
            {
                *cursor = move_index(*cursor, delta, 1);
            }
        }
        ModalAction::Sandbox(SandboxAction::Toggle) => {
            // Runtime effect: handled by `tui::runtime_actions`.
        }
        ModalAction::ThemePicker(ThemePickerAction::Move(delta)) => {
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::ThemePicker {
                cursor,
                ..
            })) = &mut app.modal
            {
                let max = crate::tui::theme::theme_count().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        ModalAction::ThemePicker(ThemePickerAction::Submit | ThemePickerAction::Cancel) => {
            // Runtime effect: preview persistence / cancel restore is handled by
            // `tui::runtime_actions`.
        }
        ModalAction::BusyCommand(BusyCommandModalAction::Move(delta)) => {
            if let Some(ModalKind::Detail(crate::tui::event::DetailModal::BusyCommand {
                rows,
                cursor,
                ..
            })) = &mut app.modal
            {
                let max = rows.len().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        ModalAction::BusyCommand(BusyCommandModalAction::Submit) => {
            // Runtime effect: `tui::runtime_actions` queues, opens, cancels, or dismisses.
        }
        ModalAction::ModePicker(ModePickerAction::Move(delta)) => {
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::ModePicker {
                rows,
                cursor,
            })) = &mut app.modal
            {
                *cursor = move_mode_cursor(rows, *cursor, delta);
            }
        }
        ModalAction::ModePicker(ModePickerAction::Cycle(_)) => {
            // Runtime effect: handled by `tui::runtime_actions` so it can apply
            // the resolved mode selection to its owning state.
        }
    }
    ActionResult::Handled
}
