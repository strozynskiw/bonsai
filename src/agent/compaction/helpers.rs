//! Pure helpers for context compaction: message grouping, token accounting,
//! summary-message construction, and a few argument/path probes. The compaction
//! algorithm itself lives in `compaction.rs`.

use super::*;

/// Sentinel error returned when compaction is cancelled mid-flight; callers
/// detect it with [`is_compaction_cancelled`] to translate it into a clean
/// interrupt rather than a hard failure. Episode eviction reuses it for a
/// cancelled card provider call, so the run loop maps both identically.
#[derive(Debug)]
pub(in crate::agent) struct CompactionCancelled;

impl std::fmt::Display for CompactionCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("context compaction cancelled")
    }
}

impl std::error::Error for CompactionCancelled {}

pub(in crate::agent) fn message_groups(
    messages: &[ChatCompletionRequestMessage],
) -> Vec<MessageGroup> {
    let mut groups = Vec::new();
    let mut index = 1usize;
    while index < messages.len() {
        if let Some(calls) = assistant_tool_calls(&messages[index]) {
            let mut indices = vec![index];
            let mut pending = calls
                .into_iter()
                .map(|call| call.call_id)
                .collect::<HashSet<_>>();
            index += 1;
            while index < messages.len() {
                let Some(call_id) = tool_message_call_id(&messages[index]) else {
                    break;
                };
                if !pending.remove(&call_id) {
                    break;
                }
                indices.push(index);
                index += 1;
                if pending.is_empty() {
                    break;
                }
            }
            groups.push(MessageGroup { indices });
        } else {
            groups.push(MessageGroup {
                indices: vec![index],
            });
            index += 1;
        }
    }
    groups
}

pub(super) fn latest_user_message_indices(
    messages: &[ChatCompletionRequestMessage],
    count: usize,
) -> HashSet<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(index, message)| {
            let (role, _text) = describe_message_full(message);
            (role == ContextRole::User).then_some(index)
        })
        .take(count)
        .collect()
}

pub(super) fn last_group_indices(groups: &[MessageGroup], count: usize) -> HashSet<usize> {
    groups
        .iter()
        .enumerate()
        .rev()
        .take(count)
        .map(|(index, _group)| index)
        .collect()
}

pub(super) fn group_is_mandatory(
    agent: &Agent,
    group: &MessageGroup,
    latest_user: &HashSet<usize>,
) -> bool {
    group.indices.iter().any(|index| {
        *index == 0
            || latest_user.contains(index)
            || agent
                .messages
                .get(*index)
                .is_some_and(|message| agent.message_has_control(*index, message, |s| s.pinned))
    })
}

pub(super) fn estimated_compaction_summary_tokens(group_count: usize) -> usize {
    600usize
        .saturating_add(group_count.saturating_mul(24))
        .min(4_000)
}

pub(super) fn compacted_tool_output_message(
    call_id: &str,
    content: &str,
) -> Option<ChatCompletionRequestMessage> {
    ChatCompletionRequestToolMessageArgs::default()
        .content(content)
        .tool_call_id(call_id)
        .build()
        .ok()
        .map(ChatCompletionRequestMessage::Tool)
}

pub(super) fn compaction_summary_message(content: &str) -> Result<ChatCompletionRequestMessage> {
    Ok(ChatCompletionRequestMessage::System(
        ChatCompletionRequestSystemMessageArgs::default()
            .content(guarded_compaction_summary_content(content))
            .build()?,
    ))
}

pub(super) fn guarded_compaction_summary_content(content: &str) -> String {
    let content = content.trim();
    let body = content
        .strip_prefix("# Compacted Context Summary")
        .map(str::trim_start)
        .unwrap_or(content);
    format!("# Compacted Context Summary\n\n{COMPACTION_SUMMARY_TRUST_GUARD}\n\n{body}")
}

pub(super) fn format_provider_compaction_summary(summary: &str) -> String {
    let summary = summary.trim();
    let restore_note = "Restore omitted originals from /ctx when exact prior wording or full tool output is needed.";
    if summary.starts_with("# Compacted Context Summary") {
        format!("{summary}\n\n{restore_note}")
    } else {
        format!("# Compacted Context Summary\n\n{restore_note}\n\n{summary}")
    }
}

pub(in crate::agent) fn is_compaction_cancelled(err: &anyhow::Error) -> bool {
    err.downcast_ref::<CompactionCancelled>().is_some()
}

