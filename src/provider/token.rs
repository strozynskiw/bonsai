use std::sync::OnceLock;

use anyhow::{Context, Result};
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use serde::de;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokenizers::Tokenizer;

use crate::model_catalog::ResolvedModel;
use crate::provider::anthropic;
use crate::provider::{InputCacheUsage, ProviderMetadata, TokenUsage};
use crate::session::ProviderSession;

/// Prompt-token counter implementation selected by provider/model metadata.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenCounterKind {
    Tiktoken,
    Qwen3,
    AnthropicCountTokens,
    ZaiTokenizer,
    #[default]
    Heuristic,
}

impl TokenCounterKind {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::Tiktoken => "tiktoken",
            Self::Qwen3 => "qwen3",
            Self::AnthropicCountTokens => "anthropic-count-tokens",
            Self::ZaiTokenizer => "zai-tokenizer",
            Self::Heuristic => "heuristic",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Self {
        match value {
            "tiktoken" => Self::Tiktoken,
            "qwen3" | "qwen-3" => Self::Qwen3,
            "anthropic-count-tokens" | "anthropic count_tokens" => Self::AnthropicCountTokens,
            "zai-tokenizer" | "zai tokenizer" => Self::ZaiTokenizer,
            _ => Self::Heuristic,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Tiktoken => "tiktoken",
            Self::Qwen3 => "qwen3",
            Self::AnthropicCountTokens => "anthropic count_tokens",
            Self::ZaiTokenizer => "Z.AI tokenizer",
            Self::Heuristic => "heuristic",
        }
    }
}

/// How much trust the agent should place in a preflight prompt estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EstimateConfidence {
    High,
    #[default]
    Low,
}

impl EstimateConfidence {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Self {
        match value {
            "high" | "high confidence" => Self::High,
            _ => Self::Low,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::High => "high confidence",
            Self::Low => "low confidence",
        }
    }
}

/// Prompt-token estimate used for context pressure, compaction, and preflight
/// checks. Provider-reported usage remains the billing source of truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptEstimate {
    pub input_tokens: usize,
    pub source: TokenCounterKind,
    pub confidence: EstimateConfidence,
    pub tool_schema_tokens: usize,
}

impl PromptEstimate {
    pub fn heuristic(
        input_tokens: usize,
        tool_schema_tokens: usize,
        source: TokenCounterKind,
    ) -> Self {
        Self {
            input_tokens,
            source,
            confidence: EstimateConfidence::Low,
            tool_schema_tokens,
        }
    }
}

/// Static model pricing in micro-USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_micros_per_million: u64,
    pub output_micros_per_million: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_micros_per_million: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_micros_per_million: Option<u64>,
}

/// One context-length pricing tier for a model.
///
/// The tier applies to the entire request when the provider-reported prompt
/// token count is at least `minimum_input_tokens`. Catalog sources must encode
/// the provider's exact boundary: use `200_000` for "at least 200K" and
/// `200_001` for "above 200K".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelPricingTier {
    pub minimum_input_tokens: u32,
    pub pricing: ModelPricing,
}

const MINUTES_PER_DAY: i64 = 24 * 60;
const MILLIS_PER_MINUTE: i64 = 60_000;

/// One recurring, half-open UTC window during which peak rates apply.
///
/// Catalog values use `HH:MM` strings. A window whose start is later than its
/// end wraps across midnight; equal endpoints are rejected as ambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UtcPricingWindow {
    start_minute: u16,
    end_minute: u16,
}

#[derive(Debug, Deserialize)]
struct RawUtcPricingWindow {
    start: Box<str>,
    end: Box<str>,
}

impl<'de> Deserialize<'de> for UtcPricingWindow {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawUtcPricingWindow::deserialize(deserializer)?;
        let start_minute = parse_utc_time(&raw.start).map_err(de::Error::custom)?;
        let end_minute = parse_utc_time(&raw.end).map_err(de::Error::custom)?;
        if start_minute == end_minute {
            return Err(de::Error::custom(
                "UTC pricing window start and end must differ",
            ));
        }
        Ok(Self {
            start_minute,
            end_minute,
        })
    }
}

impl UtcPricingWindow {
    pub(crate) const fn start_minute(self) -> u16 {
        self.start_minute
    }

    pub(crate) const fn end_minute(self) -> u16 {
        self.end_minute
    }

    fn contains_timestamp_ms(self, timestamp_ms: i64) -> bool {
        let minute = timestamp_ms
            .div_euclid(MILLIS_PER_MINUTE)
            .rem_euclid(MINUTES_PER_DAY) as u16;
        if self.start_minute < self.end_minute {
            (self.start_minute..self.end_minute).contains(&minute)
        } else {
            minute >= self.start_minute || minute < self.end_minute
        }
    }
}

