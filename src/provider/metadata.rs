use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::model_catalog::{DiscoveryKind, TransportProtocol};
use crate::provider::{ModelPricing, TokenCounterKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    OpenAiChat,
    AnthropicMessages,
    CodexResponses,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenAiChat => f.write_str("openai-chat"),
            Self::AnthropicMessages => f.write_str("anthropic-messages"),
            Self::CodexResponses => f.write_str("codex-responses"),
        }
    }
}

// `Protocol` (provider-facing) and `TransportProtocol` (catalog-facing) are the
// same wire-transport concept named in two domains. These conversions are the
// single place the mapping lives, so adding a transport is a compile error in
// both directions until both enums agree.
impl From<TransportProtocol> for Protocol {
    fn from(transport: TransportProtocol) -> Self {
        match transport {
            TransportProtocol::OpenAiChat => Self::OpenAiChat,
            TransportProtocol::AnthropicMessages => Self::AnthropicMessages,
            TransportProtocol::CodexResponses => Self::CodexResponses,
        }
    }
}

impl From<Protocol> for TransportProtocol {
    fn from(protocol: Protocol) -> Self {
        match protocol {
            Protocol::OpenAiChat => Self::OpenAiChat,
            Protocol::AnthropicMessages => Self::AnthropicMessages,
            Protocol::CodexResponses => Self::CodexResponses,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
}

impl ReasoningEffort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningOption {
    Toggle,
    Effort(Vec<ReasoningEffort>),
    BudgetTokens {
        min: Option<u32>,
        max: Option<u32>,
        default: u32,
    },
    Unknown(Box<str>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ReasoningSelection {
    #[default]
    Default,
    Off,
    On,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
    BudgetTokens(u32),
}

impl ReasoningSelection {
    /// One-step downgrade used for a length-truncated request retry.
    pub(crate) const fn lower_for_retry(self) -> Option<Self> {
        match self {
            Self::Default => Some(Self::Off),
            Self::Ultra => Some(Self::Max),
            Self::Max => Some(Self::High),
            Self::XHigh => Some(Self::High),
            Self::High => Some(Self::Medium),
            _ => None,
        }
    }

    /// The nearest stronger explicit effort advertised by the active model.
    /// Non-effort selections are deliberately not guessed: `Default`, toggles,
    /// and token budgets do not reveal a portable effective effort.
    pub(crate) fn next_higher_supported(self, supported: &[Self]) -> Option<Self> {
        let current_rank = self.explicit_effort_rank()?;
        supported
            .iter()
            .copied()
            .filter_map(|candidate| {
                candidate
                    .explicit_effort_rank()
                    .filter(|rank| *rank > current_rank)
                    .map(|rank| (rank, candidate))
            })
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, candidate)| candidate)
    }

    pub(crate) const fn explicit_effort_rank(self) -> Option<u8> {
        match self {
            Self::Minimal => Some(1),
            Self::Low => Some(2),
            Self::Medium => Some(3),
            Self::High => Some(4),
            Self::XHigh => Some(5),
            Self::Max => Some(6),
            Self::Ultra => Some(7),
            Self::Default | Self::Off | Self::On | Self::BudgetTokens(_) => None,
        }
    }

    /// Parse an exact user-facing reasoning label.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" | "auto" => Some(Self::Default),
            "off" | "none" => Some(Self::Off),
            "on" | "thinking" => Some(Self::On),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            "ultra" => Some(Self::Ultra),
            value if value.starts_with("budget:") => value
                .trim_start_matches("budget:")
                .parse::<u32>()
                .ok()
                .map(Self::BudgetTokens),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Off => "off",
            Self::On => "on",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
            Self::BudgetTokens(_) => "budget_tokens",
        }
    }

