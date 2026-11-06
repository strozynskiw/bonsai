//! Foreground command execution: spawn the shell under the live sandbox policy,
//! stream and bound both pipes concurrently, enforce the timeout by taking the
//! whole process group down, and reap the child. Produces the [`CommandResult`]
//! and [`CommandSummary`] the adapter renders and the tests assert against.

use std::path::Path;

use anyhow::{Context, Result};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::BashTool;
use super::output::{OutputAccumulators, OutputStream};
use crate::tool::{OutputTruncationContext, ToolExecutionContext};
use crate::util::utf8::decode_stream_chunk;
use crate::verification_scheduler::{
    SharedVerification, SharedVerificationOutcome, SharedVerificationPayload,
    SharedVerificationProvenance, VerificationExecutionPolicy, VerificationRunRequest,
};

/// How long a timed-out command's process group gets after `SIGTERM` before it
/// is `SIGKILL`ed, so a stuck child can't linger after the command times out.
const PROCESS_GROUP_KILL_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// Outcome of running a foreground command. A named struct (vs. a positional
/// tuple) keeps `stdout`/`stderr` from being transposed at the call site.
pub(super) struct CommandResult {
    /// Bounded capture of stdout (≤ `MAX_OUTPUT_CHARS`) for read-tracking and the
    /// structured tool output; the full copy lives in the spooled artifact.
    pub(super) stdout: String,
    /// Bounded capture of stderr (≤ `MAX_OUTPUT_CHARS`).
    pub(super) stderr: String,
    /// Combined stdout+stderr ready to show the model: the full output when it
    /// fits in memory, otherwise a preview plus a pointer to the spooled file.
    pub(super) body: String,
    /// Set when `body` is a preview and the full output was spooled to disk.
    pub(super) truncation: Option<OutputTruncationContext>,
    pub(super) exit_code: Option<i32>,
    pub(super) timed_out: bool,
    pub(super) summary: CommandSummary,
    /// Whether the command actually ran under the sandbox. False when the
    /// sandbox was inactive or stepped past via an approved escape; drives the
    /// "retry with escape_sandbox=true" nudge on a confined failure.
    pub(super) confined: bool,
    /// Complete bounded Cargo diagnostics captured before raw output truncation.
    pub(super) cargo_output: Option<String>,
}

impl CommandResult {
    /// Append the shared-attempt provenance so the run loop can bind this
    /// session's own verification evidence to the attempt that produced it.
    ///
    /// The marker is scheduler-authored text rather than command output, so it
    /// is added to the model-visible body only: `stdout` stays the command's own
    /// capture, which is what read-path extraction interprets.
    fn with_shared_provenance(mut self, provenance: SharedVerificationProvenance) -> Self {
        push_shared_marker(&mut self.body, &provenance);
        self
    }

    /// Rebuild a result from a shared attempt's stored payload. A follower only
    /// ever sees the bounded stored body, so the summary reports the shared
    /// attempt's own execution duration instead of this session's wait.
    fn from_shared(
        payload: SharedVerificationPayload,
        provenance: SharedVerificationProvenance,
        timeout_secs: u64,
    ) -> Self {
        let mut body = payload.body;
        if payload.truncated {
            body =
                format!("[shared result was truncated to the scheduler's output budget]\n\n{body}");
        }
        // A follower has no separate stream capture, so the shared body — taken
        // before the scheduler's own marker line is appended — stands in for the
        // command's output rather than the marker polluting it.
        let stdout = body.clone();
        push_shared_marker(&mut body, &provenance);
        let combined_output_chars = body.chars().count();
        let last_output_lines = body
            .lines()
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(str::to_string)
            .collect();
        let summary = CommandSummary {
            exit_code: payload.exit_code,
            signal: None,
            timed_out: payload.timed_out,
            timeout_secs,
            duration: std::time::Duration::from_millis(
                u64::try_from(provenance.compute_ms).unwrap_or(0),
            ),
            stdout_bytes: stdout.len(),
            stderr_bytes: 0,
            combined_output_chars,
            saved_output: None,
            last_output_lines,
        };
        Self {
            stdout,
            stderr: String::new(),
            body,
            truncation: None,
            exit_code: payload.exit_code,
            timed_out: payload.timed_out,
            summary,
            confined: payload.confined,
            cargo_output: None,
        }
    }
}