fn parse_utc_time(value: &str) -> std::result::Result<u16, String> {
    let Some((hour, minute)) = value.split_once(':') else {
        return Err(format!("UTC time `{value}` must use HH:MM"));
    };
    if hour.len() != 2 || minute.len() != 2 {
        return Err(format!("UTC time `{value}` must use HH:MM"));
    }
    let hour = hour
        .parse::<u16>()
        .map_err(|_| format!("UTC time `{value}` has an invalid hour"))?;
    let minute = minute
        .parse::<u16>()
        .map_err(|_| format!("UTC time `{value}` has an invalid minute"))?;
    if hour > 23 || minute > 59 {
        return Err(format!("UTC time `{value}` is outside 00:00-23:59"));
    }
    Ok(hour * 60 + minute)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TokenRateSchedule {
    base: ModelPricing,
    tiers: Box<[ModelPricingTier]>,
}

impl TokenRateSchedule {
    fn new(base: ModelPricing, mut tiers: Vec<ModelPricingTier>) -> Self {
        tiers.retain(|tier| tier.minimum_input_tokens > 0);
        tiers.sort_by_key(|tier| tier.minimum_input_tokens);
        tiers.dedup_by_key(|tier| tier.minimum_input_tokens);
        Self {
            base,
            tiers: tiers.into_boxed_slice(),
        }
    }

    fn pricing_for_prompt_tokens(&self, prompt_tokens: u32) -> ModelPricing {
        self.tiers
            .iter()
            .rev()
            .find(|tier| prompt_tokens >= tier.minimum_input_tokens)
            .map(|tier| tier.pricing)
            .unwrap_or(self.base)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PeakRateSchedule {
    rates: TokenRateSchedule,
    windows: Box<[UtcPricingWindow]>,
}

/// A model's standard token rates plus prompt-length and UTC peak overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPricingSchedule {
    standard: TokenRateSchedule,
    peak: Option<PeakRateSchedule>,
}

impl ModelPricingSchedule {
    /// Builds a schedule from base rates and ordered input-size tiers.
    pub fn new(base: ModelPricing, tiers: Vec<ModelPricingTier>) -> Self {
        Self {
            standard: TokenRateSchedule::new(base, tiers),
            peak: None,
        }
    }

    /// Adds recurring UTC windows with their own base and input-size rates.
    pub fn with_peak_pricing(
        mut self,
        base: ModelPricing,
        tiers: Vec<ModelPricingTier>,
        mut windows: Vec<UtcPricingWindow>,
    ) -> Self {
        windows.sort_by_key(|window| (window.start_minute, window.end_minute));
        windows.dedup();
        self.peak = (!windows.is_empty()).then(|| PeakRateSchedule {
            rates: TokenRateSchedule::new(base, tiers),
            windows: windows.into_boxed_slice(),
        });
        self
    }

    /// Builds a schedule with one rate at every prompt length.
    pub fn flat(pricing: ModelPricing) -> Self {
        Self::new(pricing, Vec::new())
    }

    /// Returns the rates used at and below the first tier threshold.
    pub const fn base(&self) -> ModelPricing {
        self.standard.base
    }

    /// Selects standard rates from the provider-reported prompt-token count.
    pub fn pricing_for_prompt_tokens(&self, prompt_tokens: u32) -> ModelPricing {
        self.standard.pricing_for_prompt_tokens(prompt_tokens)
    }

    /// Selects the display rate appropriate for a configured context window.
    pub fn pricing_for_context_window(&self, context_window: Option<u32>) -> ModelPricing {
        context_window
            .map(|tokens| self.pricing_for_prompt_tokens(tokens))
            .unwrap_or(self.base())
    }

    /// Selects prompt-length and peak rates at an epoch-millisecond timestamp.
    pub fn pricing_for_usage_at(&self, usage: TokenUsage, timestamp_ms: i64) -> ModelPricing {
        self.peak
            .as_ref()
            .filter(|peak| {
                peak.windows
                    .iter()
                    .any(|window| window.contains_timestamp_ms(timestamp_ms))
            })
            .map(|peak| peak.rates.pricing_for_prompt_tokens(usage.prompt_tokens))
            .unwrap_or_else(|| self.pricing_for_prompt_tokens(usage.prompt_tokens))
    }
}

impl ModelPricing {
    pub const fn new(input_micros_per_million: u64, output_micros_per_million: u64) -> Self {
        Self {
            input_micros_per_million,
            output_micros_per_million,
            cache_read_micros_per_million: None,
            cache_write_micros_per_million: None,
        }
    }

    pub const fn with_cache_rates(
        mut self,
        cache_read_micros_per_million: Option<u64>,
        cache_write_micros_per_million: Option<u64>,
    ) -> Self {
        self.cache_read_micros_per_million = cache_read_micros_per_million;
        self.cache_write_micros_per_million = cache_write_micros_per_million;
        self
    }

    pub fn cost_micros_for_usage(self, usage: TokenUsage) -> u64 {
        self.cost_micros_for_parts(
            u64::from(usage.prompt_tokens),
            u64::from(usage.completion_tokens),
            usage.input_cache,
        )
    }

    /// What this turn would have cost with no prompt cache — every input token
    /// billed at the full input rate. Subtracting the actual cache-aware cost
    /// yields the prompt-cache savings. `prompt_tokens` is already
    /// cache-inclusive, so passing `None` prices the whole prompt at full rate.
    pub fn no_cache_cost_micros_for_usage(self, usage: TokenUsage) -> u64 {
        self.cost_micros_for_parts(
            u64::from(usage.prompt_tokens),
            u64::from(usage.completion_tokens),
            None,
        )
    }

    fn cost_micros_for_parts(
        self,
        prompt_tokens: u64,
        completion_tokens: u64,
        input_cache: Option<InputCacheUsage>,
    ) -> u64 {
        let (input_tokens, cache_read_tokens, cache_write_tokens) = match input_cache {
            Some(input_cache) if input_cache.total_input_tokens > prompt_tokens => (
                prompt_tokens,
                input_cache.read_tokens,
                input_cache.creation_tokens,
            ),
            Some(input_cache) => (
                prompt_tokens
                    .saturating_sub(input_cache.read_tokens)
                    .saturating_sub(input_cache.creation_tokens),
                input_cache.read_tokens,
                input_cache.creation_tokens,
            ),
            None => (prompt_tokens, 0, 0),
        };
        let cache_read_rate = self
            .cache_read_micros_per_million
            .unwrap_or(self.input_micros_per_million);
        let cache_write_rate = self
            .cache_write_micros_per_million
            .unwrap_or(self.input_micros_per_million);
        let micros_times_million = input_tokens
            .saturating_mul(self.input_micros_per_million)
            .saturating_add(completion_tokens.saturating_mul(self.output_micros_per_million))
            .saturating_add(cache_read_tokens.saturating_mul(cache_read_rate))
            .saturating_add(cache_write_tokens.saturating_mul(cache_write_rate));
        micros_times_million.saturating_add(999_999) / 1_000_000
    }

    #[cfg(test)]
    pub fn cost_micros(self, prompt_tokens: u32, completion_tokens: u32) -> u64 {
        self.cost_micros_for_parts(u64::from(prompt_tokens), u64::from(completion_tokens), None)
    }
}

#[derive(Debug, Clone)]
pub struct PromptEstimator {
    model: String,
    kind: TokenCounterKind,
    pricing: Option<ModelPricingSchedule>,
    anthropic: Option<AnthropicCountTokensClient>,
    zai: Option<ZaiTokenizerClient>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PromptEstimatorCacheKey {
    model: String,
    kind: TokenCounterKind,
}

impl PromptEstimator {
    pub fn from_metadata(metadata: &ProviderMetadata, session: &ProviderSession) -> Self {
        let model = session.model.clone();
        let kind = metadata
            .token_counter
            .unwrap_or(TokenCounterKind::Heuristic);
        let anthropic = matches!(kind, TokenCounterKind::AnthropicCountTokens)
            .then(|| AnthropicCountTokensClient::new(session, metadata));
        let zai = matches!(kind, TokenCounterKind::ZaiTokenizer)
            .then(|| ZaiTokenizerClient::new(session, metadata, &model));
        Self {
            model,
            kind,
            pricing: metadata.pricing.map(ModelPricingSchedule::flat),
            anthropic,
            zai,
        }
    }

    pub(crate) fn from_resolved_model(
        metadata: &ProviderMetadata,
        session: &ProviderSession,
        resolved: &ResolvedModel,
    ) -> Self {
        let kind = resolved
            .token_counter
            .unwrap_or(TokenCounterKind::Heuristic);
        let anthropic = matches!(kind, TokenCounterKind::AnthropicCountTokens)
            .then(|| AnthropicCountTokensClient::new_for_resolved(session, resolved, metadata));
        let zai = matches!(kind, TokenCounterKind::ZaiTokenizer)
            .then(|| ZaiTokenizerClient::new(session, metadata, resolved.remote_model_id.as_ref()));
        Self {
            model: resolved.remote_model_id.to_string(),
            kind,
            pricing: resolved.pricing_schedule.clone(),
            anthropic,
            zai,
        }
    }

    pub fn heuristic() -> Self {
        Self {
            model: String::new(),
            kind: TokenCounterKind::Heuristic,
            pricing: None,
            anthropic: None,
            zai: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests(
        model: impl Into<String>,
        kind: TokenCounterKind,
        pricing: Option<ModelPricing>,
    ) -> Self {
        Self {
            model: model.into(),
            kind,
            pricing: pricing.map(ModelPricingSchedule::flat),
            anthropic: None,
            zai: None,
        }
    }

    pub fn pricing(&self) -> Option<&ModelPricingSchedule> {
        self.pricing.as_ref()
    }

    pub(crate) fn cache_key(&self) -> PromptEstimatorCacheKey {
        PromptEstimatorCacheKey {
            model: self.model.clone(),
            kind: self.kind,
        }
    }

    pub(crate) fn estimate_tool_schema_tokens_from_bytes(&self, schema: &[u8]) -> usize {
        if schema.is_empty() {
            return 0;
        }
        std::str::from_utf8(schema)
            .ok()
            .and_then(|text| count_text_with_local_tokenizer(self.kind, &self.model, text).ok())
            .unwrap_or_else(|| heuristic_text_tokens(&String::from_utf8_lossy(schema)))
    }

    pub(crate) fn estimate_tool_schema_tokens_for_report_from_bytes(&self, schema: &[u8]) -> usize {
        if schema.is_empty() {
            0
        } else {
            std::str::from_utf8(schema)
                .ok()
                .and_then(|text| count_text_with_local_tokenizer(self.kind, &self.model, text).ok())
                .unwrap_or_else(|| heuristic_text_tokens(&String::from_utf8_lossy(schema)))
        }
    }

    #[cfg(test)]
    pub async fn estimate_prompt(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
    ) -> PromptEstimate {
        let tool_schema_tokens = estimate_tool_schema_tokens(tools);
        self.estimate_prompt_with_tool_schema_tokens(messages, tools, tool_schema_tokens)
            .await
    }

    pub(crate) async fn estimate_prompt_with_tool_schema_tokens(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        tool_schema_tokens: usize,
    ) -> PromptEstimate {
        match self.kind {
            TokenCounterKind::Tiktoken | TokenCounterKind::Qwen3 => {
                let kind = self.kind;
                estimate_with_local_tokenizer(
                    kind,
                    &self.model,
                    messages,
                    tools,
                    tool_schema_tokens,
                )
                .unwrap_or_else(|err| {
                    tracing::debug!(
                        model = %self.model,
                        counter = kind.label(),
                        error = %format!("{err:#}"),
                        "local tokenizer estimate failed; falling back to heuristic token count"
                    );
                    heuristic_estimate_with_tool_schema_tokens(
                        messages,
                        tool_schema_tokens,
                        TokenCounterKind::Heuristic,
                    )
                })
            }
            TokenCounterKind::AnthropicCountTokens => {
                if let Some(client) = &self.anthropic {
                    match client.count_tokens(messages, tools).await {
                        Ok(tokens) => PromptEstimate {
                            input_tokens: tokens,
                            source: TokenCounterKind::AnthropicCountTokens,
                            confidence: EstimateConfidence::High,
                            tool_schema_tokens,
                        },
                        Err(err) => {
                            tracing::debug!(
                                error = %format!("{err:#}"),
                                "Anthropic count_tokens failed; falling back to heuristic token count"
                            );
                            heuristic_estimate_with_tool_schema_tokens(
                                messages,
                                tool_schema_tokens,
                                TokenCounterKind::Heuristic,
                            )
                        }
                    }
                } else {
                    heuristic_estimate_with_tool_schema_tokens(
                        messages,
                        tool_schema_tokens,
                        TokenCounterKind::Heuristic,
                    )
                }
            }
            TokenCounterKind::ZaiTokenizer => {
                if let Some(client) = &self.zai {
                    match client.count_tokens(messages, tools).await {
                        Ok(tokens) => PromptEstimate {
                            input_tokens: tokens,
                            source: TokenCounterKind::ZaiTokenizer,
                            confidence: EstimateConfidence::High,
                            tool_schema_tokens,
                        },
                        Err(err) => {
                            tracing::debug!(
                                error = %format!("{err:#}"),
                                "Z.AI tokenizer failed; falling back to heuristic token count"
                            );
                            heuristic_estimate_with_tool_schema_tokens(
                                messages,
                                tool_schema_tokens,
                                TokenCounterKind::Heuristic,
                            )
                        }
                    }
                } else {
                    heuristic_estimate_with_tool_schema_tokens(
                        messages,
                        tool_schema_tokens,
                        TokenCounterKind::Heuristic,
                    )
                }
            }
            TokenCounterKind::Heuristic => heuristic_estimate_with_tool_schema_tokens(
                messages,
                tool_schema_tokens,
                TokenCounterKind::Heuristic,
            ),
        }
    }

    pub(crate) fn estimate_messages_for_report_with_tool_schema_tokens(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tool_schema_tokens: usize,
    ) -> (Vec<usize>, PromptEstimate) {
        let (message_tokens, source, confidence) =
            estimate_messages_for_report(&self.model, self.kind, messages);
        let total = message_tokens
            .iter()
            .copied()
            .sum::<usize>()
            .saturating_add(tool_schema_tokens);
        (
            message_tokens,
            PromptEstimate {
                input_tokens: total,
                source,
                confidence,
                tool_schema_tokens,
            },
        )
    }

    pub fn estimate_text_for_report(&self, text: &str) -> usize {
        estimate_text_for_report(&self.model, self.kind, text)
    }
}

impl Default for PromptEstimator {
    fn default() -> Self {
        Self::heuristic()
    }
}

#[derive(Debug, Clone)]
struct ZaiTokenizerClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

impl ZaiTokenizerClient {
    fn new(session: &ProviderSession, metadata: &ProviderMetadata, model: &str) -> Self {
        let base_url = if session.base_url.trim().is_empty() {
            metadata.default_base_url.as_ref()
        } else {
            session.base_url.trim()
        };
        Self {
            http: crate::provider::http_client(),
            endpoint: format!("{}/tokenizer", base_url.trim_end_matches('/')),
            api_key: session.api_key.clone(),
            model: model.to_string(),
        }
    }

    async fn count_tokens(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
    ) -> Result<usize> {
        let mut body = json!({
            "model": self.model,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        let response = self
            .http
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call Z.AI tokenizer")?;
        if !response.status().is_success() {
            anyhow::bail!("Z.AI tokenizer returned HTTP {}", response.status());
        }
        let payload: Value = response
            .json()
            .await
            .context("Failed to parse Z.AI tokenizer response")?;
        payload
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64)
            .and_then(|tokens| usize::try_from(tokens).ok())
            .context("Z.AI tokenizer response missing usage.prompt_tokens")
    }
}

#[derive(Debug, Clone)]
struct AnthropicCountTokensClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
}

impl AnthropicCountTokensClient {
    fn new(session: &ProviderSession, metadata: &ProviderMetadata) -> Self {
        Self::build(
            session,
            metadata,
            &metadata.endpoint_path,
            session.model.clone(),
        )
    }

    fn new_for_resolved(
        session: &ProviderSession,
        resolved: &ResolvedModel,
        metadata: &ProviderMetadata,
    ) -> Self {
        let endpoint_path = resolved
            .endpoint_path
            .as_deref()
            .unwrap_or(&metadata.endpoint_path);
        Self::build(
            session,
            metadata,
            endpoint_path,
            resolved.remote_model_id.to_string(),
        )
    }

    /// Shared construction: resolve the base URL (session override, else the
    /// provider default) and assemble the `…/count_tokens` endpoint. `new` and
    /// `new_for_resolved` differ only in the endpoint path and model.
    fn build(
        session: &ProviderSession,
        metadata: &ProviderMetadata,
        endpoint_path: &str,
        model: String,
    ) -> Self {
        let base_url = if session.base_url.trim().is_empty() {
            metadata.default_base_url.to_string()
        } else {
            session.base_url.trim().trim_end_matches('/').to_string()
        };
        let endpoint = format!(
            "{}/{}/count_tokens",
            base_url.trim_end_matches('/'),
            endpoint_path.trim_start_matches('/').trim_end_matches('/')
        );
        Self {
            http: crate::provider::http_client(),
            endpoint,
            api_key: session.api_key.clone(),
            model,
        }
    }

    async fn count_tokens(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
    ) -> Result<usize> {
        let (system, anthropic_messages) = anthropic::transform_messages(messages)?;
        let body = json!({
            "model": self.model,
            "system": system,
            "messages": anthropic_messages,
            "tools": anthropic::transform_tools(tools)?,
        });
        let response = self
            .http
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", anthropic::ANTHROPIC_API_VERSION)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to call Anthropic count_tokens")?;

        if !response.status().is_success() {
            anyhow::bail!("count_tokens returned HTTP {}", response.status());
        }

        let payload: Value = response
            .json()
            .await
            .context("Failed to parse count_tokens response")?;
        payload
            .get("input_tokens")
            .and_then(Value::as_u64)
            .map(|tokens| tokens as usize)
            .context("count_tokens response missing input_tokens")
    }
}

fn estimate_with_local_tokenizer(
    kind: TokenCounterKind,
    model: &str,
    messages: &[ChatCompletionRequestMessage],
    tools: &[ChatCompletionTool],
    tool_schema_tokens: usize,
) -> Result<PromptEstimate> {
    let text = serialized_prompt(messages, tools)?;
    let input_tokens = count_text_with_local_tokenizer(kind, model, &text)?;
    Ok(PromptEstimate {
        input_tokens,
        source: kind,
        confidence: local_tokenizer_confidence(kind, model),
        tool_schema_tokens,
    })
}

#[cfg(test)]
fn estimate_with_tiktoken(
    model: &str,
    messages: &[ChatCompletionRequestMessage],
    tools: &[ChatCompletionTool],
    tool_schema_tokens: usize,
) -> Result<PromptEstimate> {
    estimate_with_local_tokenizer(
        TokenCounterKind::Tiktoken,
        model,
        messages,
        tools,
        tool_schema_tokens,
    )
}

/// Text used to heuristically size a message: the serialized JSON (which mirrors
/// the wire payload the model actually sees), falling back to the `Debug`
/// rendering only when serialization fails.
fn heuristic_message_text(message: &ChatCompletionRequestMessage) -> String {
    serde_json::to_string(&message_value_for_estimation(message))
        .unwrap_or_else(|_| format!("{message:?}"))
}

/// Serialize a message to JSON for token estimation, replacing embedded image
/// data URIs with a tiny placeholder. A base64-encoded image can be megabytes;
/// counting its characters as tokens would massively overcount the prompt and
/// falsely trigger compaction. Vision models bill images at a small fixed tile
/// cost, so a placeholder is far closer to reality than the raw base64 length.
fn message_value_for_estimation(message: &ChatCompletionRequestMessage) -> serde_json::Value {
    let mut value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    redact_image_data_uris(&mut value);
    value
}

fn redact_image_data_uris(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(url)) = map.get_mut("url")
                && url.starts_with("data:")
            {
                *url = "[image]".to_string();
            }
            for nested in map.values_mut() {
                redact_image_data_uris(nested);
            }
        }
        serde_json::Value::Array(items) => {
            for nested in items {
                redact_image_data_uris(nested);
            }
        }
        _ => {}
    }
}

fn estimate_messages_for_report(
    model: &str,
    kind: TokenCounterKind,
    messages: &[ChatCompletionRequestMessage],
) -> (Vec<usize>, TokenCounterKind, EstimateConfidence) {
    match local_tokenizer_for_counter(kind, model) {
        Ok(Some(tokenizer)) => {
            let message_tokens = messages
                .iter()
                .map(|message| count_message_with_tokenizer(tokenizer, message))
                .collect::<Result<Vec<_>>>();
            match message_tokens {
                Ok(tokens) => (tokens, kind, local_tokenizer_confidence(kind, model)),
                Err(err) => {
                    tracing::debug!(
                        counter = kind.label(),
                        error = %format!("{err:#}"),
                        "local tokenizer report estimate failed; falling back to heuristic token count"
                    );
                    (
                        heuristic_message_tokens_for_report(messages),
                        TokenCounterKind::Heuristic,
                        EstimateConfidence::Low,
                    )
                }
            }
        }
        Ok(None) => (
            heuristic_message_tokens_for_report(messages),
            if matches!(
                kind,
                TokenCounterKind::AnthropicCountTokens | TokenCounterKind::ZaiTokenizer
            ) {
                TokenCounterKind::Heuristic
            } else {
                kind
            },
            EstimateConfidence::Low,
        ),
        Err(err) => {
            tracing::debug!(
                counter = kind.label(),
                error = %format!("{err:#}"),
                "local tokenizer report estimate failed; falling back to heuristic token count"
            );
            (
                heuristic_message_tokens_for_report(messages),
                TokenCounterKind::Heuristic,
                EstimateConfidence::Low,
            )
        }
    }
}

/// Confidence in a local tokenizer for the selected model family.
///
/// Gateway catalogs often use tiktoken as a fast approximation for Claude,
/// Gemini, Grok, and other non-OpenAI families. Counting still works there,
/// but presenting the result as exact is misleading. Keep high confidence for
/// native OpenAI and Qwen 3 families and label every cross-family estimate as
/// approximate.
fn local_tokenizer_confidence(kind: TokenCounterKind, model: &str) -> EstimateConfidence {
    let model = model.to_ascii_lowercase();
    let native_family = match kind {
        TokenCounterKind::Tiktoken => {
            model.starts_with("gpt-")
                || model.starts_with("o1")
                || model.starts_with("o3")
                || model.starts_with("o4")
                || model.starts_with("text-embedding-")
        }
        TokenCounterKind::Qwen3 => model.contains("qwen3"),
        TokenCounterKind::AnthropicCountTokens
        | TokenCounterKind::ZaiTokenizer
        | TokenCounterKind::Heuristic => false,
    };
    if native_family {
        EstimateConfidence::High
    } else {
        EstimateConfidence::Low
    }
}

fn count_message_with_tokenizer(
    tokenizer: LocalTokenizerRef,
    message: &ChatCompletionRequestMessage,
) -> Result<usize> {
    serde_json::to_string(&message_value_for_estimation(message))
        .context("Failed to serialize message for token report")
        .and_then(|text| tokenizer.count_text(&text))
}

fn heuristic_message_tokens_for_report(messages: &[ChatCompletionRequestMessage]) -> Vec<usize> {
    messages
        .iter()
        .map(|message| heuristic_text_tokens(&heuristic_message_text(message)))
        .collect()
}

fn estimate_text_for_report(model: &str, kind: TokenCounterKind, text: &str) -> usize {
    count_text_with_local_tokenizer(kind, model, text)
        .unwrap_or_else(|_| heuristic_text_tokens(text))
}

#[cfg(test)]
fn estimate_tool_schema_tokens(tools: &[ChatCompletionTool]) -> usize {
    if tools.is_empty() {
        0
    } else {
        serde_json::to_string(tools)
            .map(|text| heuristic_text_tokens(&text))
            .unwrap_or(1)
    }
}

fn heuristic_estimate_with_tool_schema_tokens(
    messages: &[ChatCompletionRequestMessage],
    tool_schema_tokens: usize,
    source: TokenCounterKind,
) -> PromptEstimate {
    let message_tokens = messages
        .iter()
        .map(|message| heuristic_text_tokens(&heuristic_message_text(message)))
        .sum::<usize>();
    PromptEstimate::heuristic(
        message_tokens.saturating_add(tool_schema_tokens).max(1),
        tool_schema_tokens,
        source,
    )
}

fn serialized_prompt(
    messages: &[ChatCompletionRequestMessage],
    tools: &[ChatCompletionTool],
) -> Result<String> {
    let messages: Vec<serde_json::Value> =
        messages.iter().map(message_value_for_estimation).collect();
    serde_json::to_string(&json!({
        "messages": messages,
        "tools": tools,
    }))
    .context("Failed to serialize prompt")
}

static O200K_BPE: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
static CL100K_BPE: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
static QWEN3_TOKENIZER: OnceLock<Tokenizer> = OnceLock::new();

const QWEN3_TOKENIZER_JSON: &[u8] = include_bytes!("../../models/tokenizers/qwen3/tokenizer.json");

#[derive(Clone, Copy)]
enum LocalTokenizerRef {
    Tiktoken(&'static tiktoken_rs::CoreBPE),
    HuggingFace(&'static Tokenizer),
}

impl LocalTokenizerRef {
    fn count_text(self, text: &str) -> Result<usize> {
        match self {
            Self::Tiktoken(bpe) => Ok(bpe.encode_with_special_tokens(text).len().max(1)),
            Self::HuggingFace(tokenizer) => tokenizer
                .encode(text, false)
                .map(|encoding| encoding.len().max(1))
                .map_err(|err| anyhow::anyhow!("Failed to encode text with tokenizer: {err}")),
        }
    }
}

fn count_text_with_local_tokenizer(
    kind: TokenCounterKind,
    model: &str,
    text: &str,
) -> Result<usize> {
    let tokenizer = local_tokenizer_for_counter(kind, model)?
        .with_context(|| format!("{} has no local tokenizer", kind.label()))?;
    tokenizer.count_text(text)
}

fn local_tokenizer_for_counter(
    kind: TokenCounterKind,
    model: &str,
) -> Result<Option<LocalTokenizerRef>> {
    match kind {
        TokenCounterKind::Tiktoken => Ok(Some(LocalTokenizerRef::Tiktoken(
            tiktoken_bpe_for_model(model)?,
        ))),
        TokenCounterKind::Qwen3 => Ok(Some(LocalTokenizerRef::HuggingFace(
            cached_huggingface_tokenizer(&QWEN3_TOKENIZER, QWEN3_TOKENIZER_JSON, "qwen3")?,
        ))),
        TokenCounterKind::AnthropicCountTokens
        | TokenCounterKind::ZaiTokenizer
        | TokenCounterKind::Heuristic => Ok(None),
    }
}

fn tiktoken_bpe_for_model(model: &str) -> Result<&'static tiktoken_rs::CoreBPE> {
    let lower = model.to_ascii_lowercase();
    if lower.contains("gpt-5")
        || lower.contains("gpt-4o")
        || lower.contains("gpt-4.1")
        || lower.contains("gpt-4.5")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        cached_bpe(&O200K_BPE, tiktoken_rs::o200k_base, "o200k_base")
    } else {
        cached_bpe(&CL100K_BPE, tiktoken_rs::cl100k_base, "cl100k_base")
    }
}

fn cached_bpe(
    cache: &'static OnceLock<tiktoken_rs::CoreBPE>,
    load: fn() -> Result<tiktoken_rs::CoreBPE, anyhow::Error>,
    label: &str,
) -> Result<&'static tiktoken_rs::CoreBPE> {
    if cache.get().is_none() {
        let bpe = load().with_context(|| format!("Failed to load {label} tokenizer"))?;
        let _ = cache.set(bpe);
    }
    cache
        .get()
        .with_context(|| format!("Failed to cache {label} tokenizer"))
}

