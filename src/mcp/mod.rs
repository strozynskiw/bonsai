//! The MCP client: connects every enabled `[mcp.servers.*]` entry
//! concurrently at startup, discovers each server's tools, and wraps them as
//! namespaced [`Tool`] implementations the coding registry appends after
//! `webfetch` (byte-stable prompt suffix). Architectural analog of
//! `src/lsp/` (`LspClient::spawn`, `LspHub`), with one difference:
//! connections are eager, not lazy — registries are `Arc`-frozen after
//! bootstrap, so discovery must complete before they're built.

mod auth;
mod client;
mod tool;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, ConfigSource, McpServerConfig, McpTransportConfig};
use crate::extension::gate::ExtensionGate;
use crate::extension::status::{
    DisableReason, DiscoveredTool, ExtensionRegistry, ExtensionState, ExtensionStatus,
};
use crate::extension::{ExtensionId, register_or_degrade};
use crate::sandbox::CommandSandbox;
use crate::session::{CredentialPersistence, CredentialStore};
use crate::tool::{Tool, ToolRegistry};
use auth::{CredentialStorageOutcome, McpOAuthStore, PendingAuthorization, RemoteRevocation};

pub(crate) use client::McpConnection;
pub(crate) use tool::McpTool;

/// Wall-clock budget for one server's connect-plus-discover attempt. A slow
/// or hung server contributes zero tools plus a `Failed` status rather than
/// blocking every other server (and startup itself) indefinitely.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// A browser callback remains live long enough for an ordinary sign-in while
/// still releasing its loopback listener and PKCE state if abandoned.
const AUTHORIZATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const AUTHORIZATION_SETUP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;

/// One configured server's live (or last-known) connection, shared between
/// the hub and every [`McpTool`] the server contributed.
pub(crate) struct McpServerHandle {
    config: McpServerConfig,
    sandbox: CommandSandbox,
    project_root: PathBuf,
    extension_id: ExtensionId,
    extensions: Arc<ExtensionRegistry>,
    /// `None` after a connection failure or an explicit disable; `call_tool`
    /// reports "server unavailable" rather than panicking.
    connection: Mutex<Option<Arc<McpConnection>>>,
    oauth_store: Arc<McpOAuthStore>,
    pending_authorization: Mutex<Option<PendingAuthorization>>,
    authorization_generation: AtomicU64,
    authorization_cancellation: Mutex<CancellationToken>,
}

#[derive(Debug, Clone, Copy)]
struct McpHttpTransport<'a> {
    url: &'a str,
    headers: &'a BTreeMap<String, String>,
    client_id: Option<&'a str>,
    scopes: &'a [String],
}

impl McpServerHandle {
    fn new(
        config: McpServerConfig,
        runtime: &McpRuntime,
        extension_id: ExtensionId,
        oauth_store: Arc<McpOAuthStore>,
        connection: Option<McpConnection>,
    ) -> Self {
        Self {
            config,
            sandbox: runtime.sandbox.clone(),
            project_root: runtime.project_root.clone(),
            extension_id,
            extensions: runtime.extensions.clone(),
            connection: Mutex::new(connection.map(Arc::new)),
            oauth_store,
            pending_authorization: Mutex::new(None),
            authorization_generation: AtomicU64::new(0),
            authorization_cancellation: Mutex::new(CancellationToken::new()),
        }
    }

    async fn connection(&self) -> Option<Arc<McpConnection>> {
        self.connection.lock().await.clone()
    }
}

pub(crate) struct McpHub {
    servers: BTreeMap<String, Arc<McpServerHandle>>,
    credential_persistence: std::sync::RwLock<CredentialPersistence>,
}

