use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::lsp::LspHub;
use crate::sandbox::CommandSandbox;
use crate::tool::output::cap_text;
use crate::tool::schema::{
    closed_object, parse_args, path_property, string_enum_property, string_property,
};
use crate::tool::{
    ActionEffect, ActionPlan, ActionPolicy, ParallelPolicy, RiskTier, Tool, ToolOutput,
};

const DIAGNOSTICS_TIMEOUT_SECS: u64 = 120;
const MAX_OUTPUT_CHARS: usize = 40_000;
const MAX_CARGO_JSON_LINE_BYTES: usize = 256 * 1024;
const MAX_CARGO_CAPTURE_CHARS: usize = 40_000;
const MAX_CARGO_DIAGNOSTICS: usize = 100;
const MAX_CARGO_SUMMARY_LINES: usize = 8;
const MAX_CARGO_SUMMARY_LINE_CHARS: usize = 1_000;

const KINDS: &[&str] = &["all", "error", "warning"];

pub struct DiagnosticsTool {
    project_root: PathBuf,
    lsp_hub: Option<Arc<LspHub>>,
    sandbox: CommandSandbox,
    policy: ActionPolicy,
}

impl DiagnosticsTool {
    #[cfg(test)]
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            project_root,
            lsp_hub: None,
            sandbox: CommandSandbox::disabled(),
            policy: ActionPolicy::testing(),
        }
    }

    pub(crate) fn with_lsp(
        project_root: PathBuf,
        lsp_hub: Arc<LspHub>,
        sandbox: CommandSandbox,
        policy: ActionPolicy,
    ) -> Self {
        Self {
            project_root,
            lsp_hub: Some(lsp_hub),
            sandbox,
            policy,
        }
    }
}

/// Severity levels the diagnostics tool includes. Snake-case serde so an invalid
/// `kind` is rejected at parse time; the model-facing schema enum uses [`KINDS`].
#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
enum DiagnosticKind {
    All,
    #[default]
    Error,
    Warning,
}

#[derive(Deserialize)]
struct DiagnosticsArgs {
    /// Which severity levels to include. Defaults to "error".
    #[serde(default)]
    kind: DiagnosticKind,
    /// Optional package/crate name to check (Rust: `--package <name>`).
    #[serde(default)]
    package: Option<String>,
    /// Optional file path to diagnose through the registered language server.
    #[serde(default)]
    path: Option<String>,
}

// ── Rust / Cargo JSON message types ─────────────────────────────────────────

#[derive(Deserialize)]
struct CargoLine {
    reason: String,
    message: Option<CargoMessage>,
}

#[derive(Deserialize)]
struct CargoMessage {
    level: String,
    message: String,
    code: Option<CargoCode>,
    spans: Vec<CargoSpan>,
    // Rendered form from rustc — includes the source snippet with carets.
    rendered: Option<String>,
}

#[derive(Deserialize)]
struct CargoCode {
    code: String,
}

#[derive(Deserialize)]
struct CargoSpan {
    file_name: String,
    line_start: u32,
    column_start: u32,
    is_primary: bool,
}

// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum Ecosystem {
    Rust,
    Unknown,
}

fn detect_ecosystem(root: &std::path::Path) -> Ecosystem {
    if root.join("Cargo.toml").exists() {
        return Ecosystem::Rust;
    }
    Ecosystem::Unknown
}

struct DiagEntry {
    level: String,
    location: String,
    summary: String,
    rendered: Option<String>,
}

/// Bounded streaming capture for Cargo's JSON message stream.
///
/// Raw shell output is independently truncated and spooled. This capture keeps
/// complete `compiler-message` records as they pass through either pipe, so a
/// diagnostic buried in the omitted middle cannot disappear before formatting.
pub(crate) struct CargoOutputCapture {
    stdout: CargoStreamBuffer,
    stderr: CargoStreamBuffer,
    entries: Vec<DiagEntry>,
    retained_chars: usize,
    omitted_diagnostics: usize,
    summary_lines: std::collections::VecDeque<String>,
    oversized_records: usize,
}

impl CargoOutputCapture {
    pub(crate) fn new() -> Self {
        Self {
            stdout: CargoStreamBuffer::default(),
            stderr: CargoStreamBuffer::default(),
            entries: Vec::new(),
            retained_chars: 0,
            omitted_diagnostics: 0,
            summary_lines: std::collections::VecDeque::new(),
            oversized_records: 0,
        }
    }

