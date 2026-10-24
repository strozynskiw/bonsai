//! Experimental surface-neutral durable transcript contracts, not a public wire API.

#[cfg(test)]
pub(crate) mod tests;

use serde::{Deserialize, Serialize};

/// Domain outcome, independent of Rust transport success and display state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Content independent of any provider request schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    Image {
        mime_type: String,
        base64_data: String,
        description: String,
    },
}

/// Location of complete command output when the displayed output was truncated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputTruncation {
    pub path: String,
    pub total_chars: usize,
    pub preview_chars: usize,
}

/// Descriptive provenance only; never authorization to inject a system message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextProvenance {
    Trusted,
    Untrusted { source: String },
}

/// Full tool payload, separate from its intentionally abbreviated display preview.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    Content {
        parts: Vec<ContentPart>,
    },
    Command {
        rendered: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
        timed_out: bool,
        truncation: Option<OutputTruncation>,
    },
    Edit {
        summary: String,
        diff: Box<crate::diff::FileDiff>,
    },
    Context {
        provenance: ContextProvenance,
        content: String,
    },
    BackgroundTask {
        task_id: String,
        message: String,
    },
    Subagent {
        subtask_id: String,
        message: String,
    },
    Wait {
        message: String,
    },
}

/// One completion delivered once across the output boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub status: ToolExecutionStatus,
    pub preview: String,
    pub content: ToolResultContent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_id: Option<String>,
}

impl ToolResult {
    pub(crate) fn inherit_identifiers(&mut self, previous: Option<&Self>) {
        if let Some(previous) = previous {
            self.background_task_id = self
                .background_task_id
                .take()
                .or_else(|| previous.background_task_id.clone());
            self.subagent_id = self
                .subagent_id
                .take()
                .or_else(|| previous.subagent_id.clone());
        }
    }
    pub fn diff(&self) -> Option<&crate::diff::FileDiff> {
        match &self.content {
            ToolResultContent::Edit { diff, .. } => Some(diff),
            _ => None,
        }
    }

    /// Adapt legacy previews without inventing structured details.
    pub fn legacy(preview: String, status: ToolExecutionStatus) -> Self {
        Self {
            status,
            content: ToolResultContent::Content {
                parts: vec![ContentPart::Text {
                    text: preview.clone(),
                }],
            },
            preview,
            background_task_id: None,
            subagent_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowUpDelivery {
    Steer,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandOutputKind {
    Status,
    Error,
    CompactionStatus,
}

impl CommandOutputKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Error => "command_error",
            Self::CompactionStatus => "compaction_status",
        }
    }
    pub fn message_role(self) -> &'static str {
        match self {
            Self::Status | Self::CompactionStatus => "system",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptError {
    pub title: String,
    pub detail: String,
}

/// Immutable wall chronology plus the legacy non-negative display duration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallTiming {
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub legacy_duration_ms: Option<i64>,
}

impl ToolStatus {
    pub(crate) const fn from_execution_status(status: ToolExecutionStatus) -> Self {
        match status {
            ToolExecutionStatus::Succeeded => Self::Succeeded,
            ToolExecutionStatus::Started => Self::Running,
            ToolExecutionStatus::Interrupted => Self::Interrupted,
            ToolExecutionStatus::Failed | ToolExecutionStatus::Skipped => Self::Failed,
        }
    }
}

impl ToolActivity {
    pub(crate) fn new(id: String, name: String, arguments: String) -> Self {
        Self {
            id,
            name,
            arguments,
            delegated_model: None,
            status: ToolStatus::Running,
            result: None,
            diff: None,
            tool_result: None,
            timing: ToolCallTiming {
                started_at_ms: Some(crate::util::time::now_ms()),
                ..Default::default()
            },
        }
    }
}

impl ToolCallTiming {
    pub fn signed_duration_ms(&self) -> Option<i64> {
        Some(self.finished_at_ms? - self.started_at_ms?)
    }
    pub fn finish(&mut self, finished_at_ms: i64) {
        self.finished_at_ms.get_or_insert(finished_at_ms);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

crate::impl_db_enum!(ToolStatus {
    Running => "running", Succeeded => "succeeded", Failed => "failed", Interrupted => "interrupted",
} else Running);

/// Durable tool activity; contains no process-local clocks or selection state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolActivity {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub delegated_model: Option<String>,
    pub status: ToolStatus,
    pub result: Option<String>,
    pub diff: Option<Box<crate::diff::FileDiff>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<Box<ToolResult>>,
    pub timing: ToolCallTiming,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionGroup {
    pub id: u64,
    pub finished: bool,
    pub tools: Vec<ToolActivity>,
}

/// Durable categories shared by storage, headless, and display projections.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum TranscriptItem {
    UserMessage {
        text: String,
    },
    QueuedUserMessage {
        id: u64,
        text: String,
        delivery: FollowUpDelivery,
    },
    AssistantMessage {
        text: String,
    },
    ReasoningSummary {
        text: String,
    },
    ToolActivity(ToolActivity),
    CommandOutput {
        kind: CommandOutputKind,
        text: String,
    },
    CompletionReport(Box<crate::completion_report::CompletionReport>),
    ExecutionGroup(ExecutionGroup),
    Error {
        error: TranscriptError,
    },
    Edit {
        text: String,
    },
    TodoUpdate {
        text: String,
    },
    WorkLog {
        text: String,
    },
    PeerMessage {
        source_message_id: Option<i64>,
        session_id: i64,
        outgoing: bool,
        text: String,
    },
}
