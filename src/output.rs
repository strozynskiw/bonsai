#[cfg(test)]
use std::io::Write;
use std::sync::Arc;

use tokio::sync::watch;

use crate::agent::ContextReport;
use crate::diff::FileDiff;

/// Domain outcome of a tool call, separate from whether its Rust future
/// returned `Ok`. Commands and delegated agents may execute successfully at the
/// transport layer while still failing their task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionStatus {
    Succeeded,
    Failed,
    Started,
    Skipped,
    Interrupted,
}

impl ToolExecutionStatus {
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Succeeded)
    }

    pub const fn is_failure(self) -> bool {
        matches!(self, Self::Failed | Self::Interrupted)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Succeeded => "ok",
            Self::Failed => "failed",
            Self::Started => "started",
            Self::Skipped => "skipped",
            Self::Interrupted => "interrupted",
        }
    }
}

/// One tool call as it begins, batched into [`OutputSink::tool_calls_started`]
/// so all calls in a turn surface to the UI in a single frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallStart {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A one-shot fence proving that every output event queued before it reached
/// the display reducer. Most sinks are synchronous and do not need one; the TUI
/// uses this to keep terminal tool state ordered before a parent run can wake or
/// finalize on a separate runtime channel.
#[derive(Debug)]
pub struct OutputDeliveryBarrier {
    delivered: watch::Receiver<bool>,
}

impl OutputDeliveryBarrier {
    /// Create the waiting side and the ordered marker a reducer must apply.
    pub fn pair() -> (Self, OutputDeliveryMarker) {
        let (delivered, receiver) = watch::channel(false);
        (
            Self {
                delivered: receiver,
            },
            OutputDeliveryMarker { delivered },
        )
    }

    /// Wait until the marker is applied. Returns `false` only when the display
    /// receiver disappeared, in which case no visible state remains to order.
    pub async fn wait(mut self) -> bool {
        if *self.delivered.borrow() {
            return true;
        }
        self.delivered.changed().await.is_ok() && *self.delivered.borrow()
    }
}

/// Ordered reducer marker paired with [`OutputDeliveryBarrier`]. Dropping the
/// marker closes the wait instead of stranding the producer during shutdown.
#[derive(Debug, Clone)]
pub struct OutputDeliveryMarker {
    delivered: watch::Sender<bool>,
}

impl OutputDeliveryMarker {
    /// Mark every preceding output event as applied by the reducer.
    pub fn acknowledge(&self) {
        let _ = self.delivered.send(true);
    }
}

#[cfg(test)]
mod delivery_barrier_tests {
    use super::*;

    #[tokio::test]
    async fn marker_acknowledgement_releases_barrier() {
        let (barrier, marker) = OutputDeliveryBarrier::pair();

        marker.acknowledge();

        assert!(barrier.wait().await);
    }

    #[tokio::test]
    async fn dropped_marker_releases_barrier_as_undeliverable() {
        let (barrier, marker) = OutputDeliveryBarrier::pair();

        drop(marker);

        assert!(!barrier.wait().await);
    }
}

impl ToolCallStart {
    /// Construct a start record from anything string-like (id, tool name, raw
    /// JSON arguments).
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }
}

