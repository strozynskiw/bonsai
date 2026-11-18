//! Drives the real `bonsai acp` binary over stdio as an ACP v1 client would
//! (Zed's agent panel speaks the same newline-delimited JSON-RPC 2.0).
#![cfg(unix)]

use anyhow::{Context, Result, bail};
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
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
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

/// How the scripted client answers `session/request_permission`.
#[derive(Clone, Copy)]
enum Answer {
    Option(&'static str),
    Cancelled,
}

struct Acp {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    answer: Answer,
    /// Every `session/update` payload, in arrival order.
    updates: Vec<Value>,
    /// Every agent → client request received (method, params).
    requests: Vec<Value>,
}

impl Acp {
    fn spawn(home: &Path, workspace: &Path, provider: &str) -> Result<Self> {
        Self::spawn_with(home, workspace, provider, &[])
    }

    fn spawn_with(
        home: &Path,
        workspace: &Path,
        provider: &str,
        env: &[(&str, &str)],
    ) -> Result<Self> {
        let mut child = Command::new(binary())
            .arg("acp")
            .current_dir(workspace)
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
            .env("OPENAI_COMPATIBLE_BASE_URL", format!("{provider}/v1"))
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().context("stdin")?;
        let stdout = child.stdout.take().context("stdout")?;
        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            next_id: 0,
            answer: Answer::Option("allow_once"),
            updates: Vec::new(),
            requests: Vec::new(),
        })
    }

    async fn write(&mut self, message: Value) -> Result<()> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    /// Send a request without waiting; returns its id.
    async fn send(&mut self, method: &str, params: Value) -> Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        self.write(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        Ok(id)
    }

    /// Pump the stream until the response to `id`, answering agent requests.
    async fn response(&mut self, id: u64) -> Result<Value> {
        tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await?
                    .context("agent closed stdout")?;
                let message: Value = serde_json::from_str(&line)
                    .with_context(|| format!("stdout must carry only JSON-RPC: {line}"))?;
                assert_eq!(message["jsonrpc"], "2.0", "{message}");
                match (message.get("method"), message.get("id")) {
                    (Some(method), Some(request)) => {
                        let request = request.clone();
                        anyhow::ensure!(
                            method == "session/request_permission",
                            "unexpected agent request {message}"
                        );
                        self.requests.push(message["params"].clone());
                        let outcome = match self.answer {
                            Answer::Option(option) => {
                                json!({"outcome":"selected","optionId":option})
                            }
                            Answer::Cancelled => json!({"outcome":"cancelled"}),
                        };
                        self.write(
                            json!({"jsonrpc":"2.0","id":request,"result":{"outcome":outcome}}),
                        )
                        .await?;
                    }
                    (Some(method), None) => {
                        anyhow::ensure!(method == "session/update", "unexpected {message}");
                        self.updates.push(message["params"]["update"].clone());
                    }
                    (None, Some(response)) if response == &json!(id) => return Ok(message),
                    _ => bail!("unexpected message {message}"),
                }
            }
        })
        .await
        .context("ACP response timeout")?
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send(method, params).await?;
        let response = self.response(id).await?;
        match response.get("result") {
            Some(result) => Ok(result.clone()),
            None => bail!("{method} failed: {}", response["error"]),
        }
    }

    async fn error(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.send(method, params).await?;
        let response = self.response(id).await?;
        response
            .get("error")
            .cloned()
            .with_context(|| format!("{method} unexpectedly succeeded: {response}"))
    }

    async fn initialize(&mut self) -> Result<Value> {
        self.call(
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false},"clientInfo":{"name":"acp-smoke","version":"0"}}),
        )
        .await
    }

    async fn new_session(&mut self, cwd: &Path) -> Result<String> {
        let result = self
            .call("session/new", json!({"cwd":cwd,"mcpServers":[]}))
            .await?;
        result["sessionId"]
            .as_str()
            .map(str::to_owned)
            .context("sessionId")
    }

    async fn prompt(&mut self, session: &str, text: &str) -> Result<Value> {
        self.call(
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":text}]}),
        )
        .await
    }

    fn updates_of<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = &'a Value> + 'a {
        self.updates
            .iter()
            .filter(move |update| update["sessionUpdate"] == kind)
    }

    fn agent_text(&self) -> String {
        self.updates_of("agent_message_chunk")
            .filter_map(|u| u["content"]["text"].as_str())
            .collect()
    }

    /// Closing stdin is how an editor ends the connection: bonsai must drain
    /// and exit cleanly.
    async fn close(mut self) -> Result<()> {
        drop(self.stdin);
        let status = tokio::time::timeout(Duration::from_secs(35), self.child.wait())
            .await
            .context("agent did not exit after stdin closed")??;
        anyhow::ensure!(status.success(), "agent exited with {status}");
        Ok(())
    }
}

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body.to_owned())
}

