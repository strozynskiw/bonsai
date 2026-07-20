//! `/mcp` — status and session control for configured MCP servers.
//! `/mcp tools <server>` lists the tools discovered on one server, read from
//! the inventory `attempt_and_register` threads through `ExtensionStatus`.

use crate::extension::ExtensionId;
use crate::extension::status::{ExtensionRegistry, ExtensionState};
use crate::extension::truncate_to_char_boundary;
use crate::mcp::{McpAuthorizationCompletion, McpHub, McpRevocationOutcome};
use crate::session::CredentialPersistence;

pub(crate) const MCP_USAGE: &str = "Usage: /mcp [tools|enable|disable|reload|revoke <server> | auth <server> [file|keyring|session] | callback <server> <redirect-url>]";

/// Cap for one rendered description line; the stored first line is complete.
const TOOL_DESCRIPTION_MAX: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpCommandRequest {
    View,
    Tools(String),
    Enable(String),
    Disable(String),
    Reload(String),
    Auth {
        server: String,
        persistence: Option<CredentialPersistence>,
    },
    Callback {
        server: String,
        callback_url: String,
    },
    Revoke(String),
}

pub(crate) fn parse_mcp_command(input: &str) -> Result<McpCommandRequest, String> {
    let args = super::command_args(input, "/mcp", MCP_USAGE)?;
    match args.as_slice() {
        [] => Ok(McpCommandRequest::View),
        ["tools", server] => Ok(McpCommandRequest::Tools((*server).to_string())),
        ["enable", server] => Ok(McpCommandRequest::Enable((*server).to_string())),
        ["disable", server] => Ok(McpCommandRequest::Disable((*server).to_string())),
        ["reload", server] => Ok(McpCommandRequest::Reload((*server).to_string())),
        ["auth", server] => Ok(McpCommandRequest::Auth {
            server: (*server).to_string(),
            persistence: None,
        }),
        ["auth", server, persistence] => {
            let persistence = CredentialPersistence::parse(persistence).ok_or_else(|| {
                "MCP credential storage must be file, keyring, or session.".to_string()
            })?;
            Ok(McpCommandRequest::Auth {
                server: (*server).to_string(),
                persistence: Some(persistence),
            })
        }
        ["callback", server, callback_url] => Ok(McpCommandRequest::Callback {
            server: (*server).to_string(),
            callback_url: (*callback_url).to_string(),
        }),
        ["revoke", server] => Ok(McpCommandRequest::Revoke((*server).to_string())),
        _ => Err(MCP_USAGE.to_string()),
    }
}

/// One line per configured server: name, state, provenance, capabilities,
/// tool count, transport.
pub(crate) fn format_mcp_view(extensions: &ExtensionRegistry) -> String {
    let servers: Vec<_> = extensions
        .snapshot()
        .into_iter()
        .filter(|status| matches!(status.id, ExtensionId::McpServer(_)))
        .collect();
    if servers.is_empty() {
        return "MCP servers: none configured.".to_string();
    }
    let mut lines = vec!["MCP servers:".to_string()];
    for status in servers {
        let ExtensionId::McpServer(name) = &status.id else {
            continue;
        };
        let state = status.state.label();
        let capabilities = if status.capabilities.capabilities.is_empty() {
            "undeclared".to_string()
        } else {
            status
                .capabilities
                .capabilities
                .iter()
                .map(|c| crate::extension::capabilities::capability_label(*c))
                .collect::<Vec<_>>()
                .join(",")
        };
        lines.push(format!(
            "  {name} — {state}, capabilities=[{capabilities}], {} ({})",
            status.detail,
            status.source.label()
        ));
    }
    lines.join("\n")
}

