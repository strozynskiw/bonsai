mod anthropic;
mod auth;
mod codex;
mod discovery;
mod endpoint;
mod metadata;
mod minimax_coding_plan;
mod openai_catalog;
mod opencode;
mod reasoning;
mod registry;
mod request_preview;
mod sse;
mod streaming;
#[cfg(test)]
pub(crate) mod test_utils;
mod think_tags;
mod token;
mod tool_calls;
mod transform;
mod usage;

pub use anthropic::AnthropicFactory;
pub use auth::{AuthInput, AuthorizeOutcome};
pub use codex::CodexFactory;
pub(crate) use codex::codex_cached_authorization;
pub(crate) use discovery::{DetectedServer, detect_server, fetch_models_with_discovery};
pub(crate) use endpoint::{
    normalize_anthropic_compatible_base_url, normalize_openai_compatible_base_url,
};
pub use metadata::{
    ANTHROPIC_PARAMETERS, AuthRequirement, CODEX_REASONING, DEFAULT_CONTEXT_WINDOW_TOKENS,
    NO_PARAMETERS, NO_REASONING, ParameterPreview, Protocol, ProviderCapabilities,
    ProviderMetadata, ReasoningEffort, ReasoningOption, ReasoningSelection,
};
pub use minimax_coding_plan::MiniMaxCodingPlanFactory;
pub use opencode::OpenCodeFactory;
pub(crate) use reasoning::ReasoningCodec;
pub use registry::ProviderRegistry;
pub use request_preview::{
    ProviderRequestDiagnostics, ProviderRequestPreview, ProviderWireSection, WireCacheHint,
};
pub(crate) use request_preview::{
    WireField, annotate_cache_control_sections, wire_sections_from_body,
};
pub(crate) use token::PromptEstimatorCacheKey;
pub use token::{
    EstimateConfidence, ModelPricing, PromptEstimate, PromptEstimator, TokenCounterKind,
};

use anyhow::Result;
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
#[cfg(test)]
use serde_json::json;
use thiserror::Error;

use crate::model_catalog::{LiveModelAvailability, RunTarget, TransportProtocol};
use crate::output::SharedSink;
use crate::session::ProviderSession;

/// How volatile project state is represented in an agent conversation.
///
/// This is provider-specific because the cache contracts differ: compatible
/// providers retain their established mutable system tail by default, while
/// explicitly selected automatic-cache routes use exact append-only input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProjectStateCacheStrategy {
    /// Preserve Bonsai's established provider behavior: update volatile state
    /// in system message zero and let the transport place its cache breakpoint.
    #[default]
    MutableSystemTail,
    /// Store changed state as immutable named user messages so every previous
    /// automatic-cache request remains an exact prefix of the next input.
    AppendOnlyHistory,
}

/// Metadata for every built-in provider — the single source of truth for
/// defaults and env-var names that session normalization consumes. Kept in
/// registration order; a test asserts it matches the registry's factories.
pub(crate) fn all_metadata() -> [&'static ProviderMetadata; 4] {
    [
        &opencode::OPENCODE_METADATA,
        &codex::CODEX_METADATA,
        &anthropic::ANTHROPIC_METADATA,
        &minimax_coding_plan::MINIMAX_CODING_PLAN_METADATA,
    ]
}

/// Look up a provider's metadata by id (case-insensitive), if built in.
pub(crate) fn metadata_for(id: &str) -> Option<&'static ProviderMetadata> {
    all_metadata().into_iter().find(|meta| meta.is_known_id(id))
}

/// Failure returned by a provider while preparing or streaming a response.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderFailure {
    #[error("provider HTTP error ({status}): {message}")]
    Http {
        status: u16,
        message: String,
        retry_after_secs: Option<u64>,
    },
    #[error("provider transport error: {message}")]
    Transport { message: String },
    #[error("provider response decode error: {message}")]
    Decode { message: String },
    #[error("provider configuration error: {message}")]
    Configuration { message: String },
}

