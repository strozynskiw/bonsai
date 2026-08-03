use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::model_catalog::{ConnectionAuth, ConnectionSpec, LiveModelAvailability, ModelCatalog};
use crate::provider::anthropic::AnthropicFactory;
use crate::provider::codex::CodexFactory;
use crate::provider::minimax_coding_plan::MiniMaxCodingPlanFactory;
use crate::provider::opencode::OpenCodeFactory;
use crate::provider::{
    AuthInput, AuthRequirement, AuthorizeOutcome, NO_PARAMETERS, NO_REASONING, Protocol,
    ProviderCapabilities, ProviderFactory, ProviderMetadata,
};
use crate::session::ProviderSession;

pub struct ProviderRegistry {
    factories: Vec<Arc<dyn ProviderFactory>>,
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("ids", &self.ids())
            .finish()
    }
}

impl ProviderRegistry {
    pub fn new(factories: Vec<Arc<dyn ProviderFactory>>) -> Self {
        Self { factories }
    }

    pub fn default_registry() -> Self {
        let factories: Vec<Arc<dyn ProviderFactory>> = vec![
            Arc::new(OpenCodeFactory),
            Arc::new(CodexFactory),
            Arc::new(AnthropicFactory),
            Arc::new(MiniMaxCodingPlanFactory),
        ];
        Self::new(factories)
    }

    pub fn from_catalog(catalog: &ModelCatalog) -> Self {
        let builtin_registry = Self::default_registry();
        let connections = catalog.connections();
        // `get`/`lookup` resolve by first match, so a duplicate id would silently
        // shadow the later connection. The catalog is expected to dedupe ids
        // upstream; assert it in debug builds rather than fail silently.
        debug_assert!(
            {
                let mut ids: Vec<&str> = connections.iter().map(|c| c.id.as_str()).collect();
                ids.sort_unstable();
                let len = ids.len();
                ids.dedup();
                ids.len() == len
            },
            "catalog connections must have unique ids"
        );
        let factories = connections
            .into_iter()
            .map(|connection| {
                Arc::new(CatalogProviderFactory::new(
                    connection,
                    catalog.target_remote_models_for_connection(&connection.id),
                    builtin_registry.get(connection.id.as_str()),
                )) as Arc<dyn ProviderFactory>
            })
            .collect();
        Self::new(factories)
    }

    pub fn all(&self) -> &[Arc<dyn ProviderFactory>] {
        &self.factories
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn ProviderFactory>> {
        self.factories
            .iter()
            .find(|factory| factory.metadata().is_known_id(id))
            .cloned()
    }

    pub fn ids(&self) -> Vec<&str> {
        self.factories
            .iter()
            .map(|factory| factory.metadata().id.as_ref())
            .collect()
    }

    pub fn lookup(&self, id: &str) -> Option<&dyn ProviderFactory> {
        self.factories
            .iter()
            .find(|factory| factory.metadata().is_known_id(id))
            .map(|arc| arc.as_ref() as &dyn ProviderFactory)
    }
}

struct CatalogProviderFactory {
    metadata: ProviderMetadata,
    delegate: Option<Arc<dyn ProviderFactory>>,
}

impl CatalogProviderFactory {
    fn new(
        connection: &ConnectionSpec,
        seed_models: Vec<String>,
        delegate: Option<Arc<dyn ProviderFactory>>,
    ) -> Self {
        Self {
            metadata: build_provider_metadata(connection, seed_models),
            delegate,
        }
    }
}

#[async_trait]
impl ProviderFactory for CatalogProviderFactory {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    async fn authorize(&self, input: AuthInput) -> anyhow::Result<AuthorizeOutcome> {
        match self.metadata.auth_requirement {
            AuthRequirement::ApiKeyRequired => {
                crate::provider::auth::authorize_api_key(&self.metadata, input)
            }
            AuthRequirement::ApiKeyOptional => {
                authorize_optional_api_key(&self.metadata, input).await
            }
            AuthRequirement::CodexCache => CodexFactory.authorize(input).await,
        }
    }

    fn is_authorized(&self, session: &ProviderSession) -> bool {
        crate::provider::auth::is_authorized(&self.metadata, session)
    }

