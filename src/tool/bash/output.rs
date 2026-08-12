//! Bounded, spooling collection of a foreground command's output.
//!
//! Everything that watches a running command's stdout/stderr lives here behind
//! [`OutputAccumulators`]: the per-stream bounded captures, the combined body
//! that spills to a spool file once it outgrows the in-memory cap, the
//! summary's last-lines ring, and the optional throttled live preview. The
//! caller streams chunks in with [`OutputAccumulators::push`] and takes one
//! [`CollectedOutput`] back from [`OutputAccumulators::finish`]; the internal
//! fields never leak, so overflow-to-disk and truncation policy stay owned here.

use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::tool::{OutputTruncationContext, ToolExecutionContext};

pub(super) const MAX_OUTPUT_CHARS: usize = 30000;
const PREVIEW_CHARS: usize = 2000;
const LIVE_OUTPUT_PREVIEW_CHARS: usize = 12000;
const LIVE_OUTPUT_EMIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const LAST_OUTPUT_LINES: usize = 8;
const LAST_OUTPUT_LINE_CHARS: usize = 240;
pub(super) const SMOL_MAX_OUTPUT_CHARS: usize = 6_000;
const SMOL_PREVIEW_CHARS: usize = 1_000;
const SMOL_LIVE_OUTPUT_PREVIEW_CHARS: usize = 2_000;
const SMOL_LAST_OUTPUT_LINES: usize = 4;
const SMOL_LAST_OUTPUT_LINE_CHARS: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BashOutputBudget {
    max_output_chars: usize,
    preview_chars: usize,
    live_output_preview_chars: usize,
    last_output_lines: usize,
    last_output_line_chars: usize,
}

impl BashOutputBudget {
    pub(crate) const fn normal() -> Self {
        Self {
            max_output_chars: MAX_OUTPUT_CHARS,
            preview_chars: PREVIEW_CHARS,
            live_output_preview_chars: LIVE_OUTPUT_PREVIEW_CHARS,
            last_output_lines: LAST_OUTPUT_LINES,
            last_output_line_chars: LAST_OUTPUT_LINE_CHARS,
        }
    }

    pub(crate) const fn smol() -> Self {
        Self {
            max_output_chars: SMOL_MAX_OUTPUT_CHARS,
            preview_chars: SMOL_PREVIEW_CHARS,
            live_output_preview_chars: SMOL_LIVE_OUTPUT_PREVIEW_CHARS,
            last_output_lines: SMOL_LAST_OUTPUT_LINES,
            last_output_line_chars: SMOL_LAST_OUTPUT_LINE_CHARS,
        }
    }
}

/// Monotonic counter that disambiguates truncated-output filenames within a
/// process so two large outputs in the same wall-clock instant don't collide.
static OUTPUT_FILE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Append `chunk`'s chars to `text` until `kept` reaches `max`, returning how
/// many chars were appended. The one "keep the first N chars, drop the rest"
/// primitive behind every bounded accumulator below.
fn push_chars_bounded(text: &mut String, kept: &mut usize, max: usize, chunk: &str) -> usize {
    let mut appended = 0usize;
    for ch in chunk.chars() {
        if *kept >= max {
            break;
        }
        text.push(ch);
        *kept += 1;
        appended += 1;
    }
    appended
}

/// Accumulates one stream into a bounded in-memory `String`, so a flood of
/// output can't grow the capture without limit. Overflow past the cap is
/// dropped here — [`CombinedOutput`] keeps the full copy on disk.
struct BoundedCapture {
    text: String,
    kept_chars: usize,
    max_chars: usize,
}

impl BoundedCapture {
    fn new(max_chars: usize) -> Self {
        Self {
            text: String::new(),
            kept_chars: 0,
            max_chars,
        }
    }

    fn push(&mut self, chunk: &str) {
        push_chars_bounded(&mut self.text, &mut self.kept_chars, self.max_chars, chunk);
    }
}

/// A lazily-created spool file under `.bonsai/tool-output` that receives the full
/// command output once it outgrows the in-memory cap.
struct CombinedSpool {
    file: tokio::fs::File,
    filename: String,
}

/// Collects stdout+stderr in arrival order, keeping a bounded in-memory head for
/// display while streaming the *complete* output straight to a spool file once
/// it exceeds the cap. A huge command can't exhaust memory, yet its full output
/// stays recoverable from the artifact.
struct CombinedOutput {
    head: String,
    head_chars: usize,
    total_chars: usize,
    project_root: PathBuf,
    spool: Option<CombinedSpool>,
    budget: BashOutputBudget,
}