/// Status emitted after an automatic compaction so the user sees what the
/// mid-run pause accomplished: the token delta, what was summarized/stubbed, and
/// where to inspect or restore it. `session_count` is how many compactions have
/// run this session (for the de-noised `×N` counter).
pub(super) fn compaction_done_status(report: &CompactionReport, session_count: usize) -> String {
    let restore = if report.summary_source_available {
        " · originals restorable via /ctx"
    } else {
        ""
    };
    format!(
        "Context compacted ×{session_count} this session · {} → {} tokens\n{}{restore}",
        crate::util::format::format_tokens_k(report.before_tokens),
        crate::util::format::format_tokens_k(report.after_tokens),
        compaction_change_breakdown(report),
    )
}

/// `"N groups summarized, M tool outputs stubbed"`, omitting any zero part.
/// Shared by the automatic status and the manual `/compact` report so both read
/// consistently. Only called when the report has changes, so a part is present.
pub(super) fn compaction_change_breakdown(report: &CompactionReport) -> String {
    let mut parts = Vec::new();
    if report.messages_omitted > 0 {
        parts.push(format!(
            "{} group{} summarized",
            report.messages_omitted,
            plural_s(report.messages_omitted),
        ));
    }
    if report.tool_outputs_stubbed > 0 {
        parts.push(format!(
            "{} tool output{} stubbed",
            report.tool_outputs_stubbed,
            plural_s(report.tool_outputs_stubbed),
        ));
    }
    if report.repacked {
        parts.push("prefix repacked".to_string());
    }
    if parts.is_empty() {
        return "no changes".to_string();
    }
    parts.join(", ")
}

fn plural_s(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

pub(super) fn tool_argument_string(arguments: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

pub(super) fn message_path_hint(message: &ChatCompletionRequestMessage) -> Option<String> {
    if let Some(calls) = assistant_tool_calls(message) {
        for call in calls {
            if let Some(path) = tool_argument_string(&call.arguments, "path") {
                return Some(format!("- {path}"));
            }
        }
    }
    let (_role, text) = describe_message_full(message);
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("File: ")
            .map(|path| format!("- {}", path.trim()))
    })
}

pub(super) fn valid_context_control_ids(
    messages: &[ChatCompletionRequestMessage],
    message_ids: &[String],
) -> HashSet<String> {
    messages
        .iter()
        .enumerate()
        .flat_map(|(index, message)| {
            message_control_ids(index, message, message_ids.get(index).map(String::as_str))
        })
        .collect()
}

pub(in crate::agent) fn next_queued_messages(
    queued_messages: &mut Option<&mut mpsc::UnboundedReceiver<QueuedUserMessageCommand>>,
    pending_messages: &mut VecDeque<QueuedUserMessage>,
    cancelled_message_ids: &mut HashSet<u64>,
) -> Vec<QueuedUserMessage> {
    if let Some(receiver) = queued_messages.as_mut() {
        while let Ok(command) = receiver.try_recv() {
            match command {
                QueuedUserMessageCommand::Send(message) => pending_messages.push_back(message),
                QueuedUserMessageCommand::Cancel(id) => {
                    cancelled_message_ids.insert(id);
                }
            }
        }
    }

    let mut ready = Vec::new();
    while let Some(message) = pending_messages.pop_front() {
        if cancelled_message_ids.remove(&message.id) {
            continue;
        }
        ready.push(message);
    }

    ready
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(messages_omitted: usize, tool_outputs_stubbed: usize) -> CompactionReport {
        CompactionReport {
            mode: CompactionMode::Automatic,
            preview: false,
            summary_source: CompactionSummarySource::Deterministic,
            before_tokens: 162_200,
            after_tokens: 101_000,
            target_tokens: 100_000,
            before_messages: 10,
            after_messages: 6,
            messages_omitted,
            tool_outputs_stubbed,
            summary_source_available: true,
            repacked: messages_omitted > 0,
            target_reached: true,
        }
    }

    #[test]
    fn compaction_done_status_shows_breakdown_counter_and_ctx_pointer() {
        let status = compaction_done_status(&report(5, 12), 3);
        assert!(status.contains("×3 this session"), "{status}");
        assert!(status.contains("162.2k → 101.0k tokens"), "{status}");
        assert!(status.contains("5 groups summarized"), "{status}");
        assert!(status.contains("12 tool outputs stubbed"), "{status}");
        assert!(status.contains("prefix repacked"), "{status}");
        assert!(status.contains("restorable via /ctx"), "{status}");
    }

    #[test]
    fn compaction_change_breakdown_omits_zero_parts_and_singularizes() {
        assert_eq!(
            compaction_change_breakdown(&report(1, 0)),
            "1 group summarized, prefix repacked"
        );
        assert_eq!(
            compaction_change_breakdown(&report(0, 1)),
            "1 tool output stubbed"
        );
    }
}