/// Secret-free result of explicitly probing enabled MCP servers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpReachabilityReport {
    pub(crate) enabled: usize,
    pub(crate) reachable: usize,
    pub(crate) failed_servers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpAuthorizationStart {
    pub authorization_url: String,
    pub redirect_uri: String,
    pub persistence: CredentialPersistence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpAuthorizationCompletion {
    Reconnected {
        persistence: CredentialPersistence,
        fell_back: bool,
    },
    RestartRequired {
        discovered_tools: usize,
        persistence: CredentialPersistence,
        fell_back: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum McpRevocationOutcome {
    RevokedRemotely,
    ClearedLocally,
    ClearedLocallyBySandbox,
    ClearedLocallyAfterRemoteFailure(String),
    NotAuthorized,
}

/// Shared runtime dependencies for MCP server discovery and reloads.
///
/// Keeping these together makes the startup trust boundary explicit: every
/// transport receives the same extension gate, command sandbox, and workspace
/// root that the session was initialized with.
#[derive(Clone)]
struct McpRuntime {
    gate: Arc<ExtensionGate>,
    extensions: Arc<ExtensionRegistry>,
    sandbox: CommandSandbox,
    project_root: PathBuf,
    credentials: CredentialStore,
    credential_persistence: CredentialPersistence,
}

impl McpHub {
    /// Connect every enabled server concurrently, discover its tools, and
    /// return both the hub (for `/mcp` enable/disable/reload) and the
    /// flattened, namespaced tool list in `(server, tool)` sorted order — a
    /// byte-stable prompt suffix across restarts regardless of which server
    /// answered first.
    pub(crate) async fn connect_and_discover(
        config: &Config,
        gate: Arc<ExtensionGate>,
        extensions: Arc<ExtensionRegistry>,
        sandbox: CommandSandbox,
        project_root: PathBuf,
        credentials: CredentialStore,
        credential_persistence: CredentialPersistence,
    ) -> (Arc<McpHub>, Vec<Arc<McpTool>>) {
        let enabled: Vec<(String, McpServerConfig, ConfigSource)> = config
            .mcp_servers
            .iter()
            .filter(|(_, (server, _))| server.enabled)
            .map(|(name, (server, source))| (name.clone(), server.clone(), *source))
            .collect();

        let runtime = McpRuntime {
            gate,
            extensions,
            sandbox,
            project_root,
            credentials,
            credential_persistence,
        };
        let attempts = enabled.into_iter().map(|(name, server_config, source)| {
            let runtime = runtime.clone();
            async move {
                // `connect` must own its config (not borrow `server_config`),
                // since `server_config` is also moved into `attempt_and_register`
                // below and the borrow would otherwise outlive it across the
                // `.await` inside that call.
                let connect_config = server_config.clone();
                let connect_runtime = runtime.clone();
                let oauth_store = Arc::new(McpOAuthStore::new(
                    runtime.credentials.clone(),
                    &name,
                    mcp_credential_resource(&server_config),
                    runtime.credential_persistence,
                ));
                let connect_oauth_store = oauth_store.clone();
                let connect = async move {
                    connect_and_list(
                        &connect_config,
                        &connect_runtime.sandbox,
                        &connect_runtime.project_root,
                        &connect_oauth_store,
                    )
                    .await
                };
                attempt_and_register(name, server_config, source, runtime, oauth_store, connect)
                    .await
            }
        });
        let results = futures::future::join_all(attempts).await;

        let mut servers = BTreeMap::new();
        let mut tools: Vec<Arc<McpTool>> = Vec::new();
        for ((name, handle), server_tools) in results {
            servers.insert(name, handle);
            tools.extend(server_tools);
        }
        (
            Arc::new(McpHub {
                servers,
                credential_persistence: std::sync::RwLock::new(credential_persistence),
            }),
            tools,
        )
    }

    /// Recheck every server configured as enabled when this hub was built.
    /// Failed startup connections are retried without mutating the live hub.
    pub(crate) async fn probe_reachability(&self) -> McpReachabilityReport {
        let attempts = self.servers.iter().map(|(name, handle)| async move {
            let reachable = match handle.connection().await {
                Some(connection) => tokio::time::timeout(STARTUP_TIMEOUT, connection.list_tools())
                    .await
                    .is_ok_and(|result| result.is_ok()),
                None => tokio::time::timeout(
                    STARTUP_TIMEOUT,
                    connect_and_list(
                        &handle.config,
                        &handle.sandbox,
                        &handle.project_root,
                        &handle.oauth_store,
                    ),
                )
                .await
                .is_ok_and(|result| result.is_ok()),
            };
            (name.clone(), reachable)
        });
        reachability_report(futures::future::join_all(attempts).await)
    }

    /// Session-only enable/disable (`/mcp enable|disable <server>`). Flips
    /// the shared `ExtensionRegistry` state; `McpTool::execute` checks it on
    /// every call, so a disabled server's calls short-circuit visibly even
    /// though the advertised tool array (registries are frozen after
    /// bootstrap) is unchanged until restart. Returns `false` for an unknown
    /// server name.
    pub(crate) fn set_enabled(&self, server: &str, enabled: bool) -> bool {
        let Some(handle) = self.servers.get(server) else {
            return false;
        };
        let state = if enabled {
            ExtensionState::Enabled
        } else {
            ExtensionState::Disabled {
                reason: DisableReason::Session,
            }
        };
        handle.extensions.set_state(&handle.extension_id, state);
        true
    }

    /// Drop and reconnect one server (`/mcp reload <server>`). Refreshes the
    /// connection only — a schema change on the remote side still needs a
    /// restart to reach the (frozen) advertised tool array. For the same
    /// reason we deliberately leave the `ExtensionStatus` tool inventory
    /// untouched (`set_state` mutates only `state`): it must keep reflecting
    /// the startup-registered set, which is what `/mcp tools` reports as
    /// actually callable.
    pub(crate) async fn reload(&self, server: &str) -> Result<()> {
        let Some(handle) = self.servers.get(server) else {
            anyhow::bail!("unknown MCP server '{server}'");
        };
        *handle.connection.lock().await = None;
        let outcome = tokio::time::timeout(
            STARTUP_TIMEOUT,
            connect_and_list(
                &handle.config,
                &handle.sandbox,
                &handle.project_root,
                &handle.oauth_store,
            ),
        )
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "did not respond within {STARTUP_TIMEOUT:?}"
            ))
        });
        match outcome {
            Ok((connection, _tools)) => {
                *handle.connection.lock().await = Some(Arc::new(connection));
                handle
                    .extensions
                    .set_state(&handle.extension_id, ExtensionState::Enabled);
                Ok(())
            }
            Err(err) => {
                handle.extensions.set_state(
                    &handle.extension_id,
                    ExtensionState::Failed {
                        error: format!("{err:#}"),
                    },
                );
                Err(err)
            }
        }
    }

    /// Start OAuth 2.1 authorization for one configured HTTP server. A
    /// short-lived loopback callback listener completes normal browser flows;
    /// `/mcp callback` can submit the same redirect URL when the browser runs
    /// on another device.
    pub(crate) async fn begin_authorization(
        &self,
        server: &str,
        persistence: CredentialPersistence,
    ) -> Result<McpAuthorizationStart> {
        let handle = self.server(server)?;
        let http = handle.http_transport()?;
        if has_authorization_header(http.headers) {
            anyhow::bail!(
                "MCP server '{server}' already has a static Authorization header; remove it before using OAuth"
            );
        }
        if handle.sandbox.is_active() && handle.sandbox.deny_network() {
            anyhow::bail!(
                "MCP OAuth for '{server}' is blocked because the command sandbox has network denied. Allow network with `/sandbox net off` before authorizing."
            );
        }

        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .context("failed to bind the MCP OAuth loopback callback")?;
        let callback_address = listener
            .local_addr()
            .context("failed to read the MCP OAuth callback address")?;
        let redirect_uri = format!("http://{callback_address}/callback");
        let generation = handle
            .authorization_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        handle.oauth_store.set_persistence(persistence).await;
        let (pending, authorization_url) = tokio::time::timeout(
            AUTHORIZATION_SETUP_TIMEOUT,
            PendingAuthorization::begin(
                http.url,
                &redirect_uri,
                http.client_id,
                http.scopes,
                persistence,
                generation,
            ),
        )
        .await
        .with_context(|| {
            format!("MCP OAuth discovery did not complete within {AUTHORIZATION_SETUP_TIMEOUT:?}")
        })??;
        *handle.pending_authorization.lock().await = Some(pending);
        let cancellation = CancellationToken::new();
        {
            let mut current = handle.authorization_cancellation.lock().await;
            current.cancel();
            *current = cancellation.clone();
        }
        handle.extensions.set_state(
            &handle.extension_id,
            ExtensionState::Degraded {
                warning: format!(
                    "OAuth authorization pending; finish in the browser or run /mcp callback {server} <redirect-url>"
                ),
            },
        );

        let weak_handle = Arc::downgrade(&handle);
        tokio::spawn(async move {
            run_oauth_callback_listener(
                weak_handle,
                listener,
                generation,
                callback_address,
                cancellation,
            )
            .await;
        });

        Ok(McpAuthorizationStart {
            authorization_url,
            redirect_uri,
            persistence,
        })
    }

    /// Complete a pending OAuth flow from a pasted redirect URL. This is the
    /// headless/remote-browser fallback for the loopback listener above.
    pub(crate) async fn complete_authorization(
        &self,
        server: &str,
        callback_url: &str,
    ) -> Result<McpAuthorizationCompletion> {
        let handle = self.server(server)?;
        let generation = handle
            .pending_authorization
            .lock()
            .await
            .as_ref()
            .map(|pending| pending.generation)
            .with_context(|| {
                format!("no OAuth authorization is pending for MCP server '{server}'")
            })?;
        handle
            .complete_authorization(generation, callback_url)
            .await
    }

    /// Best-effort RFC 7009 revocation followed by unconditional local token
    /// removal. The server remains configured but visibly requires a new login.
    pub(crate) async fn revoke_authorization(&self, server: &str) -> Result<McpRevocationOutcome> {
        let handle = self.server(server)?;
        let http = handle.http_transport()?;
        if has_authorization_header(http.headers) {
            anyhow::bail!(
                "MCP server '{server}' uses a static Authorization header; remove or rotate it in config"
            );
        }
        handle
            .authorization_generation
            .fetch_add(1, Ordering::Relaxed);
        handle.authorization_cancellation.lock().await.cancel();
        *handle.pending_authorization.lock().await = None;
        *handle.connection.lock().await = None;
        handle.extensions.set_state(
            &handle.extension_id,
            ExtensionState::Failed {
                error: format!(
                    "OAuth authorization is not available; run /mcp auth {server} to sign in"
                ),
            },
        );
        let outcome = if handle.sandbox.is_active() && handle.sandbox.deny_network() {
            auth::clear_local(&handle.oauth_store).await?;
            RemoteRevocation::SkippedBySandbox
        } else {
            auth::revoke(http.url, &handle.oauth_store).await?
        };
        Ok(match outcome {
            RemoteRevocation::Completed => McpRevocationOutcome::RevokedRemotely,
            RemoteRevocation::Unsupported => McpRevocationOutcome::ClearedLocally,
            RemoteRevocation::SkippedBySandbox => McpRevocationOutcome::ClearedLocallyBySandbox,
            RemoteRevocation::Failed(error) => {
                McpRevocationOutcome::ClearedLocallyAfterRemoteFailure(error)
            }
            RemoteRevocation::NotAuthorized => McpRevocationOutcome::NotAuthorized,
        })
    }

    /// Server names known to the hub (configured and enabled at startup),
    /// for `/mcp`'s usage errors.
    pub(crate) fn server_names(&self) -> Vec<&str> {
        self.servers.keys().map(String::as_str).collect()
    }

    pub(crate) fn credential_persistence(&self) -> CredentialPersistence {
        *self
            .credential_persistence
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn set_credential_persistence(&self, persistence: CredentialPersistence) {
        *self
            .credential_persistence
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = persistence;
    }

    fn server(&self, server: &str) -> Result<Arc<McpServerHandle>> {
        self.servers
            .get(server)
            .cloned()
            .with_context(|| unknown_server_error(server, &self.server_names()))
    }
}

