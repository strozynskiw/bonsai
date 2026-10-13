use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::model_catalog::{AvailableModel, LiveModelAvailability, ModelFeature, RunTarget};
use crate::output::SharedSink;
use crate::provider::reasoning::{ReasoningCodec, anthropic_adaptive_effort};
use crate::provider::think_tags::ThinkTagSplitter;
use crate::provider::transform::{self, ContentPart};
use crate::provider::{
    ANTHROPIC_PARAMETERS, NO_REASONING, Protocol, Provider, ProviderCapabilities, ProviderFactory,
    ProviderMetadata, ProviderRequestDiagnostics, ProviderRequestPreview, ReasoningSelection,
    StreamedResponse, TokenCounterKind, ToolCall, WireField, sse, streaming, usage,
    wire_sections_from_body,
};
use crate::session::ProviderSession;
use crate::util::tool_args::tool_call_arguments_value;

pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 16_000;
const THINKING_BUDGET_HEADROOM_TOKENS: u32 = 1_024;
const ANTHROPIC_MODELS_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
/// `GET /v1/models` pages at most this many times (1,000 rows each) so a
/// misbehaving gateway cannot loop the refresh forever.
const ANTHROPIC_MODELS_MAX_PAGES: usize = 5;

pub static ANTHROPIC_METADATA: LazyLock<ProviderMetadata> = LazyLock::new(|| {
    ProviderMetadata::new(
        "anthropic",
        "Anthropic API",
        "claude-sonnet-5",
        "https://api.anthropic.com",
        Some("ANTHROPIC_API_KEY"),
        Some("ANTHROPIC_MODEL"),
        Some("ANTHROPIC_BASE_URL"),
        // Offline fallback when `GET /v1/models` is unreachable; the live listing
        // supersedes this whenever a key works. Keep in sync with anthropic.toml.
        &[
            "claude-sonnet-4-5",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5",
            "claude-fable-5",
            "claude-fable-5-1",
            "claude-haiku-4-5",
            "claude-opus-4-1",
        ],
        Protocol::AnthropicMessages,
        ProviderCapabilities::new(NO_REASONING, ANTHROPIC_PARAMETERS)
            .with_prompt_cache()
            .with_vision(),
        "v1/messages",
    )
    .with_context_window(200_000)
    .with_token_counter(TokenCounterKind::AnthropicCountTokens)
});

pub struct AnthropicFactory;

#[async_trait]
impl ProviderFactory for AnthropicFactory {
    fn metadata(&self) -> &ProviderMetadata {
        &ANTHROPIC_METADATA
    }

    async fn list_models(&self, session: &ProviderSession) -> Result<Vec<String>> {
        Ok(self
            .list_available_models(session)
            .await?
            .remote_model_ids())
    }

    async fn list_available_models(
        &self,
        session: &ProviderSession,
    ) -> Result<LiveModelAvailability> {
        // No key yet: the static seed lineup is the best available answer —
        // the picker must still show something before authorization.
        if session.api_key.trim().is_empty() {
            return Ok(LiveModelAvailability::from_remote_ids(
                self.metadata().seed_model_list(),
            ));
        }
        // With a key, a fetch failure propagates as Err so the refresh path
        // keeps an existing (richer) live cache instead of overwriting it
        // with bare seed rows.
        let target = crate::provider::fallback_run_target(self.metadata(), session);
        let availability = fetch_anthropic_models(session, &target).await?;
        if availability.models.is_empty() {
            // An authorized listing that comes back empty is a server quirk,
            // not "no models" — keep the seed lineup usable.
            return Ok(LiveModelAvailability::from_remote_ids(
                self.metadata().seed_model_list(),
            ));
        }
        Ok(availability)
    }
}

