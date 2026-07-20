//! OAuth support for remote Streamable HTTP MCP servers.
//!
//! The protocol state machine comes from `rmcp`; this module binds its token
//! store to bonsai's protected credential boundary and keeps authorization
//! codes/PKCE state process-local. OAuth credentials never enter SQLite or an
//! MCP config file.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationSession, CredentialStore as RmcpCredentialStore,
    InMemoryCredentialStore, OAuthClientConfig, OAuthHttpClient, OAuthHttpClientError,
    OAuthHttpClientFuture, OAuthHttpRedirectPolicy, OAuthHttpRequest, OAuthState,
    StoredCredentials,
};
use tokio::sync::RwLock;

use crate::session::{CredentialPersistence, CredentialSource, CredentialStore};

const CREDENTIAL_KEY_PREFIX: &str = "mcp.oauth.";
const OAUTH_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const OAUTH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_OAUTH_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_OAUTH_REDIRECTS: usize = 5;
const OAUTH_REVOCATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
struct PinnedOAuthHttpClient;

impl OAuthHttpClient for PinnedOAuthHttpClient {
    fn execute(&self, operation: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let OAuthHttpRequest {
                request,
                redirect_policy,
                timeout,
                ..
            } = operation;
            let request = reqwest::Request::try_from(request)
                .map_err(|error| oauth_http_error("invalid OAuth HTTP request", error))?;
            let client = pinned_http_client(
                request.url(),
                redirect_policy,
                timeout.unwrap_or(OAUTH_HTTP_TIMEOUT),
            )
            .await
            .map_err(|error| oauth_http_error("OAuth HTTP request was blocked", error))?;
            let response = client
                .execute(request)
                .await
                .map_err(|error| oauth_http_error("OAuth HTTP request failed", error))?;

            let mut builder = http::Response::builder()
                .status(response.status())
                .version(response.version());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|error| oauth_http_error("OAuth response read failed", error))?;
                if body.len().saturating_add(chunk.len()) > MAX_OAUTH_RESPONSE_BYTES {
                    return Err(OAuthHttpClientError::new(format!(
                        "OAuth HTTP response exceeds {MAX_OAUTH_RESPONSE_BYTES} bytes"
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            builder
                .body(body)
                .map_err(|error| oauth_http_error("invalid OAuth HTTP response", error))
        })
    }
}

async fn pinned_http_client(
    url: &reqwest::Url,
    redirect_policy: OAuthHttpRedirectPolicy,
    timeout: Duration,
) -> Result<reqwest::Client> {
    validate_secure_oauth_url(url)?;
    let host = url.host_str().context("OAuth URL has no host")?;
    let port = url
        .port_or_known_default()
        .context("OAuth URL has no known port")?;
    let allow_private = is_loopback_host(host);
    let addresses =
        crate::tool::resolve_safe_http_addresses(host, port, OAUTH_CONNECT_TIMEOUT, allow_private)
            .await?;
    let redirect = match redirect_policy {
        OAuthHttpRedirectPolicy::Follow => reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_OAUTH_REDIRECTS {
                return attempt.error(std::io::Error::other("OAuth HTTP redirect limit exceeded"));
            }
            let Some(initial) = attempt.previous().first() else {
                return attempt.follow();
            };
            if same_origin(initial, attempt.url()) {
                attempt.follow()
            } else {
                attempt.error(std::io::Error::other(
                    "cross-origin OAuth HTTP redirect rejected",
                ))
            }
        }),
        OAuthHttpRedirectPolicy::Stop => reqwest::redirect::Policy::none(),
        _ => reqwest::redirect::Policy::none(),
    };
    let builder = reqwest::Client::builder()
        .redirect(redirect)
        .connect_timeout(OAUTH_CONNECT_TIMEOUT)
        .timeout(timeout)
        .user_agent(concat!("bonsai/", env!("CARGO_PKG_VERSION")));
    let builder = if host.parse::<IpAddr>().is_ok() {
        builder
    } else {
        builder.resolve_to_addrs(host, &addresses)
    };
    builder.build().context("failed to build OAuth HTTP client")
}