/// Probe enabled MCP configuration without constructing the full extension runtime.
pub(crate) async fn probe_configured_reachability(
    config: &Config,
    sandbox: &CommandSandbox,
    project_root: &Path,
    credentials: CredentialStore,
    credential_persistence: CredentialPersistence,
) -> McpReachabilityReport {
    let attempts = config
        .mcp_servers
        .iter()
        .filter(|(_, (server, _))| server.enabled)
        .map(|(name, (server, _))| {
            let name = name.clone();
            let server = server.clone();
            let oauth_store = McpOAuthStore::new(
                credentials.clone(),
                &name,
                mcp_credential_resource(&server),
                credential_persistence,
            );
            async move {
                let reachable = tokio::time::timeout(
                    STARTUP_TIMEOUT,
                    connect_and_list(&server, sandbox, project_root, &oauth_store),
                )
                .await
                .is_ok_and(|result| result.is_ok());
                (name, reachable)
            }
        });
    reachability_report(futures::future::join_all(attempts).await)
}

fn reachability_report(attempts: Vec<(String, bool)>) -> McpReachabilityReport {
    let enabled = attempts.len();
    let reachable = attempts.iter().filter(|(_, reachable)| *reachable).count();
    let mut failed_servers = attempts
        .into_iter()
        .filter_map(|(name, reachable)| (!reachable).then_some(name))
        .collect::<Vec<_>>();
    failed_servers.sort();
    McpReachabilityReport {
        enabled,
        reachable,
        failed_servers,
    }
}