impl SharedVerificationOutcome for CommandResult {
    fn shared_payload(&self) -> SharedVerificationPayload {
        SharedVerificationPayload {
            exit_code: self.exit_code,
            timed_out: self.timed_out,
            confined: self.confined,
            body: self.body.clone(),
            truncated: self.truncation.is_some(),
        }
    }
}

fn push_shared_marker(body: &mut String, provenance: &SharedVerificationProvenance) {
    if !body.is_empty() {
        body.push_str("\n\n");
    }
    body.push_str(&provenance.marker());
}

#[derive(Debug, Clone, Copy, Default)]
struct ProcessExit {
    exit_code: Option<i32>,
    signal: Option<i32>,
}

#[derive(Debug)]
pub(super) struct CommandSummary {
    pub(super) exit_code: Option<i32>,
    pub(super) signal: Option<i32>,
    pub(super) timed_out: bool,
    pub(super) timeout_secs: u64,
    pub(super) duration: std::time::Duration,
    pub(super) stdout_bytes: usize,
    pub(super) stderr_bytes: usize,
    pub(super) combined_output_chars: usize,
    pub(super) saved_output: Option<String>,
    pub(super) last_output_lines: Vec<String>,
}

#[cfg(unix)]
fn process_exit(status: &std::process::ExitStatus) -> ProcessExit {
    use std::os::unix::process::ExitStatusExt;

    let exit_code = status.code();
    ProcessExit {
        exit_code,
        signal: exit_code.is_none().then(|| status.signal()).flatten(),
    }
}

#[cfg(not(unix))]
fn process_exit(status: &std::process::ExitStatus) -> ProcessExit {
    ProcessExit {
        exit_code: status.code(),
        signal: None,
    }
}

impl CommandSummary {
    pub(super) fn footer(&self, command: &str) -> String {
        let mut footer = String::new();
        footer.push_str("[Command summary]\n");
        footer.push_str(&format!("command: {}\n", compact_summary_line(command)));
        footer.push_str(&format!(
            "exit_code: {}\n",
            self.exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".to_string())
        ));
        if let Some(signal) = self.signal {
            footer.push_str(&format!("signal: {signal}\n"));
        }
        footer.push_str(&format!("timed_out: {}\n", self.timed_out));
        if self.timed_out {
            footer.push_str(&format!("timeout_seconds: {}\n", self.timeout_secs));
        }
        footer.push_str(&format!(
            "duration: {}\n",
            format_command_duration(self.duration)
        ));
        footer.push_str(&format!("stdout_bytes: {}\n", self.stdout_bytes));
        footer.push_str(&format!("stderr_bytes: {}\n", self.stderr_bytes));
        footer.push_str(&format!(
            "combined_output_chars: {}\n",
            self.combined_output_chars
        ));
        if let Some(path) = &self.saved_output {
            footer.push_str(&format!("saved_output: {path}\n"));
        }
        footer.push_str("last_output:\n");
        if self.last_output_lines.is_empty() {
            footer.push_str("(no output)");
        } else {
            footer.push_str(&self.last_output_lines.join("\n"));
        }
        footer
    }
}

