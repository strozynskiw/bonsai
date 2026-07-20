//! Rendering of chat messages, tool outputs, and background-task snapshots into
//! the human-readable strings shown in `/ctx` and injected as context.

use std::fmt::Write as _;

use super::*;

/// `message`'s content as an owned string, via a JSON round-trip. Falls back
/// to empty content on the (practically unreachable) serialization failure,
/// matching every existing infallible call site.
pub(super) fn message_content_string(message: &ChatCompletionRequestMessage) -> String {
    let value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    message_content_text(&value)
}

/// Like [`message_content_string`], but `None` on serialization failure
/// instead of falling back to empty content — for the callers that should
/// skip their operation entirely rather than act on blank text.
pub(super) fn try_message_content_string(message: &ChatCompletionRequestMessage) -> Option<String> {
    let value = serde_json::to_value(message).ok()?;
    Some(message_content_text(&value))
}

pub(super) fn describe_message(message: &ChatCompletionRequestMessage) -> (ContextRole, String) {
    use serde_json::Value;

    let value = serde_json::to_value(message).unwrap_or(Value::Null);
    let role = match value.get("role").and_then(Value::as_str) {
        Some("user") => ContextRole::User,
        Some("assistant") => ContextRole::Assistant,
        Some("tool") => ContextRole::Tool,
        _ => ContextRole::System,
    };

    let mut text = match value.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };

    if let Some(tool_calls) = value.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let args = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !text.is_empty() {
                text.push('\n');
            }
            let _ = write!(text, "→ {name}({args})");
        }
    }

    (role, context_preview(&text))
}

pub(super) fn tool_context_detail(tool_call: &ToolCall, output: &ToolOutput) -> ToolContextDetail {
    let rendered_summary = output.rendered_summary().to_string();
    let result = match output {
        ToolOutput::Command {
            rendered,
            stdout,
            stderr,
            exit_code,
            timed_out,
            truncation,
        } => ToolContextResult::Command {
            rendered: rendered.clone(),
            stdout: stdout.clone(),
            stderr: stderr.clone(),
            exit_code: *exit_code,
            timed_out: *timed_out,
            truncation: truncation.clone(),
        },
        ToolOutput::Text(_)
        | ToolOutput::Read { .. }
        | ToolOutput::TextWithUsage { .. }
        | ToolOutput::WaitStarted { .. } => ToolContextResult::Text {
            rendered: rendered_summary,
        },
        // Both context variants render as plain text in the transcript / `/ctx`
        // / GC snapshot. For untrusted content the frame is part of the rendered
        // text, so it survives compaction and session restore intact.
        ToolOutput::TrustedContext { .. } | ToolOutput::UntrustedContext { .. } => {
            ToolContextResult::Text {
                rendered: rendered_summary,
            }
        }
        ToolOutput::BackgroundTaskStarted { task_id, .. } => {
            ToolContextResult::BackgroundTaskStarted {
                task_id: task_id.clone(),
                message: rendered_summary,
            }
        }
        ToolOutput::SubagentStarted { subtask_id, .. } => ToolContextResult::SubagentStarted {
            subtask_id: subtask_id.clone(),
            message: rendered_summary,
        },
        ToolOutput::Edit { diff, .. } => ToolContextResult::Edit {
            summary: rendered_summary,
            diff_preview: diff_context_preview(diff),
        },
        ToolOutput::Image {
            mime_type,
            base64_data,
            ..
        } => ToolContextResult::Image {
            description: rendered_summary,
            image: ToolImageContext {
                mime_type: mime_type.clone(),
                base64_bytes: base64_data.len(),
            },
        },
    };
    ToolContextDetail {
        call_id: tool_call.id.clone(),
        name: tool_call.name.clone(),
        arguments: compact_tool_arguments_for_context(&tool_call.name, &tool_call.arguments),
        read_evidence: match output {
            ToolOutput::Read { evidence, .. } => Some(evidence.clone()),
            _ => None,
        },
        result,
        reuse_target_call_id: None,
    }
}

pub(super) fn diff_context_preview(diff: &crate::diff::FileDiff) -> String {
    diff.files()
        .map(single_diff_context_preview)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn single_diff_context_preview(diff: &crate::diff::FileDiff) -> String {
    let mut text = format!(
        "{} {:?}\n+{} -{}\nnew size: {} bytes",
        diff.path, diff.status, diff.added_lines, diff.removed_lines, diff.new_size
    );
    if let Some(old_size) = diff.old_size {
        let _ = write!(text, "\nold size: {old_size} bytes");
    }
    if diff.truncated {
        text.push_str("\ntruncated: true");
    }
    for hunk in &diff.hunks {
        let _ = write!(text, "\n@@ -{} +{} @@", hunk.old_start, hunk.new_start);
        for line in &hunk.lines {
            let prefix = match line.kind {
                crate::diff::DiffLineKind::Added => "+",
                crate::diff::DiffLineKind::Removed => "-",
                crate::diff::DiffLineKind::Context => " ",
            };
            text.push('\n');
            text.push_str(prefix);
            text.push_str(&line.content);
        }
    }
    text
}

pub(super) fn background_completion_status(
    tasks: &[crate::background::BackgroundTaskSnapshot],
) -> String {
    match tasks {
        [] => "[background] no completed tasks".to_string(),
        [task] => format!("[background] {} {}", task.id, task.status.label()),
        tasks => {
            let task_ids = tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            if tasks.iter().all(|task| task.status.is_success()) {
                format!("[background] {} tasks succeeded: {task_ids}", tasks.len())
            } else {
                let summary = tasks
                    .iter()
                    .map(|task| format!("{} {}", task.id, task.status.label()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("[background] {} tasks finished: {summary}", tasks.len())
            }
        }
    }
}

pub(super) fn background_completion_message(
    tasks: &[crate::background::BackgroundTaskSnapshot],
) -> String {
    if let [task] = tasks {
        task.completion_report()
    } else {
        let mut message = format!("{} background commands finished.", tasks.len());
        for task in tasks {
            let _ = write!(message, "\n\n--- {} ---\n{}", task.id, task.detail());
        }
        message
    }
}
