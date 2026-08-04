use crate::tui::event::{AppAction, Focus};

use super::super::{
    AppState, CompletionState, CopyNoticeKind, DeferredCommand, DeferredCommandPayload,
    FollowUpDelivery, MouseArea, TranscriptItem, next_mouse_click,
};
use super::ActionResult;

/// A paste longer than this many lines collapses into a `[Text N]` chip.
const PASTE_CHIP_MIN_LINES: usize = 3;
/// A paste longer than this many characters collapses into a `[Text N]` chip.
const PASTE_CHIP_MIN_CHARS: usize = 200;

/// Whether a text paste is big enough to collapse into a chip rather than
/// inserting inline. Matches the Claude Code / Codex feel: a few lines or a
/// couple hundred characters.
fn should_chip_paste(text: &str) -> bool {
    text.lines().count() > PASTE_CHIP_MIN_LINES || text.chars().count() > PASTE_CHIP_MIN_CHARS
}

pub(super) fn handle(app: &mut AppState, action: AppAction) -> ActionResult {
    match action {
        AppAction::InputChar(ch) => {
            if app.focus == Focus::Input {
                app.composer.reset_navigation();
                app.composer.insert_char(ch);
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::InsertNewline => {
            if app.focus == Focus::Input {
                app.composer.reset_navigation();
                app.composer.insert_newline();
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::PasteText(text) => {
            if app.focus == Focus::Input {
                // Refuse to absorb a pasted live credential into the composer.
                // The /authorize key-entry modal uses a separate paste path, so
                // legitimately pasting an API key there is unaffected. Scans the
                // FULL payload before the chip-or-inline decision below.
                if let Some(kind) = crate::redact::first_secret(&text) {
                    app.set_copy_notice(
                        format!(
                            "Refused paste: looks like a live {}. Use an env var or file reference instead.",
                            kind.label()
                        ),
                        CopyNoticeKind::Error,
                    );
                } else {
                    app.composer.reset_navigation();
                    // A long paste collapses into a `[Text N]` chip so it can't
                    // flood the composer; short pastes insert inline as before.
                    if should_chip_paste(&text) {
                        if !app.composer.insert_text_chip(&text) {
                            app.set_copy_notice(
                                "Paste is too large to attach.".to_string(),
                                CopyNoticeKind::Error,
                            );
                        }
                    } else {
                        app.composer.insert_text(&text);
                    }
                    app.composer_follow = true;
                    app.recompute_completion();
                }
            }
        }
        AppAction::PasteImage {
            base64,
            byte_len,
            width,
            height,
        } => {
            if app.focus == Focus::Input {
                app.composer.reset_navigation();
                app.composer
                    .insert_image_chip(crate::tui::image_paste::EncodedImage {
                        base64,
                        byte_len,
                        width,
                        height,
                    });
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::PasteFromClipboard => {
            // Handled in the event loop, which has clipboard + catalog access.
            return ActionResult::unhandled(AppAction::PasteFromClipboard);
        }
        AppAction::Backspace => {
            if app.focus == Focus::Input {
                app.composer.reset_navigation();
                app.composer.backspace();
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::DeleteForward => {
            if app.focus == Focus::Input {
                app.composer.reset_navigation();
                app.composer.delete_forward();
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::ComposerUndo => {
            if app.focus == Focus::Input {
                app.composer.undo();
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::ComposerRedo => {
            if app.focus == Focus::Input {
                app.composer.redo();
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::CursorLeft => {
            if app.focus == Focus::Input {
                app.composer.move_left();
                app.recompute_completion();
            }
        }
        AppAction::CursorRight => {
            if app.focus == Focus::Input {
                app.composer.move_right();
                app.recompute_completion();
            }
        }
        AppAction::CursorUp => {
            if app.focus == Focus::Input {
                app.composer.move_up();
                app.recompute_completion();
            }
        }
        AppAction::CursorDown => {
            if app.focus == Focus::Input {
                app.composer.move_down();
                app.recompute_completion();
            }
        }
        AppAction::CursorStart => {
            if app.focus == Focus::Input {
                app.composer.move_to_start();
                app.recompute_completion();
            }
        }
        AppAction::CursorEnd => {
            if app.focus == Focus::Input {
                app.composer.move_to_end();
                app.recompute_completion();
            }
        }
        AppAction::CursorWordLeft => {
            if app.focus == Focus::Input {
                app.composer.move_word_left();
                app.recompute_completion();
            }
        }
        AppAction::CursorWordRight => {
            if app.focus == Focus::Input {
                app.composer.move_word_right();
                app.recompute_completion();
            }
        }
        AppAction::DeleteWordBack => {
            if app.focus == Focus::Input {
                app.composer.reset_navigation();
                app.composer.delete_word_back();
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::DeleteToLineStart => {
            if app.focus == Focus::Input {
                app.composer.reset_navigation();
                app.composer.delete_to_line_start();
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::DeleteToLineEnd => {
            if app.focus == Focus::Input {
                app.composer.reset_navigation();
                app.composer.delete_to_line_end();
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::CompletionMove(delta) => {
            let len = app.completion.candidates.len();
            if app.completion.active && len > 0 {
                let len = len as i32;
                let current = app.completion.cursor as i32;
                let delta = delta as i32;
                let next = if delta.abs() <= 1 {
                    (current + delta).rem_euclid(len)
                } else {
                    (current + delta).clamp(0, len - 1)
                };
                app.completion.cursor = next as usize;
            }
        }
        AppAction::CompletionDismiss => {
            app.completion = CompletionState::default();
        }
        AppAction::ExtendCursorLeft => {
            if app.focus == Focus::Input {
                app.composer.extend_left();
                app.auto_copy_text_selection();
            }
        }
        AppAction::ExtendCursorRight => {
            if app.focus == Focus::Input {
                app.composer.extend_right();
                app.auto_copy_text_selection();
            }
        }
        AppAction::ExtendCursorUp => {
            if app.focus == Focus::Input {
                app.composer.extend_up();
                app.auto_copy_text_selection();
            }
        }
        AppAction::ExtendCursorDown => {
            if app.focus == Focus::Input {
                app.composer.extend_down();
                app.auto_copy_text_selection();
            }
        }
        AppAction::ExtendCursorStart => {
            if app.focus == Focus::Input {
                app.composer.extend_to_start();
                app.auto_copy_text_selection();
            }
        }
        AppAction::ExtendCursorEnd => {
            if app.focus == Focus::Input {
                app.composer.extend_to_end();
                app.auto_copy_text_selection();
            }
        }
        AppAction::ComposerClick {
            char_index,
            kind,
            column,
            row,
        } => {
            app.transcript_selection = None;
            app.transcript_focus = None;
            app.scroll_transcript_focus_into_view = false;
            app.active_group_tool_selection = None;
            app.focus = Focus::Input;
            app.apply_composer_click(char_index, kind);
            // Copy happens on release (PointerSelectionEnd), not per drag tick.
            app.pointer_selecting = true;
            app.last_mouse_click = Some(next_mouse_click(
                app.last_mouse_click,
                MouseArea::Composer,
                column,
                row,
            ));
        }
        AppAction::ComposerDrag { char_index } => {
            if app.focus == Focus::Input {
                app.composer.extend_selection_to(char_index);
                app.pointer_selecting = true;
            }
        }
        AppAction::ComposerSelectAll => {
            if app.focus == Focus::Input {
                app.composer.select_all();
                app.auto_copy_text_selection();
            }
        }
        AppAction::ClearInput => {
            app.composer.clear_content();
            app.composer.reset_navigation();
            app.reset_composer_scroll();
            app.recompute_completion();
        }
        AppAction::SubmitInput(text) => {
            // The user has engaged with the new session, so retire the startup
            // interrupted-session hint (an ephemeral banner, never persisted).
            app.session_hint = None;
            app.finish_input_submission(text.clone());
            app.push_transcript_item(TranscriptItem::UserMessage { text });
        }
        AppAction::SubmitCommandInput(text) => app.finish_input_submission(text),
        AppAction::SteerInput {
            id,
            text,
            content,
            mode,
        } => app.enqueue_follow_up(id, text, content, mode, FollowUpDelivery::Steer),
        AppAction::QueueNextInput {
            id,
            text,
            content,
            mode,
        } => app.enqueue_follow_up(id, text, content, mode, FollowUpDelivery::Queue),
        AppAction::QueueDeferredCommand { input, label } => {
            let id = app.next_deferred_command_id();
            app.deferred_commands.push(DeferredCommand {
                id,
                input,
                label: label.clone(),
                payload: DeferredCommandPayload::SlashCommand,
            });
            app.push_transcript_item(TranscriptItem::CommandOutput {
                kind: crate::tui::event::CommandOutputKind::Status,
                text: format!("Queued {label} for after the current run."),
            });
            app.maybe_scroll_to_bottom_current();
        }
        AppAction::QueueDeferredModelSelection {
            input,
            label,
            selection,
        } => {
            let id = app.next_deferred_command_id();
            app.deferred_commands.push(DeferredCommand {
                id,
                input,
                label: label.clone(),
                payload: DeferredCommandPayload::ModelSelection(selection),
            });
            app.push_transcript_item(TranscriptItem::CommandOutput {
                kind: crate::tui::event::CommandOutputKind::Status,
                text: format!("Queued {label} for after the current run."),
            });
            app.maybe_scroll_to_bottom_current();
        }
        AppAction::RemoveDeferredCommand { id } => {
            app.deferred_commands.retain(|command| command.id != id);
        }
        AppAction::CancelQueuedInput { id } => {
            app.remove_queued_input(id);
        }
        AppAction::CancelFocusedQueuedInput => {
            if let Some(id) = app.focused_queued_input_id() {
                app.remove_queued_input(id);
            }
        }
        AppAction::WithdrawQueuedInput => {
            if app.focus == Focus::Input
                && app.input().is_empty()
                && let Some(queued) = app.queued_inputs.last().cloned()
            {
                app.remove_queued_input(queued.id);
                // Restore the exact draft, chips included, not just the display
                // text — a withdrawn message must be editable as it was.
                app.composer.restore_content(queued.content);
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::DropQueuedInput => {
            if !app.queued_inputs.is_empty() {
                app.queued_inputs.clear();
                app.transcript
                    .retain(|item| !matches!(item, TranscriptItem::QueuedUserMessage { .. }));
                if let Some(index) = app.transcript_focus {
                    app.transcript_focus = if app.transcript.is_empty() {
                        None
                    } else {
                        Some(index.min(app.transcript.len().saturating_sub(1)))
                    };
                }
                app.maybe_scroll_to_bottom_current();
            }
        }
        AppAction::HistoryPrev => {
            if app.focus == Focus::Input {
                app.composer.history_prev();
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::HistoryNext => {
            if app.focus == Focus::Input {
                app.composer.history_next();
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        AppAction::CompleteInputTo(replacement) => {
            if app.focus == Focus::Input {
                if replacement != app.composer.text {
                    app.composer.reset_navigation();
                    app.composer.replace_text_keep_chips(replacement);
                    app.composer_follow = true;
                }
                if app
                    .composer
                    .text
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace)
                {
                    app.recompute_completion();
                } else {
                    app.completion = CompletionState::default();
                }
            }
        }
        AppAction::CompleteInputRange {
            start,
            end,
            replacement,
        } => {
            if app.focus == Focus::Input {
                app.composer.reset_navigation();
                app.composer.replace_range(start, end, &replacement);
                app.composer_follow = true;
                app.recompute_completion();
            }
        }
        action => return ActionResult::unhandled(action),
    }
    ActionResult::Handled
}
