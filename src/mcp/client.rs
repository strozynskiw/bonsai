//! One live connection to an MCP server. `rmcp` owns the request/response
//! correlation and the wire protocol; this wraps it with bonsai's config
//! shape (stdio env/cwd, HTTP headers, `${VAR}` expansion) and a call
//! timeout.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool as RmcpTool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::auth::AuthClient;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use tokio::io::{AsyncBufReadExt, BufReader};

use super::auth::McpOAuthStore;
use crate::sandbox::CommandSandbox;

/// How many trailing stderr lines of one server are retained for failure
/// messages, and how long a single line may be before it is truncated.
const STDERR_TAIL_LINES: usize = 20;
const STDERR_LINE_CHARS: usize = 2_000;

/// How long a failed handshake waits for the child's stderr drain before
/// reporting, so the tail is complete rather than raced. The child has already
/// exited by then (its stdout reached EOF), so this is a cap, not a delay.
const STDERR_DRAIN_GRACE: Duration = Duration::from_millis(250);

/// One connection's `rmcp` peer. A bare `()` client handler: bonsai never
/// receives server-initiated requests (sampling, roots) in v1, so the no-op
/// `ClientHandler` impl is all this needs.
pub(crate) struct McpConnection {
    peer: RunningService<RoleClient, ()>,
}

impl McpConnection {
    /// Wrap an already-handshaked peer. The real constructors are
    /// [`Self::connect_stdio`]/[`Self::connect_http`]; this is the seam
    /// `src/mcp/tests.rs` uses to drive the client against an in-process fake
    /// server instead of a real stdio/HTTP transport.
    #[cfg(test)]
    pub(crate) fn from_peer(peer: RunningService<RoleClient, ()>) -> Self {
        Self { peer }
    }

    pub(crate) async fn connect_stdio(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&Path>,
        sandbox: &CommandSandbox,
        project_root: &Path,
    ) -> Result<Self> {
        let cwd = cwd.unwrap_or(project_root);
        let mut cmd = if sandbox.is_active() {
            let script = shell_command(command, args);
            let (cmd, decision) = sandbox.command("sh", &script, cwd);
            decision.log();
            cmd
        } else {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args).current_dir(cwd);
            cmd
        };
        for (key, value) in env {
            cmd.env(key, crate::util::env::expand_env_vars(value));
        }
        // `TokioChildProcess` inherits stderr by default, which lets any MCP
        // server's logging write straight into the terminal bonsai's TUI owns
        // and shred the frame. Pipe it instead, drain it for the life of the
        // connection, and keep only a bounded tail for diagnostics.
        let (transport, stderr) = TokioChildProcess::builder(cmd)
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn the MCP server process")?;
        let tail = StderrTail::default();
        let drain = stderr.map(|stderr| drain_stderr(stderr, command.to_string(), tail.clone()));
        let peer = match ().serve(transport).await {
            Ok(peer) => peer,
            Err(err) => {
                // The child is gone (its stdout hit EOF), so the drain finishes
                // promptly; the cap only bounds a server that is still running.
                if let Some(drain) = drain {
                    let _ = tokio::time::timeout(STDERR_DRAIN_GRACE, drain).await;
                }
                let mut context = String::from("MCP stdio handshake failed");
                if let Some(tail) = tail.text() {
                    context.push_str(&format!("\nserver stderr:\n{tail}"));
                }
                return Err(anyhow::Error::new(err).context(context));
            }
        };
        Ok(Self { peer })
    }

    pub(crate) async fn connect_http(
        url: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<Self> {
        let config = http_transport_config(url, headers)?;
        let transport =
            StreamableHttpClientTransport::with_client(crate::provider::http_client(), config);
        let peer = ().serve(transport).await.context("MCP HTTP handshake failed")?;
        Ok(Self { peer })
    }

    pub(super) async fn connect_http_authorized(
        url: &str,
        headers: &BTreeMap<String, String>,
        store: &McpOAuthStore,
    ) -> Result<Self> {
        let manager = super::auth::stored_authorization_manager(url, store)
            .await?
            .context("MCP OAuth authorization is required")?;
        let http_client = super::auth::pinned_resource_client(url).await?;
        let client = AuthClient::new(http_client, manager);
        let config = http_transport_config(url, headers)?;
        let transport = StreamableHttpClientTransport::with_client(client, config);
        let peer = ().serve(transport).await.context(
            "MCP OAuth handshake failed; the login may be expired or revoked — authorize again",
        )?;
        Ok(Self { peer })
    }

    pub(crate) async fn list_tools(&self) -> Result<Vec<RmcpTool>> {
        self.peer
            .list_all_tools()
            .await
            .context("failed to list MCP tools")
    }

    pub(crate) async fn call_tool(
        &self,
        remote_name: &str,
        arguments: serde_json::Value,
        timeout: Duration,
    ) -> Result<CallToolResult> {
        let arguments = match arguments {
            serde_json::Value::Object(map) => Some(map),
            serde_json::Value::Null => None,
            other => {
                let mut map = serde_json::Map::new();
                map.insert("value".to_string(), other);
                Some(map)
            }
        };
        let mut params = CallToolRequestParams::new(remote_name.to_string());
        if let Some(arguments) = arguments {
            params = params.with_arguments(arguments);
        }
        tokio::time::timeout(timeout, self.peer.call_tool(params))
            .await
            .with_context(|| format!("MCP call to '{remote_name}' timed out after {timeout:?}"))?
            .with_context(|| format!("MCP call to '{remote_name}' failed"))
    }
}

