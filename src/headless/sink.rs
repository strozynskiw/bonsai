use std::io::{self, Write};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::Result;
use serde::Serialize;

use super::*;
use crate::output::OutputSink;

type SharedWriter = Arc<StdMutex<Box<dyn Write + Send>>>;

/// Machine-readable headless output schema. The 1.x line keeps version 1
/// backward compatible; a breaking payload change must increment this value.
const HEADLESS_OUTPUT_SCHEMA_VERSION: u32 = 1;

pub(crate) struct HeadlessSink {
    format: OutputFormat,
    stdout: SharedWriter,
    stderr: SharedWriter,
    assistant_output: StdMutex<String>,
    assistant_line_open: StdMutex<bool>,
    write_error: StdMutex<Option<String>>,
    recorder: TranscriptRecorder,
    completion_evidence: crate::completion_report::CompletionEvidenceCollector,
}

impl HeadlessSink {
    pub(crate) fn stdio(format: OutputFormat) -> Self {
        Self::with_writers(format, Box::new(io::stdout()), Box::new(io::stderr()))
    }

    #[cfg(test)]
    pub(crate) fn new(
        format: OutputFormat,
        stdout: Box<dyn Write + Send>,
        stderr: Box<dyn Write + Send>,
    ) -> Self {
        Self::with_writers(format, stdout, stderr)
    }

    fn with_writers(
        format: OutputFormat,
        stdout: Box<dyn Write + Send>,
        stderr: Box<dyn Write + Send>,
    ) -> Self {
        Self {
            format,
            stdout: Arc::new(StdMutex::new(stdout)),
            stderr: Arc::new(StdMutex::new(stderr)),
            assistant_output: StdMutex::new(String::new()),
            assistant_line_open: StdMutex::new(false),
            write_error: StdMutex::new(None),
            recorder: TranscriptRecorder::default(),
            completion_evidence: crate::completion_report::CompletionEvidenceCollector::default(),
        }
    }

    pub(crate) fn user_message(&self, text: &str) {
        self.recorder.user_message(text);
    }

    pub(crate) fn take_transcript(&self) -> Vec<crate::tui::app::TranscriptItem> {
        self.recorder.take()
    }

    pub(crate) fn finish(&self, output: &HeadlessFinalOutput) -> Result<()> {
        match self.format {
            OutputFormat::Text => {
                let assistant_output = self
                    .assistant_output
                    .lock()
                    .map(|value| value.clone())
                    .unwrap_or_default();
                if assistant_output.is_empty() && !output.output.is_empty() {
                    self.write_stdout(&output.output);
                    if !output.output.ends_with('\n') {
                        self.write_stdout("\n");
                    }
                } else if self
                    .assistant_line_open
                    .lock()
                    .map(|value| *value)
                    .unwrap_or(false)
                {
                    self.write_stdout("\n");
                }
                if let Ok(mut line_open) = self.assistant_line_open.lock() {
                    *line_open = false;
                }
                self.write_stderr(&format!("{}\n", output.completion_report.render_compact()));
                self.write_stderr(&format!(
                    "Lifecycle: {} · Task: {}\n",
                    output.session_lifecycle,
                    output.task_outcome.label()
                ));
                if let Some(reason) = output.task_terminal_reason.as_ref() {
                    self.write_stderr(&format!(
                        "Task reason: {} · {}\n",
                        reason.code.as_db_str(),
                        reason.detail
                    ));
                }
                self.write_stderr(&format!("Resume: bonsai -c {}\n", output.session_id));
            }
            OutputFormat::Json => {
                self.write_versioned_json_stdout(output)?;
            }
            OutputFormat::StreamJson => {
                self.write_stream_event(&StreamFinalEvent {
                    event_type: "final",
                    final_output: output,
                })?;
            }
        }
        self.finish_pending_writes()
    }

    pub(crate) fn finish_pending_writes(&self) -> Result<()> {
        if let Some(error) = self.take_write_error() {
            anyhow::bail!("{error}");
        }
        Ok(())
    }