/// Provider-enforced account limit represented by an HTTP failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderLimitKind {
    RateLimit,
    Quota,
}

/// Result type for provider response streaming.
pub type ProviderResult<T> = std::result::Result<T, ProviderFailure>;

/// Format a terminal provider failure with the same recovery guidance on every
/// product surface.
pub(crate) fn agent_failure_detail(error: &anyhow::Error) -> String {
    let mut detail = format!("{error:#}");
    if let Some(hint) = error
        .downcast_ref::<ProviderFailure>()
        .and_then(ProviderFailure::user_action_hint)
    {
        detail.push_str("\n\nAction: ");
        detail.push_str(hint);
    }
    detail
}

impl ProviderFailure {
    pub fn is_retryable(&self) -> bool {
        if self.limit_kind() == Some(ProviderLimitKind::Quota) {
            return false;
        }
        // 529 is Anthropic's overload status (also emitted as an
        // `overloaded_error` frame mid-stream); treat it like the other
        // transient classes.
        matches!(self, Self::Http { status, .. } if matches!(*status, 429 | 500 | 502 | 503 | 504 | 529))
            || matches!(self, Self::Transport { .. } | Self::Decode { .. })
    }

    /// Classify provider-enforced rate and account limits without exposing the
    /// provider's response body to terminal-state persistence.
    pub(crate) fn limit_kind(&self) -> Option<ProviderLimitKind> {
        let Self::Http {
            status, message, ..
        } = self
        else {
            return None;
        };
        if *status == 402 || message_signals_quota_exhaustion(message) {
            Some(ProviderLimitKind::Quota)
        } else if *status == 429 {
            Some(ProviderLimitKind::RateLimit)
        } else {
            None
        }
    }

    /// Build a typed error from an in-stream error frame, mapping the provider's
    /// error `type`/`code` onto an HTTP-status proxy so transient overload and
    /// rate-limit frames are retried by `chat_stream_with_retry` exactly like an
    /// up-front non-2xx response. Both `type` and `code` are consulted (OpenAI
    /// often keys the generic class in `type` and the specific one in `code`),
    /// and the first recognized class wins. Unknown/permanent classes map to 400
    /// so they surface immediately instead of being retried.
    pub fn from_stream_error(
        error_type: Option<&str>,
        error_code: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        fn classify(class: &str) -> Option<u16> {
            match class {
                "overloaded_error" => Some(529),
                "rate_limit_error" | "rate_limit_exceeded" | "rate_limited" => Some(429),
                "api_error" | "internal_server_error" | "server_error" => Some(503),
                _ => None,
            }
        }
        let status = error_type
            .and_then(classify)
            .or_else(|| error_code.and_then(classify))
            .unwrap_or(400);
        Self::Http {
            status,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    /// A retryable error for a corrupt or truncated stream frame (malformed JSON,
    /// or a frame the server cut off mid-flight).
    pub fn stream_decode(message: impl Into<String>) -> Self {
        Self::Decode {
            message: message.into(),
        }
    }

    /// A retryable error for a transport-level stream failure: the connection was
    /// reset, the response body read errored, or a chunk timed out mid-stream
    /// (e.g. an HTTP/2 stream reset). These are transient and safe to retry — a
    /// stream that dies mid-flight returns `Err` before any tool call is
    /// dispatched, so the turn had no side effects. The retry coordinator
    /// handles this variant exhaustively rather than recovering it by downcast.
    pub fn transport(message: impl Into<String>) -> Self {
        Self::Transport {
            message: message.into(),
        }
    }

    /// A non-retryable error caused by invalid provider configuration or request
    /// construction.
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::Configuration {
            message: message.into(),
        }
    }

    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::Http {
                retry_after_secs, ..
            } => *retry_after_secs,
            Self::Transport { .. } | Self::Decode { .. } | Self::Configuration { .. } => None,
        }
    }

    /// Actionable guidance after a provider failure becomes terminal. Transient
    /// failures are still retried first by the agent's bounded retry policy.
    pub(crate) fn user_action_hint(&self) -> Option<&'static str> {
        match self {
            Self::Http { status: 401, .. } => Some(
                "The provider rejected the active credential. Run /authorize to replace it; for Codex, run `codex login` first.",
            ),
            Self::Http { status: 403, .. } => Some(
                "The account cannot access this request or model. Check account permissions, or choose another model with /model.",
            ),
            Self::Http { status: 404, .. } => Some(
                "The selected model or provider endpoint is no longer available. Choose an available target with /model or /provider, then retry.",
            ),
            Self::Transport { .. } => Some(
                "The provider could not be reached after bounded retries. Check the network, provider status, and configured endpoint, then resume the session.",
            ),
            Self::Http { .. } | Self::Decode { .. } | Self::Configuration { .. } => None,
        }
    }
}