/// Bounded tail of one server's stderr, filled by [`drain_stderr`] and shared
/// with the connection attempt so a failed handshake can explain itself.
#[derive(Debug, Clone, Default)]
struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl StderrTail {
    fn push(&self, line: String) {
        let Ok(mut lines) = self.lines.lock() else {
            return;
        };
        if lines.len() == STDERR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// The retained tail, or `None` when the server wrote nothing to stderr.
    fn text(&self) -> Option<String> {
        let lines = self.lines.lock().ok()?;
        if lines.is_empty() {
            return None;
        }
        Some(lines.iter().cloned().collect::<Vec<_>>().join("\n"))
    }
}

/// Drain a child's stderr off-thread for the life of the process: forwarded to
/// the debug log (redacted, truncated) and folded into the retained tail. An
/// undrained pipe would eventually block a chatty server.
fn drain_stderr(
    stderr: tokio::process::ChildStderr,
    label: String,
    tail: StderrTail,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let line = crate::redact::redact(&line).into_owned();
                    let line = truncate_line(&line);
                    tracing::debug!(server = %label, "{line}");
                    tail.push(line);
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::debug!(server = %label, %error, "MCP server stderr closed");
                    break;
                }
            }
        }
    })
}

fn truncate_line(line: &str) -> String {
    if line.chars().count() <= STDERR_LINE_CHARS {
        return line.to_string();
    }
    let mut truncated: String = line.chars().take(STDERR_LINE_CHARS).collect();
    truncated.push('…');
    truncated
}

fn http_transport_config(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> Result<StreamableHttpClientTransportConfig> {
    let mut header_map = std::collections::HashMap::new();
    for (key, value) in headers {
        let name = reqwest::header::HeaderName::from_bytes(key.as_bytes())
            .with_context(|| format!("invalid MCP header name '{key}'"))?;
        let value =
            reqwest::header::HeaderValue::from_str(&crate::util::env::expand_env_vars(value))
                .with_context(|| format!("invalid MCP header value for '{key}'"))?;
        header_map.insert(name, value);
    }
    Ok(StreamableHttpClientTransportConfig::with_uri(url).custom_headers(header_map))
}

/// Quote an executable and its argv into a POSIX-shell program without letting
/// config values become shell syntax. This wrapper is used only when an active
/// [`CommandSandbox`] needs the common `sh -c` launch shape; unconfined hosts
/// retain the direct `Command::new(program).args(argv)` spawn above.
fn shell_command(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_wrapper_quotes_every_argv_value() {
        let script = shell_command(
            "tool; echo pwned",
            &["two words".to_string(), "'".to_string()],
        );
        assert_eq!(script, "'tool; echo pwned' 'two words' ''\"'\"''");
    }

    #[test]
    fn stderr_tail_keeps_only_the_most_recent_lines() {
        let tail = StderrTail::default();
        for index in 0..STDERR_TAIL_LINES + 3 {
            tail.push(format!("line {index}"));
        }
        let text = tail.text().expect("a non-empty tail");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), STDERR_TAIL_LINES);
        assert_eq!(lines.first().copied(), Some("line 3"));
        assert_eq!(
            lines.last().copied(),
            Some(format!("line {}", STDERR_TAIL_LINES + 2).as_str())
        );
        assert_eq!(StderrTail::default().text(), None);
    }

    /// The regression for a server whose logging used to land in the terminal
    /// bonsai's TUI owns: stderr must be piped, not inherited, and a failed
    /// handshake must be able to quote what the server said.
    #[tokio::test]
    async fn stdio_stderr_is_captured_for_failure_diagnostics() {
        let outcome = tokio::time::timeout(
            Duration::from_secs(30),
            McpConnection::connect_stdio(
                "sh",
                &[
                    "-c".to_string(),
                    "echo 'blender says: starting' >&2; exit 3".to_string(),
                ],
                &BTreeMap::new(),
                None,
                &CommandSandbox::disabled(),
                &std::env::temp_dir(),
            ),
        )
        .await
        .expect("a child that never speaks MCP must fail the handshake promptly");
        let Err(error) = outcome else {
            panic!("a process that never speaks MCP cannot handshake");
        };
        let message = format!("{error:#}");
        assert!(message.contains("MCP stdio handshake failed"), "{message}");
        assert!(message.contains("blender says: starting"), "{message}");
    }
}
