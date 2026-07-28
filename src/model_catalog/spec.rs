use serde::de;
use serde::{Deserialize, Deserializer, Serialize};

use crate::model_role::LegacyModelRole;
use crate::provider::{
    ModelPricing, ModelPricingTier, ReasoningCodec, ReasoningEffort, ReasoningOption,
    ReasoningSelection, TokenCounterKind,
};

use super::*;

const fn default_enabled() -> bool {
    true
}

/// How a connection's live model list is discovered. `Generic` uses the
/// transport's standard listing endpoint (`/models`); the server-specific
/// kinds hit richer native APIs (context window, display name, capabilities)
/// that the OpenAI-compatible surface does not expose. `Static` skips discovery
/// entirely and offers exactly the connection's catalog models — for a backend
/// whose `/models` is unusable or returns off-topic entries (e.g. a provider
/// that lists TTS/ASR voice models alongside chat).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DiscoveryKind {
    #[default]
    Generic,
    Gemini,
    LmStudio,
    Mistral,
    OpenRouter,
    Ollama,
    Tencent,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TransportProtocol {
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    AnthropicMessages,
    CodexResponses,
}

/// Provider/model-specific prompt-cache behavior.
///
/// IMPORTANT CACHE COMPATIBILITY CONTRACT: the default preserves each
/// transport's established request body. Opt in only after live verification;
/// moving volatile context across the history boundary can either restore a
/// warm prefix or invalidate every existing provider cache entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PromptCachePolicy {
    /// Use the transport's established request without an override.
    #[default]
    TransportDefault,
    /// Preserve a growing reusable history with the transport's supported
    /// mechanism: explicit breakpoint before a mutable tail (Anthropic) or
    /// immutable in-place snapshots (automatic Chat Completions caching).
    RollingHistory,
    /// OpenRouter's documented top-level automatic caching for Anthropic
    /// models, plus an explicit sticky session identity.
    OpenRouterAnthropic,
    /// Tencent TokenHub uses both a lane-scoped `prompt_cache_key` request
    /// field and a conversation-stable `X-Session-ID` header. The two values
    /// have different scopes and are intentionally emitted together.
    TokenHub,
}

impl PromptCachePolicy {
    /// Whether mutable project state must be stored as immutable history.
    ///
    /// This is the shared agent-side half of every rolling policy. Wire
    /// transports still choose their own representation: automatic caches keep
    /// snapshots in place, while Anthropic puts the latest snapshot behind an
    /// explicit breakpoint.
    pub(crate) const fn uses_append_only_project_state(self) -> bool {
        matches!(
            self,
            Self::RollingHistory | Self::OpenRouterAnthropic | Self::TokenHub
        )
    }

    /// Whether to emit OpenRouter's Anthropic-only automatic cache controls.
    pub(crate) const fn uses_openrouter_anthropic_control(self) -> bool {
        matches!(self, Self::OpenRouterAnthropic)
    }