    pub const fn from_effort(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::Minimal => Self::Minimal,
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::XHigh => Self::XHigh,
            ReasoningEffort::Max => Self::Max,
            ReasoningEffort::Ultra => Self::Ultra,
        }
    }

    pub const fn sort_rank(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::Off => 1,
            Self::On => 2,
            Self::Minimal => 3,
            Self::Low => 4,
            Self::Medium => 5,
            Self::High => 6,
            Self::XHigh => 7,
            Self::Max => 8,
            Self::Ultra => 9,
            Self::BudgetTokens(_) => 10,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::BudgetTokens(tokens) => format!("budget:{tokens}"),
            _ => self.as_str().to_string(),
        }
    }

    fn from_label(value: &str) -> Self {
        Self::parse(value).unwrap_or_default()
    }

    fn from_json_value(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Default,
            serde_json::Value::Bool(true) => Self::On,
            serde_json::Value::Bool(false) => Self::Off,
            serde_json::Value::Number(number) => number
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .map(Self::BudgetTokens)
                .unwrap_or(Self::Default),
            serde_json::Value::String(value) => Self::from_label(value),
            serde_json::Value::Array(_) => Self::Default,
            serde_json::Value::Object(object) => {
                for key in [
                    "reasoning",
                    "profile",
                    "variant",
                    "effort",
                    "budget_tokens",
                    "budgetTokens",
                ] {
                    if let Some(value) = object.get(key) {
                        return Self::from_json_value(value);
                    }
                }
                if object.len() == 1
                    && let Some((key, value)) = object.iter().next()
                {
                    let selection = Self::from_label(key);
                    if selection != Self::Default {
                        return selection;
                    }
                    return Self::from_json_value(value);
                }
                Self::Default
            }
        }
    }
}

impl fmt::Display for ReasoningSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

impl Serialize for ReasoningSelection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.label())
    }
}

impl<'de> Deserialize<'de> for ReasoningSelection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        Ok(Self::from_json_value(&value))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum ParameterPreview {
    MaxTokens(u32),
}