fn tool(name: &str, args: Value, id: usize) -> String {
    let delta = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("call-{id}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]});
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

const REASONED_TEXT: &str = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"considering\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"fixture \"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"complete\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
const DONE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"fixture complete\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

/// Mount a scripted chat endpoint: response `n` comes from `script(n)`.
async fn provider(
    script: impl Fn(usize) -> ResponseTemplate + Send + Sync + 'static,
) -> (MockServer, Arc<AtomicUsize>) {
    let mock = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &Request| script(counter.fetch_add(1, Ordering::SeqCst)))
        .mount(&mock)
        .await;
    (mock, calls)
}

/// Slice 1 bar: initialize → session/new → session/prompt over real stdio,
/// then session/load replays the durable history on the same connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcript_initialize_new_prompt_and_load() -> Result<()> {
    let (mock, _) = provider(|_| sse(REASONED_TEXT)).await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;

    let init = acp.initialize().await?;
    assert_eq!(init["protocolVersion"], 1);
    assert_eq!(init["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        init["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
        true
    );
    assert_eq!(init["agentInfo"]["name"], "bonsai");
    assert_eq!(init["authMethods"], json!([]));

    let session = acp.new_session(workspace.path()).await?;
    assert!(session.parse::<i64>().is_ok(), "session id {session}");

    let done = acp.prompt(&session, "Say hello").await?;
    assert_eq!(done["stopReason"], "end_turn");
    assert_eq!(acp.agent_text(), "fixture complete");
    assert!(
        acp.updates_of("agent_thought_chunk")
            .any(|u| u["content"]["text"] == "considering"),
        "reasoning streams as thought chunks: {:?}",
        acp.updates
    );

    acp.updates.clear();
    let loaded = acp
        .call(
            "session/load",
            json!({"sessionId":session,"cwd":workspace.path(),"mcpServers":[]}),
        )
        .await?;
    assert_eq!(
        loaded["configOptions"][0]["currentValue"],
        "openai-compatible::acceptance-model"
    );
    assert!(
        acp.updates_of("user_message_chunk")
            .any(|u| u["content"]["text"] == "Say hello"),
        "history replays the user turn: {:?}",
        acp.updates
    );
    assert_eq!(acp.agent_text(), "fixture complete");

    // Unknown sessions and methods fail as JSON-RPC errors, not crashes.
    let missing = acp
        .error(
            "session/prompt",
            json!({"sessionId":"999999","prompt":[{"type":"text","text":"x"}]}),
        )
        .await?;
    assert_eq!(missing["code"], -32002);
    acp.close().await
}

/// Slices 3–4: tool calls stream, a mutating command raises a permission
/// prompt carrying only the user's decision, and the edit arrives as a diff.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_call_permission_and_edit_diff() -> Result<()> {
    let (mock, calls) = provider(|n| {
        sse(&match n {
            0 => tool("bash", json!({"command":"printf original > report.txt"}), n),
            1 => tool("read", json!({"path":"report.txt"}), n),
            2 => tool(
                "edit",
                json!({"path":"report.txt","old_string":"original","new_string":"qualified"}),
                n,
            ),
            _ => DONE.into(),
        })
    })
    .await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let session = acp.new_session(workspace.path()).await?;

    // ACP has no path to raise autonomy: mode switching is not offered.
    let refused = acp
        .error(
            "session/set_mode",
            json!({"sessionId":session,"modeId":"yolo"}),
        )
        .await?;
    assert_eq!(refused["code"], -32601);

    let done = acp.prompt(&session, "Run the fixture").await?;
    assert_eq!(done["stopReason"], "end_turn");
    assert_eq!(calls.load(Ordering::SeqCst), 4);

    // bash and the workspace write each ask; both attach to their own card.
    assert_eq!(acp.requests.len(), 2, "{:?}", acp.requests);
    let permission = acp.requests.first().context("permission prompt")?;
    assert_eq!(permission["sessionId"], session);
    let options: Vec<_> = permission["options"]
        .as_array()
        .context("options")?
        .iter()
        .map(|o| {
            (
                o["optionId"].as_str().unwrap_or_default(),
                o["kind"].clone(),
            )
        })
        .collect();
    assert_eq!(
        options,
        vec![
            ("allow_once", json!("allow_once")),
            ("allow_session", json!("allow_always")),
            ("reject", json!("reject_once")),
        ]
    );
    let started: Vec<_> = acp.updates_of("tool_call").cloned().collect();
    assert_eq!(
        permission["toolCall"]["toolCallId"], started[0]["toolCallId"],
        "the prompt is attached to the bash call that raised it"
    );
    assert_eq!(started.len(), 3, "{started:?}");
    assert_eq!(started[0]["kind"], "execute");
    assert_eq!(started[1]["kind"], "read");
    assert_eq!(started[2]["kind"], "edit");
    // Paths use the client's own spelling of cwd. On macOS the temp root is a
    // symlink, so the raw spelling differs from the canonical one and this
    // comparison proves the server did not canonicalize; on Linux the temp root
    // is already canonical and both spellings coincide.
    let report = workspace.path().join("report.txt");
    assert_eq!(started[2]["locations"][0]["path"], json!(report));

    let edit = started[2]["toolCallId"].clone();
    assert_eq!(acp.requests[1]["toolCall"]["toolCallId"], edit);
    let diff = acp
        .updates_of("tool_call_update")
        .filter(|u| u["toolCallId"] == edit)
        .flat_map(|u| u["content"].as_array().cloned().unwrap_or_default())
        .find(|c| c["type"] == "diff")
        .context("edit diff")?;
    assert_eq!(diff["path"], json!(report));
    assert_eq!(diff["oldText"], "original");
    assert_eq!(diff["newText"], "qualified");
    assert!(
        acp.updates_of("tool_call_update")
            .filter(|u| u["toolCallId"] == edit)
            .any(|u| u["status"] == "completed")
    );
    assert_eq!(std::fs::read_to_string(&report)?, "qualified");
    acp.close().await
}

