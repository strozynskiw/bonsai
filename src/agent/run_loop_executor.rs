//! Helpers that keep the run-loop's tool-batch body small: argument parsing,
//! per-tool completion reporting, and image-result message construction.

use async_openai::types::chat::ChatCompletionRequestUserMessage;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::*;
use crate::tool::Tool;
use crate::tool::arg_repair::{RepairNote, repair_arguments, unwrap_double_encoded_arguments};
use crate::tool::schema::compact_schema_hint;

const TOOL_ARGUMENT_PREVIEW_CHARS: usize = 600;
const TOOL_RESULT_PREVIEW_CHARS: usize = 4_000;
pub(super) const COMMAND_SUMMARY_MARKER: &str = "[Command summary]";

#[derive(Debug, Default)]
pub(super) struct WorktreeSnapshot {
    files: BTreeMap<String, String>,
}

impl WorktreeSnapshot {
    pub(super) fn paths(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    pub(super) fn changed_paths(&self, current: &Self) -> Vec<String> {
        self.files
            .keys()
            .chain(current.files.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|path| self.files.get(*path) != current.files.get(*path))
            .cloned()
            .collect()
    }
}

pub(super) async fn capture_worktree_snapshot(root: &Path) -> Option<WorktreeSnapshot> {
    capture_worktree_snapshot_including(root, &[]).await
}

pub(super) async fn capture_worktree_snapshot_including(
    root: &Path,
    retained_paths: &[String],
) -> Option<WorktreeSnapshot> {
    let commands: [&[&str]; 3] = [
        &["diff", "--name-only", "-z", "HEAD", "--"],
        &["diff", "--cached", "--name-only", "-z", "--"],
        &["ls-files", "--others", "--exclude-standard", "-z"],
    ];
    let mut paths = retained_paths.iter().cloned().collect::<BTreeSet<_>>();
    let mut command_succeeded = false;
    for args in commands {
        let output = tokio::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            continue;
        }
        command_succeeded = true;
        for raw in output.stdout.split(|byte| *byte == 0) {
            if !raw.is_empty() {
                paths.insert(String::from_utf8_lossy(raw).into_owned());
            }
        }
    }
    if !command_succeeded {
        return None;
    }