pub(super) fn compact_summary_line(text: &str) -> String {
    const MAX_CHARS: usize = 240;

    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        return compact;
    }
    let mut truncated = compact.chars().take(MAX_CHARS).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn format_command_duration(duration: std::time::Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

impl BashTool {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_command(
        &self,
        command: &str,
        cwd: &Path,
        timeout_secs: u64,
        escape: bool,
        allow_network: bool,
        context: Option<ToolExecutionContext>,
        cancellation: Option<CancellationToken>,
    ) -> Result<CommandResult> {
        use tokio::io::AsyncReadExt;

        // Confine the child per the live sandbox policy (a no-op unless enabled;
        // an enforcement floor independent of the autonomy level). On macOS this
        // rewrites the program to `sandbox-exec`, which exec-replaces itself with
        // the shell — so the `kill_on_drop` / process-group handling below still
        // reaches the shell. An approved `escape` spawns the same command
        // unconfined for this one run (the shared sandbox stays active).
        let (mut cmd, decision) = if escape {
            self.sandbox.command_unconfined(&self.shell, command, cwd)
        } else {
            self.sandbox
                .command_with_network(&self.shell, command, cwd, allow_network)
        };
        let confined = decision.confined;
        decision.log();
        // Backstop: SIGKILL the shell PID if this future is dropped. The timeout
        // path below kills the whole process group so the shell's children die
        // too — `kill_on_drop` only reaches the shell itself.
        cmd.kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Run the shell in its own process group so a timeout can take its forked
        // children down with it instead of orphaning them.
        crate::process_group::configure(&mut cmd);

        let started = std::time::Instant::now();
        let mut child = cmd.spawn().context("Failed to execute command")?;
        if let Some(context) = &context {
            context.sink().command_spawned(context.tool_call_id());
        }
        let pid = child.id();
        let mut stdout_pipe = child.stdout.take().expect("stdout is piped");
        let mut stderr_pipe = child.stderr.take().expect("stderr is piped");

        let capture_cargo_output = command
            .split_whitespace()
            .any(|token| token == "--message-format=json");
        let mut accumulators = OutputAccumulators::new(
            self.canonical_project_root.clone(),
            self.output_budget,
            context,
            capture_cargo_output,
        );
        let mut stdout_bytes = 0usize;
        let mut stderr_bytes = 0usize;

        // Stream both pipes concurrently (draining only one could fill the
        // other's buffer and deadlock the child), capping memory while spooling
        // any overflow to disk, then reap the child — all under the timeout.
        let collect = async {
            let mut stdout_buf = [0u8; 8192];
            let mut stderr_buf = [0u8; 8192];
            let mut stdout_carry: Vec<u8> = Vec::new();
            let mut stderr_carry: Vec<u8> = Vec::new();
            let mut stdout_open = true;
            let mut stderr_open = true;
            while stdout_open || stderr_open {
                tokio::select! {
                    result = stdout_pipe.read(&mut stdout_buf), if stdout_open => match result {
                        Ok(0) | Err(_) => stdout_open = false,
                        Ok(n) => {
                            stdout_bytes = stdout_bytes.saturating_add(n);
                            let text = decode_stream_chunk(&mut stdout_carry, &stdout_buf[..n]);
                            accumulators.push(OutputStream::Stdout, &text).await?;
                        }
                    },
                    result = stderr_pipe.read(&mut stderr_buf), if stderr_open => match result {
                        Ok(0) | Err(_) => stderr_open = false,
                        Ok(n) => {
                            stderr_bytes = stderr_bytes.saturating_add(n);
                            let text = decode_stream_chunk(&mut stderr_carry, &stderr_buf[..n]);
                            accumulators.push(OutputStream::Stderr, &text).await?;
                        }
                    },
                }
            }
            // Flush any incomplete trailing bytes left mid-character at EOF.
            if !stdout_carry.is_empty() {
                let text = String::from_utf8_lossy(&stdout_carry).into_owned();
                accumulators.push(OutputStream::Stdout, &text).await?;
            }
            if !stderr_carry.is_empty() {
                let text = String::from_utf8_lossy(&stderr_carry).into_owned();
                accumulators.push(OutputStream::Stderr, &text).await?;
            }
            accumulators.flush_live();
            let status = child.wait().await.context("Failed to wait on command")?;
            Ok::<ProcessExit, anyhow::Error>(process_exit(&status))
        };

        let termination = {
            let timeout_signal = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs));
            tokio::pin!(timeout_signal);
            tokio::select! {
                biased;
                () = wait_for_cancellation(cancellation.as_ref()) => Termination::Cancelled,
                () = &mut timeout_signal => Termination::TimedOut,
                result = collect => Termination::Finished(result),
            }
        };
        let (process_exit, timed_out, cancelled) = match termination {
            Termination::Finished(result) => (result?, false, false),
            // A cancelled shared verification takes the same path as a timeout:
            // the whole process group dies and the child is reaped, so nothing
            // outlives a decision that no subscriber still needs the result.
            Termination::TimedOut | Termination::Cancelled => (
                terminate_and_reap(&mut child, pid).await,
                matches!(termination, Termination::TimedOut),
                matches!(termination, Termination::Cancelled),
            ),
        };

        let collected = accumulators.finish().await?;
        let duration = started.elapsed();
        let saved_output = collected
            .truncation
            .as_ref()
            .map(|truncation| truncation.path.clone());
        let summary = CommandSummary {
            exit_code: process_exit.exit_code,
            signal: process_exit.signal,
            timed_out,
            timeout_secs,
            duration,
            stdout_bytes,
            stderr_bytes,
            combined_output_chars: collected.total_chars,
            saved_output,
            last_output_lines: collected.last_output_lines,
        };
        let mut body = collected.body;
        if cancelled {
            body = format!("{SHARED_CANCELLATION_NOTE}\n\n{body}");
        }
        Ok(CommandResult {
            stdout: collected.stdout,
            stderr: collected.stderr,
            body,
            truncation: collected.truncation,
            exit_code: process_exit.exit_code,
            timed_out,
            summary,
            confined,
            cargo_output: collected.cargo_output,
        })
    }
}

