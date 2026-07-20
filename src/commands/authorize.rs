//! Provider (de)authorization orchestration: apply an auth input to the
//! current session, persist per [`SaveAuthPolicy`], and refresh dependent
//! caches — shared by `/authorize` (TUI + headless) and the provider manager.

use async_trait::async_trait;

use crate::model_catalog::{ConnectionId, ModelCatalog};
use crate::provider::{AuthInput, ProviderRegistry};
use crate::session::{CredentialSource, SaveAuthPolicy, SessionStore};

/// Durable authorization state produced by a provider auth mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderAuthorizationState {
    /// The committed provider session is authorized.
    Authorized,
    /// The committed provider session is unauthorized.
    Unauthorized,
}

/// Derived or external cleanup work that failed after authoritative state was
/// committed. These warnings never reverse the reported auth mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderAuthMutationWarning {
    /// A live-model cache refresh or removal failed.
    LiveModelCache {
        /// Operation that failed (`refresh` or `clear`).
        operation: &'static str,
        /// Bounded error detail suitable for the command transcript.
        detail: String,
    },
    /// Durable auth was cleared but a Bonsai-owned external secret needs a
    /// retry before cleanup is complete.
    CredentialCleanup {
        /// Backend error detail suitable for the command transcript.
        detail: String,
    },
}

impl std::fmt::Display for ProviderAuthMutationWarning {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LiveModelCache { operation, detail } => write!(
                formatter,
                "Provider auth was committed, but the live model cache could not be {operation}: {detail}. It will be refreshed later."
            ),
            Self::CredentialCleanup { detail } => write!(
                formatter,
                "Provider auth was cleared, but stored credential cleanup is incomplete: {detail}. Retry /unauthorize to finish cleanup."
            ),
        }
    }
}

/// Coherent result of an authoritative provider auth mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderAuthMutationOutcome {
    /// State successfully committed to the session store.
    pub(crate) committed_state: ProviderAuthorizationState,
    /// Non-fatal failures in replaceable cache or external cleanup work.
    pub(crate) warnings: Vec<ProviderAuthMutationWarning>,
}

impl ProviderAuthMutationOutcome {
    fn committed(committed_state: ProviderAuthorizationState) -> Self {
        Self {
            committed_state,
            warnings: Vec::new(),
        }
    }
}

pub(crate) async fn authorize_provider_with_input_as_current(
    registry: &ProviderRegistry,
    id: &str,
    input: AuthInput,
    session_store: &mut SessionStore,
    catalog: Option<&ModelCatalog>,
) -> anyhow::Result<ProviderAuthMutationOutcome> {
    authorize_provider_with_input_as_current_with_saver(
        registry,
        id,
        input,
        session_store,
        catalog,
        PersistentSessionStoreSaver,
    )
    .await
}

#[async_trait]
trait SessionStoreSaver {
    async fn save(
        &mut self,
        session_store: &SessionStore,
        policy: SaveAuthPolicy,
    ) -> anyhow::Result<()>;
}

struct PersistentSessionStoreSaver;

#[async_trait]
impl SessionStoreSaver for PersistentSessionStoreSaver {
    async fn save(
        &mut self,
        session_store: &SessionStore,
        policy: SaveAuthPolicy,
    ) -> anyhow::Result<()> {
        match policy {
            SaveAuthPolicy::PreserveExisting => session_store.save_async().await,
            SaveAuthPolicy::AllowClear => session_store.save_allowing_auth_clear_async().await,
        }
    }
}

#[cfg(test)]
struct CallbackSessionStoreSaver<F>(F);

#[cfg(test)]
#[async_trait]
impl<F> SessionStoreSaver for CallbackSessionStoreSaver<F>
where
    F: FnMut(&SessionStore, SaveAuthPolicy) -> anyhow::Result<()> + Send,
{
    async fn save(
        &mut self,
        session_store: &SessionStore,
        policy: SaveAuthPolicy,
    ) -> anyhow::Result<()> {
        (self.0)(session_store, policy)
    }
}