/// The per-server inventory for `/mcp tools <server>`: one line per discovered
/// tool — remote name plus the first line of its description. Reads the
/// registry snapshot only, so it stays hub-independent like `View`.
pub(crate) fn format_mcp_tools_view(
    server: &str,
    extensions: &ExtensionRegistry,
) -> Result<String, String> {
    let snapshot = extensions.snapshot();
    let status = snapshot
        .iter()
        .find(|status| matches!(&status.id, ExtensionId::McpServer(name) if name == server));
    let Some(status) = status else {
        let known: Vec<&str> = snapshot
            .iter()
            .filter_map(|status| match &status.id {
                ExtensionId::McpServer(name) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        return Err(unknown_server_message(server, &known));
    };

    if let ExtensionState::Failed { error } = &status.state {
        return Err(format!(
            "'{server}' failed: {error}. No tools are available; try /mcp reload {server}."
        ));
    }
    if status.tools.is_empty() {
        return Ok(format!(
            "No tools discovered on '{server}' (the server returned none, or allow_tools filtered them all)."
        ));
    }

    let state_note = match &status.state {
        ExtensionState::Enabled => String::new(),
        other => format!(" — {}", other.label()),
    };
    let mut lines = vec![format!(
        "MCP tools on '{server}' ({}){state_note}:",
        status.tools.len()
    )];
    for tool in &status.tools {
        if tool.description.is_empty() {
            lines.push(format!("  {}", tool.name));
        } else {
            let description = truncate_to_char_boundary(&tool.description, TOOL_DESCRIPTION_MAX);
            let ellipsis = if description.len() < tool.description.len() {
                "…"
            } else {
                ""
            };
            lines.push(format!("  {} — {description}{ellipsis}", tool.name));
        }
    }
    Ok(lines.join("\n"))
}

pub(crate) async fn apply_mcp_command(
    request: McpCommandRequest,
    hub: Option<&McpHub>,
    extensions: &ExtensionRegistry,
) -> Result<String, String> {
    match request {
        McpCommandRequest::View => Ok(format_mcp_view(extensions)),
        McpCommandRequest::Tools(server) => format_mcp_tools_view(&server, extensions),
        McpCommandRequest::Enable(server) | McpCommandRequest::Disable(server) if hub.is_none() => {
            let _ = server;
            Err("MCP is unavailable in this session.".to_string())
        }
        McpCommandRequest::Enable(server) => {
            if hub.unwrap().set_enabled(&server, true) {
                Ok(format!("Enabled '{server}' for this session."))
            } else {
                Err(unknown_server_message(
                    &server,
                    &hub.unwrap().server_names(),
                ))
            }
        }
        McpCommandRequest::Disable(server) => {
            if hub.unwrap().set_enabled(&server, false) {
                Ok(format!(
                    "Disabled '{server}' for this session; calls to its tools will fail visibly."
                ))
            } else {
                Err(unknown_server_message(
                    &server,
                    &hub.unwrap().server_names(),
                ))
            }
        }
        McpCommandRequest::Reload(server) => {
            let Some(hub) = hub else {
                return Err("MCP is unavailable in this session.".to_string());
            };
            match hub.reload(&server).await {
                Ok(()) => Ok(format!(
                    "Reconnected '{server}'. A tool schema change on the remote side needs a restart to take effect."
                )),
                Err(err) => Err(format!(
                    "Failed to reload '{server}': {}",
                    redacted_error(&err)
                )),
            }
        }
        McpCommandRequest::Auth {
            server,
            persistence,
        } => {
            let Some(hub) = hub else {
                return Err("MCP is unavailable in this session.".to_string());
            };
            let persistence = persistence.unwrap_or_else(|| hub.credential_persistence());
            match hub.begin_authorization(&server, persistence).await {
                Ok(start) => Ok(format!(
                    "OAuth authorization for '{server}' is ready ({storage}).\nOpen this URL in a browser:\n{url}\n\nA loopback callback is listening at {redirect}. If the browser is on another device, copy its final redirect URL and run `/mcp callback {server} <redirect-url>`.",
                    storage = start.persistence.label(),
                    url = start.authorization_url,
                    redirect = start.redirect_uri,
                )),
                Err(error) => Err(format!(
                    "Failed to start OAuth authorization for '{server}': {}",
                    redacted_error(&error)
                )),
            }
        }
        McpCommandRequest::Callback {
            server,
            callback_url,
        } => {
            let Some(hub) = hub else {
                return Err("MCP is unavailable in this session.".to_string());
            };
            match hub.complete_authorization(&server, &callback_url).await {
                Ok(McpAuthorizationCompletion::Reconnected {
                    persistence,
                    fell_back,
                }) => Ok(format!(
                    "Authorized and reconnected '{server}' using {}{}.",
                    persistence.label(),
                    credential_fallback_note(fell_back)
                )),
                Ok(McpAuthorizationCompletion::RestartRequired {
                    discovered_tools,
                    persistence,
                    fell_back,
                }) => Ok(format!(
                    "Authorized '{server}' using {}{}. Restart bonsai to register {discovered_tools} discovered tool(s).",
                    persistence.label(),
                    credential_fallback_note(fell_back)
                )),
                Err(error) => Err(format!(
                    "Failed to complete OAuth authorization for '{server}': {}",
                    redacted_error(&error)
                )),
            }
        }
        McpCommandRequest::Revoke(server) => {
            let Some(hub) = hub else {
                return Err("MCP is unavailable in this session.".to_string());
            };
            match hub.revoke_authorization(&server).await {
                Ok(McpRevocationOutcome::RevokedRemotely) => Ok(format!(
                    "Revoked '{server}' at the authorization server and cleared its local login."
                )),
                Ok(McpRevocationOutcome::ClearedLocally) => Ok(format!(
                    "Cleared the local login for '{server}'; its authorization server does not advertise token revocation."
                )),
                Ok(McpRevocationOutcome::ClearedLocallyBySandbox) => Ok(format!(
                    "Cleared the local login for '{server}'. Remote revocation was skipped because sandbox network access is denied."
                )),
                Ok(McpRevocationOutcome::ClearedLocallyAfterRemoteFailure(error)) => Ok(format!(
                    "Cleared the local login for '{server}', but remote revocation failed: {error}"
                )),
                Ok(McpRevocationOutcome::NotAuthorized) => {
                    Ok(format!("'{server}' has no stored OAuth login."))
                }
                Err(error) => Err(format!(
                    "Failed to revoke OAuth authorization for '{server}': {}",
                    redacted_error(&error)
                )),
            }
        }
    }
}

fn redacted_error(error: &anyhow::Error) -> String {
    crate::redact::redact(&format!("{error:#}")).into_owned()
}

fn credential_fallback_note(fell_back: bool) -> &'static str {
    if fell_back {
        " (requested storage unavailable; session-only fallback)"
    } else {
        ""
    }
}

fn unknown_server_message(server: &str, known: &[&str]) -> String {
    if known.is_empty() {
        format!("Unknown MCP server '{server}'. No servers are configured.")
    } else {
        format!(
            "Unknown MCP server '{server}'. Configured: {}.",
            known.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigSource, DeclaredCapabilities};
    use crate::extension::status::{DisableReason, DiscoveredTool, ExtensionStatus};

    fn tool(name: &str, description: &str) -> DiscoveredTool {
        DiscoveredTool {
            name: name.to_string(),
            description: description.to_string(),
        }
    }

    fn mcp_status(
        name: &str,
        state: ExtensionState,
        tools: Vec<DiscoveredTool>,
    ) -> ExtensionStatus {
        ExtensionStatus {
            id: ExtensionId::McpServer(name.to_string()),
            source: ConfigSource::Project,
            capabilities: DeclaredCapabilities::default(),
            state,
            detail: format!("{} tool(s)", tools.len()),
            tools,
        }
    }

    #[test]
    fn parses_view_and_toggle_variants() {
        assert_eq!(parse_mcp_command("/mcp"), Ok(McpCommandRequest::View));
        assert_eq!(
            parse_mcp_command("/mcp tools github"),
            Ok(McpCommandRequest::Tools("github".to_string()))
        );
        assert_eq!(
            parse_mcp_command("/mcp enable github"),
            Ok(McpCommandRequest::Enable("github".to_string()))
        );
        assert_eq!(
            parse_mcp_command("/mcp disable github"),
            Ok(McpCommandRequest::Disable("github".to_string()))
        );
        assert_eq!(
            parse_mcp_command("/mcp reload github"),
            Ok(McpCommandRequest::Reload("github".to_string()))
        );
        assert_eq!(
            parse_mcp_command("/mcp auth github"),
            Ok(McpCommandRequest::Auth {
                server: "github".to_string(),
                persistence: None,
            })
        );
        assert_eq!(
            parse_mcp_command("/mcp auth github session"),
            Ok(McpCommandRequest::Auth {
                server: "github".to_string(),
                persistence: Some(CredentialPersistence::Session),
            })
        );
        assert_eq!(
            parse_mcp_command("/mcp callback github http://127.0.0.1/callback?code=x&state=y"),
            Ok(McpCommandRequest::Callback {
                server: "github".to_string(),
                callback_url: "http://127.0.0.1/callback?code=x&state=y".to_string(),
            })
        );
        assert_eq!(
            parse_mcp_command("/mcp revoke github"),
            Ok(McpCommandRequest::Revoke("github".to_string()))
        );
        assert!(parse_mcp_command("/mcp bogus").is_err());
        assert!(parse_mcp_command("/mcp auth github disk").is_err());
        assert!(parse_mcp_command("/mcp enable").is_err());
        assert!(parse_mcp_command("/mcp tools").is_err());
    }

    #[test]
    fn view_reports_no_servers() {
        let extensions = ExtensionRegistry::new();
        assert_eq!(
            format_mcp_view(&extensions),
            "MCP servers: none configured."
        );
    }

    #[test]
    fn tools_lists_inventory_with_descriptions() {
        let extensions = ExtensionRegistry::new();
        extensions.upsert(mcp_status(
            "github",
            ExtensionState::Enabled,
            vec![
                tool("create_issue", "Create a new issue in a repository"),
                tool("ping", ""),
            ],
        ));
        assert_eq!(
            format_mcp_tools_view("github", &extensions),
            Ok("MCP tools on 'github' (2):\n  create_issue — Create a new issue in a repository\n  ping".to_string())
        );
    }

    #[test]
    fn tools_truncates_a_long_description() {
        let extensions = ExtensionRegistry::new();
        let long = "x".repeat(TOOL_DESCRIPTION_MAX + 20);
        extensions.upsert(mcp_status(
            "github",
            ExtensionState::Enabled,
            vec![tool("wordy", &long)],
        ));
        let rendered = format_mcp_tools_view("github", &extensions).unwrap();
        let expected_body = "x".repeat(TOOL_DESCRIPTION_MAX);
        assert!(rendered.ends_with(&format!("  wordy — {expected_body}…")));
    }

    #[test]
    fn tools_reports_unknown_server_with_known_names() {
        let extensions = ExtensionRegistry::new();
        assert_eq!(
            format_mcp_tools_view("ghost", &extensions),
            Err("Unknown MCP server 'ghost'. No servers are configured.".to_string())
        );

        extensions.upsert(mcp_status(
            "fake",
            ExtensionState::Enabled,
            vec![tool("echo", "")],
        ));
        let err = format_mcp_tools_view("ghost", &extensions).unwrap_err();
        assert!(err.contains("Configured: fake"), "got: {err}");
    }

    #[test]
    fn tools_reports_failed_server_as_error() {
        let extensions = ExtensionRegistry::new();
        extensions.upsert(mcp_status(
            "github",
            ExtensionState::Failed {
                error: "handshake timed out".to_string(),
            },
            Vec::new(),
        ));
        let err = format_mcp_tools_view("github", &extensions).unwrap_err();
        assert!(
            err.contains("'github' failed: handshake timed out"),
            "got: {err}"
        );
        assert!(err.contains("/mcp reload github"), "got: {err}");
    }

    #[test]
    fn tools_reports_empty_inventory() {
        let extensions = ExtensionRegistry::new();
        extensions.upsert(mcp_status("github", ExtensionState::Enabled, Vec::new()));
        let msg = format_mcp_tools_view("github", &extensions).unwrap();
        assert!(
            msg.contains("No tools discovered on 'github'"),
            "got: {msg}"
        );
    }

    #[test]
    fn tools_header_notes_disabled_state() {
        let extensions = ExtensionRegistry::new();
        extensions.upsert(mcp_status(
            "github",
            ExtensionState::Disabled {
                reason: DisableReason::Session,
            },
            vec![tool("echo", "Echo the input")],
        ));
        let rendered = format_mcp_tools_view("github", &extensions).unwrap();
        assert!(
            rendered.starts_with("MCP tools on 'github' (1) — disabled (Session):"),
            "got: {rendered}"
        );
    }

    #[tokio::test]
    async fn enable_disable_without_a_hub_reports_unavailable() {
        let extensions = ExtensionRegistry::new();
        let result = apply_mcp_command(
            McpCommandRequest::Enable("github".to_string()),
            None,
            &extensions,
        )
        .await;
        assert_eq!(
            result,
            Err("MCP is unavailable in this session.".to_string())
        );
    }
}
