#![cfg(unix)]

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

fn binary() -> PathBuf {
    std::env::var_os("BONSAI_SURFACE_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_bonsai")))
}
fn command(home: &Path, workspace: &Path, provider: &str) -> Command {
    let mut cmd = Command::new(binary());
    cmd.current_dir(workspace)
        .env_clear()
        .kill_on_drop(true)
        .env("HOME", home)
        .env("BONSAI_HOME", home.join("state"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("BONSAI_DOTENV", "0")
        .env("BONSAI_DISABLE_KEYRING", "1")
        .env("BONSAI_DISABLE_MODELS_FETCH", "1")
        .env("BONSAI_MEMORY_EMBEDDINGS", "off")
        .env("BONSAI_EPISODES", "0")
        .env("BONSAI_PROVIDER", "openai-compatible")
        .env("OPENAI_COMPATIBLE_MODEL", "acceptance-model")
        .env("OPENAI_COMPATIBLE_API_KEY", "qualification-synthetic")
        .env("OPENAI_COMPATIBLE_BASE_URL", format!("{provider}/v1"));
    cmd
}
struct Server {
    child: Child,
    url: String,
    token: String,
    client: reqwest::Client,
}
impl Server {
    async fn start(home: &Path, workspace: &Path, provider: &str) -> Result<Self> {
        let mut child = command(home, workspace, provider)
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = child.stderr.take().context("missing stderr")?;
        let mut lines = BufReader::new(stderr).lines();
        let (url, token) = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(line) = lines.next_line().await? {
                if let Some(line) = line.strip_prefix("Experimental server ") {
                    let (url, rest) = line.split_once("; token ").context("startup URL")?;
                    let (_, file) = rest.split_once(" in ").context("startup token path")?;
                    return Ok::<_, anyhow::Error>((
                        format!("{url}/api/experimental/v0"),
                        tokio::fs::read_to_string(file).await?,
                    ));
                }
            }
            anyhow::bail!("server exited before readiness")
        })
        .await
        .context("server readiness timeout")??;
        // Drain diagnostics without retaining sensitive session content.
        tokio::spawn(async move { while lines.next_line().await.ok().flatten().is_some() {} });
        Ok(Self {
            child,
            url,
            token,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(45))
                .build()?,
        })
    }
    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let r = self
            .client
            .post(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        let status = r.status();
        let body: Value = r.json().await?;
        anyhow::ensure!(status.is_success(), "POST {path}: {status} {body}");
        Ok(body)
    }
    async fn get(&self, path: &str) -> Result<Value> {
        Ok(self
            .client
            .get(format!("{}{path}", self.url))
            .bearer_auth(&self.token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }
    async fn python(&self, args: &[&str]) -> Result<Value> {
        use std::io::Write;
        let mut token = tempfile::NamedTempFile::new()?;
        token.write_all(self.token.as_bytes())?;
        let output = Command::new("python3")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/server_client.py"))
            .args(["--url", &self.url, "--token-file"])
            .arg(token.path())
            .args(args)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "Python client: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    }

    async fn stop(&mut self) -> Result<()> {
        let pid = self.child.id().context("server PID")?;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGINT,
        )?;
        anyhow::ensure!(
            tokio::time::timeout(Duration::from_secs(35), self.child.wait())
                .await??
                .success(),
            "server shutdown failed"
        );
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_restart_never_replays_pending_approval_or_accepted_run() -> Result<()> {
    let mock = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &Request| {
            c.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(tool("bash", json!({"command":"touch must-not-exist"}), 0))
        })
        .mount(&mock)
        .await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut server = Server::start(home.path(), workspace.path(), &mock.uri()).await?;
    let id = server
        .post("/sessions", json!({"request_id":"create"}))
        .await?["session_id"]
        .as_i64()
        .context("session")?;
    server
        .post(
            &format!("/sessions/{id}/resume"),
            json!({"request_id":"resume"}),
        )
        .await?;
    server
        .post(
            &format!("/sessions/{id}/messages"),
            json!({"request_id":"accepted","text":"Run the fixture"}),
        )
        .await?;
    let pending = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let s = server.get(&format!("/sessions/{id}")).await?;
            if s["pending"].as_array().is_some_and(|p| !p.is_empty()) {
                return Ok::<_, anyhow::Error>(s);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    let request = pending["pending"][0]["request_id"]
        .as_u64()
        .context("prompt id")?;
    server.child.kill().await?;
    server.child.wait().await?;
    assert!(!workspace.path().join("must-not-exist").exists());
    let mut restarted = Server::start(home.path(), workspace.path(), &mock.uri()).await?;
    // Legacy TUI/headless claims also use the conservative 15-second heartbeat
    // fence. A dead server must recover once that fence expires, never autorun.
    tokio::time::timeout(Duration::from_secs(22), async {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let response = restarted
                .client
                .post(format!("{}/sessions/{id}/resume", restarted.url))
                .bearer_auth(&restarted.token)
                .json(&json!({"request_id":format!("explicit-recovery-{attempt}")}))
                .send()
                .await?;
            if response.status().is_success() {
                break Ok::<_, anyhow::Error>(());
            }
            anyhow::ensure!(
                response.status() == reqwest::StatusCode::CONFLICT,
                "unexpected recovery status: {}",
                response.status()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("dead owner recovery timed out")??;
    let snapshot = restarted.get(&format!("/sessions/{id}")).await?;
    assert_eq!(snapshot["state"], "idle");
    assert_eq!(snapshot["pending"], json!([]));
    let retried = restarted
        .post(
            &format!("/sessions/{id}/messages"),
            json!({"request_id":"accepted","text":"Run the fixture"}),
        )
        .await?;
    assert_eq!(retried["state"], "interrupted");
    let stale=restarted.client.post(format!("{}/sessions/{id}/interactions/{request}/responses",restarted.url)).bearer_auth(&restarted.token).json(&json!({"request_id":"stale","run_id":pending["run_id"],"answer":{"kind":"permission","decision":"allow_once"}})).send().await?;
    assert_eq!(stale.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!workspace.path().join("must-not-exist").exists());
    restarted.stop().await?;
    Ok(())
}

fn fixture_response(n: usize) -> ResponseTemplate {
    let body = match n {
        0 => tool("bash", json!({"command":"printf original > report.txt"}), n),
        1 => tool("read", json!({"path":"report.txt"}), n),
        2 => tool(
            "edit",
            json!({"path":"report.txt","old_string":"original","new_string":"qualified"}),
            n,
        ),
        3 => DONE.into(),
        _ => {
            return ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(DONE)
                .set_delay(Duration::from_secs(20));
        }
    };
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn tool(name: &str, args: Value, id: usize) -> String {
    let delta = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("call-{id}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]});
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}
const DONE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"fixture complete\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

async fn settle(server: &Server, session: i64, approvals: bool) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let s = server.get(&format!("/sessions/{session}")).await?;
            for pending in s["pending"].as_array().into_iter().flatten() {
                anyhow::ensure!(approvals, "unexpected prompt: {pending}");
                anyhow::ensure!(
                    matches!(pending["kind"].as_str(), Some("sandbox" | "permission")),
                    "unexpected approval type"
                );
                let kind = if pending["kind"] == "sandbox" {
                    "sandbox"
                } else {
                    "permission"
                };
                let session_arg = session.to_string();
                let request_arg = pending["request_id"].to_string();
                let run = s["run_id"].as_str().context("run ID")?;
                let answer = json!({"kind":kind,"decision":"allow_once"}).to_string();
                server
                    .python(&["answer", &session_arg, run, &request_arg, &answer])
                    .await?;
            }
            if s["state"] != "running" {
                return Ok::<_, anyhow::Error>(s);
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .context("run settlement timeout")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_server_approval_diff_interrupt_restart_and_python_client() -> Result<()> {
    let mock = MockServer::start().await;
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &Request| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            fixture_response(n)
        })
        .mount(&mock)
        .await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut server = Server::start(home.path(), workspace.path(), &mock.uri()).await?;
    let created = server.python(&["--request-id", "create", "create"]).await?;
    let id = created["session_id"].as_i64().context("session ID")?;
    server
        .post(
            &format!("/sessions/{id}/resume"),
            json!({"request_id":"resume"}),
        )
        .await?;
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "create/resume must not run model"
    );
    // Local CLI can delegate read without control, and revoke it immediately.
    let delegated_file = home.path().join("delegated-token");
    let minted = command(home.path(), workspace.path(), &mock.uri())
        .args(["serve", "token", "mint", "--output"])
        .arg(&delegated_file)
        .args(["--scope", "read", "--session", &id.to_string()])
        .output()
        .await?;
    anyhow::ensure!(
        minted.status.success(),
        "mint: {}",
        String::from_utf8_lossy(&minted.stderr)
    );
    let delegated = tokio::fs::read_to_string(&delegated_file).await?;
    assert!(!String::from_utf8_lossy(&minted.stdout).contains(delegated.trim()));
    let token_id = String::from_utf8_lossy(&minted.stdout)
        .split_whitespace()
        .nth(1)
        .context("minted token ID")?
        .to_owned();
    assert!(
        server
            .client
            .get(format!("{}/sessions/{id}", server.url))
            .bearer_auth(delegated.trim())
            .send()
            .await?
            .status()
            .is_success()
    );
    assert_eq!(
        server
            .client
            .post(format!("{}/sessions/{id}/interrupt", server.url))
            .bearer_auth(delegated.trim())
            .json(&json!({"request_id":"forbidden"}))
            .send()
            .await?
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let revoked = command(home.path(), workspace.path(), &mock.uri())
        .args(["serve", "token", "revoke", &token_id])
        .output()
        .await?;
    anyhow::ensure!(revoked.status.success(), "revoke failed");
    assert_eq!(
        server
            .client
            .get(format!("{}/sessions/{id}", server.url))
            .bearer_auth(delegated.trim())
            .send()
            .await?
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    // Run the example's actual fetch-stream path; reading a frame cannot approve.
    let mut python_token = tempfile::NamedTempFile::new()?;
    std::io::Write::write_all(&mut python_token, server.token.as_bytes())?;
    let mut python_events = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/server_client.py"))
        .args(["--url", &server.url, "--token-file"])
        .arg(python_token.path())
        .args(["events", &id.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines = BufReader::new(python_events.stdout.take().context("Python stream")?).lines();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(line) = lines.next_line().await? {
            if line == "event: snapshot" {
                return Ok::<_, anyhow::Error>(());
            }
        }
        anyhow::bail!("Python stream ended before snapshot")
    })
    .await??;
    python_events.kill().await?;
    let events_path = format!("/sessions/{id}/events");
    let mut stream = server
        .client
        .get(format!("{}{events_path}", server.url))
        .bearer_auth(&server.token)
        .send()
        .await?
        .error_for_status()?;
    let initial = String::from_utf8(stream.chunk().await?.context("snapshot")?.to_vec())?;
    let cursor = initial
        .lines()
        .find_map(|l| l.strip_prefix("id: "))
        .context("snapshot cursor")?
        .to_owned();
    drop(stream);
    let admitted = server
        .post(
            &format!("/sessions/{id}/messages"),
            json!({"request_id":"chat","text":"Create the fixture, read it and edit it."}),
        )
        .await?;
    assert!(admitted["run_id"].is_string());
    let snapshot = settle(&server, id, true).await?;
    assert_eq!(
        tokio::fs::read_to_string(workspace.path().join("report.txt"))
            .await
            .with_context(|| format!("fixture output missing; snapshot: {snapshot}"))?,
        "qualified"
    );
    let serialized = serde_json::to_string(&snapshot)?;
    assert!(serialized.contains("fixture complete"), "{serialized}");
    let tools = snapshot["transcript"].as_array().context("transcript")?;
    assert!(
        tools.iter().any(
            |item| item["data"]["tool_result"]["content"]["type"] == "edit"
                && item["data"]["tool_result"]["status"] == "succeeded"
        ),
        "typed successful diff must survive snapshot: {serialized}"
    );
    assert!(snapshot["transcript"].to_string().contains("user_approved"));
    assert_eq!(count.load(Ordering::SeqCst), 4);
    let requests = mock.received_requests().await.context("mock requests")?;
    assert!(
        requests
            .iter()
            .any(|r| String::from_utf8_lossy(&r.body).contains("original")),
        "tool result reaches provider"
    );
    // Only the backend-generated report is published, not arbitrary tool files.
    let artifacts = server.get(&format!("/sessions/{id}/artifacts")).await?;
    let records = artifacts["artifacts"].as_array().context("artifact list")?;
    assert_eq!(records.len(), 1);
    assert!(
        records[0]["name"]
            .as_str()
            .unwrap()
            .starts_with("completion-")
    );
    let download = server
        .client
        .get(format!(
            "{}/sessions/{id}/artifacts/{}",
            server.url,
            records[0]["id"].as_str().unwrap()
        ))
        .bearer_auth(&server.token)
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(download.headers()["x-content-type-options"], "nosniff");
    assert!(
        download.headers()["content-disposition"]
            .to_str()?
            .starts_with("attachment;")
    );
    let bytes = download.bytes().await?;
    assert_eq!(bytes.len() as u64, records[0]["size"].as_u64().unwrap());
    let report: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(report["status"], "completed");
    let python_list = server.python(&["list"]).await?;
    assert!(python_list.to_string().contains(&id.to_string()));
    let token_file = home.path().join("client-token");
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(&token_file, &server.token)?;
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))?;
    let client = Command::new("python3")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/server_client.py"))
        .args(["--url", &server.url, "--token-file"])
        .arg(&token_file)
        .args(["get", &id.to_string()])
        .output()
        .await?;
    anyhow::ensure!(
        client.status.success(),
        "Python client: {}",
        String::from_utf8_lossy(&client.stderr)
    );
    let from_python: Value = serde_json::from_slice(&client.stdout)?;
    assert_eq!(from_python["session_id"], id);
    server
        .post(
            &format!("/sessions/{id}/messages"),
            json!({"request_id":"interrupt-me","text":"Wait."}),
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while count.load(Ordering::SeqCst) < 5 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    server
        .post(
            &format!("/sessions/{id}/interrupt"),
            json!({"request_id":"interrupt"}),
        )
        .await?;
    let interrupted = settle(&server, id, false).await?;
    assert!(
        interrupted["transcript"]
            .as_array()
            .context("transcript")?
            .iter()
            .any(|item| item["type"] == "completion_report"
                && item["data"]["status"] == "interrupted")
    );
    server.stop().await?;
    let before = count.load(Ordering::SeqCst);
    let mut restarted = Server::start(home.path(), workspace.path(), &mock.uri()).await?;
    let saved = restarted.get(&format!("/sessions/{id}")).await?;
    assert_eq!(saved["state"], "unloaded");
    restarted
        .post(
            &format!("/sessions/{id}/resume"),
            json!({"request_id":"restart-resume"}),
        )
        .await?;
    assert_eq!(
        count.load(Ordering::SeqCst),
        before,
        "restart must not replay accepted work"
    );
    assert!(
        restarted
            .get(&format!("/sessions/{id}"))
            .await?
            .to_string()
            .contains("fixture complete")
    );
    let mut replay = restarted
        .client
        .get(format!("{}{events_path}", restarted.url))
        .bearer_auth(&restarted.token)
        .header("last-event-id", cursor)
        .send()
        .await?;
    let reset = String::from_utf8(replay.chunk().await?.context("reset")?.to_vec())?;
    assert!(reset.contains("event: reset"), "{reset}");
    drop(replay);
    restarted.stop().await?;
    // Same model/tool script through headless. Its noninteractive surface must
    // receive explicit operator autonomy; server used correlated allow-once.
    let headless_mock = MockServer::start().await;
    let headless_calls = Arc::new(AtomicUsize::new(0));
    let c = headless_calls.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &Request| fixture_response(c.fetch_add(1, Ordering::SeqCst)))
        .mount(&headless_mock)
        .await;
    let headless_home = tempfile::tempdir()?;
    let headless_workspace = tempfile::tempdir()?;
    let output = tokio::time::timeout(
        Duration::from_secs(40),
        command(
            headless_home.path(),
            headless_workspace.path(),
            &headless_mock.uri(),
        )
        .args([
            "-p",
            "Create the fixture, read it and edit it.",
            "--output-format",
            "json",
            "--autonomy",
            "yolo",
            "--isolation",
            "off",
            "--max-turns",
            "6",
        ])
        .output(),
    )
    .await??;
    anyhow::ensure!(
        output.status.success(),
        "headless comparison: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let headless: Value = serde_json::from_slice(&output.stdout)?;
    assert!(headless.to_string().contains("fixture complete"));
    assert_eq!(
        tokio::fs::read_to_string(headless_workspace.path().join("report.txt")).await?,
        "qualified"
    );
    assert_eq!(headless_calls.load(Ordering::SeqCst), 4);
    Ok(())
}