#[cfg(test)]
async fn authorize_provider_with_input_as_current_using_save<F>(
    registry: &ProviderRegistry,
    id: &str,
    input: AuthInput,
    session_store: &mut SessionStore,
    catalog: Option<&ModelCatalog>,
    save: F,
) -> anyhow::Result<ProviderAuthMutationOutcome>
where
    F: FnMut(&SessionStore, SaveAuthPolicy) -> anyhow::Result<()> + Send,
{
    authorize_provider_with_input_as_current_with_saver(
        registry,
        id,
        input,
        session_store,
        catalog,
        CallbackSessionStoreSaver(save),
    )
    .await
}

async fn authorize_provider_with_input_as_current_with_saver<S>(
    registry: &ProviderRegistry,
    id: &str,
    input: AuthInput,
    session_store: &mut SessionStore,
    catalog: Option<&ModelCatalog>,
    mut save: S,
) -> anyhow::Result<ProviderAuthMutationOutcome>
where
    S: SessionStoreSaver,
{
    let factory = registry
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("Unknown provider '{}'", id))?;
    let id = factory.metadata().id.as_ref();
    let live_cache_connection_id = catalog.map(|_| id.parse::<ConnectionId>()).transpose()?;
    let session_file = SessionStore::path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|err| format!("<unavailable: {err:#}>"));
    tracing::info!(
        provider = %id,
        session_file = %session_file,
        "authorizing provider"
    );
    let endpoint_context_window_override = match &input {
        AuthInput::OpenAiCompatible { context_window, .. } => Some(*context_window),
        _ => None,
    };
    let requested_credential_source = match &input {
        AuthInput::FromEnv => factory
            .metadata()
            .env_var_api_key
            .as_deref()
            .map(|variable| crate::session::CredentialSource::Environment(variable.to_string()))
            .unwrap_or_default(),
        AuthInput::FromCodexCache => crate::session::CredentialSource::CodexCache,
        AuthInput::ApiKey { persistence, .. }
        | AuthInput::OpenAiCompatible {
            credential_persistence: persistence,
            ..
        } => persistence.source(),
    };
    let outcome = factory.authorize(input).await?;
    let save_auth_policy = if outcome.clear_existing_api_key {
        SaveAuthPolicy::AllowClear
    } else {
        SaveAuthPolicy::PreserveExisting
    };

    // Mutate a private snapshot first. Callers keep using the prior live
    // session until the authoritative save succeeds, then rebuild their Agent
    // from the committed snapshot returned here.
    let mut staged_session_store = session_store.clone();
    staged_session_store.ensure_provider(id);
    staged_session_store
        .set_provider_credential(id, outcome.api_key, requested_credential_source)
        .await;
    let session = staged_session_store.session_mut(id);
    if let Some(base_url) = outcome.base_url {
        session.base_url = base_url;
    }
    if let Some(model) = outcome.model {
        session.model = model;
    }
    if let Some(context_window) = endpoint_context_window_override {
        session.context_window = context_window;
    }
    session.account_id = outcome.account_id;
    session.is_fedramp_account = outcome.is_fedramp;
    if session.model.trim().is_empty() {
        session.model = factory.metadata().default_model.to_string();
    }
    session.authorized_at = Some(std::time::SystemTime::now());
    staged_session_store.set_current_kind_id(id);
    save.save(&staged_session_store, save_auth_policy).await?;
    *session_store = staged_session_store;
    let session = session_store.session(id);
    tracing::info!(
        provider = %id,
        session_file = %session_file,
        has_api_key = !session.api_key.trim().is_empty(),
        has_account_id = !session.account_id.trim().is_empty(),
        current_model = %session.model,
        "saved provider authorization before model catalog refresh"
    );

    let mut mutation =
        ProviderAuthMutationOutcome::committed(ProviderAuthorizationState::Authorized);
    if let (Some(catalog), Some(connection_id)) = (catalog, live_cache_connection_id) {
        let model_session = session_store.session(id).clone();
        match factory.list_available_models(&model_session).await {
            Ok(availability) if !availability.models.is_empty() => {
                let availability = availability
                    .with_fallback_context_window(session_store.session(id).context_window);
                if let Err(err) = catalog.write_live_availability(&connection_id, availability) {
                    tracing::warn!(
                        provider = %id,
                        error = %err,
                        "failed to persist provider model availability after authorization; using catalog fallback"
                    );
                    mutation
                        .warnings
                        .push(ProviderAuthMutationWarning::LiveModelCache {
                            operation: "refreshed",
                            detail: format!("{err:#}"),
                        });
                }
            }
            outcome => {
                if let Err(err) = &outcome {
                    tracing::warn!(
                        provider = %id,
                        error = %err,
                        "failed to refresh provider model availability after authorization; using catalog fallback"
                    );
                } else {
                    tracing::warn!(
                        provider = %id,
                        "provider model availability refresh after authorization returned no models; using catalog fallback"
                    );
                }
            }
        }
    }

    let model = session_store.session(id).model.clone();
    tracing::info!(
        provider = %id,
        model = %model,
        session_file = %session_file,
        provider_state = %session_store.provider_state_summary(),
        "authorized provider"
    );
    Ok(mutation)
}

