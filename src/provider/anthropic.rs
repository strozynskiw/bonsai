use std::collections::HashMap;
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
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-fable-5",
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
    /// Native thinking blocks captured per assistant turn, keyed by that turn's
    /// first tool-call id, and replayed ahead of the rebuilt assistant content
    /// on later requests. The Anthropic protocol expects thinking to be echoed
    /// back through a tool loop; dropping it (the old behaviour) made models
    /// stop thinking after the first tool round (observed on MiniMax:
    /// — thinking only at turn starts, none for the rest of the run). Rebuilt
    /// empty per provider instance; resumed history simply omits thinking.
    thinking_by_call_id: Mutex<HashMap<String, Vec<Value>>>,
    last_request_diagnostics: Mutex<Option<ProviderRequestDiagnostics>>,
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
        Self {
            provider_id: provider_id.into(),
            http: crate::provider::http_client(),
            model,
            base_url,
            api_key: session.api_key.clone(),
            api_key_header: api_key_header.into(),
            endpoint_path: target
                .endpoint_path
                .as_deref()
                .unwrap_or("v1/messages")
                .to_string(),
            max_tokens: target.output_limit.unwrap_or(DEFAULT_MAX_TOKENS),
            output_limit: target.output_limit,
            reasoning: target.reasoning,
            reasoning_escalation: target.reasoning_escalation,
            // Non-Anthropic codecs make no sense on this transport. Preserve
            // both current Anthropic shapes plus the Opus 4.5 hybrid; otherwise
            // fall back to the historical budget-tokens shape.
            reasoning_codec: match target.reasoning_codec {
                ReasoningCodec::AnthropicAdaptive | ReasoningCodec::AnthropicThinkingWithEffort => {
                    target.reasoning_codec
                }
                _ => ReasoningCodec::AnthropicThinking,
            },
            require_api_key,
            supports_prompt_cache,
            supports_vision,
            prompt_cache_policy: target.prompt_cache_policy,
            thinking_by_call_id: Mutex::new(HashMap::new()),
            last_request_diagnostics: Mutex::new(None),
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
    /// inside a code block, so it is excluded. Every other Anthropic-compatible
    /// endpoint (MiniMax, local models, …) instead emits reasoning inline, so it
    /// would otherwise leak into the visible answer.
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

    fn request_body_with_reasoning(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        reasoning: ReasoningSelection,
    ) -> Result<Value> {
        // Hold the thinking map only for the synchronous rebuild; never across
        // an await.
        let thinking = self.thinking_by_call_id.lock().expect("thinking map lock");
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
        let wire_messages =
            transform::messages_for_project_state_layout(messages, project_state_layout);
        // Vision safety net: mirror of the OpenAI-chat strip — a text-only
        // Anthropic-compatible endpoint rejects image blocks with a 400, and
        // images already in history would otherwise wedge every later turn.
        let wire_messages = if self.supports_vision {
            wire_messages
        } else {
            transform::strip_image_parts_for_wire(wire_messages.as_ref())
        };
        let (system, mut anthropic_messages) =
            transform_messages_with_thinking(wire_messages.as_ref(), Some(&thinking))?;
        drop(thinking);
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
        let (thinking, max_tokens) = self.thinking_payload_and_max_tokens_for(reasoning);
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
        if matches!(
            self.reasoning_codec,
            ReasoningCodec::AnthropicAdaptive | ReasoningCodec::AnthropicThinkingWithEffort
        ) && let Some(effort) = anthropic_adaptive_effort(reasoning)
        {
            body["output_config"] = json!({"effort": effort});
        }
        Ok(body)
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

    fn take_last_request_diagnostics(&self) -> Option<ProviderRequestDiagnostics> {
        ProviderRequestDiagnostics::take(&self.last_request_diagnostics)
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
        let serialized_body = crate::provider::serialize_request_body(
            "Anthropic",
            || self.ensure_authorized(),
            || {
                self.request_body_with_reasoning(
                    messages,
                    tools,
                    options.reasoning.unwrap_or(self.reasoning),
                )
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

        parse_anthropic_stream(
            response,
            cancellation_token,
            sink,
            self.splits_inline_think_tags(),
            Some(&self.thinking_by_call_id),
        )
        .await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

pub(crate) fn transform_messages(
    messages: &[ChatCompletionRequestMessage],
) -> Result<(String, Vec<Value>)> {
    transform_messages_with_thinking(messages, None)
}

/// Like [`transform_messages`], additionally replaying each assistant turn's
/// captured native thinking blocks (keyed by the turn's first tool-call id)
/// ahead of its rebuilt content — the Anthropic protocol expects thinking to be
/// passed back through tool loops.
pub(crate) fn transform_messages_with_thinking(
    messages: &[ChatCompletionRequestMessage],
    thinking_by_call_id: Option<&HashMap<String, Vec<Value>>>,
) -> Result<(String, Vec<Value>)> {
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
                let mut blocks: Vec<Value> = Vec::new();
                let tool_calls = value.get("tool_calls").and_then(Value::as_array);
                // Thinking must precede the turn's other blocks in the replayed
                // assistant content.
                if let Some(items) =
                    transform::replay_items_for(thinking_by_call_id, tool_calls.map(Vec::as_slice))
                {
                    blocks.extend(items.iter().cloned());
                }
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

#[derive(Default)]
struct BlockState {
    tool_id: Option<String>,
    tool_name: Option<String>,
    tool_input: String,
    emitted: bool,
    /// Set when this content block is a native `thinking` block; its text and
    /// signature accumulate below so the turn's thinking can be replayed on the
    /// next request (the Anthropic tool-loop contract).
    is_thinking: bool,
    thinking: String,
    thinking_signature: String,
}

async fn parse_anthropic_stream(
    response: reqwest::Response,
    cancellation_token: CancellationToken,
    sink: SharedSink,
    split_think_tags: bool,
    thinking_store: Option<&Mutex<HashMap<String, Vec<Value>>>>,
) -> crate::provider::ProviderResult<StreamedResponse> {
    let mut content = String::new();
    let mut blocks: HashMap<usize, BlockState> = HashMap::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut usage = usage::AnthropicUsageAccumulator::default();
    let mut splitter = split_think_tags.then(ThinkTagSplitter::new);
    let mut finish_reason: Option<crate::provider::FinishReason> = None;
    let mut terminal_seen = false;
    let mut reasoning_chars = 0usize;

    let interrupted = sse::drive_sse(response, cancellation_token, |event, data| {
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
    reasoning_chars = reasoning_chars.saturating_add(streaming::finish_text_stream(
        splitter.as_mut(),
        &mut content,
        &sink,
    ));

    if !interrupted && !terminal_seen {
        return Err(crate::provider::ProviderFailure::transport(
            "Anthropic stream ended before message_stop or a terminal stop_reason",
        ));
    }

    // Capture the turn's native thinking blocks for replay on the next request
    // (see `streaming::stash_reasoning_for_replay` for the keying contract).
    if let Some(store) = thinking_store {
        let mut ordered: Vec<(usize, &BlockState)> = blocks
            .iter()
            .filter(|(_, state)| state.is_thinking && !state.thinking.is_empty())
            .map(|(index, state)| (*index, state))
            .collect();
        ordered.sort_by_key(|(index, _)| *index);
        let items: Vec<Value> = ordered
            .into_iter()
            .map(|(_, state)| {
                let mut block = json!({"type": "thinking", "thinking": state.thinking});
                if !state.thinking_signature.is_empty() {
                    block["signature"] = json!(state.thinking_signature);
                }
                block
            })
            .collect();
        streaming::stash_reasoning_for_replay(
            store,
            &tool_calls,
            (!items.is_empty()).then_some(items),
        )?;
    }

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

    Ok(StreamedResponse {
        content,
        tool_calls,
        terminal,
        usage: usage.into_usage(),
        reasoning_chars,
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_anthropic_event(
    event: &str,
    data: &str,
    blocks: &mut HashMap<usize, BlockState>,
    content: &mut String,
    tool_calls: &mut Vec<ToolCall>,
    usage: &mut usage::AnthropicUsageAccumulator,
    finish_reason: &mut Option<crate::provider::FinishReason>,
    terminal_seen: &mut bool,
    reasoning_chars: &mut usize,
    sink: &SharedSink,
    splitter: Option<&mut ThinkTagSplitter>,
) -> crate::provider::ProviderResult<()> {
    let Some(payload) = sse::parse_frame(data)? else {
        return Ok(());
    };

    match event {
        "content_block_start" => {
            let index = payload.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let block = payload.get("content_block").unwrap_or(&Value::Null);
            let entry = blocks.entry(index).or_default();
            if block.get("type").and_then(Value::as_str) == Some("thinking") {
                entry.is_thinking = true;
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    entry.thinking.push_str(text);
                }
            }
            if let (Some(id), Some(name)) = (
                block.get("id").and_then(Value::as_str),
                block.get("name").and_then(Value::as_str),
            ) {
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
                        if let Some(splitter) = splitter {
                            // MiniMax and friends inline reasoning as
                            // `<think>…</think>` in text; route it to the
                            // reasoning channel and keep `content` (the re-sent
                            // history) free of the tags.
                            let split = splitter.push(text);
                            if !split.visible.is_empty() {
                                content.push_str(&split.visible);
                                sink.assistant_delta(&split.visible);
                            }
                            if !split.reasoning.is_empty() {
                                *reasoning_chars =
                                    reasoning_chars.saturating_add(split.reasoning.chars().count());
                                sink.reasoning_delta(&split.reasoning);
                            }
                        } else {
                            content.push_str(text);
                            if !text.is_empty() {
                                sink.assistant_delta(text);
                            }
                        }
                    }
                }
                "thinking_delta" => {
                    if let Some(text) = delta.get("thinking").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        *reasoning_chars = reasoning_chars.saturating_add(text.chars().count());
                        sink.reasoning_delta(text);
                        let entry = blocks.entry(index).or_default();
                        entry.is_thinking = true;
                        entry.thinking.push_str(text);
                    }
                }
                "signature_delta" => {
                    if let Some(signature) = delta.get("signature").and_then(Value::as_str)
                        && let Some(entry) = blocks.get_mut(&index)
                    {
                        entry.thinking_signature.push_str(signature);
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
                usage.apply_event_usage(u);
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
        named_user_message, sample_tool, system_message, tool_call_message, tool_result_message,
        user_message,
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
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" you go\"}}",
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

        assert_eq!(response.content, "Here you go");
        assert_eq!(response.reasoning_chars, "weighing options".chars().count());
        assert_eq!(recording.assistant_deltas().concat(), "Here you go");
        assert_eq!(recording.reasoning_deltas().concat(), "weighing options");
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
        assert_eq!(response.tool_calls[0].id, "toolu_01");
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

        let mut session = make_session();
        session.base_url = server.uri();
        let provider = anthropic_provider(&session);
        let response = provider
            .chat_stream(
                &[user_message("read foo")],
                &[],
                CancellationToken::new(),
                sink(),
            )
            .await
            .unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "toolu_1");

        // The next request rebuilds history with the same tool-call id; the
        // captured thinking must lead the assistant content.
        let follow_up = vec![
            user_message("read foo"),
            tool_call_message("toolu_1", "read", "{\"path\":\"foo\"}"),
            tool_result_message("toolu_1", "file contents"),
        ];
        let preview = provider.preview_request(&follow_up, &[]).unwrap();
        let assistant = &preview.body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        let first_block = &assistant["content"][0];
        assert_eq!(first_block["type"], "thinking", "{assistant}");
        assert_eq!(first_block["thinking"], "planning the read");
        assert_eq!(first_block["signature"], "sig-abc");
        assert_eq!(assistant["content"][1]["type"], "tool_use");
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