    fn clear_authorization(&self, session: &mut ProviderSession) {
        match self.metadata.auth_requirement {
            AuthRequirement::ApiKeyRequired => crate::provider::auth::clear_api_key(session),
            AuthRequirement::ApiKeyOptional => {
                session.api_key.clear();
                session.base_url.clear();
                session.model.clear();
                session.context_window = None;
            }
            AuthRequirement::CodexCache => CodexFactory.clear_authorization(session),
        }
    }

    async fn list_models(&self, session: &ProviderSession) -> anyhow::Result<Vec<String>> {
        Ok(self
            .list_available_models(session)
            .await?
            .remote_model_ids())
    }

    async fn list_available_models(
        &self,
        session: &ProviderSession,
    ) -> anyhow::Result<LiveModelAvailability> {
        if let Some(delegate) = &self.delegate
            && delegate.metadata().protocol == Protocol::CodexResponses
        {
            return delegate.list_available_models(session).await;
        }

        match self.metadata.protocol {
            Protocol::OpenAiChat | Protocol::AnthropicMessages => {
                crate::provider::endpoint::list_available_models_for_metadata(
                    &self.metadata,
                    session,
                )
                .await
            }
            Protocol::CodexResponses => CodexFactory.list_available_models(session).await,
        }
    }

    async fn probe_reachability(&self, session: &ProviderSession) -> anyhow::Result<()> {
        if let Some(delegate) = &self.delegate
            && delegate.metadata().protocol == Protocol::CodexResponses
        {
            return delegate.probe_reachability(session).await;
        }

        match self.metadata.protocol {
            Protocol::OpenAiChat | Protocol::AnthropicMessages => {
                crate::provider::endpoint::probe_reachability_for_metadata(&self.metadata, session)
                    .await
            }
            Protocol::CodexResponses => CodexFactory.probe_reachability(session).await,
        }
    }
}

async fn authorize_optional_api_key(
    metadata: &ProviderMetadata,
    input: AuthInput,
) -> anyhow::Result<AuthorizeOutcome> {
    match input {
        AuthInput::OpenAiCompatible {
            base_url,
            api_key,
            model,
            context_window: _,
            credential_persistence: _,
        } => {
            crate::provider::endpoint::authorize_endpoint(
                metadata,
                &base_url,
                api_key.as_deref(),
                model.as_deref(),
            )
            .await
        }
        AuthInput::FromEnv => {
            let base_url = match metadata.env_var_base_url.as_deref() {
                Some(var) => std::env::var(var)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or_else(|| metadata.default_base_url.to_string()),
                None => metadata.default_base_url.to_string(),
            };
            if base_url.trim().is_empty() {
                anyhow::bail!(
                    "{} requires a base URL. Use /authorize {}.",
                    metadata.display_name,
                    metadata.id
                );
            }
            let api_key = metadata
                .env_var_api_key
                .as_deref()
                .and_then(|var| std::env::var(var).ok())
                .filter(|value| !value.trim().is_empty());
            let model = metadata
                .env_var_model
                .as_deref()
                .and_then(|var| std::env::var(var).ok())
                .filter(|value| !value.trim().is_empty())
                .or_else(|| {
                    (!metadata.default_model.trim().is_empty())
                        .then(|| metadata.default_model.to_string())
                });
            crate::provider::endpoint::authorize_endpoint(
                metadata,
                &base_url,
                api_key.as_deref(),
                model.as_deref(),
            )
            .await
        }
        AuthInput::ApiKey { .. } => {
            anyhow::bail!(
                "{} requires a base URL. Use /authorize {}.",
                metadata.display_name,
                metadata.id
            )
        }
        AuthInput::FromCodexCache => {
            anyhow::bail!(
                "{} does not support codex auth cache",
                metadata.display_name
            )
        }
    }
}

fn build_provider_metadata(
    connection: &ConnectionSpec,
    seed_models: Vec<String>,
) -> ProviderMetadata {
    // The registry is the runtime source of truth: every connection's provider is
    // minted from this metadata, not from the builtin `*_METADATA` constants (those
    // only back `default_registry`). A wire capability declared on a builtin but not
    // mapped here is inert in production — keep this in sync with `ConnectionSpec`.
    let mut capabilities = ProviderCapabilities::new(NO_REASONING, NO_PARAMETERS);
    if connection.prompt_cache {
        capabilities = capabilities.with_prompt_cache();
    }
    if connection.reasoning_content_echo {
        capabilities = capabilities.with_reasoning_content_echo();
    }
    if connection.usage_frame_ends_stream {
        capabilities = capabilities.with_usage_frame_stream_terminal();
    }
    let default_model = connection
        .default_model
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let endpoint_path = connection
        .default_endpoint_path
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    let seed_model_refs = seed_models.iter().map(String::as_str).collect::<Vec<_>>();
    let mut metadata = ProviderMetadata::new(
        connection.id.as_str(),
        &connection.display_name,
        &default_model,
        &connection.default_base_url,
        connection.api_key_env.as_deref(),
        connection.model_env.as_deref(),
        connection.base_url_env.as_deref(),
        &seed_model_refs,
        Protocol::from(connection.transport),
        capabilities,
        &endpoint_path,
    )
    .with_auth_requirement(auth_requirement_for_connection_auth(&connection.auth))
    .with_discovery(connection.discovery)
    .with_auth_header(connection.auth_header.clone());
    metadata.model_exclude_prefixes = connection.model_exclude_prefixes.clone().into_boxed_slice();
    if let Some(token_counter) = connection.default_token_counter {
        metadata = metadata.with_token_counter(token_counter);
    }
    metadata
}

fn auth_requirement_for_connection_auth(auth: &ConnectionAuth) -> AuthRequirement {
    match auth {
        ConnectionAuth::ApiKey => AuthRequirement::ApiKeyRequired,
        ConnectionAuth::OptionalApiKey => AuthRequirement::ApiKeyOptional,
        ConnectionAuth::CodexCache => AuthRequirement::CodexCache,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_contains_all_builtins() {
        let registry = ProviderRegistry::default_registry();
        let ids: Vec<&str> = registry.ids();

        assert!(ids.contains(&"opencode"));
        assert!(ids.contains(&"codex"));
        assert!(ids.contains(&"anthropic"));
        assert!(ids.contains(&"minimax-coding-plan"));
        // Generic endpoint providers are catalog entries, not handwritten
        // provider factories, so they must not surface in the Rust registry.
        assert!(!ids.contains(&"openai-compatible"));
        assert!(!ids.contains(&"anthropic-compatible"));
    }

    #[test]
    fn compatible_connections_refresh_without_unsafe_capability_defaults() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("builtin catalog should load");
        let registry = ProviderRegistry::from_catalog(&catalog);

        for (id, protocol) in [
            ("openai-compatible", Protocol::OpenAiChat),
            ("anthropic-compatible", Protocol::AnthropicMessages),
        ] {
            let factory = registry.get(id).expect("compatible provider");
            let metadata = factory.metadata();
            assert_eq!(metadata.protocol, protocol, "{id}");
            assert_eq!(
                metadata.discovery,
                crate::model_catalog::DiscoveryKind::Auto,
                "{id}"
            );
            assert_eq!(
                metadata.token_counter,
                Some(crate::provider::TokenCounterKind::Heuristic),
                "{id}"
            );
            assert!(!metadata.capabilities.supports_prompt_cache, "{id}");
            assert!(metadata.seed_model_list().is_empty(), "{id}");
        }
    }

    /// Providers resolve through catalog connections (`from_catalog`), whose
    /// capabilities come solely from `models/builtin/connections.toml` — the
    /// builtin Rust metadata is only an auth/model-listing delegate. This
    /// pins that no builtin connection silently drops a prompt-cache
    /// capability its legacy metadata declares: exactly that drift shipped
    /// Anthropic requests without `cache_control` breakpoints (full input
    /// price every turn) while `anthropic.rs` still claimed the capability.
    #[test]
    fn catalog_connections_preserve_builtin_prompt_cache_capability() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("builtin catalog should load");
        let builtin = ProviderRegistry::default_registry();
        let registry = ProviderRegistry::from_catalog(&catalog);

        for id in builtin.ids() {
            let declares_cache = builtin
                .get(id)
                .expect("builtin factory")
                .metadata()
                .capabilities
                .supports_prompt_cache;
            if !declares_cache {
                continue;
            }
            let Some(catalog_factory) = registry.get(id) else {
                continue;
            };
            assert!(
                catalog_factory
                    .metadata()
                    .capabilities
                    .supports_prompt_cache,
                "connection `{id}` drops the prompt-cache capability its builtin \
                 metadata declares; set `prompt_cache = true` for it in \
                 models/builtin/connections.toml"
            );
        }
    }