pub(super) async fn pinned_resource_client(server_url: &str) -> Result<reqwest::Client> {
    let url = reqwest::Url::parse(server_url).context("invalid MCP OAuth resource URL")?;
    pinned_http_client(&url, OAuthHttpRedirectPolicy::Stop, OAUTH_HTTP_TIMEOUT).await
}

fn validate_secure_oauth_url(url: &reqwest::Url) -> Result<()> {
    let host = url.host_str().context("OAuth URL has no host")?;
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("OAuth URLs with embedded credentials are not allowed");
    }
    if url.fragment().is_some() {
        anyhow::bail!("OAuth URLs must not contain a fragment");
    }
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback_host(host)) {
        anyhow::bail!("OAuth URLs must use HTTPS (except loopback testing)");
    }
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn same_origin(left: &reqwest::Url, right: &reqwest::Url) -> bool {
    left.scheme() == right.scheme()
        && left
            .host_str()
            .zip(right.host_str())
            .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
        && left.port_or_known_default() == right.port_or_known_default()
}

fn oauth_http_error(context: &str, error: impl std::fmt::Display) -> OAuthHttpClientError {
    let error = error.to_string();
    let message = crate::redact::redact(&error);
    OAuthHttpClientError::new(format!("{context}: {message}"))
}

async fn authorization_manager(server_url: &str) -> Result<AuthorizationManager> {
    let url = reqwest::Url::parse(server_url).context("invalid MCP OAuth resource URL")?;
    validate_secure_oauth_url(&url)?;
    AuthorizationManager::new_with_oauth_http_client(url, Arc::new(PinnedOAuthHttpClient))
        .await
        .context("failed to initialize MCP OAuth manager")
}

fn validate_authorization_metadata(
    metadata: &rmcp::transport::auth::AuthorizationMetadata,
) -> Result<()> {
    for (label, endpoint) in [
        (
            "authorization",
            Some(metadata.authorization_endpoint.as_str()),
        ),
        ("token", Some(metadata.token_endpoint.as_str())),
        ("registration", metadata.registration_endpoint.as_deref()),
        ("issuer", metadata.issuer.as_deref()),
    ] {
        let Some(endpoint) = endpoint else {
            continue;
        };
        let endpoint = reqwest::Url::parse(endpoint)
            .with_context(|| format!("invalid MCP OAuth {label} URL"))?;
        validate_secure_oauth_url(&endpoint)
            .with_context(|| format!("unsafe MCP OAuth {label} URL"))?;
    }
    Ok(())
}

/// A per-server view over bonsai's file/keyring/session credential boundary.
///
/// Loading probes both persistent backends so changing the global default does
/// not orphan an existing MCP login. Once found, refreshes remain on the same
/// backend. A newly authorized credential uses the choice made for that flow.
#[derive(Clone)]
pub(super) struct McpOAuthStore {
    credentials: CredentialStore,
    credential_key: Arc<str>,
    persistence: Arc<RwLock<CredentialPersistence>>,
    runtime_cache: Arc<RwLock<Option<StoredCredentials>>>,
}

impl std::fmt::Debug for McpOAuthStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpOAuthStore")
            .field("credential_key", &self.credential_key)
            .finish_non_exhaustive()
    }
}

impl McpOAuthStore {
    pub(super) fn new(
        credentials: CredentialStore,
        server: &str,
        resource_url: &str,
        persistence: CredentialPersistence,
    ) -> Self {
        let resource_hash = blake3::hash(resource_url.as_bytes()).to_hex();
        Self {
            credentials,
            credential_key: format!("{CREDENTIAL_KEY_PREFIX}{server}.{}", &resource_hash[..16])
                .into(),
            persistence: Arc::new(RwLock::new(persistence)),
            runtime_cache: Arc::new(RwLock::new(None)),
        }
    }

    pub(super) async fn set_persistence(&self, persistence: CredentialPersistence) {
        *self.persistence.write().await = persistence;
    }

    pub(super) async fn persistence(&self) -> CredentialPersistence {
        *self.persistence.read().await
    }

    pub(super) async fn has_credentials(&self) -> Result<bool, AuthError> {
        Ok(self.load_credentials().await?.is_some())
    }

    pub(super) async fn has_cached_credentials(&self) -> bool {
        self.runtime_cache.read().await.is_some()
    }