struct FinalCombinedOutput {
    body: String,
    truncation: Option<OutputTruncationContext>,
    total_chars: usize,
}

impl CombinedOutput {
    fn new(project_root: PathBuf, budget: BashOutputBudget) -> Self {
        Self {
            head: String::new(),
            head_chars: 0,
            total_chars: 0,
            project_root,
            spool: None,
            budget,
        }
    }

    async fn push(&mut self, chunk: &str) -> Result<()> {
        use tokio::io::AsyncWriteExt;

        let chunk_chars = chunk.chars().count();
        self.total_chars += chunk_chars;

        // Spill to a file the first time we'd exceed the in-memory cap, flushing
        // the buffered head into it so the spool holds the complete output.
        if self.spool.is_none() && self.head_chars + chunk_chars > self.budget.max_output_chars {
            let mut spool = self.create_spool().await?;
            spool
                .file
                .write_all(self.head.as_bytes())
                .await
                .with_context(|| format!("Failed to write tool-output file: {}", spool.filename))?;
            self.spool = Some(spool);
        }

        if let Some(spool) = self.spool.as_mut() {
            spool
                .file
                .write_all(chunk.as_bytes())
                .await
                .with_context(|| format!("Failed to write tool-output file: {}", spool.filename))?;
        }

        // Keep the bounded head as the preview source either way.
        push_chars_bounded(
            &mut self.head,
            &mut self.head_chars,
            self.budget.max_output_chars,
            chunk,
        );
        Ok(())
    }

    async fn create_spool(&self) -> Result<CombinedSpool> {
        let output_dir = self.project_root.join(".bonsai").join("tool-output");
        tokio::fs::create_dir_all(&output_dir)
            .await
            .with_context(|| {
                format!(
                    "Failed to create output directory: {}",
                    output_dir.display()
                )
            })?;

        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // A bare second-resolution timestamp collides when two large outputs land
        // in the same second (e.g. concurrent `parallel: true` calls), silently
        // overwriting each other. Add sub-second precision plus a process-unique
        // counter so each overflow file is distinct.
        let seq = OUTPUT_FILE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let filename = format!(
            "bash_{}_{:09}_{}.txt",
            elapsed.as_secs(),
            elapsed.subsec_nanos(),
            seq
        );
        let path = output_dir.join(&filename);
        let file = tokio::fs::File::create(&path)
            .await
            .with_context(|| format!("Failed to create tool-output file: {}", path.display()))?;
        Ok(CombinedSpool { file, filename })
    }

    /// Flush the spool (if any) and produce the display body plus the truncation
    /// context. When everything fit in memory, returns the full output and no
    /// truncation.
    async fn finalize(mut self) -> Result<FinalCombinedOutput> {
        use tokio::io::AsyncWriteExt;

        let Some(mut spool) = self.spool.take() else {
            return Ok(FinalCombinedOutput {
                body: self.head,
                truncation: None,
                total_chars: self.total_chars,
            });
        };
        spool
            .file
            .flush()
            .await
            .with_context(|| format!("Failed to flush tool-output file: {}", spool.filename))?;

        let preview: String = self.head.chars().take(self.budget.preview_chars).collect();
        let saved_path = format!(".bonsai/tool-output/{}", spool.filename);
        let body = format!(
            "{preview}\n\n[Output truncated: {} chars total]\nFull output saved to: {saved_path}\nUse Read tool to view full output.",
            self.total_chars
        );
        let truncation = OutputTruncationContext {
            path: saved_path,
            total_chars: self.total_chars,
            preview_chars: preview.chars().count(),
        };
        Ok(FinalCombinedOutput {
            body,
            truncation: Some(truncation),
            total_chars: self.total_chars,
        })
    }
}

struct LastOutputLines {
    lines: VecDeque<String>,
    current: String,
    current_chars: usize,
    max_lines: usize,
    max_line_chars: usize,
}

impl LastOutputLines {
    fn new(budget: BashOutputBudget) -> Self {
        Self {
            lines: VecDeque::new(),
            current: String::new(),
            current_chars: 0,
            max_lines: budget.last_output_lines,
            max_line_chars: budget.last_output_line_chars,
        }
    }