/// A rejected or cancelled permission request is a denial: the command never
/// runs and the tool call ends failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_and_cancelled_permissions_never_run_the_command() -> Result<()> {
    for answer in [
        Answer::Option("reject"),
        Answer::Cancelled,
        Answer::Option("allow_for_project"),
    ] {
        let (mock, _) = provider(|n| {
            sse(&if n == 0 {
                tool("bash", json!({"command":"touch must-not-exist"}), n)
            } else {
                DONE.into()
            })
        })
        .await;
        let home = tempfile::tempdir()?;
        let workspace = tempfile::tempdir()?;
        let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
        acp.answer = answer;
        acp.initialize().await?;
        let session = acp.new_session(workspace.path()).await?;
        let done = acp.prompt(&session, "Touch it").await?;
        assert_eq!(done["stopReason"], "end_turn");
        assert_eq!(acp.requests.len(), 1);
        assert!(!workspace.path().join("must-not-exist").exists());
        let bash = acp
            .updates_of("tool_call")
            .next()
            .context("bash tool call")?["toolCallId"]
            .clone();
        assert!(
            acp.updates_of("tool_call_update")
                .any(|u| u["toolCallId"] == bash && u["status"] == "failed"),
            "{:?}",
            acp.updates
        );
        acp.close().await?;
    }
    Ok(())
}