impl McpServerHandle {
    fn http_transport(&self) -> Result<McpHttpTransport<'_>> {
        match &self.config.transport {
            McpTransportConfig::Http {
                url,
                headers,
                oauth_client_id,
                oauth_scopes,
            } => Ok(McpHttpTransport {
                url,
                headers,
                client_id: oauth_client_id.as_deref(),
                scopes: oauth_scopes,
            }),
            McpTransportConfig::Stdio { .. } => {
                anyhow::bail!("OAuth authorization is available only for HTTP MCP servers")
            }
        }
    }

    async fn complete_authorization(
        &self,
        generation: u64,
        callback_url: &str,
    ) -> Result<McpAuthorizationCompletion> {
        if self.sandbox.is_active() && self.sandbox.deny_network() {
            anyhow::bail!(
                "MCP OAuth callback is blocked because the command sandbox has network denied. Allow network with `/sandbox net off`, then submit the callback again."
            );
        }
        let pending = {
            let mut guard = self.pending_authorization.lock().await;
            let is_current = guard
                .as_ref()
                .is_some_and(|pending| pending.generation == generation);
            if !is_current {
                anyhow::bail!("this MCP OAuth callback is stale or already completed");
            }
            guard.take().context("MCP OAuth state disappeared")?
        };
        self.authorization_cancellation.lock().await.cancel();
        let mut pending = pending;
        let storage = match pending.complete(callback_url, &self.oauth_store).await {
            Ok(storage) => storage,
            Err(error) => {
                self.extensions.set_state(
                    &self.extension_id,
                    ExtensionState::Failed {
                        error: format!(
                            "OAuth callback failed; run /mcp auth {} to start again",
                            mcp_server_name(&self.extension_id)
                        ),
                    },
                );
                return Err(error);
            }
        };

        self.reconnect_after_authorization(storage).await
    }

    async fn reconnect_after_authorization(
        &self,
        storage: CredentialStorageOutcome,
    ) -> Result<McpAuthorizationCompletion> {
        let outcome = tokio::time::timeout(
            STARTUP_TIMEOUT,
            connect_and_list(
                &self.config,
                &self.sandbox,
                &self.project_root,
                &self.oauth_store,
            ),
        )
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "did not respond within {STARTUP_TIMEOUT:?}"
            ))
        });
        let (connection, discovered) = match outcome {
            Ok(value) => value,
            Err(error) => {
                let redacted_error = redacted_error(&error);
                self.extensions.set_state(
                    &self.extension_id,
                    ExtensionState::Failed {
                        error: format!(
                            "OAuth login was saved, but reconnect failed: {redacted_error}. Try /mcp reload {}",
                            mcp_server_name(&self.extension_id)
                        ),
                    },
                );
                return Err(error).context("MCP OAuth login succeeded but reconnect failed");
            }
        };
        *self.connection.lock().await = Some(Arc::new(connection));

        let registered_tools = self
            .extensions
            .snapshot()
            .into_iter()
            .find(|status| status.id == self.extension_id)
            .map(|status| status.tools.len())
            .unwrap_or(0);
        let fell_back = storage.actual != storage.requested;
        if registered_tools == 0 && !discovered.is_empty() {
            let storage_note = if fell_back {
                "; the requested credential store was unavailable, so this login is session-only"
            } else {
                ""
            };
            self.extensions.set_state(
                &self.extension_id,
                ExtensionState::Degraded {
                    warning: format!(
                        "authorized; restart bonsai to register {} discovered tool(s){storage_note}",
                        discovered.len(),
                    ),
                },
            );
            Ok(McpAuthorizationCompletion::RestartRequired {
                discovered_tools: discovered.len(),
                persistence: storage.actual,
                fell_back,
            })
        } else if fell_back {
            self.extensions.set_state(
                &self.extension_id,
                ExtensionState::Degraded {
                    warning:
                        "connected; the requested credential store was unavailable, so this login is session-only"
                            .to_string(),
                },
            );
            Ok(McpAuthorizationCompletion::Reconnected {
                persistence: storage.actual,
                fell_back,
            })
        } else {
            self.extensions
                .set_state(&self.extension_id, ExtensionState::Enabled);
            Ok(McpAuthorizationCompletion::Reconnected {
                persistence: storage.actual,
                fell_back,
            })
        }
    }

    async fn expire_authorization(&self, generation: u64) {
        let expired = {
            let mut guard = self.pending_authorization.lock().await;
            if guard
                .as_ref()
                .is_some_and(|pending| pending.generation == generation)
            {
                guard.take();
                true
            } else {
                false
            }
        };
        if expired {
            self.extensions.set_state(
                &self.extension_id,
                ExtensionState::Failed {
                    error: format!(
                        "OAuth authorization timed out; run /mcp auth {} to try again",
                        mcp_server_name(&self.extension_id)
                    ),
                },
            );
        }
    }
}