    pub(crate) fn push_stdout(&mut self, text: &str) {
        let batch = self.stdout.push(text);
        self.oversized_records += batch.oversized_records;
        for line in batch.lines {
            self.push_line(&line);
        }
    }

    pub(crate) fn push_stderr(&mut self, text: &str) {
        let batch = self.stderr.push(text);
        self.oversized_records += batch.oversized_records;
        for line in batch.lines {
            self.push_line(&line);
        }
    }

    pub(crate) fn finish(mut self) -> String {
        let (line, oversized) = self.stdout.finish();
        self.oversized_records += usize::from(oversized);
        if let Some(line) = line {
            self.push_line(&line);
        }
        let (line, oversized) = self.stderr.finish();
        self.oversized_records += usize::from(oversized);
        if let Some(line) = line {
            self.push_line(&line);
        }

        let summary = self
            .summary_lines
            .into_iter()
            .collect::<Vec<_>>()
            .join("\n");
        let mut formatted = if self.entries.is_empty() {
            if self.oversized_records > 0 {
                format!(
                    "Cargo produced {} oversized structured record(s); inspect the saved raw output for complete diagnostics.",
                    self.oversized_records
                )
            } else if summary.is_empty() {
                "No diagnostics found.".to_string()
            } else {
                format!("No compiler diagnostics found.\n\ncargo output:\n{summary}")
            }
        } else {
            format_entries(&self.entries)
        };
        if self.omitted_diagnostics > 0 || self.oversized_records > 0 {
            use std::fmt::Write as _;
            if self.omitted_diagnostics > 0 {
                let _ = write!(
                    formatted,
                    "\n\n[{} additional compiler diagnostics omitted to keep output bounded]",
                    self.omitted_diagnostics
                );
            }
            if self.oversized_records > 0 && !self.entries.is_empty() {
                let _ = write!(
                    formatted,
                    "\n\n[{} oversized Cargo record(s) omitted; inspect the saved raw output]",
                    self.oversized_records
                );
            }
        }
        if !summary.is_empty() && !self.entries.is_empty() {
            formatted.push_str("\n\ncargo output:\n");
            formatted.push_str(&summary);
        }
        formatted
    }

    fn push_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if let Some(mut entry) = parse_cargo_line(line, DiagnosticKind::All) {
            if self.entries.len() >= MAX_CARGO_DIAGNOSTICS {
                self.omitted_diagnostics += 1;
                return;
            }
            let fixed_chars = entry.location.chars().count() + entry.summary.chars().count();
            let remaining = MAX_CARGO_CAPTURE_CHARS.saturating_sub(self.retained_chars);
            if remaining <= fixed_chars {
                self.omitted_diagnostics += 1;
                return;
            }
            if let Some(rendered) = entry.rendered.as_mut() {
                truncate_chars(rendered, remaining - fixed_chars);
            }
            self.retained_chars += fixed_chars
                + entry
                    .rendered
                    .as_ref()
                    .map_or(0, |rendered| rendered.chars().count());
            self.entries.push(entry);
            return;
        }

        // Cargo's final failure summary is plain stderr even with JSON output.
        // Keep only Cargo-shaped terminal summaries, as a small tail, rather
        // than retaining arbitrary malformed/non-JSON output in memory.
        if line.starts_with("error:") {
            let mut summary = line.to_string();
            truncate_chars(&mut summary, MAX_CARGO_SUMMARY_LINE_CHARS);
            if self.summary_lines.len() == MAX_CARGO_SUMMARY_LINES {
                self.summary_lines.pop_front();
            }
            self.summary_lines.push_back(summary);
        }
    }
}

#[derive(Default)]
struct CargoStreamBuffer {
    line: String,
    dropping_oversized_line: bool,
}

struct CargoLineBatch {
    lines: Vec<String>,
    oversized_records: usize,
}

impl CargoStreamBuffer {
    fn push(&mut self, text: &str) -> CargoLineBatch {
        let mut lines = Vec::new();
        let mut oversized_records = 0;
        for segment in text.split_inclusive('\n') {
            let complete = segment.ends_with('\n');
            if !self.dropping_oversized_line {
                if self.line.len().saturating_add(segment.len()) <= MAX_CARGO_JSON_LINE_BYTES {
                    self.line.push_str(segment.trim_end_matches('\n'));
                } else {
                    self.line.clear();
                    self.dropping_oversized_line = true;
                }
            }
            if complete {
                if self.dropping_oversized_line {
                    oversized_records += 1;
                } else if !self.line.is_empty() {
                    lines.push(std::mem::take(&mut self.line));
                }
                self.line.clear();
                self.dropping_oversized_line = false;
            }
        }
        CargoLineBatch {
            lines,
            oversized_records,
        }
    }