    async fn load_credentials(&self) -> Result<Option<StoredCredentials>, AuthError> {
        if let Some(credentials) = self.runtime_cache.read().await.clone() {
            return Ok(Some(credentials));
        }

        let preferred = self.persistence().await;
        let order = match preferred {
            CredentialPersistence::File => {
                [CredentialPersistence::File, CredentialPersistence::Keyring]
            }
            CredentialPersistence::Keyring => {
                [CredentialPersistence::Keyring, CredentialPersistence::File]
            }
            CredentialPersistence::Session => {
                [CredentialPersistence::File, CredentialPersistence::Keyring]
            }
        };
        for persistence in order {
            match self.load_persistent(persistence).await {
                Ok(Some(credentials)) => {
                    *self.persistence.write().await = persistence;
                    *self.runtime_cache.write().await = Some(credentials.clone());
                    return Ok(Some(credentials));
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        server_credential = %self.credential_key,
                        source = persistence.label(),
                        %error,
                        "failed to inspect an MCP OAuth credential backend"
                    );
                }
            }
        }
        Ok(None)
    }

    async fn load_persistent(
        &self,
        persistence: CredentialPersistence,
    ) -> Result<Option<StoredCredentials>, AuthError> {
        let source = persistence.source();
        let encoded = self
            .credentials
            .get(&source, &self.credential_key)
            .await
            .map_err(|error| auth_store_error("load", error))?;
        encoded
            .map(|encoded| {
                serde_json::from_str(&encoded).map_err(|error| {
                    AuthError::InternalError(format!(
                        "stored MCP OAuth credential is invalid: {error}"
                    ))
                })
            })
            .transpose()
    }

    async fn save_credentials(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let requested = self.persistence().await;
        if requested == CredentialPersistence::Session {
            *self.runtime_cache.write().await = Some(credentials);
            self.delete_persistent_best_effort().await;
            return Ok(());
        }

        let encoded = serde_json::to_string(&credentials).map_err(|error| {
            AuthError::InternalError(format!("failed to encode MCP OAuth credential: {error}"))
        })?;
        let source = requested.source();
        match self
            .credentials
            .set(&source, &self.credential_key, &encoded)
            .await
        {
            Ok(()) => {
                *self.runtime_cache.write().await = Some(credentials);
                self.delete_other_persistent(requested).await;
                Ok(())
            }
            Err(error) => {
                tracing::warn!(
                    server_credential = %self.credential_key,
                    requested = requested.label(),
                    %error,
                    "MCP OAuth credential store is unavailable; keeping the login in this process only"
                );
                *self.runtime_cache.write().await = Some(credentials);
                *self.persistence.write().await = CredentialPersistence::Session;
                self.delete_persistent_best_effort().await;
                Ok(())
            }
        }
    }

    async fn clear_credentials(&self) -> Result<(), AuthError> {
        *self.runtime_cache.write().await = None;
        let retained_source = match self.persistence().await {
            CredentialPersistence::File => Some(CredentialSource::File),
            CredentialPersistence::Keyring => Some(CredentialSource::Keyring),
            CredentialPersistence::Session => None,
        };
        if let Some(source) = &retained_source {
            self.credentials
                .delete(source, &self.credential_key)
                .await
                .map_err(|error| auth_store_error("clear", error))?;
        }
        for source in [CredentialSource::File, CredentialSource::Keyring] {
            if retained_source.as_ref() == Some(&source) {
                continue;
            }
            if let Err(error) = self.credentials.delete(&source, &self.credential_key).await {
                tracing::warn!(
                    server_credential = %self.credential_key,
                    source = source.label(),
                    %error,
                    "failed to clear an alternate MCP OAuth credential backend"
                );
            }
        }
        Ok(())
    }

    async fn delete_other_persistent(&self, retained: CredentialPersistence) {
        let other = match retained {
            CredentialPersistence::File => CredentialSource::Keyring,
            CredentialPersistence::Keyring => CredentialSource::File,
            CredentialPersistence::Session => return,
        };
        if let Err(error) = self.credentials.delete(&other, &self.credential_key).await {
            tracing::warn!(
                server_credential = %self.credential_key,
                source = other.label(),
                %error,
                "failed to remove superseded MCP OAuth credential"
            );
        }
    }

    async fn delete_persistent_best_effort(&self) {
        for source in [CredentialSource::File, CredentialSource::Keyring] {
            if let Err(error) = self.credentials.delete(&source, &self.credential_key).await {
                tracing::warn!(
                    server_credential = %self.credential_key,
                    source = source.label(),
                    %error,
                    "failed to clear superseded MCP OAuth credential"
                );
            }
        }
    }
}

