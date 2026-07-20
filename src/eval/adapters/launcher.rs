use std::io;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

use super::AdapterRequest;

const MAX_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const PROCESS_GRACE_PERIOD: Duration = Duration::from_secs(2);
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Normalized terminal state shared by external benchmark adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdapterTerminalState {
    Completed,
    BudgetExhausted,
    Cancelled,
    TimedOut,
    Terminated,
    AuthConfigFailure,
    AgentFailure,
    VerifierFailed,
    PatchRejected,
    InternalError,
}

impl AdapterTerminalState {
    pub(crate) const fn is_success(self) -> bool {
        matches!(self, Self::Completed)
    }
}

/// Bounded, parsed result of one `bonsai -p --output-format json` child.
#[derive(Debug)]
pub(crate) struct LaunchResult {
    pub(crate) terminal_state: AdapterTerminalState,
    pub(crate) exit_code: Option<i32>,
    pub(crate) elapsed_ms: u64,
    pub(crate) timed_out: bool,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
    pub(crate) stderr: String,
    pub(crate) binary_version: Option<String>,
    pub(crate) headless: Option<HeadlessOutputProjection>,
    pub(crate) terminal_reason: Option<String>,
}

/// Stable subset of headless JSON needed by benchmark sidecars.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct HeadlessOutputProjection {
    pub(crate) status: HeadlessStatusProjection,
    pub(crate) output: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) session_id: i64,
    pub(crate) usage: HeadlessUsageProjection,
    #[serde(default)]
    pub(crate) budget_exhaustion: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) verification: Option<VerificationProjection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HeadlessStatusProjection {
    Completed,
    Failed,
    BudgetExhausted,
    Interrupted,
    Terminated,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct HeadlessUsageProjection {
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cost_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) input_cache: Option<HeadlessCacheProjection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct HeadlessCacheProjection {
    pub(crate) read_tokens: u64,
    pub(crate) creation_tokens: u64,
    pub(crate) total_input_tokens: u64,
    pub(crate) hit_rate_percent: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) struct VerificationProjection {
    #[serde(default)]
    pub(crate) repair_attempts: u32,
}

/// Launch the existing headless product surface with explicit benchmark policy.
///
/// # Errors
///
/// Returns an error when the child cannot be spawned, its stdin/stdout/stderr
/// cannot be driven, or process cleanup fails.
pub(crate) async fn launch(request: &AdapterRequest) -> Result<LaunchResult> {
    let state_dir = tempfile::TempDir::new().context("Failed to create benchmark state dir")?;
    let binary_version = binary_version(request).await;
    let mut command = Command::new(&request.runner.bonsai_binary);
    command
        .arg("-p")
        .arg("-")
        .arg("--output-format")
        .arg("json")
        .arg("--autonomy")
        .arg(request.runner.autonomy.label())
        .arg("--model")
        .arg(&request.runner.model)
        .arg("--effort")
        .arg(&request.runner.reasoning_effort)
        .arg("--max-turns")
        .arg(request.runner.budgets.max_turns.to_string())
        .arg("--max-generation-seconds")
        .arg(request.runner.budgets.max_generation_seconds.to_string())
        .arg("--max-output-chars")
        .arg(request.runner.budgets.max_output_chars.to_string())
        .arg("--max-tool-seconds")
        .arg(request.runner.budgets.max_tool_seconds.to_string())
        .arg("--timeout")
        .arg(request.runner.budgets.timeout_seconds.to_string())
        .arg("--isolation")
        .arg("off")
        .current_dir(&request.task.workspace)
        .env("BONSAI_HOME", state_dir.path())
        .env("BONSAI_PROVIDER", &request.runner.provider)
        .env("BONSAI_SANDBOX_NETWORK", request.runner.network.label())
        .env("BONSAI_DOTENV", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::process_group::configure(&mut command);

    let started = Instant::now();
    let mut child = command.spawn().with_context(|| {
        format!(
            "Failed to launch benchmark Bonsai binary {}",
            request.runner.bonsai_binary.display()
        )
    })?;
    let mut group_guard = ProcessGroupDropGuard::new(child.id());
    let stdout = child
        .stdout
        .take()
        .context("Benchmark child stdout was not piped")?;
    let stderr = child
        .stderr
        .take()
        .context("Benchmark child stderr was not piped")?;
    let stdout_task = tokio::spawn(read_bounded(stdout, MAX_STDOUT_BYTES));
    let stderr_task = tokio::spawn(read_bounded(stderr, MAX_STDERR_BYTES));

    let mut stdin = child
        .stdin
        .take()
        .context("Benchmark child stdin was not piped")?;
    stdin
        .write_all(request.task.instruction.as_bytes())
        .await
        .context("Failed to write benchmark instruction to Bonsai stdin")?;
    stdin
        .shutdown()
        .await
        .context("Failed to close benchmark Bonsai stdin")?;
    drop(stdin);

    let (status, timed_out) = wait_bounded(
        &mut child,
        Duration::from_secs(request.runner.budgets.timeout_seconds),
    )
    .await?;
    group_guard.disarm();
    let stdout = join_capture(stdout_task, "stdout").await?;
    let stderr = join_capture(stderr_task, "stderr").await?;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let headless = if stdout.truncated {
        None
    } else {
        serde_json::from_str::<HeadlessOutputProjection>(stdout.text.trim()).ok()
    };
    let exit_code = status.and_then(|value| value.code());
    let terminal_state = classify_terminal_state(timed_out, exit_code, headless.as_ref());
    let terminal_reason = terminal_reason(terminal_state, headless.as_ref(), &stderr.text);

    Ok(LaunchResult {
        terminal_state,
        exit_code,
        elapsed_ms,
        timed_out,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        stderr: stderr.text,
        binary_version,
        headless,
        terminal_reason,
    })
}

async fn binary_version(request: &AdapterRequest) -> Option<String> {
    let mut command = Command::new(&request.runner.bonsai_binary);
    command
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = timeout(VERSION_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() || output.stdout.len() > 1_024 {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

async fn wait_bounded(
    child: &mut tokio::process::Child,
    limit: Duration,
) -> Result<(Option<ExitStatus>, bool)> {
    match timeout(limit, child.wait()).await {
        Ok(result) => Ok((
            Some(result.context("Failed to wait for benchmark child")?),
            false,
        )),
        Err(_) => {
            if let Some(pid) = child.id() {
                crate::process_group::terminate_group(pid);
            }
            if let Ok(result) = timeout(PROCESS_GRACE_PERIOD, child.wait()).await {
                return Ok((
                    Some(result.context("Failed to reap timed-out benchmark child")?),
                    true,
                ));
            }
            crate::process_group::force_kill(child).await;
            let status = child
                .wait()
                .await
                .context("Failed to reap killed benchmark child")?;
            Ok((Some(status), true))
        }
    }
}

fn classify_terminal_state(
    timed_out: bool,
    exit_code: Option<i32>,
    output: Option<&HeadlessOutputProjection>,
) -> AdapterTerminalState {
    if timed_out || exit_code == Some(124) {
        return AdapterTerminalState::TimedOut;
    }
    if let Some(output) = output {
        return match output.status {
            HeadlessStatusProjection::Failed => AdapterTerminalState::AgentFailure,
            HeadlessStatusProjection::BudgetExhausted => AdapterTerminalState::BudgetExhausted,
            HeadlessStatusProjection::Interrupted => AdapterTerminalState::Cancelled,
            HeadlessStatusProjection::Terminated => AdapterTerminalState::Terminated,
            HeadlessStatusProjection::TimedOut => AdapterTerminalState::TimedOut,
            HeadlessStatusProjection::Completed if exit_code == Some(0) => {
                AdapterTerminalState::Completed
            }
            HeadlessStatusProjection::Completed => AdapterTerminalState::AgentFailure,
        };
    }
    match exit_code {
        Some(3) => AdapterTerminalState::AuthConfigFailure,
        Some(130) => AdapterTerminalState::Cancelled,
        Some(143) | None => AdapterTerminalState::Terminated,
        Some(_) => AdapterTerminalState::AgentFailure,
    }
}

fn terminal_reason(
    state: AdapterTerminalState,
    output: Option<&HeadlessOutputProjection>,
    stderr: &str,
) -> Option<String> {
    if let Some(output) = output
        && let Some(reason) = output.budget_exhaustion.as_ref()
    {
        return Some(reason.to_string());
    }
    if state == AdapterTerminalState::Completed {
        return None;
    }
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Some(format!("benchmark child ended as {state:?}"))
    } else {
        Some(stderr.to_string())
    }
}

#[derive(Debug)]
struct BoundedCapture {
    text: String,
    truncated: bool,
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
) -> io::Result<BoundedCapture> {
    let mut kept = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(kept.len());
        let take = remaining.min(read);
        kept.extend_from_slice(&buffer[..take]);
        truncated |= take < read;
    }
    Ok(BoundedCapture {
        text: String::from_utf8_lossy(&kept).into_owned(),
        truncated,
    })
}

async fn join_capture(
    task: tokio::task::JoinHandle<io::Result<BoundedCapture>>,
    label: &str,
) -> Result<BoundedCapture> {
    task.await
        .with_context(|| format!("Benchmark {label} capture task failed"))?
        .with_context(|| format!("Failed to read benchmark {label}"))
}

struct ProcessGroupDropGuard {
    pid: Option<u32>,
}

impl ProcessGroupDropGuard {
    const fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for ProcessGroupDropGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            crate::process_group::force_kill_group(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[cfg(unix)]
    fn request(workspace: &std::path::Path, binary: &std::path::Path) -> AdapterRequest {
        AdapterRequest {
            schema_version: super::super::ADAPTER_SCHEMA_VERSION,
            benchmark: super::super::BenchmarkPin {
                kind: super::super::BenchmarkKind::TerminalBench2,
                dataset: "terminal-bench".to_string(),
                dataset_version: "2.0".to_string(),
                harness_commit: super::super::HARBOR_HARNESS_COMMIT.to_string(),
                contract_commit: super::super::TERMINAL_BENCH_2_DATASET_COMMIT.to_string(),
            },
            task: super::super::BenchmarkTask {
                id: "mock-task".to_string(),
                workspace: workspace.to_path_buf(),
                instruction: "do the thing".to_string(),
                base_commit: None,
            },
            runner: super::super::BenchmarkRunner {
                bonsai_binary: binary.to_path_buf(),
                bonsai_revision: "fixture".to_string(),
                provider: "provider".to_string(),
                model: "model".to_string(),
                reasoning_effort: "high".to_string(),
                autonomy: super::super::BenchmarkAutonomy::AutoAccept,
                network: super::super::NetworkPolicy::Deny,
                budgets: super::super::BenchmarkBudgets {
                    max_turns: 2,
                    max_generation_seconds: 2,
                    max_output_chars: 10_000,
                    max_tool_seconds: 2,
                    timeout_seconds: 2,
                    max_patch_bytes: 10_000,
                },
            },
        }
    }

    #[test]
    fn terminal_states_distinguish_budget_auth_and_process_failure() {
        let usage = HeadlessUsageProjection {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
            cost_micros: Some(1),
            input_cache: None,
        };
        let output = HeadlessOutputProjection {
            status: HeadlessStatusProjection::BudgetExhausted,
            output: String::new(),
            provider: "p".to_string(),
            model: "m".to_string(),
            session_id: 1,
            usage,
            budget_exhaustion: None,
            verification: None,
        };
        assert_eq!(
            classify_terminal_state(false, Some(1), Some(&output)),
            AdapterTerminalState::BudgetExhausted
        );
        assert_eq!(
            classify_terminal_state(false, Some(3), None),
            AdapterTerminalState::AuthConfigFailure
        );
        assert_eq!(
            classify_terminal_state(false, Some(1), None),
            AdapterTerminalState::AgentFailure
        );
        let failed = HeadlessOutputProjection {
            status: HeadlessStatusProjection::Failed,
            ..output.clone()
        };
        assert_eq!(
            classify_terminal_state(false, Some(1), Some(&failed)),
            AdapterTerminalState::AgentFailure
        );
        assert_eq!(
            classify_terminal_state(true, None, None),
            AdapterTerminalState::TimedOut
        );
    }

    #[tokio::test]
    async fn bounded_capture_drains_but_retains_only_the_limit() {
        let bytes = vec![b'x'; 32];
        let capture = read_bounded(bytes.as_slice(), 10).await.unwrap();
        assert_eq!(capture.text, "xxxxxxxxxx");
        assert!(capture.truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launcher_uses_stdin_and_parses_bounded_headless_json() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().unwrap();
        let binary = temp.path().join("fake-bonsai");
        fs::write(
            &binary,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  echo "bonsai fixture"
  exit 0
fi
cat >/dev/null
printf '%s\n' '{"status":"completed","output":"done","provider":"provider","model":"model","session_id":7,"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,"cost_micros":3,"input_cache":{"read_tokens":6,"creation_tokens":0,"total_input_tokens":10,"hit_rate_percent":60}},"budget_exhaustion":null,"verification":{"repair_attempts":1},"completion_report":{}}'
"#,
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

        let result = launch(&request(temp.path(), &binary)).await.unwrap();

        assert_eq!(result.terminal_state, AdapterTerminalState::Completed);
        assert_eq!(result.binary_version.as_deref(), Some("bonsai fixture"));
        let output = result.headless.unwrap();
        assert_eq!(output.output, "done");
        assert_eq!(output.verification.unwrap().repair_attempts, 1);
        assert_eq!(output.usage.input_cache.unwrap().hit_rate_percent, Some(60));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn launcher_timeout_kills_the_child_process_group() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().unwrap();
        let binary = temp.path().join("slow-bonsai");
        let marker = temp.path().join("orphan-marker");
        fs::write(
            &binary,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo fixture; exit 0; fi\n(sleep 3; echo leaked > {}) &\nsleep 10\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let mut request = request(temp.path(), &binary);
        request.runner.budgets.timeout_seconds = 1;

        let result = launch(&request).await.unwrap();
        assert_eq!(result.terminal_state, AdapterTerminalState::TimedOut);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(
            !marker.exists(),
            "timed-out descendant escaped its process group"
        );
    }
}