fn message_signals_quota_exhaustion(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    [
        "insufficient_quota",
        "insufficient quota",
        "credit balance is too low",
        "credits exhausted",
        "out of credits",
        "billing hard limit",
        "spend limit",
        "payment required",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
}

#[cfg(test)]
mod provider_failure_tests {
    use super::{ProviderFailure, ProviderLimitKind};

    fn status(error: &ProviderFailure) -> u16 {
        let ProviderFailure::Http { status, .. } = error else {
            panic!("expected HTTP failure")
        };
        *status
    }

    #[test]
    fn permanent_provider_failures_include_actionable_hints() {
        let unauthorized = ProviderFailure::Http {
            status: 401,
            message: "expired".to_string(),
            retry_after_secs: None,
        };
        let forbidden = ProviderFailure::Http {
            status: 403,
            message: "denied".to_string(),
            retry_after_secs: None,
        };
        let unavailable = ProviderFailure::Http {
            status: 503,
            message: "down".to_string(),
            retry_after_secs: None,
        };
        let removed_target = ProviderFailure::Http {
            status: 404,
            message: "model not found".to_string(),
            retry_after_secs: None,
        };
        let offline = ProviderFailure::transport("offline");

        assert!(
            unauthorized
                .user_action_hint()
                .unwrap()
                .contains("/authorize")
        );
        assert!(forbidden.user_action_hint().unwrap().contains("/model"));
        let target_hint = removed_target.user_action_hint().unwrap();
        assert!(target_hint.contains("/model"));
        assert!(target_hint.contains("/provider"));
        assert!(offline.user_action_hint().unwrap().contains("resume"));
        assert!(unavailable.user_action_hint().is_none());
    }

    #[test]
    fn from_stream_error_maps_transient_classes_to_retryable_statuses() {
        let overloaded = ProviderFailure::from_stream_error(Some("overloaded_error"), None, "x");
        assert_eq!(status(&overloaded), 529);
        assert!(overloaded.is_retryable());

        let rate_limited = ProviderFailure::from_stream_error(Some("rate_limit_error"), None, "x");
        assert_eq!(status(&rate_limited), 429);
        assert!(rate_limited.is_retryable());

        let server = ProviderFailure::from_stream_error(Some("api_error"), None, "x");
        assert_eq!(status(&server), 503);
        assert!(server.is_retryable());

        // A generic `type` with the specific class in `code` is still mapped —
        // OpenAI keys rate limits this way.
        let by_code =
            ProviderFailure::from_stream_error(Some("requests"), Some("rate_limit_exceeded"), "x");
        assert_eq!(status(&by_code), 429);
        assert!(by_code.is_retryable());
    }

    #[test]
    fn provider_limits_distinguish_transient_rate_limits_from_quota_walls() {
        let rate_limit = ProviderFailure::Http {
            status: 429,
            message: "requests per minute exceeded".to_string(),
            retry_after_secs: Some(10),
        };
        assert_eq!(rate_limit.limit_kind(), Some(ProviderLimitKind::RateLimit));
        assert!(rate_limit.is_retryable());

        for quota in [
            ProviderFailure::Http {
                status: 429,
                message: "code=insufficient_quota".to_string(),
                retry_after_secs: None,
            },
            ProviderFailure::Http {
                status: 402,
                message: "payment required".to_string(),
                retry_after_secs: None,
            },
        ] {
            assert_eq!(quota.limit_kind(), Some(ProviderLimitKind::Quota));
            assert!(!quota.is_retryable());
        }
    }

    #[test]
    fn from_stream_error_treats_unknown_and_permanent_classes_as_non_retryable() {
        let unknown = ProviderFailure::from_stream_error(None, None, "x");
        assert!(!unknown.is_retryable());

        let permanent =
            ProviderFailure::from_stream_error(Some("invalid_request_error"), None, "x");
        assert!(!permanent.is_retryable());
    }

    #[test]
    fn http_529_is_retryable() {
        let overloaded = ProviderFailure::Http {
            status: 529,
            message: "overloaded".to_string(),
            retry_after_secs: None,
        };
        assert!(overloaded.is_retryable());
    }

    #[test]
    fn transport_and_decode_are_retryable_but_configuration_is_not() {
        let transport = ProviderFailure::transport("Stream read error: connection reset");
        assert!(transport.is_retryable());
        assert!(ProviderFailure::stream_decode("truncated JSON").is_retryable());
        assert!(!ProviderFailure::configuration("missing API key").is_retryable());
    }
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Provider-reported prompt-cache counters for input tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct InputCacheUsage {
    pub read_tokens: u64,
    pub creation_tokens: u64,
    pub total_input_tokens: u64,
}

impl InputCacheUsage {
    pub const fn new(read_tokens: u64, creation_tokens: u64, total_input_tokens: u64) -> Self {
        Self {
            read_tokens,
            creation_tokens,
            total_input_tokens,
        }
    }

    pub fn hit_rate_percent(self) -> Option<u64> {
        self.read_tokens
            .saturating_mul(100)
            .checked_div(self.total_input_tokens)
    }
}

/// Ground-truth token counts reported by the provider for a single turn.
/// `None` on a `StreamedResponse` means the provider did not report usage.
///
/// # `prompt_tokens` convention
///
/// [`prompt_tokens`](Self::prompt_tokens) is the **full input count including
/// cached tokens**, uniform across every provider, so
/// `input_cache.total_input_tokens == prompt_tokens` whenever cache counters
/// were reported. Providers that report cache separately on the wire (Anthropic
/// reports a non-cached `input_tokens` plus cache read/creation) are normalized
/// to this convention in `usage.rs` before constructing `TokenUsage`.
///
/// The per-class split for cost lives in [`input_cache`](Self::input_cache);
/// `ModelPricing::cost_micros_for_usage` derives fresh input as
/// `prompt_tokens - cache_read - cache_creation` and prices each class at its
/// own rate. Context-pressure / compaction consumers can use `prompt_tokens`
/// (or the equivalent `input_cache.total_input_tokens`) directly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub input_cache: Option<InputCacheUsage>,
}

pub(crate) fn token_count_u32(value: Option<u64>) -> u32 {
    let Some(value) = value else {
        return 0;
    };
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// A process-wide shared HTTP client. `reqwest::Client` is `Arc`-backed, so
/// cloning is cheap and every clone shares one connection pool — avoiding a
/// fresh pool and TLS configuration per provider instance or catalog fetch.
pub(crate) fn http_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new).clone()
}