#[async_trait]
impl RmcpCredentialStore for McpOAuthStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        self.load_credentials().await
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        self.save_credentials(credentials).await
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.clear_credentials().await
    }
}

fn auth_store_error(operation: &str, error: anyhow::Error) -> AuthError {
    AuthError::InternalError(format!(
        "failed to {operation} protected MCP OAuth credential: {error:#}"
    ))
}

/// PKCE authorization state. The real credential store is not touched until
/// the callback exchanges successfully, so abandoning a login cannot replace
/// a still-usable token with an incomplete dynamic-client registration.
pub(super) struct PendingAuthorization {
    state: OAuthState,
    staging: InMemoryCredentialStore,
    redirect_uri: String,
    requested_persistence: CredentialPersistence,
    pub(super) generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CredentialStorageOutcome {
    pub requested: CredentialPersistence,
    pub actual: CredentialPersistence,
}

impl std::fmt::Debug for PendingAuthorization {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PendingAuthorization")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl PendingAuthorization {
    pub(super) async fn begin(
        server_url: &str,
        redirect_uri: &str,
        configured_client_id: Option<&str>,
        configured_scopes: &[String],
        requested_persistence: CredentialPersistence,
        generation: u64,
    ) -> Result<(Self, String)> {
        let staging = InMemoryCredentialStore::new();
        let mut manager = authorization_manager(server_url).await?;
        manager.set_credential_store(staging.clone());
        let metadata = manager
            .discover_metadata()
            .await
            .context("failed to discover MCP OAuth metadata")?;
        validate_authorization_metadata(&metadata)?;
        let pkce_methods = metadata.code_challenge_methods_supported.as_ref().context(
            "MCP authorization server does not advertise PKCE methods; S256 is required",
        )?;
        if !pkce_methods.iter().any(|method| method == "S256") {
            anyhow::bail!("MCP authorization server does not support required PKCE method S256");
        }
        manager.set_metadata(metadata);
        let scopes = if configured_scopes.is_empty() {
            manager.select_scopes(None, &[])
        } else {
            configured_scopes.to_vec()
        };
        let scope_refs = scopes.iter().map(String::as_str).collect::<Vec<_>>();
        let session = if let Some(client_id) = configured_client_id {
            let config =
                OAuthClientConfig::new(client_id, redirect_uri).with_scopes(scopes.clone());
            manager
                .configure_client(config)
                .context("failed to configure the MCP OAuth client id")?;
            let authorization_url = manager
                .get_authorization_url(&scope_refs)
                .await
                .context("failed to build MCP OAuth authorization URL")?;
            AuthorizationSession::for_scope_upgrade(manager, authorization_url, redirect_uri)
        } else {
            AuthorizationSession::new(
                manager,
                &scope_refs,
                redirect_uri,
                Some("bonsai"),
                None,
            )
            .await
            .context(
                "failed to dynamically register the MCP OAuth client; configure oauth_client_id when the authorization server does not support dynamic registration",
            )?
        };
        let state = OAuthState::Session(session);
        let authorization_url = state
            .get_authorization_url()
            .await
            .context("failed to build MCP OAuth authorization URL")?;
        Ok((
            Self {
                state,
                staging,
                redirect_uri: redirect_uri.to_string(),
                requested_persistence,
                generation,
            },
            authorization_url,
        ))
    }