fn unknown_server_error(server: &str, known: &[&str]) -> String {
    if known.is_empty() {
        format!("unknown MCP server '{server}'; no servers are configured")
    } else {
        format!(
            "unknown MCP server '{server}'; configured: {}",
            known.join(", ")
        )
    }
}

fn mcp_server_name(id: &ExtensionId) -> &str {
    match id {
        ExtensionId::McpServer(name) => name,
        _ => "unknown",
    }
}

async fn run_oauth_callback_listener(
    handle: Weak<McpServerHandle>,
    listener: TcpListener,
    generation: u64,
    callback_address: std::net::SocketAddr,
    cancellation: CancellationToken,
) {
    let accepted = tokio::select! {
        () = cancellation.cancelled() => return,
        accepted = tokio::time::timeout(AUTHORIZATION_TIMEOUT, listener.accept()) => accepted,
    };
    let Some(handle) = handle.upgrade() else {
        return;
    };
    let (mut stream, _) = match accepted {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => {
            tracing::warn!(%error, "MCP OAuth callback listener failed");
            handle.expire_authorization(generation).await;
            return;
        }
        Err(_) => {
            handle.expire_authorization(generation).await;
            return;
        }
    };

    let result = match read_callback_url(&mut stream, callback_address).await {
        Ok(callback_url) => {
            handle
                .complete_authorization(generation, &callback_url)
                .await
        }
        Err(error) => Err(error),
    };
    let body = match &result {
        Ok(McpAuthorizationCompletion::Reconnected {
            persistence,
            fell_back,
        }) => format!(
            "Authorization complete. The MCP server is connected; the login uses {}{}. You can close this page.",
            persistence.label(),
            if *fell_back { " (fallback)" } else { "" }
        ),
        Ok(McpAuthorizationCompletion::RestartRequired {
            discovered_tools,
            persistence,
            fell_back,
        }) => format!(
            "Authorization complete; the login uses {}{}. Restart bonsai to register {discovered_tools} discovered MCP tool(s). You can close this page.",
            persistence.label(),
            if *fell_back { " (fallback)" } else { "" }
        ),
        Err(_) => {
            "Authorization could not be completed. Return to bonsai and start authorization again."
                .to_string()
        }
    };
    if let Err(error) = write_callback_response(&mut stream, result.is_ok(), &body).await {
        tracing::debug!(%error, "failed to write MCP OAuth browser response");
    }
}