/// Mint a conversation-stable UUIDv7 cache-routing key for OpenAI-family
/// transports (`prompt_cache_key` on Chat Completions / Responses).
///
/// IMPORTANT CODEX ROUTING CONTRACT: the official Codex client uses UUIDv7 for
/// its thread and session identity. Live testing with a custom `bonsai-*`
/// header value left reuse pinned to the initial 14,080-token prefix; a valid
/// UUID let the next turns grow to 98.7%. Keep this standards-compliant format.
/// UUID is also a valid opaque `prompt_cache_key` for other OpenAI-compatible
/// providers, so this does not change their cache policy or request layout.
pub(crate) fn new_conversation_cache_key() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Return a valid, stable Codex thread/session ID for a persisted cache key.
///
/// New sessions already store UUIDv7. Sessions created by older Bonsai builds
/// keep their original `prompt_cache_key` so an existing warm route is not
/// discarded; only the Codex routing headers receive a deterministic UUID.
/// This compatibility mapping must remain stable across process resumes.
pub(crate) fn codex_routing_thread_id(conversation_key: &str) -> String {
    if let Ok(id) = uuid::Uuid::parse_str(conversation_key) {
        return id.to_string();
    }

    let digest = blake3::hash(conversation_key.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    // RFC 9562 name-based UUID shape. The digest supplies identity; setting
    // version/variant bits makes legacy keys acceptable anywhere that parses
    // Codex's thread headers as UUIDs.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// Scope a conversation cache key to one prompt lane.
///
/// One conversation multiplexes several system prompts over a single provider
/// instance: plan mode and coding mode swap personas in place, and subagent
/// traffic can ride the parent provider. OpenAI's automatic prompt caching
/// routes by `prompt_cache_key` plus the prompt head, so lanes sharing one key
/// while sending different prefixes collide on one cache route — forensics
/// measured the planning lane at 0% cache hits over five turns while
/// the coding lane ran at 50%. Suffixing the key with a fingerprint of the
/// lane's byte-stable instructions gives each lane its own stable route while
/// staying identical across that lane's turns. Deliberate — don't simplify
/// back to one key per conversation.
pub(crate) fn lane_scoped_cache_key(conversation_key: &str, lane_instructions: &str) -> String {
    if lane_instructions.is_empty() {
        return conversation_key.to_string();
    }
    let lane = blake3::hash(lane_instructions.as_bytes()).to_hex();
    format!("{conversation_key}-{}", &lane[..8])
}

#[cfg(test)]
mod cache_key_tests {
    use super::{codex_routing_thread_id, lane_scoped_cache_key, new_conversation_cache_key};

    #[test]
    fn new_cache_keys_are_uuid_v7() {
        let first = uuid::Uuid::parse_str(&new_conversation_cache_key()).unwrap();
        let second = uuid::Uuid::parse_str(&new_conversation_cache_key()).unwrap();

        assert_eq!(first.get_version_num(), 7);
        assert_eq!(second.get_version_num(), 7);
        assert_ne!(first, second);
    }

    #[test]
    fn legacy_cache_keys_map_to_stable_valid_codex_thread_ids() {
        let legacy = "bonsai-18c1f4470ea6a260-0";
        let first = codex_routing_thread_id(legacy);

        assert_eq!(first, codex_routing_thread_id(legacy));
        assert!(uuid::Uuid::parse_str(&first).is_ok());
        assert_ne!(first, codex_routing_thread_id("bonsai-another-session"));
    }

    #[test]
    fn uuid_cache_keys_are_used_directly_as_codex_thread_ids() {
        let key = "01990abc-1234-7def-8123-456789abcdef";
        assert_eq!(codex_routing_thread_id(key), key);
    }

    #[test]
    fn lane_scoped_cache_key_is_stable_per_lane_and_distinct_across_lanes() {
        let planning = lane_scoped_cache_key("bonsai-abc", "planning persona");
        let coding = lane_scoped_cache_key("bonsai-abc", "coding persona");
        assert_eq!(
            planning,
            lane_scoped_cache_key("bonsai-abc", "planning persona"),
            "same lane must map to the same key across turns",
        );
        assert_ne!(
            planning, coding,
            "different lanes must not share a cache route",
        );
        assert!(planning.starts_with("bonsai-abc-"));
    }

    #[test]
    fn lane_scoped_cache_key_without_instructions_falls_back_to_conversation_key() {
        assert_eq!(lane_scoped_cache_key("bonsai-abc", ""), "bonsai-abc");
    }
}

/// Why a provider ended a streamed generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    GenerationBudget,
    ContentFilter,
    Other(String),
}

/// Typed terminal state for one provider stream.
///
/// A transport must only construct [`Completed`](Self::Completed) or
/// [`Incomplete`](Self::Incomplete) after observing its protocol's terminal
/// signal. Cancellation is represented separately so callers cannot mistake a
/// partial response for a completed turn.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamTerminal {
    Completed(FinishReason),
    Incomplete(FinishReason),
    Interrupted,
}

