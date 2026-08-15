use super::{AppState, CompletionState, ComposerContent, TranscriptItem};
use crate::agent::AgentMode;
use crate::storage::SessionId;
use crate::tui::pickers::ModelSelection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Whether a busy-state composer message joins the active turn or waits for
/// the next foreground run.
pub enum FollowUpDelivery {
    /// Replace the active foreground turn and open the next one immediately.
    Steer,
    /// Hold the message until the active foreground run finishes.
    Queue,
}

impl FollowUpDelivery {
    pub(crate) fn pending_label(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Queue => "queued",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedInput {
    pub id: u64,
    /// Placeholder text shown in the transcript's queued row.
    pub text: String,
    /// Full composer snapshot (buffer + chip payloads) so a withdraw can
    /// restore the exact draft, chips included.
    pub content: ComposerContent,
    pub mode: AgentMode,
    pub delivery: FollowUpDelivery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredCommand {
    pub id: u64,
    pub input: String,
    pub label: String,
    pub payload: DeferredCommandPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeferredCommandPayload {
    SlashCommand,
    ModelSelection(ModelSelection),
}

impl AppState {
    pub(crate) fn set_session_identity(
        &mut self,
        session_id: SessionId,
        name: impl Into<String>,
        summary: impl Into<String>,
    ) {
        self.current_session_id = Some(session_id);
        // A newly adopted session has not completed a run in this process yet.
        // AgentFinished replaces this with the typed run outcome.
        self.current_session_status = crate::storage::SessionStatus::Active;
        self.current_terminal_reason = None;
        self.current_session_name = name.into();
        self.current_session_summary = summary.into();
    }

    pub(crate) fn clear_session_identity(&mut self) {
        self.current_session_id = None;
        self.current_session_name.clear();
        self.current_session_summary.clear();
    }

    pub(crate) fn current_session_title(&self) -> Option<String> {
        let summary = self.current_session_summary.trim();
        if !summary.is_empty() {
            Some(summary.to_string())
        } else {
            let name = self.current_session_name.trim();
            if !name.is_empty() {
                Some(name.to_string())
            } else {
                None
            }
        }
    }

    pub fn next_queued_input_id(&mut self) -> u64 {
        let id = self.next_queued_input_id;
        self.next_queued_input_id = self.next_queued_input_id.saturating_add(1);
        id
    }

    pub fn first_queued_input(&self) -> Option<&QueuedInput> {
        self.queued_inputs.first()
    }

    pub fn last_queued_input(&self) -> Option<&QueuedInput> {
        self.queued_inputs.last()
    }

    pub(super) fn promote_queued_input_to_steer(&mut self, id: u64) {
        let Some(queued) = self.queued_inputs.iter_mut().find(|queued| queued.id == id) else {
            return;
        };
        queued.delivery = FollowUpDelivery::Steer;

        let transcript_index = self
            .transcript
            .iter()
            .position(|item| {
                matches!(item, TranscriptItem::QueuedUserMessage { id: item_id, .. } if *item_id == id)
            });
        if let Some(TranscriptItem::QueuedUserMessage { delivery, .. }) =
            transcript_index.and_then(|index| self.transcript.get_mut(index))
        {
            *delivery = FollowUpDelivery::Steer;
        }
    }

    pub fn queued_input_ids(&self) -> Vec<u64> {
        self.queued_inputs.iter().map(|queued| queued.id).collect()
    }

    pub fn next_deferred_command_id(&mut self) -> u64 {
        let id = self.next_deferred_command_id;
        self.next_deferred_command_id = self.next_deferred_command_id.saturating_add(1);
        id
    }

    pub(super) fn push_transcript_item(&mut self, item: TranscriptItem) {
        self.transcript.push_item(
            item,
            &mut self.transcript_focus,
            &mut self.transcript_selection,
        );
    }

    /// Append a context-compaction status, collapsing onto the previous row when
    /// the most recent (non-queued) transcript item is already a compaction
    /// status, so back-to-back compactions don't pile up.
    pub(super) fn push_or_collapse_compaction_status(&mut self, text: String) {
        let tail = self.transcript.first_trailing_queued_index();
        if tail > 0
            && let Some(TranscriptItem::CommandOutput {
                kind: crate::tui::event::CommandOutputKind::CompactionStatus,
                text: existing,
            }) = self.transcript.get_mut(tail - 1)
        {
            *existing = text;
            return;
        }
        self.push_transcript_item(TranscriptItem::CommandOutput {
            kind: crate::tui::event::CommandOutputKind::CompactionStatus,
            text,
        });
    }

    pub(super) fn finish_input_submission(&mut self, text: String) {
        self.composer.push_history(text);
        self.composer.clear_content();
        self.composer.reset_navigation();
        self.reset_composer_scroll();
        self.completion = CompletionState::default();
    }

    pub(super) fn enqueue_follow_up(
        &mut self,
        id: u64,
        text: String,
        content: ComposerContent,
        mode: AgentMode,
        delivery: FollowUpDelivery,
    ) {
        self.composer.push_history(text.clone());
        self.composer.clear_content();
        self.composer.reset_navigation();
        self.reset_composer_scroll();
        self.completion = CompletionState::default();
        self.queued_inputs.push(QueuedInput {
            id,
            text: text.clone(),
            content,
            mode,
            delivery,
        });
        self.push_transcript_item(TranscriptItem::QueuedUserMessage { id, text, delivery });
        self.maybe_scroll_to_bottom_current();
    }

    pub fn focused_queued_input_id(&self) -> Option<u64> {
        let index = self.transcript_focus?;
        match self.transcript.get(index) {
            Some(TranscriptItem::QueuedUserMessage { id, .. }) => Some(*id),
            _ => None,
        }
    }

    pub(super) fn mark_queued_user_message_sent(&mut self, id: u64, text: String) {
        self.queued_inputs.retain(|queued| queued.id != id);

        // Index-precise (`position` + `get_mut`) so the layout cache
        // invalidates only the promoted item, not the whole transcript.
        let index = self.transcript.iter().position(|item| {
            matches!(item, TranscriptItem::QueuedUserMessage { id: item_id, .. } if *item_id == id)
        });
        if let Some(item) = index.and_then(|index| self.transcript.get_mut(index)) {
            *item = TranscriptItem::UserMessage { text };
        } else {
            self.push_transcript_item(TranscriptItem::UserMessage { text });
        }
    }

    pub(super) fn remove_queued_input(&mut self, id: u64) {
        let queue_len = self.queued_inputs.len();
        self.queued_inputs.retain(|queued| queued.id != id);
        let queue_changed = self.queued_inputs.len() != queue_len;

        let Some(index) = self.transcript.iter().position(|item| {
            matches!(item, TranscriptItem::QueuedUserMessage { id: item_id, .. } if *item_id == id)
        }) else {
            if queue_changed {
                self.maybe_scroll_to_bottom_current();
            }
            return;
        };

        self.transcript.remove(index);
        if let Some(focus) = self.transcript_focus {
            self.transcript_focus = if self.transcript.is_empty() {
                None
            } else if focus == index {
                Some(index.min(self.transcript.len().saturating_sub(1)))
            } else if focus > index {
                Some(focus - 1)
            } else {
                Some(focus)
            };
        }
        self.transcript_selection = None;
        self.maybe_scroll_to_bottom_current();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::event::{AppAction, Focus, UiEvent};

    fn app() -> AppState {
        AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        )
    }

    #[test]
    fn queue_input_clears_composer_and_adds_pending_block() {
        let mut app = app();
        app.composer.set_text("queued text".to_string());
        app.reduce(AppAction::SteerInput {
            id: 42,
            text: "queued text".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });

        assert_eq!(app.input(), "");
        assert_eq!(app.composer.text, "");
        assert_eq!(
            app.queued_inputs,
            vec![QueuedInput {
                id: 42,
                text: "queued text".to_string(),
                content: crate::tui::app::ComposerContent::default(),
                mode: AgentMode::Coding,
                delivery: FollowUpDelivery::Steer,
            }]
        );
        assert_eq!(app.composer.history, vec!["queued text".to_string()]);
        assert!(matches!(
            app.transcript.last(),
            Some(TranscriptItem::QueuedUserMessage { id: 42, text, .. }) if text == "queued text"
        ));
    }

    #[test]
    fn queued_delivery_converts_pending_block_to_user_message() {
        let mut app = app();
        app.reduce(AppAction::SteerInput {
            id: 1,
            text: "queued text".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });

        app.reduce(AppAction::Agent(UiEvent::QueuedUserMessageSent {
            id: 1,
            text: "queued text".to_string(),
        }));

        assert!(app.queued_inputs.is_empty());
        assert!(matches!(
            app.transcript.as_slice(),
            [TranscriptItem::UserMessage { text }] if text == "queued text"
        ));
    }

    #[test]
    fn queued_inputs_append_fifo_and_clear_composer() {
        let mut app = app();
        app.reduce(AppAction::SteerInput {
            id: 1,
            text: "first".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });
        app.composer.set_text("second".to_string());
        app.reduce(AppAction::SteerInput {
            id: 2,
            text: "second".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Planning,
        });

        assert_eq!(app.input(), "");
        assert_eq!(app.composer.text, "");
        assert_eq!(
            app.queued_inputs,
            vec![
                QueuedInput {
                    id: 1,
                    text: "first".to_string(),
                    content: crate::tui::app::ComposerContent::default(),
                    mode: AgentMode::Coding,
                    delivery: FollowUpDelivery::Steer,
                },
                QueuedInput {
                    id: 2,
                    text: "second".to_string(),
                    content: crate::tui::app::ComposerContent::default(),
                    mode: AgentMode::Planning,
                    delivery: FollowUpDelivery::Steer,
                }
            ]
        );
        assert_eq!(
            app.transcript
                .iter()
                .filter(|item| matches!(item, TranscriptItem::QueuedUserMessage { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn queued_inputs_stay_at_bottom_when_output_arrives() {
        let mut app = app();
        app.reduce(AppAction::SteerInput {
            id: 1,
            text: "queued".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });

        app.reduce(AppAction::Agent(UiEvent::Status("working".to_string())));

        assert!(matches!(
            app.transcript.as_slice(),
            [
                TranscriptItem::CommandOutput { text, .. },
                TranscriptItem::QueuedUserMessage {
                    id: 1,
                    text: queued,
                    ..
                }
            ] if text == "working" && queued == "queued"
        ));
    }

    #[test]
    fn consecutive_compaction_statuses_collapse_to_one_row() {
        let mut app = app();
        app.reduce(AppAction::Agent(UiEvent::CompactionStatus(
            "first".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::CompactionStatus(
            "second".to_string(),
        )));

        assert!(matches!(
            app.transcript.as_slice(),
            [TranscriptItem::CommandOutput {
                kind: crate::tui::event::CommandOutputKind::CompactionStatus,
                text,
            }] if text == "second"
        ));
    }

    #[test]
    fn compaction_status_after_other_output_does_not_collapse() {
        let mut app = app();
        app.reduce(AppAction::Agent(UiEvent::CompactionStatus(
            "first".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::Status("other".to_string())));
        app.reduce(AppAction::Agent(UiEvent::CompactionStatus(
            "second".to_string(),
        )));

        assert_eq!(app.transcript.as_slice().len(), 3);
    }

    #[test]
    fn cancel_queued_input_removes_only_matching_pending_block() {
        let mut app = app();
        app.reduce(AppAction::SteerInput {
            id: 1,
            text: "first".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });
        app.reduce(AppAction::SteerInput {
            id: 2,
            text: "second".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Planning,
        });
        app.focus = Focus::Transcript;
        app.transcript_focus = Some(0);

        app.reduce(AppAction::CancelQueuedInput { id: 1 });

        assert_eq!(
            app.queued_inputs,
            vec![QueuedInput {
                id: 2,
                text: "second".to_string(),
                content: crate::tui::app::ComposerContent::default(),
                mode: AgentMode::Planning,
                delivery: FollowUpDelivery::Steer,
            }]
        );
        assert!(matches!(
            app.transcript.as_slice(),
            [TranscriptItem::QueuedUserMessage { id: 2, text, .. }] if text == "second"
        ));
        assert_eq!(app.transcript_focus, Some(0));
    }

    #[test]
    fn cancel_focused_queued_input_deletes_focused_queue_item() {
        let mut app = app();
        app.reduce(AppAction::SteerInput {
            id: 1,
            text: "first".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });
        app.reduce(AppAction::SteerInput {
            id: 2,
            text: "second".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });
        app.focus = Focus::Transcript;
        app.transcript_focus = Some(1);

        app.reduce(AppAction::CancelFocusedQueuedInput);

        assert_eq!(
            app.queued_inputs,
            vec![QueuedInput {
                id: 1,
                text: "first".to_string(),
                content: crate::tui::app::ComposerContent::default(),
                mode: AgentMode::Coding,
                delivery: FollowUpDelivery::Steer,
            }]
        );
        assert!(matches!(
            app.transcript.as_slice(),
            [TranscriptItem::QueuedUserMessage { id: 1, text, .. }] if text == "first"
        ));
    }

    #[test]
    fn withdraw_queued_input_restores_most_recent_message_to_composer() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::SteerInput {
            id: 1,
            text: "first".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });
        app.reduce(AppAction::SteerInput {
            id: 2,
            text: "second".to_string(),
            content: crate::tui::app::ComposerContent {
                text: "second".to_string(),
                chips: Vec::new(),
            },
            mode: AgentMode::Planning,
        });

        app.reduce(AppAction::WithdrawQueuedInput);

        assert_eq!(app.input(), "second");
        assert_eq!(app.composer.text, "second");
        assert_eq!(
            app.queued_inputs,
            vec![QueuedInput {
                id: 1,
                text: "first".to_string(),
                content: crate::tui::app::ComposerContent::default(),
                mode: AgentMode::Coding,
                delivery: FollowUpDelivery::Steer,
            }]
        );
        assert!(matches!(
            app.transcript.as_slice(),
            [TranscriptItem::QueuedUserMessage { id: 1, text, .. }] if text == "first"
        ));
    }

    #[test]
    fn drop_queued_input_removes_pending_block() {
        let mut app = app();
        app.reduce(AppAction::SteerInput {
            id: 1,
            text: "queued text".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });

        app.reduce(AppAction::DropQueuedInput);

        assert!(app.queued_inputs.is_empty());
        assert!(app.transcript.is_empty());
    }
}
