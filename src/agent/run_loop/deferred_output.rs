use std::sync::{Arc, Mutex};

use crate::{
    agent::ContextReport,
    diff::FileDiff,
    output::{OutputDeliveryBarrier, OutputSink, SharedSink, ToolCallStart, ToolExecutionStatus},
};

#[derive(Default)]
struct DeferredAssistantOutput {
    /// Accumulated answer text. One growing buffer, not a `Vec` of per-delta
    /// `String`s: this sink sits on the per-token path of deferred turns, and
    /// the flush never needed the original delta boundaries — downstream
    /// sinks only append.
    text: String,
    done: bool,
}

pub(super) struct DeferredAssistantSink {
    inner: SharedSink,
    assistant: Mutex<DeferredAssistantOutput>,
}

impl DeferredAssistantSink {
    pub(super) fn new(inner: SharedSink) -> Arc<Self> {
        Arc::new(Self {
            inner,
            assistant: Mutex::new(DeferredAssistantOutput::default()),
        })
    }

    pub(super) fn flush(&self) {
        let output = self.take_output();
        if !output.text.is_empty() {
            self.inner.assistant_delta(&output.text);
        }
        if output.done {
            self.inner.assistant_done();
        }
    }

    pub(super) fn discard(&self) {
        let _ = self.take_output();
    }

    fn take_output(&self) -> DeferredAssistantOutput {
        self.assistant
            .lock()
            .map(|mut output| std::mem::take(&mut *output))
            .unwrap_or_default()
    }
}

impl OutputSink for DeferredAssistantSink {
    fn assistant_delta(&self, text: &str) {
        if let Ok(mut output) = self.assistant.lock() {
            output.text.push_str(text);
        }
    }

    fn assistant_done(&self) {
        if let Ok(mut output) = self.assistant.lock() {
            output.done = true;
        }
    }

    fn reasoning_delta(&self, text: &str) {
        self.inner.reasoning_delta(text);
    }

    fn attempt_started(&self) {
        self.inner.attempt_started();
    }

    fn attempt_discarded(&self) {
        // The abandoned attempt's buffered answer text must go with it, or
        // the retry's deltas would splice onto it at the next flush.
        self.discard();
        self.inner.attempt_discarded();
    }

    fn thinking(&self, text: &str) {
        self.inner.thinking(text);
    }

    fn tool_calls_started(&self, calls: &[ToolCallStart]) {
        self.inner.tool_calls_started(calls);
    }

    fn tool_started(&self, id: &str, name: &str, arguments: &str) {
        self.inner.tool_started(id, name, arguments);
    }

    fn tool_output(&self, id: &str, output: &str) {
        self.inner.tool_output(id, output);
    }

    fn tool_finished(&self, id: &str, result: &str, status: ToolExecutionStatus) {
        self.inner.tool_finished(id, result, status);
    }

    fn tool_finished_with_diff(
        &self,
        id: &str,
        result: &str,
        status: ToolExecutionStatus,
        diff: FileDiff,
    ) {
        self.inner.tool_finished_with_diff(id, result, status, diff);
    }

    fn delivery_barrier(&self) -> Option<OutputDeliveryBarrier> {
        self.inner.delivery_barrier()
    }

    fn workspace_changed(&self, paths: &[String], intent: &str) {
        self.inner.workspace_changed(paths, intent);
    }

    fn queued_user_message_sent(&self, id: u64, text: &str) {
        self.inner.queued_user_message_sent(id, text);
    }

    fn context_updated(&self, report: ContextReport) {
        self.inner.context_updated(report);
    }

    fn transient_status(&self, text: &str) {
        self.inner.transient_status(text);
    }

    fn status(&self, text: &str) {
        self.inner.status(text);
    }

    fn compaction_status(&self, text: &str) {
        self.inner.compaction_status(text);
    }

    fn error(&self, text: &str) {
        self.inner.error(text);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct RecordingSink {
        output: Mutex<Vec<String>>,
    }

    impl OutputSink for RecordingSink {
        fn assistant_delta(&self, text: &str) {
            self.output
                .lock()
                .expect("recording sink mutex should not be poisoned")
                .push(format!("delta:{text}"));
        }

        fn assistant_done(&self) {
            self.output
                .lock()
                .expect("recording sink mutex should not be poisoned")
                .push("done".to_string());
        }

        fn thinking(&self, text: &str) {
            self.output
                .lock()
                .expect("recording sink mutex should not be poisoned")
                .push(format!("thinking:{text}"));
        }
    }

    #[test]
    fn buffers_assistant_output_until_flush_and_forwards_other_output() {
        let inner = Arc::new(RecordingSink::default());
        let sink = DeferredAssistantSink::new(inner.clone());

        sink.assistant_delta("hello");
        sink.assistant_delta(" world");
        sink.assistant_done();
        sink.thinking("working");

        assert_eq!(
            *inner
                .output
                .lock()
                .expect("recording sink mutex should not be poisoned"),
            vec!["thinking:working"]
        );

        sink.flush();

        assert_eq!(
            *inner
                .output
                .lock()
                .expect("recording sink mutex should not be poisoned"),
            vec!["thinking:working", "delta:hello world", "done"]
        );
    }
}