impl StreamTerminal {
    pub(crate) fn from_finish_reason(reason: FinishReason) -> Self {
        if matches!(
            reason,
            FinishReason::Length | FinishReason::GenerationBudget
        ) {
            Self::Incomplete(reason)
        } else {
            Self::Completed(reason)
        }
    }

    pub(crate) const fn finish_reason(&self) -> Option<&FinishReason> {
        match self {
            Self::Completed(reason) | Self::Incomplete(reason) => Some(reason),
            Self::Interrupted => None,
        }
    }

    pub(crate) const fn is_interrupted(&self) -> bool {
        matches!(self, Self::Interrupted)
    }
}

impl Default for StreamTerminal {
    fn default() -> Self {
        Self::Completed(FinishReason::Stop)
    }
}

impl FinishReason {
    pub(crate) fn from_openai(value: &str) -> Self {
        match value {
            "stop" | "end_turn" | "stop_sequence" => Self::Stop,
            "tool_calls" | "tool_use" => Self::ToolCalls,
            "length" | "max_tokens" | "max_output_tokens" => Self::Length,
            "generation_budget" => Self::GenerationBudget,
            "content_filter" | "refusal" => Self::ContentFilter,
            other => Self::Other(other.to_string()),
        }
    }

    pub(crate) fn as_db_str(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::ToolCalls => "tool_calls",
            Self::Length => "length",
            Self::GenerationBudget => "generation_budget",
            Self::ContentFilter => "content_filter",
            Self::Other(value) => value,
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Self {
        Self::from_openai(value)
    }
}

/// Per-call overrides that do not mutate the conversation's saved model state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderRequestOptions {
    pub reasoning: Option<ReasoningSelection>,
}