async fn read_callback_url(
    stream: &mut TcpStream,
    callback_address: std::net::SocketAddr,
) -> Result<String> {
    let mut request = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .context("failed to read MCP OAuth callback")?;
        if read == 0 {
            anyhow::bail!("MCP OAuth callback closed before sending a request");
        }
        request.extend_from_slice(&chunk[..read]);
        if request.len() > MAX_CALLBACK_REQUEST_BYTES {
            anyhow::bail!("MCP OAuth callback request is too large");
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&request).context("MCP OAuth callback is not UTF-8")?;
    let request_line = request
        .lines()
        .next()
        .context("MCP OAuth callback request is empty")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if method != "GET" || !version.starts_with("HTTP/") || parts.next().is_some() {
        anyhow::bail!("invalid MCP OAuth callback request line");
    }
    if target != "/callback" && !target.starts_with("/callback?") {
        anyhow::bail!("invalid MCP OAuth callback path");
    }
    Ok(format!("http://{callback_address}{target}"))
}

async fn write_callback_response(stream: &mut TcpStream, success: bool, body: &str) -> Result<()> {
    let status = if success { "200 OK" } else { "400 Bad Request" };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("failed to write MCP OAuth callback response")?;
    stream
        .shutdown()
        .await
        .context("failed to close MCP OAuth callback response")
}