    pub(super) async fn complete(
        &mut self,
        callback_url: &str,
        store: &McpOAuthStore,
    ) -> Result<CredentialStorageOutcome> {
        let expected = reqwest::Url::parse(&self.redirect_uri)
            .context("stored MCP OAuth redirect URI is invalid")?;
        let callback =
            reqwest::Url::parse(callback_url).context("MCP OAuth callback URL is invalid")?;
        if !same_origin(&expected, &callback)
            || expected.path() != callback.path()
            || callback.fragment().is_some()
        {
            anyhow::bail!("MCP OAuth callback URL does not match the loopback redirect URI");
        }
        self.state
            .handle_callback_url(callback_url)
            .await
            .context("MCP OAuth callback was rejected")?;
        let credentials = RmcpCredentialStore::load(&self.staging)
            .await
            .context("MCP OAuth exchange returned no readable credentials")?
            .context("MCP OAuth exchange completed without credentials")?;
        RmcpCredentialStore::save(store, credentials)
            .await
            .context("failed to save MCP OAuth credentials")?;
        Ok(CredentialStorageOutcome {
            requested: self.requested_persistence,
            actual: store.persistence().await,
        })
    }
}

/// Build an initialized manager only when this server has a stored login.
/// `get_access_token` on the authenticated transport then performs proactive
/// refresh and persists rotated refresh tokens through [`McpOAuthStore`].
pub(super) async fn stored_authorization_manager(
    server_url: &str,
    store: &McpOAuthStore,
) -> Result<Option<AuthorizationManager>> {
    if !store
        .has_credentials()
        .await
        .context("failed to inspect MCP OAuth credentials")?
    {
        return Ok(None);
    }
    let mut manager = authorization_manager(server_url).await?;
    manager.set_credential_store(store.clone());
    if !manager
        .initialize_from_store()
        .await
        .context("failed to restore MCP OAuth credentials")?
    {
        return Ok(None);
    }
    Ok(Some(manager))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RemoteRevocation {
    Completed,
    Unsupported,
    SkippedBySandbox,
    Failed(String),
    NotAuthorized,
}

pub(super) async fn clear_local(store: &McpOAuthStore) -> Result<()> {
    store
        .clear_credentials()
        .await
        .context("failed to clear local MCP OAuth credential")
}

/// Revoke remotely when the authorization server advertises RFC 7009, then
/// always clear the local credential. A remote failure is reported but never
/// turns a user-requested local sign-out into a no-op.
pub(super) async fn revoke(server_url: &str, store: &McpOAuthStore) -> Result<RemoteRevocation> {
    let Some(credentials) = store
        .load_credentials()
        .await
        .context("failed to load MCP OAuth credential for revocation")?
    else {
        return Ok(RemoteRevocation::NotAuthorized);
    };

    store
        .clear_credentials()
        .await
        .context("failed to clear local MCP OAuth credential")?;
    let remote = match tokio::time::timeout(
        OAUTH_REVOCATION_TIMEOUT,
        revoke_remote(server_url, &credentials),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("MCP OAuth revocation timed out")),
    };
    match remote {
        Ok(true) => Ok(RemoteRevocation::Completed),
        Ok(false) => Ok(RemoteRevocation::Unsupported),
        Err(error) => Ok(RemoteRevocation::Failed(
            crate::redact::redact(&format!("{error:#}")).into_owned(),
        )),
    }
}

async fn revoke_remote(server_url: &str, credentials: &StoredCredentials) -> Result<bool> {
    let manager = authorization_manager(server_url).await?;
    let metadata = manager
        .discover_metadata()
        .await
        .context("failed to discover MCP OAuth revocation metadata")?;
    validate_authorization_metadata(&metadata)?;
    let Some(endpoint) = metadata
        .additional_fields
        .get("revocation_endpoint")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(false);
    };
    validate_revocation_endpoint(endpoint)?;
    let endpoint_url =
        reqwest::Url::parse(endpoint).context("invalid MCP OAuth revocation endpoint")?;

    let token_response = credentials
        .token_response
        .as_ref()
        .context("stored MCP OAuth credential has no token")?;
    let token_json = serde_json::to_value(token_response)
        .context("failed to inspect MCP OAuth token for revocation")?;
    let (token, hint) = token_json
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .map(|token| (token, "refresh_token"))
        .or_else(|| {
            token_json
                .get("access_token")
                .and_then(serde_json::Value::as_str)
                .map(|token| (token, "access_token"))
        })
        .context("stored MCP OAuth credential has no revocable token")?;