    let mut files = BTreeMap::new();
    for path in paths {
        let absolute = root.join(&path);
        let fingerprint = match tokio::fs::symlink_metadata(&absolute).await {
            Ok(metadata) if metadata.file_type().is_symlink() => tokio::fs::read_link(&absolute)
                .await
                .map(|target| format!("link:{}", target.to_string_lossy()))
                .unwrap_or_else(|_| "unreadable-link".to_string()),
            Ok(metadata) if metadata.is_file() => match tokio::fs::read(&absolute).await {
                Ok(content) => {
                    let mut hasher = blake3::Hasher::new();
                    hasher.update(&content);
                    hasher.update(&metadata.len().to_le_bytes());
                    format!("file:{}", hasher.finalize())
                }
                Err(_) => "unreadable-file".to_string(),
            },
            Ok(_) => "other".to_string(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => "deleted".to_string(),
            Err(_) => "unreadable".to_string(),
        };
        files.insert(path, fingerprint);
    }
    Some(WorktreeSnapshot { files })
}

/// Parse a tool call's JSON arguments, returning a model-readable error string
/// when they are not a valid JSON object matching the tool's schema. The error
/// names the tool and the required fields so the next turn can self-correct.
///
/// Between parsing and validation sits the [`arg_repair`] pass: schema-invalid
/// shapes that weaker models (DeepSeek-class) predictably emit are repaired in
/// place. The pass is failure-gated, not model-gated — a value that conforms
/// to the schema is never touched, so well-behaved models are unaffected by
/// construction and no model identity needs to reach this layer.
pub(super) fn parse_tool_arguments(
    tool: &dyn Tool,
    name: &str,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    let schema = tool.parameters_schema();
    let mut value = match serde_json::from_str::<Value>(arguments) {
        Ok(value) => value,
        Err(parse_err) => {
            return Err(format_tool_argument_error(
                name,
                &schema,
                arguments,
                "are not valid JSON matching the tool schema",
                &[],
                &[],
                Some(&parse_err.to_string()),
            ));
        }
    };

    // A double-encoded payload (the whole object stringified twice) must
    // unwrap before the object check below would reject it as a string.
    let mut repairs = Vec::new();
    repairs.extend(unwrap_double_encoded_arguments(&mut value));

    if !value.is_object() {
        return Err(format_tool_argument_error(
            name,
            &schema,
            arguments,
            &format!(
                "must be a JSON object matching the tool schema (got {})",
                json_type_name(&value)
            ),
            &[],
            &[],
            None,
        ));
    };

    repairs.extend(repair_arguments(&schema, &mut value));
    repairs.extend(tool.coerce_arguments(&mut value));

    let object = value.as_object().expect("checked to be an object above");
    let missing = missing_required_fields(&schema, object);
    let rejected = rejected_fields(&schema, object);
    if missing.is_empty() && rejected.is_empty() {
        if !repairs.is_empty() {
            tracing::debug!(
                tool = %name,
                repairs = %join_repairs(&repairs),
                "repaired malformed tool-call arguments"
            );
        }
        return Ok(value);
    }

    // Repairs never remove required keys or add keys, so they cannot be the
    // cause of this failure — but naming them (e.g. an unwrapped
    // double-encoded payload) explains why the guidance below talks about an
    // object when the raw payload echoed back was a string.
    Err(format_tool_argument_error_with_repairs(
        name,
        &schema,
        arguments,
        "do not match the tool schema",
        &missing,
        &rejected,
        None,
        &repairs,
    ))
}

fn join_repairs(repairs: &[RepairNote]) -> String {
    repairs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn format_tool_argument_error(
    name: &str,
    schema: &Value,
    arguments: &str,
    reason: &str,
    missing: &[String],
    rejected: &[String],
    parse_error: Option<&str>,
) -> String {
    format_tool_argument_error_with_repairs(
        name,
        schema,
        arguments,
        reason,
        missing,
        rejected,
        parse_error,
        &[],
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "one assembly point for every error part"
)]
fn format_tool_argument_error_with_repairs(
    name: &str,
    schema: &Value,
    arguments: &str,
    reason: &str,
    missing: &[String],
    rejected: &[String],
    parse_error: Option<&str>,
    repairs: &[RepairNote],
) -> String {
    let required = required_fields(schema);
    let mut parts = vec![format!("Error: arguments for '{name}' {reason}.")];
    if !required.is_empty() {
        parts.push(format!("Required fields: {}.", required.join(", ")));
    }
    if !missing.is_empty() {
        parts.push(format!("Missing fields: {}.", missing.join(", ")));
    }
    if !rejected.is_empty() {
        parts.push(format!("Rejected fields: {}.", rejected.join(", ")));
    }
    if let Some(hint) = compact_schema_hint(schema) {
        parts.push(format!("Schema: {hint}."));
    }
    parts.push(format!(
        "Got: {}.",
        compact_argument_payload(arguments, TOOL_ARGUMENT_PREVIEW_CHARS)
    ));
    if let Some(parse_error) = parse_error {
        parts.push(format!("Parse error: {parse_error}."));
    }
    if !repairs.is_empty() {
        parts.push(format!(
            "Attempted repairs before validation: {}.",
            join_repairs(repairs)
        ));
    }
    parts.join(" ")
}

fn missing_required_fields(schema: &Value, object: &Map<String, Value>) -> Vec<String> {
    required_fields(schema)
        .into_iter()
        .filter(|field| !object.contains_key(field))
        .collect()
}

fn rejected_fields(schema: &Value, object: &Map<String, Value>) -> Vec<String> {
    if schema.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
        return Vec::new();
    }
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        let mut rejected = object.keys().cloned().collect::<Vec<_>>();
        rejected.sort_unstable();
        return rejected;
    };
    let mut rejected = object
        .keys()
        .filter(|field| !properties.contains_key(*field))
        .cloned()
        .collect::<Vec<_>>();
    rejected.sort_unstable();
    rejected
}