impl ParameterPreview {
    pub fn label(self) -> String {
        match self {
            Self::MaxTokens(value) => format!("max_tokens: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub reasoning: &'static [ReasoningSelection],
    pub parameter_preview: &'static [ParameterPreview],
    /// Whether the provider's backend supports prompt caching. On the Anthropic
    /// Messages API this gates explicit `cache_control` breakpoints; on
    /// OpenAI-family transports (Chat Completions / Codex Responses) it gates
    /// emitting a `prompt_cache_key` routing hint so a warm prefix stays pinned
    /// to the same backend across the short idle TTL between turns. Defaults to
    /// `false`; enable via [`ProviderCapabilities::with_prompt_cache`]. Usage
    /// accounting (cached-token counts) surfaces through the usage path
    /// regardless of this flag.
    pub supports_prompt_cache: bool,
    /// Whether the provider's models can accept image content parts on user
    /// messages. This is the provider-level fallback; per-model resolution
    /// prefers the model catalog's `attachment` feature when available.
    /// Defaults to `false`; enable via [`ProviderCapabilities::with_vision`].
    pub supports_vision: bool,
    /// Whether the backend needs a `reasoning_content` field echoed back on
    /// every assistant message that carries `tool_calls`. Off by default:
    /// `reasoning_content` is not part of the OpenAI Chat Completions schema,
    /// so it is only sent to backends verified to want it (see
    /// [`ProviderCapabilities::with_reasoning_content_echo`]).
    pub echoes_reasoning_content: bool,
    /// Whether a terminal usage frame ends a stream that never sends a
    /// `finish_reason`. Off by default, because for a spec-abiding backend a
    /// missing `finish_reason` means the stream was cut off mid-response, and
    /// treating that as success reports a truncated turn as a complete one (see
    /// [`ProviderCapabilities::with_usage_frame_stream_terminal`]).
    pub usage_frame_is_stream_terminal: bool,
}

impl ProviderCapabilities {
    pub const fn new(
        reasoning: &'static [ReasoningSelection],
        parameter_preview: &'static [ParameterPreview],
    ) -> Self {
        Self {
            reasoning,
            parameter_preview,
            supports_prompt_cache: false,
            supports_vision: false,
            echoes_reasoning_content: false,
            usage_frame_is_stream_terminal: false,
        }
    }

    /// Mark the provider as supporting explicit `cache_control` breakpoints.
    pub const fn with_prompt_cache(mut self) -> Self {
        self.supports_prompt_cache = true;
        self
    }

    /// Mark the provider's models as accepting image content parts.
    pub const fn with_vision(mut self) -> Self {
        self.supports_vision = true;
        self
    }

    /// Mark the backend as needing `reasoning_content` echoed back on assistant
    /// messages that carry `tool_calls`.
    pub const fn with_reasoning_content_echo(mut self) -> Self {
        self.echoes_reasoning_content = true;
        self
    }

    /// Mark the backend as ending streams on a usage frame rather than a
    /// `finish_reason`. Only for backends observed to complete a response
    /// without ever sending one.
    pub const fn with_usage_frame_stream_terminal(mut self) -> Self {
        self.usage_frame_is_stream_terminal = true;
        self
    }

    pub fn supports_reasoning(self, reasoning: ReasoningSelection) -> bool {
        reasoning == ReasoningSelection::Default || self.reasoning.contains(&reasoning)
    }
}

pub const DEFAULT_CONTEXT_WINDOW_TOKENS: u32 = 120_000;
pub const NO_PARAMETERS: &[ParameterPreview] = &[];
pub const NO_REASONING: &[ReasoningSelection] = &[];
pub const CODEX_REASONING: &[ReasoningSelection] = &[
    ReasoningSelection::Off,
    ReasoningSelection::Low,
    ReasoningSelection::Medium,
    ReasoningSelection::High,
    ReasoningSelection::XHigh,
];
pub const ANTHROPIC_PARAMETERS: &[ParameterPreview] = &[ParameterPreview::MaxTokens(16_000)];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthRequirement {
    ApiKeyRequired,
    ApiKeyOptional,
    CodexCache,
}

impl AuthRequirement {
    pub const fn uses_endpoint_setup(self) -> bool {
        matches!(self, Self::ApiKeyOptional)
    }

    pub const fn uses_codex_cache(self) -> bool {
        matches!(self, Self::CodexCache)
    }
}

#[derive(Debug, Clone)]
pub struct ProviderMetadata {
    pub id: Box<str>,
    pub display_name: Box<str>,
    pub default_model: Box<str>,
    pub default_base_url: Box<str>,
    pub env_var_api_key: Option<Box<str>>,
    pub env_var_model: Option<Box<str>>,
    pub env_var_base_url: Option<Box<str>>,
    pub seed_models: Box<[Box<str>]>,
    /// Live model-id prefixes hidden from this connection's picker.
    pub model_exclude_prefixes: Box<[Box<str>]>,
    pub protocol: Protocol,
    pub capabilities: ProviderCapabilities,
    pub endpoint_path: Box<str>,
    pub auth_requirement: AuthRequirement,
    pub context_window: Option<u32>,
    pub token_counter: Option<TokenCounterKind>,
    pub pricing: Option<ModelPricing>,
    /// How this provider's live model list is discovered (see
    /// [`DiscoveryKind`]). Built-ins are all `Generic`; catalog connections
    /// carry their configured kind through `build_provider_metadata`.
    pub discovery: DiscoveryKind,
    /// Non-default HTTP header carrying the API key. `None` uses the transport
    /// default (`x-api-key` for Anthropic, `Authorization: Bearer` for
    /// OpenAI-chat); `Some("api-key")` sends the key under that header verbatim,
    /// for backends like Xiaomi MiMo that do not accept the standard header.
    pub auth_header: Option<Box<str>>,
}

impl ProviderMetadata {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        display_name: &str,
        default_model: &str,
        default_base_url: &str,
        env_var_api_key: Option<&str>,
        env_var_model: Option<&str>,
        env_var_base_url: Option<&str>,
        seed_models: &[&str],
        protocol: Protocol,
        capabilities: ProviderCapabilities,
        endpoint_path: &str,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            default_model: default_model.into(),
            default_base_url: default_base_url.into(),
            env_var_api_key: env_var_api_key.map(Into::into),
            env_var_model: env_var_model.map(Into::into),
            env_var_base_url: env_var_base_url.map(Into::into),
            seed_models: seed_models
                .iter()
                .map(|model| Box::<str>::from(*model))
                .collect(),
            model_exclude_prefixes: Box::default(),
            protocol,
            capabilities,
            endpoint_path: endpoint_path.into(),
            auth_requirement: AuthRequirement::ApiKeyRequired,
            context_window: None,
            token_counter: None,
            pricing: None,
            discovery: DiscoveryKind::Generic,
            auth_header: None,
        }
    }

    pub fn with_auth_requirement(mut self, auth_requirement: AuthRequirement) -> Self {
        self.auth_requirement = auth_requirement;
        self
    }

    pub fn with_context_window(mut self, context_window: u32) -> Self {
        self.context_window = Some(context_window);
        self
    }

    pub fn with_token_counter(mut self, token_counter: TokenCounterKind) -> Self {
        self.token_counter = Some(token_counter);
        self
    }

    pub fn with_discovery(mut self, discovery: DiscoveryKind) -> Self {
        self.discovery = discovery;
        self
    }

    pub fn with_auth_header(mut self, auth_header: Option<Box<str>>) -> Self {
        self.auth_header = auth_header;
        self
    }

    /// The header name a raw API key is sent under on Anthropic-style
    /// transports: the connection override when set, otherwise `x-api-key`.
    /// (The OpenAI-chat transport sends `Authorization: Bearer` by default and
    /// applies any override itself, since its value format differs.)
    pub fn raw_api_key_header_name(&self) -> &str {
        self.auth_header.as_deref().unwrap_or("x-api-key")
    }

    pub fn is_known_id(&self, candidate: &str) -> bool {
        candidate.eq_ignore_ascii_case(&self.id)
    }

    /// The seed model list as owned `String`s — the fallback when a provider's
    /// live model list is empty or unavailable.
    pub fn seed_model_list(&self) -> Vec<String> {
        self.seed_models.iter().map(ToString::to_string).collect()
    }

    /// Capabilities for a given model. Capabilities are currently uniform per
    /// provider, so `model` is unused; the parameter is the forward-compat seam
    /// for per-model resolution and keeps callers stable when that lands.
    pub fn capabilities_for_model(&self, _model: &str) -> ProviderCapabilities {
        self.capabilities
    }

    pub fn normalize_reasoning_for_model(
        &self,
        model: &str,
        reasoning: ReasoningSelection,
    ) -> ReasoningSelection {
        if self
            .capabilities_for_model(model)
            .supports_reasoning(reasoning)
        {
            reasoning
        } else {
            ReasoningSelection::Default
        }
    }

    pub fn parameter_preview_for_model(&self, model: &str) -> String {
        let capabilities = self.capabilities_for_model(model);
        if capabilities.parameter_preview.is_empty() {
            "default parameters".to_string()
        } else {
            capabilities
                .parameter_preview
                .iter()
                .map(|preview| preview.label())
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin_metadata(id: &str) -> &'static ProviderMetadata {
        match crate::provider::metadata_for(id) {
            Some(metadata) => metadata,
            None => panic!("missing built-in provider metadata for {id}"),
        }
    }

    #[test]
    fn metadata_is_known_id_is_case_insensitive() {
        let meta = ProviderMetadata::new(
            "opencode",
            "OpenCode Go",
            "qwen3.7-max",
            "https://example.com",
            Some("OPENCODE_API_KEY"),
            Some("OPENCODE_MODEL"),
            Some("OPENCODE_BASE_URL"),
            &["qwen3.7-max"],
            Protocol::OpenAiChat,
            ProviderCapabilities::new(NO_REASONING, NO_PARAMETERS),
            "chat/completions",
        );

        assert!(meta.is_known_id("opencode"));
        assert!(meta.is_known_id("OpenCode"));
        assert!(meta.is_known_id("OPENCODE"));
        assert!(!meta.is_known_id("codex"));
    }

    #[test]
    fn protocol_display_is_stable() {
        assert_eq!(Protocol::OpenAiChat.to_string(), "openai-chat");
        assert_eq!(
            Protocol::AnthropicMessages.to_string(),
            "anthropic-messages"
        );
        assert_eq!(Protocol::CodexResponses.to_string(), "codex-responses");
    }

    #[test]
    fn length_retry_disables_unspecified_reasoning_and_bounds_high_effort() {
        assert_eq!(
            ReasoningSelection::Default.lower_for_retry(),
            Some(ReasoningSelection::Off)
        );
        assert_eq!(
            ReasoningSelection::Max.lower_for_retry(),
            Some(ReasoningSelection::High)
        );
        assert_eq!(ReasoningSelection::Off.lower_for_retry(), None);
        assert_eq!(ReasoningSelection::Medium.lower_for_retry(), None);
    }

    #[test]
    fn recovery_escalation_uses_only_the_nearest_advertised_effort() {
        let supported = [
            ReasoningSelection::Default,
            ReasoningSelection::Low,
            ReasoningSelection::High,
            ReasoningSelection::Max,
        ];

        assert_eq!(
            ReasoningSelection::Low.next_higher_supported(&supported),
            Some(ReasoningSelection::High)
        );
        assert_eq!(
            ReasoningSelection::High.next_higher_supported(&supported),
            Some(ReasoningSelection::Max)
        );
        assert_eq!(
            ReasoningSelection::Max.next_higher_supported(&supported),
            None
        );
        assert_eq!(
            ReasoningSelection::Default.next_higher_supported(&supported),
            None,
            "an unknown default effort must not be guessed"
        );
    }

    #[test]
    fn unsupported_reasoning_selection_normalizes_to_default() {
        let meta = ProviderMetadata::new(
            "anthropic",
            "Anthropic",
            "claude",
            "https://example.com",
            None,
            None,
            None,
            &["claude"],
            Protocol::AnthropicMessages,
            ProviderCapabilities::new(NO_REASONING, ANTHROPIC_PARAMETERS),
            "v1/messages",
        );

        assert_eq!(
            meta.normalize_reasoning_for_model(
                "claude",
                ReasoningSelection::from_effort(ReasoningEffort::High)
            ),
            ReasoningSelection::default()
        );
        assert!(
            meta.capabilities_for_model("claude")
                .supports_reasoning(ReasoningSelection::Default)
        );
        assert!(
            !meta
                .capabilities_for_model("claude")
                .supports_reasoning(ReasoningSelection::High)
        );
        assert_eq!(
            meta.parameter_preview_for_model("claude"),
            "max_tokens: 16000"
        );
    }

    #[test]
    fn builtin_connection_defaults_are_pinned() {
        struct ExpectedDefaults {
            id: &'static str,
            display_name: &'static str,
            default_model: &'static str,
            default_base_url: &'static str,
            env_var_api_key: Option<&'static str>,
            env_var_model: Option<&'static str>,
            env_var_base_url: Option<&'static str>,
            seed_models: &'static [&'static str],
            protocol: Protocol,
            endpoint_path: &'static str,
            auth_requirement: AuthRequirement,
            context_window: Option<u32>,
            token_counter: Option<TokenCounterKind>,
        }

        let cases = [
            ExpectedDefaults {
                id: "opencode",
                display_name: "OpenCode Go",
                default_model: "qwen3.7-max",
                default_base_url: "https://opencode.ai/zen/go/v1",
                env_var_api_key: Some("OPENCODE_API_KEY"),
                env_var_model: Some("OPENCODE_MODEL"),
                env_var_base_url: Some("OPENCODE_BASE_URL"),
                seed_models: &["qwen3.7-max", "glm-5.2"],
                protocol: Protocol::OpenAiChat,
                endpoint_path: "chat/completions",
                auth_requirement: AuthRequirement::ApiKeyRequired,
                context_window: Some(128_000),
                token_counter: Some(TokenCounterKind::Qwen3),
            },
            ExpectedDefaults {
                id: "codex",
                display_name: "Codex",
                default_model: "gpt-5.6-sol",
                default_base_url: "https://chatgpt.com/backend-api/codex",
                env_var_api_key: None,
                env_var_model: Some("CODEX_MODEL"),
                env_var_base_url: Some("CODEX_BASE_URL"),
                seed_models: &[
                    "gpt-5.6-sol",
                    "gpt-5.6-terra",
                    "gpt-5.6-luna",
                    "gpt-5.5",
                    "gpt-5.4",
                    "gpt-5.4-mini",
                ],
                protocol: Protocol::CodexResponses,
                endpoint_path: "responses",
                auth_requirement: AuthRequirement::CodexCache,
                context_window: Some(272_000),
                token_counter: Some(TokenCounterKind::Tiktoken),
            },
            ExpectedDefaults {
                id: "anthropic",
                display_name: "Anthropic API",
                default_model: "claude-sonnet-5",
                default_base_url: "https://api.anthropic.com",
                env_var_api_key: Some("ANTHROPIC_API_KEY"),
                env_var_model: Some("ANTHROPIC_MODEL"),
                env_var_base_url: Some("ANTHROPIC_BASE_URL"),
                seed_models: &[
                    "claude-sonnet-4-5",
                    "claude-sonnet-5",
                    "claude-opus-5",
                    "claude-opus-4-8",
                    "claude-fable-5",
                    "claude-haiku-4-5",
                    "claude-opus-4-1",
                ],
                protocol: Protocol::AnthropicMessages,
                endpoint_path: "v1/messages",
                auth_requirement: AuthRequirement::ApiKeyRequired,
                context_window: Some(200_000),
                token_counter: Some(TokenCounterKind::AnthropicCountTokens),
            },
            ExpectedDefaults {
                id: "minimax-coding-plan",
                display_name: "MiniMax Coding Plan",
                default_model: "MiniMax-M3",
                default_base_url: "https://api.minimax.io/anthropic",
                env_var_api_key: Some("MINIMAX_CODING_PLAN_API_KEY"),
                env_var_model: Some("MINIMAX_CODING_PLAN_MODEL"),
                env_var_base_url: Some("MINIMAX_CODING_PLAN_BASE_URL"),
                seed_models: &[
                    "MiniMax-M3",
                    "MiniMax-M2.5",
                    "MiniMax-M2.5-highspeed",
                    "MiniMax-M2.7",
                    "MiniMax-M2",
                    "MiniMax-M2.7-highspeed",
                    "MiniMax-M2.1",
                ],
                protocol: Protocol::AnthropicMessages,
                endpoint_path: "v1/messages",
                auth_requirement: AuthRequirement::ApiKeyRequired,
                context_window: Some(200_000),
                token_counter: Some(TokenCounterKind::AnthropicCountTokens),
            },
        ];

        for case in cases {
            let metadata = builtin_metadata(case.id);
            assert_eq!(metadata.id.as_ref(), case.id);
            assert_eq!(metadata.display_name.as_ref(), case.display_name);
            assert_eq!(
                metadata.default_model.as_ref(),
                case.default_model,
                "{}",
                case.id
            );
            assert_eq!(
                metadata.default_base_url.as_ref(),
                case.default_base_url,
                "{}",
                case.id
            );
            assert_eq!(
                metadata.env_var_api_key.as_deref(),
                case.env_var_api_key,
                "{}",
                case.id
            );
            assert_eq!(
                metadata.env_var_model.as_deref(),
                case.env_var_model,
                "{}",
                case.id
            );
            assert_eq!(
                metadata.env_var_base_url.as_deref(),
                case.env_var_base_url,
                "{}",
                case.id
            );
            assert_eq!(
                metadata
                    .seed_models
                    .iter()
                    .map(|model| model.as_ref())
                    .collect::<Vec<&str>>(),
                case.seed_models,
                "{}",
                case.id
            );
            assert_eq!(metadata.protocol, case.protocol, "{}", case.id);
            assert_eq!(
                metadata.endpoint_path.as_ref(),
                case.endpoint_path,
                "{}",
                case.id
            );
            assert_eq!(
                metadata.auth_requirement, case.auth_requirement,
                "{}",
                case.id
            );
            assert_eq!(metadata.context_window, case.context_window, "{}", case.id);
            assert_eq!(metadata.token_counter, case.token_counter, "{}", case.id);
            // Built-ins all use the generic listing endpoint; server-specific
            // discovery kinds are a catalog-connection concern.
            assert_eq!(metadata.discovery, DiscoveryKind::Generic, "{}", case.id);
        }
    }
}