/// `session/cancel` stops a running turn promptly with `cancelled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stops_the_turn() -> Result<()> {
    let (mock, _) = provider(|_| sse(DONE).set_delay(Duration::from_secs(30))).await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let session = acp.new_session(workspace.path()).await?;
    let id = acp
        .send(
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"Wait"}]}),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = std::time::Instant::now();
    acp.notify("session/cancel", json!({"sessionId":session}))
        .await?;
    let response = acp.response(id).await?;
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(acp.agent_text().contains("_Stopped._"), "{:?}", acp.updates);
    acp.close().await
}

/// Client-supplied MCP servers and untrusted foreign directories are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_mcp_servers_and_untrusted_cwd_are_rejected() -> Result<()> {
    let (mock, calls) = provider(|_| sse(DONE)).await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let elsewhere = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let mcp = acp
        .error(
            "session/new",
            json!({"cwd":workspace.path(),"mcpServers":[{"name":"evil","command":"/bin/sh","args":["-c","touch pwned"],"env":[]}]}),
        )
        .await?;
    assert_eq!(mcp["code"], -32602);
    assert!(mcp["message"].as_str().unwrap_or_default().contains("MCP"));
    assert!(!workspace.path().join("pwned").exists());
    let foreign = acp
        .error(
            "session/new",
            json!({"cwd":elsewhere.path(),"mcpServers":[]}),
        )
        .await?;
    assert_eq!(foreign["code"], -32602);
    assert!(
        foreign["message"]
            .as_str()
            .unwrap_or_default()
            .contains("trusted")
    );
    let relative = acp
        .error("session/new", json!({"cwd":"relative","mcpServers":[]}))
        .await?;
    assert_eq!(relative["code"], -32602);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    acp.close().await
}

/// Authority stays agent-side: even when the operator saved `yolo` in the
/// TUI, an ACP session runs at the fixed surface level (`ask`). The command
/// still prompts, and the client's rejection keeps it from running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_yolo_preference_never_reaches_an_acp_session() -> Result<()> {
    let (mock, _) = provider(|n| {
        sse(&if n == 0 {
            tool("bash", json!({"command":"touch must-not-exist"}), n)
        } else {
            DONE.into()
        })
    })
    .await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    // First run creates and migrates the database.
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    acp.close().await?;
    let db = home.path().join("state").join("bonsai.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db.display())).await?;
    sqlx::query(
        "INSERT INTO user_preferences (key, value) VALUES ('approval_level', 'yolo') \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .execute(&pool)
    .await?;
    pool.close().await;

    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.answer = Answer::Option("reject");
    acp.initialize().await?;
    let created = acp
        .call(
            "session/new",
            json!({"cwd":workspace.path(),"mcpServers":[]}),
        )
        .await?;
    let session = created["sessionId"].as_str().context("id")?.to_owned();
    let autonomy = &created["configOptions"][1];
    assert_eq!(autonomy["id"], "autonomy");
    assert_eq!(
        autonomy["currentValue"], "ask",
        "threads always start at ask"
    );
    assert_eq!(
        levels(autonomy),
        ["ask", "conservative", "balanced", "auto-accept", "yolo"],
        "the operator chose yolo, so it is selectable"
    );
    acp.prompt(&session, "Touch it").await?;
    assert_eq!(acp.requests.len(), 1, "yolo must not skip the prompt");
    assert!(!workspace.path().join("must-not-exist").exists());
    acp.close().await
}

/// A provider failure is visible in the thread (redacted), not a silent
/// empty turn, and the session stays usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_failure_is_shown_and_the_session_recovers() -> Result<()> {
    let (mock, _) = provider(|n| {
        if n == 0 {
            ResponseTemplate::new(401).set_body_string(
                r#"{"error":{"message":"invalid api key sk-ant-api03-SECRETSECRETSECRETSECRET"}}"#,
            )
        } else {
            sse(DONE)
        }
    })
    .await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let session = acp.new_session(workspace.path()).await?;
    let id = acp
        .send(
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"Hi"}]}),
        )
        .await?;
    let response = acp.response(id).await?;
    let text = acp.agent_text();
    assert!(text.contains("**Run failed.**"), "{text} / {response}");
    assert!(
        !text.contains("SECRETSECRET"),
        "secrets are redacted: {text}"
    );
    acp.updates.clear();
    let done = acp.prompt(&session, "Again").await?;
    assert_eq!(done["stopReason"], "end_turn");
    assert_eq!(acp.agent_text(), "fixture complete");
    acp.close().await
}