    fn push(&mut self, chunk: &str) {
        for ch in chunk.chars() {
            if matches!(ch, '\n' | '\r') {
                self.finish_current();
            } else if self.current_chars < self.max_line_chars {
                self.current.push(ch);
                self.current_chars += 1;
            }
        }
    }

    fn into_lines(mut self) -> Vec<String> {
        self.finish_current();
        self.lines.into_iter().collect()
    }

    fn finish_current(&mut self) {
        let line = self.current.trim();
        if !line.is_empty() {
            if self.lines.len() >= self.max_lines {
                self.lines.pop_front();
            }
            self.lines.push_back(line.to_string());
        }
        self.current.clear();
        self.current_chars = 0;
    }
}

struct LiveCommandOutput {
    tool_call_id: String,
    sink: crate::output::SharedSink,
    head: String,
    head_chars: usize,
    tail: VecDeque<char>,
    total_chars: usize,
    last_emit: Option<std::time::Instant>,
    head_limit: usize,
    tail_limit: usize,
}

impl LiveCommandOutput {
    fn new(context: ToolExecutionContext, budget: BashOutputBudget) -> Self {
        let head_limit = budget.live_output_preview_chars / 2;
        let tail_limit = budget.live_output_preview_chars.saturating_sub(head_limit);
        Self {
            tool_call_id: context.tool_call_id().to_string(),
            sink: context.sink().clone(),
            head: String::new(),
            head_chars: 0,
            tail: VecDeque::new(),
            total_chars: 0,
            last_emit: None,
            head_limit,
            tail_limit,
        }
    }

    fn push(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.total_chars += chunk.chars().count();
        // Head fills first; whatever doesn't fit rolls through the tail ring.
        let taken =
            push_chars_bounded(&mut self.head, &mut self.head_chars, self.head_limit, chunk);
        for ch in chunk.chars().skip(taken) {
            if self.tail.len() >= self.tail_limit {
                self.tail.pop_front();
            }
            if self.tail_limit > 0 {
                self.tail.push_back(ch);
            }
        }
        if self.should_emit() {
            self.emit();
        }
    }

    fn flush(&mut self) {
        if self.total_chars > 0 {
            self.emit();
        }
    }

    fn should_emit(&self) -> bool {
        self.last_emit
            .map(|last_emit| last_emit.elapsed() >= LIVE_OUTPUT_EMIT_INTERVAL)
            .unwrap_or(true)
    }

    fn emit(&mut self) {
        let mut output = self.head.clone();
        let retained_chars = self.head_chars + self.tail.len();
        let omitted_chars = self.total_chars.saturating_sub(retained_chars);
        if omitted_chars > 0 {
            output.push_str(&format!(
                "\n\n[Live output truncated: {retained_chars} chars shown, {} chars total so far, {omitted_chars} middle chars omitted]\n",
                self.total_chars
            ));
        }
        output.extend(self.tail.iter());
        crate::redact::redact_in_place(&mut output);
        self.sink.tool_output(&self.tool_call_id, &output);
        self.last_emit = Some(std::time::Instant::now());
    }
}

/// Which pipe a decoded chunk came from — selects the per-stream capture in
/// [`OutputAccumulators::push`].
#[derive(Clone, Copy)]
pub(super) enum OutputStream {
    Stdout,
    Stderr,
}

/// Everything that watches a foreground command's output, behind one `push`:
/// the per-stream captures, the spooling combined body, the summary's
/// last-lines ring, and the optional live preview. One fan-out site instead of
/// four copies (stdout/stderr × read/EOF-carry).
pub(super) struct OutputAccumulators {
    stdout: BoundedCapture,
    stderr: BoundedCapture,
    combined: CombinedOutput,
    last_output: LastOutputLines,
    live_output: Option<LiveCommandOutput>,
    cargo_output: Option<crate::tool::diagnostics::CargoOutputCapture>,
}

/// The finished captures a command run needs after the streaming loop ends:
/// bounded stdout/stderr, the display body, its truncation context, the total
/// character count, and the trailing lines for the summary. Assembling this in
/// [`OutputAccumulators::finish`] keeps the accumulator internals private.
pub(super) struct CollectedOutput {
    pub(super) stdout: String,
    pub(super) stderr: String,
    pub(super) body: String,
    pub(super) truncation: Option<OutputTruncationContext>,
    pub(super) total_chars: usize,
    pub(super) last_output_lines: Vec<String>,
    pub(super) cargo_output: Option<String>,
}