    #[test]
    fn builtin_catalog_exposes_openrouter_connection_metadata() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("builtin catalog should load");
        let registry = ProviderRegistry::from_catalog(&catalog);
        let factory = registry.get("openrouter").expect("openrouter provider");
        let metadata = factory.metadata();

        assert_eq!(metadata.id.as_ref(), "openrouter");
        assert_eq!(metadata.display_name.as_ref(), "OpenRouter");
        assert_eq!(metadata.auth_requirement, AuthRequirement::ApiKeyRequired);
        assert_eq!(metadata.protocol, Protocol::OpenAiChat);
        assert_eq!(
            metadata.default_base_url.as_ref(),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(metadata.default_model.as_ref(), "openrouter/gpt-5.6-sol");
        assert_eq!(metadata.endpoint_path.as_ref(), "chat/completions");
        assert_eq!(
            metadata.env_var_api_key.as_deref(),
            Some("OPENROUTER_API_KEY")
        );
        assert_eq!(metadata.env_var_model.as_deref(), Some("OPENROUTER_MODEL"));
        assert_eq!(
            metadata.env_var_base_url.as_deref(),
            Some("OPENROUTER_BASE_URL")
        );
        assert_eq!(
            metadata.token_counter,
            Some(crate::provider::TokenCounterKind::Heuristic)
        );
        assert_eq!(
            metadata.seed_model_list(),
            vec![
                "openai/gpt-5.6-sol",
                "anthropic/claude-sonnet-5",
                "openai/gpt-5.2",
                "anthropic/claude-haiku-4.5"
            ]
        );
        assert!(metadata.capabilities.supports_prompt_cache);
        assert_eq!(
            metadata.discovery,
            crate::model_catalog::DiscoveryKind::OpenRouter
        );
    }