/// Editors multiplex threads over one process: two sessions stay usable side
/// by side, and a cancel in one does not touch the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_threads_share_one_connection() -> Result<()> {
    let (mock, calls) = provider(|_| sse(DONE)).await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let first = acp.new_session(workspace.path()).await?;
    let second = acp.new_session(workspace.path()).await?;
    assert_ne!(first, second);
    for session in [&first, &second, &first] {
        acp.updates.clear();
        let done = acp.prompt(session, "Hello").await?;
        assert_eq!(done["stopReason"], "end_turn", "{session}");
        assert_eq!(acp.agent_text(), "fixture complete");
    }
    acp.notify("session/cancel", json!({"sessionId":second}))
        .await?;
    acp.updates.clear();
    let done = acp.prompt(&first, "Still here?").await?;
    assert_eq!(done["stopReason"], "end_turn");
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    acp.close().await
}

/// Reopening a thread after the editor restarts: a fresh process loads the
/// session the previous process released on clean exit, with its history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_process_loads_a_cleanly_closed_session() -> Result<()> {
    let (mock, _) = provider(|_| sse(DONE)).await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let session = acp.new_session(workspace.path()).await?;
    acp.prompt(&session, "Remember me").await?;
    acp.close().await?;

    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    acp.call(
        "session/load",
        json!({"sessionId":session,"cwd":workspace.path(),"mcpServers":[]}),
    )
    .await?;
    assert!(
        acp.updates_of("user_message_chunk")
            .any(|u| u["content"]["text"] == "Remember me"),
        "{:?}",
        acp.updates
    );
    acp.updates.clear();
    let done = acp.prompt(&session, "And now?").await?;
    assert_eq!(done["stopReason"], "end_turn");
    assert_eq!(acp.agent_text(), "fixture complete");
    acp.close().await
}