    /// Whether a Chat Completions cache header supplements, rather than
    /// replaces, the body `prompt_cache_key`.
    pub(crate) const fn emits_body_key_with_header(self) -> bool {
        matches!(self, Self::TokenHub)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ConnectionAuth {
    ApiKey,
    OptionalApiKey,
    CodexCache,
}

/// One predefined service origin for a catalog connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
pub(crate) struct ServiceOrigin {
    pub id: Box<str>,
    pub display_name: Box<str>,
    pub base_url: Box<str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RunTarget {
    pub connection_id: ConnectionId,
    pub model_id: ModelId,
    pub remote_model_id: Box<str>,
    pub base_url: Box<str>,
    pub transport: TransportProtocol,
    pub prompt_cache_header: Option<Box<str>>,
    pub prompt_cache_policy: PromptCachePolicy,
    pub reasoning_codec: ReasoningCodec,
    pub endpoint_path: Option<Box<str>>,
    pub context_window: Option<u32>,
    pub output_limit: Option<u32>,
    pub reasoning: ReasoningSelection,
    /// One request-local effort increase that the resolved model explicitly
    /// supports. Used only after a failed verification repair.
    pub reasoning_escalation: Option<ReasoningSelection>,
    /// Whether the resolved model accepts image content parts. From the
    /// catalog `attachment` feature on the resolved path, or the provider's
    /// metadata capabilities on the fallback path; transports OR in the
    /// provider-level capability so known-vision providers keep working when
    /// the catalog has no row. Non-vision transports downgrade image parts to
    /// a text placeholder so an image already in history cannot 400 every
    /// later turn.
    pub supports_vision: bool,
    pub use_responses_lite: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ConnectionSpec {
    pub id: ConnectionId,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub display_name: Box<str>,
    pub auth: ConnectionAuth,
    pub transport: TransportProtocol,
    #[serde(default)]
    pub default_base_url: Box<str>,
    /// Named service endpoints that users can switch between while retaining
    /// the same provider identity, credentials, model, and shortcuts.
    #[serde(default)]
    pub origins: Vec<ServiceOrigin>,
    #[serde(default)]
    pub api_key_env: Option<Box<str>>,
    #[serde(default)]
    pub model_env: Option<Box<str>>,
    #[serde(default)]
    pub base_url_env: Option<Box<str>>,
    #[serde(default, deserialize_with = "deserialize_optional_model_id")]
    pub default_model: Option<ModelId>,
    #[serde(default)]
    pub default_endpoint_path: Option<Box<str>>,
    #[serde(default)]
    pub default_token_counter: Option<TokenCounterKind>,
    /// Models.dev provider namespace used for dynamically discovered models
    /// whose remote ids are bare model names. Defaults to the connection id.
    ///
    /// Gateways such as OpenCode Go use a different models.dev namespace
    /// (`opencode-go`) than their stable Bonsai connection id (`opencode`).
    /// Keeping the mapping on the connection lets `/refresh` enrich a newly
    /// served model with current pricing, limits, and capabilities before a
    /// hand-written target exists.
    #[serde(default)]
    pub models_dev_provider: Option<ConnectionId>,
    /// Remote model-id prefixes omitted from live discovery. This keeps
    /// non-chat products and known wire-incompatible variants out of agent
    /// pickers while allowing genuinely new model families through.
    #[serde(default)]
    pub model_exclude_prefixes: Vec<Box<str>>,
    #[serde(default)]
    pub reasoning_codec: Option<ReasoningCodec>,
    /// Whether this connection's backend supports prompt caching. On
    /// OpenAI-chat transports it gates the configured cache route (a
    /// `prompt_cache_key` body field by default, or `prompt_cache_header` when
    /// set); on Anthropic transports it gates `cache_control` breakpoints.
    /// Builtin metadata providers carry this on their capabilities; catalog
    /// connections opt in here.
    #[serde(default)]
    pub prompt_cache: bool,
    /// Optional Chat Completions header carrying the stable conversation cache
    /// route. By default this replaces the `prompt_cache_key` request field;
    /// policies such as TokenHub can explicitly require both.
    #[serde(default)]
    pub prompt_cache_header: Option<Box<str>>,
    /// Prompt-cache policy inherited by every target unless that model declares
    /// an override.
    #[serde(default)]
    pub prompt_cache_policy: PromptCachePolicy,
    /// Whether this backend needs `reasoning_content` echoed back on assistant
    /// messages. Most compatible backends need it only with `tool_calls`;
    /// provider-specific codecs may require replay on plain turns too.
    /// Off-schema for Chat Completions, so it is only sent where verified.
    #[serde(default)]
    pub reasoning_content_echo: bool,
    /// Whether a terminal usage frame ends a stream that never sends
    /// `finish_reason`. Off by default: for a normal OpenAI-compatible backend a
    /// missing `finish_reason` means the stream was truncated, and accepting it
    /// would report a cut-off turn as complete.
    #[serde(default)]
    pub usage_frame_ends_stream: bool,
    /// HTTP header name carrying the API key, when the backend does not accept
    /// the transport default (`x-api-key` for Anthropic, `Authorization: Bearer`
    /// for OpenAI-chat). Xiaomi MiMo, for example, only reads a bare `api-key`
    /// header. `None` keeps the transport default. The value is sent verbatim
    /// with no scheme prefix.
    #[serde(default)]
    pub auth_header: Option<Box<str>>,
    #[serde(default)]
    pub discovery: DiscoveryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ConnectionSpecPatch {
    pub id: ConnectionId,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub display_name: Option<Box<str>>,
    #[serde(default)]
    pub auth: Option<ConnectionAuth>,
    #[serde(default)]
    pub transport: Option<TransportProtocol>,
    #[serde(default)]
    pub default_base_url: Option<Box<str>>,
    #[serde(default)]
    pub origins: Option<Vec<ServiceOrigin>>,
    #[serde(default)]
    pub api_key_env: Option<Box<str>>,
    #[serde(default)]
    pub model_env: Option<Box<str>>,
    #[serde(default)]
    pub base_url_env: Option<Box<str>>,
    #[serde(default, deserialize_with = "deserialize_optional_model_id_patch")]
    pub default_model: Option<Option<ModelId>>,
    #[serde(default)]
    pub default_endpoint_path: Option<Box<str>>,
    #[serde(default)]
    pub default_token_counter: Option<TokenCounterKind>,
    #[serde(default)]
    pub models_dev_provider: Option<ConnectionId>,
    #[serde(default)]
    pub model_exclude_prefixes: Option<Vec<Box<str>>>,
    #[serde(default)]
    pub reasoning_codec: Option<ReasoningCodec>,
    #[serde(default)]
    pub prompt_cache: Option<bool>,
    #[serde(default)]
    pub prompt_cache_header: Option<Box<str>>,
    #[serde(default)]
    pub prompt_cache_policy: Option<PromptCachePolicy>,
    #[serde(default)]
    pub reasoning_content_echo: Option<bool>,
    #[serde(default)]
    pub usage_frame_ends_stream: Option<bool>,
    #[serde(default)]
    pub auth_header: Option<Box<str>>,
    #[serde(default)]
    pub discovery: Option<DiscoveryKind>,
}

impl ConnectionSpecPatch {
    pub(crate) fn into_complete(self, source_name: &str) -> Result<ConnectionSpec, CatalogError> {
        let id = self.id;
        let display_name = self
            .display_name
            .ok_or_else(|| missing_connection_field(source_name, &id, "display_name"))?;
        let auth = self
            .auth
            .ok_or_else(|| missing_connection_field(source_name, &id, "auth"))?;
        let transport = self
            .transport
            .ok_or_else(|| missing_connection_field(source_name, &id, "transport"))?;
        Ok(ConnectionSpec {
            id,
            enabled: self.enabled.unwrap_or(true),
            display_name,
            auth,
            transport,
            default_base_url: self.default_base_url.unwrap_or_default(),
            origins: self.origins.unwrap_or_default(),
            api_key_env: self.api_key_env,
            model_env: self.model_env,
            base_url_env: self.base_url_env,
            default_model: self.default_model.unwrap_or(None),
            default_endpoint_path: self.default_endpoint_path,
            default_token_counter: self.default_token_counter,
            models_dev_provider: self.models_dev_provider,
            model_exclude_prefixes: self.model_exclude_prefixes.unwrap_or_default(),
            reasoning_codec: self.reasoning_codec,
            prompt_cache: self.prompt_cache.unwrap_or(false),
            prompt_cache_header: self.prompt_cache_header,
            prompt_cache_policy: self.prompt_cache_policy.unwrap_or_default(),
            reasoning_content_echo: self.reasoning_content_echo.unwrap_or(false),
            usage_frame_ends_stream: self.usage_frame_ends_stream.unwrap_or(false),
            auth_header: self.auth_header,
            discovery: self.discovery.unwrap_or_default(),
        })
    }
}

fn missing_connection_field(
    source_name: &str,
    id: &ConnectionId,
    field: &'static str,
) -> CatalogError {
    CatalogError::MissingConnectionField {
        source_name: source_name.to_string(),
        id: id.clone(),
        field,
    }
}

impl ConnectionSpec {
    pub(crate) fn apply_patch(&mut self, patch: ConnectionSpecPatch) {
        let origins_supplied = patch.origins.is_some();
        if let Some(enabled) = patch.enabled {
            self.enabled = enabled;
        }
        if let Some(display_name) = patch.display_name {
            self.display_name = display_name;
        }
        if let Some(auth) = patch.auth {
            self.auth = auth;
        }
        if let Some(transport) = patch.transport {
            self.transport = transport;
        }
        if let Some(default_base_url) = patch.default_base_url {
            self.default_base_url = default_base_url;
            if !origins_supplied
                && !self.origins.iter().any(|origin| {
                    origin.base_url.trim_end_matches('/')
                        == self.default_base_url.trim_end_matches('/')
                })
            {
                self.origins.clear();
            }
        }
        if let Some(origins) = patch.origins {
            self.origins = origins;
        }
        if let Some(api_key_env) = patch.api_key_env {
            self.api_key_env = Some(api_key_env);
        }
        if let Some(model_env) = patch.model_env {
            self.model_env = Some(model_env);
        }
        if let Some(base_url_env) = patch.base_url_env {
            self.base_url_env = Some(base_url_env);
        }
        if let Some(default_model) = patch.default_model {
            self.default_model = default_model;
        }
        if let Some(default_endpoint_path) = patch.default_endpoint_path {
            self.default_endpoint_path = Some(default_endpoint_path);
        }
        if let Some(default_token_counter) = patch.default_token_counter {
            self.default_token_counter = Some(default_token_counter);
        }
        if let Some(models_dev_provider) = patch.models_dev_provider {
            self.models_dev_provider = Some(models_dev_provider);
        }
        if let Some(model_exclude_prefixes) = patch.model_exclude_prefixes {
            self.model_exclude_prefixes = model_exclude_prefixes;
        }
        if let Some(reasoning_codec) = patch.reasoning_codec {
            self.reasoning_codec = Some(reasoning_codec);
        }
        if let Some(prompt_cache) = patch.prompt_cache {
            self.prompt_cache = prompt_cache;
        }
        if let Some(prompt_cache_header) = patch.prompt_cache_header {
            self.prompt_cache_header = Some(prompt_cache_header);
        }
        if let Some(prompt_cache_policy) = patch.prompt_cache_policy {
            self.prompt_cache_policy = prompt_cache_policy;
        }
        if let Some(reasoning_content_echo) = patch.reasoning_content_echo {
            self.reasoning_content_echo = reasoning_content_echo;
        }
        if let Some(usage_frame_ends_stream) = patch.usage_frame_ends_stream {
            self.usage_frame_ends_stream = usage_frame_ends_stream;
        }
        if let Some(auth_header) = patch.auth_header {
            self.auth_header = Some(auth_header);
        }
        if let Some(discovery) = patch.discovery {
            self.discovery = discovery;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct TargetSpec {
    pub connection: ConnectionId,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub model: ModelId,
    #[serde(default)]
    pub display_name: Option<Box<str>>,
    #[serde(default)]
    pub metadata_model: Option<ModelId>,
    #[serde(default)]
    pub remote_model: Option<Box<str>>,
    /// Alternate persisted or user-facing selectors that resolve to this
    /// target without appearing as additional models in provider pickers.
    #[serde(default)]
    pub aliases: Vec<Box<str>>,
    #[serde(default)]
    pub recommended: bool,
    #[serde(default)]
    pub recommended_effort: Option<ReasoningSelection>,
    #[serde(default)]
    pub discouraged_efforts: Vec<ReasoningSelection>,
    #[serde(default, rename = "default")]
    pub is_default: bool,
    #[serde(default)]
    pub transport: Option<TransportProtocol>,
    /// Optional model-level override for the connection cache policy.
    #[serde(default)]
    pub prompt_cache_policy: Option<PromptCachePolicy>,
    #[serde(default)]
    pub endpoint_path: Option<Box<str>>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub output_limit: Option<u32>,
    #[serde(default)]
    pub token_counter: Option<TokenCounterKind>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning_codec: Option<ReasoningCodec>,
    #[serde(default, deserialize_with = "deserialize_optional_reasoning_options")]
    pub reasoning_options: Option<Vec<ReasoningOption>>,
    #[serde(default)]
    pub features: Vec<ModelFeature>,
    #[serde(default)]
    pub pricing: Option<ModelPricing>,
    #[serde(default)]
    pub pricing_tiers: Vec<ModelPricingTier>,
    #[serde(default)]
    pub roles: Vec<LegacyModelRole>,
    /// Marks a deliberate divergence from refreshed metadata (working-window
    /// caps, price-tier pins, beta-gated limits). Pinned values remain
    /// authoritative and skip drift warnings; unpinned values act as offline
    /// fallbacks so `/refresh` can adopt newer provider/models.dev metadata.
    #[serde(default)]
    pub pinned: bool,
    /// Narrower alternative to `pinned = true`: only the named metadata fields
    /// keep their catalog values while every other field can refresh normally.
    /// This is used for provider-specific working-window caps whose prices and
    /// capabilities must still track live sources.
    #[serde(default)]
    pub pinned_fields: Vec<ModelMetadataField>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ModelMetadataField {
    DisplayName,
    ContextWindow,
    OutputLimit,
    Pricing,
    Features,
    Reasoning,
}

#[derive(Debug, Deserialize)]
struct RawTargetSpecPatch {
    connection: ConnectionId,
    #[serde(default)]
    enabled: Option<bool>,
    model: String,
    #[serde(default)]
    display_name: Option<Box<str>>,
    #[serde(default)]
    metadata_model: Option<ModelId>,
    #[serde(default)]
    remote_model: Option<Box<str>>,
    #[serde(default)]
    aliases: Option<Vec<Box<str>>>,
    #[serde(default)]
    recommended: Option<bool>,
    #[serde(default)]
    recommended_effort: Option<ReasoningSelection>,
    #[serde(default)]
    discouraged_efforts: Option<Vec<ReasoningSelection>>,
    #[serde(default, rename = "default")]
    is_default: Option<bool>,
    #[serde(default)]
    transport: Option<TransportProtocol>,
    #[serde(default)]
    prompt_cache_policy: Option<PromptCachePolicy>,
    #[serde(default)]
    endpoint_path: Option<Box<str>>,
    #[serde(default)]
    context_window: Option<u32>,
    #[serde(default)]
    output_limit: Option<u32>,
    #[serde(default)]
    token_counter: Option<TokenCounterKind>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    reasoning_codec: Option<ReasoningCodec>,
    #[serde(default, deserialize_with = "deserialize_optional_reasoning_options")]
    reasoning_options: Option<Vec<ReasoningOption>>,
    #[serde(default)]
    features: Option<Vec<ModelFeature>>,
    #[serde(default)]
    pricing: Option<ModelPricing>,
    #[serde(default)]
    pricing_tiers: Option<Vec<ModelPricingTier>>,
    #[serde(default)]
    roles: Option<Vec<LegacyModelRole>>,
    #[serde(default)]
    pinned: Option<bool>,
    #[serde(default)]
    pinned_fields: Option<Vec<ModelMetadataField>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetSpecPatch {
    pub connection: ConnectionId,
    pub enabled: Option<bool>,
    pub model: ModelId,
    pub display_name: Option<Box<str>>,
    pub metadata_model: Option<ModelId>,
    pub remote_model: Option<Box<str>>,
    pub aliases: Option<Vec<Box<str>>>,
    pub recommended: Option<bool>,
    pub recommended_effort: Option<ReasoningSelection>,
    pub discouraged_efforts: Option<Vec<ReasoningSelection>>,
    pub is_default: Option<bool>,
    pub transport: Option<TransportProtocol>,
    pub prompt_cache_policy: Option<PromptCachePolicy>,
    pub endpoint_path: Option<Box<str>>,
    pub context_window: Option<u32>,
    pub output_limit: Option<u32>,
    pub token_counter: Option<TokenCounterKind>,
    pub max_tokens: Option<u32>,
    pub reasoning_codec: Option<ReasoningCodec>,
    pub reasoning_options: Option<Vec<ReasoningOption>>,
    pub features: Option<Vec<ModelFeature>>,
    pub pricing: Option<ModelPricing>,
    pub pricing_tiers: Option<Vec<ModelPricingTier>>,
    pub roles: Option<Vec<LegacyModelRole>>,
    pub pinned: Option<bool>,
    pub pinned_fields: Option<Vec<ModelMetadataField>>,
}

impl<'de> Deserialize<'de> for TargetSpecPatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawTargetSpecPatch::deserialize(deserializer)?;
        let (model, alias_display_name) =
            normalize_target_model(&raw.connection, &raw.model).map_err(de::Error::custom)?;
        Ok(Self {
            connection: raw.connection,
            enabled: raw.enabled,
            model,
            display_name: raw.display_name.or(alias_display_name),
            metadata_model: raw.metadata_model,
            remote_model: raw.remote_model,
            aliases: raw.aliases,
            recommended: raw.recommended,
            recommended_effort: raw.recommended_effort,
            discouraged_efforts: raw.discouraged_efforts,
            is_default: raw.is_default,
            transport: raw.transport,
            prompt_cache_policy: raw.prompt_cache_policy,
            endpoint_path: raw.endpoint_path,
            context_window: raw.context_window,
            output_limit: raw.output_limit,
            token_counter: raw.token_counter,
            max_tokens: raw.max_tokens,
            reasoning_codec: raw.reasoning_codec,
            reasoning_options: raw.reasoning_options,
            features: raw.features,
            pricing: raw.pricing,
            pricing_tiers: raw.pricing_tiers,
            roles: raw.roles,
            pinned: raw.pinned,
            pinned_fields: raw.pinned_fields,
        })
    }
}

impl TargetSpecPatch {
    pub(crate) fn into_complete(self) -> TargetSpec {
        TargetSpec {
            connection: self.connection,
            enabled: self.enabled.unwrap_or(true),
            model: self.model,
            display_name: self.display_name,
            metadata_model: self.metadata_model,
            remote_model: self.remote_model,
            aliases: self.aliases.unwrap_or_default(),
            recommended: self.recommended.unwrap_or(false),
            recommended_effort: self.recommended_effort,
            discouraged_efforts: self.discouraged_efforts.unwrap_or_default(),
            is_default: self.is_default.unwrap_or(false),
            transport: self.transport,
            prompt_cache_policy: self.prompt_cache_policy,
            endpoint_path: self.endpoint_path,
            context_window: self.context_window,
            output_limit: self.output_limit,
            token_counter: self.token_counter,
            max_tokens: self.max_tokens,
            reasoning_codec: self.reasoning_codec,
            reasoning_options: self.reasoning_options,
            features: self.features.unwrap_or_default(),
            pricing: self.pricing,
            pricing_tiers: self.pricing_tiers.unwrap_or_default(),
            roles: self.roles.unwrap_or_default(),
            pinned: self.pinned.unwrap_or(false),
            pinned_fields: self.pinned_fields.unwrap_or_default(),
        }
    }
}

impl TargetSpec {
    pub(crate) fn pins(&self, field: ModelMetadataField) -> bool {
        self.pinned || self.pinned_fields.contains(&field)
    }

    pub(crate) fn apply_patch(&mut self, patch: TargetSpecPatch) {
        if let Some(enabled) = patch.enabled {
            self.enabled = enabled;
        }
        if let Some(recommended_effort) = patch.recommended_effort {
            self.recommended_effort = Some(recommended_effort);
        }
        if let Some(discouraged_efforts) = patch.discouraged_efforts {
            self.discouraged_efforts = discouraged_efforts;
        }
        if let Some(metadata_model) = patch.metadata_model {
            self.metadata_model = Some(metadata_model);
        }
        if let Some(display_name) = patch.display_name {
            self.display_name = Some(display_name);
        }
        if let Some(remote_model) = patch.remote_model {
            self.remote_model = Some(remote_model);
        }
        if let Some(aliases) = patch.aliases {
            self.aliases = aliases;
        }
        if let Some(recommended) = patch.recommended {
            self.recommended = recommended;
        }
        if let Some(is_default) = patch.is_default {
            self.is_default = is_default;
        }
        if let Some(transport) = patch.transport {
            self.transport = Some(transport);
        }
        if let Some(prompt_cache_policy) = patch.prompt_cache_policy {
            self.prompt_cache_policy = Some(prompt_cache_policy);
        }
        if let Some(endpoint_path) = patch.endpoint_path {
            self.endpoint_path = Some(endpoint_path);
        }
        if let Some(context_window) = patch.context_window {
            self.context_window = Some(context_window);
        }
        if let Some(output_limit) = patch.output_limit {
            self.output_limit = Some(output_limit);
        }
        if let Some(token_counter) = patch.token_counter {
            self.token_counter = Some(token_counter);
        }
        if let Some(max_tokens) = patch.max_tokens {
            self.max_tokens = Some(max_tokens);
        }
        if let Some(reasoning_codec) = patch.reasoning_codec {
            self.reasoning_codec = Some(reasoning_codec);
        }
        if let Some(reasoning_options) = patch.reasoning_options {
            self.reasoning_options = Some(reasoning_options);
        }
        if let Some(features) = patch.features {
            self.features = features;
        }
        if let Some(pricing) = patch.pricing {
            self.pricing = Some(pricing);
        }
        if let Some(pricing_tiers) = patch.pricing_tiers {
            self.pricing_tiers = pricing_tiers;
        }
        if let Some(roles) = patch.roles {
            self.roles = roles;
        }
        if let Some(pinned) = patch.pinned {
            self.pinned = pinned;
        }
        if let Some(pinned_fields) = patch.pinned_fields {
            self.pinned_fields = pinned_fields;
        }
    }
}

fn normalize_target_model(
    connection: &ConnectionId,
    value: &str,
) -> Result<(ModelId, Option<Box<str>>), IdError> {
    match value.parse::<ModelId>() {
        Ok(model) => Ok((model, None)),
        Err(IdError::ModelMissingSeparator(_)) if is_bare_target_model_alias(value) => {
            let model = format!("{}/{}", connection.as_str(), value).parse::<ModelId>()?;
            Ok((model, Some(value.into())))
        }
        Err(err) => Err(err),
    }
}

fn is_bare_target_model_alias(value: &str) -> bool {
    value.trim() == value
        && !value.is_empty()
        && !value.contains('/')
        && !value.chars().any(char::is_whitespace)
}

fn deserialize_optional_model_id<'de, D>(deserializer: D) -> Result<Option<ModelId>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref() {
        None | Some("") => Ok(None),
        Some(value) => value.parse().map(Some).map_err(de::Error::custom),
    }
}

fn deserialize_optional_model_id_patch<'de, D>(
    deserializer: D,
) -> Result<Option<Option<ModelId>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref() {
        None => Ok(None),
        Some("") => Ok(Some(None)),
        Some(value) => value.parse().map(Some).map(Some).map_err(de::Error::custom),
    }
}

fn deserialize_optional_reasoning_options<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<ReasoningOption>>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Option::<Vec<RawTargetReasoningOption>>::deserialize(deserializer)?;
    values
        .map(|values| {
            let mut options = Vec::new();
            for value in values {
                options.extend(value.into_options().map_err(de::Error::custom)?);
            }
            Ok(options)
        })
        .transpose()
}

#[derive(Debug, Deserialize)]
struct RawTargetReasoningOption {
    #[serde(rename = "type")]
    option_type: Box<str>,
    #[serde(default)]
    values: Vec<Box<str>>,
    #[serde(default, deserialize_with = "deserialize_optional_non_negative_u32")]
    min: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_optional_non_negative_u32")]
    max: Option<u32>,
    #[serde(default)]
    default: Option<u32>,
}

impl RawTargetReasoningOption {
    fn into_options(self) -> Result<Vec<ReasoningOption>, String> {
        match self.option_type.as_ref() {
            "toggle" => Ok(vec![ReasoningOption::Toggle]),
            "effort" => {
                let mut has_toggle = false;
                let mut efforts = Vec::new();
                for value in &self.values {
                    if matches!(value.as_ref(), "none" | "off") {
                        has_toggle = true;
                    } else {
                        efforts.push(parse_reasoning_effort(value)?);
                    }
                }
                let mut options = Vec::new();
                if has_toggle {
                    options.push(ReasoningOption::Toggle);
                }
                let efforts = dedup_efforts(efforts);
                if !efforts.is_empty() {
                    options.push(ReasoningOption::Effort(efforts));
                }
                Ok(options)
            }
            "budget_tokens" => Ok(vec![ReasoningOption::BudgetTokens {
                min: self.min,
                max: self.max,
                default: self
                    .default
                    .unwrap_or_else(|| default_budget_tokens(self.min, self.max)),
            }]),
            other => Ok(vec![ReasoningOption::Unknown(other.into())]),
        }
    }
}

pub(crate) fn parse_reasoning_effort(value: &str) -> Result<ReasoningEffort, String> {
    if value.trim() != value {
        return Err(format!("invalid reasoning effort `{value}`"));
    }
    match value {
        "minimal" => Ok(ReasoningEffort::Minimal),
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "high" => Ok(ReasoningEffort::High),
        "xhigh" | "x-high" => Ok(ReasoningEffort::XHigh),
        "max" => Ok(ReasoningEffort::Max),
        "ultra" => Ok(ReasoningEffort::Ultra),
        _ => Err(format!("unknown reasoning effort `{value}`")),
    }
}
