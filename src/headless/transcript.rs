use std::collections::HashMap;
use std::sync::Mutex as StdMutex;
use std::time::Instant;

use crate::diff::FileDiff;
use crate::tui::app::{ToolActivity, ToolStatus, TranscriptItem};

#[derive(Debug, Default)]
pub(crate) struct TranscriptRecorder {
    inner: StdMutex<TranscriptRecorderState>,
}

impl TranscriptRecorder {
    pub(crate) fn user_message(&self, text: &str) {
        self.with_state(|state| {
            state.flush_text_buffers();
            state.items.push(TranscriptItem::UserMessage {
                text: text.to_string(),
            });
        });
    }

    pub(crate) fn assistant_delta(&self, text: &str) {
        self.with_state(|state| {
            state.flush_reasoning();
            state.assistant.push_str(text);
        });
    }

    pub(crate) fn assistant_done(&self) {
        self.with_state(TranscriptRecorderState::flush_assistant);
    }

    pub(crate) fn reasoning_delta(&self, text: &str) {
        self.with_state(|state| {
            state.flush_assistant();
            state.reasoning.push_str(text);
        });
    }

    pub(crate) fn status(&self, text: &str) {
        self.with_state(|state| {
            state.flush_text_buffers();
            state.items.push(TranscriptItem::CommandOutput {
                kind: crate::tui::event::CommandOutputKind::Status,
                text: text.to_string(),
            });
        });
    }

    /// A provider attempt is about to stream: remember how much buffered
    /// text is already committed so a later retraction removes exactly this
    /// attempt's contribution (a prior call's reasoning can still sit in the
    /// buffer waiting for its flush boundary).
    pub(crate) fn attempt_started(&self) {
        self.with_state(|state| {
            state.assistant_floor = state.assistant.len();
            state.reasoning_floor = state.reasoning.len();
        });
    }

    /// The attempt was abandoned; drop what it streamed into the buffers.
    /// `truncate` is a no-op when a flush already consumed the buffer, so a
    /// stale floor cannot resurrect or damage committed items.
    pub(crate) fn attempt_discarded(&self) {
        self.with_state(|state| {
            state.assistant.truncate(state.assistant_floor);
            state.reasoning.truncate(state.reasoning_floor);
        });
    }

    pub(crate) fn tool_started(&self, id: &str, name: &str, arguments: &str) {
        self.with_state(|state| {
            state.flush_text_buffers();
            state.tools.insert(
                id.to_string(),
                ToolActivity::new(
                    id.to_string(),
                    name.to_string(),
                    arguments.to_string(),
                    Instant::now(),
                ),
            );
        });
    }

    pub(crate) fn tool_finished(
        &self,
        id: &str,
        result: &str,
        success: bool,
        diff: Option<FileDiff>,
    ) {
        self.with_state(|state| {
            state.flush_text_buffers();
            let now = Instant::now();
            if let Some(mut activity) = state.tools.remove(id) {
                finish_tool_activity(&mut activity, result, success, diff, now);
                state.items.push(TranscriptItem::ToolActivity(activity));
                return;
            }
            if let Some(activity) = recorded_tool_activity_mut(&mut state.items, id) {
                finish_tool_activity(activity, result, success, diff, now);
                return;
            }
            let mut activity =
                ToolActivity::new(id.to_string(), "tool".to_string(), String::new(), now);
            finish_tool_activity(&mut activity, result, success, diff, now);
            state.items.push(TranscriptItem::ToolActivity(activity));
        });
    }

    pub(crate) fn take(&self) -> Vec<TranscriptItem> {
        self.inner
            .lock()
            .map(|mut state| state.take())
            .unwrap_or_default()
    }

    fn with_state(&self, f: impl FnOnce(&mut TranscriptRecorderState)) {
        if let Ok(mut state) = self.inner.lock() {
            f(&mut state);
        }
    }
}

fn finish_tool_activity(
    activity: &mut ToolActivity,
    result: &str,
    success: bool,
    diff: Option<FileDiff>,
    now: Instant,
) {
    activity.status = if success {
        ToolStatus::Succeeded
    } else {
        ToolStatus::Failed
    };
    activity.result = Some(result.to_string());
    activity.diff = diff;
    activity.finished_at.get_or_insert(now);
}

fn recorded_tool_activity_mut<'a>(
    items: &'a mut [TranscriptItem],
    id: &str,
) -> Option<&'a mut ToolActivity> {
    for item in items.iter_mut().rev() {
        let TranscriptItem::ToolActivity(activity) = item else {
            continue;
        };
        if activity.id == id {
            return Some(activity);
        }
    }
    None
}

