use super::{AppState, TranscriptModel};

const COPY_NOTICE_TICKS: u64 = 60;
const SESSION_HINT_TICKS: u64 = 60;
const SESSION_TOAST_TICKS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyNoticeKind {
    Status,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyNotice {
    pub text: String,
    pub kind: CopyNoticeKind,
    pub expires_at_tick: u64,
}

/// Auto-expiring orientation hint for a session-lifecycle state (e.g. an
/// interrupted session that can be resumed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHint {
    pub text: String,
    pub expires_at_tick: u64,
}

/// Auto-expiring confirmation for a session-lifecycle action (e.g. a resume).
/// Mirrors [`CopyNotice`]'s tick-based dismissal but carries no kind — these are
/// always informational status confirmations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToast {
    pub text: String,
    pub expires_at_tick: u64,
}

impl AppState {
    #[cfg(test)]
    pub(crate) fn set_clipboard(&mut self, clipboard: crate::copy::Clipboard) {
        self.clipboard = clipboard;
    }

    /// Read-only handle to the system clipboard, for the paste-from-clipboard
    /// path (image first, text fallback).
    pub(crate) fn clipboard(&self) -> &crate::copy::Clipboard {
        &self.clipboard
    }

    pub fn has_copyable_selection(&self) -> bool {
        self.composer.has_selection()
            || self.has_plan_text_selection()
            || self.has_transcript_text_selection()
            || self.selected_execution_group_tool().is_some()
            || self
                .transcript_focus
                .is_some_and(|index| index < self.transcript.len())
    }

    pub fn handle_copy_command(&mut self, input: &str) -> bool {
        let parts: Vec<&str> = input.split_whitespace().collect();
        if parts.first().copied() != Some("/copy") {
            return false;
        }

        match parts.as_slice() {
            ["/copy"] => self.copy_active_selection(),
            ["/copy", "all"] => self.copy_transcript(),
            _ => self.set_copy_notice("Usage: /copy [all]", CopyNoticeKind::Error),
        }
        true
    }

    fn copy_active_selection(&mut self) {
        let Some(text) = self.active_copy_text() else {
            self.set_copy_notice("Nothing selected.", CopyNoticeKind::Error);
            return;
        };
        self.copy_text_to_clipboard(&text, "Copied selected text.");
    }

    fn active_copy_text(&self) -> Option<String> {
        self.active_text_selection()
            .or_else(|| self.focused_copy_text())
    }

    fn active_text_selection(&self) -> Option<String> {
        if let Some(text) = self.composer.selected_text()
            && !text.is_empty()
        {
            return Some(text);
        }
        self.plan_selected_text()
            .or_else(|| self.transcript_selected_text())
    }

    fn focused_copy_text(&self) -> Option<String> {
        if let Some(activity) = self.selected_execution_group_tool() {
            return Some(crate::copy::clean_tool_activity(activity));
        }
        let index = self
            .transcript_focus
            .filter(|index| *index < self.transcript.len())?;
        let text = crate::copy::clean_transcript_item(&self.transcript[index]).into_owned();
        (!text.is_empty()).then_some(text)
    }

    pub(super) fn auto_copy_text_selection(&mut self) {
        if let Some(text) = self.active_text_selection() {
            self.copy_text_to_clipboard(&text, "Copied selected text.");
        }
    }

    /// Copy the current modal text selection to the clipboard.
    pub(super) fn copy_modal_selection(&mut self) {
        let Some(sel) = self.modal_selection else {
            return;
        };
        let (start, end) = sel.range();
        if start == end {
            return;
        }
        let text = {
            let body_lines = self.modal_body_lines.borrow();
            let plain = crate::tui::widgets::modal::modal_body_plain_text(&body_lines);
            crate::tui::widgets::modal::modal_text_range(&plain, start, end)
        };
        if !text.is_empty() {
            self.copy_text_to_clipboard(&text, "Copied selected text.");
        }
    }

