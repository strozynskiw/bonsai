//! Foreground command execution: spawn the shell under the live sandbox policy,
//! stream and bound both pipes concurrently, enforce the timeout by taking the
//! whole process group down, and reap the child. Produces the [`CommandResult`]
//! and [`CommandSummary`] the adapter renders and the tests assert against.

use std::path::Path;

use anyhow::{Context, Result};
use tokio::time::timeout;

use super::BashTool;
use super::output::{OutputAccumulators, OutputStream};
use crate::tool::{OutputTruncationContext, ToolExecutionContext};
use crate::util::utf8::decode_stream_chunk;

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
    pub(super) async fn run_command(
        &self,
        command: &str,
        cwd: &Path,
        timeout_secs: u64,
        escape: bool,
        allow_network: bool,
        context: Option<ToolExecutionContext>,
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

        let (process_exit, timed_out) =
            match timeout(std::time::Duration::from_secs(timeout_secs), collect).await {
                Ok(result) => (result?, false),
                Err(_) => {
                    // SIGTERM the whole group so the shell's children die too,
                    // then escalate to SIGKILL if it doesn't exit in the grace
                    // window, and reap to avoid a zombie.
                    if let Some(pid) = pid {
                        crate::process_group::terminate_group(pid);
                    }
                    let status = match timeout(PROCESS_GROUP_KILL_GRACE, child.wait()).await {
                        Ok(Ok(status)) => Some(status),
                        Ok(Err(err)) => {
                            tracing::warn!(%err, "failed to wait on timed-out command");
                            None
                        }
                        Err(_) => {
                            crate::process_group::force_kill(&mut child).await;
                            match child.wait().await {
                                Ok(status) => Some(status),
                                Err(err) => {
                                    tracing::warn!(
                                        %err,
                                        "failed to wait on force-killed command"
                                    );
                                    None
                                }
                            }
                        }
                    };
                    (status.as_ref().map(process_exit).unwrap_or_default(), true)
                }
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
        Ok(CommandResult {
            stdout: collected.stdout,
            stderr: collected.stderr,
            body: collected.body,
            truncation: collected.truncation,
            exit_code: process_exit.exit_code,
            timed_out,
            summary,
            confined,
            cargo_output: collected.cargo_output,
        })
    }
}