    fn finish(&mut self) -> (Option<String>, bool) {
        if self.dropping_oversized_line {
            self.line.clear();
            self.dropping_oversized_line = false;
            return (None, true);
        }
        if self.line.is_empty() {
            return (None, false);
        }
        (Some(std::mem::take(&mut self.line)), false)
    }
}

fn truncate_chars(text: &mut String, max_chars: usize) {
    if let Some((byte_index, _)) = text.char_indices().nth(max_chars) {
        text.truncate(byte_index);
    }
}

fn parse_cargo_output(stdout: &str, kind: DiagnosticKind) -> Vec<DiagEntry> {
    stdout
        .lines()
        .filter_map(|line| parse_cargo_line(line.trim(), kind))
        .collect()
}

fn parse_cargo_line(line: &str, kind: DiagnosticKind) -> Option<DiagEntry> {
    if line.is_empty() || !line.starts_with('{') {
        return None;
    }
    let record = serde_json::from_str::<CargoLine>(line).ok()?;
    if record.reason != "compiler-message" {
        return None;
    }
    let msg = record.message?;
    let level = msg.level.as_str();
    // Filter by requested kind.
    let include = match kind {
        DiagnosticKind::Error => level == "error",
        DiagnosticKind::Warning => level == "error" || level == "warning",
        DiagnosticKind::All => level != "note" && level != "help",
    };
    if !include {
        return None;
    }
    // Find the primary span for location.
    let location = msg
        .spans
        .iter()
        .find(|s| s.is_primary)
        .or_else(|| msg.spans.first())
        .map(|s| format!("{}:{}:{}", s.file_name, s.line_start, s.column_start))
        .unwrap_or_else(|| "unknown".to_string());
    let code = msg
        .code
        .as_ref()
        .map(|c| format!("[{}] ", c.code))
        .unwrap_or_default();
    let summary = format!("{}: {}{}", level, code, msg.message);
    Some(DiagEntry {
        level: level.to_string(),
        location,
        summary,
        rendered: msg.rendered,
    })
}

fn format_entries(entries: &[DiagEntry]) -> String {
    let errors = entries.iter().filter(|e| e.level == "error").count();
    let warnings = entries.iter().filter(|e| e.level == "warning").count();

    let mut out = String::new();
    use std::fmt::Write as _;

    if entries.is_empty() {
        return "No diagnostics found.".to_string();
    }

    let _ = writeln!(out, "errors: {errors}  warnings: {warnings}");
    let _ = writeln!(out);

    for entry in entries {
        if let Some(rendered) = &entry.rendered {
            // Use rustc's rendered form — it includes the source snippet.
            out.push_str(rendered.trim_end());
            out.push('\n');
        } else {
            let _ = writeln!(out, "{}: {}", entry.location, entry.summary);
        }
        out.push('\n');
    }

    out.trim_end().to_string()
}

pub(crate) fn format_cargo_json_for_bash(stdout: &str, stderr: &str) -> String {
    let entries = parse_cargo_output(stdout, DiagnosticKind::All);
    if entries.is_empty() {
        let stderr = stderr.trim();
        if stderr.is_empty() {
            "No diagnostics found.".to_string()
        } else {
            format!("No compiler diagnostics found.\n\ncargo output:\n{stderr}")
        }
    } else {
        format_entries(&entries)
    }
}