/// Stop pressed while an approved shell command runs: the turn ends promptly
/// as `cancelled`, the command is killed, and the thread keeps working.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_kills_a_running_command() -> Result<()> {
    let (mock, _) = provider(|n| {
        sse(&if n == 0 {
            tool(
                "bash",
                json!({"command":"sleep 30 && touch slept-through-cancel"}),
                n,
            )
        } else {
            DONE.into()
        })
    })
    .await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let session = acp.new_session(workspace.path()).await?;
    let id = acp
        .send(
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"Sleep"}]}),
        )
        .await?;
    // Pump until the permission is granted and the command has started.
    let started = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let line = acp.lines.next_line().await?.context("stdout closed")?;
            let message: Value = serde_json::from_str(&line)?;
            if message["method"] == "session/request_permission" {
                let request = message["id"].clone();
                acp.write(json!({"jsonrpc":"2.0","id":request,"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}))
                    .await?;
                tokio::time::sleep(Duration::from_millis(700)).await;
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await;
    started.context("permission prompt timeout")??;
    let cancelled_at = std::time::Instant::now();
    acp.notify("session/cancel", json!({"sessionId":session}))
        .await?;
    let response = acp.response(id).await?;
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(10),
        "cancel took {:?}",
        cancelled_at.elapsed()
    );
    assert!(
        acp.updates_of("tool_call_update").any(|u| u["status"] == "failed"
            && u["content"][0]["content"]["text"] == "Interrupted"),
        "{:?}",
        acp.updates
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!workspace.path().join("slept-through-cancel").exists());
    acp.updates.clear();
    let done = acp.prompt(&session, "Still alive?").await?;
    assert_eq!(done["stopReason"], "end_turn");
    acp.close().await
}

/// The model picker lists every authorized connection's models, switches the
/// session's model while idle, and refuses anything not offered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_picker_lists_authorized_models_and_switches() -> Result<()> {
    let (mock, _) = provider(|_| sse(DONE)).await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut acp = Acp::spawn_with(
        home.path(),
        workspace.path(),
        &mock.uri(),
        &[("ANTHROPIC_API_KEY", "qualification-synthetic")],
    )?;
    acp.initialize().await?;
    let created = acp
        .call(
            "session/new",
            json!({"cwd":workspace.path(),"mcpServers":[]}),
        )
        .await?;
    let session = created["sessionId"].as_str().context("id")?.to_owned();
    let model = &created["configOptions"][0];
    assert_eq!(model["id"], "model");
    assert_eq!(model["category"], "model");
    assert_eq!(model["type"], "select");
    assert_eq!(model["currentValue"], "openai-compatible::acceptance-model");
    let groups = model["options"].as_array().context("groups")?;
    let group = |id: &str| groups.iter().find(|g| g["group"] == id).cloned();
    assert!(group("openai-compatible").is_some(), "{groups:?}");
    let anthropic = group("anthropic").context("authorized anthropic listed")?;
    assert!(
        group("openai").is_none(),
        "unauthorized connections stay hidden: {groups:?}"
    );
    let claude = anthropic["options"][0]["value"]
        .as_str()
        .context("anthropic model")?
        .to_owned();

    let switched = acp
        .call(
            "session/set_config_option",
            json!({"sessionId":session,"configId":"model","value":claude}),
        )
        .await?;
    assert_eq!(switched["configOptions"][0]["currentValue"], json!(claude));
    for bad in [
        json!({"sessionId":session,"configId":"model","value":"openai::gpt-5"}),
        json!({"sessionId":session,"configId":"model","value":"nonsense"}),
        json!({"sessionId":session,"configId":"autonomy","value":"yolo"}),
    ] {
        let error = acp.error("session/set_config_option", bad).await?;
        assert_eq!(error["code"], -32602, "{error}");
    }
    acp.call(
        "session/set_config_option",
        json!({"sessionId":session,"configId":"model","value":"openai-compatible::acceptance-model"}),
    )
    .await?;
    let done = acp.prompt(&session, "Hello").await?;
    assert_eq!(done["stopReason"], "end_turn");
    assert_eq!(acp.agent_text(), "fixture complete");
    let loaded = acp
        .call(
            "session/load",
            json!({"sessionId":session,"cwd":workspace.path(),"mcpServers":[]}),
        )
        .await?;
    assert_eq!(
        loaded["configOptions"][0]["currentValue"],
        "openai-compatible::acceptance-model"
    );
    acp.close().await
}

/// Zed killed on quit leaves no clean shutdown behind. Reopening the thread
/// right after restart must wait out the dead owner's heartbeat fence and
/// load, not fail with "cannot be claimed".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_after_the_previous_process_was_killed() -> Result<()> {
    let (mock, _) = provider(|_| sse(DONE)).await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let session = acp.new_session(workspace.path()).await?;
    acp.prompt(&session, "Before the crash").await?;
    acp.child.kill().await?;
    acp.child.wait().await?;

    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let started = std::time::Instant::now();
    acp.call(
        "session/load",
        json!({"sessionId":session,"cwd":workspace.path(),"mcpServers":[]}),
    )
    .await?;
    assert!(started.elapsed() < Duration::from_secs(25));
    assert!(
        acp.updates_of("user_message_chunk")
            .any(|u| u["content"]["text"] == "Before the crash")
    );
    acp.updates.clear();
    let done = acp.prompt(&session, "After").await?;
    assert_eq!(done["stopReason"], "end_turn");
    acp.close().await
}