    fn write_stdout(&self, text: &str) {
        self.write_to(&self.stdout, text);
    }

    fn write_stderr(&self, text: &str) {
        self.write_to(&self.stderr, text);
    }

    fn write_to(&self, writer: &SharedWriter, text: &str) {
        match writer.lock() {
            Ok(mut writer) => {
                if let Err(err) = writer
                    .write_all(text.as_bytes())
                    .and_then(|()| writer.flush())
                {
                    self.remember_write_error(err.to_string());
                }
            }
            Err(err) => self.remember_write_error(format!("output lock poisoned: {err}")),
        }
    }

    fn write_json_stdout<T: Serialize>(&self, value: &T) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        self.write_stdout(&line);
        Ok(())
    }

    fn write_versioned_json_stdout<T: Serialize>(&self, value: &T) -> Result<()> {
        self.write_json_stdout(&VersionedJson {
            schema_version: HEADLESS_OUTPUT_SCHEMA_VERSION,
            payload: value,
        })
    }

    fn write_stream_event<T: Serialize>(&self, event: &T) -> Result<()> {
        if self.format == OutputFormat::StreamJson
            && let Err(err) = self.write_versioned_json_stdout(event)
        {
            tracing::error!(%err, "headless stream event serialization failed");
            return Err(err);
        }
        Ok(())
    }

    fn emit_progress(&self, text: &str) {
        match self.format {
            OutputFormat::Text | OutputFormat::Json => {
                self.write_stderr(&format!("{text}\n"));
            }
            OutputFormat::StreamJson => {
                let _ = self.write_stream_event(&StreamTextEvent {
                    event_type: "status",
                    text,
                });
            }
        }
    }

    fn emit_tool_progress(&self, text: &str) {
        if matches!(self.format, OutputFormat::Text | OutputFormat::Json) {
            self.write_stderr(&format!("{text}\n"));
        }
    }

    pub(crate) fn completion_evidence(
        &self,
    ) -> crate::completion_report::CompletionEvidenceSnapshot {
        self.completion_evidence.snapshot()
    }

    pub(crate) fn begin_completion_run(&self) {
        self.completion_evidence.begin_run();
    }

    pub(crate) fn record_completion_report(
        &self,
        report: &crate::completion_report::CompletionReport,
    ) {
        self.recorder.status(&report.render_compact());
    }

    fn remember_write_error(&self, message: String) {
        if let Ok(mut slot) = self.write_error.lock()
            && slot.is_none()
        {
            *slot = Some(message);
        }
    }

    fn take_write_error(&self) -> Option<String> {
        self.write_error
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
    }
}

#[derive(Serialize)]
struct VersionedJson<'a, T> {
    schema_version: u32,
    #[serde(flatten)]
    payload: &'a T,
}

impl OutputSink for HeadlessSink {
    fn begin_completion_run(&self) {
        self.completion_evidence.begin_run();
    }

    fn completion_evidence(&self) -> Option<crate::completion_report::CompletionEvidenceSnapshot> {
        Some(self.completion_evidence.snapshot())
    }

    fn assistant_delta(&self, text: &str) {
        self.recorder.assistant_delta(text);
        if let Ok(mut output) = self.assistant_output.lock() {
            output.push_str(text);
        }
        if !text.is_empty()
            && let Ok(mut line_open) = self.assistant_line_open.lock()
        {
            *line_open = true;
        }
        match self.format {
            OutputFormat::Text => self.write_stdout(text),
            OutputFormat::Json => {}
            OutputFormat::StreamJson => {
                let _ = self.write_stream_event(&StreamTextEvent {
                    event_type: "assistant_delta",
                    text,
                });
            }
        }
    }