impl ProviderRequestOptions {
    pub const fn with_reasoning(reasoning: ReasoningSelection) -> Self {
        Self {
            reasoning: Some(reasoning),
        }
    }
}

#[derive(Debug, Default)]
pub struct StreamedResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub terminal: StreamTerminal,
    /// Real token usage from the provider, when reported.
    pub usage: Option<TokenUsage>,
    /// Unicode scalar count streamed through the reasoning channel.
    pub reasoning_chars: usize,
}

impl StreamedResponse {
    /// An empty response marking a turn cancelled before any output arrived.
    pub fn interrupted() -> Self {
        Self {
            terminal: StreamTerminal::Interrupted,
            ..Self::default()
        }
    }

    pub(crate) const fn is_interrupted(&self) -> bool {
        self.terminal.is_interrupted()
    }

    pub(crate) const fn finish_reason(&self) -> Option<&FinishReason> {
        self.terminal.finish_reason()
    }
}

/// Build and serialize a provider request body, shared across all wire
/// transports so their stream loops focus on protocol differences.
///
/// Every `chat_stream_with_options` opens with this identical prologue:
/// authorize → build the request body → capture a wire preview → serialize
/// to JSON. The three steps are provider-specific methods; the error mapping
/// and diagnostics capture are identical.
pub(crate) fn serialize_request_body(
    provider_label: &str,
    authorize: impl FnOnce() -> Result<()>,
    build_body: impl FnOnce() -> Result<serde_json::Value>,
    preview_from: impl FnOnce(serde_json::Value) -> ProviderRequestPreview,
    diagnostics_slot: &std::sync::Mutex<Option<ProviderRequestDiagnostics>>,
) -> ProviderResult<Vec<u8>> {
    authorize().map_err(|error| ProviderFailure::configuration(error.to_string()))?;
    let body = build_body().map_err(|error| ProviderFailure::configuration(error.to_string()))?;
    let preview = preview_from(body);
    ProviderRequestDiagnostics::capture(preview, diagnostics_slot).map_err(|error| {
        ProviderFailure::configuration(format!(
            "Failed to serialize {provider_label} request body: {error}"
        ))
    })
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Select the provider's project-state cache representation.
    ///
    /// IMPORTANT CACHE COMPATIBILITY CONTRACT: the default deliberately keeps
    /// the historic Chat Completions/Anthropic behavior. Do not change the
    /// default globally. Only providers whose live backend requires exact
    /// append-only history (currently Codex Responses and OpenCode's declared
    /// rolling-history layout) should override it.
    fn project_state_cache_strategy(&self) -> ProjectStateCacheStrategy {
        ProjectStateCacheStrategy::MutableSystemTail
    }

    /// Replace the cache-routing identity for this conversation.
    ///
    /// OpenAI-family providers override this so a persisted Bonsai session can
    /// keep the same warm cache route after resume or a provider rebuild. Other
    /// transports intentionally ignore it.
    fn set_conversation_cache_key(&mut self, _key: &str) {}

    /// Reasoning selection baked into this provider's current run target.
    fn reasoning(&self) -> ReasoningSelection {
        ReasoningSelection::Default
    }

    /// One stronger request-local effort advertised by the active model.
    /// Returns `None` when the provider cannot prove a portable escalation.
    fn reasoning_escalation(&self) -> Option<ReasoningSelection> {
        None
    }

    #[cfg(test)]
    fn preview_request(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
    ) -> Result<ProviderRequestPreview> {
        let body = json!({
            "messages": messages,
            "tools": tools,
        });
        Ok(ProviderRequestPreview::with_wire_sections(
            "POST",
            "provider request",
            body.clone(),
            wire_sections_from_body(
                &body,
                &[
                    WireField {
                        id: "wire-messages",
                        label: "Messages",
                        key: "messages",
                    },
                    WireField {
                        id: "wire-tools",
                        label: "Tools",
                        key: "tools",
                    },
                ],
                false,
            ),
        ))
    }

    /// Remove diagnostics captured from the most recent JSON request body.
    /// Providers that do not own a JSON transport may leave this unsupported.
    fn take_last_request_diagnostics(&self) -> Option<ProviderRequestDiagnostics> {
        None
    }

    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        cancellation_token: tokio_util::sync::CancellationToken,
        sink: SharedSink,
    ) -> ProviderResult<StreamedResponse>;

    /// Stream with request-local overrides. Providers that do not expose
    /// reasoning controls inherit the normal request unchanged.
    async fn chat_stream_with_options(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: ProviderRequestOptions,
        cancellation_token: tokio_util::sync::CancellationToken,
        sink: SharedSink,
    ) -> ProviderResult<StreamedResponse> {
        let _ = options;
        self.chat_stream(messages, tools, cancellation_token, sink)
            .await
    }

    async fn list_models(&self) -> Result<Vec<String>>;
}

