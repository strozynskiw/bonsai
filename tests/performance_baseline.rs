#![cfg(unix)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const REPORT_SCHEMA_VERSION: u32 = 1;
const DEFAULT_SAMPLE_COUNT: usize = 5;
const MIN_SAMPLE_COUNT: usize = 3;
const MAX_SAMPLE_COUNT: usize = 30;
const TUI_ROWS: u16 = 30;
const TUI_COLS: u16 = 100;
const TUI_START_TIMEOUT: Duration = Duration::from_secs(20);
const TUI_EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_SETTLE: Duration = Duration::from_millis(750);
const IDLE_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const IDLE_MEASURE_WINDOW: Duration = Duration::from_secs(3);
const HEADLESS_TIMEOUT: Duration = Duration::from_secs(30);
const TOOL_TURNS: usize = 3;
const CACHE_TOKENS_BY_REQUEST: [u64; TOOL_TURNS + 1] = [0, 5_000, 7_500, 10_000];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PerformanceReport {
    schema_version: u32,
    identity: RunIdentity,
    sample_count: usize,
    raw: RawSamples,
    summary: PerformanceSummary,
    baseline: Option<BaselineComparison>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunIdentity {
    target: String,
    runner_class: String,
    profile: String,
    toolchain: String,
    git_commit: String,
    binary_sha256: String,
    binary_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSamples {
    fresh_startup_ms: Vec<u64>,
    returning_startup_ms: Vec<u64>,
    idle_cpu_percent: Vec<f64>,
    idle_rss_bytes: Vec<u64>,
    headless: Vec<HeadlessSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeadlessSample {
    spawn_to_first_assistant_delta_ms: u64,
    persistence_duration_ms: u64,
    context_used_tokens: Vec<u64>,
    provider_prompt_tokens: Vec<u64>,
    prompt_tokens: u64,
    completion_tokens: u64,
    input_cache_hit_rate_percent: u64,
    representative_task_cost_micros: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PerformanceSummary {
    fresh_startup_ms: U64Distribution,
    returning_startup_ms: U64Distribution,
    idle_cpu_percent: F64Distribution,
    idle_rss_bytes: U64Distribution,
    spawn_to_first_assistant_delta_ms: U64Distribution,
    persistence_duration_ms: U64Distribution,
    context_peak_tokens: U64Distribution,
    context_growth_tokens: U64Distribution,
    prompt_tokens: U64Distribution,
    completion_tokens: U64Distribution,
    input_cache_hit_rate_percent: U64Distribution,
    representative_task_cost_micros: U64Distribution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct U64Distribution {
    median: u64,
    p95: u64,
    min: u64,
    max: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct F64Distribution {
    median: f64,
    p95: f64,
    min: f64,
    max: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BaselineComparison {
    source: String,
    baseline_git_commit: String,
    passed: bool,
    violations: Vec<String>,
}

#[derive(Debug)]
struct TuiMeasurement {
    startup_ms: u64,
    idle_cpu_percent: Option<f64>,
    idle_rss_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct ProcessSample {
    cpu_time: Duration,
    rss_bytes: u64,
}

#[derive(Debug, Default)]
struct ScriptState {
    request_index: usize,
    cache_read_tokens: u64,
}

#[derive(Clone)]
struct PerformanceChat {
    state: Arc<Mutex<ScriptState>>,
}

impl Respond for PerformanceChat {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let request_index = state.request_index;
        state.request_index = state.request_index.saturating_add(1);
        let prompt_tokens = u64::try_from(request.body.len().saturating_add(3) / 4)
            .unwrap_or(u64::MAX)
            .max(1);
        let cached_tokens = fixture_cache_tokens(request_index, prompt_tokens);
        state.cache_read_tokens = state.cache_read_tokens.saturating_add(cached_tokens);
        let completion_tokens = if request_index < TOOL_TURNS { 8 } else { 12 };
        let response = scripted_response(
            request_index,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
        );
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(response)
    }
}

fn fixture_cache_tokens(request_index: usize, prompt_tokens: u64) -> u64 {
    CACHE_TOKENS_BY_REQUEST
        .get(request_index)
        .copied()
        .unwrap_or(0)
        .min(prompt_tokens)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "release performance harness; run explicitly with cargo test --release --test performance_baseline -- --ignored --nocapture"]
async fn real_release_performance_baseline() -> Result<()> {
    let report_path = required_path("BONSAI_PERF_REPORT")?;
    let sample_count = sample_count()?;
    let binary = performance_binary()?;
    let identity = run_identity(&binary)?;
    let baseline = load_baseline(&identity)?;

    let mut fresh_startup_ms = Vec::with_capacity(sample_count);
    let mut returning_startup_ms = Vec::with_capacity(sample_count);
    let mut idle_cpu_percent = Vec::with_capacity(sample_count);
    let mut idle_rss_bytes = Vec::with_capacity(sample_count);

    for _ in 0..sample_count {
        let home = tempfile::TempDir::new().context("create isolated TUI home")?;
        let project = tempfile::TempDir::new().context("create isolated TUI project")?;
        let fresh = run_tui_measurement(&binary, home.path(), project.path(), false)?;
        let returning = run_tui_measurement(&binary, home.path(), project.path(), true)?;
        fresh_startup_ms.push(fresh.startup_ms);
        returning_startup_ms.push(returning.startup_ms);
        idle_cpu_percent.push(
            returning
                .idle_cpu_percent
                .context("returning TUI run did not record idle CPU")?,
        );
        idle_rss_bytes.push(
            returning
                .idle_rss_bytes
                .context("returning TUI run did not record resident memory")?,
        );
    }

    let mut headless = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        headless.push(run_headless_sample(&binary).await?);
    }

    let raw = RawSamples {
        fresh_startup_ms,
        returning_startup_ms,
        idle_cpu_percent,
        idle_rss_bytes,
        headless,
    };
    let summary = PerformanceSummary::from_raw(&raw)?;
    let comparison = baseline.map(|(source, report)| {
        compare_baseline(
            source,
            &identity,
            &summary,
            &report.identity,
            &report.summary,
        )
    });
    let report = PerformanceReport {
        schema_version: REPORT_SCHEMA_VERSION,
        identity,
        sample_count,
        raw,
        summary,
        baseline: comparison,
    };
    write_report_atomic(&report_path, &report)?;
    println!("performance report: {}", report_path.display());

    if let Some(comparison) = &report.baseline
        && !comparison.passed
    {
        bail!(
            "performance baseline failed:\n{}",
            comparison.violations.join("\n")
        );
    }
    Ok(())
}

impl PerformanceSummary {
    fn from_raw(raw: &RawSamples) -> Result<Self> {
        let ttfo = raw
            .headless
            .iter()
            .map(|sample| sample.spawn_to_first_assistant_delta_ms)
            .collect::<Vec<_>>();
        let persistence = raw
            .headless
            .iter()
            .map(|sample| sample.persistence_duration_ms)
            .collect::<Vec<_>>();
        let context_peak = raw
            .headless
            .iter()
            .map(|sample| {
                sample
                    .provider_prompt_tokens
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(0)
            })
            .collect::<Vec<_>>();
        let context_growth = raw
            .headless
            .iter()
            .map(|sample| {
                sample
                    .provider_prompt_tokens
                    .last()
                    .copied()
                    .unwrap_or(0)
                    .saturating_sub(sample.provider_prompt_tokens.first().copied().unwrap_or(0))
            })
            .collect::<Vec<_>>();
        let prompt_tokens = raw
            .headless
            .iter()
            .map(|sample| sample.prompt_tokens)
            .collect::<Vec<_>>();
        let completion_tokens = raw
            .headless
            .iter()
            .map(|sample| sample.completion_tokens)
            .collect::<Vec<_>>();
        let cache = raw
            .headless
            .iter()
            .map(|sample| sample.input_cache_hit_rate_percent)
            .collect::<Vec<_>>();
        let cost = raw
            .headless
            .iter()
            .map(|sample| sample.representative_task_cost_micros)
            .collect::<Vec<_>>();
        Ok(Self {
            fresh_startup_ms: u64_distribution(&raw.fresh_startup_ms)?,
            returning_startup_ms: u64_distribution(&raw.returning_startup_ms)?,
            idle_cpu_percent: f64_distribution(&raw.idle_cpu_percent)?,
            idle_rss_bytes: u64_distribution(&raw.idle_rss_bytes)?,
            spawn_to_first_assistant_delta_ms: u64_distribution(&ttfo)?,
            persistence_duration_ms: u64_distribution(&persistence)?,
            context_peak_tokens: u64_distribution(&context_peak)?,
            context_growth_tokens: u64_distribution(&context_growth)?,
            prompt_tokens: u64_distribution(&prompt_tokens)?,
            completion_tokens: u64_distribution(&completion_tokens)?,
            input_cache_hit_rate_percent: u64_distribution(&cache)?,
            representative_task_cost_micros: u64_distribution(&cost)?,
        })
    }
}

fn run_tui_measurement(
    binary: &Path,
    home: &Path,
    project: &Path,
    measure_idle: bool,
) -> Result<TuiMeasurement> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: TUI_ROWS,
            cols: TUI_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("native PTY should be available")?;
    let mut command = CommandBuilder::new(binary);
    configure_tui_command(&mut command, home, project);
    let started = Instant::now();
    let mut child = pair
        .slave
        .spawn_command(command)
        .context("spawn release TUI in native PTY")?;
    let pid = child
        .process_id()
        .context("native PTY child did not expose a process id")?;
    drop(pair.slave);
    let mut killer = child.clone_killer();
    let mut reader = pair
        .master
        .try_clone_reader()
        .context("clone PTY output reader")?;
    let mut writer = pair.master.take_writer().context("open PTY input writer")?;
    let output = Arc::new(Mutex::new(Vec::new()));
    let reader_output = output.clone();
    let reader_thread = std::thread::spawn(move || -> std::io::Result<()> {
        let mut chunk = [0_u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(read) => {
                    if let Ok(mut output) = reader_output.lock() {
                        output.extend_from_slice(&chunk[..read]);
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    });
    let (status_tx, status_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = status_tx.send(child.wait());
    });

    if let Err(error) = wait_for_visible_tui_frame(&output, TUI_START_TIMEOUT) {
        let _ = killer.kill();
        return Err(error);
    }
    let startup = started.elapsed();
    let resources = if measure_idle {
        std::thread::sleep(IDLE_SETTLE);
        sample_idle_process(pid)
    } else {
        Ok((None, None))
    };

    writer.write_all(b"\x1b")?;
    writer.flush()?;
    std::thread::sleep(Duration::from_millis(250));
    writer.write_all(b"/quit\r")?;
    writer.flush()?;
    let status = match status_rx.recv_timeout(TUI_EXIT_TIMEOUT) {
        Ok(status) => status.context("TUI process should return a status")?,
        Err(_) => {
            killer.kill().context("kill timed-out TUI process")?;
            status_rx
                .recv_timeout(Duration::from_secs(3))
                .context("killed TUI process should exit")?
                .context("killed TUI process should return a status")?
        }
    };
    drop(writer);
    drop(pair.master);
    reader_thread
        .join()
        .map_err(|_| anyhow::anyhow!("PTY output reader panicked"))?
        .context("drain PTY output")?;
    if !status.success() {
        bail!("TUI performance sample exited with status {status:?}");
    }
    let (idle_cpu_percent, idle_rss_bytes) = resources?;
    Ok(TuiMeasurement {
        startup_ms: millis_u64(startup),
        idle_cpu_percent,
        idle_rss_bytes,
    })
}

fn configure_tui_command(command: &mut CommandBuilder, home: &Path, project: &Path) {
    command.cwd(project);
    command.env_clear();
    command.env("HOME", home);
    command.env("PATH", inherited_path());
    command.env("TERM", "xterm-256color");
    command.env("LANG", "C.UTF-8");
    command.env("LC_ALL", "C");
    command.env("BONSAI_HOME", home.join("bonsai-home"));
    command.env("BONSAI_DOTENV", "0");
    command.env("BONSAI_DISABLE_MODELS_FETCH", "1");
    command.env("BONSAI_MEMORY_EMBEDDINGS", "off");
    command.env("BONSAI_EPISODES", "0");
    command.env("BONSAI_PROVIDER", "openai-compatible");
    command.env(
        "OPENAI_COMPATIBLE_MODEL",
        "openai-compatible/performance-model",
    );
}

fn wait_for_visible_tui_frame(output: &Mutex<Vec<u8>>, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    let deadline = started + timeout;
    while Instant::now() < deadline {
        let bytes = output
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY output lock was poisoned"))?
            .clone();
        let mut parser = vt100::Parser::new(TUI_ROWS, TUI_COLS, 0);
        parser.process(&bytes);
        let visible = parser.screen().contents();
        if visible.to_ascii_lowercase().contains("bonsai") {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    bail!("TUI did not render a visible application frame within {timeout:?}")
}

fn sample_idle_process(pid: u32) -> Result<(Option<f64>, Option<u64>)> {
    #[cfg(target_os = "linux")]
    let clock_ticks_per_second = linux_clock_ticks_per_second()?;
    #[cfg(target_os = "linux")]
    let sample = |pid| sample_linux_process(pid, clock_ticks_per_second);
    #[cfg(not(target_os = "linux"))]
    let sample = sample_ps_process;

    let first = sample(pid)?;
    let started = Instant::now();
    let deadline = started + IDLE_MEASURE_WINDOW;
    let mut last = first;
    let mut peak_rss_bytes = first.rss_bytes;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        std::thread::sleep(remaining.min(IDLE_SAMPLE_INTERVAL));
        last = sample(pid)?;
        peak_rss_bytes = peak_rss_bytes.max(last.rss_bytes);
    }
    let cpu_percent = process_cpu_percent(first.cpu_time, last.cpu_time, started.elapsed())?;
    Ok((Some(cpu_percent), Some(peak_rss_bytes)))
}

#[cfg(not(target_os = "linux"))]
fn sample_ps_process(pid: u32) -> Result<ProcessSample> {
    let pid = pid.to_string();
    let output = std::process::Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-o", "time=", "-o", "rss=", "-p", pid.as_str()])
        .output()
        .context("run ps for TUI performance sample")?;
    if !output.status.success() {
        bail!(
            "ps failed for pid {pid}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("ps output should be UTF-8")?;
    let mut fields = stdout.split_whitespace();
    let cpu_time = parse_process_cpu_time(fields.next().context("ps output omitted CPU time")?)?;
    let rss_kib = fields
        .next()
        .context("ps output omitted RSS")?
        .parse::<u64>()
        .context("parse ps RSS")?;
    Ok(ProcessSample {
        cpu_time,
        rss_bytes: rss_kib.saturating_mul(1024),
    })
}

#[cfg(target_os = "linux")]
fn sample_linux_process(pid: u32, clock_ticks_per_second: u64) -> Result<ProcessSample> {
    let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
    let status_path = PathBuf::from(format!("/proc/{pid}/status"));
    let stat = fs::read_to_string(&stat_path)
        .with_context(|| format!("read Linux process stat {}", stat_path.display()))?;
    let status = fs::read_to_string(&status_path)
        .with_context(|| format!("read Linux process status {}", status_path.display()))?;
    Ok(ProcessSample {
        cpu_time: parse_linux_proc_cpu_time(&stat, clock_ticks_per_second)?,
        rss_bytes: parse_linux_proc_rss_bytes(&status)?,
    })
}

#[cfg(target_os = "linux")]
fn linux_clock_ticks_per_second() -> Result<u64> {
    let output = std::process::Command::new("getconf")
        .env("LC_ALL", "C")
        .arg("CLK_TCK")
        .output()
        .context("run getconf CLK_TCK for Linux performance sampling")?;
    if !output.status.success() {
        bail!(
            "getconf CLK_TCK failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let ticks = String::from_utf8(output.stdout)
        .context("getconf CLK_TCK output should be UTF-8")?
        .trim()
        .parse::<u64>()
        .context("parse getconf CLK_TCK output")?;
    if ticks == 0 {
        bail!("getconf CLK_TCK returned zero");
    }
    Ok(ticks)
}

fn parse_linux_proc_cpu_time(stat: &str, clock_ticks_per_second: u64) -> Result<Duration> {
    if clock_ticks_per_second == 0 {
        bail!("Linux clock ticks per second must be nonzero");
    }
    let command_end = stat
        .rfind(')')
        .context("Linux process stat omitted command terminator")?;
    let fields = stat
        .get(command_end + 1..)
        .context("Linux process stat command boundary was invalid")?
        .split_whitespace()
        .collect::<Vec<_>>();
    let user_ticks = fields
        .get(11)
        .context("Linux process stat omitted user CPU ticks")?
        .parse::<u64>()
        .context("parse Linux process user CPU ticks")?;
    let system_ticks = fields
        .get(12)
        .context("Linux process stat omitted system CPU ticks")?
        .parse::<u64>()
        .context("parse Linux process system CPU ticks")?;
    let total_ticks = user_ticks
        .checked_add(system_ticks)
        .context("Linux process CPU ticks overflowed")?;
    let seconds = total_ticks / clock_ticks_per_second;
    let remainder = total_ticks % clock_ticks_per_second;
    let nanos = u128::from(remainder)
        .saturating_mul(1_000_000_000)
        .checked_div(u128::from(clock_ticks_per_second))
        .context("convert Linux process CPU ticks")?;
    let nanos = u32::try_from(nanos).context("Linux process CPU nanoseconds overflowed")?;
    Ok(Duration::new(seconds, nanos))
}

fn parse_linux_proc_rss_bytes(status: &str) -> Result<u64> {
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .context("Linux process status omitted VmRSS")?;
    let mut fields = value.split_whitespace();
    let kib = fields
        .next()
        .context("Linux process VmRSS omitted a value")?
        .parse::<u64>()
        .context("parse Linux process VmRSS")?;
    if fields.next() != Some("kB") || fields.next().is_some() {
        bail!("Linux process VmRSS did not use the expected kB unit");
    }
    kib.checked_mul(1024)
        .context("Linux process VmRSS overflowed bytes")
}

fn parse_process_cpu_time(value: &str) -> Result<Duration> {
    let (days, clock) = match value.split_once('-') {
        Some((days, clock)) => (
            days.parse::<u64>().context("parse ps CPU-time days")?,
            clock,
        ),
        None => (0, value),
    };
    let fields = clock.split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds) = match fields.as_slice() {
        [minutes, seconds] => (0, *minutes, *seconds),
        [hours, minutes, seconds] => (
            hours.parse::<u64>().context("parse ps CPU-time hours")?,
            *minutes,
            *seconds,
        ),
        _ => bail!("ps CPU time has unsupported format: {value}"),
    };
    let minutes = minutes
        .parse::<u64>()
        .context("parse ps CPU-time minutes")?;
    if fields.len() == 3 && minutes >= 60 {
        bail!("ps CPU-time minutes are out of range: {value}");
    }
    let seconds = seconds
        .parse::<f64>()
        .context("parse ps CPU-time seconds")?;
    if !seconds.is_finite() || !(0.0..60.0).contains(&seconds) {
        bail!("ps CPU-time seconds are out of range: {value}");
    }
    let base_seconds = days
        .checked_mul(24 * 60 * 60)
        .and_then(|value| value.checked_add(hours.checked_mul(60 * 60)?))
        .and_then(|value| value.checked_add(minutes.checked_mul(60)?))
        .context("ps CPU time overflowed Duration")?;
    Duration::from_secs(base_seconds)
        .checked_add(Duration::try_from_secs_f64(seconds).context("convert ps CPU-time seconds")?)
        .context("ps CPU time overflowed Duration")
}

fn process_cpu_percent(start: Duration, end: Duration, elapsed: Duration) -> Result<f64> {
    if elapsed.is_zero() {
        bail!("idle CPU measurement window was empty");
    }
    let cpu_time = end
        .checked_sub(start)
        .context("process CPU time moved backwards")?;
    Ok(cpu_time.as_secs_f64() / elapsed.as_secs_f64() * 100.0)
}

async fn run_headless_sample(binary: &Path) -> Result<HeadlessSample> {
    let server = MockServer::start().await;
    let state = Arc::new(Mutex::new(ScriptState::default()));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(PerformanceChat {
            state: state.clone(),
        })
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new().context("create isolated headless home")?;
    let project = tempfile::TempDir::new().context("create isolated headless project")?;
    prepare_headless_fixture(home.path(), project.path())?;

    let mut command = tokio::process::Command::new(binary);
    configure_headless_command(&mut command, home.path(), project.path(), &server);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = command.spawn().context("spawn release headless binary")?;
    let stdout = child.stdout.take().context("capture headless stdout")?;
    let mut stderr = child.stderr.take().context("capture headless stderr")?;
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let mut lines = BufReader::new(stdout).lines();
    let capture = tokio::time::timeout(HEADLESS_TIMEOUT, async {
        let mut first_assistant = None;
        let mut context_used_tokens = Vec::new();
        let mut provider_prompt_tokens = Vec::new();
        let mut final_event = None;
        while let Some(line) = lines.next_line().await? {
            let event: Value = serde_json::from_str(&line)
                .with_context(|| format!("headless stream emitted invalid JSON: {line}"))?;
            match event.get("type").and_then(Value::as_str) {
                Some("assistant_delta") => {
                    let nonempty = event
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|text| !text.is_empty());
                    if nonempty && first_assistant.is_none() {
                        first_assistant = Some(started.elapsed());
                    }
                }
                Some("context") => {
                    if let Some(tokens) = event.get("used_tokens").and_then(Value::as_u64) {
                        context_used_tokens.push(tokens);
                    }
                    if let Some(tokens) = event.get("last_prompt_tokens").and_then(Value::as_u64) {
                        provider_prompt_tokens.push(tokens);
                    }
                }
                Some("final") => final_event = Some(event),
                _ => {}
            }
        }
        let status = child.wait().await.context("wait for headless binary")?;
        Ok::<_, anyhow::Error>((
            status,
            first_assistant,
            context_used_tokens,
            provider_prompt_tokens,
            final_event,
        ))
    })
    .await;
    let (status, first_assistant, context_used_tokens, provider_prompt_tokens, final_event) =
        match capture {
            Ok(result) => result?,
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                bail!("headless performance sample exceeded {HEADLESS_TIMEOUT:?}");
            }
        };
    let stderr = stderr_task.await.context("join headless stderr reader")??;
    if !status.success() {
        bail!(
            "headless performance sample failed with {status}: {}",
            String::from_utf8_lossy(&stderr)
        );
    }
    let final_event = final_event.context("headless stream omitted final event")?;
    let (request_count, expected_cache_read_tokens) = {
        let state = state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (state.request_index, state.cache_read_tokens)
    };
    if request_count != TOOL_TURNS + 1 {
        bail!(
            "headless fixture made {request_count} provider requests; expected {}",
            TOOL_TURNS + 1
        );
    }
    if context_used_tokens.len() < TOOL_TURNS + 1 {
        bail!(
            "headless fixture emitted {} context samples; expected at least {}",
            context_used_tokens.len(),
            TOOL_TURNS + 1
        );
    }
    if provider_prompt_tokens.len() < TOOL_TURNS + 1 {
        bail!(
            "headless fixture emitted {} provider prompt-token samples; expected at least {}",
            provider_prompt_tokens.len(),
            TOOL_TURNS + 1
        );
    }
    let usage = final_event
        .get("usage")
        .context("headless final event omitted usage")?;
    let input_cache = usage
        .get("input_cache")
        .context("headless final event omitted input cache usage")?;
    let cache_read_tokens = required_u64(input_cache, "read_tokens")?;
    if cache_read_tokens != expected_cache_read_tokens {
        bail!(
            "headless fixture reported {cache_read_tokens} cache-read tokens; expected {expected_cache_read_tokens}"
        );
    }
    let input_cache_hit_rate_percent = required_u64(input_cache, "hit_rate_percent")?;
    if input_cache_hit_rate_percent == 0 {
        bail!("headless fixture cache hit rate rounded to zero");
    }
    let representative_task_cost_micros = required_u64(usage, "cost_micros")?;
    if representative_task_cost_micros == 0 {
        bail!("headless fixture did not resolve priced model metadata");
    }
    Ok(HeadlessSample {
        spawn_to_first_assistant_delta_ms: millis_u64(
            first_assistant.context("headless stream omitted assistant delta")?,
        ),
        persistence_duration_ms: final_event
            .get("persistence_duration_ms")
            .and_then(Value::as_u64)
            .context("headless final event omitted persistence_duration_ms")?,
        context_used_tokens,
        provider_prompt_tokens,
        prompt_tokens: required_u64(usage, "prompt_tokens")?,
        completion_tokens: required_u64(usage, "completion_tokens")?,
        input_cache_hit_rate_percent,
        representative_task_cost_micros,
    })
}

fn configure_headless_command(
    command: &mut tokio::process::Command,
    home: &Path,
    project: &Path,
    server: &MockServer,
) {
    command
        .current_dir(project)
        .env_clear()
        .env("HOME", home)
        .env("PATH", inherited_path())
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C")
        .env("BONSAI_HOME", home.join("bonsai-home"))
        .env("BONSAI_DOTENV", "0")
        .env("BONSAI_DISABLE_MODELS_FETCH", "1")
        .env("BONSAI_MEMORY_EMBEDDINGS", "off")
        .env("BONSAI_EPISODES", "0")
        .env("BONSAI_PROVIDER", "openai-compatible")
        .env(
            "OPENAI_COMPATIBLE_MODEL",
            "openai-compatible/performance-model",
        )
        .env(
            "OPENAI_COMPATIBLE_BASE_URL",
            format!("{}/v1", server.uri()),
        )
        .args([
            "-p",
            "Read sample-1.txt, sample-2.txt, and sample-3.txt in order, then answer fixture complete.",
            "--output-format",
            "stream-json",
            "--autonomy",
            "yolo",
            "--isolation",
            "off",
            "--max-turns",
            "8",
            "--timeout",
            "25",
        ]);
}

fn prepare_headless_fixture(home: &Path, project: &Path) -> Result<()> {
    for index in 1..=TOOL_TURNS {
        fs::write(
            project.join(format!("sample-{index}.txt")),
            format!(
                "deterministic performance fixture {index}\n{}\n",
                "x".repeat(256)
            ),
        )?;
    }
    let model_dir = home.join("bonsai-home/models");
    fs::create_dir_all(&model_dir)?;
    fs::write(
        model_dir.join("performance.toml"),
        r#"
[[targets]]
connection = "openai-compatible"
model = "openai-compatible/performance-model"
remote_model = "performance-model"
default = true
context_window = 131072
output_limit = 4096
token_counter = "heuristic"
features = ["tool-call"]
pricing = { input_micros_per_million = 2000000, output_micros_per_million = 10000000, cache_read_micros_per_million = 200000, cache_write_micros_per_million = 2500000 }
"#,
    )?;
    Ok(())
}

fn scripted_response(
    request_index: usize,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
) -> String {
    let mut frames = Vec::new();
    if request_index < TOOL_TURNS {
        let arguments = json!({"path": format!("sample-{}.txt", request_index + 1)}).to_string();
        let content = (request_index == 0).then_some("Inspecting the fixture.");
        frames.push(json!({
            "choices": [{
                "delta": {
                    "content": content,
                    "tool_calls": [{
                        "index": 0,
                        "id": format!("perf-call-{request_index}"),
                        "type": "function",
                        "function": {"name": "read", "arguments": arguments}
                    }]
                }
            }]
        }));
        frames.push(json!({
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        }));
    } else {
        frames.push(json!({
            "choices": [{"delta": {"content": "fixture complete"}}]
        }));
        frames.push(json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}]
        }));
    }
    frames.push(json!({
        "choices": [],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "prompt_tokens_details": {
                "cached_tokens": cached_tokens,
                "cache_write_tokens": 0
            }
        }
    }));
    let mut body = String::new();
    for frame in frames {
        body.push_str("data: ");
        body.push_str(&frame.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn load_baseline(identity: &RunIdentity) -> Result<Option<(String, PerformanceReport)>> {
    let Some(path) = std::env::var_os("BONSAI_PERF_BASELINE").map(PathBuf::from) else {
        return Ok(None);
    };
    let body = fs::read_to_string(&path)
        .with_context(|| format!("read performance baseline {}", path.display()))?;
    let report: PerformanceReport = serde_json::from_str(&body)
        .with_context(|| format!("parse performance baseline {}", path.display()))?;
    if report.schema_version != REPORT_SCHEMA_VERSION {
        bail!(
            "performance baseline {} uses schema {}, expected {}",
            path.display(),
            report.schema_version,
            REPORT_SCHEMA_VERSION
        );
    }
    if !same_comparison_class(&report.identity, identity) {
        bail!(
            "performance baseline target/runner/profile/toolchain {}/{}/{}/{} does not match current {}/{}/{}/{}",
            report.identity.target,
            report.identity.runner_class,
            report.identity.profile,
            report.identity.toolchain,
            identity.target,
            identity.runner_class,
            identity.profile,
            identity.toolchain
        );
    }
    Ok(Some((baseline_source(&path), report)))
}

fn same_comparison_class(baseline: &RunIdentity, current: &RunIdentity) -> bool {
    baseline.target == current.target
        && baseline.runner_class == current.runner_class
        && baseline.profile == current.profile
        && baseline.toolchain == current.toolchain
}

fn baseline_source(path: &Path) -> String {
    let Some(file_name) = path.file_name() else {
        return "baseline".to_string();
    };
    if path
        .parent()
        .is_some_and(|parent| parent.ends_with("eval/baselines/performance"))
    {
        return format!("eval/baselines/performance/{}", file_name.to_string_lossy());
    }
    file_name.to_string_lossy().into_owned()
}

fn compare_baseline(
    source: String,
    current_identity: &RunIdentity,
    current: &PerformanceSummary,
    baseline_identity: &RunIdentity,
    baseline: &PerformanceSummary,
) -> BaselineComparison {
    let mut violations = Vec::new();
    check_upper(
        &mut violations,
        "fresh startup p95",
        current.fresh_startup_ms.p95,
        baseline.fresh_startup_ms.p95,
        0.25,
        50,
        "ms",
    );
    check_upper(
        &mut violations,
        "returning startup p95",
        current.returning_startup_ms.p95,
        baseline.returning_startup_ms.p95,
        0.25,
        50,
        "ms",
    );
    check_upper_f64(
        &mut violations,
        "idle CPU median",
        current.idle_cpu_percent.median,
        baseline.idle_cpu_percent.median,
        0.50,
        1.0,
        "%",
    );
    check_upper(
        &mut violations,
        "idle RSS p95",
        current.idle_rss_bytes.p95,
        baseline.idle_rss_bytes.p95,
        0.20,
        16 * 1024 * 1024,
        "bytes",
    );
    check_upper(
        &mut violations,
        "spawn-to-first-output p95",
        current.spawn_to_first_assistant_delta_ms.p95,
        baseline.spawn_to_first_assistant_delta_ms.p95,
        0.25,
        75,
        "ms",
    );
    check_upper(
        &mut violations,
        "persistence p95",
        current.persistence_duration_ms.p95,
        baseline.persistence_duration_ms.p95,
        0.30,
        5,
        "ms",
    );
    check_upper(
        &mut violations,
        "binary size",
        current_identity.binary_bytes,
        baseline_identity.binary_bytes,
        0.02,
        256 * 1024,
        "bytes",
    );
    check_upper(
        &mut violations,
        "context peak",
        current.context_peak_tokens.median,
        baseline.context_peak_tokens.median,
        0.02,
        64,
        "tokens",
    );
    check_upper(
        &mut violations,
        "context growth",
        current.context_growth_tokens.median,
        baseline.context_growth_tokens.median,
        0.02,
        64,
        "tokens",
    );
    check_upper(
        &mut violations,
        "representative prompt tokens",
        current.prompt_tokens.median,
        baseline.prompt_tokens.median,
        0.02,
        64,
        "tokens",
    );
    check_upper(
        &mut violations,
        "representative task cost",
        current.representative_task_cost_micros.median,
        baseline.representative_task_cost_micros.median,
        0.02,
        10,
        "micros",
    );
    check_lower(
        &mut violations,
        "input-cache hit rate",
        current.input_cache_hit_rate_percent.median,
        baseline.input_cache_hit_rate_percent.median,
        0.02,
        2,
        "%",
    );
    BaselineComparison {
        source,
        baseline_git_commit: baseline_identity.git_commit.clone(),
        passed: violations.is_empty(),
        violations,
    }
}

fn check_upper(
    violations: &mut Vec<String>,
    label: &str,
    actual: u64,
    baseline: u64,
    relative_margin: f64,
    absolute_margin: u64,
    unit: &str,
) {
    let absolute_delta = actual.saturating_sub(baseline);
    let relative_limit = baseline as f64 * (1.0 + relative_margin);
    if actual as f64 > relative_limit && absolute_delta > absolute_margin {
        violations.push(format!(
            "{label} was {actual} {unit}, baseline {baseline} {unit} (+{absolute_delta} {unit})"
        ));
    }
}

fn check_upper_f64(
    violations: &mut Vec<String>,
    label: &str,
    actual: f64,
    baseline: f64,
    relative_margin: f64,
    absolute_margin: f64,
    unit: &str,
) {
    let absolute_delta = actual - baseline;
    let relative_limit = baseline * (1.0 + relative_margin);
    if actual > relative_limit && absolute_delta > absolute_margin {
        violations.push(format!(
            "{label} was {actual:.2}{unit}, baseline {baseline:.2}{unit} (+{absolute_delta:.2}{unit})"
        ));
    }
}

fn check_lower(
    violations: &mut Vec<String>,
    label: &str,
    actual: u64,
    baseline: u64,
    relative_margin: f64,
    absolute_margin: u64,
    unit: &str,
) {
    let absolute_delta = baseline.saturating_sub(actual);
    let relative_limit = baseline as f64 * (1.0 - relative_margin);
    if (actual as f64) < relative_limit && absolute_delta > absolute_margin {
        violations.push(format!(
            "{label} was {actual}{unit}, baseline {baseline}{unit} (-{absolute_delta}{unit})"
        ));
    }
}

fn performance_binary() -> Result<PathBuf> {
    let binary = std::env::var_os("BONSAI_PERF_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_bonsai")));
    let metadata = fs::metadata(&binary)
        .with_context(|| format!("read performance binary metadata {}", binary.display()))?;
    if !metadata.is_file() {
        bail!("performance binary is not a file: {}", binary.display());
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        bail!("performance binary is not executable: {}", binary.display());
    }
    fs::canonicalize(&binary)
        .with_context(|| format!("canonicalize performance binary {}", binary.display()))
}

fn run_identity(binary: &Path) -> Result<RunIdentity> {
    if cfg!(debug_assertions) {
        bail!("performance harness must run with cargo test --release");
    }
    let profile = "release".to_string();
    let toolchain_output = std::process::Command::new("rustc")
        .args(["--version", "--verbose"])
        .output()
        .context("run rustc --version --verbose")?;
    if !toolchain_output.status.success() {
        bail!("rustc --version --verbose failed");
    }
    let toolchain = String::from_utf8(toolchain_output.stdout)
        .context("rustc version output should be UTF-8")?;
    let target = toolchain
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .context("rustc version output omitted host target")?
        .to_string();
    let runner_class =
        std::env::var("BONSAI_PERF_RUNNER_CLASS").unwrap_or_else(|_| "local".to_string());
    if runner_class.is_empty()
        || !runner_class
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("BONSAI_PERF_RUNNER_CLASS must contain only ASCII letters, digits, '.', '_', or '-'");
    }
    let git_output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .context("resolve git commit")?;
    if !git_output.status.success() {
        bail!("git rev-parse HEAD failed");
    }
    let git_commit = String::from_utf8(git_output.stdout)
        .context("git commit should be UTF-8")?
        .trim()
        .to_string();
    Ok(RunIdentity {
        target,
        runner_class,
        profile,
        toolchain: toolchain.trim().to_string(),
        git_commit,
        binary_sha256: sha256_file(binary)?,
        binary_bytes: fs::metadata(binary)
            .with_context(|| format!("read binary metadata {}", binary.display()))?
            .len(),
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("open release binary {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn write_report_atomic(path: &Path, report: &PerformanceReport) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("create report directory {}", parent.display()))?;
    let mut body = serde_json::to_vec_pretty(report).context("serialize performance report")?;
    body.push(b'\n');
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("create temporary report in {}", parent.display()))?;
    temporary.write_all(&body)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| {
        anyhow::anyhow!(
            "persist performance report {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

fn sample_count() -> Result<usize> {
    let count = std::env::var("BONSAI_PERF_SAMPLES")
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .context("BONSAI_PERF_SAMPLES must be an integer")
        })
        .transpose()?
        .unwrap_or(DEFAULT_SAMPLE_COUNT);
    if !(MIN_SAMPLE_COUNT..=MAX_SAMPLE_COUNT).contains(&count) {
        bail!("BONSAI_PERF_SAMPLES must be between {MIN_SAMPLE_COUNT} and {MAX_SAMPLE_COUNT}");
    }
    Ok(count)
}

fn required_path(name: &str) -> Result<PathBuf> {
    let value = std::env::var_os(name).with_context(|| format!("{name} is required"))?;
    let path = PathBuf::from(value);
    if path.as_os_str().is_empty() {
        bail!("{name} must not be empty");
    }
    Ok(path)
}

fn u64_distribution(values: &[u64]) -> Result<U64Distribution> {
    if values.is_empty() {
        bail!("cannot summarize an empty integer sample");
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    Ok(U64Distribution {
        median: median_u64(&sorted),
        p95: percentile_index(&sorted, 95),
        min: sorted[0],
        max: sorted[sorted.len() - 1],
    })
}

fn f64_distribution(values: &[f64]) -> Result<F64Distribution> {
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        bail!("floating-point samples must be non-empty and finite");
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Ok(F64Distribution {
        median: median_f64(&sorted),
        p95: percentile_index(&sorted, 95),
        min: sorted[0],
        max: sorted[sorted.len() - 1],
    })
}

fn median_u64(sorted: &[u64]) -> u64 {
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        sorted[middle - 1]
            .saturating_add(sorted[middle])
            .saturating_div(2)
    } else {
        sorted[middle]
    }
}

fn median_f64(sorted: &[f64]) -> f64 {
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

fn percentile_index<T: Copy>(sorted: &[T], percentile: usize) -> T {
    let rank = sorted
        .len()
        .saturating_mul(percentile)
        .saturating_add(99)
        .saturating_div(100)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[rank]
}

fn required_u64(value: &Value, field: &str) -> Result<u64> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .with_context(|| format!("headless final event omitted numeric {field}"))
}

fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn inherited_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noisy_regression_requires_relative_and_absolute_margin() {
        let mut violations = Vec::new();
        check_upper(&mut violations, "startup", 126, 100, 0.25, 50, "ms");
        assert!(violations.is_empty());
        check_upper(&mut violations, "startup", 176, 100, 0.25, 50, "ms");
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn distributions_use_nearest_rank_p95() {
        let distribution = u64_distribution(&[5, 1, 3, 4, 2]).unwrap();
        assert_eq!(distribution.median, 3);
        assert_eq!(distribution.p95, 5);
    }

    #[test]
    fn parses_portable_ps_cpu_time_formats() {
        assert_eq!(
            parse_process_cpu_time("0:01.25").unwrap(),
            Duration::from_millis(1_250)
        );
        assert_eq!(
            parse_process_cpu_time("01:02:03").unwrap(),
            Duration::from_secs(3_723)
        );
        assert_eq!(
            parse_process_cpu_time("2-03:04:05.50").unwrap(),
            Duration::from_millis(183_845_500)
        );
    }

    #[test]
    fn parses_linux_proc_process_sample() {
        let stat = "4242 (bonsai ) worker) S 1 2 3 4 5 6 7 8 9 10 120 30";
        assert_eq!(
            parse_linux_proc_cpu_time(stat, 100).unwrap(),
            Duration::from_millis(1_500)
        );
        assert_eq!(
            parse_linux_proc_rss_bytes("Name:\tbonsai\nVmRSS:\t 1234 kB\n").unwrap(),
            1_263_616
        );
    }

    #[test]
    fn idle_cpu_uses_process_time_delta_over_wall_time() {
        let percent = process_cpu_percent(
            Duration::from_secs(2),
            Duration::from_millis(2_150),
            Duration::from_secs(3),
        )
        .unwrap();

        assert!((percent - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn cache_expectation_uses_the_actual_capped_value() {
        assert_eq!(fixture_cache_tokens(3, 9_000), 9_000);
        assert_eq!(fixture_cache_tokens(TOOL_TURNS + 1, 9_000), 0);
    }

    #[test]
    fn canonical_baseline_source_preserves_only_the_repo_relative_suffix() {
        let expected = "eval/baselines/performance/aarch64-apple-darwin.json";
        assert_eq!(
            baseline_source(Path::new(
                "/private/workspace/eval/baselines/performance/aarch64-apple-darwin.json"
            )),
            expected
        );
        assert_eq!(baseline_source(Path::new(expected)), expected);
    }

    #[test]
    fn noncanonical_absolute_baseline_source_falls_back_to_filename() {
        assert_eq!(
            baseline_source(Path::new("/private/workspace/reviewed.json")),
            "reviewed.json"
        );
        assert_eq!(
            baseline_source(Path::new(
                "/private/not-eval/baselines/performance/reviewed.json"
            )),
            "reviewed.json"
        );
    }

    #[test]
    fn runner_class_is_part_of_the_baseline_comparison_class() {
        let current = test_identity("github-macos-15-arm64");
        let mut baseline = current.clone();
        assert!(same_comparison_class(&baseline, &current));

        baseline.runner_class = "local".to_string();
        assert!(!same_comparison_class(&baseline, &current));
    }

    fn test_identity(runner_class: &str) -> RunIdentity {
        RunIdentity {
            target: "aarch64-apple-darwin".to_string(),
            runner_class: runner_class.to_string(),
            profile: "release".to_string(),
            toolchain: "rustc test".to_string(),
            git_commit: "a".repeat(40),
            binary_sha256: "b".repeat(64),
            binary_bytes: 1,
        }
    }
}