/// Register every discovered MCP tool into `registry`, in `(server, tool)`
/// order, immediately after `webfetch` — the byte-stable append-last seam
/// (`src/bootstrap.rs`). A wire-name collision (a builtin, or an earlier
/// extension) marks the *owning server* `Degraded` in `extensions` and skips
/// just that tool; the builtin (or earlier registrant) always wins and
/// registration order for the rest is unaffected.
pub(crate) fn register_mcp_tools(
    registry: &mut ToolRegistry,
    tools: &[Arc<McpTool>],
    extensions: &ExtensionRegistry,
) {
    for tool in tools {
        if let Err(warning) = register_or_degrade(registry, tool.clone() as Arc<dyn Tool>) {
            let id = ExtensionId::McpServer(tool.server_name().to_string());
            // The skipped tool isn't callable, so drop it from the inventory
            // `/mcp tools` reads before marking the owning server degraded.
            extensions.remove_discovered_tool(&id, tool.remote_name());
            extensions.set_state(&id, ExtensionState::Degraded { warning });
        }
    }
}

/// One server's connect-plus-discover attempt, bounded by [`STARTUP_TIMEOUT`]
/// and folded into an `ExtensionStatus`. `connect` is a parameter (rather
/// than dispatching on `server_config.transport` here) so `src/mcp/tests.rs`
/// can drive this exact registration/collision/capability logic against an
/// in-process fake server, without a real stdio/HTTP transport.
async fn attempt_and_register(
    name: String,
    server_config: McpServerConfig,
    source: ConfigSource,
    runtime: McpRuntime,
    oauth_store: Arc<McpOAuthStore>,
    connect: impl std::future::Future<Output = Result<(McpConnection, Vec<rmcp::model::Tool>)>>,
) -> ((String, Arc<McpServerHandle>), Vec<Arc<McpTool>>) {
    let outcome = tokio::time::timeout(STARTUP_TIMEOUT, connect)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "did not respond within {STARTUP_TIMEOUT:?}"
            ))
        });
    let extension_id = ExtensionId::McpServer(name.clone());
    match outcome {
        Ok((connection, mut discovered)) => {
            discovered.sort_by(|a, b| a.name.cmp(&b.name));
            let allow_tools = &server_config.allow_tools;
            discovered.retain(|remote_tool| {
                allow_tools.is_empty()
                    || allow_tools
                        .iter()
                        .any(|allowed| allowed.as_str() == remote_tool.name.as_ref())
            });

            let handle = Arc::new(McpServerHandle::new(
                server_config.clone(),
                &runtime,
                extension_id.clone(),
                oauth_store.clone(),
                Some(connection),
            ));
            let mut tools = Vec::new();
            let mut inventory = Vec::with_capacity(discovered.len());
            for remote_tool in &discovered {
                let remote_name = remote_tool.name.to_string();
                let remote_description = remote_tool.description.as_deref().unwrap_or_default();
                inventory.push(DiscoveredTool {
                    name: remote_name.clone(),
                    description: remote_description
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                });
                tools.push(Arc::new(McpTool::from_discovered_tool(
                    &name,
                    remote_tool,
                    handle.clone(),
                    runtime.gate.clone(),
                )));
            }
            let auth_detail = match &server_config.transport {
                McpTransportConfig::Http { headers, .. } if has_authorization_header(headers) => {
                    ", static auth".to_string()
                }
                McpTransportConfig::Http { .. } if oauth_store.has_cached_credentials().await => {
                    format!(", OAuth ({})", oauth_store.persistence().await.label())
                }
                _ => String::new(),
            };
            runtime.extensions.upsert(ExtensionStatus {
                id: extension_id,
                source,
                capabilities: server_config.declared.clone(),
                state: ExtensionState::Enabled,
                detail: format!("{} tool(s){auth_detail}", discovered.len()),
                tools: inventory,
            });
            ((name, handle), tools)
        }
        Err(err) => {
            let error = connection_failure_message(&name, &server_config, &err);
            runtime.extensions.upsert(ExtensionStatus {
                id: extension_id.clone(),
                source,
                capabilities: server_config.declared.clone(),
                state: ExtensionState::Failed { error },
                detail: String::new(),
                tools: Vec::new(),
            });
            let handle = Arc::new(McpServerHandle::new(
                server_config,
                &runtime,
                extension_id,
                oauth_store,
                None,
            ));
            ((name, handle), Vec::new())
        }
    }
}