fn required_fields(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn compact_argument_payload(arguments: &str, max_chars: usize) -> String {
    if arguments.is_empty() {
        "<empty>".to_string()
    } else {
        truncate(arguments, max_chars)
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Report a single tool's completion to the sink from inside the concurrent
/// task, so each tool finishes individually with its own execution time instead
/// of all batch members "finishing" together after `join_all` returns.
pub(super) fn report_tool_completion(
    sink: &SharedSink,
    tool_call: &ToolCall,
    result: &ToolOutput,
    status: crate::output::ToolExecutionStatus,
) {
    match result {
        ToolOutput::BackgroundTaskStarted { message, .. }
        | ToolOutput::SubagentStarted { message, .. } => {
            sink.thinking(&format!("Started {}", tool_call.name));
            sink.tool_output(&tool_call.id, &truncate(message, TOOL_RESULT_PREVIEW_CHARS));
        }
        ToolOutput::Edit { summary, diff } => {
            sink.thinking(&format!("Finished {}", tool_call.name));
            sink.tool_finished_with_diff(
                &tool_call.id,
                &truncate(summary, TOOL_RESULT_PREVIEW_CHARS),
                status,
                diff.clone(),
            );
        }
        ToolOutput::Image { description, .. } => {
            sink.thinking(&format!("Finished {}", tool_call.name));
            sink.tool_finished(&tool_call.id, description, status);
        }
        ToolOutput::Command { rendered, .. } => {
            sink.thinking(&format!("Finished {}", tool_call.name));
            sink.tool_finished(
                &tool_call.id,
                &truncate_command_result(rendered, TOOL_RESULT_PREVIEW_CHARS),
                status,
            );
        }
        _ => {
            sink.thinking(&format!("Finished {}", tool_call.name));
            sink.tool_finished(
                &tool_call.id,
                &truncate(result.rendered_summary(), TOOL_RESULT_PREVIEW_CHARS),
                status,
            );
        }
    }
}

fn truncate_command_result(rendered: &str, max_chars: usize) -> String {
    let char_count = rendered.chars().count();
    if char_count <= max_chars {
        return rendered.to_string();
    }
    let Some(footer_start) = command_summary_footer_start(rendered) else {
        return truncate(rendered, max_chars);
    };

    let footer = &rendered[footer_start..];
    let footer_chars = footer.chars().count();
    if footer_chars >= max_chars {
        return footer.to_string();
    }

    let notice = format!("... ({char_count} chars total, command summary preserved below)");
    let fixed_chars = footer_chars
        .saturating_add(notice.chars().count())
        .saturating_add(4);
    if fixed_chars >= max_chars {
        return footer.to_string();
    }

    let preview_chars = max_chars - fixed_chars;
    let preview = rendered[..footer_start]
        .trim_end()
        .chars()
        .take(preview_chars)
        .collect::<String>();
    format!("{preview}\n\n{notice}\n\n{footer}")
}

fn command_summary_footer_start(text: &str) -> Option<usize> {
    let mut search_end = text.len();
    while let Some(index) = text[..search_end].rfind(COMMAND_SUMMARY_MARKER) {
        let summary = text[index + COMMAND_SUMMARY_MARKER.len()..].trim_start_matches(['\r', '\n']);
        if has_command_summary_fields(summary) {
            return Some(index);
        }
        search_end = index;
    }
    None
}

fn has_command_summary_fields(summary: &str) -> bool {
    let mut lines = summary.lines().map(str::trim);
    let Some(command) = lines.next() else {
        return false;
    };
    let Some(exit_code) = lines.next() else {
        return false;
    };
    let mut next = lines.next();

    if next.is_some_and(|line| line.starts_with("signal: ")) {
        next = lines.next();
    }
    let Some(timed_out) = next else {
        return false;
    };
    next = lines.next();
    if next.is_some_and(|line| line.starts_with("timeout_seconds: ")) {
        next = lines.next();
    }
    let Some(duration) = next else {
        return false;
    };
    let Some(stdout_bytes) = lines.next() else {
        return false;
    };
    let Some(stderr_bytes) = lines.next() else {
        return false;
    };
    let Some(combined_output_chars) = lines.next() else {
        return false;
    };
    next = lines.next();
    if next.is_some_and(|line| line.starts_with("saved_output: ")) {
        next = lines.next();
    }
    matches!(
        (
            command.starts_with("command: "),
            exit_code.starts_with("exit_code: "),
            timed_out.starts_with("timed_out: "),
            duration.starts_with("duration: "),
            stdout_bytes.starts_with("stdout_bytes: "),
            stderr_bytes.starts_with("stderr_bytes: "),
            combined_output_chars.starts_with("combined_output_chars: "),
            next,
        ),
        (
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            Some("last_output:")
        )
    )
}

/// Synthetic tool results for a batch dropped (or never started) by mid-batch
/// cancellation. The assistant message already references every tool-call id, so
/// each one still needs a reply or the next request is malformed; the UI's
/// active tools are cleared wholesale on interrupt, so no per-tool finish is
/// emitted here.
pub(super) fn interrupted_tool_results(
    batch: Vec<ToolCall>,
) -> Vec<(ToolCall, ToolOutput, crate::output::ToolExecutionStatus)> {
    batch
        .into_iter()
        .map(|tool_call| {
            (
                tool_call,
                ToolOutput::Text("Error: tool interrupted before completion.".to_string()),
                crate::output::ToolExecutionStatus::Interrupted,
            )
        })
        .collect()
}

pub(super) fn skipped_tool_results(
    calls: Vec<ToolCall>,
    message: &str,
) -> Vec<(ToolCall, ToolOutput, crate::output::ToolExecutionStatus)> {
    calls
        .into_iter()
        .map(|call| {
            (
                call,
                ToolOutput::Text(message.to_string()),
                crate::output::ToolExecutionStatus::Skipped,
            )
        })
        .collect()
}

/// Build the synthetic user message that carries an image tool result back to
/// the model as a base64 data URI.
pub(super) fn image_user_message(
    mime_type: &str,
    base64_data: &str,
) -> ChatCompletionRequestMessage {
    let data_uri = format!("data:{mime_type};base64,{base64_data}");
    ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content: ChatCompletionRequestUserMessageContent::Array(vec![
            ChatCompletionRequestUserMessageContentPart::Text(
                ChatCompletionRequestMessageContentPartText {
                    text: "Here is the image content you requested to view:".to_string(),
                },
            ),
            ChatCompletionRequestUserMessageContentPart::ImageUrl(
                ChatCompletionRequestMessageContentPartImage {
                    image_url: ImageUrl {
                        url: data_uri,
                        detail: None,
                    },
                },
            ),
        ]),
        name: None,
    })
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::output::{OutputSink, SharedSink};
    use crate::tool::ToolOutput;
    use crate::tool::schema::{
        array_property, bounded_integer_property, closed_object, string_property,
    };

    /// Stub tool with a schema rich enough to exercise every repair rule
    /// through the real `parse_tool_arguments` path.
    struct RepairableTool;

    #[async_trait]
    impl Tool for RepairableTool {
        fn name(&self) -> &str {
            "repairable"
        }

        fn description(&self) -> &str {
            "test stub"
        }

        fn parameters_schema(&self) -> Value {
            closed_object(
                [
                    (
                        "files",
                        array_property("Files", string_property("File path")),
                    ),
                    ("count", bounded_integer_property("Count", Some(1), None)),
                    ("note", string_property("Optional note")),
                ],
                &["files"],
            )
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolOutput> {
            unreachable!("parse-only tests never execute the tool")
        }
    }

    #[test]
    fn parse_tool_arguments_valid_args_are_returned_identical() {
        let args = r#"{"files": ["a.rs", "b.rs"], "count": 2}"#;

        let value = parse_tool_arguments(&RepairableTool, "repairable", args)
            .expect("valid arguments should parse");

        let direct = serde_json::from_str::<Value>(args).expect("test input is valid JSON");
        assert_eq!(value, direct, "conformant arguments must be untouched");
    }

    #[test]
    fn parse_tool_arguments_repairs_deepseek_shaped_payload() {
        // The three headline DeepSeek shapes in one call: bare string for an
        // array, quoted integer, and null for an optional field.
        let args = r#"{"files": "src/main.rs", "count": "5", "note": null}"#;

        let value = parse_tool_arguments(&RepairableTool, "repairable", args)
            .expect("repairable arguments should parse after repair");

        assert_eq!(value, json!({"files": ["src/main.rs"], "count": 5}));
    }

    #[test]
    fn parse_tool_arguments_unwraps_double_encoded_payload() {
        let args = r#""{\"files\": [\"a.rs\"]}""#;

        let value = parse_tool_arguments(&RepairableTool, "repairable", args)
            .expect("double-encoded object should unwrap and parse");

        assert_eq!(value, json!({"files": ["a.rs"]}));
    }

    #[test]
    fn parse_tool_arguments_error_mentions_attempted_repairs() {
        // Unwraps to an object, but the required `files` field is absent —
        // the guidance must note the unwrap so "Got: <a string>" makes sense.
        let args = r#""{\"count\": 5}""#;

        let message = parse_tool_arguments(&RepairableTool, "repairable", args)
            .expect_err("missing required field should still fail");

        assert!(message.contains("Missing fields: files"), "{message}");
        assert!(
            message.contains("Attempted repairs before validation:"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn parse_tool_arguments_maps_read_line_range_aliases() {
        // Exact shape observed in the tool-call log: `start_line`/`end_line`
        // (as quoted numbers) imported from the `read_region` convention,
        // rejected by `read`'s closed schema before the per-tool coercion
        // hook existed.
        let fixture = crate::tool::test_utils::TestFixture::new();
        let tool =
            crate::tool::ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let args = r#"{"end_line": "120", "path": "src/tui/event.rs", "start_line": "32"}"#;

        let value = parse_tool_arguments(&tool, "read", args)
            .expect("line-range aliases should coerce onto offset/limit");

        assert_eq!(
            value,
            json!({"path": "src/tui/event.rs", "offset": 32, "limit": 89})
        );
    }

    #[test]
    fn parse_tool_arguments_plain_string_payload_still_rejected() {
        let message = parse_tool_arguments(&RepairableTool, "repairable", r#""not json""#)
            .expect_err("a non-object payload should be rejected");

        assert!(message.contains("must be a JSON object"), "{message}");
        assert!(
            !message.contains("Attempted repairs"),
            "no repair fired, none should be reported: {message}"
        );
    }

    #[derive(Default)]
    struct FinishedSink {
        results: std::sync::Mutex<Vec<String>>,
    }

    impl FinishedSink {
        fn result(&self) -> String {
            self.results
                .lock()
                .expect("finished sink mutex should not be poisoned")
                .last()
                .cloned()
                .expect("tool completion should be captured")
        }
    }

    impl OutputSink for FinishedSink {
        fn tool_finished(
            &self,
            _id: &str,
            result: &str,
            _status: crate::output::ToolExecutionStatus,
        ) {
            self.results
                .lock()
                .expect("finished sink mutex should not be poisoned")
                .push(result.to_string());
        }
    }

    fn command_summary_footer() -> String {
        [
            "[Command summary]",
            "command: printf long-output",
            "exit_code: 0",
            "timed_out: false",
            "duration: 120ms",
            "stdout_bytes: 5000",
            "stderr_bytes: 0",
            "combined_output_chars: 5000",
            "last_output:",
            "tail",
        ]
        .join("\n")
    }

    #[test]
    fn command_completion_truncation_preserves_summary_footer() {
        let sink = std::sync::Arc::new(FinishedSink::default());
        let shared_sink: SharedSink = sink.clone();
        let body = "x".repeat(4_500);
        let rendered = format!("{body}\n\n{}", command_summary_footer());
        let output = ToolOutput::Command {
            rendered,
            stdout: body,
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
            truncation: None,
        };
        let tool_call = ToolCall {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
        };

        report_tool_completion(
            &shared_sink,
            &tool_call,
            &output,
            crate::output::ToolExecutionStatus::Succeeded,
        );

        let result = sink.result();
        assert!(
            result.contains("command summary preserved below"),
            "large command output should be body-truncated with footer preserved: {result}"
        );
        assert!(
            result.contains("[Command summary]\ncommand: printf long-output\nexit_code: 0"),
            "footer should remain parseable after completion truncation: {result}"
        );
        assert!(
            result.ends_with("last_output:\ntail"),
            "footer should remain the final section: {result}"
        );
    }
}