#[async_trait]
impl Tool for DiagnosticsTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::SelfAuthorized
    }

    fn name(&self) -> &str {
        "diagnostics"
    }

    fn description(&self) -> &str {
        "Run the project's type-checker or compiler in check mode and return structured \
         diagnostics (errors and warnings) with file, line, column, and message. \
         Use after editing files to verify correctness without a full build. \
         A path uses the registered Rust, TypeScript/JavaScript, Python, or Go language server; \
         pathless Rust projects use cargo check. kind='error' (default) shows only errors; \
         kind='warning' adds warnings; kind='all' includes notes."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object(
            [
                (
                    "kind",
                    string_enum_property(
                        "Severity filter: error (default), warning, or all",
                        KINDS,
                    ),
                ),
                (
                    "package",
                    string_property(
                        "Rust: package name to check (--package <name>). \
                         Omit to check the whole workspace.",
                    ),
                ),
                (
                    "path",
                    path_property(
                        "Optional file path to diagnose through the language server. \
                         Omit for project-wide Rust cargo check.",
                    ),
                ),
            ],
            &[],
        )
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        // Two simultaneous cargo check invocations on the same workspace would
        // conflict on the lock file.
        ParallelPolicy::Serialized
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: DiagnosticsArgs = parse_args("diagnostics", args)?;
        let mut lsp_error = None;

        if let Some(path) = args.path.as_deref()
            && let Some(hub) = &self.lsp_hub
        {
            match hub
                .diagnostics_for_path(path, args.kind != DiagnosticKind::Error)
                .await
            {
                Ok(diagnostics) => {
                    let text = crate::lsp::format_diagnostics(
                        &self.project_root,
                        &diagnostics.value,
                        "LSP diagnostics",
                    );
                    let text = diagnostics
                        .recovery_notice
                        .map_or(text.clone(), |notice| format!("{notice}\n\n{text}"));
                    return Ok(ToolOutput::Text(cap_text(
                        text,
                        MAX_OUTPUT_CHARS,
                        "Use path= to diagnose another file.",
                    )));
                }
                Err(err) => {
                    tracing::debug!(
                        path,
                        error = %err,
                        "LSP diagnostics failed; falling back to project diagnostics"
                    );
                    lsp_error = Some(err);
                }
            }
        }

        let ecosystem = detect_ecosystem(&self.project_root);
        let text = match ecosystem {
            Ecosystem::Rust => {
                let subject = args.package.as_deref().map_or_else(
                    || "cargo check (workspace)".to_string(),
                    |package| format!("cargo check --package {package}"),
                );
                // Cargo check can execute project build scripts and writes
                // build/cache state. It remains the deliberate Balanced-tier
                // verification exception, but now crosses the same auditable
                // authorization and OS-confinement boundaries as Bash.
                let plan = ActionPlan::new(
                    "diagnostics",
                    subject,
                    [ActionEffect::CodeExecution, ActionEffect::WorkspaceWrite],
                )
                .with_risk_tier(RiskTier::Medium);
                self.policy.authorize(&plan).await?;
                run_cargo_check(
                    &self.project_root,
                    args.package.as_deref(),
                    args.kind,
                    &self.sandbox,
                )
                .await?
            }
            Ecosystem::Unknown => {
                if let Some(error) = lsp_error {
                    return Ok(ToolOutput::Text(format!(
                        "LSP diagnostics unavailable: {error}\n\
                         Continue with built-in symbol_search/grep and the project's checker through bash."
                    )));
                }
                return Ok(ToolOutput::Text(
                    "diagnostics: no supported build system detected in the project root.\n\
                     Currently supports: Rust (Cargo.toml)."
                        .to_string(),
                ));
            }
        };

        Ok(ToolOutput::Text(cap_text(
            text,
            MAX_OUTPUT_CHARS,
            "Use package= to narrow to a specific crate.",
        )))
    }
}