/// Construct the concrete [`Provider`] for a resolved run target. This is the
/// single place the transport↔implementation mapping lives, so adding a
/// provider on an existing transport needs no new provider type.
pub(crate) fn provider_for(
    metadata: &ProviderMetadata,
    session: &ProviderSession,
    target: &RunTarget,
) -> Box<dyn Provider> {
    match target.transport {
        TransportProtocol::OpenAiChat => {
            Box::new(opencode::OpenAiCompatibleProvider::with_api_key_policy(
                metadata.id.as_ref(),
                session,
                target,
                match metadata.auth_requirement {
                    AuthRequirement::ApiKeyOptional => opencode::ApiKeyPolicy::Optional,
                    AuthRequirement::ApiKeyRequired | AuthRequirement::CodexCache => {
                        opencode::ApiKeyPolicy::Required
                    }
                },
                metadata.capabilities,
            ))
        }
        TransportProtocol::AnthropicMessages => {
            Box::new(anthropic::AnthropicCompatibleProvider::new(
                metadata.id.as_ref(),
                session,
                target,
                anthropic::AnthropicTransportFlags {
                    require_api_key: metadata.auth_requirement != AuthRequirement::ApiKeyOptional,
                    supports_prompt_cache: metadata.capabilities.supports_prompt_cache,
                },
                metadata.raw_api_key_header_name(),
            ))
        }
        TransportProtocol::CodexResponses => Box::new(codex::CodexProvider::new(session, target)),
    }
}

