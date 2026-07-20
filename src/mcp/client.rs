//! One live connection to an MCP server. `rmcp` owns the request/response
//! correlation and the wire protocol; this wraps it with bonsai's config
//! shape (stdio env/cwd, HTTP headers, `${VAR}` expansion) and a call
//! timeout.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool as RmcpTool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::auth::AuthClient;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};

use super::auth::McpOAuthStore;
use crate::sandbox::CommandSandbox;

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
        let transport =
            TokioChildProcess::new(cmd).context("failed to spawn the MCP server process")?;
        let peer = ().serve(transport).await.context("MCP stdio handshake failed")?;
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
}