    #[test]
    fn builtin_catalog_exposes_openai_api_connection_metadata() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("builtin catalog should load");
        let registry = ProviderRegistry::from_catalog(&catalog);
        let factory = registry.get("openai").expect("OpenAI API provider");
        let metadata = factory.metadata();

        assert_eq!(metadata.id.as_ref(), "openai");
        assert_eq!(metadata.display_name.as_ref(), "OpenAI API");
        assert_eq!(metadata.auth_requirement, AuthRequirement::ApiKeyRequired);
        assert_eq!(metadata.protocol, Protocol::OpenAiChat);
        assert_eq!(
            metadata.default_base_url.as_ref(),
            "https://api.openai.com/v1"
        );
        assert_eq!(metadata.default_model.as_ref(), "openai/gpt-5.6-sol");
        assert_eq!(metadata.endpoint_path.as_ref(), "chat/completions");
        assert_eq!(metadata.env_var_api_key.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(metadata.env_var_model.as_deref(), Some("OPENAI_MODEL"));
        assert_eq!(
            metadata.env_var_base_url.as_deref(),
            Some("OPENAI_BASE_URL")
        );
        assert_eq!(
            metadata.token_counter,
            Some(crate::provider::TokenCounterKind::Tiktoken)
        );
        let seed_models = metadata.seed_model_list();
        for model in [
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.4-nano",
        ] {
            assert!(
                seed_models.iter().any(|seed| seed == model),
                "OpenAI offline catalog must include {model}"
            );
        }
        assert!(metadata.capabilities.supports_prompt_cache);
    }

    #[test]
    fn builtin_catalog_exposes_deepseek_connection_contract() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("builtin catalog should load");
        let registry = ProviderRegistry::from_catalog(&catalog);
        let factory = registry.get("deepseek").expect("DeepSeek API provider");
        let metadata = factory.metadata();