    fn assistant_done(&self) {
        self.recorder.assistant_done();
        let had_open_line = self
            .assistant_line_open
            .lock()
            .map(|value| *value)
            .unwrap_or(false);
        if self.format == OutputFormat::Text && had_open_line {
            self.write_stdout("\n");
        }
        if let Ok(mut line_open) = self.assistant_line_open.lock() {
            *line_open = false;
        }
    }

    fn reasoning_delta(&self, text: &str) {
        self.recorder.reasoning_delta(text);
        match self.format {
            OutputFormat::Text | OutputFormat::Json => {
                self.write_stderr(&format!("[reasoning] {text}\n"));
            }
            OutputFormat::StreamJson => {
                let _ = self.write_stream_event(&StreamTextEvent {
                    event_type: "reasoning_delta",
                    text,
                });
            }
        }
    }

    fn attempt_started(&self) {
        self.recorder.attempt_started();
    }

    fn attempt_discarded(&self) {
        self.recorder.attempt_discarded();
        match self.format {
            // Reasoning already went to stderr line-by-line and cannot be
            // unprinted; the note is how a reader knows those lines were
            // abandoned rather than part of the final turn.
            OutputFormat::Text | OutputFormat::Json => {
                self.write_stderr("[retry] discarded streamed output from a failed attempt\n");
            }
            OutputFormat::StreamJson => {
                let _ = self.write_stream_event(&StreamMarkerEvent {
                    event_type: "attempt_discarded",
                });
            }
        }
    }

    fn thinking(&self, text: &str) {
        self.emit_progress(text);
    }

    fn tool_started(&self, id: &str, name: &str, arguments: &str) {
        self.recorder.tool_started(id, name, arguments);
        self.completion_evidence
            .record_tool_started(id, name, arguments);
        self.emit_tool_progress(&format!("[{name}:{id} running] {arguments}"));
        let _ = self.write_stream_event(&StreamToolStartedEvent {
            event_type: "tool_started",
            id,
            name,
            arguments,
        });
    }

    fn tool_output(&self, id: &str, output: &str) {
        self.emit_tool_progress(&format!("[tool:{id} output] {output}"));
        let _ = self.write_stream_event(&StreamToolOutputEvent {
            event_type: "tool_output",
            id,
            output,
        });
    }

    fn tool_finished(&self, id: &str, result: &str, status: crate::output::ToolExecutionStatus) {
        self.recorder.tool_finished(id, result, status, None);
        self.completion_evidence
            .record_tool_result(id, result, status, None);
        let state = status.label();
        self.emit_tool_progress(&format!("[tool:{id} {state}] {result}"));
        let _ = self.write_stream_event(&StreamToolFinishedEvent {
            event_type: "tool_finished",
            id,
            result,
            status,
            diff: None,
        });
    }

    fn tool_finished_with_diff(
        &self,
        id: &str,
        result: &str,
        status: crate::output::ToolExecutionStatus,
        diff: crate::diff::FileDiff,
    ) {
        self.recorder
            .tool_finished(id, result, status, Some(diff.clone()));
        self.completion_evidence
            .record_tool_result(id, result, status, Some(&diff));
        let state = status.label();
        self.emit_tool_progress(&format!("[tool:{id} {state}] {result}"));
        self.emit_tool_progress(&format!("[diff:{} ready]", diff.path));
        let _ = self.write_stream_event(&StreamToolFinishedEvent {
            event_type: "tool_finished",
            id,
            result,
            status,
            diff: Some(&diff),
        });
    }

    fn workspace_changed(&self, paths: &[String], intent: &str) {
        self.completion_evidence
            .record_workspace_changes(paths, intent);
    }

    fn status(&self, text: &str) {
        self.emit_progress(text);
    }

    fn context_updated(&self, report: crate::agent::ContextReport) {
        let _ = self.write_stream_event(&StreamContextEvent::from_report(&report));
    }

    fn error(&self, text: &str) {
        match self.format {
            OutputFormat::Text | OutputFormat::Json => {
                self.write_stderr(&format!("{text}\n"));
            }
            OutputFormat::StreamJson => {
                let _ = self.write_stream_event(&StreamTextEvent {
                    event_type: "error",
                    text,
                });
            }
        }
    }
}