/// Live model discovery against `GET /v1/models`. The endpoint reports
/// model limits and capabilities per model, so live data corrects the static
/// catalog whenever they disagree — new models appear with a runnable request
/// shape without a binary update, and retired ones drop out.
async fn fetch_anthropic_models(
    session: &ProviderSession,
    target: &RunTarget,
) -> Result<LiveModelAvailability> {
    let api_key = session.api_key.trim().to_string();
    if api_key.is_empty() {
        anyhow::bail!("Anthropic model listing requires ANTHROPIC_API_KEY");
    }
    let base_url = target.base_url.trim().trim_end_matches('/').to_string();
    let http = crate::provider::http_client();

    let mut models = Vec::new();
    let mut after_id: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    for _page in 0..ANTHROPIC_MODELS_MAX_PAGES {
        let mut request = http
            .get(transform::endpoint_url(&base_url, "v1/models"))
            .header("x-api-key", &api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("Accept", "application/json")
            .query(&[("limit", "1000")]);
        if let Some(after_id) = &after_id {
            request = request.query(&[("after_id", after_id.as_str())]);
        }

        let response = timeout(ANTHROPIC_MODELS_REFRESH_TIMEOUT, request.send())
            .await
            .context("Timed out listing Anthropic models")?
            .context("Failed to list Anthropic models")?;
        if !response.status().is_success() {
            return Err(sse::error_from_response(response).await.into());
        }
        let value: Value = response
            .json()
            .await
            .context("Failed to parse Anthropic models")?;
        let page = parse_anthropic_models_page(value)?;

        models.extend(page.models);
        if !page.has_more {
            return Ok(LiveModelAvailability {
                models,
                ..LiveModelAvailability::default()
            });
        }
        let last_id = page
            .last_id
            .filter(|last_id| !last_id.trim().is_empty())
            .context("Anthropic models response has `has_more` without a `last_id`")?;
        if !seen_cursors.insert(last_id.clone()) {
            anyhow::bail!("Anthropic models response repeated cursor `{last_id}`");
        }
        after_id = Some(last_id);
    }

    anyhow::bail!("Anthropic model list exceeded {ANTHROPIC_MODELS_MAX_PAGES} pages")
}

#[derive(Debug, serde::Deserialize)]
struct AnthropicModelsPage {
    #[serde(default)]
    data: Vec<AnthropicModelRow>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

/// Transport-neutral result of parsing one Anthropic Models API page.
/// Compatible connections reuse this so limits and capabilities cannot drift
/// from the first-party provider implementation.
#[derive(Debug)]
pub(crate) struct ParsedAnthropicModelsPage {
    pub(crate) models: Vec<AvailableModel>,
    pub(crate) has_more: bool,
    pub(crate) last_id: Option<String>,
}

/// Parse one Anthropic Models API page into catalog metadata.
pub(crate) fn parse_anthropic_models_page(value: Value) -> Result<ParsedAnthropicModelsPage> {
    let page: AnthropicModelsPage =
        serde_json::from_value(value).context("Failed to parse Anthropic models")?;
    Ok(ParsedAnthropicModelsPage {
        models: page
            .data
            .into_iter()
            .map(AnthropicModelRow::into_available)
            .collect(),
        has_more: page.has_more,
        last_id: page.last_id,
    })
}

#[derive(Debug, serde::Deserialize)]
struct AnthropicModelRow {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    /// Context window; reported by api.anthropic.com, absent on older
    /// Anthropic-compatible gateways.
    #[serde(default)]
    max_input_tokens: Option<i64>,
    /// Maximum generated tokens; absent on older Anthropic-compatible
    /// gateways.
    #[serde(default)]
    max_tokens: Option<i64>,
    /// Rich capability metadata returned by the first-party Models API.
    #[serde(default)]
    capabilities: Option<AnthropicModelCapabilities>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AnthropicModelCapabilities {
    #[serde(default)]
    effort: Option<AnthropicEffortCapability>,
    #[serde(default)]
    image_input: Option<AnthropicCapabilitySupport>,
    #[serde(default)]
    pdf_input: Option<AnthropicCapabilitySupport>,
    #[serde(default)]
    structured_outputs: Option<AnthropicCapabilitySupport>,
    #[serde(default)]
    thinking: Option<AnthropicThinkingCapability>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AnthropicCapabilitySupport {
    #[serde(default)]
    supported: bool,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AnthropicEffortCapability {
    #[serde(default)]
    supported: bool,
    #[serde(default)]
    low: AnthropicCapabilitySupport,
    #[serde(default)]
    medium: AnthropicCapabilitySupport,
    #[serde(default)]
    high: AnthropicCapabilitySupport,
    #[serde(default)]
    xhigh: AnthropicCapabilitySupport,
    #[serde(default)]
    max: AnthropicCapabilitySupport,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AnthropicThinkingCapability {
    #[serde(default)]
    supported: bool,
    #[serde(default)]
    types: AnthropicThinkingTypes,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AnthropicThinkingTypes {
    #[serde(default)]
    adaptive: AnthropicCapabilitySupport,
    #[serde(default)]
    enabled: AnthropicCapabilitySupport,
}

#[derive(Debug, Default)]
struct AnthropicLiveMetadata {
    features: Vec<ModelFeature>,
    reasoning: Vec<ReasoningSelection>,
    recommended_reasoning: Option<ReasoningSelection>,
    reasoning_codec: Option<ReasoningCodec>,
}

impl AnthropicModelCapabilities {
    fn into_live_metadata(self, model_id: &str) -> AnthropicLiveMetadata {
        let mut metadata = AnthropicLiveMetadata {
            // Client-defined tool use is part of the Messages API contract but
            // is not repeated in the model capability object.
            features: vec![ModelFeature::ToolCall],
            ..AnthropicLiveMetadata::default()
        };
        if self.image_input.is_some_and(|value| value.supported)
            || self.pdf_input.is_some_and(|value| value.supported)
        {
            metadata.features.push(ModelFeature::Attachment);
        }
        if self.structured_outputs.is_some_and(|value| value.supported) {
            metadata.features.push(ModelFeature::StructuredOutput);
        }

        let thinking_supported = self
            .thinking
            .as_ref()
            .is_some_and(|thinking| thinking.supported);
        let adaptive_thinking = self
            .thinking
            .as_ref()
            .is_some_and(|thinking| thinking.types.adaptive.supported);
        let enabled_thinking = self
            .thinking
            .as_ref()
            .is_some_and(|thinking| thinking.types.enabled.supported);
        let effort_supported = self.effort.as_ref().is_some_and(|effort| effort.supported);
        if thinking_supported || effort_supported {
            metadata.features.push(ModelFeature::Reasoning);
        }

        if let Some(effort) = self.effort.as_ref().filter(|effort| effort.supported) {
            if !anthropic_thinking_is_always_on(model_id) {
                metadata.reasoning.push(ReasoningSelection::Off);
            }
            for (supported, selection) in [
                (effort.low.supported, ReasoningSelection::Low),
                (effort.medium.supported, ReasoningSelection::Medium),
                (effort.high.supported, ReasoningSelection::High),
                (effort.xhigh.supported, ReasoningSelection::XHigh),
                (effort.max.supported, ReasoningSelection::Max),
            ] {
                if supported {
                    metadata.reasoning.push(selection);
                }
            }
            metadata.recommended_reasoning = metadata
                .reasoning
                .contains(&ReasoningSelection::High)
                .then_some(ReasoningSelection::High);
        }
        if adaptive_thinking {
            metadata.reasoning_codec = Some(ReasoningCodec::AnthropicAdaptive);
        } else if enabled_thinking && effort_supported {
            metadata.reasoning_codec = Some(ReasoningCodec::AnthropicThinkingWithEffort);
        }
        metadata
    }
}

fn anthropic_thinking_is_always_on(model_id: &str) -> bool {
    model_id.starts_with("claude-fable-5") || model_id.starts_with("claude-mythos-5")
}

impl AnthropicModelRow {
    fn into_available(self) -> AvailableModel {
        let context_window = self
            .max_input_tokens
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0);
        let output_limit = self
            .max_tokens
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0);
        let metadata = self
            .capabilities
            .map(|capabilities| capabilities.into_live_metadata(&self.id))
            .unwrap_or_default();
        let mut model = AvailableModel::with_metadata(
            self.id,
            context_window,
            self.display_name,
            metadata.features,
        )
        .with_output_limit(output_limit);
        if !metadata.reasoning.is_empty() {
            model = model.with_reasoning(metadata.reasoning, metadata.recommended_reasoning);
        }
        if let Some(reasoning_codec) = metadata.reasoning_codec {
            model = model.with_reasoning_codec(reasoning_codec);
        }
        model
    }
}

pub struct AnthropicCompatibleProvider {
    provider_id: String,
    http: reqwest::Client,
    model: String,
    base_url: String,
    api_key: String,
    /// Header the raw API key is sent under. Usually `x-api-key`; a connection
    /// may override it (e.g. Xiaomi MiMo requires a bare `api-key` header and
    /// 401s on `x-api-key`).
    api_key_header: String,
    endpoint_path: String,
    max_tokens: u32,
    output_limit: Option<u32>,
    reasoning: ReasoningSelection,
    reasoning_escalation: Option<ReasoningSelection>,
    /// Which Anthropic thinking wire shape this model takes. Budget-tokens
    /// models (claude-sonnet-4-5 and earlier, MiniMax, local endpoints) use
    /// [`ReasoningCodec::AnthropicThinking`]; the adaptive generation
    /// (claude-sonnet-4-6+/opus-4-7+/sonnet-5/fable-5) 400s on budget payloads
    /// and uses [`ReasoningCodec::AnthropicAdaptive`]. Sourced from the
    /// catalog target so the per-model TOML decides.
    reasoning_codec: ReasoningCodec,
    /// Whether a non-empty API key is mandatory. `false` for providers with an
    /// optional key (`anthropic-compatible`), so a keyless local endpoint that
    /// `auth::is_authorized` already accepts can also issue chat requests.
    require_api_key: bool,
    /// Whether to emit `cache_control` prompt-cache breakpoints in the request
    /// body. Sourced from the provider's [`ProviderCapabilities`].
    supports_prompt_cache: bool,
    /// Whether the active model accepts image content. When false, image
    /// parts are downgraded to a text placeholder before serialization so an
    /// image already in history cannot 400 every later turn.
    supports_vision: bool,
    /// Connection/model-specific placement of mutable project state relative
    /// to the rolling history breakpoint.
    prompt_cache_policy: crate::model_catalog::PromptCachePolicy,
    native_replay: crate::provider::NativeReplayStore,
    native_replay_scope: String,
    last_request_diagnostics: Mutex<Option<ProviderRequestDiagnostics>>,
    tool_call_identities: crate::provider::ToolCallIdentityStore,
    tool_call_scope: String,
}

/// Transport behavior toggles resolved from provider metadata. A named struct
/// rather than two adjacent positional bools: transposing auth-required and
/// prompt-cache at a call site would compile silently and flip both behaviors.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AnthropicTransportFlags {
    pub(crate) require_api_key: bool,
    pub(crate) supports_prompt_cache: bool,
    pub(crate) supports_vision: bool,
}

impl AnthropicCompatibleProvider {
    pub(crate) fn new(
        provider_id: impl Into<String>,
        session: &ProviderSession,
        target: &RunTarget,
        flags: AnthropicTransportFlags,
        api_key_header: impl Into<String>,
    ) -> Self {
        let AnthropicTransportFlags {
            require_api_key,
            supports_prompt_cache,
            supports_vision,
        } = flags;
        let base_url = target.base_url.trim().trim_end_matches('/').to_string();
        let model = target.remote_model_id.to_string();
        let provider_id = provider_id.into();
        let endpoint_path = target
            .endpoint_path
            .as_deref()
            .unwrap_or("v1/messages")
            .to_string();
        let tool_call_scope = crate::provider::tool_call_scope(
            "anthropic-messages",
            &provider_id,
            &base_url,
            &endpoint_path,
        );
        // Only these codecs have an Anthropic wire representation.
        let reasoning_codec = match target.reasoning_codec {
            ReasoningCodec::AnthropicAdaptive | ReasoningCodec::AnthropicThinkingWithEffort => {
                target.reasoning_codec
            }
            _ => ReasoningCodec::AnthropicThinking,
        };
        let native_replay_scope = crate::provider::tool_call_scope(
            "anthropic-messages-native",
            &tool_call_scope,
            &model,
            &format!("{reasoning_codec:?}"),
        );
        Self {
            provider_id,
            http: crate::provider::http_client(),
            model,
            base_url,
            api_key: session.api_key.clone(),
            api_key_header: api_key_header.into(),
            endpoint_path,
            max_tokens: target.output_limit.unwrap_or(DEFAULT_MAX_TOKENS),
            output_limit: target.output_limit,
            reasoning: target.reasoning,
            reasoning_escalation: target.reasoning_escalation,
            reasoning_codec,
            require_api_key,
            supports_prompt_cache,
            supports_vision,
            prompt_cache_policy: target.prompt_cache_policy,
            native_replay: crate::provider::NativeReplayStore::default(),
            native_replay_scope,
            last_request_diagnostics: Mutex::new(None),
            tool_call_identities: crate::provider::ToolCallIdentityStore::default(),
            tool_call_scope,
        }
    }

    fn ensure_authorized(&self) -> Result<()> {
        transform::ensure_authorized(
            self.provider_id.as_str(),
            &self.api_key,
            &self.base_url,
            &self.model,
            self.require_api_key,
        )
    }

    fn messages_endpoint(&self) -> String {
        transform::endpoint_url(&self.base_url, &self.endpoint_path)
    }

    fn thinking_payload_for(&self, reasoning: ReasoningSelection) -> Option<Value> {
        self.reasoning_codec.encode_json(reasoning)
    }

    fn thinking_payload_and_max_tokens_for(
        &self,
        reasoning: ReasoningSelection,
    ) -> (Option<Value>, u32) {
        let Some(mut thinking) = self.thinking_payload_for(reasoning) else {
            return (None, self.max_tokens);
        };
        let Some(budget_tokens) = thinking
            .get("budget_tokens")
            .and_then(Value::as_u64)
            .and_then(|tokens| u32::try_from(tokens).ok())
        else {
            return (Some(thinking), self.max_tokens);
        };

        let desired_max_tokens = budget_tokens
            .saturating_add(THINKING_BUDGET_HEADROOM_TOKENS)
            .max(budget_tokens.saturating_add(1));
        let mut max_tokens = self.max_tokens.max(desired_max_tokens);
        if let Some(output_limit) = self.output_limit {
            max_tokens = max_tokens.min(output_limit);
        }

        if budget_tokens < max_tokens {
            return (Some(thinking), max_tokens);
        }

        let Some(adjusted_budget) = budget_below_max_tokens(max_tokens) else {
            tracing::warn!(
                model = %self.model,
                requested_budget_tokens = budget_tokens,
                max_tokens,
                "omitting Anthropic thinking because max_tokens cannot exceed budget_tokens"
            );
            return (None, max_tokens);
        };
        tracing::warn!(
            model = %self.model,
            requested_budget_tokens = budget_tokens,
            adjusted_budget_tokens = adjusted_budget,
            max_tokens,
            "shrinking Anthropic thinking budget below max_tokens"
        );
        thinking["budget_tokens"] = json!(adjusted_budget);
        (Some(thinking), max_tokens)
    }

    fn api_key_header_name(&self) -> &str {
        &self.api_key_header
    }

    /// Whether to split inline `<think>…</think>` reasoning out of the text
    /// channel. Real Anthropic (`claude-*`) streams reasoning on a dedicated
    /// `thinking_delta` channel and may legitimately print a literal `<think>`
    /// inside a code block, so it is excluded. Compatible endpoints can emit a
    /// leading inline block; observing a native thinking block disables that
    /// fallback before any answer text is parsed.
    fn splits_inline_think_tags(&self) -> bool {
        self.provider_id != ANTHROPIC_METADATA.id.as_ref()
    }

    #[cfg(test)]
    fn request_body(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
    ) -> Result<Value> {
        self.request_body_with_reasoning(messages, tools, self.reasoning)
    }

    #[cfg(test)]
    fn request_body_with_reasoning(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        reasoning: ReasoningSelection,
    ) -> Result<Value> {
        self.request_body_with_context(messages, tools, reasoning, &Default::default())
    }

    fn request_body_with_context(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        reasoning: ReasoningSelection,
        context: &crate::provider::ProviderRequestContext,
    ) -> Result<Value> {
        let pinned_settings = self.tool_turn_settings(messages, context)?;
        let reasoning = pinned_settings
            .as_ref()
            .map_or(reasoning, |settings| settings.reasoning);
        // IMPORTANT CACHE COMPATIBILITY: native Anthropic and MiniMax Coding
        // Plan retain their verified system-tail layout. OpenCode opts into a
        // rolling-history layout in catalog config because its read-coverage
        // tail changes after virtually every tool call. Do not globalize that
        // provider-specific ordering or remove the declarative selection.
        let project_state_layout = if self.prompt_cache_policy.uses_append_only_project_state() {
            transform::ProjectStateWireLayout::LatestAtEnd
        } else {
            transform::ProjectStateWireLayout::SystemTail
        };
        let contiguous = transform::restore_tool_result_contiguity(messages);
        let wire_messages =
            transform::messages_for_project_state_layout(contiguous.as_ref(), project_state_layout);
        // Image safety net: mirror of the OpenAI-chat sanitize — a text-only
        // Anthropic-compatible endpoint rejects image blocks with a 400, an
        // unsupported media type (e.g. `image/svg+xml`) 400s even vision
        // models, and images already in history would otherwise wedge every
        // later turn.
        let wire_messages =
            transform::sanitize_image_parts_for_wire(wire_messages.as_ref(), self.supports_vision);
        let (system, mut anthropic_messages) = transform_messages_with_replay(
            wire_messages.as_ref(),
            &self.native_replay,
            &self.native_replay_scope,
            context,
        )?;
        let mut tools = transform_tools(tools)?;
        // Prompt-cache breakpoints (Anthropic allows 4): the tools prefix, the
        // byte-stable system prefix, and the whole prior-history prefix. The
        // last is a rolling breakpoint — it caches everything up to the current
        // turn so the next turn reads it back.
        if self.supports_prompt_cache {
            mark_last_tool_cacheable(&mut tools);
            if self.prompt_cache_policy.uses_append_only_project_state() {
                mark_rolling_history_cacheable(&mut anthropic_messages);
            } else {
                mark_last_message_cacheable(&mut anthropic_messages);
            }
        }
        let (mut thinking, mut max_tokens) = self.thinking_payload_and_max_tokens_for(reasoning);
        let mut output_config = matches!(
            self.reasoning_codec,
            ReasoningCodec::AnthropicAdaptive | ReasoningCodec::AnthropicThinkingWithEffort
        )
        .then(|| anthropic_adaptive_effort(reasoning))
        .flatten()
        .map(|effort| json!({"effort": effort}));
        if let Some(settings) = pinned_settings {
            thinking = settings.thinking;
            output_config = settings.output_config;
            if let Some(budget) = thinking
                .as_ref()
                .and_then(|value| value.get("budget_tokens"))
                .and_then(Value::as_u64)
            {
                let desired_max = u32::try_from(budget.saturating_add(1))
                    .context("Anthropic tool-turn thinking budget exceeds output limits")?;
                max_tokens = max_tokens.max(desired_max);
                if let Some(limit) = self.output_limit {
                    max_tokens = max_tokens.min(limit);
                }
                if budget >= u64::from(max_tokens) {
                    anyhow::bail!(
                        "Anthropic output limit cannot preserve the current tool-turn thinking budget of {budget} tokens"
                    );
                }
            }
        }
        let mut body = json!({
            "model": self.model,
            "system": system_field(&system, self.supports_prompt_cache),
            "messages": anthropic_messages,
            "tools": tools,
            "max_tokens": max_tokens,
            "stream": true,
        });
        if let Some(thinking) = thinking {
            body["thinking"] = thinking;
        }
        if let Some(output_config) = output_config {
            body["output_config"] = output_config;
        }
        let identity_ids = self
            .tool_call_identities
            .project_request(&self.tool_call_scope, &mut body);
        self.tool_call_identities
            .project_reuse_references(&mut body, &identity_ids);
        Ok(body)
    }

    fn tool_turn_settings(
        &self,
        messages: &[ChatCompletionRequestMessage],
        context: &crate::provider::ProviderRequestContext,
    ) -> crate::provider::ProviderResult<Option<crate::provider::AnthropicReplaySettings>> {
        context.validate_assistant_count(
            messages
                .iter()
                .filter(|message| matches!(message, ChatCompletionRequestMessage::Assistant(_)))
                .count(),
        )?;
        let mut ids = context
            .assistant_response_ids
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .rev();
        for message in messages.iter().rev() {
            match message {
                // Named user rows are injected context, not a new human turn.
                ChatCompletionRequestMessage::User(user) if user.name.is_none() => return Ok(None),
                ChatCompletionRequestMessage::Assistant(assistant) => {
                    let id = ids.next().and_then(Option::as_ref);
                    if id.is_none()
                        && assistant.name.as_deref()
                            == Some(crate::context::INCOMPLETE_ASSISTANT_MESSAGE_NAME)
                    {
                        continue;
                    }
                    if assistant.tool_calls.as_ref().is_none_or(Vec::is_empty) {
                        return Ok(None);
                    }
                    return Ok(id.and_then(|id| {
                        self.native_replay
                            .anthropic_settings_for(&self.native_replay_scope, id)
                    }));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    fn request_preview_from_body(&self, body: Value) -> ProviderRequestPreview {
        let mut sections = wire_sections_from_body(
            &body,
            &[
                WireField {
                    id: "wire-tools",
                    label: "Tools",
                    key: "tools",
                },
                WireField {
                    id: "wire-system",
                    label: "System",
                    key: "system",
                },
                WireField {
                    id: "wire-messages",
                    label: "Messages",
                    key: "messages",
                },
            ],
            true,
        );
        crate::provider::annotate_cache_control_sections(&mut sections, &body);
        ProviderRequestPreview::with_wire_sections("POST", self.messages_endpoint(), body, sections)
    }
}

fn budget_below_max_tokens(max_tokens: u32) -> Option<u32> {
    if max_tokens <= 1 {
        return None;
    }
    let budget = max_tokens
        .saturating_sub(THINKING_BUDGET_HEADROOM_TOKENS)
        .max(1)
        .min(max_tokens - 1);
    Some(budget)
}

/// The ephemeral (5-minute) prompt-cache marker.
fn ephemeral_cache_control() -> Value {
    json!({"type": "ephemeral"})
}

/// Build the Anthropic `system` field. When caching, split off the volatile
/// tail at the `## Volatile state` heading so the `cache_control` breakpoint
/// lands on the byte-stable prefix (persona + steering files). The tail holds
/// the volatile project state plus any compaction-summary or mode-transition
/// system messages that `transform_messages` hoists after it — content that can
/// change across turns while the cached prefix stays identical. `head + tail ==
/// system`, so the model sees the same input either way.
fn system_field(system: &str, cache: bool) -> Value {
    if !cache || system.is_empty() {
        return json!(system);
    }
    let needle = format!("\n\n{}\n", crate::context::VOLATILE_STATE_HEADING);
    // Split on the *last* occurrence: compaction summaries and mode-transition
    // notes are hoisted into `system` after the volatile tail and must stay in
    // the uncached remainder.
    if let Some(idx) = system.rfind(&needle) {
        if idx > 0 {
            return json!([
                {"type": "text", "text": &system[..idx], "cache_control": ephemeral_cache_control()},
                {"type": "text", "text": &system[idx..]},
            ]);
        }
        // The entire system is volatile — nothing stable to cache.
        return json!(system);
    }
    // No volatile tail — the whole system prompt is byte-stable.
    json!([
        {"type": "text", "text": system, "cache_control": ephemeral_cache_control()},
    ])
}

/// Cache the tool prefix by marking the last tool definition.
fn mark_last_tool_cacheable(tools: &mut [Value]) {
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = ephemeral_cache_control();
    }
}

/// Cache the prior-history prefix by marking the last content block of the last
/// message. Every message shape `transform_messages` emits carries an array
/// `content`, so the breakpoint always has a home.
fn mark_last_message_cacheable(messages: &mut [Value]) {
    if let Some(block) = messages
        .last_mut()
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
        .and_then(|blocks| blocks.last_mut())
    {
        block["cache_control"] = ephemeral_cache_control();
    }
}

/// Cache the stable history block immediately before the mutable project-state
/// tail selected by [`PromptCachePolicy::RollingHistory`].
///
/// Anthropic merges adjacent user messages, so the state update can be the last
/// text block in the same message as a user request or tool result. Marking the
/// state block itself recreates the regression this strategy exists to prevent:
/// the next read changes that block before the backend can match the history.
/// IMPORTANT: do not simplify this to [`mark_last_message_cacheable`].
fn mark_rolling_history_cacheable(messages: &mut [Value]) {
    let tail_is_project_state = messages
        .last()
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .and_then(|blocks| blocks.last())
        .is_some_and(is_project_state_block);
    if !tail_is_project_state {
        mark_last_message_cacheable(messages);
        return;
    }

    let mut skipped_state_tail = false;
    for message in messages.iter_mut().rev() {
        let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in blocks.iter_mut().rev() {
            if !skipped_state_tail {
                skipped_state_tail = true;
                continue;
            }
            block["cache_control"] = ephemeral_cache_control();
            return;
        }
    }
}

fn is_project_state_block(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .is_some_and(|text| {
                // Legacy prefix: resumed sessions persisted before the
                // `Harness note:` envelope still carry it.
                text.starts_with(crate::context::PROJECT_STATE_UPDATE_PREFIX)
                    || text.starts_with(crate::context::PREVIOUS_PROJECT_STATE_UPDATE_PREFIX)
                    || text.starts_with(crate::context::LEGACY_PROJECT_STATE_UPDATE_PREFIX)
            })
}

#[async_trait]
impl Provider for AnthropicCompatibleProvider {
    fn prepare_request_for_estimation(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: crate::provider::ProviderRequestOptions,
        context: &crate::provider::ProviderRequestContext,
    ) -> crate::provider::ProviderResult<crate::provider::PreparedProviderRequest> {
        let reasoning = self.reasoning_for_request(messages, options, context)?;
        let body = self
            .request_body_with_context(messages, tools, reasoning, context)
            .map_err(|error| {
                crate::provider::ProviderFailure::configuration(format!("{error:#}"))
            })?;
        Ok(crate::provider::PreparedProviderRequest {
            protocol: Some(Protocol::AnthropicMessages),
            body,
            reasoning,
            native_budget: self.native_replay.selected_budgets(
                &self.native_replay_scope,
                context,
                Protocol::AnthropicMessages,
            ),
            count_endpoint: Some(format!(
                "{}/count_tokens",
                self.messages_endpoint().trim_end_matches('/')
            )),
            count_auth_header: Some(self.api_key_header.clone()),
        })
    }
    fn set_native_replay_store(&mut self, store: crate::provider::NativeReplayStore) {
        self.native_replay = store;
    }

    fn set_tool_call_identity_store(&mut self, store: crate::provider::ToolCallIdentityStore) {
        self.tool_call_identities = store;
    }

    fn project_state_cache_strategy(&self) -> crate::provider::ProjectStateCacheStrategy {
        if self.prompt_cache_policy.uses_append_only_project_state() {
            // IMPORTANT OPENCODE CACHE CONTRACT: this must agree with the
            // request-side rolling breakpoint. Keeping mutable state in
            // message zero makes the breakpoint irrelevant because the prompt
            // has already diverged before reaching history.
            crate::provider::ProjectStateCacheStrategy::AppendOnlyHistory
        } else {
            crate::provider::ProjectStateCacheStrategy::MutableSystemTail
        }
    }

    fn reasoning(&self) -> ReasoningSelection {
        self.reasoning
    }

    fn reasoning_escalation(&self) -> Option<ReasoningSelection> {
        self.reasoning_escalation
    }

    #[cfg(test)]
    fn preview_request(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
    ) -> Result<ProviderRequestPreview> {
        let body = self.request_body(messages, tools)?;
        Ok(self.request_preview_from_body(body))
    }

    #[cfg(test)]
    fn preview_request_with_context(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        context: &crate::provider::ProviderRequestContext,
    ) -> Result<ProviderRequestPreview> {
        let body = self.request_body_with_context(messages, tools, self.reasoning, context)?;
        Ok(self.request_preview_from_body(body))
    }

    fn take_last_request_diagnostics(&self) -> Option<ProviderRequestDiagnostics> {
        ProviderRequestDiagnostics::take(&self.last_request_diagnostics)
    }

    fn reasoning_for_request(
        &self,
        messages: &[ChatCompletionRequestMessage],
        options: crate::provider::ProviderRequestOptions,
        context: &crate::provider::ProviderRequestContext,
    ) -> crate::provider::ProviderResult<ReasoningSelection> {
        Ok(self
            .tool_turn_settings(messages, context)?
            .map_or(options.reasoning.unwrap_or(self.reasoning), |settings| {
                settings.reasoning
            }))
    }

    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.chat_stream_with_options(
            messages,
            tools,
            crate::provider::ProviderRequestOptions::default(),
            cancellation_token,
            sink,
        )
        .await
    }

    async fn chat_stream_with_options(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: crate::provider::ProviderRequestOptions,
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.chat_stream_with_context(
            messages,
            tools,
            options,
            &Default::default(),
            cancellation_token,
            sink,
        )
        .await
    }

    async fn chat_stream_with_context(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: crate::provider::ProviderRequestOptions,
        context: &crate::provider::ProviderRequestContext,
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        let effective_reasoning = self.reasoning_for_request(messages, options, context)?;
        let mut request_settings = None;
        let mut output_ceiling_tokens = None;
        let serialized_body = crate::provider::serialize_request_body(
            "Anthropic",
            || self.ensure_authorized(),
            || {
                let body = self
                    .prepare_request_for_estimation(messages, tools, options, context)?
                    .body;
                output_ceiling_tokens = body.get("max_tokens").and_then(Value::as_u64);
                request_settings = Some(crate::provider::AnthropicReplaySettings {
                    reasoning: effective_reasoning,
                    thinking: body.get("thinking").cloned(),
                    output_config: body.get("output_config").cloned(),
                });
                Ok(body)
            },
            |body| self.request_preview_from_body(body),
            &self.last_request_diagnostics,
        )?;

        let mut builder = self
            .http
            .post(self.messages_endpoint())
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("content-type", "application/json");
        // Only attach the auth header when a key is set; keyless local endpoints
        // (the `anthropic-compatible` optional-key case) reject an empty
        // `x-api-key`. Mirrors `discovery::fetch_generic_models`.
        if !self.api_key.trim().is_empty() {
            builder = builder.header(self.api_key_header_name(), self.api_key.trim());
        }
        let builder = builder.body(serialized_body);
        let Some(response) = streaming::send_json_stream(builder, &cancellation_token).await?
        else {
            return Ok(StreamedResponse::interrupted());
        };

        let parsed = parse_anthropic_stream(
            response,
            cancellation_token,
            sink,
            self.splits_inline_think_tags(),
            &self.tool_call_identities,
            &self.tool_call_scope,
        )
        .await?;
        if let Some(response_id) = &context.response_id
            && matches!(
                parsed.response.terminal,
                crate::provider::StreamTerminal::Completed(_)
            )
            && (parsed.replay_blocks.is_some() || !parsed.response.tool_calls.is_empty())
        {
            self.native_replay
                .insert(crate::provider::NativeReplayRecord {
                    response_id: response_id.clone(),
                    provider_scope: self.native_replay_scope.clone(),
                    payload: crate::provider::NativeReplayPayload::AnthropicMessages(
                        crate::provider::AnthropicNativeReplay {
                            budget_evidence: Some(crate::provider::NativeBudgetEvidence {
                                output_tokens: parsed.reported_output_tokens,
                                output_ceiling_tokens,
                            }),
                            content_blocks: parsed.replay_blocks,
                            request_settings,
                        },
                    ),
                })?;
        }
        Ok(parsed.response)
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
pub(crate) fn transform_messages(
    messages: &[ChatCompletionRequestMessage],
) -> Result<(String, Vec<Value>)> {
    transform_messages_with_replay(
        messages,
        &crate::provider::NativeReplayStore::default(),
        "",
        &Default::default(),
    )
}

/// Select native assistant content before adjacent role rows are merged. The
/// sidecar addresses original assistant rows, never the merged wire rows.
fn transform_messages_with_replay(
    messages: &[ChatCompletionRequestMessage],
    replay: &crate::provider::NativeReplayStore,
    scope: &str,
    context: &crate::provider::ProviderRequestContext,
) -> Result<(String, Vec<Value>)> {
    context.validate_assistant_count(
        messages
            .iter()
            .filter(|message| matches!(message, ChatCompletionRequestMessage::Assistant(_)))
            .count(),
    )?;
    let mut response_ids = context
        .assistant_response_ids
        .as_deref()
        .unwrap_or(&[])
        .iter();
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();

    for message in messages {
        let value = serde_json::to_value(message).context("Failed to serialize message")?;
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match role {
            "system" | "developer" => {
                if let Some(text) = transform::content_to_text(value.get("content")) {
                    system_parts.push(text);
                }
            }
            "user" => {
                let content = content_to_anthropic_user(value.get("content"));
                push_anthropic_message(&mut out, "user", content);
            }
            "assistant" => {
                let response_id = response_ids.next().and_then(Option::as_ref);
                if response_id.is_none()
                    && value.get("name").and_then(Value::as_str)
                        == Some(crate::context::INCOMPLETE_ASSISTANT_MESSAGE_NAME)
                {
                    if let Some(text) = transform::content_to_text(value.get("content")) {
                        push_anthropic_message(
                            &mut out,
                            "user",
                            vec![
                                json!({"type":"text","text":crate::tool::wrap_untrusted_content("incomplete assistant output from the previous generation",&text)}),
                            ],
                        );
                    }
                    continue;
                }
                if let Some(crate::provider::NativeReplayPayload::AnthropicMessages(native)) =
                    response_id.and_then(|id| replay.payload_for(scope, id))
                    && let Some(content_blocks) = native.content_blocks
                {
                    push_anthropic_message(&mut out, "assistant", content_blocks);
                    continue;
                }
                let mut blocks: Vec<Value> = Vec::new();
                let tool_calls = value.get("tool_calls").and_then(Value::as_array);
                if let Some(text) = transform::content_to_text(value.get("content"))
                    && !text.is_empty()
                {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                if let Some(tool_calls) = tool_calls {
                    for tool_call in tool_calls {
                        let (id, name, arguments) = transform::tool_call_parts(tool_call);
                        let input = tool_call_arguments_value(arguments);
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                }
                push_anthropic_message(&mut out, "assistant", blocks);
            }
            "tool" => {
                let call_id = value
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let output = transform::content_to_text(value.get("content")).unwrap_or_default();
                push_anthropic_message(
                    &mut out,
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": output,
                    })],
                );
            }
            _ => {}
        }
    }

    Ok((system_parts.join("\n\n"), out))
}

/// Anthropic requires alternating role turns. A parallel tool batch is stored
/// internally as one tool message per call, so merge consecutive equal roles
/// into one ordered content array before sending it. This also avoids
/// compatible gateways silently retaining only the final tool result.
fn push_anthropic_message(out: &mut Vec<Value>, role: &str, mut content: Vec<Value>) {
    if content.is_empty() {
        return;
    }
    if let Some(existing) = out.last_mut()
        && existing.get("role").and_then(Value::as_str) == Some(role)
        && let Some(blocks) = existing.get_mut("content").and_then(Value::as_array_mut)
    {
        blocks.append(&mut content);
        return;
    }
    out.push(json!({"role": role, "content": content}));
}

fn content_to_anthropic_user(content: Option<&Value>) -> Vec<Value> {
    transform::content_parts(content)
        .into_iter()
        .map(|part| match part {
            ContentPart::Text(text) => json!({"type": "text", "text": text}),
            ContentPart::ImageUrl(url) => json!({
                "type": "image",
                "source": anthropic_image_source(&url),
            }),
        })
        .collect()
}

/// The Messages API rejects `data:` URIs inside a `url` source; embedded
/// images must use the `base64` source shape with an explicit media type.
/// Remote http(s) URLs pass through as a `url` source.
fn anthropic_image_source(url: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some((media_type, data)) = rest.split_once(";base64,")
    {
        return json!({"type": "base64", "media_type": media_type, "data": data});
    }
    json!({"type": "url", "url": url})
}

pub(crate) fn transform_tools(tools: &[ChatCompletionTool]) -> Result<Vec<Value>> {
    tools
        .iter()
        .map(|tool| {
            let func = transform::tool_function(tool)?;
            Ok(json!({
                "name": func.name,
                "description": func.description,
                "input_schema": func.parameters,
            }))
        })
        .collect()
}

#[derive(Debug, Default)]
struct BlockState {
    tool_id: Option<String>,
    tool_name: Option<String>,
    tool_input: String,
    emitted: bool,
    native_block: Option<Value>,
    tool_call_index: Option<usize>,
    reconstruction_error: Option<String>,
}

impl BlockState {
    fn append_native_text(&mut self, kind: &str, field: &str, text: &str) {
        let block = self
            .native_block
            .get_or_insert_with(|| json!({"type": kind}));
        if block.get("type").and_then(Value::as_str) != Some(kind) {
            return;
        }
        if let Some(Value::String(value)) = block.get_mut(field) {
            value.push_str(text);
        } else {
            block[field] = Value::String(text.to_string());
        }
    }
}

#[derive(Debug)]
struct ParsedAnthropicStream {
    response: StreamedResponse,
    replay_blocks: Option<Vec<Value>>,
    reported_output_tokens: Option<u64>,
}

fn emit_anthropic_text(
    text: &str,
    splitter: Option<&mut ThinkTagSplitter>,
    content: &mut String,
    reasoning_chars: &mut usize,
    sink: &SharedSink,
) {
    if let Some(splitter) = splitter {
        let split = splitter.push(text);
        if !split.visible.is_empty() {
            content.push_str(&split.visible);
            sink.assistant_delta(&split.visible);
        }
        if !split.reasoning.is_empty() {
            *reasoning_chars = reasoning_chars.saturating_add(split.reasoning.chars().count());
            sink.reasoning_delta(&split.reasoning);
        }
    } else if !text.is_empty() {
        content.push_str(text);
        sink.assistant_delta(text);
    }
}

async fn parse_anthropic_stream(
    response: reqwest::Response,
    cancellation_token: CancellationToken,
    sink: SharedSink,
    split_think_tags: bool,
    identities: &crate::provider::ToolCallIdentityStore,
    identity_scope: &str,
) -> crate::provider::ProviderResult<ParsedAnthropicStream> {
    let mut content = String::new();
    let mut blocks: BTreeMap<usize, BlockState> = BTreeMap::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage = usage::AnthropicUsageAccumulator::default();
    let mut splitter = split_think_tags.then(ThinkTagSplitter::new);
    let mut finish_reason: Option<crate::provider::FinishReason> = None;
    let mut terminal_seen = false;
    let mut reasoning_chars = 0usize;

    let interrupted = sse::drive_sse(response, cancellation_token.clone(), |event, data| {
        // Anthropic always pairs `event:` with the following `data:`; a data
        // line with no event is ignored, matching the original behaviour.
        match event {
            Some(event) => handle_anthropic_event(
                event,
                data,
                &mut blocks,
                &mut content,
                &mut tool_calls,
                &mut usage,
                &mut finish_reason,
                &mut terminal_seen,
                &mut reasoning_chars,
                &sink,
                splitter.as_mut(),
            ),
            None => Ok(()),
        }
    })
    .await?;

    // Flush any text the splitter held back waiting on a possible tag boundary,
    // then signal end-of-message.
    let reasoning_tail = streaming::finish_text_stream(splitter.as_mut(), &mut content, &sink);
    reasoning_chars = reasoning_chars.saturating_add(reasoning_tail.chars().count());
    let interrupted = interrupted || cancellation_token.is_cancelled();

    if !interrupted && !terminal_seen {
        return Err(crate::provider::ProviderFailure::transport(
            "Anthropic stream ended before message_stop or a terminal stop_reason",
        ));
    }

    // A complete native turn includes its text/tool boundaries, not just the
    // thinking blocks. Keep opaque fields and block order exactly as received.
    let has_native_thinking = blocks.values().any(|state| {
        matches!(
            state
                .native_block
                .as_ref()
                .and_then(|block| block["type"].as_str()),
            Some("thinking" | "redacted_thinking")
        )
    });
    if finish_reason.is_none() && terminal_seen {
        finish_reason = Some(if tool_calls.is_empty() {
            crate::provider::FinishReason::Stop
        } else {
            crate::provider::FinishReason::ToolCalls
        });
    }

    let terminal = if interrupted {
        crate::provider::StreamTerminal::Interrupted
    } else {
        crate::provider::StreamTerminal::from_finish_reason(
            finish_reason.unwrap_or(crate::provider::FinishReason::Stop),
        )
    };

    if has_native_thinking && matches!(terminal, crate::provider::StreamTerminal::Completed(_)) {
        if !blocks.values().all(|state| state.emitted) {
            return Err(crate::provider::ProviderFailure::transport(
                "Anthropic native response ended with an unclosed content block",
            ));
        }
        if let Some(error) = blocks
            .values()
            .find_map(|state| state.reconstruction_error.as_deref())
        {
            return Err(crate::provider::ProviderFailure::configuration(error));
        }
    }
    identities.assign_calls(identity_scope, &mut tool_calls);
    let replay_blocks =
        (has_native_thinking && blocks.values().all(|state| state.emitted)).then(|| {
            blocks
                .into_values()
                .filter_map(|state| {
                    let mut block = state.native_block?;
                    if let Some(index) = state.tool_call_index
                        && let Some(call) = tool_calls.get(index)
                    {
                        block["id"] = Value::String(call.id.clone());
                    }
                    Some(block)
                })
                .collect()
        });

    let reported_output_tokens = usage.completed_output_tokens();
    Ok(ParsedAnthropicStream {
        response: StreamedResponse {
            content,
            tool_calls,
            terminal,
            usage: usage.into_usage(),
            reasoning_chars,
        },
        replay_blocks,
        reported_output_tokens,
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_anthropic_event(
    event: &str,
    data: &str,
    blocks: &mut BTreeMap<usize, BlockState>,
    content: &mut String,
    tool_calls: &mut Vec<ToolCall>,
    usage: &mut usage::AnthropicUsageAccumulator,
    finish_reason: &mut Option<crate::provider::FinishReason>,
    terminal_seen: &mut bool,
    reasoning_chars: &mut usize,
    sink: &SharedSink,
    mut splitter: Option<&mut ThinkTagSplitter>,
) -> crate::provider::ProviderResult<()> {
    let Some(payload) = sse::parse_frame(data)? else {
        return Ok(());
    };

    match event {
        "content_block_start" => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let block = payload.get("content_block").unwrap_or(&Value::Null);
            let entry = blocks.entry(index).or_default();
            entry.native_block = Some(block.clone());
            if matches!(
                block.get("type").and_then(Value::as_str),
                Some("thinking" | "redacted_thinking")
            ) && let Some(splitter) = splitter.as_deref_mut()
            {
                let literal = splitter.use_native_reasoning_channel();
                if !literal.visible.is_empty() {
                    content.push_str(&literal.visible);
                    sink.assistant_delta(&literal.visible);
                }
            }
            if block.get("type").and_then(Value::as_str) == Some("thinking")
                && let Some(text) = block.get("thinking").and_then(Value::as_str)
                && !text.is_empty()
            {
                *reasoning_chars = reasoning_chars.saturating_add(text.chars().count());
                sink.reasoning_delta(text);
            }
            if block.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = block.get("text").and_then(Value::as_str)
            {
                emit_anthropic_text(text, splitter, content, reasoning_chars, sink);
            }
            if let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) && block.get("type").and_then(Value::as_str) == Some("tool_use")
            {
                entry.tool_id = Some(id.to_string());
                entry.tool_name = Some(name.to_string());
            }
        }
        "content_block_delta" => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let delta = payload.get("delta").unwrap_or(&Value::Null);
            let delta_type = delta
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match delta_type {
                "text_delta" => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        blocks
                            .entry(index)
                            .or_default()
                            .append_native_text("text", "text", text);
                        emit_anthropic_text(text, splitter, content, reasoning_chars, sink);
                    }
                }
                "thinking_delta" => {
                    if let Some(splitter) = splitter {
                        let literal = splitter.use_native_reasoning_channel();
                        if !literal.visible.is_empty() {
                            content.push_str(&literal.visible);
                            sink.assistant_delta(&literal.visible);
                        }
                    }
                    if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                        blocks
                            .entry(index)
                            .or_default()
                            .append_native_text("thinking", "thinking", text);
                        if !text.is_empty() {
                            *reasoning_chars = reasoning_chars.saturating_add(text.chars().count());
                            sink.reasoning_delta(text);
                        }
                    }
                }
                "signature_delta" => {
                    if let Some(signature) = delta.get("signature").and_then(Value::as_str)
                        && let Some(entry) = blocks.get_mut(&index)
                    {
                        entry.append_native_text("thinking", "signature", signature);
                    }
                }
                "input_json_delta" => {
                    if let Some(partial) = delta.get("partial_json").and_then(Value::as_str)
                        && let Some(entry) = blocks.get_mut(&index)
                    {
                        entry.tool_input.push_str(partial);
                    }
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(entry) = blocks.get_mut(&index)
                && !entry.emitted
            {
                entry.emitted = true;
                if let (Some(id), Some(name)) = (entry.tool_id.take(), entry.tool_name.take()) {
                    if let Some(block) = &mut entry.native_block {
                        if !entry.tool_input.is_empty() {
                            match serde_json::from_str::<Value>(&entry.tool_input) {
                                Ok(input) => block["input"] = input,
                                Err(error) => {
                                    entry.reconstruction_error = Some(format!(
                                        "Anthropic native tool `{name}` cannot replay its input unchanged: raw arguments {:?}; parse error: {error}",
                                        entry.tool_input,
                                    ))
                                }
                            }
                        } else if let Some(input) = block.get("input") {
                            entry.tool_input = serde_json::to_string(input).map_err(|error| {
                                crate::provider::ProviderFailure::transport(format!(
                                    "Anthropic tool `{name}` input serialization failed: {error}"
                                ))
                            })?;
                        }
                    }
                    entry.tool_call_index = Some(tool_calls.len());
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: std::mem::take(&mut entry.tool_input),
                    });
                }
            }
        }
        "message_start" => {
            if let Some(u) = payload.pointer("/message/usage") {
                usage.apply_event_usage(u);
            }
        }
        "message_delta" => {
            if let Some(reason) = payload
                .pointer("/delta/stop_reason")
                .and_then(Value::as_str)
            {
                *finish_reason = Some(crate::provider::FinishReason::from_openai(reason));
                *terminal_seen = true;
            }
            // `output_tokens` here is cumulative; the last value wins. Input
            // and cache fields are absent on real Anthropic deltas (their
            // `message_start` values stand) but carry the final breakdown on
            // MiniMax-style providers — `apply_event_usage` handles both.
            if let Some(u) = payload.pointer("/usage") {
                usage.apply_final_event_usage(u);
            }
        }
        "message_stop" => {
            *terminal_seen = true;
        }
        "error" => {
            // Typed so transient `overloaded_error`/`rate_limit_error` frames
            // are retried instead of failing the turn as a plain error.
            return Err(sse::stream_error_from_object(
                payload.get("error").unwrap_or(&Value::Null),
                "Anthropic stream error",
                &payload,
            ));
        }
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        unsafe_code,
        reason = "edition-2024 requires `unsafe` around std::env::{set_var,remove_var}; test-only"
    )]
    use super::*;
    use crate::output::{OutputSink, StdoutSink};
    use crate::provider::minimax_coding_plan::MiniMaxCodingPlanFactory;
    use crate::provider::test_utils::{
        assistant_message, named_user_message, sample_tool, system_message, tool_call_message,
        tool_result_message, user_message,
    };
    use crate::provider::{AuthInput, ProviderFailure};
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sink() -> SharedSink {
        Arc::new(StdoutSink)
    }

    #[derive(Default)]
    struct RecordingSink {
        assistant_deltas: Mutex<Vec<String>>,
        reasoning_deltas: Mutex<Vec<String>>,
        assistant_done_count: Mutex<usize>,
    }

    impl RecordingSink {
        fn assistant_deltas(&self) -> Vec<String> {
            self.assistant_deltas
                .lock()
                .expect("recording sink assistant mutex should not be poisoned")
                .clone()
        }

        fn reasoning_deltas(&self) -> Vec<String> {
            self.reasoning_deltas
                .lock()
                .expect("recording sink reasoning mutex should not be poisoned")
                .clone()
        }

        fn assistant_done_count(&self) -> usize {
            *self
                .assistant_done_count
                .lock()
                .expect("recording sink done mutex should not be poisoned")
        }
    }

    impl OutputSink for RecordingSink {
        fn assistant_delta(&self, text: &str) {
            self.assistant_deltas
                .lock()
                .expect("recording sink assistant mutex should not be poisoned")
                .push(text.to_string());
        }

        fn assistant_done(&self) {
            *self
                .assistant_done_count
                .lock()
                .expect("recording sink done mutex should not be poisoned") += 1;
        }

        fn reasoning_delta(&self, text: &str) {
            self.reasoning_deltas
                .lock()
                .expect("recording sink reasoning mutex should not be poisoned")
                .push(text.to_string());
        }
    }

    fn make_session() -> ProviderSession {
        ProviderSession {
            api_key: "sk-ant-test".to_string(),
            credential_source: crate::session::CredentialSource::Session,
            base_url: String::new(),
            model: "claude-sonnet-4-5".to_string(),
            context_window: None,
            reasoning: crate::provider::ReasoningSelection::default(),
            model_reasoning: std::collections::HashMap::new(),
            account_id: String::new(),
            is_fedramp_account: false,
            authorized_at: None,
        }
    }

    fn build_test_provider(
        factory: &dyn ProviderFactory,
        session: &ProviderSession,
    ) -> Box<dyn Provider> {
        let target = crate::provider::fallback_run_target(factory.metadata(), session);
        factory.build_target(session, &target)
    }

    fn anthropic_provider(session: &ProviderSession) -> Box<dyn Provider> {
        build_test_provider(&AnthropicFactory, session)
    }

    fn loopback_anthropic_provider(
        server: &MockServer,
        model: &str,
    ) -> AnthropicCompatibleProvider {
        let url = reqwest::Url::parse(&server.uri()).unwrap();
        assert!(
            url.host_str()
                .unwrap()
                .parse::<std::net::IpAddr>()
                .unwrap()
                .is_loopback()
        );
        let mut session = make_session();
        session.base_url = server.uri();
        session.model = model.to_string();
        let target = crate::provider::fallback_run_target(&ANTHROPIC_METADATA, &session);
        let mut provider = AnthropicCompatibleProvider::new(
            "anthropic",
            &session,
            &target,
            AnthropicTransportFlags {
                require_api_key: true,
                supports_prompt_cache: false,
                supports_vision: true,
            },
            "x-api-key",
        );
        provider.http = reqwest::Client::new();
        provider
    }

    fn minimax_provider(session: &ProviderSession) -> Box<dyn Provider> {
        build_test_provider(&MiniMaxCodingPlanFactory, session)
    }

    fn concrete_anthropic_provider(
        reasoning: ReasoningSelection,
        output_limit: Option<u32>,
    ) -> AnthropicCompatibleProvider {
        let mut session = make_session();
        session.reasoning = reasoning;
        let mut target = crate::provider::fallback_run_target(&ANTHROPIC_METADATA, &session);
        target.reasoning = reasoning;
        target.output_limit = output_limit;
        AnthropicCompatibleProvider::new(
            "anthropic",
            &session,
            &target,
            AnthropicTransportFlags {
                require_api_key: true,
                supports_prompt_cache: false,
                supports_vision: true,
            },
            "x-api-key",
        )
    }

    fn adaptive_anthropic_provider(reasoning: ReasoningSelection) -> AnthropicCompatibleProvider {
        let mut session = make_session();
        session.reasoning = reasoning;
        let mut target = crate::provider::fallback_run_target(&ANTHROPIC_METADATA, &session);
        target.reasoning = reasoning;
        target.reasoning_codec = ReasoningCodec::AnthropicAdaptive;
        AnthropicCompatibleProvider::new(
            "anthropic",
            &session,
            &target,
            AnthropicTransportFlags {
                require_api_key: true,
                supports_prompt_cache: false,
                supports_vision: true,
            },
            "x-api-key",
        )
    }

    fn hybrid_anthropic_provider(reasoning: ReasoningSelection) -> AnthropicCompatibleProvider {
        let mut session = make_session();
        session.reasoning = reasoning;
        let mut target = crate::provider::fallback_run_target(&ANTHROPIC_METADATA, &session);
        target.reasoning = reasoning;
        target.reasoning_codec = ReasoningCodec::AnthropicThinkingWithEffort;
        AnthropicCompatibleProvider::new(
            "anthropic",
            &session,
            &target,
            AnthropicTransportFlags {
                require_api_key: true,
                supports_prompt_cache: false,
                supports_vision: true,
            },
            "x-api-key",
        )
    }

    fn anthropic_sse(events: &[&str]) -> String {
        let mut out = String::new();
        for event in events {
            out.push_str(event);
            if !event.ends_with("\n\n") {
                out.push_str("\n\n");
            }
        }
        out
    }

    #[test]
    fn preview_request_wire_sections_follow_serialized_body_order() {
        let provider = anthropic_provider(&make_session());
        let messages = vec![system_message("system prompt"), user_message("hello")];
        let preview = provider
            .preview_request(&messages, &[sample_tool()])
            .unwrap();

        let ids = preview
            .wire_sections
            .iter()
            .map(|section| section.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                "wire-max-tokens",
                "wire-messages",
                "wire-model",
                "wire-stream",
                "wire-system",
                "wire-tools",
            ]
        );
        let paths = preview
            .wire_sections
            .iter()
            .map(|section| section.provider_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            preview
                .body
                .as_object()
                .unwrap()
                .keys()
                .map(|key| format!("$.{key}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn request_body_raises_default_max_tokens_above_large_thinking_budget() {
        let provider = concrete_anthropic_provider(ReasoningSelection::Max, None);
        let body = provider
            .request_body(&[user_message("hello")], &[])
            .unwrap();

        assert_eq!(body["thinking"]["budget_tokens"], json!(32_768));
        assert_eq!(
            body["max_tokens"],
            json!(32_768 + THINKING_BUDGET_HEADROOM_TOKENS)
        );
        assert!(
            body["max_tokens"].as_u64().unwrap()
                > body["thinking"]["budget_tokens"].as_u64().unwrap()
        );
    }

    #[test]
    fn request_body_shrinks_thinking_budget_when_output_limit_caps_max_tokens() {
        let provider = concrete_anthropic_provider(ReasoningSelection::Max, Some(16_000));
        let body = provider
            .request_body(&[user_message("hello")], &[])
            .unwrap();

        assert_eq!(body["max_tokens"], json!(16_000));
        assert_eq!(
            body["thinking"]["budget_tokens"],
            json!(16_000 - THINKING_BUDGET_HEADROOM_TOKENS)
        );
        assert!(
            body["max_tokens"].as_u64().unwrap()
                > body["thinking"]["budget_tokens"].as_u64().unwrap()
        );
    }

    #[test]
    fn request_local_reasoning_override_uses_smaller_thinking_budget() {
        let provider = concrete_anthropic_provider(ReasoningSelection::High, None);

        let body = provider
            .request_body_with_reasoning(&[user_message("hello")], &[], ReasoningSelection::Medium)
            .unwrap();

        assert_eq!(body["thinking"]["budget_tokens"], json!(8_192));
        assert_eq!(provider.reasoning, ReasoningSelection::High);
    }

    #[test]
    fn adaptive_request_uses_thinking_envelope_and_output_effort() {
        let provider = adaptive_anthropic_provider(ReasoningSelection::Ultra);

        let body = provider
            .request_body(&[user_message("hello")], &[])
            .unwrap();

        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert_eq!(body["output_config"], json!({"effort": "max"}));
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        assert!(body["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn adaptive_request_can_disable_thinking_without_effort() {
        let provider = adaptive_anthropic_provider(ReasoningSelection::Off);

        let body = provider
            .request_body(&[user_message("hello")], &[])
            .unwrap();

        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn hybrid_request_uses_budget_tokens_and_output_effort() {
        let provider = hybrid_anthropic_provider(ReasoningSelection::High);

        let body = provider
            .request_body(&[user_message("hello")], &[])
            .unwrap();

        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "budget_tokens": 16_384})
        );
        assert_eq!(body["output_config"], json!({"effort": "high"}));
        assert!(body["max_tokens"].as_u64().unwrap() > 16_384);
    }

    fn pasted_image_user_message() -> ChatCompletionRequestMessage {
        use async_openai::types::chat::{
            ChatCompletionRequestMessageContentPartImage, ChatCompletionRequestUserMessage,
            ChatCompletionRequestUserMessageContent, ChatCompletionRequestUserMessageContentPart,
            ImageUrl,
        };
        ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
            content: ChatCompletionRequestUserMessageContent::Array(vec![
                ChatCompletionRequestUserMessageContentPart::ImageUrl(
                    ChatCompletionRequestMessageContentPartImage {
                        prompt_cache_breakpoint: None,
                        image_url: ImageUrl {
                            url: "data:image/png;base64,AAAA".to_string(),
                            detail: None,
                        },
                    },
                ),
            ]),
            name: None,
        })
    }

    #[test]
    fn request_body_strips_images_for_non_vision_model() {
        // An image already in history (pasted under a vision model, then a
        // switch to a text-only Anthropic-compatible endpoint) must not 400
        // every later turn — the wire body downgrades it to a placeholder.
        let session = make_session();
        let target = crate::provider::fallback_run_target(&ANTHROPIC_METADATA, &session);
        let provider = AnthropicCompatibleProvider::new(
            "minimax-coding-plan",
            &session,
            &target,
            AnthropicTransportFlags {
                require_api_key: true,
                supports_prompt_cache: false,
                supports_vision: false,
            },
            "x-api-key",
        );

        let body = provider
            .request_body(&[user_message("hi"), pasted_image_user_message()], &[])
            .unwrap();
        let serialized = body.to_string();

        assert!(!serialized.contains("image_url"));
        assert!(!serialized.contains("\"type\":\"image\""));
        assert!(serialized.contains(crate::provider::transform::IMAGE_OMITTED_PLACEHOLDER));
    }

    #[test]
    fn request_body_keeps_images_for_vision_model() {
        let provider = concrete_anthropic_provider(ReasoningSelection::Off, None);

        let body = provider
            .request_body(&[user_message("hi"), pasted_image_user_message()], &[])
            .unwrap();
        let serialized = body.to_string();

        assert!(serialized.contains("\"type\":\"image\""));
    }

    #[tokio::test]
    async fn chat_stream_parses_text_delta_events() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test"))
            .and(header("anthropic-version", ANTHROPIC_API_VERSION))
            .and(body_partial_json(json!({"stream": true, "model": "claude-sonnet-4-5"})))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: message_start\ndata: {\"type\":\"message_start\"}",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello, \"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"world!\"}}",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let messages = vec![user_message("hi")];
        let response = provider
            .chat_stream(&messages, &[], CancellationToken::new(), sink())
            .await
            .unwrap();

        assert_eq!(response.content, "Hello, world!");
        assert!(response.tool_calls.is_empty());
        assert!(!response.is_interrupted());
        let diagnostics = provider.take_last_request_diagnostics().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&diagnostics.serialized_body).unwrap(),
            provider.preview_request(&messages, &[]).unwrap().body
        );
        assert_eq!(provider.take_last_request_diagnostics(), None);
    }

    #[tokio::test]
    async fn minimax_chat_stream_uses_minimax_api_key_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("X-Api-Key", "sk-mm-test"))
            .and(header("anthropic-version", ANTHROPIC_API_VERSION))
            .and(body_partial_json(json!({"stream": true, "model": "MiniMax-M3"})))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: message_start\ndata: {\"type\":\"message_start\"}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.api_key = "sk-mm-test".to_string();
        session.base_url = server.uri();
        session.model = "MiniMax-M3".to_string();
        let provider = minimax_provider(&session);
        let response = provider
            .chat_stream(&[user_message("hi")], &[], CancellationToken::new(), sink())
            .await
            .unwrap();

        assert_eq!(response.content, "ok");
    }

    #[tokio::test]
    async fn chat_stream_skips_empty_text_delta_sink_output() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"\"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"\"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" \"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let recording = Arc::new(RecordingSink::default());
        let response = provider
            .chat_stream(
                &[user_message("hi")],
                &[],
                CancellationToken::new(),
                recording.clone(),
            )
            .await
            .unwrap();

        assert_eq!(response.content, " Hello");
        assert_eq!(
            recording.assistant_deltas(),
            vec![" ".to_string(), "Hello".to_string()]
        );
        assert!(recording.reasoning_deltas().is_empty());
        assert_eq!(recording.assistant_done_count(), 1);
    }

    #[tokio::test]
    async fn minimax_splits_inline_think_tags_into_reasoning() {
        // MiniMax (Anthropic protocol, no reasoning channel) emits reasoning
        // inline as `<think>…</think>` in the text deltas — here split across
        // chunk boundaries. It must land in the reasoning channel, not the
        // visible answer, and not leak into the re-sent history.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"<thi\"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"nk>weighing options</think>Here\"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" you go <think>literal example</think>\"}}",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.api_key = "sk-mm-test".to_string();
        session.base_url = server.uri();
        session.model = "MiniMax-M3".to_string();
        let provider = minimax_provider(&session);
        let recording = Arc::new(RecordingSink::default());
        let response = provider
            .chat_stream(
                &[user_message("hi")],
                &[],
                CancellationToken::new(),
                recording.clone(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.content,
            "Here you go <think>literal example</think>"
        );
        assert_eq!(response.reasoning_chars, "weighing options".chars().count());
        assert_eq!(
            recording.assistant_deltas().concat(),
            "Here you go <think>literal example</think>"
        );
        assert_eq!(recording.reasoning_deltas().concat(), "weighing options");
    }

    #[tokio::test]
    async fn compatible_native_thinking_preserves_leading_literal_tags() {
        for native_start in [
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque\"}}",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"native reasoning\"}}",
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST")).and(path("/v1/messages")).respond_with(
                ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                    native_start,
                    "event: content_block_stop\ndata: {\"index\":0}",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"<thi\"}}",
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"nk>literal example</think>\"}}",
                    "event: content_block_stop\ndata: {\"index\":1}",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}",
                ]))
            ).mount(&server).await;
            let mut session = make_session();
            session.base_url = server.uri();
            session.model = "MiniMax-M3".to_string();
            let provider = minimax_provider(&session);
            let sink = Arc::new(RecordingSink::default());
            let response = provider
                .chat_stream(
                    &[user_message("example")],
                    &[],
                    CancellationToken::new(),
                    sink.clone(),
                )
                .await
                .unwrap();
            assert_eq!(response.content, "<think>literal example</think>");
            assert_eq!(sink.assistant_deltas().concat(), response.content);
            assert!(!sink.reasoning_deltas().concat().contains("literal example"));
        }
    }

    #[tokio::test]
    async fn real_anthropic_keeps_literal_think_tags_in_text() {
        // Real Claude streams reasoning on `thinking_delta` and may legitimately
        // print a literal `<think>` (e.g. in a code sample); the splitter must
        // stay off so such text is preserved verbatim.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"use <think> tags\"}}",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let recording = Arc::new(RecordingSink::default());
        let response = provider
            .chat_stream(
                &[user_message("hi")],
                &[],
                CancellationToken::new(),
                recording.clone(),
            )
            .await
            .unwrap();

        assert_eq!(response.content, "use <think> tags");
        assert_eq!(recording.assistant_deltas().concat(), "use <think> tags");
        assert!(recording.reasoning_deltas().is_empty());
    }

    #[tokio::test]
    async fn chat_stream_parses_thinking_delta_events() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reasoning...\"}}",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let response = provider
            .chat_stream(
                &[user_message("think please")],
                &[],
                CancellationToken::new(),
                sink(),
            )
            .await
            .unwrap();

        assert_eq!(response.content, "");
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.reasoning_chars, "reasoning...".chars().count());
    }

    #[tokio::test]
    async fn chat_stream_accumulates_tool_use_input_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_01\",\"name\":\"read\"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Cargo.toml\\\"}\"}}",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let response = provider
            .chat_stream(
                &[user_message("read Cargo.toml")],
                &[],
                CancellationToken::new(),
                sink(),
            )
            .await
            .unwrap();

        assert_eq!(response.tool_calls.len(), 1);
        crate::provider::test_utils::assert_tool_call_identity(
            provider.as_ref(),
            &response.tool_calls[0],
            "toolu_01",
        );
        assert_eq!(response.tool_calls[0].name, "read");
        assert_eq!(
            response.tool_calls[0].arguments,
            "{\"path\":\"Cargo.toml\"}"
        );
    }

    #[tokio::test]
    async fn chat_stream_returns_overloaded_frame_as_retryable_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let result = provider
            .chat_stream(&[user_message("hi")], &[], CancellationToken::new(), sink())
            .await;

        let err = result.expect_err("an error frame should fail the stream");
        assert!(err.to_string().contains("overloaded"));
        // The frame must surface as a retryable typed error (529), not a plain
        // bail, so `chat_stream_with_retry` backs off instead of failing the turn.
        assert!(err.is_retryable());
        assert!(matches!(err, ProviderFailure::Http { status: 529, .. }));
    }

    #[tokio::test]
    async fn chat_stream_treats_truncated_final_frame_as_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            // A final data frame cut off mid-JSON with no closing blank line:
            // `drive_sse` flushes it at EOF, the malformed JSON fails to parse,
            // and the transport surfaces a retryable error instead of a silent
            // empty "success".
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    "event: content_block_delta\ndata: {\"type\":\"content_block_de",
                ),
            )
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let result = provider
            .chat_stream(&[user_message("hi")], &[], CancellationToken::new(), sink())
            .await;

        let err = result.expect_err("a truncated stream should fail, not succeed empty");
        assert!(err.is_retryable());
        assert!(matches!(err, ProviderFailure::Decode { .. }));
    }

    #[tokio::test]
    async fn chat_stream_rejects_clean_eof_after_valid_delta() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let error = provider
            .chat_stream(&[user_message("hi")], &[], CancellationToken::new(), sink())
            .await
            .expect_err("valid deltas without a terminal event must be retried");

        assert!(error.is_retryable());
        assert!(matches!(error, ProviderFailure::Transport { .. }));
    }

    #[tokio::test]
    async fn chat_stream_reports_http_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let result = provider
            .chat_stream(&[user_message("hi")], &[], CancellationToken::new(), sink())
            .await;

        let err = result.unwrap_err();
        assert!(matches!(err, ProviderFailure::Http { status: 401, .. }));
    }

    #[tokio::test]
    async fn chat_stream_captures_token_usage() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":1,\"cache_read_input_tokens\":30,\"cache_creation_input_tokens\":10}}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":99}}",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let response = provider
            .chat_stream(&[user_message("hi")], &[], CancellationToken::new(), sink())
            .await
            .unwrap();

        let usage = response.usage.expect("usage should be captured");
        // prompt_tokens is cache-inclusive (42 fresh + 30 read + 10 creation).
        assert_eq!(usage.prompt_tokens, 82);
        assert_eq!(usage.completion_tokens, 99);
        assert_eq!(
            usage.input_cache,
            Some(crate::provider::InputCacheUsage::new(30, 10, 82))
        );
    }

    #[tokio::test]
    async fn chat_stream_captures_minimax_style_delta_usage() {
        // MiniMax reports `input_tokens: 0` on `message_start` and the real
        // input + cache breakdown on `message_delta`. The parser must read it
        // there, or input/cache are lost (the `↑0 · cache n/a` bug).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}",
                "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":14,\"output_tokens\":23,\"cache_creation_input_tokens\":0,\"cache_read_input_tokens\":1328}}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let response = provider
            .chat_stream(&[user_message("hi")], &[], CancellationToken::new(), sink())
            .await
            .unwrap();

        let usage = response.usage.expect("usage should be captured");
        // Cache-inclusive: 14 fresh + 1328 read + 0 creation.
        assert_eq!(usage.prompt_tokens, 1342);
        assert_eq!(usage.completion_tokens, 23);
        assert_eq!(
            usage.input_cache,
            Some(crate::provider::InputCacheUsage::new(1328, 0, 1342))
        );
    }

    #[tokio::test]
    async fn chat_stream_cancellation_marks_interrupted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}",
            ])))
            .mount(&server)
            .await;

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let token = CancellationToken::new();
        token.cancel();

        let response = provider
            .chat_stream(&[user_message("hi")], &[], token, sink())
            .await
            .unwrap();

        assert!(response.is_interrupted());
    }

    #[tokio::test]
    async fn thinking_blocks_are_captured_and_replayed_through_tool_loops() {
        // The Anthropic protocol expects a turn's thinking to be passed back
        // with its tool results; dropping it made models stop thinking after
        // the first tool round. Stream a thinking block + a tool
        // call, then verify the next request replays the thinking block ahead
        // of the rebuilt assistant content, signature included.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(anthropic_sse(&[
                "event: message_start\ndata: {\"type\":\"message_start\"}",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"planning the read\"}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-abc\"}}",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}",
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\",\"input\":{}}}",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"foo\\\"}\"}}",
                "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}",
                "event: message_stop\ndata: {\"type\":\"message_stop\"}",
            ])))
            .mount(&server)
            .await;

        let provider = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
        let response_id = crate::provider::ResponseId::new();
        let response = provider
            .chat_stream_with_context(
                &[user_message("read foo")],
                &[],
                Default::default(),
                &crate::provider::ProviderRequestContext {
                    response_id: Some(response_id.clone()),
                    ..Default::default()
                },
                CancellationToken::new(),
                sink(),
            )
            .await
            .unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        crate::provider::test_utils::assert_tool_call_identity(
            &provider,
            &response.tool_calls[0],
            "toolu_1",
        );

        // The next request rebuilds history with the same tool-call id; the
        // captured thinking must lead the assistant content.
        let follow_up = vec![
            user_message("read foo"),
            tool_call_message(&response.tool_calls[0].id, "read", "{\"path\":\"foo\"}"),
            tool_result_message(&response.tool_calls[0].id, "file contents"),
        ];
        let preview = provider
            .preview_request_with_context(
                &follow_up,
                &[],
                &crate::provider::ProviderRequestContext {
                    assistant_response_ids: Some(vec![Some(response_id)]),
                    response_id: None,
                },
            )
            .unwrap();
        let assistant = &preview.body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        let first_block = &assistant["content"][0];
        assert_eq!(first_block["type"], "thinking", "{assistant}");
        assert_eq!(first_block["thinking"], "planning the read");
        assert_eq!(first_block["signature"], "sig-abc");
        assert_eq!(assistant["content"][1]["type"], "tool_use");
        assert_eq!(assistant["content"][1]["id"], "toolu_1");
    }

    fn native_content_sse(blocks: &[Value], finish: Option<&str>) -> String {
        let mut frames = vec![format!(
            "event: message_start\ndata: {}",
            json!({
                "message":{"usage":{"input_tokens":12,"output_tokens":1}}
            })
        )];
        for (index, block) in blocks.iter().enumerate() {
            frames.push(format!(
                "event: content_block_start\ndata: {}",
                json!({
                    "index":index,"content_block":block
                })
            ));
            frames.push(format!(
                "event: content_block_stop\ndata: {}",
                json!({"index":index})
            ));
        }
        if let Some(finish) = finish {
            frames.push(format!(
                "event: message_delta\ndata: {}",
                json!({
                    "delta":{"stop_reason":finish},"usage":{"output_tokens":100}
                })
            ));
            frames.push("event: message_stop\ndata: {}".into());
        }
        frames.join("\n\n") + "\n\n"
    }

    #[tokio::test]
    async fn native_anthropic_thinking_settings_survive_tool_turn_rebuild_and_release_on_human_input()
     {
        use crate::provider::test_utils::named_user_message;
        use crate::provider::{
            NativeReplayRecord, NativeReplayStore, ProviderRequestContext, ProviderRequestOptions,
            ResponseId,
        };
        for (codec, initial, requested, output_limit) in [
            (
                ReasoningCodec::AnthropicThinking,
                ReasoningSelection::High,
                ReasoningSelection::Off,
                None,
            ),
            (
                ReasoningCodec::AnthropicAdaptive,
                ReasoningSelection::High,
                ReasoningSelection::Off,
                None,
            ),
            (
                ReasoningCodec::AnthropicAdaptive,
                ReasoningSelection::Off,
                ReasoningSelection::High,
                None,
            ),
            (
                ReasoningCodec::AnthropicThinking,
                ReasoningSelection::BudgetTokens(32768),
                ReasoningSelection::Off,
                Some(8192),
            ),
            (
                ReasoningCodec::AnthropicThinkingWithEffort,
                ReasoningSelection::High,
                ReasoningSelection::Off,
                None,
            ),
        ] {
            let server = MockServer::start().await;
            let mut blocks = vec![];
            if initial != ReasoningSelection::Off {
                blocks.push(json!({"type":"thinking","thinking":"","signature":"exact=="}));
            }
            blocks.push(
                json!({"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"foo"}}),
            );
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(native_content_sse(&blocks, Some("tool_use"))),
                )
                .mount(&server)
                .await;
            let mut provider = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
            provider.reasoning_codec = codec;
            provider.native_replay_scope = crate::provider::tool_call_scope(
                "anthropic-messages-native",
                &provider.tool_call_scope,
                &provider.model,
                &format!("{codec:?}"),
            );
            provider.reasoning = initial;
            provider.output_limit = output_limit;
            let id = ResponseId::new();
            let response = provider
                .chat_stream_with_context(
                    &[user_message("read foo")],
                    &[],
                    ProviderRequestOptions::with_reasoning(initial),
                    &ProviderRequestContext {
                        response_id: Some(id.clone()),
                        ..Default::default()
                    },
                    CancellationToken::new(),
                    sink(),
                )
                .await
                .unwrap();
            let restored = NativeReplayStore::default();
            restored
                .restore(
                    &serde_json::from_slice::<Vec<NativeReplayRecord>>(
                        &serde_json::to_vec(&provider.native_replay.snapshot()).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
            let mut rebuilt = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
            rebuilt.reasoning_codec = codec;
            rebuilt.native_replay_scope = provider.native_replay_scope.clone();
            rebuilt.reasoning = requested;
            rebuilt.output_limit = output_limit;
            rebuilt.set_native_replay_store(restored.clone());
            rebuilt
                .tool_call_identities
                .restore(&provider.tool_call_identities.snapshot());
            let history = vec![
                user_message("read foo"),
                tool_call_message(&response.tool_calls[0].id, "read", "{\"path\":\"foo\"}"),
                tool_result_message(&response.tool_calls[0].id, "contents"),
                crate::agent::test_tool_image_message("image/png", "AAAA", "Image file: foo.png"),
                named_user_message("bonsai_background", "background note"),
                named_user_message(
                    crate::context::PROJECT_STATE_MESSAGE_NAME,
                    "updated project state",
                ),
            ];
            let second_id = ResponseId::new();
            let context = ProviderRequestContext {
                assistant_response_ids: Some(vec![Some(id.clone())]),
                response_id: Some(second_id.clone()),
            };
            assert_eq!(
                rebuilt
                    .reasoning_for_request(
                        &history,
                        ProviderRequestOptions::with_reasoning(requested),
                        &context
                    )
                    .unwrap(),
                initial
            );
            let mut partial_history = history.clone();
            partial_history.push(crate::agent::test_incomplete_assistant_message(
                "partial answer",
            ));
            let partial_context = ProviderRequestContext {
                assistant_response_ids: Some(vec![Some(id.clone()), None]),
                response_id: None,
            };
            let partial_body = rebuilt
                .request_body_with_context(&partial_history, &[], requested, &partial_context)
                .unwrap();
            let last = partial_body["messages"].as_array().unwrap().last().unwrap();
            assert_eq!(
                last["role"], "user",
                "interrupted bytes must not become assistant prefill"
            );
            assert!(last.to_string().contains("partial answer"));
            assert!(last.to_string().contains("untrusted"));
            assert_eq!(
                rebuilt
                    .reasoning_for_request(
                        &partial_history,
                        ProviderRequestOptions::with_reasoning(requested),
                        &partial_context
                    )
                    .unwrap(),
                initial
            );
            assert_eq!(
                rebuilt
                    .request_body_with_context(&partial_history, &[], requested, &partial_context)
                    .unwrap()
                    .get("thinking"),
                provider
                    .request_body(&[user_message("read foo")], &[])
                    .unwrap()
                    .get("thinking")
            );
            let completed_context = ProviderRequestContext {
                assistant_response_ids: Some(vec![Some(id.clone()), Some(ResponseId::new())]),
                response_id: None,
            };
            assert_eq!(
                rebuilt
                    .reasoning_for_request(&partial_history, Default::default(), &completed_context)
                    .unwrap(),
                requested
            );
            let mut legacy_history = history.clone();
            legacy_history.push(crate::provider::test_utils::assistant_message(
                "completed legacy answer",
            ));
            legacy_history.push(named_user_message("bonsai_harness", "continue"));
            assert_eq!(
                rebuilt
                    .reasoning_for_request(&legacy_history, Default::default(), &partial_context)
                    .unwrap(),
                requested
            );
            let mut unknown_tool_history = history.clone();
            unknown_tool_history.push(tool_call_message("other_call", "read", "{}"));
            unknown_tool_history.push(tool_result_message("other_call", "contents"));
            assert_eq!(
                rebuilt
                    .reasoning_for_request(
                        &unknown_tool_history,
                        Default::default(),
                        &completed_context
                    )
                    .unwrap(),
                requested
            );
            let second = rebuilt
                .chat_stream_with_context(
                    &history,
                    &[],
                    ProviderRequestOptions::with_reasoning(requested),
                    &context,
                    CancellationToken::new(),
                    sink(),
                )
                .await
                .unwrap();
            let requests = server.received_requests().await.unwrap();
            let first_body: Value = serde_json::from_slice(&requests[0].body).unwrap();
            let second_body: Value = serde_json::from_slice(&requests[1].body).unwrap();
            assert_eq!(first_body.get("thinking"), second_body.get("thinking"));
            if let Some(limit) = output_limit {
                assert!(
                    first_body["thinking"]["budget_tokens"].as_u64().unwrap() < u64::from(limit)
                );
                assert_ne!(first_body["thinking"]["budget_tokens"], json!(32768));
            }
            assert_eq!(
                first_body.get("output_config"),
                second_body.get("output_config")
            );
            let saved = restored.snapshot();
            assert_eq!(saved.len(), 2);
            assert!(saved.iter().all(|record|matches!(&record.payload,crate::provider::NativeReplayPayload::AnthropicMessages(native) if native.request_settings.as_ref().is_some_and(|settings|settings.reasoning==initial))));

            let mut next_history = history;
            next_history.push(tool_call_message(&second.tool_calls[0].id, "read", "{}"));
            next_history.push(tool_result_message(
                &second.tool_calls[0].id,
                "more contents",
            ));
            let next_context = ProviderRequestContext {
                assistant_response_ids: Some(vec![Some(id), Some(second_id)]),
                response_id: None,
            };
            assert_eq!(
                rebuilt
                    .reasoning_for_request(&next_history, Default::default(), &next_context)
                    .unwrap(),
                initial
            );
            next_history.push(user_message("now change the task"));
            assert_eq!(
                rebuilt
                    .reasoning_for_request(&next_history, Default::default(), &next_context)
                    .unwrap(),
                requested
            );
            let new_body = rebuilt
                .request_body_with_context(&next_history, &[], requested, &next_context)
                .unwrap();
            assert_eq!(
                new_body.get("thinking"),
                rebuilt.thinking_payload_for(requested).as_ref()
            );
        }
    }

    #[test]
    fn native_anthropic_locked_budget_cannot_be_silently_shrunk_by_new_output_limit() {
        use crate::provider::{
            AnthropicNativeReplay, AnthropicReplaySettings, NativeReplayPayload,
            NativeReplayRecord, ProviderRequestContext, ResponseId,
        };
        let mut provider = concrete_anthropic_provider(ReasoningSelection::Off, None);
        let id = ResponseId::new();
        provider
            .native_replay
            .insert(NativeReplayRecord {
                response_id: id.clone(),
                provider_scope: provider.native_replay_scope.clone(),
                payload: NativeReplayPayload::AnthropicMessages(AnthropicNativeReplay {
                    budget_evidence: None,
                    content_blocks: None,
                    request_settings: Some(AnthropicReplaySettings {
                        reasoning: ReasoningSelection::High,
                        thinking: Some(json!({"type":"enabled","budget_tokens":8000})),
                        output_config: None,
                    }),
                }),
            })
            .unwrap();
        provider.output_limit = Some(4000);
        let history = [
            tool_call_message("call_1", "read", "{}"),
            tool_result_message("call_1", "contents"),
        ];
        let context = ProviderRequestContext {
            assistant_response_ids: Some(vec![Some(id)]),
            response_id: None,
        };
        let error = provider
            .request_body_with_context(&history, &[], ReasoningSelection::Off, &context)
            .unwrap_err();
        assert!(error.to_string().contains("cannot preserve"));
        assert!(provider.take_last_request_diagnostics().is_none());
    }

    #[test]
    fn native_anthropic_legacy_payload_without_settings_remains_readable() {
        let native: crate::provider::AnthropicNativeReplay = serde_json::from_value(
            json!({"content_blocks":[{"type":"thinking","thinking":"","signature":"exact=="}]}),
        )
        .unwrap();
        assert_eq!(native.content_blocks.unwrap()[0]["signature"], "exact==");
        assert!(native.request_settings.is_none());
    }

    #[tokio::test]
    async fn native_anthropic_final_callback_cancellation_does_not_pin_new_settings() {
        #[derive(Debug)]
        struct CancelOnDone(CancellationToken);
        impl crate::output::OutputSink for CancelOnDone {
            fn assistant_done(&self) {
                self.0.cancel();
            }
        }
        use crate::provider::{ProviderRequestContext, ResponseId};
        let server = MockServer::start().await;
        let blocks = [
            json!({"type":"text","text":"partial"}),
            json!({"type":"tool_use","id":"toolu_1","name":"read","input":{}}),
        ];
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(native_content_sse(&blocks, Some("tool_use"))),
            )
            .mount(&server)
            .await;
        let provider = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
        let cancellation = CancellationToken::new();
        let response = provider
            .chat_stream_with_context(
                &[],
                &[],
                Default::default(),
                &ProviderRequestContext {
                    response_id: Some(ResponseId::new()),
                    ..Default::default()
                },
                cancellation.clone(),
                Arc::new(CancelOnDone(cancellation)),
            )
            .await
            .unwrap();
        assert!(response.is_interrupted());
        assert!(provider.native_replay.snapshot().is_empty());
    }

    #[tokio::test]
    async fn native_anthropic_content_survives_rebuild_with_redacted_and_empty_signed_blocks() {
        use crate::provider::{
            NativeReplayRecord, NativeReplayStore, ProviderRequestContext, ResponseId,
            ToolCallIdentityStore,
        };
        let server = MockServer::start().await;
        let input = json!({"path":"foo","nested":{"id":"toolu_shared","value":"π"}});
        let expected = json!([
            {"type":"thinking","thinking":"initial π then more","signature":"prefix:ABC",
             "extension":{"id":"toolu_shared","opaque":"exact=="}},
            {"type":"redacted_thinking","data":"encrypted==","extension":{"id":"opaque"}},
            {"type":"thinking","thinking":"","signature":"empty:XYZ"},
            {"type":"text","text":"same answer","extension":"text metadata"},
            {"type":"tool_use","id":"toolu_shared","name":"read","input":input,
             "extension":{"id":"toolu_shared"}}
        ]);
        let event = |name: &str, value: Value| format!("event: {name}\ndata: {value}");
        let frames = vec![
            event(
                "message_start",
                json!({"message":{"usage":{"input_tokens":12,"output_tokens":1}}}),
            ),
            event(
                "content_block_start",
                json!({"index":0,"content_block":{
                    "type":"thinking","thinking":"initial π ","signature":"prefix:",
                    "extension":{"id":"toolu_shared","opaque":"exact=="}
                }}),
            ),
            event(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"thinking_delta","thinking":"then more"}}),
            ),
            event(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"signature_delta","signature":"A"}}),
            ),
            event(
                "content_block_delta",
                json!({"index":0,"delta":{"type":"signature_delta","signature":"BC"}}),
            ),
            event("content_block_stop", json!({"index":0})),
            event(
                "content_block_start",
                json!({"index":1,"content_block":expected[1]}),
            ),
            event("content_block_stop", json!({"index":1})),
            event(
                "content_block_start",
                json!({"index":2,"content_block":{"type":"thinking","thinking":"","signature":"empty:"}}),
            ),
            event(
                "content_block_delta",
                json!({"index":2,"delta":{"type":"thinking_delta","thinking":""}}),
            ),
            event(
                "content_block_delta",
                json!({"index":2,"delta":{"type":"signature_delta","signature":"XYZ"}}),
            ),
            event("content_block_stop", json!({"index":2})),
            event(
                "content_block_start",
                json!({"index":3,"content_block":{"type":"text","text":"same ","extension":"text metadata"}}),
            ),
            event(
                "content_block_delta",
                json!({"index":3,"delta":{"type":"text_delta","text":"answer"}}),
            ),
            event("content_block_stop", json!({"index":3})),
            event(
                "content_block_start",
                json!({"index":4,"content_block":{"type":"tool_use","id":"toolu_shared","name":"read","input":{},"extension":{"id":"toolu_shared"}}}),
            ),
            event(
                "content_block_delta",
                json!({"index":4,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"foo\","}}),
            ),
            event(
                "content_block_delta",
                json!({"index":4,"delta":{"type":"input_json_delta","partial_json":"\"nested\":{\"id\":\"toolu_shared\",\"value\":\"π\"}}"}}),
            ),
            event("content_block_stop", json!({"index":4})),
            event(
                "message_delta",
                json!({"delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":100}}),
            ),
            event("message_stop", json!({})),
        ];
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(frames.join("\n\n") + "\n\n"))
            .mount(&server)
            .await;
        let replay = NativeReplayStore::default();
        let identities = ToolCallIdentityStore::default();
        let mut provider = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
        provider.set_native_replay_store(replay.clone());
        provider.set_tool_call_identity_store(identities.clone());
        let id = ResponseId::new();
        let recording = Arc::new(RecordingSink::default());
        let response = provider
            .chat_stream_with_context(
                &[user_message("read foo")],
                &[],
                Default::default(),
                &ProviderRequestContext {
                    response_id: Some(id.clone()),
                    ..Default::default()
                },
                CancellationToken::new(),
                recording.clone(),
            )
            .await
            .unwrap();
        assert_eq!(response.content, "same answer");
        assert_eq!(recording.assistant_deltas().concat(), response.content);
        assert_eq!(recording.reasoning_deltas().concat(), "initial π then more");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&response.tool_calls[0].arguments).unwrap(),
            input
        );

        let restored = NativeReplayStore::default();
        restored
            .restore(
                &serde_json::from_slice::<Vec<NativeReplayRecord>>(
                    &serde_json::to_vec(&replay.snapshot()).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let restored_ids = ToolCallIdentityStore::default();
        restored_ids.restore(
            &serde_json::from_slice::<Vec<crate::provider::ToolCallIdentity>>(
                &serde_json::to_vec(&identities.snapshot()).unwrap(),
            )
            .unwrap(),
        );
        let mut rebuilt = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
        rebuilt.set_native_replay_store(restored.clone());
        rebuilt.set_tool_call_identity_store(restored_ids);
        let mut assistant = tool_call_message(&response.tool_calls[0].id, "read", "{}");
        if let ChatCompletionRequestMessage::Assistant(row) = &mut assistant {
            row.content = Some(
                async_openai::types::chat::ChatCompletionRequestAssistantMessageContent::Text(
                    response.content,
                ),
            );
        }
        let history = [
            user_message("read foo"),
            assistant,
            tool_result_message(&response.tool_calls[0].id, "contents"),
        ];
        let context = ProviderRequestContext {
            assistant_response_ids: Some(vec![Some(id.clone())]),
            response_id: Some(ResponseId::new()),
        };
        rebuilt
            .chat_stream_with_context(
                &history,
                &[],
                Default::default(),
                &context,
                CancellationToken::new(),
                sink(),
            )
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(body["messages"][1]["content"], expected);
        assert_eq!(
            body["messages"][2]["content"][0]["tool_use_id"],
            "toolu_shared"
        );
        assert!(
            !String::from_utf8(requests[1].body.clone())
                .unwrap()
                .contains(
                    &serde_json::from_value::<String>(serde_json::to_value(&id).unwrap()).unwrap()
                )
        );
        assert_eq!(restored.snapshot().len(), 2);
        let plain = rebuilt.preview_request(&history, &[]).unwrap().body;
        assert_eq!(plain["messages"][1]["content"][0]["type"], "text");
        assert_eq!(plain["messages"][1]["content"][1]["input"], json!({}));
        let mut other = loopback_anthropic_provider(&server, "claude-haiku-4-5");
        other.set_native_replay_store(restored);
        let absent = other
            .preview_request_with_context(&history, &[], &context)
            .unwrap()
            .body;
        assert_eq!(absent["messages"][1]["content"][0]["type"], "text");
    }

    #[tokio::test]
    async fn native_anthropic_repeated_plain_replies_select_provenance_before_role_merge() {
        use crate::provider::{ProviderRequestContext, ResponseId};
        let server = MockServer::start().await;
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                let index = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_string(native_content_sse(&[
                json!({"type":"thinking","thinking":"","signature":format!("trace-{index}")}),
                json!({"type":"text","text":"same answer"}),
            ],Some("end_turn")))
            })
            .mount(&server)
            .await;
        let provider = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
        let ids = [ResponseId::new(), ResponseId::new()];
        for id in &ids {
            let response = provider
                .chat_stream_with_context(
                    &[],
                    &[],
                    Default::default(),
                    &ProviderRequestContext {
                        response_id: Some(id.clone()),
                        ..Default::default()
                    },
                    CancellationToken::new(),
                    sink(),
                )
                .await
                .unwrap();
            assert_eq!(response.content, "same answer");
        }
        let history = [
            assistant_message("same answer"),
            assistant_message("same answer"),
            assistant_message("same answer"),
        ];
        let preview = provider
            .preview_request_with_context(
                &history,
                &[],
                &ProviderRequestContext {
                    assistant_response_ids: Some(vec![
                        Some(ids[1].clone()),
                        None,
                        Some(ids[0].clone()),
                    ]),
                    response_id: None,
                },
            )
            .unwrap()
            .body;
        let content = preview["messages"][0]["content"].as_array().unwrap();
        assert_eq!(preview["messages"].as_array().unwrap().len(), 1);
        assert_eq!(content.len(), 5);
        assert_eq!(content[0]["signature"], "trace-1");
        assert_eq!(content[2], json!({"type":"text","text":"same answer"}));
        assert_eq!(content[3]["signature"], "trace-0");
        assert!(
            provider
                .preview_request_with_context(
                    &history,
                    &[],
                    &ProviderRequestContext {
                        assistant_response_ids: Some(vec![Some(ids[0].clone())]),
                        response_id: None,
                    }
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_anthropic_incomplete_failed_and_cancelled_streams_create_no_replay() {
        use crate::provider::{NativeReplayStore, ProviderRequestContext, ResponseId};
        let blocks = [
            json!({"type":"thinking","thinking":"","signature":"opaque=="}),
            json!({"type":"text","text":"partial"}),
        ];
        for finish in [
            None,
            Some("max_tokens"),
            Some("model_context_window_exceeded"),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_string(native_content_sse(&blocks, finish)),
                )
                .mount(&server)
                .await;
            let store = NativeReplayStore::default();
            let mut provider = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
            provider.set_native_replay_store(store.clone());
            let result = provider
                .chat_stream_with_context(
                    &[],
                    &[],
                    Default::default(),
                    &ProviderRequestContext {
                        response_id: Some(ResponseId::new()),
                        ..Default::default()
                    },
                    CancellationToken::new(),
                    sink(),
                )
                .await;
            if finish.is_none() {
                assert!(result.is_err());
            } else {
                assert_eq!(
                    result.unwrap().finish_reason(),
                    Some(&crate::provider::FinishReason::Length)
                );
            }
            assert!(store.snapshot().is_empty());
        }
        struct CancelOnText(CancellationToken);
        impl OutputSink for CancelOnText {
            fn assistant_delta(&self, _: &str) {
                self.0.cancel();
            }
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(native_content_sse(&blocks, Some("end_turn"))),
            )
            .mount(&server)
            .await;
        let mut provider = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
        let store = NativeReplayStore::default();
        provider.set_native_replay_store(store.clone());
        let cancel = CancellationToken::new();
        let response = provider
            .chat_stream_with_context(
                &[],
                &[],
                Default::default(),
                &ProviderRequestContext {
                    response_id: Some(ResponseId::new()),
                    ..Default::default()
                },
                cancel.clone(),
                Arc::new(CancelOnText(cancel)),
            )
            .await
            .unwrap();
        assert!(response.is_interrupted());
        assert!(store.snapshot().is_empty());
    }

    #[tokio::test]
    async fn native_anthropic_unclosed_blocks_cannot_complete_or_admit_tool_calls() {
        use crate::provider::{NativeReplayStore, ProviderRequestContext, ResponseId};
        for with_tool in [false, true] {
            let server = MockServer::start().await;
            let mut blocks = vec![json!({"type":"thinking","thinking":"","signature":"exact=="})];
            if with_tool {
                blocks.push(
                    json!({"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"foo"}}),
                );
            }
            let stream = native_content_sse(&blocks, None)
                + &format!(
                    "event: content_block_start\ndata: {}\n\nevent: message_stop\ndata: {{}}\n\n",
                    json!({"index":blocks.len(),"content_block":{"type":"text","text":"unfinished"}}),
                );
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_string(stream))
                .mount(&server)
                .await;
            let store = NativeReplayStore::default();
            let mut provider = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
            provider.set_native_replay_store(store.clone());
            let error = provider
                .chat_stream_with_context(
                    &[],
                    &[],
                    Default::default(),
                    &ProviderRequestContext {
                        response_id: Some(ResponseId::new()),
                        ..Default::default()
                    },
                    CancellationToken::new(),
                    sink(),
                )
                .await
                .expect_err("unclosed native blocks cannot complete");
            assert!(error.to_string().contains("unclosed content block"));
            assert!(store.snapshot().is_empty());
            assert!(provider.tool_call_identities.snapshot().is_empty());
        }
    }

    #[tokio::test]
    async fn native_anthropic_malformed_inputs_preserve_unsigned_repair_and_reject_signed_reconstruction()
     {
        use crate::provider::{NativeReplayStore, ProviderRequestContext, ResponseId};
        let raw = "{broken π";
        for signed in [false, true] {
            let server = MockServer::start().await;
            let thinking = if signed {
                vec![json!({"type":"thinking","thinking":"","signature":"exact=="})]
            } else {
                vec![]
            };
            let index = thinking.len();
            let stream = native_content_sse(&thinking, None)
                + &format!(
                    "event: content_block_start\ndata: {}\n\nevent: content_block_delta\ndata: {}\n\nevent: content_block_stop\ndata: {}\n\nevent: message_delta\ndata: {}\n\nevent: message_stop\ndata: {{}}\n\n",
                    json!({"index":index,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}),
                    json!({"index":index,"delta":{"type":"input_json_delta","partial_json":raw}}),
                    json!({"index":index}),
                    json!({"delta":{"stop_reason":"tool_use"}}),
                );
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_string(stream))
                .mount(&server)
                .await;
            let store = NativeReplayStore::default();
            let mut provider = loopback_anthropic_provider(&server, "claude-sonnet-4-5");
            provider.set_native_replay_store(store.clone());
            let result = provider
                .chat_stream_with_context(
                    &[],
                    &[],
                    Default::default(),
                    &ProviderRequestContext {
                        response_id: Some(ResponseId::new()),
                        ..Default::default()
                    },
                    CancellationToken::new(),
                    sink(),
                )
                .await;
            if signed {
                let error = result.expect_err("signed native input cannot be repaired for replay");
                let detail = error.to_string();
                assert!(detail.contains("read"));
                assert!(detail.contains(raw));
                assert!(detail.contains("parse error"));
                assert!(!error.is_retryable());
                assert!(provider.tool_call_identities.snapshot().is_empty());
                assert!(store.snapshot().is_empty());
            } else {
                let response = result.unwrap();
                assert_eq!(response.tool_calls.len(), 1);
                assert_eq!(response.tool_calls[0].arguments, raw);
                assert_eq!(
                    response.finish_reason(),
                    Some(&crate::provider::FinishReason::ToolCalls)
                );
                let records = store.snapshot();
                assert_eq!(records.len(), 1);
                let crate::provider::NativeReplayPayload::AnthropicMessages(native) =
                    &records[0].payload
                else {
                    panic!("wrong transport")
                };
                assert!(native.content_blocks.is_none());
                assert!(native.request_settings.is_some());
            }
        }
    }

    #[tokio::test]
    async fn transform_messages_extracts_system_and_tool_results() {
        let messages = vec![
            system_message("You are helpful."),
            user_message("Read foo"),
            tool_call_message("toolu_1", "read", "{\"path\":\"foo\"}"),
            tool_result_message("toolu_1", "file contents"),
        ];

        let (system, anthropic_messages) = transform_messages(&messages).unwrap();
        assert_eq!(system, "You are helpful.");
        assert_eq!(anthropic_messages.len(), 3);

        let user = &anthropic_messages[0];
        assert_eq!(user["role"], "user");
        let assistant = &anthropic_messages[1];
        assert_eq!(assistant["role"], "assistant");
        let tool_block = &assistant["content"][0];
        assert_eq!(tool_block["type"], "tool_use");
        assert_eq!(tool_block["id"], "toolu_1");
        assert_eq!(tool_block["name"], "read");

        let tool_result = &anthropic_messages[2];
        assert_eq!(tool_result["role"], "user");
        assert_eq!(tool_result["content"][0]["type"], "tool_result");
        assert_eq!(tool_result["content"][0]["tool_use_id"], "toolu_1");
    }

    #[test]
    fn transform_messages_merges_parallel_tool_results_into_one_user_turn() {
        let messages = vec![
            system_message("You are helpful."),
            user_message("Read both files"),
            tool_call_message("toolu_1", "read", "{\"path\":\"one.rs\"}"),
            tool_result_message("toolu_1", "one contents"),
            tool_result_message("toolu_2", "two contents"),
        ];

        let (_system, anthropic_messages) = transform_messages(&messages).unwrap();
        assert_eq!(anthropic_messages.len(), 3);
        let results = &anthropic_messages[2];
        assert_eq!(results["role"], "user");
        assert_eq!(results["content"].as_array().unwrap().len(), 2);
        assert_eq!(results["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(results["content"][1]["tool_use_id"], "toolu_2");
    }

    #[test]
    fn image_source_embeds_data_uri_as_base64() {
        let source = anthropic_image_source("data:image/png;base64,aGVsbG8=");
        assert_eq!(source["type"], "base64");
        assert_eq!(source["media_type"], "image/png");
        assert_eq!(source["data"], "aGVsbG8=");
    }

    #[test]
    fn image_source_passes_remote_url_through() {
        let source = anthropic_image_source("https://example.com/cat.png");
        assert_eq!(source["type"], "url");
        assert_eq!(source["url"], "https://example.com/cat.png");
    }

    #[test]
    fn user_image_content_part_becomes_base64_image_block() {
        let content = serde_json::json!([
            {"type": "text", "text": "what is this?"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
        ]);
        let blocks = content_to_anthropic_user(Some(&content));
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "QUJD");
    }

    #[tokio::test]
    async fn authorize_from_env_reads_key() {
        let factory = AnthropicFactory;
        crate::util::test_env::with_var_async("ANTHROPIC_API_KEY", Some("sk-test"), async {
            let outcome = factory.authorize(AuthInput::FromEnv).await.unwrap();
            assert_eq!(outcome.api_key, "sk-test");
        })
        .await;
    }

    #[tokio::test]
    async fn authorize_rejects_empty_api_key() {
        let factory = AnthropicFactory;
        let result = factory
            .authorize(AuthInput::ApiKey {
                api_key: "   ".to_string(),
                persistence: crate::session::CredentialPersistence::File,
                base_url: None,
            })
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn authorize_rejects_codex_cache_input() {
        let factory = AnthropicFactory;
        let result = factory.authorize(AuthInput::FromCodexCache).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn is_authorized_checks_api_key_presence() {
        let factory = AnthropicFactory;
        let mut session = make_session();
        session.api_key.clear();
        assert!(!factory.is_authorized(&session));
        session.api_key = "sk-test".to_string();
        assert!(factory.is_authorized(&session));
    }

    #[tokio::test]
    async fn clear_authorization_wipes_api_key() {
        let factory = AnthropicFactory;
        let mut session = make_session();
        factory.clear_authorization(&mut session);
        assert!(session.api_key.is_empty());
    }

    #[tokio::test]
    async fn list_models_returns_seed_list_without_api_key() {
        let factory = AnthropicFactory;
        let mut session = make_session();
        session.api_key.clear();
        assert_eq!(factory.metadata().context_window, Some(200_000));
        let models = factory.list_models(&session).await.unwrap();
        assert!(models.contains(&"claude-sonnet-4-5".to_string()));
        assert!(models.contains(&"claude-sonnet-5".to_string()));
        assert!(models.contains(&"claude-opus-5".to_string()));
        assert!(models.contains(&"claude-opus-4-8".to_string()));
        assert!(models.contains(&"claude-fable-5".to_string()));
        assert!(models.contains(&"claude-haiku-4-5".to_string()));
        assert!(models.contains(&"claude-opus-4-1".to_string()));
    }

    #[tokio::test]
    async fn list_available_models_fetches_live_lineup_with_context_windows() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("anthropic-version", ANTHROPIC_API_VERSION))
            .and(header("x-api-key", "sk-ant-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{
                    "data": [
                        {"type": "model", "id": "claude-sonnet-5",
                         "display_name": "Claude Sonnet 5",
                         "max_input_tokens": 1000000, "max_tokens": 128000,
                         "capabilities": {
                           "effort": {
                             "supported": true,
                             "low": {"supported": true},
                             "medium": {"supported": true},
                             "high": {"supported": true},
                             "xhigh": {"supported": true},
                             "max": {"supported": true}
                           },
                           "image_input": {"supported": true},
                           "pdf_input": {"supported": true},
                           "structured_outputs": {"supported": true},
                           "thinking": {
                             "supported": true,
                             "types": {
                               "adaptive": {"supported": true},
                               "enabled": {"supported": false}
                             }
                           }
                         }},
                        {"type": "model", "id": "claude-fable-5",
                         "display_name": "Claude Fable 5",
                         "max_input_tokens": 1000000, "max_tokens": 128000,
                         "capabilities": {
                           "effort": {
                             "supported": true,
                             "low": {"supported": true},
                             "medium": {"supported": true},
                             "high": {"supported": true},
                             "xhigh": {"supported": true},
                             "max": {"supported": true}
                           },
                           "thinking": {
                             "supported": true,
                             "types": {
                               "adaptive": {"supported": true}
                             }
                           }
                         }},
                        {"type": "model", "id": "claude-opus-4-5",
                         "display_name": "Claude Opus 4.5",
                         "max_input_tokens": 200000, "max_tokens": 64000,
                         "capabilities": {
                           "effort": {
                             "supported": true,
                             "low": {"supported": true},
                             "medium": {"supported": true},
                             "high": {"supported": true}
                           },
                           "thinking": {
                             "supported": true,
                             "types": {
                               "adaptive": {"supported": false},
                               "enabled": {"supported": true}
                             }
                           }
                         }},
                        {"type": "model", "id": "claude-haiku-4-5",
                         "display_name": "Claude Haiku 4.5",
                         "max_input_tokens": 200000, "max_tokens": 64000,
                         "capabilities": {
                           "effort": {"supported": false},
                           "thinking": {
                             "supported": true,
                             "types": {
                               "adaptive": {"supported": false},
                               "enabled": {"supported": true}
                             }
                           }
                         }},
                        {"type": "model", "id": "claude-mystery",
                         "max_input_tokens": 0, "max_tokens": -1}
                    ],
                    "has_more": false,
                    "first_id": "claude-sonnet-5",
                    "last_id": "claude-mystery"
                }"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let factory = AnthropicFactory;
        let mut session = make_session();
        session.base_url = server.uri();
        let availability = factory.list_available_models(&session).await.unwrap();

        assert_eq!(availability.models.len(), 5);
        let sonnet = &availability.models[0];
        assert_eq!(sonnet.remote_model_id.as_ref(), "claude-sonnet-5");
        assert_eq!(sonnet.context_window, Some(1_000_000));
        assert_eq!(sonnet.output_limit, Some(128_000));
        assert_eq!(sonnet.display_name.as_deref(), Some("Claude Sonnet 5"));
        assert_eq!(
            sonnet.features,
            vec![
                ModelFeature::ToolCall,
                ModelFeature::Attachment,
                ModelFeature::StructuredOutput,
                ModelFeature::Reasoning,
            ]
        );
        assert_eq!(
            sonnet.supported_reasoning,
            vec![
                ReasoningSelection::Off,
                ReasoningSelection::Low,
                ReasoningSelection::Medium,
                ReasoningSelection::High,
                ReasoningSelection::XHigh,
                ReasoningSelection::Max,
            ]
        );
        assert_eq!(sonnet.recommended_reasoning, Some(ReasoningSelection::High));
        assert_eq!(
            sonnet.reasoning_codec,
            Some(ReasoningCodec::AnthropicAdaptive)
        );

        let fable = &availability.models[1];
        assert!(!fable.supported_reasoning.contains(&ReasoningSelection::Off));
        assert_eq!(
            fable.reasoning_codec,
            Some(ReasoningCodec::AnthropicAdaptive)
        );
        let opus = &availability.models[2];
        assert_eq!(
            opus.reasoning_codec,
            Some(ReasoningCodec::AnthropicThinkingWithEffort)
        );
        assert_eq!(
            opus.supported_reasoning,
            vec![
                ReasoningSelection::Off,
                ReasoningSelection::Low,
                ReasoningSelection::Medium,
                ReasoningSelection::High,
            ]
        );
        // Non-positive limits stay unknown so static or models.dev catalog
        // metadata keeps governing.
        assert_eq!(availability.models[4].context_window, None);
        assert_eq!(availability.models[4].output_limit, None);
        assert!(availability.models[4].features.is_empty());
        // Manual-only rows leave the selection details unknown so static or
        // models.dev budget-token controls remain authoritative.
        assert!(availability.models[3].supported_reasoning.is_empty());
        assert_eq!(availability.models[3].reasoning_codec, None);
    }

    #[tokio::test]
    async fn list_available_models_falls_back_to_seeds_on_empty_listing() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(r#"{"data": [], "has_more": false}"#),
            )
            .mount(&server)
            .await;

        let factory = AnthropicFactory;
        let mut session = make_session();
        session.base_url = server.uri();
        let models = factory.list_models(&session).await.unwrap();
        assert!(models.contains(&"claude-sonnet-4-5".to_string()));
    }

    #[tokio::test]
    async fn list_available_models_propagates_http_errors_with_key() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let factory = AnthropicFactory;
        let mut session = make_session();
        session.base_url = server.uri();
        // An error with a key must NOT silently degrade to seeds: the refresh
        // path relies on Err to preserve an existing live cache.
        assert!(factory.list_available_models(&session).await.is_err());
    }

    // ---- Prompt-cache activation -------------------------------------------

    const VOLATILE_HEADING: &str = crate::context::VOLATILE_STATE_HEADING;

    /// Build a concrete Anthropic provider with the prompt-cache flag forced.
    fn cache_provider(supports_prompt_cache: bool) -> AnthropicCompatibleProvider {
        let session = make_session();
        let target = crate::provider::fallback_run_target(&ANTHROPIC_METADATA, &session);
        AnthropicCompatibleProvider::new(
            "anthropic",
            &session,
            &target,
            AnthropicTransportFlags {
                require_api_key: true,
                supports_prompt_cache,
                supports_vision: true,
            },
            "x-api-key",
        )
    }

    fn rolling_history_cache_provider() -> AnthropicCompatibleProvider {
        let session = make_session();
        let mut target = crate::provider::fallback_run_target(&ANTHROPIC_METADATA, &session);
        target.prompt_cache_policy = crate::model_catalog::PromptCachePolicy::RollingHistory;
        AnthropicCompatibleProvider::new(
            "opencode",
            &session,
            &target,
            AnthropicTransportFlags {
                require_api_key: true,
                supports_prompt_cache: true,
                supports_vision: true,
            },
            "x-api-key",
        )
    }

    /// Count `cache_control` markers anywhere in a JSON value.
    fn count_cache_control(value: &Value) -> usize {
        match value {
            Value::Object(map) => {
                usize::from(map.contains_key("cache_control"))
                    + map.values().map(count_cache_control).sum::<usize>()
            }
            Value::Array(items) => items.iter().map(count_cache_control).sum(),
            _ => 0,
        }
    }

    fn system_with_volatile() -> String {
        format!(
            "You are a coding agent.\n\n# Project context\n\n## Environment\n- cwd: /repo\n\n{VOLATILE_HEADING}\n- git: dirty"
        )
    }

    #[test]
    fn request_body_emits_three_cache_breakpoints() {
        let provider = cache_provider(true);
        let messages = vec![
            system_message(&system_with_volatile()),
            user_message("hello"),
        ];
        let body = provider.request_body(&messages, &[sample_tool()]).unwrap();

        // Tools prefix + system head + history tail = exactly three breakpoints.
        assert_eq!(count_cache_control(&body), 3);

        // System splits into [cached head, uncached volatile tail].
        let system = body["system"]
            .as_array()
            .expect("system should be a block array when caching");
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["cache_control"], json!({"type": "ephemeral"}));
        assert!(system[1].get("cache_control").is_none());
        assert!(
            system[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("You are a coding agent.")
        );
        assert!(
            system[1]["text"]
                .as_str()
                .unwrap()
                .contains(VOLATILE_HEADING)
        );

        // The last tool carries the tools breakpoint.
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(
            tools.last().unwrap()["cache_control"],
            json!({"type": "ephemeral"})
        );

        // The last content block of the last message carries the rolling breakpoint.
        let last_block = body["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(last_block["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn durable_project_state_preserves_anthropic_cache_wire_layout() {
        let provider = cache_provider(true);
        let volatile = format!("{VOLATILE_HEADING}\n- git: dirty");
        let legacy = provider
            .request_body(
                &[
                    system_message(&format!("stable instructions\n\n{volatile}")),
                    user_message("hello"),
                ],
                &[sample_tool()],
            )
            .unwrap();
        let durable = provider
            .request_body(
                &[
                    system_message("stable instructions"),
                    user_message("hello"),
                    named_user_message(
                        crate::context::PROJECT_STATE_MESSAGE_NAME,
                        &transform::volatile_context_user_text(&volatile),
                    ),
                ],
                &[sample_tool()],
            )
            .unwrap();

        assert_eq!(
            durable, legacy,
            "IMPORTANT: do not change Anthropic's proven explicit-breakpoint layout"
        );
    }

    #[test]
    fn rolling_history_breakpoint_stays_before_mutable_project_state() {
        let provider = rolling_history_cache_provider();
        assert_eq!(
            provider.project_state_cache_strategy(),
            crate::provider::ProjectStateCacheStrategy::AppendOnlyHistory,
            "the agent and Anthropic wire builder must share the rolling policy"
        );
        let first_state = format!("{VOLATILE_HEADING}\n- read coverage: none");
        let first = provider
            .request_body(
                &[
                    system_message(&format!("stable instructions\n\n{first_state}")),
                    user_message("read LICENSE"),
                ],
                &[sample_tool()],
            )
            .unwrap();

        let first_system = first["system"].as_array().unwrap();
        assert_eq!(first_system.len(), 1);
        assert_eq!(first_system[0]["text"], json!("stable instructions"));
        let first_blocks = first["messages"][0]["content"].as_array().unwrap();
        assert_eq!(first_blocks.len(), 2);
        assert_eq!(
            first_blocks[0]["cache_control"],
            json!({"type": "ephemeral"}),
            "the user request, not mutable state, is the rolling checkpoint"
        );
        assert!(first_blocks[1].get("cache_control").is_none());
        assert!(is_project_state_block(&first_blocks[1]));

        let second_state = format!("{VOLATILE_HEADING}\n- read coverage: LICENSE");
        let second = provider
            .request_body(
                &[
                    system_message(&format!("stable instructions\n\n{second_state}")),
                    user_message("read LICENSE"),
                    tool_call_message("call-1", "read", r#"{"path":"LICENSE"}"#),
                    tool_result_message("call-1", "MIT License"),
                ],
                &[sample_tool()],
            )
            .unwrap();

        assert_eq!(first["system"], second["system"]);
        let second_blocks = second["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_array()
            .unwrap();
        assert_eq!(second_blocks[0]["type"], json!("tool_result"));
        assert_eq!(
            second_blocks[0]["cache_control"],
            json!({"type": "ephemeral"}),
            "the complete tool loop must be reusable on the next request"
        );
        assert!(second_blocks[1].get("cache_control").is_none());
        assert!(is_project_state_block(&second_blocks[1]));
        assert_eq!(count_cache_control(&second), 3);
    }

    /// Compare the actual serialized prefix, retaining message boundaries and
    /// cache markers; stripping those would hide the regression in #27.
    fn rolling_history_wire_prefix(body: &Value) -> Vec<u8> {
        let messages = body["messages"].as_array().unwrap();
        let last_message = messages.last().unwrap();
        assert_eq!(last_message["role"], "user");
        let blocks = last_message["content"].as_array().unwrap();
        let (state, history) = blocks.split_last().unwrap();
        assert!(is_project_state_block(state));
        assert!(state.get("cache_control").is_none());
        assert_eq!(
            history.last().unwrap()["cache_control"],
            json!({"type": "ephemeral"}),
            "the checkpoint must stay on the stable block merged before state"
        );

        let wire = serde_json::to_vec(body).unwrap();
        let state_bytes = serde_json::to_vec(state).unwrap();
        let state_start = wire
            .windows(state_bytes.len())
            .position(|bytes| bytes == state_bytes)
            .expect("the serialized request must contain its mutable state block");
        wire[..state_start].to_vec()
    }

    #[test]
    fn rolling_history_wire_bytes_stay_stable_with_merged_user_messages() {
        let provider = rolling_history_cache_provider();
        let request = |state: &str| {
            provider
                .request_body(
                    &[
                        system_message("stable instructions"),
                        user_message("Read LICENSE"),
                        user_message("Report its license — keep it brief."),
                        named_user_message(
                            crate::context::PROJECT_STATE_MESSAGE_NAME,
                            &transform::volatile_context_user_text(state),
                        ),
                    ],
                    &[sample_tool()],
                )
                .unwrap()
        };
        let state = format!("{VOLATILE_HEADING}\n- git: clean");
        let first = request(&state);
        let repeated = request(&state);
        let updated = request(&format!("{VOLATILE_HEADING}\n- git: dirty"));

        assert_eq!(first["messages"].as_array().unwrap().len(), 1);
        let blocks = first["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3, "both user messages and state must merge");
        assert_eq!(blocks[0]["text"], "Read LICENSE");
        assert_eq!(blocks[1]["text"], "Report its license — keep it brief.");
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&repeated).unwrap(),
            "an unchanged request must be byte-identical"
        );
        assert_ne!(first, updated, "the updated state must reach the wire");
        assert_eq!(
            rolling_history_wire_prefix(&first),
            rolling_history_wire_prefix(&updated),
            "changing mutable state must preserve the serialized history prefix"
        );
    }

    #[test]
    fn rolling_history_wire_bytes_stay_stable_with_merged_parallel_tool_results() {
        let provider = rolling_history_cache_provider();
        let request = |state: &str| {
            provider
                .request_body(
                    &[
                        system_message("stable instructions"),
                        user_message("Read LICENSE and README.md"),
                        tool_call_message("call-1", "read", r#"{"path":"LICENSE"}"#),
                        tool_call_message("call-2", "read", r#"{"path":"README.md"}"#),
                        tool_result_message("call-1", "MIT License\nCopyright \"Bonsai\""),
                        tool_result_message("call-2", "# Bonsai\nRust TUI — coding agent"),
                        named_user_message(
                            crate::context::PROJECT_STATE_MESSAGE_NAME,
                            &transform::volatile_context_user_text(state),
                        ),
                    ],
                    &[sample_tool()],
                )
                .unwrap()
        };
        let state = format!("{VOLATILE_HEADING}\n- read coverage: LICENSE");
        let first = request(&state);
        let repeated = request(&state);
        let updated = request(&format!(
            "{VOLATILE_HEADING}\n- read coverage: LICENSE, README.md"
        ));

        assert_eq!(first["messages"].as_array().unwrap().len(), 3);
        assert_eq!(first["messages"][1]["content"].as_array().unwrap().len(), 2);
        let blocks = first["messages"][2]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 3, "both tool results and state must merge");
        assert_eq!(blocks[0]["tool_use_id"], "call-1");
        assert_eq!(blocks[1]["tool_use_id"], "call-2");
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&repeated).unwrap(),
            "an unchanged request must be byte-identical"
        );
        assert_ne!(first, updated, "the updated state must reach the wire");
        assert_eq!(
            rolling_history_wire_prefix(&first),
            rolling_history_wire_prefix(&updated),
            "changing mutable state must preserve the serialized tool-result prefix"
        );
    }

    #[test]
    fn project_state_cache_detection_accepts_every_envelope_generation() {
        for prefix in [
            crate::context::PROJECT_STATE_UPDATE_PREFIX,
            crate::context::PREVIOUS_PROJECT_STATE_UPDATE_PREFIX,
            crate::context::LEGACY_PROJECT_STATE_UPDATE_PREFIX,
        ] {
            assert!(is_project_state_block(&json!({
                "type": "text",
                "text": format!("{prefix}\n\n## Volatile state\n- git: clean")
            })));
        }
    }

    #[test]
    fn request_body_without_cache_support_has_no_breakpoints() {
        let provider = cache_provider(false);
        let messages = vec![
            system_message(&system_with_volatile()),
            user_message("hello"),
        ];
        let body = provider.request_body(&messages, &[sample_tool()]).unwrap();
        assert_eq!(count_cache_control(&body), 0);
        // System stays a plain string when caching is off.
        assert!(body["system"].is_string());
    }

    #[test]
    fn preview_sections_carry_cache_hints_matching_the_emitted_markers() {
        use crate::provider::WireCacheHint;
        let provider = cache_provider(true);
        let messages = vec![
            system_message(&system_with_volatile()),
            user_message("hello"),
        ];
        let preview = provider
            .preview_request(&messages, &[sample_tool()])
            .unwrap();
        let by_id = |id: &str| {
            preview
                .wire_sections
                .iter()
                .find(|section| section.id == id)
                .unwrap_or_else(|| panic!("{id} section"))
        };

        let tools = by_id("wire-tools");
        assert_eq!(tools.cache, Some(WireCacheHint::CachedPrefix));
        assert_eq!(
            tools.children.last().unwrap().cache,
            Some(WireCacheHint::Breakpoint),
            "the last tool carries the tools-prefix breakpoint"
        );

        let system = by_id("wire-system");
        assert_eq!(system.cache, Some(WireCacheHint::CachedPrefix));
        assert_eq!(system.children[0].cache, Some(WireCacheHint::Breakpoint));
        assert_eq!(
            system.children[1].cache,
            Some(WireCacheHint::Volatile),
            "the volatile tail after the system-head breakpoint is uncached"
        );

        let messages_section = by_id("wire-messages");
        assert_eq!(messages_section.cache, Some(WireCacheHint::CachedPrefix));

        // Parameters carry no cache annotation.
        assert_eq!(by_id("wire-model").cache, None);
    }

    #[test]
    fn preview_sections_without_cache_support_carry_no_hints() {
        let provider = cache_provider(false);
        let messages = vec![
            system_message(&system_with_volatile()),
            user_message("hello"),
        ];
        let preview = provider
            .preview_request(&messages, &[sample_tool()])
            .unwrap();
        fn all_none(section: &crate::provider::ProviderWireSection) -> bool {
            section.cache.is_none() && section.children.iter().all(all_none)
        }
        assert!(preview.wire_sections.iter().all(all_none));
    }

    #[test]
    fn cached_system_prefix_is_byte_stable_across_turns() {
        let provider = cache_provider(true);
        let persona =
            "You are a coding agent.\n\n# Project context\n\n## Environment\n- cwd: /repo";

        let turn1_system = format!("{persona}\n\n{VOLATILE_HEADING}\n- git: clean");
        let body1 = provider
            .request_body(
                &[system_message(&turn1_system), user_message("first")],
                &[sample_tool()],
            )
            .unwrap();

        // A later turn: different volatile tail and a grown history.
        let turn2_system =
            format!("{persona}\n\n{VOLATILE_HEADING}\n- git: 4 changes\n- branch: feat");
        let body2 = provider
            .request_body(
                &[
                    system_message(&turn2_system),
                    user_message("first"),
                    tool_result_message("call-1", "ok"),
                    user_message("second"),
                ],
                &[sample_tool()],
            )
            .unwrap();

        let head1 = body1["system"][0]["text"].as_str().unwrap();
        let head2 = body2["system"][0]["text"].as_str().unwrap();
        assert_eq!(
            head1, head2,
            "cached prefix must be byte-identical across turns"
        );

        // head + tail reconstructs the original system string exactly.
        let tail1 = body1["system"][1]["text"].as_str().unwrap();
        assert_eq!(format!("{head1}{tail1}"), turn1_system);
    }

    #[test]
    fn system_field_without_volatile_tail_is_one_cached_block() {
        let value = system_field("You are stable.", true);
        let blocks = value.as_array().expect("expected a block array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"], "You are stable.");
        assert_eq!(blocks[0]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn system_field_splits_on_last_volatile_marker() {
        // Compaction hoists a summary after the volatile tail; it must stay uncached.
        let system =
            format!("Persona\n\n{VOLATILE_HEADING}\n- git: dirty\n\nConversation summary so far.");
        let blocks = system_field(&system, true);
        let blocks = blocks.as_array().unwrap();
        assert_eq!(blocks[0]["text"], "Persona");
        let tail = blocks[1]["text"].as_str().unwrap();
        assert!(tail.contains("Conversation summary so far."));
        assert!(tail.starts_with(&format!("\n\n{VOLATILE_HEADING}")));
    }

    #[test]
    fn volatile_heading_const_matches_rendered_snapshot() {
        // The provider's split needle is coupled to the rendered heading; guard it.
        let snapshot = crate::context::ProjectContextSnapshot {
            environment: "## Environment\n- cwd: /repo".to_string(),
            volatile_state: format!("{VOLATILE_HEADING}\n- git: dirty"),
            steering_files: Vec::new(),
            repo_map: String::new(),
            skills_index: String::new(),
            smol_skills_index: String::new(),
            agents_index: String::new(),
            memory_index: String::new(),
            stale_read_advisory: String::new(),
            peer_status: String::new(),
        };
        assert!(
            snapshot
                .render()
                .contains(&format!("\n\n{VOLATILE_HEADING}\n"))
        );
    }
}