fn levels(option: &Value) -> Vec<String> {
    option["options"]
        .as_array()
        .map(|options| {
            options
                .iter()
                .filter_map(|o| o["value"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

async fn seed_approval_level(home: &Path, level: &str) -> Result<()> {
    let db = home.join("state").join("bonsai.db");
    let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", db.display())).await?;
    sqlx::query(
        "INSERT INTO user_preferences (key, value) VALUES ('approval_level', ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(level)
    .execute(&pool)
    .await?;
    pool.close().await;
    Ok(())
}

/// The editor selects autonomy only up to the level the operator saved in
/// the TUI (default `balanced`); the selected level applies live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn autonomy_is_selectable_up_to_the_operator_level() -> Result<()> {
    let (mock, _) = provider(|n| {
        sse(&if n % 2 == 0 {
            tool("bash", json!({"command":"ls"}), n)
        } else {
            DONE.into()
        })
    })
    .await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let created = acp
        .call(
            "session/new",
            json!({"cwd":workspace.path(),"mcpServers":[]}),
        )
        .await?;
    let session = created["sessionId"].as_str().context("id")?.to_owned();
    let autonomy = &created["configOptions"][1];
    assert_eq!(autonomy["category"], "mode");
    assert_eq!(autonomy["currentValue"], "ask");
    assert_eq!(levels(autonomy), ["ask", "conservative", "balanced"]);
    for above in ["auto-accept", "yolo"] {
        let error = acp
            .error(
                "session/set_config_option",
                json!({"sessionId":session,"configId":"autonomy","value":above}),
            )
            .await?;
        assert_eq!(error["code"], -32602, "{above}: {error}");
    }

    // ask: even a read-only command prompts.
    acp.prompt(&session, "List").await?;
    assert_eq!(acp.requests.len(), 1);
    // conservative: read-only runs without a prompt, live, same session.
    let set = acp
        .call(
            "session/set_config_option",
            json!({"sessionId":session,"configId":"autonomy","value":"conservative"}),
        )
        .await?;
    assert_eq!(set["configOptions"][1]["currentValue"], "conservative");
    acp.prompt(&session, "List again").await?;
    assert_eq!(acp.requests.len(), 1, "no new prompt at conservative");
    acp.close().await?;

    // An operator who saved `ask` grants nothing above it.
    seed_approval_level(home.path(), "ask").await?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let created = acp
        .call(
            "session/new",
            json!({"cwd":workspace.path(),"mcpServers":[]}),
        )
        .await?;
    assert_eq!(levels(&created["configOptions"][1]), ["ask"]);
    let session = created["sessionId"].as_str().context("id")?.to_owned();
    let error = acp
        .error(
            "session/set_config_option",
            json!({"sessionId":session,"configId":"autonomy","value":"conservative"}),
        )
        .await?;
    assert_eq!(error["code"], -32602);
    acp.close().await
}

/// Zed's thread history: `session/list` returns this workspace's sessions,
/// newest first, titled by their prompt, and refuses untrusted foreign cwds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_list_feeds_thread_history() -> Result<()> {
    let (mock, _) = provider(|_| sse(DONE)).await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let elsewhere = tempfile::tempdir()?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    let init = acp.initialize().await?;
    assert_eq!(
        init["agentCapabilities"]["sessionCapabilities"]["list"],
        json!({})
    );
    let first = acp.new_session(workspace.path()).await?;
    acp.prompt(&first, "Fix the parser\nwith details below")
        .await?;
    let second = acp.new_session(workspace.path()).await?;
    acp.prompt(&second, "Write the docs").await?;

    let listed = acp
        .call("session/list", json!({"cwd":workspace.path()}))
        .await?;
    let sessions = listed["sessions"].as_array().context("sessions")?;
    let ids: Vec<_> = sessions.iter().map(|s| s["sessionId"].clone()).collect();
    assert_eq!(ids, vec![json!(second), json!(first)], "newest first");
    assert_eq!(sessions[0]["title"], "Write the docs");
    assert_eq!(sessions[1]["title"], "Fix the parser with details below");
    assert_eq!(sessions[0]["cwd"], json!(workspace.path()));
    let updated = sessions[0]["updatedAt"].as_str().context("updatedAt")?;
    assert!(
        updated.len() == 20 && updated.ends_with('Z') && updated.as_bytes()[10] == b'T',
        "{updated}"
    );
    assert!(listed.get("nextCursor").is_none_or(Value::is_null));

    let default_cwd = acp.call("session/list", json!({})).await?;
    assert_eq!(default_cwd["sessions"].as_array().map(Vec::len), Some(2));
    let foreign = acp
        .error("session/list", json!({"cwd":elsewhere.path()}))
        .await?;
    assert_eq!(foreign["code"], -32602);
    acp.close().await
}

/// Custom slash commands: after `session/new` the editor gets an
/// `available_commands_update` with the workspace's commands, and a prompt
/// starting with `/<name>` reaches the model as the expanded body. The launch
/// directory is untrusted here, so only global commands are offered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_commands_are_announced_and_expanded() -> Result<()> {
    let (mock, _) = provider(|_| sse(DONE)).await;
    let home = tempfile::tempdir()?;
    let workspace = tempfile::tempdir()?;
    let global = home.path().join("state/commands");
    std::fs::create_dir_all(&global)?;
    std::fs::write(
        global.join("greet.md"),
        "---\ndescription: Greet someone\nargument-hint: <name>\n---\nSay hello to $ARGUMENTS politely.\n",
    )?;
    std::fs::write(global.join("blank.md"), "---\ndescription: empty\n---\n\n")?;
    let project = workspace.path().join(".bonsai/commands");
    std::fs::create_dir_all(&project)?;
    std::fs::write(project.join("inject.md"), "PROJECT_COMMAND_BODY_SENTINEL")?;
    let mut acp = Acp::spawn(home.path(), workspace.path(), &mock.uri())?;
    acp.initialize().await?;
    let session = acp.new_session(workspace.path()).await?;

    let done = acp.prompt(&session, "/greet Ada").await?;
    assert_eq!(done["stopReason"], "end_turn");
    let announced: Vec<&Value> = acp.updates_of("available_commands_update").collect();
    let [update] = announced.as_slice() else {
        bail!("expected one available_commands_update: {:?}", acp.updates);
    };
    let commands = update["availableCommands"]
        .as_array()
        .context("availableCommands")?;
    let names: Vec<&str> = commands.iter().filter_map(|c| c["name"].as_str()).collect();
    assert_eq!(
        names,
        ["blank", "greet"],
        "untrusted project commands stay hidden"
    );
    let greet = &commands[1];
    assert_eq!(greet["description"], "Greet someone");
    assert_eq!(greet["input"]["hint"], "<name>");

    let requests = mock
        .received_requests()
        .await
        .context("mock records requests")?;
    let [request] = requests.as_slice() else {
        bail!("expected one provider request, got {}", requests.len());
    };
    let body = String::from_utf8_lossy(&request.body);
    assert!(body.contains("Say hello to Ada politely."), "{body}");
    assert!(!body.contains("/greet"), "raw slash line leaked: {body}");

    let refused = acp
        .error(
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"/blank"}]}),
        )
        .await?;
    assert_eq!(refused["code"], -32602);
    assert!(
        refused["message"]
            .as_str()
            .is_some_and(|message| message.contains("empty prompt body")),
        "{refused}"
    );
    acp.close().await
}