#[derive(Serialize)]
struct StreamTextEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    text: &'a str,
}

/// A payload-free lifecycle marker (e.g. `attempt_discarded`, which tells a
/// stream consumer that the reasoning/assistant deltas since the failed
/// attempt began are not part of the final turn).
#[derive(Serialize)]
struct StreamMarkerEvent {
    #[serde(rename = "type")]
    event_type: &'static str,
}

#[derive(Serialize)]
struct StreamToolStartedEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    id: &'a str,
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct StreamToolOutputEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    id: &'a str,
    output: &'a str,
}

#[derive(Serialize)]
struct StreamToolFinishedEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    id: &'a str,
    result: &'a str,
    status: crate::output::ToolExecutionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<&'a crate::diff::FileDiff>,
}

#[derive(Serialize)]
struct StreamContextEvent {
    #[serde(rename = "type")]
    event_type: &'static str,
    used_tokens: usize,
    budget_tokens: usize,
    last_prompt_tokens: Option<u32>,
    last_completion_tokens: Option<u32>,
    session_prompt_tokens: u64,
    session_completion_tokens: u64,
    session_cost_micros: Option<u64>,
    last_turn_cost_micros: Option<u64>,
}

impl StreamContextEvent {
    fn from_report(report: &crate::agent::ContextReport) -> Self {
        Self {
            event_type: "context",
            used_tokens: report.used_tokens(),
            budget_tokens: report.budget_tokens,
            last_prompt_tokens: report.last_prompt_tokens,
            last_completion_tokens: report.last_completion_tokens,
            session_prompt_tokens: report.session_prompt_tokens,
            session_completion_tokens: report.session_completion_tokens,
            session_cost_micros: report.session_cost_micros,
            last_turn_cost_micros: report.last_turn_cost_micros,
        }
    }
}