async fn run_cargo_check(
    root: &std::path::Path,
    package: Option<&str>,
    kind: DiagnosticKind,
    sandbox: &CommandSandbox,
) -> Result<String> {
    let mut script = "exec cargo check --message-format=json --quiet".to_string();
    if let Some(pkg) = package {
        script.push_str(" --package ");
        script.push_str(&shell_quote(pkg));
    }
    let (mut cmd, sandbox_decision) = sandbox.command("sh", &script, root);
    sandbox_decision.log();
    cmd.kill_on_drop(true)
        // Suppress the progress spinner; diagnostics go to stdout as JSON.
        .env("CARGO_TERM_COLOR", "never");
    crate::process_group::configure(&mut cmd);

    let out = tokio::time::timeout(Duration::from_secs(DIAGNOSTICS_TIMEOUT_SECS), cmd.output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "cargo check timed out after {DIAGNOSTICS_TIMEOUT_SECS}s — \
                 use package= to scope to a single crate"
            )
        })??;

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // If cargo itself failed to even start (e.g., not a Rust project, cargo not installed),
    // the stdout is empty and stderr has the reason.
    if stdout.is_empty() && !out.status.success() {
        let msg = stderr.trim();
        anyhow::bail!("cargo check failed: {msg}");
    }

    let entries = parse_cargo_output(&stdout, kind);
    Ok(format_entries(&entries))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cargo_message(level: &str, code: &str, message: &str, file: &str) -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": level,
                "message": message,
                "code": { "code": code },
                "spans": [{
                    "file_name": file,
                    "line_start": 7,
                    "column_start": 3,
                    "is_primary": true
                }],
                "rendered": format!("{level}[{code}]: {message}\n  --> {file}:7:3\n")
            }
        })
        .to_string()
    }

    #[test]
    fn streaming_cargo_capture_retains_diagnostics_from_both_streams() {
        let first = cargo_message("error", "E0001", "first failure", "src/first.rs");
        let second = cargo_message("warning", "unused", "second warning", "src/second.rs");
        let split = first.len() / 2;
        let mut capture = CargoOutputCapture::new();

        capture.push_stdout("{malformed json}\n");
        capture.push_stdout(&first[..split]);
        capture.push_stdout(&format!("{}\n", &first[split..]));
        capture.push_stderr(&format!(
            "{second}\nerror: could not compile `fixture` due to 1 previous error\n"
        ));

        let output = capture.finish();
        assert!(output.contains("E0001"), "{output}");
        assert!(output.contains("src/first.rs:7:3"), "{output}");
        assert!(output.contains("unused"), "{output}");
        assert!(output.contains("src/second.rs:7:3"), "{output}");
        assert!(output.contains("could not compile `fixture`"), "{output}");
        assert!(
            !output.contains("No compiler diagnostics found"),
            "{output}"
        );
    }

    #[test]
    fn streaming_cargo_capture_is_bounded_and_reports_omissions() {
        let mut capture = CargoOutputCapture::new();
        for index in 0..=MAX_CARGO_DIAGNOSTICS {
            let record = cargo_message(
                "error",
                &format!("E{index:04}"),
                "bounded failure",
                &format!("src/file_{index}.rs"),
            );
            capture.push_stdout(&format!("{record}\n"));
        }

        let output = capture.finish();
        assert!(output.contains("E0000"), "{output}");
        assert!(
            output.contains("additional compiler diagnostics omitted"),
            "{output}"
        );
        assert!(output.chars().count() <= MAX_CARGO_CAPTURE_CHARS + 5_000);
    }

    #[test]
    fn streaming_cargo_capture_flushes_unterminated_record_on_finish() {
        let record = cargo_message("error", "E0999", "cancelled build", "src/cancelled.rs");
        let mut capture = CargoOutputCapture::new();
        capture.push_stdout(&record);

        let output = capture.finish();
        assert!(output.contains("E0999"), "{output}");
        assert!(output.contains("src/cancelled.rs:7:3"), "{output}");
    }

    #[test]
    fn streaming_cargo_capture_ignores_partial_or_success_records() {
        let mut partial = CargoOutputCapture::new();
        partial.push_stdout(r#"{"reason":"compiler-message""#);
        assert_eq!(partial.finish(), "No diagnostics found.");

        let mut oversized = CargoOutputCapture::new();
        oversized.push_stdout(&"x".repeat(MAX_CARGO_JSON_LINE_BYTES + 1));
        let output = oversized.finish();
        assert!(output.contains("oversized structured record"), "{output}");
        assert!(!output.contains("No diagnostics found"), "{output}");

        let mut success = CargoOutputCapture::new();
        success.push_stdout(
            r#"{"reason":"build-finished","success":true}
"#,
        );
        success.push_stderr("Finished `dev` profile\n");
        assert_eq!(success.finish(), "No diagnostics found.");
    }

    #[test]
    fn parse_cargo_output_extracts_errors() {
        // Minimal cargo JSON message with one error.
        let line = r#"{"reason":"compiler-message","package_id":"foo","manifest_path":"","target":{"kind":["lib"],"crate_types":["lib"],"name":"foo","src_path":"","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"code":{"code":"E0308","explanation":""},"level":"error","message":"mismatched types","spans":[{"file_name":"src/lib.rs","byte_start":0,"byte_end":1,"line_start":5,"line_end":5,"column_start":3,"column_end":10,"is_primary":true,"text":[],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"rendered":"error[E0308]: mismatched types\n  --> src/lib.rs:5:3\n"}}"#;
        let entries = parse_cargo_output(line, DiagnosticKind::Error);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].level, "error");
        assert!(
            entries[0].location.contains("src/lib.rs"),
            "{}",
            entries[0].location
        );
        assert!(
            entries[0].summary.contains("E0308"),
            "{}",
            entries[0].summary
        );
    }

    #[test]
    fn parse_cargo_output_filters_warnings_when_kind_is_error() {
        let warning = r#"{"reason":"compiler-message","package_id":"foo","manifest_path":"","target":{"kind":["lib"],"crate_types":["lib"],"name":"foo","src_path":"","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"code":{"code":"unused","explanation":""},"level":"warning","message":"unused variable","spans":[{"file_name":"src/lib.rs","byte_start":0,"byte_end":1,"line_start":2,"line_end":2,"column_start":5,"column_end":10,"is_primary":true,"text":[],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"rendered":"warning: unused variable\n"}}"#;
        let entries = parse_cargo_output(warning, DiagnosticKind::Error);
        assert!(
            entries.is_empty(),
            "warnings must be filtered when kind=error"
        );
    }

    #[test]
    fn parse_cargo_output_includes_warnings_when_kind_is_warning() {
        let warning = r#"{"reason":"compiler-message","package_id":"foo","manifest_path":"","target":{"kind":["lib"],"crate_types":["lib"],"name":"foo","src_path":"","edition":"2021","doc":true,"doctest":true,"test":true},"message":{"code":{"code":"unused","explanation":""},"level":"warning","message":"unused variable","spans":[{"file_name":"src/lib.rs","byte_start":0,"byte_end":1,"line_start":2,"line_end":2,"column_start":5,"column_end":10,"is_primary":true,"text":[],"label":null,"suggested_replacement":null,"suggestion_applicability":null,"expansion":null}],"children":[],"rendered":"warning: unused variable\n"}}"#;
        let entries = parse_cargo_output(warning, DiagnosticKind::Warning);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].level, "warning");
    }

    #[test]
    fn format_entries_empty() {
        let text = format_entries(&[]);
        assert!(text.contains("No diagnostics"), "{text}");
    }

    #[test]
    fn format_entries_uses_rendered_when_available() {
        let entry = DiagEntry {
            level: "error".to_string(),
            location: "src/lib.rs:5:3".to_string(),
            summary: "error[E0308]: mismatched types".to_string(),
            rendered: Some("error[E0308]: mismatched types\n  --> src/lib.rs:5:3\n".to_string()),
        };
        let text = format_entries(&[entry]);
        assert!(text.contains("error[E0308]"), "{text}");
        assert!(text.contains("src/lib.rs:5:3"), "{text}");
    }

    #[test]
    fn package_argument_is_shell_quoted() {
        assert_eq!(
            shell_quote("demo; touch /tmp/nope"),
            "'demo; touch /tmp/nope'"
        );
        assert_eq!(shell_quote("it's"), "'it'\"'\"'s'");
    }

    #[tokio::test]
    async fn unknown_ecosystem_returns_helpful_message() {
        let tmp = tempfile::TempDir::new().unwrap();
        let tool = DiagnosticsTool::new(tmp.path().to_path_buf());
        let result = tool.execute(json!({})).await.unwrap();
        let text = match result {
            ToolOutput::Text(t) => t,
            _ => panic!("expected Text"),
        };
        assert!(text.contains("no supported build system"), "{text}");
    }

    #[tokio::test]
    async fn missing_python_server_preserves_install_and_fallback_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("main.py"), "value = 1\n").unwrap();
        let mut spec = crate::lsp::LanguageServerSpec::python();
        spec.command = tmp.path().join("missing-pyright").display().to_string();
        let hub = Arc::new(crate::lsp::LspHub::with_registry(
            tmp.path().to_path_buf(),
            crate::lsp::LanguageServerRegistry::new(vec![spec]),
        ));
        let tool = DiagnosticsTool::with_lsp(
            tmp.path().to_path_buf(),
            hub,
            CommandSandbox::disabled(),
            ActionPolicy::testing(),
        );

        let output = tool.execute(json!({"path": "main.py"})).await.unwrap();
        let ToolOutput::Text(text) = output else {
            panic!("expected Text");
        };

        assert!(text.contains("npm install -g pyright"), "{text}");
        assert!(text.contains("symbol_search/grep"), "{text}");
        assert!(text.contains("project's checker through bash"), "{text}");
    }
}