        assert_eq!(metadata.id.as_ref(), "deepseek");
        assert_eq!(metadata.display_name.as_ref(), "DeepSeek API");
        assert_eq!(metadata.auth_requirement, AuthRequirement::ApiKeyRequired);
        assert_eq!(metadata.protocol, Protocol::OpenAiChat);
        assert_eq!(
            metadata.default_base_url.as_ref(),
            "https://api.deepseek.com"
        );
        assert_eq!(
            metadata.default_model.as_ref(),
            "deepseek/deepseek-v4-flash"
        );
        assert_eq!(metadata.endpoint_path.as_ref(), "chat/completions");
        assert_eq!(
            metadata.env_var_api_key.as_deref(),
            Some("DEEPSEEK_API_KEY")
        );
        assert_eq!(metadata.env_var_model.as_deref(), Some("DEEPSEEK_MODEL"));
        assert_eq!(
            metadata.env_var_base_url.as_deref(),
            Some("DEEPSEEK_BASE_URL")
        );
        assert_eq!(
            metadata.seed_model_list(),
            vec!["deepseek-v4-flash", "deepseek-v4-pro"]
        );
        assert!(metadata.capabilities.supports_prompt_cache);
        assert!(metadata.capabilities.echoes_reasoning_content);
        assert!(!metadata.capabilities.supports_vision);
    }

    #[tokio::test]
    async fn deepseek_refresh_uses_authenticated_models_listing() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer sk-deepseek-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    {
                        "id": "deepseek-v4-flash",
                        "object": "model",
                        "owned_by": "deepseek"
                    },
                    {
                        "id": "deepseek-v4-pro",
                        "object": "model",
                        "owned_by": "deepseek"
                    },
                    {
                        "id": "deepseek-v4-turbo",
                        "object": "model",
                        "owned_by": "deepseek"
                    },
                    {
                        "id": "deepseek-embedding-v1",
                        "object": "model",
                        "owned_by": "deepseek"
                    }
                ]
            })))
            .mount(&server)
            .await;

        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("builtin catalog should load");
        let registry = ProviderRegistry::from_catalog(&catalog);
        let factory = registry.get("deepseek").expect("DeepSeek API provider");
        let session = ProviderSession::new(
            "sk-deepseek-test".to_string(),
            server.uri(),
            "deepseek-v4-flash".to_string(),
        );

        let availability = factory.list_available_models(&session).await.unwrap();

        assert_eq!(
            availability.remote_model_ids(),
            vec![
                "deepseek-v4-flash".to_string(),
                "deepseek-v4-pro".to_string(),
                "deepseek-v4-turbo".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn tencent_refresh_uses_catalog_discovery_and_filters_tokenhub_products() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer sk-hunyuan-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": "hy3", "name": "Hy3", "status": "online"},
                    {"id": "hy4", "name": "Hy4", "status": "online"},
                    {"id": "hy3-preview", "name": "Hy3 Preview", "status": "pre-offline"},
                    {"id": "deepseek-v4", "name": "DeepSeek V4", "status": "online"},
                    {"id": "hunyuan-video", "status": "online"}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("builtin catalog should load");
        let registry = ProviderRegistry::from_catalog(&catalog);
        let factory = registry.get("tencent").expect("Tencent Hunyuan provider");
        assert_eq!(
            factory.metadata().discovery,
            crate::model_catalog::DiscoveryKind::Tencent
        );
        assert!(factory.metadata().capabilities.supports_prompt_cache);
        assert!(factory.metadata().capabilities.echoes_reasoning_content);
        let session = ProviderSession::new(
            "sk-hunyuan-test".to_string(),
            format!("{}/v1", server.uri()),
            "hy3".to_string(),
        );

        let availability = factory.list_available_models(&session).await.unwrap();

        assert_eq!(
            availability.remote_model_ids(),
            vec!["hy3".to_string(), "hy4".to_string()]
        );
        assert_eq!(availability.models[0].output_limit, Some(128_000));
        assert!(availability.models[1].features.is_empty());
    }

    #[tokio::test]
    async fn xai_refresh_discovers_new_language_models_without_excluded_variants() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer sk-xai-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    {"id": "grok-build-0.1", "object": "model"},
                    {"id": "grok-build-0.2", "object": "model"},
                    {"id": "grok-4.20-0309-reasoning", "object": "model"},
                    {"id": "grok-4.20-multi-agent-0309", "object": "model"},
                    {"id": "grok-imagine-image", "object": "model"}
                ]
            })))
            .mount(&server)
            .await;

        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("builtin catalog should load");
        let registry = ProviderRegistry::from_catalog(&catalog);
        let factory = registry.get("xai").expect("xAI provider");
        let session = ProviderSession::new(
            "sk-xai-test".to_string(),
            server.uri(),
            "grok-build-0.1".to_string(),
        );

        let availability = factory.list_available_models(&session).await.unwrap();

        assert_eq!(
            availability.remote_model_ids(),
            vec!["grok-build-0.1".to_string(), "grok-build-0.2".to_string(),],
            "new language models remain discoverable while configured incompatible prefixes stay hidden"
        );
    }

    #[test]
    fn builtin_catalog_exposes_separate_vendor_api_and_plan_factories() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("builtin catalog should load");
        let registry = ProviderRegistry::from_catalog(&catalog);
        let cases = [
            (
                "minimax",
                Protocol::AnthropicMessages,
                "MINIMAX_API_KEY",
                "minimax/MiniMax-M3",
            ),
            (
                "minimax-coding-plan",
                Protocol::AnthropicMessages,
                "MINIMAX_CODING_PLAN_API_KEY",
                "minimax-coding-plan/MiniMax-M3",
            ),
            (
                "mimo",
                Protocol::OpenAiChat,
                "MIMO_API_KEY",
                "mimo/mimo-v2.5-pro",
            ),
            // mimo-coding-plan is asserted separately below — its account-
            // dedicated base URL makes it an endpoint-setup (ApiKeyOptional)
            // connection, not ApiKeyRequired.
            ("zai", Protocol::OpenAiChat, "ZAI_API_KEY", "zai/glm-5.2"),
            (
                "zai-coding-plan",
                Protocol::OpenAiChat,
                "ZAI_CODING_PLAN_API_KEY",
                "zai-coding-plan/glm-5.2",
            ),
            (
                "moonshotai",
                Protocol::OpenAiChat,
                "MOONSHOT_API_KEY",
                "moonshotai/kimi-k2.7-code",
            ),
            (
                "kimi-coding-plan",
                Protocol::OpenAiChat,
                "KIMI_CODING_PLAN_API_KEY",
                "kimi-coding-plan/kimi-for-coding",
            ),
        ];

        for (id, protocol, api_key_env, default_model) in cases {
            let factory = registry
                .get(id)
                .unwrap_or_else(|| panic!("missing catalog provider {id}"));
            let metadata = factory.metadata();
            assert_eq!(metadata.auth_requirement, AuthRequirement::ApiKeyRequired);
            assert_eq!(metadata.protocol, protocol);
            assert_eq!(metadata.env_var_api_key.as_deref(), Some(api_key_env));
            assert_eq!(metadata.default_model.as_ref(), default_model);
        }

        // The MiMo coding plan hands each account a dedicated, region-specific
        // base URL, so it is an endpoint-setup connection (user supplies the URL)
        // rather than a fixed-host ApiKeyRequired one.
        let plan = registry
            .get("mimo-coding-plan")
            .expect("missing mimo-coding-plan");
        assert_eq!(
            plan.metadata().auth_requirement,
            AuthRequirement::ApiKeyOptional
        );
        assert!(plan.metadata().auth_requirement.uses_endpoint_setup());
        assert_eq!(plan.metadata().protocol, Protocol::OpenAiChat);
    }

    #[test]
    fn auth_header_override_controls_raw_key_header() {
        // No builtin connection currently overrides the header, but the general
        // mechanism (used by local connection overrides) must resolve to the
        // configured header when set and the x-api-key default otherwise.
        let base = ProviderMetadata::new(
            "t",
            "T",
            "m",
            "http://x",
            None,
            None,
            None,
            &[],
            Protocol::AnthropicMessages,
            ProviderCapabilities::new(NO_REASONING, NO_PARAMETERS),
            "v1/messages",
        );
        assert_eq!(base.raw_api_key_header_name(), "x-api-key");
        let overridden = base.with_auth_header(Some("api-key".into()));
        assert_eq!(overridden.raw_api_key_header_name(), "api-key");
    }

    #[test]
    fn get_is_case_insensitive() {
        let registry = ProviderRegistry::default_registry();
        assert!(registry.get("opencode").is_some());
        assert!(registry.get("OpenCode").is_some());
        assert!(registry.get("ANTHROPIC").is_some());
        assert!(registry.get("minimax-coding-plan").is_some());
    }

    #[test]
    fn get_returns_none_for_unknown_provider() {
        let registry = ProviderRegistry::default_registry();
        assert!(registry.get("not-a-provider").is_none());
        assert!(registry.get("").is_none());
    }

    #[test]
    fn catalog_registries_do_not_intern_metadata_globally() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("builtin catalog should load");
        let first = ProviderRegistry::from_catalog(&catalog);
        let second = ProviderRegistry::from_catalog(&catalog);
        let first_metadata = first.lookup("openai").expect("first factory").metadata();
        let second_metadata = second.lookup("openai").expect("second factory").metadata();

        assert_eq!(first_metadata.id, second_metadata.id);
        assert!(
            !std::ptr::eq(first_metadata, second_metadata),
            "each catalog factory must own metadata that drops with its registry"
        );
    }

    #[test]
    fn all_returns_factories_in_registration_order() {
        let registry = ProviderRegistry::default_registry();
        let all = registry.all();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].metadata().id.as_ref(), "opencode");
        assert_eq!(all[1].metadata().id.as_ref(), "codex");
        assert_eq!(all[3].metadata().id.as_ref(), "minimax-coding-plan");
    }

    #[test]
    fn lookup_returns_known_factory() {
        let registry = ProviderRegistry::default_registry();
        assert!(registry.lookup("opencode").is_some());
        assert!(registry.lookup("minimax-coding-plan").is_some());
    }

    /// The generic-endpoint auth flow (base URL + optional key) now lives
    /// entirely on `CatalogProviderFactory`'s `ApiKeyOptional` arms — the
    /// coverage that used to sit on the deleted `OpenAiCompatibleFactory`.
    #[tokio::test]
    async fn catalog_factory_authorizes_and_clears_optional_api_key_endpoints() {
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
        let factory = registry.get("local-endpoint").expect("catalog factory");

        let outcome = factory
            .authorize(AuthInput::OpenAiCompatible {
                base_url: "localhost:11434/v1".to_string(),
                api_key: None,
                model: Some("llama-local".to_string()),
                context_window: None,
                credential_persistence: crate::session::CredentialPersistence::File,
            })
            .await
            .unwrap();
        assert_eq!(outcome.api_key, "");
        assert_eq!(
            outcome.base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
        assert_eq!(outcome.model.as_deref(), Some("llama-local"));

        let mut session = ProviderSession {
            api_key: "sk".to_string(),
            base_url: "http://localhost/v1".to_string(),
            model: "model".to_string(),
            context_window: Some(32_768),
            ..ProviderSession::default()
        };
        assert!(factory.is_authorized(&session));
        factory.clear_authorization(&mut session);
        assert!(session.api_key.is_empty());
        assert!(session.base_url.is_empty());
        assert!(session.model.is_empty());
        assert_eq!(session.context_window, None);
        assert!(!factory.is_authorized(&session));
    }

    #[test]
    fn all_metadata_matches_registry_factories() {
        // Built-in metadata (consumed by session normalization) must stay in sync
        // with the registry's factory list — same ids, same order.
        let registry = ProviderRegistry::default_registry();
        let factory_ids: Vec<&str> = registry.ids();
        let metadata_ids: Vec<&str> = crate::provider::all_metadata()
            .iter()
            .map(|meta| meta.id.as_ref())
            .collect();
        assert_eq!(factory_ids, metadata_ids);
    }
}