pub(crate) async fn unauthorize_provider(
    registry: &ProviderRegistry,
    id: &str,
    session_store: &mut SessionStore,
    catalog: Option<&ModelCatalog>,
) -> anyhow::Result<ProviderAuthMutationOutcome> {
    let factory = registry
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("Unknown provider '{}'", id))?;
    let id = factory.metadata().id.as_ref();
    let live_cache_connection_id = catalog.map(|_| id.parse::<ConnectionId>()).transpose()?;

    // Commit the conservative state before touching replaceable caches or
    // external credential stores. A save failure therefore leaves the live
    // session and its credential explicitly unchanged; every later failure
    // leaves both SQLite and the runtime session unauthorized.
    let mut staged_session_store = session_store.clone();
    factory.clear_authorization(staged_session_store.session_mut(id));
    let staged_session = staged_session_store.session_mut(id);
    staged_session.api_key.clear();
    staged_session.credential_source = CredentialSource::None;
    staged_session.authorized_at = None;
    staged_session_store
        .save_allowing_auth_clear_async()
        .await?;
    *session_store = staged_session_store;

    let mut mutation =
        ProviderAuthMutationOutcome::committed(ProviderAuthorizationState::Unauthorized);
    if let Err(err) = session_store.clear_provider_credential(id).await {
        tracing::warn!(provider = %id, error = %err, "provider auth cleared with incomplete credential cleanup");
        mutation
            .warnings
            .push(ProviderAuthMutationWarning::CredentialCleanup {
                detail: format!("{err:#}"),
            });
    }
    if let (Some(catalog), Some(connection_id)) = (catalog, live_cache_connection_id)
        && let Err(err) = catalog.clear_live_availability(&connection_id)
    {
        tracing::warn!(provider = %id, error = %err, "provider auth cleared with stale live-model cache");
        mutation
            .warnings
            .push(ProviderAuthMutationWarning::LiveModelCache {
                operation: "cleared",
                detail: format!("{err:#}"),
            });
    }
    tracing::info!(provider = %id, "unauthorized provider");
    Ok(mutation)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, LazyLock};

    use async_trait::async_trait;

    use super::{
        ProviderAuthMutationWarning, ProviderAuthorizationState,
        authorize_provider_with_input_as_current_using_save, unauthorize_provider,
    };
    use crate::provider::{
        AuthInput, AuthorizeOutcome, NO_PARAMETERS, NO_REASONING, Protocol, ProviderCapabilities,
        ProviderFactory, ProviderMetadata, ProviderRegistry,
    };
    use crate::session::{SaveAuthPolicy, SessionStore};

    static TEST_PROVIDER_METADATA: LazyLock<ProviderMetadata> = LazyLock::new(|| {
        ProviderMetadata::new(
            "test-provider",
            "Test Provider",
            "test-model",
            "https://example.test/v1",
            None,
            None,
            None,
            &["test-model"],
            Protocol::OpenAiChat,
            ProviderCapabilities::new(NO_REASONING, NO_PARAMETERS),
            "chat/completions",
        )
    });

    fn catalog_with_test_provider(home: &std::path::Path) -> crate::model_catalog::ModelCatalog {
        let provider_dir = home.join("providers");
        std::fs::create_dir_all(&provider_dir).unwrap();
        std::fs::write(
            provider_dir.join("test-provider.toml"),
            r#"
                [[connections]]
                id = "test-provider"
                display_name = "Test Provider"
                auth = "api-key"
                transport = "openai-chat"
                default_base_url = "https://example.test/v1"
                default_model = "test-provider/test-model"
                default_endpoint_path = "chat/completions"
            "#,
        )
        .unwrap();
        crate::model_catalog::load_catalog_from_home(home).unwrap()
    }

    #[derive(Debug, Clone, Copy)]
    enum TestCatalogBehavior {
        Fails,
        Available,
    }

    struct TestProviderFactory(TestCatalogBehavior);

    #[async_trait]
    impl ProviderFactory for TestProviderFactory {
        fn metadata(&self) -> &ProviderMetadata {
            &TEST_PROVIDER_METADATA
        }

        async fn authorize(&self, input: AuthInput) -> anyhow::Result<AuthorizeOutcome> {
            match input {
                AuthInput::ApiKey { api_key, .. } => Ok(AuthorizeOutcome::new(api_key)),
                AuthInput::FromEnv
                | AuthInput::FromCodexCache
                | AuthInput::OpenAiCompatible { .. } => {
                    anyhow::bail!("test provider only accepts pasted keys")
                }
            }
        }

        fn is_authorized(&self, session: &crate::session::ProviderSession) -> bool {
            !session.api_key.trim().is_empty()
        }

        fn clear_authorization(&self, session: &mut crate::session::ProviderSession) {
            session.api_key.clear();
        }

        async fn list_models(
            &self,
            _session: &crate::session::ProviderSession,
        ) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }

        async fn list_available_models(
            &self,
            _session: &crate::session::ProviderSession,
        ) -> anyhow::Result<crate::model_catalog::LiveModelAvailability> {
            match self.0 {
                TestCatalogBehavior::Fails => anyhow::bail!("catalog unavailable"),
                TestCatalogBehavior::Available => Ok(
                    crate::model_catalog::LiveModelAvailability::from_remote_ids([
                        "test-model".to_string()
                    ]),
                ),
            }
        }
    }

    #[tokio::test]
    async fn authorize_saves_credentials_before_model_catalog_refresh() {
        let registry = ProviderRegistry::new(vec![Arc::new(TestProviderFactory(
            TestCatalogBehavior::Fails,
        ))]);
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let mut store = SessionStore::default();
        let mut saves = Vec::new();

        authorize_provider_with_input_as_current_using_save(
            &registry,
            "test-provider",
            AuthInput::ApiKey {
                api_key: "sk-test".to_string(),
                persistence: crate::session::CredentialPersistence::File,
            },
            &mut store,
            Some(&catalog),
            |store, policy| {
                saves.push((policy, store.clone()));
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(saves.len(), 1);
        assert_eq!(saves[0].0, SaveAuthPolicy::PreserveExisting);
        assert_eq!(saves[0].1.current_kind_id(), "test-provider");
        assert_eq!(saves[0].1.session("test-provider").api_key, "sk-test");
        assert_eq!(store.current_kind_id(), "test-provider");
    }

    #[tokio::test]
    async fn authorization_cache_failure_is_committed_and_restart_stable() {
        let home = tempfile::TempDir::new().unwrap();
        let storage = crate::storage::Storage::open_at(home.path().join("bonsai.db"))
            .await
            .unwrap();
        let catalog = catalog_with_test_provider(home.path());
        let cache_path = home.path().join("cache/live-models/test-provider.json");
        std::fs::create_dir_all(&cache_path).unwrap();
        let registry = ProviderRegistry::new(vec![Arc::new(TestProviderFactory(
            TestCatalogBehavior::Available,
        ))]);
        let mut store = SessionStore::load_with_storage(&storage).await.unwrap();

        let mutation = super::authorize_provider_with_input_as_current(
            &registry,
            "test-provider",
            AuthInput::ApiKey {
                api_key: "sk-cache-write".to_string(),
                persistence: crate::session::CredentialPersistence::File,
            },
            &mut store,
            Some(&catalog),
        )
        .await
        .unwrap();

        assert_eq!(
            mutation.committed_state,
            ProviderAuthorizationState::Authorized
        );
        assert!(matches!(
            mutation.warnings.as_slice(),
            [ProviderAuthMutationWarning::LiveModelCache {
                operation: "refreshed",
                ..
            }]
        ));
        assert!(
            registry
                .get("test-provider")
                .unwrap()
                .is_authorized(store.session("test-provider"))
        );
        assert_eq!(store.current_kind_id(), "test-provider");

        let reloaded = SessionStore::load_with_storage_and_catalog(&storage, Some(&catalog))
            .await
            .unwrap();
        assert_eq!(reloaded.current_kind_id(), "test-provider");
        assert_eq!(reloaded.session("test-provider").api_key, "sk-cache-write");
        assert!(
            registry
                .get("test-provider")
                .unwrap()
                .is_authorized(reloaded.session("test-provider"))
        );
    }

    #[tokio::test]
    async fn unauthorization_cache_failure_stays_conservatively_committed() {
        let home = tempfile::TempDir::new().unwrap();
        let storage = crate::storage::Storage::open_at(home.path().join("bonsai.db"))
            .await
            .unwrap();
        let catalog = catalog_with_test_provider(home.path());
        let registry = ProviderRegistry::new(vec![Arc::new(TestProviderFactory(
            TestCatalogBehavior::Available,
        ))]);
        let mut store = SessionStore::load_with_storage(&storage).await.unwrap();
        super::authorize_provider_with_input_as_current(
            &registry,
            "test-provider",
            AuthInput::ApiKey {
                api_key: "sk-cache-clear".to_string(),
                persistence: crate::session::CredentialPersistence::File,
            },
            &mut store,
            None,
        )
        .await
        .unwrap();
        let cache_path = home.path().join("cache/live-models/test-provider.json");
        std::fs::create_dir_all(&cache_path).unwrap();

        let mutation = unauthorize_provider(&registry, "test-provider", &mut store, Some(&catalog))
            .await
            .unwrap();

        assert_eq!(
            mutation.committed_state,
            ProviderAuthorizationState::Unauthorized
        );
        assert!(matches!(
            mutation.warnings.as_slice(),
            [ProviderAuthMutationWarning::LiveModelCache {
                operation: "cleared",
                ..
            }]
        ));
        assert!(store.session("test-provider").api_key.is_empty());
        assert_eq!(
            store.session("test-provider").credential_source,
            crate::session::CredentialSource::None
        );
        assert_eq!(store.session("test-provider").authorized_at, None);
        assert!(
            !registry
                .get("test-provider")
                .unwrap()
                .is_authorized(store.session("test-provider"))
        );

        let reloaded = SessionStore::load_with_storage_and_catalog(&storage, Some(&catalog))
            .await
            .unwrap();
        assert!(reloaded.session("test-provider").api_key.is_empty());
        assert_eq!(
            reloaded.session("test-provider").credential_source,
            crate::session::CredentialSource::None
        );
        assert!(
            !registry
                .get("test-provider")
                .unwrap()
                .is_authorized(reloaded.session("test-provider"))
        );
    }

    #[tokio::test]
    async fn failed_authorization_save_does_not_replace_live_session_snapshot() {
        let registry = ProviderRegistry::new(vec![Arc::new(TestProviderFactory(
            TestCatalogBehavior::Available,
        ))]);
        let mut store = SessionStore::default();
        let original = store.clone();

        let result = authorize_provider_with_input_as_current_using_save(
            &registry,
            "test-provider",
            AuthInput::ApiKey {
                api_key: "sk-unsaved".to_string(),
                persistence: crate::session::CredentialPersistence::Session,
            },
            &mut store,
            None,
            |_store, _policy| anyhow::bail!("injected save failure"),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(store.current_kind_id(), original.current_kind_id());
        assert_eq!(store.providers.len(), original.providers.len());
        assert!(!store.providers.contains_key("test-provider"));
    }

    /// A registry whose `local-endpoint` id resolves through a catalog-backed
    /// `ApiKeyOptional` factory — the generic endpoint auth path that used to
    /// live on the deleted `OpenAiCompatibleFactory`.
    fn endpoint_registry() -> (tempfile::TempDir, ProviderRegistry) {
        let home = tempfile::TempDir::new().unwrap();
        crate::model_catalog::write_local_catalog_entry(
            home.path(),
            crate::model_catalog::LocalCatalogEntryInput {
                connection: crate::model_catalog::LocalCatalogConnectionInput {
                    id: "local-endpoint".parse().unwrap(),
                    display_name: "Local Endpoint".to_string(),
                    transport: crate::model_catalog::TransportProtocol::OpenAiChat,
                    base_url: "http://localhost:9999/v1".to_string(),
                    discovery: Default::default(),
                },
                targets: vec![crate::model_catalog::LocalCatalogTargetInput {
                    remote_model: "llama-local".to_string(),
                    display_name: None,
                    context_window: None,
                    output_limit: None,
                    tool_call: true,
                }],
            },
        )
        .unwrap();
        let catalog = crate::model_catalog::load_catalog_from_home(home.path()).unwrap();
        let registry = ProviderRegistry::from_catalog(&catalog);
        (home, registry)
    }

    #[tokio::test]
    async fn authorize_endpoint_provider_blank_key_allows_persisted_key_clear() {
        let (_home, registry) = endpoint_registry();
        let mut store = SessionStore::default();
        store.ensure_provider("local-endpoint");
        store.session_mut("local-endpoint").context_window = Some(65_536);
        let mut saves = Vec::new();

        authorize_provider_with_input_as_current_using_save(
            &registry,
            "local-endpoint",
            AuthInput::OpenAiCompatible {
                base_url: "localhost:11434/v1".to_string(),
                api_key: None,
                model: Some("llama-local".to_string()),
                context_window: None,
                credential_persistence: crate::session::CredentialPersistence::File,
            },
            &mut store,
            None,
            |store, policy| {
                saves.push((policy, store.clone()));
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(saves.len(), 1);
        assert!(
            saves
                .iter()
                .all(|(policy, _)| *policy == SaveAuthPolicy::AllowClear)
        );
        assert_eq!(
            saves[0].1.session("local-endpoint").base_url,
            "http://localhost:11434/v1"
        );
        assert!(saves[0].1.session("local-endpoint").api_key.is_empty());
        assert!(store.session("local-endpoint").api_key.is_empty());
        assert_eq!(store.session("local-endpoint").context_window, None);
    }

    #[tokio::test]
    async fn authorize_endpoint_provider_persists_context_window_fallback() {
        let (_home, registry) = endpoint_registry();
        let mut store = SessionStore::default();
        store.ensure_provider("local-endpoint");

        authorize_provider_with_input_as_current_using_save(
            &registry,
            "local-endpoint",
            AuthInput::OpenAiCompatible {
                base_url: "localhost:11434/v1".to_string(),
                api_key: None,
                model: Some("llama-local".to_string()),
                context_window: Some(32_768),
                credential_persistence: crate::session::CredentialPersistence::File,
            },
            &mut store,
            None,
            |_store, _policy| Ok(()),
        )
        .await
        .unwrap();

        assert_eq!(store.session("local-endpoint").context_window, Some(32_768));
    }
}