    fn transcript_selected_text(&self) -> Option<String> {
        self.transcript.selected_text(self.transcript_selection)
    }

    fn plan_selected_text(&self) -> Option<String> {
        crate::tui::widgets::plan::selected_text(self)
    }

    fn has_plan_text_selection(&self) -> bool {
        crate::tui::widgets::plan::has_text_selection(self)
    }

    fn has_transcript_text_selection(&self) -> bool {
        TranscriptModel::has_text_selection(self.transcript_selection)
    }

    fn copy_transcript(&mut self) {
        match self.transcript.collect_text() {
            Some(text) => self.copy_text_to_clipboard(&text, "Copied transcript."),
            None => {
                self.set_copy_notice("Copy failed: Nothing to copy yet.", CopyNoticeKind::Error)
            }
        }
    }

    fn copy_text_to_clipboard(&mut self, text: &str, success: &str) {
        match crate::copy::copy_text_to(&self.clipboard, text) {
            Ok(()) => self.set_copy_notice(success, CopyNoticeKind::Status),
            Err(err) => {
                self.set_copy_notice(format!("Copy failed: {err:#}"), CopyNoticeKind::Error)
            }
        }
    }

    pub(crate) fn set_copy_notice(&mut self, text: impl Into<String>, kind: CopyNoticeKind) {
        self.copy_notice = Some(CopyNotice {
            text: text.into(),
            kind,
            expires_at_tick: self.tick.saturating_add(COPY_NOTICE_TICKS),
        });
    }

    pub(super) fn clear_expired_copy_notice(&mut self) {
        let Some(notice) = &self.copy_notice else {
            return;
        };
        if self.tick >= notice.expires_at_tick {
            self.copy_notice = None;
        }
    }

    /// Raise an auto-dismissing session-lifecycle hint (e.g. "Interrupted
    /// session found. Use /resume…"). These are hints, not state indicators, so
    /// they should not pin the composer meta line indefinitely.
    pub(crate) fn set_session_hint(&mut self, text: impl Into<String>) {
        self.session_hint = Some(SessionHint {
            text: text.into(),
            expires_at_tick: self.tick.saturating_add(SESSION_HINT_TICKS),
        });
    }

    pub(super) fn clear_expired_session_hint(&mut self) {
        let Some(hint) = &self.session_hint else {
            return;
        };
        if self.tick >= hint.expires_at_tick {
            self.session_hint = None;
        }
    }

    /// Raise an auto-dismissing session-lifecycle toast (e.g. "Resumed session
    /// #9."). `pub(crate)` so the run-loop persistence layer can set it without
    /// pushing a persisted transcript row.
    pub(crate) fn set_session_toast(&mut self, text: impl Into<String>) {
        self.session_toast = Some(SessionToast {
            text: text.into(),
            expires_at_tick: self.tick.saturating_add(SESSION_TOAST_TICKS),
        });
    }

    pub(super) fn clear_expired_session_toast(&mut self) {
        let Some(toast) = &self.session_toast else {
            return;
        };
        if self.tick >= toast.expires_at_tick {
            self.session_toast = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        AppState, CopyNotice, CopyNoticeKind, ExecutionGroup, InlineToolSelection, PlanPosition,
        SelectionKind, ToolActivity, ToolStatus, TranscriptItem, TranscriptPosition,
    };
    use super::{COPY_NOTICE_TICKS, SESSION_HINT_TICKS, SESSION_TOAST_TICKS};
    use crate::copy::test_support::fake_clipboard;
    use crate::copy::{Clipboard, ClipboardSink};
    use crate::tui::event::{AppAction, Focus};
    use std::time::Instant;

    fn app() -> AppState {
        AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        )
    }

    fn tool_activity(id: &str, name: &str) -> ToolActivity {
        ToolActivity {
            id: id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
            delegated_model: None,
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at: Instant::now(),
            finished_at: Some(Instant::now()),
            timing: Default::default(),
        }
    }

