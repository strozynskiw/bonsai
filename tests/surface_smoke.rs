#![cfg(unix)]

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOOL_CALL_RESPONSE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"pwd\\\"}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: [DONE]\n\n",
);
const COMPLETION_RESPONSE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"fixture complete\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

#[derive(Clone)]
struct ScriptedChat {
    requests: Arc<AtomicUsize>,
}

impl Respond for ScriptedChat {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let response = if self.requests.fetch_add(1, Ordering::SeqCst) == 0 {
            TOOL_CALL_RESPONSE
        } else {
            COMPLETION_RESPONSE
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(response)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_binary_headless_task_runs_tool_and_emits_completion_contract() -> Result<()> {
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ScriptedChat {
            requests: requests.clone(),
        })
        .mount(&server)
        .await;
    let home = tempfile::TempDir::new()?;
    let project = tempfile::TempDir::new()?;
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_bonsai"));
    command
        .current_dir(project.path())
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", inherited_path())
        .env("BONSAI_HOME", home.path().join("bonsai-home"))
        .env("BONSAI_DOTENV", "0")
        .env("BONSAI_DISABLE_MODELS_FETCH", "1")
        .env("BONSAI_MEMORY_EMBEDDINGS", "off")
        .env("BONSAI_EPISODES", "0")
        .env("BONSAI_PROVIDER", "openai-compatible")
        .env("OPENAI_COMPATIBLE_MODEL", "acceptance-model")
        .env("OPENAI_COMPATIBLE_BASE_URL", format!("{}/v1", server.uri()))
        .args([
            "-p",
            "Run pwd once, then report completion.",
            "--output-format",
            "json",
            "--autonomy",
            "yolo",
            "--isolation",
            "off",
        ]);

    let output = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .context("headless smoke task timed out")??;
    if !output.status.success() {
        bail!(
            "headless smoke failed with {}:\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let result: Value = serde_json::from_slice(&output.stdout)
        .context("headless smoke should emit one JSON result")?;

    assert_eq!(result["schema_version"], 1);
    assert_eq!(result["status"], "completed");
    assert_eq!(result["output"], "fixture complete");
    assert_eq!(result["completion_report"]["status"], "completed");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    Ok(())
}

#[test]
fn real_binary_tui_opens_native_terminal_and_exits_cleanly() -> Result<()> {
    let home = tempfile::TempDir::new()?;
    let project = tempfile::TempDir::new()?;
    let output = run_tui_smoke(home.path(), project.path())?;
    let rendered = String::from_utf8_lossy(&output);

    assert!(
        rendered.to_ascii_lowercase().contains("bonsai"),
        "TUI never rendered its application frame: {rendered:?}"
    );
    assert!(
        output.windows(2).any(|window| window == b"\x1b["),
        "TUI did not emit terminal control sequences"
    );
    Ok(())
}

fn run_tui_smoke(home: &Path, project: &Path) -> Result<Vec<u8>> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("native PTY should be available")?;
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_bonsai"));
    command.cwd(project);
    command.env_clear();
    command.env("HOME", home);
    command.env("PATH", inherited_path());
    command.env("TERM", "xterm-256color");
    command.env("LANG", "C.UTF-8");
    command.env("BONSAI_HOME", home.join("bonsai-home"));
    command.env("BONSAI_DOTENV", "0");
    command.env("BONSAI_DISABLE_MODELS_FETCH", "1");
    command.env("BONSAI_MEMORY_EMBEDDINGS", "off");
    command.env("BONSAI_EPISODES", "0");
    command.env("BONSAI_PROVIDER", "openai-compatible");
    command.env("OPENAI_COMPATIBLE_MODEL", "acceptance-model");

    let mut child = pair
        .slave
        .spawn_command(command)
        .context("TUI binary should start in the PTY")?;
    drop(pair.slave);
    let mut killer = child.clone_killer();
    let mut reader = pair
        .master
        .try_clone_reader()
        .context("PTY output reader should open")?;
    let mut writer = pair
        .master
        .take_writer()
        .context("PTY input writer should open")?;
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
                // Linux PTY masters report EIO after the slave closes.
                Err(error) if error.raw_os_error() == Some(5) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    });
    let (status_tx, status_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = status_tx.send(child.wait());
    });

    if let Err(error) = wait_for_tui_frame(&output, Duration::from_secs(20)) {
        let _ = killer.kill();
        return Err(error);
    }
    writer.write_all(b"\x1b")?;
    writer.flush()?;
    std::thread::sleep(Duration::from_millis(250));
    let _ = writer.write_all(b"/quit\r");
    let _ = writer.flush();

    let status = match status_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(status) => status.context("TUI process should return a status")?,
        Err(_) => {
            killer.kill().context("timed-out TUI process should stop")?;
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
        .context("PTY output should drain")?;
    if !status.success() {
        bail!("TUI smoke exited with status {status:?}");
    }
    Arc::try_unwrap(output)
        .map_err(|_| anyhow::anyhow!("PTY output still has outstanding readers"))?
        .into_inner()
        .map_err(|_| anyhow::anyhow!("PTY output lock was poisoned"))
}

fn wait_for_tui_frame(output: &Mutex<Vec<u8>>, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let rendered = output
            .lock()
            .map_err(|_| anyhow::anyhow!("PTY output lock was poisoned"))?;
        if String::from_utf8_lossy(&rendered)
            .to_ascii_lowercase()
            .contains("bonsai")
        {
            return Ok(());
        }
        drop(rendered);
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!("TUI did not render within {timeout:?}")
}

fn inherited_path() -> String {
    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string())
}