#[derive(Serialize)]
struct StreamFinalEvent<'a> {
    #[serde(rename = "type")]
    event_type: &'static str,
    #[serde(flatten)]
    final_output: &'a HeadlessFinalOutput,
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    #[derive(Clone)]
    struct BufferWriter {
        buffer: Arc<StdMutex<Vec<u8>>>,
    }

    impl BufferWriter {
        fn new() -> (Self, Arc<StdMutex<Vec<u8>>>) {
            let buffer = Arc::new(StdMutex::new(Vec::new()));
            (
                Self {
                    buffer: buffer.clone(),
                },
                buffer,
            )
        }
    }

    impl Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self.buffer.lock() {
                Ok(mut buffer) => {
                    buffer.extend_from_slice(buf);
                    Ok(buf.len())
                }
                Err(_) => Err(io::Error::other("buffer lock poisoned")),
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn read_buffer(buffer: &Arc<StdMutex<Vec<u8>>>) -> String {
        let bytes = buffer.lock().expect("buffer lock").clone();
        String::from_utf8(bytes).expect("utf8")
    }

    fn sample_final() -> HeadlessFinalOutput {
        HeadlessFinalOutput {
            status: HeadlessStatus::Completed,
            session_lifecycle: "completed".to_string(),
            task_outcome: crate::storage::TaskOutcome::Succeeded,
            task_terminal_reason: None,
            output: "done".to_string(),
            provider: "provider-a".to_string(),
            model: "model-a".to_string(),
            session_id: 42,
            usage: HeadlessUsage::from_totals(crate::agent::UsageTotals {
                prompt_tokens: 10,
                completion_tokens: 5,
                cost_micros: Some(25),
                no_cache_cost_micros: Some(25),
                input_cache: Some(crate::provider::InputCacheUsage::new(6, 1, 10)),
            }),
            persistence_duration_ms: 7,
            budget_exhaustion: None,
            verification: None,
            completion_report: crate::completion_report::CompletionReport::from_evidence(
                crate::completion_report::CompletionStatus::Completed,
                crate::completion_report::CompletionEvidenceSnapshot::default(),
                crate::completion_report::CompletionSessionEvidence {
                    completion_guard: None,
                    verification: None,
                    review: None,
                    authorization_decisions: &[],
                    usage: crate::agent::UsageTotals {
                        prompt_tokens: 10,
                        completion_tokens: 5,
                        cost_micros: Some(25),
                        no_cache_cost_micros: Some(25),
                        input_cache: None,
                    },
                    session_budget: crate::run_budget::SessionBudgetUsage::default(),
                    budget_exhaustion: None,
                },
            ),
        }
    }

    #[test]
    fn final_json_output_is_valid() {
        let (stdout, stdout_buffer) = BufferWriter::new();
        let (stderr, stderr_buffer) = BufferWriter::new();
        let sink = HeadlessSink::new(OutputFormat::Json, Box::new(stdout), Box::new(stderr));

        sink.finish(&sample_final()).unwrap();

        let value: Value = serde_json::from_str(&read_buffer(&stdout_buffer)).unwrap();
        assert_eq!(value["status"], "completed");
        assert_eq!(value["session_lifecycle"], "completed");
        assert_eq!(value["task_outcome"], "succeeded");
        assert!(value.get("task_terminal_reason").is_none());
        assert_eq!(value["schema_version"], HEADLESS_OUTPUT_SCHEMA_VERSION);
        assert_eq!(value["output"], "done");
        assert_eq!(value["session_id"], 42);
        assert_eq!(value["usage"]["total_tokens"], 15);
        assert_eq!(value["usage"]["input_cache"]["read_tokens"], 6);
        assert_eq!(value["usage"]["input_cache"]["hit_rate_percent"], 600);
        assert_eq!(value["persistence_duration_ms"], 7);
        assert_eq!(value["completion_report"]["status"], "completed");
        assert!(read_buffer(&stderr_buffer).is_empty());
    }

    #[test]
    fn final_json_distinguishes_budget_exhaustion() {
        let (stdout, stdout_buffer) = BufferWriter::new();
        let (stderr, stderr_buffer) = BufferWriter::new();
        let sink = HeadlessSink::new(OutputFormat::Json, Box::new(stdout), Box::new(stderr));
        let mut output = sample_final();
        output.status = HeadlessStatus::BudgetExhausted;
        output.session_lifecycle = "interrupted".to_string();
        output.task_outcome = crate::storage::TaskOutcome::Blocked;
        output.task_terminal_reason = Some(crate::storage::TaskTerminalReason::new(
            crate::storage::TaskTerminalReasonCode::BudgetExhausted,
            "Run-time budget exhausted.",
        ));
        output.budget_exhaustion =
            Some(crate::run_budget::RunBudgetExhaustion::RunTime { limit_seconds: 300 });
        output.completion_report = crate::completion_report::CompletionReport::from_evidence(
            crate::completion_report::CompletionStatus::BudgetExhausted,
            crate::completion_report::CompletionEvidenceSnapshot::default(),
            crate::completion_report::CompletionSessionEvidence {
                completion_guard: None,
                verification: None,
                review: None,
                authorization_decisions: &[],
                usage: crate::agent::UsageTotals::default(),
                session_budget: crate::run_budget::SessionBudgetUsage::default(),
                budget_exhaustion: output.budget_exhaustion,
            },
        );

        sink.finish(&output).unwrap();

        let value: Value = serde_json::from_str(&read_buffer(&stdout_buffer)).unwrap();
        assert_eq!(value["status"], "budget_exhausted");
        assert_eq!(value["session_lifecycle"], "interrupted");
        assert_eq!(value["task_outcome"], "blocked");
        assert_eq!(value["task_terminal_reason"]["code"], "budget_exhausted");
        assert_eq!(value["budget_exhaustion"]["kind"], "run_time");
        assert_eq!(value["budget_exhaustion"]["limit_seconds"], 300);
        assert_eq!(
            value["completion_report"]["usage"]["budget_exhaustion"]["kind"],
            "run_time"
        );
        assert!(read_buffer(&stderr_buffer).is_empty());
    }

    #[test]
    fn stream_json_output_is_newline_delimited_events() {
        let (stdout, stdout_buffer) = BufferWriter::new();
        let (stderr, _stderr_buffer) = BufferWriter::new();
        let sink = HeadlessSink::new(OutputFormat::StreamJson, Box::new(stdout), Box::new(stderr));

        sink.assistant_delta("hi");
        sink.status("working");
        sink.tool_started("call-1", "read", r#"{"path":"README.md"}"#);
        sink.finish(&sample_final()).unwrap();

        let output = read_buffer(&stdout_buffer);
        let events = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 4);
        assert!(events.iter().all(|event| {
            event["schema_version"] == serde_json::json!(HEADLESS_OUTPUT_SCHEMA_VERSION)
        }));
        assert_eq!(events[0]["type"], "assistant_delta");
        assert_eq!(events[1]["type"], "status");
        assert_eq!(events[2]["type"], "tool_started");
        assert_eq!(events[3]["type"], "final");
    }

    #[test]
    fn stream_json_error_output_is_newline_delimited_json() {
        let (stdout, stdout_buffer) = BufferWriter::new();
        let (stderr, stderr_buffer) = BufferWriter::new();
        let sink = HeadlessSink::new(OutputFormat::StreamJson, Box::new(stdout), Box::new(stderr));

        sink.error("provider missing");
        sink.finish_pending_writes().unwrap();

        let output = read_buffer(&stdout_buffer);
        let events = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["schema_version"], HEADLESS_OUTPUT_SCHEMA_VERSION);
        assert_eq!(events[0]["type"], "error");
        assert_eq!(events[0]["text"], "provider missing");
        assert!(read_buffer(&stderr_buffer).is_empty());
    }

    #[test]
    fn text_output_keeps_assistant_on_stdout_and_progress_on_stderr() {
        let (stdout, stdout_buffer) = BufferWriter::new();
        let (stderr, stderr_buffer) = BufferWriter::new();
        let sink = HeadlessSink::new(OutputFormat::Text, Box::new(stdout), Box::new(stderr));

        sink.assistant_delta("hello");
        sink.status("working");
        sink.tool_finished(
            "call-1",
            "ok",
            crate::output::ToolExecutionStatus::Succeeded,
        );
        sink.finish(&sample_final()).unwrap();

        let stdout = read_buffer(&stdout_buffer);
        let stderr = read_buffer(&stderr_buffer);
        assert_eq!(stdout, "hello\n");
        assert!(stderr.contains("working"));
        assert!(stderr.contains("[tool:call-1 ok] ok"));
        assert!(stderr.contains("Lifecycle: completed · Task: succeeded"));
        assert!(stderr.contains("Resume: bonsai -c 42"));
    }

    #[test]
    fn text_output_surfaces_the_typed_task_reason() {
        let (stdout, _stdout_buffer) = BufferWriter::new();
        let (stderr, stderr_buffer) = BufferWriter::new();
        let sink = HeadlessSink::new(OutputFormat::Text, Box::new(stdout), Box::new(stderr));
        let mut output = sample_final();
        output.session_lifecycle = "interrupted".to_string();
        output.task_outcome = crate::storage::TaskOutcome::Blocked;
        output.task_terminal_reason = Some(crate::storage::TaskTerminalReason::new(
            crate::storage::TaskTerminalReasonCode::BudgetExhausted,
            "Run-time budget exhausted.",
        ));

        sink.finish(&output).unwrap();

        let stderr = read_buffer(&stderr_buffer);
        assert!(stderr.contains("Lifecycle: interrupted · Task: blocked"));
        assert!(stderr.contains("Task reason: budget_exhausted · Run-time budget exhausted."));
    }

    #[test]
    fn stream_json_context_event_is_compact_usage_projection() {
        let (stdout, stdout_buffer) = BufferWriter::new();
        let (stderr, _stderr_buffer) = BufferWriter::new();
        let sink = HeadlessSink::new(OutputFormat::StreamJson, Box::new(stdout), Box::new(stderr));

        sink.context_updated(crate::agent::ContextReport {
            budget_tokens: 100,
            prompt_estimate_tokens: 25,
            last_prompt_tokens: Some(20),
            last_completion_tokens: Some(5),
            session_prompt_tokens: 70,
            session_completion_tokens: 9,
            session_cost_micros: Some(123),
            last_turn_cost_micros: Some(45),
            ..Default::default()
        });

        let output = read_buffer(&stdout_buffer);
        let value: Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(value["type"], "context");
        assert_eq!(value["used_tokens"], 25);
        assert_eq!(value["budget_tokens"], 100);
        assert_eq!(value["session_cost_micros"], 123);
    }

    #[test]
    fn repeated_tool_failure_classifies_completed_run_as_failed() {
        let (stdout, _stdout_buffer) = BufferWriter::new();
        let (stderr, _stderr_buffer) = BufferWriter::new();
        let sink = HeadlessSink::new(OutputFormat::Json, Box::new(stdout), Box::new(stderr));

        sink.begin_completion_run();
        // One failure is adaptive noise and keeps the run Completed; the same
        // invocation failing three times is a detected loop and fails it.
        sink.tool_started("call-1", "bash", r#"{"command":"cargo test"}"#);
        sink.tool_finished(
            "call-1",
            "Error: denied",
            crate::output::ToolExecutionStatus::Failed,
        );
        let noisy =
            classify_headless_status(HeadlessStatus::Completed, &sink.completion_evidence(), None);
        assert_eq!(noisy, HeadlessStatus::Completed);

        for call in ["call-2", "call-3"] {
            sink.tool_started(call, "bash", r#"{"command":"cargo test"}"#);
            sink.tool_finished(
                call,
                "Error: denied",
                crate::output::ToolExecutionStatus::Failed,
            );
        }
        let status =
            classify_headless_status(HeadlessStatus::Completed, &sink.completion_evidence(), None);
        let outcome =
            HeadlessRunOutcome::from_status(status, crate::storage::SessionId::from_raw(1));
        assert_eq!(status, HeadlessStatus::Failed);
        assert_eq!(outcome.exit_code(), 1);
    }

    #[test]
    fn repaired_bash_failure_completes_without_erasing_attempt_history() {
        let (stdout, _stdout_buffer) = BufferWriter::new();
        let (stderr, _stderr_buffer) = BufferWriter::new();
        let sink = HeadlessSink::new(OutputFormat::Json, Box::new(stdout), Box::new(stderr));

        sink.begin_completion_run();
        sink.tool_started(
            "call-1",
            "bash",
            r#"{"command":"cargo test","timeout_ms":1000}"#,
        );
        sink.tool_finished(
            "call-1",
            "tests failed",
            crate::output::ToolExecutionStatus::Failed,
        );
        sink.tool_started(
            "call-2",
            "bash",
            r#"{"timeout_ms":5000,"command":"cargo test"}"#,
        );
        sink.tool_finished(
            "call-2",
            "tests passed",
            crate::output::ToolExecutionStatus::Succeeded,
        );

        let evidence = sink.completion_evidence();
        let status = classify_headless_status(HeadlessStatus::Completed, &evidence, None);
        let outcome =
            HeadlessRunOutcome::from_status(status, crate::storage::SessionId::from_raw(1));
        assert_eq!(evidence.failed_tool_attempts, 1);
        assert!(!evidence.unresolved_tool_failure);
        assert_eq!(status, HeadlessStatus::Completed);
        assert_eq!(outcome.exit_code(), 0);
        assert_eq!(
            status.storage_status(),
            crate::storage::SessionStatus::Completed
        );
    }
}