    let mut encoded_form = reqwest::Url::parse("http://localhost/")
        .context("failed to initialize MCP OAuth revocation form")?;
    encoded_form
        .query_pairs_mut()
        .append_pair("token", token)
        .append_pair("token_type_hint", hint)
        .append_pair("client_id", &credentials.client_id);
    let encoded_form = encoded_form.query().unwrap_or_default().to_string();
    let client = pinned_http_client(
        &endpoint_url,
        OAuthHttpRedirectPolicy::Stop,
        OAUTH_HTTP_TIMEOUT,
    )
    .await
    .context("MCP OAuth revocation endpoint was blocked")?;
    let response = client
        .post(endpoint_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(encoded_form)
        .send()
        .await
        .context("MCP OAuth revocation request failed")?;
    if !response.status().is_success() {
        anyhow::bail!(
            "MCP OAuth revocation endpoint returned HTTP {}",
            response.status()
        );
    }
    Ok(true)
}

fn validate_revocation_endpoint(endpoint: &str) -> Result<()> {
    let url = reqwest::Url::parse(endpoint).context("invalid MCP OAuth revocation endpoint")?;
    validate_secure_oauth_url(&url).context("unsafe MCP OAuth revocation endpoint")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn credentials(access_token: &str) -> StoredCredentials {
        serde_json::from_value(serde_json::json!({
            "client_id": "bonsai-test",
            "token_response": {
                "access_token": access_token,
                "token_type": "Bearer",
                "expires_in": 3600
            },
            "granted_scopes": ["mcp:tools"],
            "token_received_at": 1
        }))
        .expect("valid test credentials")
    }

    #[tokio::test]
    async fn persistent_login_survives_a_changed_default() {
        let boundary = CredentialStore::memory();
        let original = McpOAuthStore::new(
            boundary.clone(),
            "remote",
            "https://mcp.example/mcp",
            CredentialPersistence::File,
        );
        original
            .save_credentials(credentials("secret-token"))
            .await
            .unwrap();

        let restored = McpOAuthStore::new(
            boundary,
            "remote",
            "https://mcp.example/mcp",
            CredentialPersistence::Keyring,
        );
        let loaded = restored.load_credentials().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "bonsai-test");
        assert_eq!(restored.persistence().await, CredentialPersistence::File);

        let different_endpoint = McpOAuthStore::new(
            restored.credentials.clone(),
            "remote",
            "https://other.example/mcp",
            CredentialPersistence::File,
        );
        assert!(
            different_endpoint
                .load_credentials()
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn session_login_is_not_written_to_persistent_backends() {
        let store = McpOAuthStore::new(
            CredentialStore::memory(),
            "remote",
            "https://mcp.example/mcp",
            CredentialPersistence::Session,
        );
        store
            .save_credentials(credentials("secret-token"))
            .await
            .unwrap();

        assert!(store.load_credentials().await.unwrap().is_some());
        assert!(
            store
                .load_persistent(CredentialPersistence::File)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .load_persistent(CredentialPersistence::Keyring)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn clear_removes_every_backend() {
        let boundary = CredentialStore::memory();
        let file = McpOAuthStore::new(
            boundary.clone(),
            "remote",
            "https://mcp.example/mcp",
            CredentialPersistence::File,
        );
        file.save_credentials(credentials("file-token"))
            .await
            .unwrap();
        file.set_persistence(CredentialPersistence::Keyring).await;
        file.save_credentials(credentials("keyring-token"))
            .await
            .unwrap();
        file.clear_credentials().await.unwrap();

        assert!(file.load_credentials().await.unwrap().is_none());
    }

    #[test]
    fn revocation_endpoint_requires_https_or_loopback() {
        assert!(validate_revocation_endpoint("https://auth.example/revoke").is_ok());
        assert!(validate_revocation_endpoint("http://127.0.0.1:8080/revoke").is_ok());
        assert!(validate_revocation_endpoint("http://auth.example/revoke").is_err());
        assert!(
            validate_secure_oauth_url(
                &reqwest::Url::parse("https://user:secret@auth.example/revoke").unwrap()
            )
            .is_err()
        );
        assert!(
            validate_secure_oauth_url(
                &reqwest::Url::parse("https://auth.example/revoke#token").unwrap()
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn browser_flow_discovers_registers_exchanges_and_persists() {
        let server = MockServer::start().await;
        let resource_url = format!("{}/mcp", server.uri());
        let resource_metadata = serde_json::json!({
            "resource": resource_url.clone(),
            "authorization_servers": [server.uri()],
            "scopes_supported": ["mcp:tools"]
        });
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-protected-resource/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(resource_metadata))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": server.uri(),
                "authorization_endpoint": format!("{}/authorize", server.uri()),
                "token_endpoint": format!("{}/token", server.uri()),
                "registration_endpoint": format!("{}/register", server.uri()),
                "revocation_endpoint": format!("{}/revoke", server.uri()),
                "scopes_supported": ["mcp:tools", "offline_access"],
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/revoke"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "client_id": "bonsai-dynamic-client",
                "redirect_uris": ["http://127.0.0.1:4242/callback"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-secret",
                "refresh_token": "refresh-secret",
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": "mcp:tools offline_access"
            })))
            .expect(2)
            .mount(&server)
            .await;

        let store = McpOAuthStore::new(
            CredentialStore::memory(),
            "remote",
            &resource_url,
            CredentialPersistence::File,
        );
        let (mut pending, authorization_url) = PendingAuthorization::begin(
            &resource_url,
            "http://127.0.0.1:4242/callback",
            None,
            &[],
            CredentialPersistence::File,
            1,
        )
        .await
        .unwrap();
        let authorization_url = reqwest::Url::parse(&authorization_url).unwrap();
        assert_eq!(authorization_url.path(), "/authorize");
        assert_eq!(
            authorization_url
                .query_pairs()
                .find(|(key, _)| key == "code_challenge_method")
                .map(|(_, value)| value.into_owned()),
            Some("S256".to_string())
        );
        assert_eq!(
            authorization_url
                .query_pairs()
                .find(|(key, _)| key == "resource")
                .map(|(_, value)| value.into_owned()),
            Some(resource_url.clone())
        );
        let csrf = authorization_url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .unwrap();
        pending
            .complete(
                &format!("http://127.0.0.1:4242/callback?code=accepted&state={csrf}"),
                &store,
            )
            .await
            .unwrap();

        let mut stored = store.load_credentials().await.unwrap().unwrap();
        assert_eq!(stored.client_id, "bonsai-dynamic-client");
        assert!(stored.token_response.is_some());
        assert_eq!(store.persistence().await, CredentialPersistence::File);

        stored.token_received_at = Some(0);
        store.save_credentials(stored).await.unwrap();
        let manager = stored_authorization_manager(&resource_url, &store)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(manager.get_access_token().await.unwrap(), "access-secret");
        assert_eq!(
            revoke(&resource_url, &store).await.unwrap(),
            RemoteRevocation::Completed
        );
        assert!(store.load_credentials().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn configured_public_client_does_not_require_dynamic_registration() {
        let server = MockServer::start().await;
        let resource_url = format!("{}/mcp", server.uri());
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-protected-resource/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "resource": resource_url.clone(),
                "authorization_servers": [server.uri()],
                "scopes_supported": ["mcp:tools"]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "issuer": server.uri(),
                "authorization_endpoint": format!("{}/authorize", server.uri()),
                "token_endpoint": format!("{}/token", server.uri()),
                "scopes_supported": ["mcp:tools"],
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"]
            })))
            .mount(&server)
            .await;

        let scopes = vec!["mcp:tools".to_string()];
        let (_pending, authorization_url) = PendingAuthorization::begin(
            &resource_url,
            "http://127.0.0.1:4242/callback",
            Some("pre-registered-bonsai"),
            &scopes,
            CredentialPersistence::Session,
            1,
        )
        .await
        .unwrap();
        let authorization_url = reqwest::Url::parse(&authorization_url).unwrap();
        assert_eq!(
            authorization_url
                .query_pairs()
                .find(|(key, _)| key == "client_id")
                .map(|(_, value)| value.into_owned()),
            Some("pre-registered-bonsai".to_string())
        );
        assert!(
            authorization_url
                .query_pairs()
                .any(|(key, value)| key == "scope" && value == "mcp:tools")
        );
    }
}
