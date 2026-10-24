//! Explicit projection between durable entries and process-local display state.

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;
use super::*;
use crate::protocol as p;

impl From<&ToolActivity> for p::ToolActivity {
    fn from(a: &ToolActivity) -> Self {
        Self {
            id: a.id.clone(),
            name: a.name.clone(),
            arguments: a.arguments.clone(),
            delegated_model: a.delegated_model.clone(),
            status: match a.status {
                ToolStatus::Running => p::ToolStatus::Running,
                ToolStatus::Succeeded => p::ToolStatus::Succeeded,
                ToolStatus::Failed => p::ToolStatus::Failed,
                ToolStatus::Interrupted => p::ToolStatus::Interrupted,
            },
            result: a.result.clone(),
            diff: a.diff.clone(),
            tool_result: a.tool_result.clone(),
            timing: p::ToolCallTiming {
                started_at_ms: a.timing.started_at_ms,
                finished_at_ms: a.timing.finished_at_ms,
                legacy_duration_ms: a.timing.legacy_duration_ms.or_else(|| {
                    a.finished_at
                        .map(|_| a.duration().as_millis().min(i64::MAX as u128) as i64)
                }),
            },
        }
    }
}

impl From<p::ToolActivity> for ToolActivity {
    fn from(a: p::ToolActivity) -> Self {
        let now = Instant::now();
        let duration = a
            .timing
            .legacy_duration_ms
            .or_else(|| a.timing.signed_duration_ms())
            .and_then(|ms| u64::try_from(ms).ok())
            .map(Duration::from_millis)
            .unwrap_or_default();
        let running = a.status == p::ToolStatus::Running;
        Self {
            id: a.id,
            name: a.name,
            arguments: a.arguments,
            delegated_model: a.delegated_model,
            status: match a.status {
                p::ToolStatus::Running => ToolStatus::Running,
                p::ToolStatus::Succeeded => ToolStatus::Succeeded,
                p::ToolStatus::Failed => ToolStatus::Failed,
                p::ToolStatus::Interrupted => ToolStatus::Interrupted,
            },
            result: a.result,
            diff: a.diff,
            tool_result: a.tool_result,
            started_at: now.checked_sub(duration).unwrap_or(now),
            finished_at: (!running).then_some(now),
            timing: ToolCallTiming {
                started_at_ms: a.timing.started_at_ms,
                finished_at_ms: a.timing.finished_at_ms,
                legacy_duration_ms: a.timing.legacy_duration_ms,
            },
        }
    }
}

impl From<&TranscriptItem> for p::TranscriptItem {
    fn from(item: &TranscriptItem) -> Self {
        match item {
            TranscriptItem::UserMessage { text } => Self::UserMessage { text: text.clone() },
            TranscriptItem::QueuedUserMessage { id, text, delivery } => Self::QueuedUserMessage {
                id: *id,
                text: text.clone(),
                delivery: match delivery {
                    crate::tui::app::FollowUpDelivery::Steer => p::FollowUpDelivery::Steer,
                    crate::tui::app::FollowUpDelivery::Queue => p::FollowUpDelivery::Queue,
                },
            },
            TranscriptItem::AssistantMessage { text } => {
                Self::AssistantMessage { text: text.clone() }
            }
            TranscriptItem::ReasoningSummary { text } => {
                Self::ReasoningSummary { text: text.clone() }
            }
            TranscriptItem::ToolActivity(a) => Self::ToolActivity(a.into()),
            TranscriptItem::CommandOutput { kind, text } => Self::CommandOutput {
                kind: match kind {
                    CommandOutputKind::Status => p::CommandOutputKind::Status,
                    CommandOutputKind::Error => p::CommandOutputKind::Error,
                    CommandOutputKind::CompactionStatus => p::CommandOutputKind::CompactionStatus,
                },
                text: text.clone(),
            },
            TranscriptItem::CompletionReport(r) => Self::CompletionReport(r.clone()),
            TranscriptItem::ExecutionGroup(g) => Self::ExecutionGroup(p::ExecutionGroup {
                id: g.id,
                finished: g.finished_at.is_some(),
                tools: g.tools.iter().map(Into::into).collect(),
            }),
            TranscriptItem::Error { error } => Self::Error {
                error: p::TranscriptError {
                    title: error.title.clone(),
                    detail: error.detail.clone(),
                },
            },
            TranscriptItem::Edit { text } => Self::Edit { text: text.clone() },
            TranscriptItem::TodoUpdate { text } => Self::TodoUpdate { text: text.clone() },
            TranscriptItem::WorkLog { text } => Self::WorkLog { text: text.clone() },
            TranscriptItem::PeerMessage {
                source_message_id,
                session_id,
                outgoing,
                text,
            } => Self::PeerMessage {
                source_message_id: *source_message_id,
                session_id: *session_id,
                outgoing: *outgoing,
                text: text.clone(),
            },
        }
    }
}

impl From<p::TranscriptItem> for TranscriptItem {
    fn from(item: p::TranscriptItem) -> Self {
        match item {
            p::TranscriptItem::UserMessage { text } => Self::UserMessage { text },
            p::TranscriptItem::QueuedUserMessage { id, text, delivery } => {
                Self::QueuedUserMessage {
                    id,
                    text,
                    delivery: match delivery {
                        p::FollowUpDelivery::Steer => crate::tui::app::FollowUpDelivery::Steer,
                        p::FollowUpDelivery::Queue => crate::tui::app::FollowUpDelivery::Queue,
                    },
                }
            }
            p::TranscriptItem::AssistantMessage { text } => Self::AssistantMessage { text },
            p::TranscriptItem::ReasoningSummary { text } => Self::ReasoningSummary { text },
            p::TranscriptItem::ToolActivity(a) => Self::ToolActivity(a.into()),
            p::TranscriptItem::CommandOutput { kind, text } => Self::CommandOutput {
                kind: match kind {
                    p::CommandOutputKind::Status => CommandOutputKind::Status,
                    p::CommandOutputKind::Error => CommandOutputKind::Error,
                    p::CommandOutputKind::CompactionStatus => CommandOutputKind::CompactionStatus,
                },
                text,
            },
            p::TranscriptItem::CompletionReport(r) => Self::CompletionReport(r),
            p::TranscriptItem::ExecutionGroup(g) => Self::ExecutionGroup(ExecutionGroup {
                id: g.id,
                finished_at: g.finished.then(Instant::now),
                tools: g.tools.into_iter().map(Into::into).collect(),
            }),
            p::TranscriptItem::Error { error } => Self::Error {
                error: UiError {
                    title: error.title,
                    detail: error.detail,
                },
            },
            p::TranscriptItem::Edit { text } => Self::Edit { text },
            p::TranscriptItem::TodoUpdate { text } => Self::TodoUpdate { text },
            p::TranscriptItem::WorkLog { text } => Self::WorkLog { text },
            p::TranscriptItem::PeerMessage {
                source_message_id,
                session_id,
                outgoing,
                text,
            } => Self::PeerMessage {
                source_message_id,
                session_id,
                outgoing,
                text,
            },
        }
    }
}