async fn connect_and_list(
    config: &McpServerConfig,
    sandbox: &CommandSandbox,
    project_root: &Path,
    oauth_store: &McpOAuthStore,
) -> Result<(McpConnection, Vec<rmcp::model::Tool>)> {
    let connection = match &config.transport {
        McpTransportConfig::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            McpConnection::connect_stdio(command, args, env, cwd.as_deref(), sandbox, project_root)
                .await?
        }
        McpTransportConfig::Http { url, headers, .. } => {
            if sandbox.is_active() && sandbox.deny_network() {
                anyhow::bail!(
                    "MCP HTTP server '{url}' is blocked: the command sandbox has network denied. Allow network with `/sandbox net off` before connecting."
                );
            }
            if has_authorization_header(headers) || !oauth_store.has_credentials().await? {
                McpConnection::connect_http(url, headers).await?
            } else {
                McpConnection::connect_http_authorized(url, headers, oauth_store).await?
            }
        }
    };
    let tools = connection.list_tools().await?;
    Ok((connection, tools))
}

fn has_authorization_header(headers: &BTreeMap<String, String>) -> bool {
    headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("authorization"))
}

fn mcp_credential_resource(config: &McpServerConfig) -> &str {
    match &config.transport {
        McpTransportConfig::Http { url, .. } => url,
        McpTransportConfig::Stdio { .. } => "stdio",
    }
}

fn connection_failure_message(
    name: &str,
    config: &McpServerConfig,
    error: &anyhow::Error,
) -> String {
    let mut message = redacted_error(error);
    if let McpTransportConfig::Http { headers, .. } = &config.transport
        && !has_authorization_header(headers)
    {
        message.push_str(&format!(
            ". If this server requires OAuth, run /mcp auth {name}"
        ));
    }
    message
}

fn redacted_error(error: &anyhow::Error) -> String {
    crate::redact::redact(&format!("{error:#}")).into_owned()
}