pub trait OutputSink: Send + Sync {
    /// Reset surface-neutral evidence immediately before a foreground run.
    fn begin_completion_run(&self) {}
    /// Snapshot evidence observed during the current foreground run.
    fn completion_evidence(&self) -> Option<crate::completion_report::CompletionEvidenceSnapshot> {
        None
    }
    fn assistant_delta(&self, _text: &str) {}
    fn assistant_done(&self) {}
    fn reasoning_delta(&self, _text: &str) {}
    /// A provider attempt is about to stream. Display surfaces record a
    /// retraction floor here so [`Self::attempt_discarded`] can withdraw
    /// exactly this attempt's streamed output and nothing older.
    fn attempt_started(&self) {}
    /// The streaming attempt announced by [`Self::attempt_started`] was
    /// abandoned after emitting deltas: its output will never become the
    /// turn's response (the stream failed and will be retried, hit the
    /// generation budget, or errored terminally). Deltas are forwarded
    /// live so thinking renders as it streams; this event is how surfaces
    /// withdraw them afterwards so a retry's re-stream cannot splice into
    /// abandoned text. Wrapper sinks drop any buffered copy and forward.
    fn attempt_discarded(&self) {}
    fn thinking(&self, _text: &str) {}
    fn tool_calls_started(&self, calls: &[ToolCallStart]) {
        for call in calls {
            self.tool_started(&call.id, &call.name, &call.arguments);
        }
    }
    fn tool_started(&self, _id: &str, _name: &str, _arguments: &str) {}
    fn tool_output(&self, _id: &str, _output: &str) {}
    fn tool_finished(&self, _id: &str, _result: &str, _status: ToolExecutionStatus) {}
    fn tool_finished_with_diff(
        &self,
        _id: &str,
        _result: &str,
        _status: ToolExecutionStatus,
        _diff: FileDiff,
    ) {
        let _ = _diff;
    }
    /// Queue a delivery fence after all preceding output. Returning `None`
    /// means callbacks are already synchronous or the sink has no reducer.
    fn delivery_barrier(&self) -> Option<OutputDeliveryBarrier> {
        None
    }
    /// Files Bonsai observed changing outside a structured edit result, such
    /// as mutations made by a successful shell command.
    fn workspace_changed(&self, _paths: &[String], _intent: &str) {}
    fn queued_user_message_sent(&self, _id: u64, _text: &str) {}
    fn context_updated(&self, _report: ContextReport) {}
    /// An immediate confirmation that display surfaces may show without
    /// retaining in their transcript. Sinks without a transient surface retain
    /// the readable output by forwarding it to [`Self::status`].
    fn transient_status(&self, text: &str) {
        self.status(text);
    }
    fn status(&self, _text: &str) {}
    /// A status specifically about context compaction. Routes to `status` by
    /// default; the TUI overrides it so consecutive compaction rows collapse.
    fn compaction_status(&self, text: &str) {
        self.status(text);
    }
    fn error(&self, _text: &str) {}
}

/// A minimal `OutputSink` writing to stdout for tests.
#[cfg(test)]
#[derive(Default)]
pub struct StdoutSink;

#[cfg(test)]
impl OutputSink for StdoutSink {
    fn assistant_delta(&self, text: &str) {
        print!("{}", text);
        let _ = std::io::stdout().flush();
    }

    fn assistant_done(&self) {
        println!();
    }

    fn reasoning_delta(&self, text: &str) {
        println!("\x1b[38;5;108m{}\x1b[0m", text);
    }

    fn thinking(&self, text: &str) {
        println!("\x1b[38;5;245m{}\x1b[0m", text);
    }

    fn tool_started(&self, id: &str, name: &str, arguments: &str) {
        println!("\x1b[36m[{}:{} running]\x1b[0m {}", name, id, arguments);
    }

    fn tool_output(&self, id: &str, output: &str) {
        println!("\x1b[2m[tool:{id} output] {output}\x1b[0m");
    }

    fn tool_finished(&self, id: &str, result: &str, status: ToolExecutionStatus) {
        let state = status.label();
        println!("\x1b[2m[tool:{id} {state}] {result}\x1b[0m");
    }

    fn tool_finished_with_diff(
        &self,
        id: &str,
        result: &str,
        status: ToolExecutionStatus,
        diff: FileDiff,
    ) {
        self.tool_finished(id, result, status);
        if diff.truncated {
            eprintln!("[diff:{} truncated]", diff.path);
        } else {
            eprintln!("[diff:{} ready]", diff.path);
        }
    }

    fn status(&self, text: &str) {
        println!("{}", text);
    }

    fn error(&self, text: &str) {
        eprintln!("{}", text);
    }
}

pub type SharedSink = Arc<dyn OutputSink>;