impl OutputAccumulators {
    pub(super) fn new(
        project_root: PathBuf,
        budget: BashOutputBudget,
        context: Option<ToolExecutionContext>,
        capture_cargo_output: bool,
    ) -> Self {
        Self {
            stdout: BoundedCapture::new(budget.max_output_chars),
            stderr: BoundedCapture::new(budget.max_output_chars),
            combined: CombinedOutput::new(project_root, budget),
            last_output: LastOutputLines::new(budget),
            live_output: context.map(|context| LiveCommandOutput::new(context, budget)),
            cargo_output: capture_cargo_output
                .then(crate::tool::diagnostics::CargoOutputCapture::new),
        }
    }

    pub(super) async fn push(&mut self, stream: OutputStream, text: &str) -> Result<()> {
        if let Some(cargo_output) = self.cargo_output.as_mut() {
            match stream {
                OutputStream::Stdout => cargo_output.push_stdout(text),
                OutputStream::Stderr => cargo_output.push_stderr(text),
            }
        }
        match stream {
            OutputStream::Stdout => self.stdout.push(text),
            OutputStream::Stderr => self.stderr.push(text),
        }
        self.combined.push(text).await?;
        self.last_output.push(text);
        if let Some(live_output) = self.live_output.as_mut() {
            live_output.push(text);
        }
        Ok(())
    }

    pub(super) fn flush_live(&mut self) {
        if let Some(live_output) = self.live_output.as_mut() {
            live_output.flush();
        }
    }

    /// Drain the accumulators into the display-ready captures. Flushes the
    /// spool, so a truncated body carries its saved-output pointer.
    pub(super) async fn finish(self) -> Result<CollectedOutput> {
        let OutputAccumulators {
            stdout,
            stderr,
            combined,
            last_output,
            live_output: _,
            cargo_output,
        } = self;
        let finalized = combined.finalize().await?;
        Ok(CollectedOutput {
            stdout: stdout.text,
            stderr: stderr.text,
            body: finalized.body,
            truncation: finalized.truncation,
            total_chars: finalized.total_chars,
            last_output_lines: last_output.into_lines(),
            cargo_output: cargo_output.map(|capture| capture.finish()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiler_error_record() -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "message": "missing field",
                "code": { "code": "E0063" },
                "spans": [{
                    "file_name": "src/lib.rs",
                    "line_start": 42,
                    "column_start": 9,
                    "is_primary": true
                }],
                "rendered": "error[E0063]: missing field\n  --> src/lib.rs:42:9\n"
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn cargo_diagnostic_survives_raw_output_spooling() {
        let temp = tempfile::tempdir().expect("temp project");
        let mut output = OutputAccumulators::new(
            temp.path().to_path_buf(),
            BashOutputBudget::normal(),
            None,
            true,
        );
        let filler = format!("{}\n", "x".repeat(MAX_OUTPUT_CHARS));
        output.push(OutputStream::Stdout, &filler).await.unwrap();
        output
            .push(
                OutputStream::Stdout,
                &format!("{}\n", compiler_error_record()),
            )
            .await
            .unwrap();
        output.push(OutputStream::Stdout, &filler).await.unwrap();

        let collected = output.finish().await.unwrap();
        assert!(collected.truncation.is_some());
        assert!(
            !collected.stdout.contains("E0063") && !collected.stderr.contains("E0063"),
            "the regression requires the diagnostic to be beyond raw stream captures"
        );
        let diagnostics = collected
            .cargo_output
            .expect("structured Cargo capture should be present");
        assert!(diagnostics.contains("E0063"), "{diagnostics}");
        assert!(diagnostics.contains("src/lib.rs:42:9"), "{diagnostics}");
    }

    #[tokio::test]
    async fn generic_output_does_not_allocate_a_cargo_capture() {
        let temp = tempfile::tempdir().expect("temp project");
        let mut output = OutputAccumulators::new(
            temp.path().to_path_buf(),
            BashOutputBudget::normal(),
            None,
            false,
        );
        output
            .push(OutputStream::Stdout, "ordinary output\n")
            .await
            .unwrap();

        let collected = output.finish().await.unwrap();
        assert_eq!(collected.body, "ordinary output\n");
        assert!(collected.cargo_output.is_none());
        assert!(collected.truncation.is_none());
    }
}