/// Note prepended to a shared verification whose process group was taken down
/// because every subscriber detached before it produced a result.
pub(super) const SHARED_CANCELLATION_NOTE: &str = "[shared verification cancelled: no subscriber still needs the result, so the process group was terminated]";

/// Why the streaming phase ended.
enum Termination {
    Finished(std::result::Result<ProcessExit, anyhow::Error>),
    TimedOut,
    Cancelled,
}

/// Resolve once the caller's cancellation token fires. Without a token this
/// never resolves, so the select falls through to the command's own timeout.
async fn wait_for_cancellation(token: Option<&CancellationToken>) {
    match token {
        Some(token) => token.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

impl BashTool {
    /// Run a recognized verification check through the cross-process scheduler.
    ///
    /// `None` means the scheduler declined — disabled, or a command it will not
    /// coordinate — and the caller keeps ordinary foreground execution. An
    /// interrupted shared attempt is returned as an error so a stale or unbound
    /// result is never read as a verification pass.
    pub(super) async fn run_scheduled_verification(
        &self,
        command: &str,
        cwd: &Path,
        timeout_secs: u64,
        escaped: bool,
        context: Option<ToolExecutionContext>,
    ) -> Option<Result<CommandResult>> {
        let (scheduler, session_id) = self.verification_scheduler.as_ref()?.scheduler().await?;
        // Neither scheduled path requests network access: planning commands are
        // never scheduled, and a foreground verification needs no network.
        let allow_network = false;
        let (_, decision) = if escaped {
            self.sandbox.command_unconfined(&self.shell, command, cwd)
        } else {
            self.sandbox
                .command_with_network(&self.shell, command, cwd, allow_network)
        };
        let request = VerificationRunRequest {
            command: command.to_string(),
            cwd: cwd.to_path_buf(),
            timeout_secs,
            tool_call_id: context
                .as_ref()
                .map(|context| context.tool_call_id().to_string())
                .unwrap_or_default(),
            session_id,
            policy: VerificationExecutionPolicy {
                confined: decision.confined,
                escaped,
                allow_network,
                shell: self.shell.clone(),
                timeout_secs,
            },
            cancellation: Some(CancellationToken::new()),
        };
        let sink = context.as_ref().map(|context| context.sink().clone());
        let cancellation = request.cancellation.clone();
        let producer = async {
            self.run_command(
                command,
                cwd,
                timeout_secs,
                escaped,
                allow_network,
                context,
                cancellation,
            )
            .await
        };
        match scheduler.run(&request, sink.as_ref(), producer).await {
            Ok(SharedVerification::Produced { value, provenance }) => {
                Some(Ok(value.with_shared_provenance(provenance)))
            }
            Ok(SharedVerification::Reused {
                payload,
                provenance,
            }) => Some(Ok(CommandResult::from_shared(
                payload,
                provenance,
                timeout_secs,
            ))),
            Ok(SharedVerification::Declined) => None,
            Ok(SharedVerification::Interrupted { job_key, reason }) => Some(Err(anyhow::anyhow!(
                "Shared verification attempt {job_key} was interrupted: {reason}. No verification evidence was produced for this check; rerun it."
            ))),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "verification scheduler declined; running the check ordinarily"
                );
                None
            }
        }
    }
}

/// SIGTERM the whole group so the shell's children die too, then escalate to
/// SIGKILL if it does not exit in the grace window, and reap to avoid a zombie.
async fn terminate_and_reap(child: &mut tokio::process::Child, pid: Option<u32>) -> ProcessExit {
    if let Some(pid) = pid {
        crate::process_group::terminate_group(pid);
    }
    match timeout(PROCESS_GROUP_KILL_GRACE, child.wait()).await {
        Ok(Ok(status)) => process_exit(&status),
        Ok(Err(err)) => {
            tracing::warn!(%err, "failed to wait on a terminated command");
            ProcessExit::default()
        }
        Err(_) => {
            crate::process_group::force_kill(child).await;
            match child.wait().await {
                Ok(status) => process_exit(&status),
                Err(err) => {
                    tracing::warn!(%err, "failed to wait on a force-killed command");
                    ProcessExit::default()
                }
            }
        }
    }
}