    fn copied_text(copied: &std::sync::Arc<std::sync::Mutex<Option<String>>>) -> Option<String> {
        copied
            .lock()
            .expect("fake clipboard lock should be healthy")
            .clone()
    }

    #[derive(Debug)]
    struct FailingClipboard;

    impl ClipboardSink for FailingClipboard {
        fn set_text(&self, _text: &str) -> anyhow::Result<()> {
            anyhow::bail!("clipboard unavailable")
        }
    }

    #[test]
    fn transcript_drag_copy_uses_exact_selected_text() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "hello world".to_string(),
        });
        app.focus = Focus::Input;

        app.reduce(AppAction::TranscriptClick {
            position: TranscriptPosition {
                item: 0,
                grapheme: 0,
                width: 80,
            },
            kind: SelectionKind::Position,
            extend: false,
            column: 0,
            row: 0,
        });
        app.reduce(AppAction::TranscriptDrag {
            position: TranscriptPosition {
                item: 0,
                grapheme: 5,
                width: 80,
            },
            scroll_delta: 0,
        });

        // Nothing is copied mid-drag: the clipboard and notice stay untouched
        // until the button is released.
        assert_eq!(copied_text(&copied), None);
        assert!(app.copy_notice.is_none());

        app.reduce(AppAction::PointerSelectionEnd);

        assert_eq!(copied_text(&copied).as_deref(), Some("hello"));
        assert_eq!(app.transcript.len(), 1);
        assert!(matches!(
            app.copy_notice.as_ref(),
            Some(CopyNotice {
                text,
                kind: CopyNoticeKind::Status,
                ..
            }) if text == "Copied selected text."
        ));
    }

    #[test]
    fn transcript_double_click_copy_uses_exact_word() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "hello world".to_string(),
        });

        app.reduce(AppAction::TranscriptClick {
            position: TranscriptPosition {
                item: 0,
                grapheme: 6,
                width: 80,
            },
            kind: SelectionKind::Word,
            extend: false,
            column: 0,
            row: 0,
        });
        app.reduce(AppAction::PointerSelectionEnd);

        assert_eq!(copied_text(&copied).as_deref(), Some("world"));
    }

    #[test]
    fn plan_drag_copy_uses_exact_selected_text() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.plan.edit().add_task("copy this task");

        app.reduce(AppAction::PlanClick {
            position: PlanPosition {
                line: 1,
                grapheme: 8,
                width: 80,
            },
            kind: SelectionKind::Position,
            column: 0,
            row: 0,
        });
        app.reduce(AppAction::PlanDrag {
            position: PlanPosition {
                line: 1,
                grapheme: 12,
                width: 80,
            },
            scroll_delta: 0,
        });
        app.reduce(AppAction::PointerSelectionEnd);

        assert_eq!(copied_text(&copied).as_deref(), Some("copy"));
        assert!(matches!(
            app.copy_notice.as_ref(),
            Some(CopyNotice {
                text,
                kind: CopyNoticeKind::Status,
                ..
            }) if text == "Copied selected text."
        ));
    }

    #[test]
    fn plan_double_click_copy_uses_exact_word() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.plan.edit().add_task("copy this task");

        app.reduce(AppAction::PlanClick {
            position: PlanPosition {
                line: 1,
                grapheme: 14,
                width: 80,
            },
            kind: SelectionKind::Word,
            column: 0,
            row: 0,
        });
        app.reduce(AppAction::PointerSelectionEnd);

        assert_eq!(copied_text(&copied).as_deref(), Some("this"));
    }

    #[test]
    fn copy_selection_falls_back_to_focused_transcript_row() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "hello world".to_string(),
        });

        app.reduce(AppAction::TranscriptClick {
            position: TranscriptPosition {
                item: 0,
                grapheme: 2,
                width: 80,
            },
            kind: SelectionKind::Position,
            extend: false,
            column: 0,
            row: 0,
        });
        assert!(app.handle_copy_command("/copy"));

        assert_eq!(copied_text(&copied).as_deref(), Some("hello world"));
        assert_eq!(app.transcript.len(), 1);
        assert!(matches!(
            app.copy_notice.as_ref(),
            Some(CopyNotice {
                text,
                kind: CopyNoticeKind::Status,
                ..
            }) if text == "Copied selected text."
        ));
    }

    #[test]
    fn composer_drag_auto_copies_selected_text() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.focus = Focus::Input;
        app.composer.set_text("hello world".to_string());
        app.reduce(AppAction::ComposerClick {
            char_index: 0,
            kind: SelectionKind::Position,
            column: 0,
            row: 0,
        });
        app.reduce(AppAction::ComposerDrag { char_index: 5 });
        app.reduce(AppAction::PointerSelectionEnd);

        assert_eq!(copied_text(&copied).as_deref(), Some("hello"));
        assert!(matches!(
            app.copy_notice.as_ref(),
            Some(CopyNotice {
                text,
                kind: CopyNoticeKind::Status,
                ..
            }) if text == "Copied selected text."
        ));
    }

    #[test]
    fn transcript_plain_click_release_does_not_copy() {
        // Clicking to place the caret (a collapsed selection) and releasing
        // must not copy anything or raise the notice.
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "hello world".to_string(),
        });

        app.reduce(AppAction::TranscriptClick {
            position: TranscriptPosition {
                item: 0,
                grapheme: 3,
                width: 80,
            },
            kind: SelectionKind::Position,
            extend: false,
            column: 0,
            row: 0,
        });
        app.reduce(AppAction::PointerSelectionEnd);

        assert_eq!(copied_text(&copied), None);
        assert!(app.copy_notice.is_none());
    }

    #[test]
    fn composer_selection_copy_uses_footer_notice_without_transcript_status() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.composer.set_text("hello world".to_string());
        app.composer.set_selection(0, 5);

        assert!(app.handle_copy_command("/copy"));

        assert_eq!(copied_text(&copied).as_deref(), Some("hello"));
        assert!(app.transcript.is_empty());
        assert!(matches!(
            app.copy_notice.as_ref(),
            Some(CopyNotice {
                text,
                kind: CopyNoticeKind::Status,
                ..
            }) if text == "Copied selected text."
        ));
    }

    #[test]
    fn copy_command_copies_selection_only() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "transcript text".to_string(),
        });
        app.composer.set_text("draft text".to_string());
        app.composer.set_selection(0, 5);

        assert!(app.handle_copy_command("/copy"));

        assert_eq!(copied_text(&copied).as_deref(), Some("draft"));
        assert_eq!(app.transcript.len(), 1);
    }

    #[test]
    fn copy_command_copies_focused_transcript_row() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.transcript.push(TranscriptItem::UserMessage {
            text: "focused prompt".to_string(),
        });
        app.focus = Focus::Transcript;
        app.transcript_focus = Some(0);

        assert!(app.handle_copy_command("/copy"));

        assert_eq!(copied_text(&copied).as_deref(), Some("focused prompt"));
        assert!(matches!(
            app.copy_notice.as_ref(),
            Some(CopyNotice {
                text,
                kind: CopyNoticeKind::Status,
                ..
            }) if text == "Copied selected text."
        ));
    }

    #[test]
    fn copy_command_copies_selected_execution_group_tool() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        let mut group = ExecutionGroup::new(7, Instant::now());
        group.tools.push(tool_activity("tool-1", "bash"));
        app.transcript.push(TranscriptItem::ExecutionGroup(group));
        app.focus = Focus::Transcript;
        app.transcript_focus = Some(0);
        app.active_group_tool_selection = Some(InlineToolSelection {
            group_id: 7,
            selected_tool: 0,
        });

        assert!(app.handle_copy_command("/copy"));

        let copied = copied_text(&copied).unwrap_or_default();
        assert!(copied.contains("tool done: bash (tool-1)"));
        assert!(copied.contains("args: {}"));
    }

    #[test]
    fn copy_all_command_copies_transcript_without_command_text() {
        let mut app = app();
        let (clipboard, copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.transcript.push(TranscriptItem::UserMessage {
            text: "first".to_string(),
        });
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "reply".to_string(),
        });

        assert!(app.handle_copy_command("/copy all"));

        assert_eq!(copied_text(&copied).as_deref(), Some("first\n\nreply"));
        assert_eq!(app.transcript.len(), 2);
        assert!(matches!(
            app.copy_notice.as_ref(),
            Some(CopyNotice {
                text,
                kind: CopyNoticeKind::Status,
                ..
            }) if text == "Copied transcript."
        ));
    }

    #[test]
    fn invalid_copy_command_shows_usage_notice() {
        let mut app = app();

        assert!(app.handle_copy_command("/copy latest"));

        assert!(matches!(
            app.copy_notice.as_ref(),
            Some(CopyNotice {
                text,
                kind: CopyNoticeKind::Error,
                ..
            }) if text == "Usage: /copy [all]"
        ));
        assert!(app.transcript.is_empty());
    }

    #[test]
    fn copy_failure_shows_footer_error() {
        let mut app = app();
        app.set_clipboard(Clipboard::new(std::sync::Arc::new(FailingClipboard)));
        app.composer.set_text("hello".to_string());
        app.composer.set_selection(0, 5);

        assert!(app.handle_copy_command("/copy"));

        assert!(matches!(
            app.copy_notice.as_ref(),
            Some(CopyNotice {
                text,
                kind: CopyNoticeKind::Error,
                ..
            }) if text == "Copy failed: clipboard unavailable"
        ));
        assert!(app.transcript.is_empty());
    }

    #[test]
    fn copy_notice_expires_after_tick_window() {
        let mut app = app();
        let (clipboard, _copied) = fake_clipboard();
        app.set_clipboard(clipboard);
        app.composer.set_text("hello".to_string());
        app.composer.set_selection(0, 5);
        assert!(app.handle_copy_command("/copy"));
        assert!(app.copy_notice.is_some());

        for _ in 0..COPY_NOTICE_TICKS {
            app.reduce(AppAction::Tick);
        }

        assert!(app.copy_notice.is_none());
    }

    #[test]
    fn session_toast_is_ephemeral_and_expires_after_tick_window() {
        let mut app = app();
        app.set_session_toast("Resumed session #9.");

        // The toast is ephemeral UI: it never becomes a transcript row, so
        // resuming a session can't accumulate "Resumed session #N" lines.
        assert_eq!(
            app.session_toast.as_ref().map(|toast| toast.text.as_str()),
            Some("Resumed session #9.")
        );
        assert!(app.transcript.is_empty());

        for _ in 0..SESSION_TOAST_TICKS {
            app.reduce(AppAction::Tick);
        }

        assert!(app.session_toast.is_none());
    }

    #[test]
    fn session_hint_is_ephemeral_and_cleared_on_submit() {
        let mut app = app();
        app.set_session_hint("Interrupted session found. Use /resume to bring it back.");

        // A hint is a banner, not a transcript row.
        assert!(app.transcript.is_empty());

        // Engaging with the new session (submitting a prompt) retires the hint.
        app.reduce(AppAction::SubmitInput("hello".to_string()));
        assert!(app.session_hint.is_none());
    }

    #[test]
    fn session_hint_is_ephemeral_and_expires_after_tick_window() {
        let mut app = app();
        app.set_session_hint("Interrupted session found. Use /resume to bring it back.");

        assert_eq!(
            app.session_hint.as_ref().map(|hint| hint.text.as_str()),
            Some("Interrupted session found. Use /resume to bring it back.")
        );
        assert!(app.transcript.is_empty());

        for _ in 0..SESSION_HINT_TICKS {
            app.reduce(AppAction::Tick);
        }

        assert!(app.session_hint.is_none());
    }
}