#[derive(Debug, Default)]
struct TranscriptRecorderState {
    items: Vec<TranscriptItem>,
    assistant: String,
    reasoning: String,
    /// Buffer lengths at the last `attempt_started`; text beyond them belongs
    /// to the in-flight attempt and is what `attempt_discarded` truncates.
    assistant_floor: usize,
    reasoning_floor: usize,
    tools: HashMap<String, ToolActivity>,
}

impl TranscriptRecorderState {
    fn flush_text_buffers(&mut self) {
        self.flush_assistant();
        self.flush_reasoning();
    }

    fn flush_assistant(&mut self) {
        if !self.assistant.trim().is_empty() {
            self.items.push(TranscriptItem::AssistantMessage {
                text: std::mem::take(&mut self.assistant),
            });
        } else {
            self.assistant.clear();
        }
    }

    fn flush_reasoning(&mut self) {
        if !self.reasoning.trim().is_empty() {
            self.items.push(TranscriptItem::ReasoningSummary {
                text: std::mem::take(&mut self.reasoning),
            });
        } else {
            self.reasoning.clear();
        }
    }

    fn take(&mut self) -> Vec<TranscriptItem> {
        self.flush_text_buffers();
        let now = Instant::now();
        for (_id, mut activity) in std::mem::take(&mut self.tools) {
            if activity.status == ToolStatus::Running {
                activity.status = ToolStatus::Failed;
                activity
                    .result
                    .get_or_insert_with(|| "Interrupted before completion".to_string());
            }
            activity.finished_at.get_or_insert(now);
            self.items.push(TranscriptItem::ToolActivity(activity));
        }
        std::mem::take(&mut self.items)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorder_buffers_assistant_message() {
        let recorder = TranscriptRecorder::default();

        recorder.user_message("hello");
        recorder.assistant_delta("hi");
        recorder.assistant_delta(" there");
        recorder.assistant_done();

        let items = recorder.take();
        assert_eq!(items.len(), 2);
        assert!(matches!(&items[0], TranscriptItem::UserMessage { text } if text == "hello"));
        assert!(
            matches!(&items[1], TranscriptItem::AssistantMessage { text } if text == "hi there")
        );
    }

    #[test]
    fn recorder_retracts_discarded_attempt_reasoning() {
        let recorder = TranscriptRecorder::default();

        recorder.attempt_started();
        recorder.reasoning_delta("committed thought");
        // The first attempt succeeded; its reasoning still sits in the buffer
        // waiting for a flush boundary when the next attempt begins.
        recorder.attempt_started();
        recorder.reasoning_delta("doomed thought");
        recorder.attempt_discarded();

        let items = recorder.take();
        assert_eq!(items.len(), 1);
        assert!(
            matches!(&items[0], TranscriptItem::ReasoningSummary { text } if text == "committed thought")
        );
    }

    #[test]
    fn recorder_records_tool_result() {
        let recorder = TranscriptRecorder::default();

        recorder.tool_started("call-1", "read", r#"{"path":"README.md"}"#);
        recorder.tool_finished("call-1", "ok", true, None);

        let items = recorder.take();
        assert_eq!(items.len(), 1);
        let TranscriptItem::ToolActivity(activity) = &items[0] else {
            panic!("expected tool activity");
        };
        assert_eq!(activity.id, "call-1");
        assert_eq!(activity.name, "read");
        assert_eq!(activity.status, ToolStatus::Succeeded);
        assert_eq!(activity.result.as_deref(), Some("ok"));
    }

    #[test]
    fn recorder_updates_duplicate_tool_finish_in_place() {
        let recorder = TranscriptRecorder::default();

        recorder.tool_started("call-1", "read", r#"{"path":"README.md"}"#);
        recorder.tool_finished("call-1", "full output", true, None);
        recorder.tool_finished("call-1", "reused previous read", true, None);

        let items = recorder.take();
        assert_eq!(items.len(), 1);
        let TranscriptItem::ToolActivity(activity) = &items[0] else {
            panic!("expected tool activity");
        };
        assert_eq!(activity.id, "call-1");
        assert_eq!(activity.name, "read");
        assert_eq!(activity.result.as_deref(), Some("reused previous read"));
    }

    #[test]
    fn recorder_marks_drained_running_tools_failed() {
        let recorder = TranscriptRecorder::default();

        recorder.tool_started("call-1", "bash", r#"{"command":"sleep 30"}"#);

        let items = recorder.take();
        assert_eq!(items.len(), 1);
        let TranscriptItem::ToolActivity(activity) = &items[0] else {
            panic!("expected tool activity");
        };
        assert_eq!(activity.status, ToolStatus::Failed);
        assert_eq!(
            activity.result.as_deref(),
            Some("Interrupted before completion")
        );
        assert!(activity.finished_at.is_some());
    }
}