fn cached_huggingface_tokenizer(
    cache: &'static OnceLock<Tokenizer>,
    bytes: &'static [u8],
    label: &str,
) -> Result<&'static Tokenizer> {
    if cache.get().is_none() {
        let tokenizer = Tokenizer::from_bytes(bytes)
            .map_err(|err| anyhow::anyhow!("Failed to load {label} tokenizer: {err}"))?;
        let _ = cache.set(tokenizer);
    }
    cache
        .get()
        .with_context(|| format!("Failed to cache {label} tokenizer"))
}

fn heuristic_text_tokens(text: &str) -> usize {
    text.chars().count().saturating_div(4).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::chat::{
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        ChatCompletionTool, FunctionObjectArgs,
    };
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::provider::test_utils::provider_session;

    #[test]
    fn model_pricing_computes_micro_usd_from_actual_usage() {
        let pricing = ModelPricing::new(3_000_000, 15_000_000);
        assert_eq!(pricing.cost_micros(1_000_000, 1_000_000), 18_000_000);
        assert_eq!(pricing.cost_micros(1, 0), 3);
    }

    #[test]
    fn model_pricing_uses_cache_rates_for_usage() {
        let pricing = ModelPricing::new(3_000_000, 15_000_000)
            .with_cache_rates(Some(300_000), Some(3_750_000));
        let usage = TokenUsage {
            prompt_tokens: 750_000,
            completion_tokens: 100_000,
            input_cache: Some(InputCacheUsage::new(200_000, 50_000, 1_000_000)),
        };

        assert_eq!(pricing.cost_micros_for_usage(usage), 3_997_500);
    }

    #[test]
    fn pricing_schedule_switches_at_each_inclusive_input_threshold() {
        let base = ModelPricing::new(1_000_000, 2_000_000);
        let medium = ModelPricing::new(3_000_000, 4_000_000);
        let high = ModelPricing::new(5_000_000, 6_000_000);
        let schedule = ModelPricingSchedule::new(
            base,
            vec![
                ModelPricingTier {
                    minimum_input_tokens: 400_001,
                    pricing: high,
                },
                ModelPricingTier {
                    minimum_input_tokens: 200_001,
                    pricing: medium,
                },
            ],
        );

        assert_eq!(schedule.pricing_for_prompt_tokens(200_000), base);
        assert_eq!(schedule.pricing_for_prompt_tokens(200_001), medium);
        assert_eq!(schedule.pricing_for_prompt_tokens(400_000), medium);
        assert_eq!(schedule.pricing_for_prompt_tokens(400_001), high);
    }

    #[test]
    fn pricing_schedule_uses_peak_rates_inside_each_utc_window() {
        let standard = ModelPricing::new(1_000_000, 2_000_000);
        let peak = ModelPricing::new(3_000_000, 4_000_000);
        let windows = [
            json!({"start": "01:00", "end": "04:00"}),
            json!({"start": "06:00", "end": "10:00"}),
        ]
        .into_iter()
        .map(serde_json::from_value)
        .collect::<std::result::Result<Vec<UtcPricingWindow>, _>>()
        .expect("valid UTC pricing windows");
        let schedule =
            ModelPricingSchedule::flat(standard).with_peak_pricing(peak, Vec::new(), windows);
        let usage = TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 0,
            input_cache: None,
        };
        let rate_at = |hour: i64, minute: i64| {
            schedule
                .pricing_for_usage_at(usage, (hour * 60 + minute) * MILLIS_PER_MINUTE)
                .input_micros_per_million
        };

        assert_eq!(
            [
                rate_at(0, 59),
                rate_at(1, 0),
                rate_at(3, 59),
                rate_at(4, 0),
                rate_at(6, 0),
                rate_at(9, 59),
                rate_at(10, 0),
            ],
            [
                standard.input_micros_per_million,
                peak.input_micros_per_million,
                peak.input_micros_per_million,
                standard.input_micros_per_million,
                peak.input_micros_per_million,
                peak.input_micros_per_million,
                standard.input_micros_per_million,
            ]
        );
    }

    #[test]
    fn pricing_schedule_supports_peak_windows_across_midnight() {
        let standard = ModelPricing::new(1_000_000, 2_000_000);
        let peak = ModelPricing::new(3_000_000, 4_000_000);
        let window = serde_json::from_value(json!({"start": "23:30", "end": "01:15"}))
            .expect("valid wrapping UTC pricing window");
        let schedule =
            ModelPricingSchedule::flat(standard).with_peak_pricing(peak, Vec::new(), vec![window]);
        let usage = TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 0,
            input_cache: None,
        };
        let rate_at = |hour: i64, minute: i64| {
            schedule
                .pricing_for_usage_at(usage, (hour * 60 + minute) * MILLIS_PER_MINUTE)
                .input_micros_per_million
        };

        assert_eq!(
            [
                rate_at(23, 29),
                rate_at(23, 30),
                rate_at(0, 0),
                rate_at(1, 15)
            ],
            [
                standard.input_micros_per_million,
                peak.input_micros_per_million,
                peak.input_micros_per_million,
                standard.input_micros_per_million,
            ]
        );
    }

    #[test]
    fn utc_pricing_window_rejects_equal_endpoints() {
        let error =
            serde_json::from_value::<UtcPricingWindow>(json!({"start": "04:00", "end": "04:00"}))
                .expect_err("equal endpoints are ambiguous");

        assert!(error.to_string().contains("start and end must differ"));
    }

    #[test]
    fn no_cache_cost_prices_whole_prompt_at_full_rate() {
        let pricing = ModelPricing::new(3_000_000, 15_000_000)
            .with_cache_rates(Some(300_000), Some(3_750_000));
        // Read-heavy turn: most of the prompt is served from cache.
        let usage = TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            input_cache: Some(InputCacheUsage::new(900_000, 0, 1_000_000)),
        };
        // The no-cache baseline prices all 1M input tokens at the full input rate.
        assert_eq!(pricing.no_cache_cost_micros_for_usage(usage), 3_000_000);
        // The cache-aware cost is far lower, so the cache saved money.
        assert!(
            pricing.cost_micros_for_usage(usage) < pricing.no_cache_cost_micros_for_usage(usage)
        );
    }

    #[test]
    fn token_counter_kind_round_trips_provider_labels() {
        assert_eq!(TokenCounterKind::Qwen3.as_db_str(), "qwen3");
        assert_eq!(
            TokenCounterKind::from_db_str("qwen3"),
            TokenCounterKind::Qwen3
        );
        assert_eq!(
            TokenCounterKind::from_db_str("qwen-3"),
            TokenCounterKind::Qwen3
        );

        let encoded = serde_json::to_string(&TokenCounterKind::Qwen3)
            .expect("token counter kind should serialize");
        assert_eq!(encoded, "\"qwen3\"");
        let decoded: TokenCounterKind =
            serde_json::from_str("\"qwen3\"").expect("qwen3 should deserialize");
        assert_eq!(decoded, TokenCounterKind::Qwen3);

        assert_eq!(
            TokenCounterKind::from_db_str("zai-tokenizer"),
            TokenCounterKind::ZaiTokenizer
        );
        assert_eq!(
            serde_json::to_string(&TokenCounterKind::ZaiTokenizer).unwrap(),
            "\"zai-tokenizer\""
        );
    }

    #[test]
    fn tiktoken_estimate_counts_tool_schema() {
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content("hello")
                .build()
                .expect("user message should build"),
        )];
        let tool = ChatCompletionTool {
            function: FunctionObjectArgs::default()
                .name("read")
                .description("Read a file")
                .parameters(json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }))
                .build()
                .expect("function object should build"),
        };

        let without_tools = estimate_with_tiktoken("gpt-5.5", &messages, &[], 0)
            .expect("tiktoken estimate should succeed");
        let tool_schema_tokens = estimate_tool_schema_tokens(std::slice::from_ref(&tool));
        let with_tools = estimate_with_tiktoken("gpt-5.5", &messages, &[tool], tool_schema_tokens)
            .expect("tiktoken estimate should succeed");

        assert_eq!(with_tools.source, TokenCounterKind::Tiktoken);
        assert_eq!(with_tools.confidence, EstimateConfidence::High);
        assert!(with_tools.tool_schema_tokens > 0);
        assert!(with_tools.input_tokens > without_tools.input_tokens);
    }

    #[test]
    fn embedded_image_data_uri_does_not_inflate_estimate() {
        // A ~2 MB base64 image: counting its characters as tokens would read as
        // ~500k tokens and falsely trigger compaction. Redaction keeps it tiny.
        let big = format!("data:image/png;base64,{}", "A".repeat(2_000_000));
        let message: ChatCompletionRequestMessage = serde_json::from_value(json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "what is in this screenshot?"},
                {"type": "image_url", "image_url": {"url": big}},
            ],
        }))
        .expect("image message should deserialize");

        let estimate = estimate_with_tiktoken("gpt-5.5", std::slice::from_ref(&message), &[], 0)
            .expect("tiktoken estimate should succeed");
        assert!(
            estimate.input_tokens < 1_000,
            "image base64 must not inflate the estimate, got {}",
            estimate.input_tokens
        );
    }

    #[test]
    fn tiktoken_unknown_model_falls_back_to_cl100k() {
        let messages = vec![ChatCompletionRequestMessage::System(
            ChatCompletionRequestSystemMessageArgs::default()
                .content("system")
                .build()
                .expect("system message should build"),
        )];
        let estimate = estimate_with_tiktoken("custom-openai-model", &messages, &[], 0)
            .expect("unknown model should still use a BPE fallback");
        assert_eq!(estimate.source, TokenCounterKind::Tiktoken);
        assert_eq!(estimate.confidence, EstimateConfidence::Low);
        assert!(estimate.input_tokens > 0);
    }

    #[test]
    fn gateway_tiktoken_estimates_are_low_confidence_for_non_openai_models() {
        let messages = vec![ChatCompletionRequestMessage::System(
            ChatCompletionRequestSystemMessageArgs::default()
                .content("system")
                .build()
                .expect("system message should build"),
        )];
        let estimate = estimate_with_tiktoken("claude-sonnet-5", &messages, &[], 0)
            .expect("gateway approximation should still produce a count");

        assert_eq!(estimate.source, TokenCounterKind::Tiktoken);
        assert_eq!(estimate.confidence, EstimateConfidence::Low);
    }

    #[test]
    fn tiktoken_bpe_is_cached_by_model_family() {
        let first = tiktoken_bpe_for_model("gpt-5.5").expect("gpt model should load o200k");
        let second = tiktoken_bpe_for_model("gpt-4o").expect("gpt model should reuse o200k");
        let fallback =
            tiktoken_bpe_for_model("custom-openai-model").expect("custom model should load cl100k");

        assert!(std::ptr::eq(first, second));
        assert!(!std::ptr::eq(first, fallback));
    }

    #[test]
    fn qwen3_tokenizer_is_cached() {
        let first = cached_huggingface_tokenizer(&QWEN3_TOKENIZER, QWEN3_TOKENIZER_JSON, "qwen3")
            .expect("qwen3 tokenizer should load");
        let second = cached_huggingface_tokenizer(&QWEN3_TOKENIZER, QWEN3_TOKENIZER_JSON, "qwen3")
            .expect("qwen3 tokenizer should reuse cache");

        assert!(std::ptr::eq(first, second));
    }

    #[tokio::test]
    async fn qwen3_estimate_routes_content_through_local_tokenizer() {
        let estimator = PromptEstimator::for_tests("qwen3-coder", TokenCounterKind::Qwen3, None);
        let short = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content("hi")
                .build()
                .expect("user message should build"),
        )];
        let long = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content("the quick brown fox jumps over the lazy dog ".repeat(20))
                .build()
                .expect("user message should build"),
        )];

        let short_estimate = estimator
            .estimate_prompt_with_tool_schema_tokens(&short, &[], 0)
            .await;
        let long_estimate = estimator
            .estimate_prompt_with_tool_schema_tokens(&long, &[], 0)
            .await;

        // Routing: the local qwen3 tokenizer produced the count, not the
        // heuristic fallback (which would report Heuristic/Low).
        assert_eq!(long_estimate.source, TokenCounterKind::Qwen3);
        assert_eq!(long_estimate.confidence, EstimateConfidence::High);
        // The message content must actually reach the tokenizer: ~9x the text
        // has to cost substantially more tokens, even with BPE merges.
        assert!(
            long_estimate.input_tokens > short_estimate.input_tokens * 2,
            "longer prompt must count substantially more tokens: short={} long={}",
            short_estimate.input_tokens,
            long_estimate.input_tokens
        );
    }

    #[test]
    fn qwen3_counts_tool_schema_for_runtime_and_reports() {
        let estimator = PromptEstimator::for_tests("qwen3-coder", TokenCounterKind::Qwen3, None);
        let schema = br#"{"type":"object","properties":{"path":{"type":"string"}}}"#;
        let larger = br#"{"type":"object","properties":{"path":{"type":"string"},"mode":{"type":"string"},"recursive":{"type":"boolean"}}}"#;

        let runtime = estimator.estimate_tool_schema_tokens_from_bytes(schema);
        let report = estimator.estimate_tool_schema_tokens_for_report_from_bytes(schema);

        // Both public entry points must agree on the same schema bytes.
        assert_eq!(runtime, report);
        // The count must reflect the schema: non-empty costs tokens, more
        // schema text costs more tokens, empty input is free.
        assert!(runtime > 0, "non-empty schema must count as tokens");
        assert!(
            estimator.estimate_tool_schema_tokens_from_bytes(larger) > runtime,
            "a larger schema must count as more tokens"
        );
        assert_eq!(estimator.estimate_tool_schema_tokens_from_bytes(b""), 0);
        assert_eq!(
            estimator.estimate_tool_schema_tokens_for_report_from_bytes(b""),
            0
        );
    }

    #[test]
    fn qwen3_report_counts_messages_with_local_tokenizer() {
        let messages = vec![
            ChatCompletionRequestMessage::System(
                ChatCompletionRequestSystemMessageArgs::default()
                    .content("the quick brown fox jumps over the lazy dog")
                    .build()
                    .expect("system message should build"),
            ),
            ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessageArgs::default()
                    .content("hi")
                    .build()
                    .expect("user message should build"),
            ),
        ];
        let estimator = PromptEstimator::for_tests("qwen3-coder", TokenCounterKind::Qwen3, None);

        let (message_tokens, estimate) =
            estimator.estimate_messages_for_report_with_tool_schema_tokens(&messages, 7);

        // The report routes through the local tokenizer, not the heuristic.
        assert_eq!(estimate.source, TokenCounterKind::Qwen3);
        assert_eq!(estimate.confidence, EstimateConfidence::High);
        // Per-message counts are real counts: positive, and the much longer
        // system message costs more tokens than the two-char user message.
        assert_eq!(message_tokens.len(), 2);
        assert!(
            message_tokens.iter().all(|count| *count > 0),
            "every message must count as at least one token: {message_tokens:?}"
        );
        assert!(
            message_tokens[0] > message_tokens[1],
            "longer message must count more tokens: {message_tokens:?}"
        );
        // The total is the per-message sum plus the tool schema, and the
        // schema charge is reported on the estimate.
        assert_eq!(
            estimate.input_tokens,
            message_tokens.iter().sum::<usize>() + 7
        );
        assert_eq!(estimate.tool_schema_tokens, 7);
    }

    #[test]
    fn remote_report_counts_fall_back_to_heuristic() {
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content("hello")
                .build()
                .expect("user message should build"),
        )];
        let estimator = PromptEstimator::for_tests(
            "claude-sonnet-4-5",
            TokenCounterKind::AnthropicCountTokens,
            None,
        );

        let (message_tokens, estimate) =
            estimator.estimate_messages_for_report_with_tool_schema_tokens(&messages, 0);
        let expected = heuristic_message_tokens_for_report(&messages);

        assert_eq!(message_tokens, expected);
        assert_eq!(estimate.source, TokenCounterKind::Heuristic);
        assert_eq!(estimate.confidence, EstimateConfidence::Low);
    }

    #[tokio::test]
    async fn anthropic_count_tokens_success_is_high_confidence() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages/count_tokens"))
            .and(header("x-api-key", "key"))
            .and(body_partial_json(json!({
                "model": "claude-sonnet-4-5",
                "system": "system",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "input_tokens": 42
            })))
            .mount(&server)
            .await;
        let session = provider_session("key", &server.uri(), "claude-sonnet-4-5");
        let estimator = PromptEstimator::from_metadata(
            &crate::provider::anthropic::ANTHROPIC_METADATA,
            &session,
        );
        let messages = vec![
            ChatCompletionRequestMessage::System(
                ChatCompletionRequestSystemMessageArgs::default()
                    .content("system")
                    .build()
                    .expect("system message should build"),
            ),
            ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessageArgs::default()
                    .content("hello")
                    .build()
                    .expect("user message should build"),
            ),
        ];

        let estimate = estimator.estimate_prompt(&messages, &[]).await;

        assert_eq!(estimate.input_tokens, 42);
        assert_eq!(estimate.source, TokenCounterKind::AnthropicCountTokens);
        assert_eq!(estimate.confidence, EstimateConfidence::High);
    }

    #[tokio::test]
    async fn zai_tokenizer_success_is_high_confidence() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tokenizer"))
            .and(header("authorization", "Bearer zai-key"))
            .and(body_partial_json(json!({
                "model": "glm-5.2",
                "messages": [{"role": "user", "content": "hello"}],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "usage": {"prompt_tokens": 37, "total_tokens": 37}
            })))
            .mount(&server)
            .await;
        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("built-in catalog should load");
        let registry = crate::provider::ProviderRegistry::from_catalog(&catalog);
        let factory = registry.get("zai").expect("Z.AI provider should exist");
        let metadata = factory.metadata();
        let session = provider_session("zai-key", &server.uri(), "glm-5.2");
        let estimator = PromptEstimator::from_metadata(metadata, &session);
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content("hello")
                .build()
                .expect("user message should build"),
        )];

        let estimate = estimator.estimate_prompt(&messages, &[]).await;

        assert_eq!(estimate.input_tokens, 37);
        assert_eq!(estimate.source, TokenCounterKind::ZaiTokenizer);
        assert_eq!(estimate.confidence, EstimateConfidence::High);
    }

    #[tokio::test]
    async fn resolved_model_count_tokens_uses_resolved_endpoint_and_remote_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages/count_tokens"))
            .and(header("x-api-key", "key"))
            .and(body_partial_json(json!({
                "model": "minimax-m3",
                "system": "system",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "input_tokens": 24
            })))
            .mount(&server)
            .await;
        let catalog = crate::model_catalog::ModelCatalog::load_builtin()
            .expect("built-in catalog should load");
        let connection_id = "opencode"
            .parse::<crate::model_catalog::ConnectionId>()
            .expect("opencode is a connection id");
        let resolved = catalog
            .resolve_connection_model(&connection_id, "minimax-m3")
            .expect("opencode MiniMax should resolve");
        let session = provider_session("key", &server.uri(), "minimax-m3");
        let estimator = PromptEstimator::from_resolved_model(
            &crate::provider::opencode::OPENCODE_METADATA,
            &session,
            &resolved,
        );
        let messages = vec![
            ChatCompletionRequestMessage::System(
                ChatCompletionRequestSystemMessageArgs::default()
                    .content("system")
                    .build()
                    .expect("system message should build"),
            ),
            ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessageArgs::default()
                    .content("hello")
                    .build()
                    .expect("user message should build"),
            ),
        ];

        let estimate = estimator.estimate_prompt(&messages, &[]).await;

        assert_eq!(estimate.input_tokens, 24);
        assert_eq!(estimate.source, TokenCounterKind::AnthropicCountTokens);
        assert_eq!(estimate.confidence, EstimateConfidence::High);
    }

    #[tokio::test]
    async fn anthropic_count_tokens_404_falls_back_to_heuristic() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages/count_tokens"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let session = provider_session("key", &server.uri(), "claude-sonnet-4-5");
        let estimator = PromptEstimator::from_metadata(
            &crate::provider::anthropic::ANTHROPIC_METADATA,
            &session,
        );
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content("hello")
                .build()
                .expect("user message should build"),
        )];

        let estimate = estimator.estimate_prompt(&messages, &[]).await;

        assert_eq!(estimate.source, TokenCounterKind::Heuristic);
        assert_eq!(estimate.confidence, EstimateConfidence::Low);
        assert!(estimate.input_tokens > 0);
    }

    #[tokio::test]
    async fn anthropic_count_tokens_network_error_falls_back_to_heuristic() {
        let session = provider_session("key", "http://127.0.0.1:9", "claude-sonnet-4-5");
        let estimator = PromptEstimator::from_metadata(
            &crate::provider::anthropic::ANTHROPIC_METADATA,
            &session,
        );
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content("hello")
                .build()
                .expect("user message should build"),
        )];

        let estimate = estimator.estimate_prompt(&messages, &[]).await;

        assert_eq!(estimate.source, TokenCounterKind::Heuristic);
        assert_eq!(estimate.confidence, EstimateConfidence::Low);
        assert!(estimate.input_tokens > 0);
    }
}