pub(crate) fn fallback_run_target(
    metadata: &ProviderMetadata,
    session: &ProviderSession,
) -> RunTarget {
    let connection_id = crate::model_catalog::ConnectionId::fallback(&metadata.id);
    let remote_model_id: Box<str> = if session.model.trim().is_empty() {
        metadata.default_model.as_ref().into()
    } else {
        session.model.trim().into()
    };
    let model_id = remote_model_id
        .as_ref()
        .parse()
        .unwrap_or_else(|_err| crate::model_catalog::ModelId::fallback(&connection_id));
    let base_url = if session.base_url.trim().is_empty() {
        metadata.default_base_url.as_ref().into()
    } else {
        session.base_url.trim().trim_end_matches('/').into()
    };
    let transport = TransportProtocol::from(metadata.protocol);
    let reasoning = session
        .model_reasoning
        .get(session.model.as_str())
        .copied()
        .unwrap_or(session.reasoning);
    let reasoning = if metadata.capabilities.supports_reasoning(reasoning) {
        reasoning
    } else {
        crate::provider::ReasoningSelection::default()
    };
    let reasoning_escalation = reasoning.next_higher_supported(metadata.capabilities.reasoning);
    RunTarget {
        connection_id,
        model_id,
        remote_model_id,
        base_url,
        transport,
        prompt_cache_policy: crate::model_catalog::PromptCachePolicy::TransportDefault,
        reasoning_codec: crate::provider::ReasoningCodec::default_for_transport(transport),
        endpoint_path: (!metadata.endpoint_path.is_empty())
            .then(|| metadata.endpoint_path.as_ref().into()),
        context_window: session.context_window.or(metadata.context_window),
        output_limit: None,
        reasoning,
        reasoning_escalation,
        use_responses_lite: false,
    }
}

#[async_trait]
pub trait ProviderFactory: Send + Sync {
    fn metadata(&self) -> &ProviderMetadata;

    fn build_target(&self, session: &ProviderSession, target: &RunTarget) -> Box<dyn Provider> {
        provider_for(self.metadata(), session, target)
    }

    async fn authorize(&self, input: AuthInput) -> Result<AuthorizeOutcome> {
        match self.metadata().auth_requirement {
            AuthRequirement::ApiKeyRequired => auth::authorize_api_key(self.metadata(), input),
            AuthRequirement::ApiKeyOptional => {
                anyhow::bail!(
                    "{} requires endpoint authorization setup",
                    self.metadata().display_name
                )
            }
            AuthRequirement::CodexCache => {
                anyhow::bail!(
                    "{} requires Codex cache authorization",
                    self.metadata().display_name
                )
            }
        }
    }

    fn is_authorized(&self, session: &ProviderSession) -> bool {
        auth::is_authorized(self.metadata(), session)
    }

    fn clear_authorization(&self, session: &mut ProviderSession) {
        auth::clear_api_key(session);
    }

    async fn list_models(&self, session: &ProviderSession) -> Result<Vec<String>>;

    async fn list_available_models(
        &self,
        session: &ProviderSession,
    ) -> Result<LiveModelAvailability> {
        let models = self.list_models(session).await?;
        Ok(LiveModelAvailability::from_remote_ids(models))
    }

    /// Perform a real, read-only provider request for release diagnostics.
    /// Implementations with offline model-list fallbacks override this so a
    /// fallback cannot be mistaken for network reachability.
    async fn probe_reachability(&self, session: &ProviderSession) -> Result<()> {
        self.list_available_models(session).await.map(|_| ())
    }
}
