//! Live model discovery, dispatched on a connection's [`DiscoveryKind`].
//!
//! The OpenAI-compatible `/models` surface many local servers expose returns
//! bare model ids — no context window, no capabilities. Their native APIs are
//! richer: LM Studio reports `max_context_length` and display names on
//! `/api/v1/models` (and `/api/v0/models`), Ollama reports context length and
//! a capabilities array via `/api/show`. This module owns those probes plus
//! the generic per-transport listing fetch, so callers get the richest
//! [`LiveModelAvailability`] the server can provide and degrade to the generic
//! listing when a native probe fails.

use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use serde_json::Value;

use crate::model_catalog::{AvailableModel, DiscoveryKind, LiveModelAvailability, ModelFeature};
use crate::provider::anthropic::ANTHROPIC_API_VERSION;
use crate::provider::openai_catalog::{
    available_model_from_item, available_models_from_response, is_coding_model_id,
};
use crate::provider::{
    Protocol, ProviderMetadata, ReasoningCodec, ReasoningSelection, TokenCounterKind, sse,
};

/// Per-request timeout for native discovery probes. Local servers answer in
/// milliseconds; a probe that takes longer is down or wedged, and the caller
/// falls back to the generic listing (or its own fallback ladder) instead of
/// hanging the picker.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// OpenRouter returns the complete public tool-capable catalog in one response,
/// which is substantially larger than a local native probe.
const REMOTE_CATALOG_TIMEOUT: Duration = Duration::from_secs(15);
const ZAI_OPENAPI_URL: &str = "https://docs.z.ai/openapi.json";

/// Bound on per-model `/api/show` calls for one Ollama listing; beyond this
/// the remaining models are listed without metadata rather than hammering the
/// server with unbounded requests.
const OLLAMA_SHOW_CAP: usize = 50;
const OLLAMA_SHOW_CONCURRENCY: usize = 4;

/// Fetch the live model list for a connection via its metadata's discovery
/// kind and protocol.
pub(crate) async fn fetch_models_for_metadata(
    metadata: &ProviderMetadata,
    base_url: &str,
    api_key: &str,
) -> Result<LiveModelAvailability> {
    // `Static` connections curate their model list in the catalog and never hit
    // the network (see `DiscoveryKind::Static`): offer exactly the seed models.
    let mut availability = if metadata.discovery == DiscoveryKind::Static {
        LiveModelAvailability::from_remote_ids(metadata.seed_model_list())
    } else {
        fetch_models_with_discovery(
            metadata.discovery,
            metadata.protocol,
            &metadata.display_name,
            base_url,
            api_key,
            metadata.auth_header.as_deref(),
        )
        .await?
    };
    availability.models.retain(|model| {
        !metadata
            .model_exclude_prefixes
            .iter()
            .any(|prefix| model.remote_model_id.starts_with(prefix.as_ref()))
    });
    Ok(availability)
}

/// Fetch the live model list for an endpoint, using the given discovery kind.
/// Native probes degrade to the generic per-transport listing on any failure,
/// so a misconfigured kind is never worse than `Generic`. A *successful*
/// native probe is trusted even when it lists no chat models — the generic
/// listing would only re-add entries the probe deliberately filtered (e.g. an
/// LM Studio serving only embedding models). `display_label` names the
/// provider in error messages and logs.
pub(crate) async fn fetch_models_with_discovery(
    discovery: DiscoveryKind,
    protocol: Protocol,
    display_label: &str,
    base_url: &str,
    api_key: &str,
    auth_header: Option<&str>,
) -> Result<LiveModelAvailability> {
    let native = match discovery {
        DiscoveryKind::Generic => {
            return fetch_generic_models(protocol, display_label, base_url, api_key, auth_header)
                .await;
        }
        // `Static` is resolved from the catalog in `fetch_models_for_metadata`
        // before reaching here; this arm only guards the wizard/local path,
        // which never selects it. No network list to fetch.
        DiscoveryKind::Static => return Ok(LiveModelAvailability::default()),
        DiscoveryKind::Gemini => fetch_gemini_models(base_url, api_key).await,
        DiscoveryKind::LmStudio => fetch_lm_studio_models(base_url, api_key).await,
        DiscoveryKind::Mistral => fetch_mistral_models(base_url, api_key).await,
        DiscoveryKind::Ollama => fetch_ollama_models(base_url).await,
        DiscoveryKind::OpenRouter => fetch_openrouter_models(base_url, api_key).await,
        DiscoveryKind::Tencent => fetch_tencent_models(base_url, api_key).await,
        DiscoveryKind::QwenCloud => fetch_qwencloud_models(base_url, api_key, auth_header).await,
        DiscoveryKind::Zai => fetch_zai_models().await,
    };
    match native {
        Ok(availability) => Ok(availability),
        Err(err) if discovery == DiscoveryKind::QwenCloud => Err(err),
        Err(err) => {
            tracing::warn!(
                provider = %display_label,
                kind = ?discovery,
                error = %err,
                "native model probe failed; falling back to generic listing"
            );
            fetch_generic_models(protocol, display_label, base_url, api_key, auth_header).await
        }
    }
}

// ---------------------------------------------------------------------------
// Qwen Cloud filtered OpenAI-compatible catalog
// ---------------------------------------------------------------------------

async fn fetch_qwencloud_models(
    base_url: &str,
    api_key: &str,
    auth_header: Option<&str>,
) -> Result<LiveModelAvailability> {
    let availability = fetch_generic_models(
        Protocol::OpenAiChat,
        "Qwen Cloud",
        base_url,
        api_key,
        auth_header,
    )
    .await?;
    Ok(LiveModelAvailability {
        models: availability
            .models
            .into_iter()
            .filter(|model| is_qwencloud_chat_model(&model.remote_model_id))
            .collect(),
        ..availability
    })
}

fn is_qwencloud_chat_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    id.starts_with("qwen")
        && ![
            "embedding",
            "rerank",
            "tts",
            "asr",
            "audio",
            "omni",
            "image",
        ]
        .iter()
        .any(|product| id.contains(product))
}

// ---------------------------------------------------------------------------
// Z.AI official OpenAPI catalog
// ---------------------------------------------------------------------------

/// Fetch Z.AI's documented tool-capable chat lineup. Z.AI does not expose a
/// `/models` endpoint; model enums in its official OpenAPI document are the
/// machine-readable source of truth used by its own API reference.
async fn fetch_zai_models() -> Result<LiveModelAvailability> {
    let response = crate::provider::http_client()
        .get(ZAI_OPENAPI_URL)
        .timeout(REMOTE_CATALOG_TIMEOUT)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Failed to fetch Z.AI OpenAPI model catalog")?;
    if !response.status().is_success() {
        return Err(sse::error_from_response(response).await.into());
    }
    let value: Value = response
        .json()
        .await
        .context("Failed to parse Z.AI OpenAPI model catalog")?;
    Ok(LiveModelAvailability {
        models: zai_models_from_openapi(&value)?,
        ..LiveModelAvailability::default()
    })
}

fn zai_models_from_openapi(value: &Value) -> Result<Vec<AvailableModel>> {
    let Some(text_models) = value
        .pointer("/components/schemas/ChatCompletionTextRequest/properties/model/enum")
        .and_then(Value::as_array)
    else {
        anyhow::bail!("Z.AI OpenAPI catalog has no text-model enum");
    };
    let vision_models = value
        .pointer("/components/schemas/ChatCompletionVisionRequest/properties/model/enum")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| model.as_str().is_some_and(|id| id.starts_with("glm-4.6v")));
    let mut seen = std::collections::HashSet::new();
    let models = text_models
        .iter()
        .chain(vision_models)
        .filter_map(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .filter(|id| seen.insert(*id))
        .map(zai_model_from_id)
        .collect();
    Ok(models)
}

fn zai_model_from_id(id: &str) -> AvailableModel {
    let mut features = vec![
        ModelFeature::ToolCall,
        ModelFeature::StructuredOutput,
        ModelFeature::Temperature,
    ];
    if id.starts_with("glm-4.6v") {
        features.push(ModelFeature::Attachment);
    }
    let supports_thinking = id != "glm-4-32b-0414-128k";
    if supports_thinking {
        features.push(ModelFeature::Reasoning);
    }
    let mut model = AvailableModel::with_metadata(id, None, None, features)
        .with_token_counter(Some(TokenCounterKind::ZaiTokenizer));
    if zai_supports_reasoning_effort(id) {
        model = model
            .with_reasoning_codec(ReasoningCodec::ZaiThinking)
            .with_reasoning(
                vec![
                    ReasoningSelection::Off,
                    ReasoningSelection::Minimal,
                    ReasoningSelection::Low,
                    ReasoningSelection::Medium,
                    ReasoningSelection::High,
                    ReasoningSelection::XHigh,
                    ReasoningSelection::Max,
                ],
                Some(ReasoningSelection::Max),
            );
    } else if supports_thinking {
        model = model
            .with_reasoning_codec(ReasoningCodec::ZaiThinking)
            .with_reasoning(vec![ReasoningSelection::Off, ReasoningSelection::On], None);
    }
    model
}

fn zai_supports_reasoning_effort(id: &str) -> bool {
    let Some(version) = id.strip_prefix("glm-") else {
        return false;
    };
    let mut components = version.split(['.', '-']);
    let major = components
        .next()
        .and_then(|component| component.parse::<u32>().ok());
    let minor = components
        .next()
        .and_then(|component| component.parse::<u32>().ok());
    major.is_some_and(|major| major > 5)
        || (major == Some(5) && minor.is_some_and(|minor| minor >= 2))
}

// ---------------------------------------------------------------------------
// Generic per-transport listing
// ---------------------------------------------------------------------------

/// Fetch the provider's model catalog from the transport's standard listing
/// endpoint. One request builder, one JSON parse, one error mapping; only the
/// endpoint path and auth header vary by protocol (OpenAI: `/models` + bearer;
/// Anthropic: `/v1/models` + `x-api-key` + `anthropic-version`). The auth
/// header is omitted when the key is blank so keyless local endpoints are not
/// rejected.
async fn fetch_generic_models(
    protocol: Protocol,
    display_label: &str,
    base_url: &str,
    api_key: &str,
    auth_header: Option<&str>,
) -> Result<LiveModelAvailability> {
    let base = base_url.trim_end_matches('/');
    let api_key = api_key.trim();
    let builder = match protocol {
        Protocol::OpenAiChat => {
            let mut builder = crate::provider::http_client()
                .get(format!("{base}/models"))
                .header("Accept", "application/json");
            if !api_key.is_empty() {
                // A connection override sends the raw key under its own header;
                // otherwise the OpenAI default `Authorization: Bearer`.
                builder = match auth_header {
                    Some(header) => builder.header(header, api_key),
                    None => builder.header("Authorization", format!("Bearer {api_key}")),
                };
            }
            builder
        }
        Protocol::AnthropicMessages => {
            let mut builder = crate::provider::http_client()
                .get(format!("{base}/v1/models"))
                .header("Accept", "application/json")
                .header("anthropic-version", ANTHROPIC_API_VERSION);
            if !api_key.is_empty() {
                builder = builder.header(auth_header.unwrap_or("x-api-key"), api_key);
            }
            builder
        }
        Protocol::CodexResponses => {
            anyhow::bail!("{display_label} does not support a model catalog endpoint")
        }
    };

    let response = builder
        .send()
        .await
        .with_context(|| format!("Failed to fetch {display_label} model list"))?;
    if !response.status().is_success() {
        return Err(sse::error_from_response(response).await.into());
    }
    let value: Value = response
        .json()
        .await
        .with_context(|| format!("Failed to parse {display_label} model list response"))?;
    let mut models = available_models_from_response(&value);
    sort_models(&mut models);
    Ok(LiveModelAvailability {
        models,
        ..LiveModelAvailability::default()
    })
}

// ---------------------------------------------------------------------------
// Tencent TokenHub native model probe
// ---------------------------------------------------------------------------

/// Fetch TokenHub's authenticated catalog and retain only online Tencent
/// Hunyuan language models. TokenHub is a multi-vendor, multi-modal gateway;
/// exposing its entire `/models` payload under the Tencent connection would
/// mix unrelated vendors and non-chat assets into the coding-model picker.
async fn fetch_tencent_models(base_url: &str, api_key: &str) -> Result<LiveModelAvailability> {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let mut builder = crate::provider::http_client()
        .get(endpoint)
        .timeout(REMOTE_CATALOG_TIMEOUT)
        .header("Accept", "application/json");
    let api_key = api_key.trim();
    if !api_key.is_empty() {
        builder = builder.bearer_auth(api_key);
    }
    let response = builder
        .send()
        .await
        .context("Failed to fetch Tencent TokenHub model list")?;
    if !response.status().is_success() {
        return Err(sse::error_from_response(response).await.into());
    }
    let value: Value = response
        .json()
        .await
        .context("Failed to parse Tencent TokenHub model list")?;
    let models = tencent_models_from_response(&value)?;
    Ok(LiveModelAvailability {
        models,
        ..LiveModelAvailability::default()
    })
}

fn tencent_models_from_response(value: &Value) -> Result<Vec<AvailableModel>> {
    let Some(items) = value.get("data").and_then(Value::as_array) else {
        anyhow::bail!("Tencent TokenHub model list response has no `data` array");
    };
    let mut models = items
        .iter()
        .filter(|item| {
            item.get("status")
                .and_then(Value::as_str)
                .is_none_or(|status| status.eq_ignore_ascii_case("online"))
        })
        .filter_map(tencent_model_from_item)
        .collect::<Vec<_>>();
    models.sort_by(|left, right| {
        tencent_model_rank(&left.remote_model_id)
            .cmp(&tencent_model_rank(&right.remote_model_id))
            .then_with(|| left.remote_model_id.cmp(&right.remote_model_id))
    });
    models.dedup_by(|left, right| left.remote_model_id == right.remote_model_id);
    Ok(models)
}

fn tencent_model_from_item(item: &Value) -> Option<AvailableModel> {
    let id = item.get("id").and_then(Value::as_str)?;
    if !is_hunyuan_language_model(id) {
        return None;
    }
    let display_name = item
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty() && !name.eq_ignore_ascii_case(id))
        .map(str::to_string);

    let mut model = AvailableModel::with_metadata(id, None, display_name, Vec::new())
        .with_token_counter(Some(TokenCounterKind::Heuristic));
    let known_output_limit = match id.to_ascii_lowercase().as_str() {
        "hy3" => Some(128_000),
        "hy3-preview" => Some(128_000),
        _ => None,
    };
    if let Some(output_limit) = known_output_limit {
        model.context_window = Some(256_000);
        model.output_limit = Some(output_limit);
        model.features = vec![
            ModelFeature::ToolCall,
            ModelFeature::Reasoning,
            ModelFeature::StructuredOutput,
            ModelFeature::Temperature,
        ];
        model = model
            .with_reasoning_codec(ReasoningCodec::Hunyuan)
            .with_reasoning(
                vec![
                    ReasoningSelection::Off,
                    ReasoningSelection::Low,
                    ReasoningSelection::High,
                ],
                Some(ReasoningSelection::High),
            );
    }
    Some(model)
}

fn is_hunyuan_language_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    let family = id
        .strip_prefix("hy")
        .and_then(|suffix| suffix.chars().next())
        .is_some_and(|first| first.is_ascii_digit())
        || id.starts_with("hunyuan-");
    family
        && ![
            "embedding",
            "image",
            "video",
            "vision",
            "translation",
            "role",
            "tts",
            "asr",
            "3d",
        ]
        .iter()
        .any(|excluded| id.contains(excluded))
}

fn tencent_model_rank(id: &str) -> u8 {
    let id = id.to_ascii_lowercase();
    match id.as_str() {
        "hy3" => 0,
        "hy3-preview" => 1,
        _ if id
            .strip_prefix("hy")
            .and_then(|suffix| suffix.chars().next())
            .is_some_and(|first| first.is_ascii_digit()) =>
        {
            2
        }
        _ => 3,
    }
}

// ---------------------------------------------------------------------------
// OpenRouter native model probe
// ---------------------------------------------------------------------------

/// Fetch OpenRouter's rich public catalog, restricted to text-output models
/// that advertise tool calling and ranked by current weekly usage. The
/// response supplies billed pricing (including long-context overrides), route
/// limits, tokenizers, and normalized reasoning controls that the generic
/// OpenAI-compatible listing parser cannot infer on its own.
async fn fetch_openrouter_models(base_url: &str, api_key: &str) -> Result<LiveModelAvailability> {
    let endpoint = format!("{}/models", base_url.trim_end_matches('/'));
    let mut url = reqwest::Url::parse(&endpoint)
        .with_context(|| format!("Invalid OpenRouter models endpoint {endpoint}"))?;
    url.query_pairs_mut()
        .append_pair("output_modalities", "text")
        .append_pair("supported_parameters", "tools")
        .append_pair("sort", "most-popular");

    let mut builder = crate::provider::http_client()
        .get(url)
        .timeout(REMOTE_CATALOG_TIMEOUT)
        .header("Accept", "application/json");
    let api_key = api_key.trim();
    if !api_key.is_empty() {
        builder = builder.bearer_auth(api_key);
    }
    let response = builder
        .send()
        .await
        .context("Failed to fetch OpenRouter model list")?;
    if !response.status().is_success() {
        return Err(sse::error_from_response(response).await.into());
    }
    let value: Value = response
        .json()
        .await
        .context("Failed to parse OpenRouter model list")?;
    let models = openrouter_models_from_response(&value)?;
    Ok(LiveModelAvailability {
        models,
        ..LiveModelAvailability::default()
    })
}

fn openrouter_models_from_response(value: &Value) -> Result<Vec<AvailableModel>> {
    let Some(items) = value.get("data").and_then(Value::as_array) else {
        anyhow::bail!("OpenRouter model list response has no `data` array");
    };
    let mut seen = std::collections::HashSet::new();
    Ok(items
        .iter()
        .filter(|item| openrouter_supports_parameter(item, "tools"))
        .filter(|item| openrouter_supports_text_output(item))
        .filter_map(openrouter_model_from_item)
        // Preserve OpenRouter's `most-popular` order while defending against
        // malformed duplicate rows.
        .filter(|model| seen.insert(model.remote_model_id.to_string()))
        .collect())
}

fn openrouter_model_from_item(item: &Value) -> Option<AvailableModel> {
    let mut model = available_model_from_item(item)?;
    let top_provider = item.get("top_provider");
    if let Some(context_window) = top_provider
        .and_then(|provider| provider.get("context_length"))
        .and_then(positive_u32_from_value)
    {
        model.context_window = Some(context_window);
    }
    model.output_limit = top_provider
        .and_then(|provider| provider.get("max_completion_tokens"))
        .and_then(positive_u32_from_value);
    model.reasoning_codec = Some(ReasoningCodec::OpenRouter);
    if model.token_counter.is_none() {
        model.token_counter = Some(TokenCounterKind::Heuristic);
    }
    if let Some((supported, recommended)) = openrouter_reasoning_from_item(item) {
        model = model.with_reasoning(supported, recommended);
        push_unique_feature(&mut model.features, ModelFeature::Reasoning);
    }
    Some(model)
}

fn openrouter_supports_parameter(item: &Value, parameter: &str) -> bool {
    item.get("supported_parameters")
        .and_then(Value::as_array)
        .is_some_and(|parameters| {
            parameters
                .iter()
                .filter_map(Value::as_str)
                .any(|value| value == parameter)
        })
}

fn openrouter_supports_text_output(item: &Value) -> bool {
    item.get("architecture")
        .and_then(|architecture| architecture.get("output_modalities"))
        .and_then(Value::as_array)
        .is_none_or(|modalities| {
            modalities
                .iter()
                .filter_map(Value::as_str)
                .any(|modality| modality == "text")
        })
}

fn openrouter_reasoning_from_item(
    item: &Value,
) -> Option<(Vec<ReasoningSelection>, Option<ReasoningSelection>)> {
    let reasoning = item.get("reasoning")?.as_object()?;
    let mandatory = reasoning
        .get("mandatory")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut supported = Vec::new();
    if !mandatory {
        supported.push(ReasoningSelection::Off);
    }

    match reasoning.get("supported_efforts") {
        Some(Value::Array(efforts)) => supported.extend(
            efforts
                .iter()
                .filter_map(Value::as_str)
                .filter_map(openrouter_effort),
        ),
        // OpenRouter documents null as accepting every gateway effort value.
        Some(Value::Null) => supported.extend([
            ReasoningSelection::Minimal,
            ReasoningSelection::Low,
            ReasoningSelection::Medium,
            ReasoningSelection::High,
            ReasoningSelection::XHigh,
            ReasoningSelection::Max,
        ]),
        _ => {}
    }
    if mandatory {
        supported.retain(|selection| *selection != ReasoningSelection::Off);
    }

    let supports_max_tokens = reasoning
        .get("supports_max_tokens")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if supports_max_tokens {
        supported.push(ReasoningSelection::BudgetTokens(4_096));
    }
    if supported.is_empty() || (!mandatory && supported == [ReasoningSelection::Off]) {
        supported.push(ReasoningSelection::On);
    }

    let default_enabled = reasoning.get("default_enabled").and_then(Value::as_bool);
    let default_effort = reasoning
        .get("default_effort")
        .and_then(Value::as_str)
        .and_then(openrouter_effort);
    let recommended = match (default_enabled, default_effort) {
        (Some(false), _) if !mandatory => Some(ReasoningSelection::Off),
        (_, Some(effort)) => Some(effort),
        (Some(true), None) if supports_max_tokens => Some(ReasoningSelection::BudgetTokens(4_096)),
        (Some(true), None) => Some(ReasoningSelection::On),
        _ => None,
    };
    Some((supported, recommended))
}

fn openrouter_effort(value: &str) -> Option<ReasoningSelection> {
    match value {
        "none" => Some(ReasoningSelection::Off),
        "minimal" => Some(ReasoningSelection::Minimal),
        "low" => Some(ReasoningSelection::Low),
        "medium" => Some(ReasoningSelection::Medium),
        "high" => Some(ReasoningSelection::High),
        "xhigh" => Some(ReasoningSelection::XHigh),
        "max" => Some(ReasoningSelection::Max),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Mistral native model probe
// ---------------------------------------------------------------------------

/// Fetch Mistral's model cards rather than treating `/models` as an OpenAI
/// list. The native response identifies archived/non-chat entries, aliases,
/// context limits, function calling, and vision support. Bonsai is a tool-using
/// coding agent, so a model must support both chat completions and function
/// calling to be selectable.
async fn fetch_mistral_models(base_url: &str, api_key: &str) -> Result<LiveModelAvailability> {
    let base = base_url.trim_end_matches('/');
    let mut builder = crate::provider::http_client()
        .get(format!("{base}/models"))
        .timeout(PROBE_TIMEOUT)
        .header("Accept", "application/json");
    let api_key = api_key.trim();
    if !api_key.is_empty() {
        builder = builder.bearer_auth(api_key);
    }
    let response = builder
        .send()
        .await
        .context("Failed to fetch Mistral native model list")?;
    if !response.status().is_success() {
        return Err(sse::error_from_response(response).await.into());
    }
    let value: Value = response
        .json()
        .await
        .context("Failed to parse Mistral native model list")?;
    let mut models = mistral_models_from_response(&value)?;
    sort_models(&mut models);
    models.dedup_by(|left, right| left.remote_model_id == right.remote_model_id);
    Ok(LiveModelAvailability {
        models,
        ..LiveModelAvailability::default()
    })
}

fn mistral_models_from_response(value: &Value) -> Result<Vec<AvailableModel>> {
    let Some(items) = value.get("data").and_then(Value::as_array) else {
        anyhow::bail!("Mistral native model list response has no `data` array");
    };
    Ok(items
        .iter()
        .filter(|item| item.get("archived").and_then(Value::as_bool) != Some(true))
        .filter_map(|item| {
            let capabilities = item.get("capabilities")?;
            let supports_chat = capabilities
                .get("completion_chat")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let supports_tools = capabilities
                .get("function_calling")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            (supports_chat && supports_tools).then_some((item, capabilities))
        })
        .flat_map(|(item, capabilities)| {
            let mut features = vec![ModelFeature::ToolCall];
            if capabilities
                .get("vision")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                features.push(ModelFeature::Attachment);
            }
            if capabilities
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                features.push(ModelFeature::Reasoning);
            }
            let context_window = item
                .get("max_context_length")
                .and_then(positive_u32_from_value);
            let ids = item.get("id").and_then(Value::as_str).into_iter().chain(
                item.get("aliases")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str),
            );
            ids.map(move |id| {
                AvailableModel::with_metadata(id, context_window, None, features.clone())
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Google Gemini native model probe
// ---------------------------------------------------------------------------

/// Fetch Gemini's native paginated model catalog. The OpenAI-compatible
/// `/models` shim reports only ids; the native endpoint also supplies input
/// and output limits and the supported generation methods.
async fn fetch_gemini_models(base_url: &str, api_key: &str) -> Result<LiveModelAvailability> {
    let endpoint = gemini_models_endpoint(base_url);
    let mut models = Vec::new();
    let mut page_token: Option<String> = None;

    loop {
        let mut url = reqwest::Url::parse(&endpoint)
            .with_context(|| format!("Invalid Gemini models endpoint {endpoint}"))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("pageSize", "1000");
            if let Some(token) = page_token.as_deref() {
                query.append_pair("pageToken", token);
            }
        }

        let mut builder = crate::provider::http_client()
            .get(url)
            .timeout(PROBE_TIMEOUT)
            .header("Accept", "application/json");
        let api_key = api_key.trim();
        if !api_key.is_empty() {
            builder = builder.header("x-goog-api-key", api_key);
        }
        let response = builder
            .send()
            .await
            .context("Failed to fetch Gemini native model list")?;
        if !response.status().is_success() {
            return Err(sse::error_from_response(response).await.into());
        }
        let value: Value = response
            .json()
            .await
            .context("Failed to parse Gemini native model list")?;
        models.extend(gemini_models_from_response(&value)?);
        page_token = value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|token| !token.is_empty());
        if page_token.is_none() {
            break;
        }
    }

    sort_models(&mut models);
    models.dedup_by(|left, right| left.remote_model_id == right.remote_model_id);
    Ok(LiveModelAvailability {
        models,
        ..LiveModelAvailability::default()
    })
}

fn gemini_models_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    let native_root = base.strip_suffix("/openai").unwrap_or(base);
    format!("{native_root}/models")
}

fn gemini_models_from_response(value: &Value) -> Result<Vec<AvailableModel>> {
    let Some(items) = value.get("models").and_then(Value::as_array) else {
        anyhow::bail!("Gemini native model list response has no `models` array");
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            let supports_generate_content = item
                .get("supportedGenerationMethods")
                .and_then(Value::as_array)
                .is_some_and(|methods| {
                    methods
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|method| method == "generateContent")
                });
            if !supports_generate_content {
                return None;
            }
            let name = item.get("name").and_then(Value::as_str)?;
            let id = name.strip_prefix("models/").unwrap_or(name);
            if !is_coding_model_id(id) {
                return None;
            }
            let context_window = item
                .get("inputTokenLimit")
                .and_then(positive_u32_from_value);
            let output_limit = item
                .get("outputTokenLimit")
                .and_then(positive_u32_from_value);
            let display_name = item
                .get("displayName")
                .and_then(Value::as_str)
                .filter(|display_name| !display_name.trim().is_empty() && *display_name != id)
                .map(str::to_string);
            Some(
                AvailableModel::with_metadata(id, context_window, display_name, Vec::new())
                    .with_output_limit(output_limit),
            )
        })
        .collect())
}

// ---------------------------------------------------------------------------
// LM Studio native probe
// ---------------------------------------------------------------------------

/// Fetch models from LM Studio's native REST API, which reports
/// `max_context_length` and display names that the OpenAI-compatible surface
/// omits. Tries the current `/api/v1/models` first, then the older
/// `/api/v0/models`; the caller degrades to the generic listing when both
/// fail.
async fn fetch_lm_studio_models(base_url: &str, api_key: &str) -> Result<LiveModelAvailability> {
    let root = server_root(base_url);
    let v1 = fetch_probe_json(&format!("{root}/api/v1/models"), api_key).await;
    let value = match v1 {
        Ok(value) => value,
        Err(v1_err) => fetch_probe_json(&format!("{root}/api/v0/models"), api_key)
            .await
            .map_err(|v0_err| v1_err.context(v0_err))?,
    };
    let mut models = lm_studio_models_from_response(&value)?;
    sort_models(&mut models);
    Ok(LiveModelAvailability {
        models,
        ..LiveModelAvailability::default()
    })
}

/// Parse either LM Studio response shape: `/api/v1/models` lists under
/// `models` with `key` + `display_name`; `/api/v0/models` lists under `data`
/// with `id`. Embedding models are filtered out — they are listed alongside
/// LLMs but cannot chat. An unrecognized shape is an error, not an empty
/// list, so the caller's generic fallback can kick in.
fn lm_studio_models_from_response(value: &Value) -> Result<Vec<AvailableModel>> {
    let items = ["models", "data"]
        .into_iter()
        .find_map(|field| value.get(field).and_then(Value::as_array));
    let Some(items) = items else {
        anyhow::bail!("LM Studio model list response has no `models` or `data` array");
    };
    Ok(items.iter().filter_map(lm_studio_model_from_item).collect())
}

fn lm_studio_model_from_item(item: &Value) -> Option<AvailableModel> {
    let id = ["key", "id"]
        .into_iter()
        .find_map(|field| item.get(field).and_then(Value::as_str))?;
    let model_type = item.get("type").and_then(Value::as_str).unwrap_or_default();
    // v1 reports "embedding", v0 "embeddings" — match the prefix.
    if model_type.starts_with("embedding") {
        return None;
    }
    let mut features = features_from_capabilities(item.get("capabilities"));
    if model_type == "vlm" {
        push_unique_feature(&mut features, ModelFeature::Attachment);
    }
    let context_window = item
        .get("max_context_length")
        .and_then(positive_u32_from_value);
    let display_name = item
        .get("display_name")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty() && *name != id)
        .map(str::to_string);
    Some(AvailableModel::with_metadata(
        id,
        context_window,
        display_name,
        features,
    ))
}

// ---------------------------------------------------------------------------
// Ollama native probe
// ---------------------------------------------------------------------------

/// Fetch models from Ollama's native API: `/api/tags` for the list, then a
/// bounded fan-out of `/api/show` calls for per-model context length and
/// capabilities. A failed `show` degrades that one entry to a bare id — the
/// listing itself never fails on metadata.
async fn fetch_ollama_models(base_url: &str) -> Result<LiveModelAvailability> {
    let root = server_root(base_url);
    let tags = fetch_probe_json(&format!("{root}/api/tags"), "").await?;
    // An unrecognized shape is an error, not an empty library, so the
    // caller's generic fallback can kick in.
    let Some(items) = tags.get("models").and_then(Value::as_array) else {
        anyhow::bail!("Ollama /api/tags response has no `models` array");
    };
    let names: Vec<String> = items
        .iter()
        .filter_map(|item| item.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect();

    if names.len() > OLLAMA_SHOW_CAP {
        tracing::warn!(
            total = names.len(),
            cap = OLLAMA_SHOW_CAP,
            "Ollama library exceeds metadata probe cap; listing the rest without metadata"
        );
    }
    let mut models: Vec<AvailableModel> = futures::stream::iter(names.into_iter().enumerate())
        .map(|(index, name)| {
            let root = root.clone();
            async move {
                if index >= OLLAMA_SHOW_CAP {
                    return AvailableModel::remote(name);
                }
                match fetch_ollama_show(&root, &name).await {
                    Ok(model) => model,
                    Err(err) => {
                        tracing::debug!(model = %name, error = %err, "ollama show failed");
                        AvailableModel::remote(name)
                    }
                }
            }
        })
        .buffer_unordered(OLLAMA_SHOW_CONCURRENCY)
        .collect()
        .await;
    sort_models(&mut models);
    Ok(LiveModelAvailability {
        models,
        ..LiveModelAvailability::default()
    })
}

async fn fetch_ollama_show(root: &str, name: &str) -> Result<AvailableModel> {
    let response = crate::provider::http_client()
        .post(format!("{root}/api/show"))
        .timeout(PROBE_TIMEOUT)
        .json(&serde_json::json!({ "model": name }))
        .send()
        .await
        .with_context(|| format!("Failed to fetch Ollama metadata for {name}"))?;
    if !response.status().is_success() {
        anyhow::bail!("Ollama /api/show for {name} returned {}", response.status());
    }
    let value: Value = response
        .json()
        .await
        .with_context(|| format!("Failed to parse Ollama metadata for {name}"))?;
    Ok(ollama_model_from_show(name, &value))
}

fn ollama_model_from_show(name: &str, value: &Value) -> AvailableModel {
    let context_window = value
        .get("model_info")
        .and_then(Value::as_object)
        .and_then(|info| {
            info.iter()
                .find(|(key, _)| key.ends_with(".context_length"))
                .and_then(|(_, value)| positive_u32_from_value(value))
        });
    let features = features_from_capabilities(value.get("capabilities"));
    AvailableModel::with_metadata(name, context_window, None, features)
}

// ---------------------------------------------------------------------------
// Server detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetectedServer {
    LmStudio,
    Ollama,
    OpenAiCompatible,
    AnthropicCompatible,
    Unreachable,
}

/// Best-effort server-kind detection for the provider add flow: probe each
/// server family's distinctive endpoint concurrently and pick the most
/// specific hit. Short timeout — this runs interactively while the user waits.
pub(crate) async fn detect_server(base_url: &str, api_key: &str) -> DetectedServer {
    const DETECT_TIMEOUT: Duration = Duration::from_secs(2);
    let root = server_root(base_url);
    let base = base_url.trim_end_matches('/');

    let lm_studio = probe_ok(
        crate::provider::http_client()
            .get(format!("{root}/api/v0/models"))
            .timeout(DETECT_TIMEOUT),
    );
    let ollama = probe_ok(
        crate::provider::http_client()
            .get(format!("{root}/api/tags"))
            .timeout(DETECT_TIMEOUT),
    );
    let openai = {
        let mut builder = crate::provider::http_client()
            .get(format!("{base}/models"))
            .timeout(DETECT_TIMEOUT)
            .header("Accept", "application/json");
        if !api_key.trim().is_empty() {
            builder = builder.header("Authorization", format!("Bearer {}", api_key.trim()));
        }
        probe_ok(builder)
    };
    let anthropic = {
        let mut builder = crate::provider::http_client()
            .get(format!("{base}/v1/models"))
            .timeout(DETECT_TIMEOUT)
            .header("Accept", "application/json")
            .header("anthropic-version", ANTHROPIC_API_VERSION);
        if !api_key.trim().is_empty() {
            builder = builder.header("x-api-key", api_key.trim());
        }
        probe_ok(builder)
    };

    let (lm_studio, ollama, openai, anthropic) =
        futures::join!(lm_studio, ollama, openai, anthropic);
    if lm_studio {
        DetectedServer::LmStudio
    } else if ollama {
        DetectedServer::Ollama
    } else if openai {
        DetectedServer::OpenAiCompatible
    } else if anthropic {
        DetectedServer::AnthropicCompatible
    } else {
        DetectedServer::Unreachable
    }
}

/// True when the probe returns 2xx JSON — the bar for "this endpoint family
/// exists here". Auth failures (401/403) don't count: they prove a server is
/// listening but not which family, and the specific probes must not
/// misclassify a locked-down generic endpoint.
async fn probe_ok(builder: reqwest::RequestBuilder) -> bool {
    match builder.send().await {
        Ok(response) if response.status().is_success() => response.json::<Value>().await.is_ok(),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The server root for native APIs: the configured chat base URL minus its
/// OpenAI-compat `/v1` suffix (LM Studio and Ollama both serve chat under
/// `/v1` and their native APIs under `/api/...` at the root).
fn server_root(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_string()
}

async fn fetch_probe_json(url: &str, api_key: &str) -> Result<Value> {
    let mut builder = crate::provider::http_client()
        .get(url)
        .timeout(PROBE_TIMEOUT)
        .header("Accept", "application/json");
    let api_key = api_key.trim();
    if !api_key.is_empty() {
        builder = builder.header("Authorization", format!("Bearer {api_key}"));
    }
    let response = builder
        .send()
        .await
        .with_context(|| format!("Failed to fetch {url}"))?;
    if !response.status().is_success() {
        anyhow::bail!("{url} returned {}", response.status());
    }
    response
        .json()
        .await
        .with_context(|| format!("Failed to parse response from {url}"))
}

/// Map a server-reported capabilities array (LM Studio, Ollama) onto catalog
/// features. Unknown entries are ignored; Ollama's `completion` is table
/// stakes, not a feature.
fn features_from_capabilities(capabilities: Option<&Value>) -> Vec<ModelFeature> {
    let mut features = Vec::new();
    let Some(items) = capabilities.and_then(Value::as_array) else {
        return features;
    };
    for capability in items.iter().filter_map(Value::as_str) {
        let feature = match capability {
            "tool_use" | "tools" => Some(ModelFeature::ToolCall),
            "vision" => Some(ModelFeature::Attachment),
            "thinking" | "reasoning" => Some(ModelFeature::Reasoning),
            _ => None,
        };
        if let Some(feature) = feature {
            push_unique_feature(&mut features, feature);
        }
    }
    features
}

fn push_unique_feature(features: &mut Vec<ModelFeature>, feature: ModelFeature) {
    if !features.contains(&feature) {
        features.push(feature);
    }
}

fn positive_u32_from_value(value: &Value) -> Option<u32> {
    let parsed = value.as_u64().and_then(|value| u32::try_from(value).ok())?;
    (parsed > 0).then_some(parsed)
}

fn sort_models(models: &mut [AvailableModel]) {
    models.sort_by(|left, right| left.remote_model_id.cmp(&right.remote_model_id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn qwencloud_discovery_filters_non_chat_products() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer dashscope-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": "qwen3.8-max-preview"},
                    {"id": "qwen3.7-plus"},
                    {"id": "qwen3-vl-plus"},
                    {"id": "qwen-max"},
                    {"id": "qwen-plus"},
                    {"id": "qwen-turbo"},
                    {"id": "qwen3-embedding-8b"},
                    {"id": "paraformer-v2"},
                    {"id": "deepseek-v4"}
                ]
            })))
            .mount(&server)
            .await;

        let availability = fetch_models_with_discovery(
            DiscoveryKind::QwenCloud,
            Protocol::OpenAiChat,
            "Qwen Cloud",
            &format!("{}/v1", server.uri()),
            "dashscope-key",
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            availability
                .models
                .iter()
                .map(|model| model.remote_model_id.as_ref())
                .collect::<Vec<_>>(),
            [
                "qwen-max",
                "qwen-plus",
                "qwen-turbo",
                "qwen3-vl-plus",
                "qwen3.7-plus",
                "qwen3.8-max-preview"
            ]
        );
    }

    #[test]
    fn zai_openapi_catalog_reads_only_tool_capable_chat_models() {
        let value = serde_json::json!({
            "components": {
                "schemas": {
                    "ChatCompletionTextRequest": {
                        "properties": {
                            "model": {
                                "enum": [
                                    "glm-5.2",
                                    "glm-4.7",
                                    "glm-4-32b-0414-128k",
                                    "glm-5.2"
                                ]
                            }
                        }
                    },
                    "ChatCompletionVisionRequest": {
                        "properties": {
                            "model": {
                                "enum": [
                                    "glm-5v-turbo",
                                    "glm-4.6v",
                                    "glm-4.6v-flashx",
                                    "glm-4.5v"
                                ]
                            }
                        }
                    }
                }
            }
        });

        let models = zai_models_from_openapi(&value).unwrap();

        assert_eq!(
            models
                .iter()
                .map(|model| model.remote_model_id.as_ref())
                .collect::<Vec<_>>(),
            [
                "glm-5.2",
                "glm-4.7",
                "glm-4-32b-0414-128k",
                "glm-4.6v",
                "glm-4.6v-flashx",
            ]
        );
        assert!(
            models
                .iter()
                .all(|model| model.features.contains(&ModelFeature::ToolCall))
        );
        assert_eq!(
            models[0].supported_reasoning,
            [
                ReasoningSelection::Off,
                ReasoningSelection::Minimal,
                ReasoningSelection::Low,
                ReasoningSelection::Medium,
                ReasoningSelection::High,
                ReasoningSelection::XHigh,
                ReasoningSelection::Max,
            ]
        );
        assert_eq!(
            models[0].recommended_reasoning,
            Some(ReasoningSelection::Max)
        );
        assert!(
            !models[2].features.contains(&ModelFeature::Reasoning),
            "the legacy GLM-4 model predates Z.AI's thinking control"
        );
        assert!(models[3].features.contains(&ModelFeature::Attachment));
        assert!(zai_supports_reasoning_effort("glm-5.3"));
        assert!(zai_supports_reasoning_effort("glm-6"));
        assert!(!zai_supports_reasoning_effort("glm-5-turbo"));
    }

    #[test]
    fn zai_openapi_catalog_rejects_an_unrecognized_document() {
        let error = zai_models_from_openapi(&serde_json::json!({})).unwrap_err();
        assert!(error.to_string().contains("text-model enum"));
    }

    #[test]
    fn tencent_response_keeps_online_hunyuan_language_models_with_known_metadata() {
        let value = serde_json::json!({
            "data": [
                {"id": "deepseek-v4", "name": "DeepSeek V4", "status": "online"},
                {"id": "hy4", "name": "Future Hy4", "status": "online"},
                {"id": "hy3-preview", "name": "Hy3 Preview", "status": "online"},
                {"id": "hy3", "name": "Hy3", "status": "online"},
                {"id": "hy3", "name": "duplicate", "status": "online"},
                {"id": "hunyuan-translation", "status": "online"},
                {"id": "hunyuan-turbos-latest", "status": "online"},
                {"id": "hunyuan-vision", "status": "online"},
                {"id": "hy2", "status": "pre-offline"}
            ]
        });

        let models = tencent_models_from_response(&value).unwrap();
        let ids = models
            .iter()
            .map(|model| model.remote_model_id.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(ids, ["hy3", "hy3-preview", "hy4", "hunyuan-turbos-latest"]);
        assert_eq!(models[0].context_window, Some(256_000));
        assert_eq!(models[0].output_limit, Some(128_000));
        assert_eq!(models[0].reasoning_codec, Some(ReasoningCodec::Hunyuan));
        assert_eq!(
            models[0].supported_reasoning,
            vec![
                ReasoningSelection::Off,
                ReasoningSelection::Low,
                ReasoningSelection::High
            ]
        );
        assert_eq!(
            models[0].recommended_reasoning,
            Some(ReasoningSelection::High)
        );
        assert_eq!(
            models[0].features,
            vec![
                ModelFeature::ToolCall,
                ModelFeature::Reasoning,
                ModelFeature::StructuredOutput,
                ModelFeature::Temperature
            ]
        );
        assert_eq!(models[1].output_limit, Some(128_000));
        assert_eq!(models[2].context_window, None);
        assert!(models[2].features.is_empty());
        assert_eq!(models[2].token_counter, Some(TokenCounterKind::Heuristic));
    }

    #[tokio::test]
    async fn tencent_discovery_uses_tokenhub_models_endpoint_and_bearer_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer hunyuan-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"id": "hy3", "name": "Hy3", "status": "online"},
                    {"id": "hy3-video", "status": "online"}
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let availability = fetch_models_with_discovery(
            DiscoveryKind::Tencent,
            Protocol::OpenAiChat,
            "Tencent Hunyuan",
            &format!("{}/v1", server.uri()),
            "hunyuan-key",
            None,
        )
        .await
        .unwrap();

        assert_eq!(availability.remote_model_ids(), vec!["hy3".to_string()]);
        assert_eq!(availability.models[0].context_window, Some(256_000));
        assert_eq!(availability.models[0].output_limit, Some(128_000));
    }

    #[test]
    fn openrouter_response_filters_and_preserves_popularity_order() {
        let value = serde_json::json!({
            "data": [
                {
                    "id": "openai/gpt-5.6-sol",
                    "name": "OpenAI: GPT-5.6 Sol",
                    "context_length": 1_050_000,
                    "architecture": {
                        "input_modalities": ["text", "image"],
                        "output_modalities": ["text"],
                        "tokenizer": "GPT"
                    },
                    "supported_parameters": [
                        "tools",
                        "reasoning",
                        "structured_outputs",
                        "temperature"
                    ],
                    "top_provider": {
                        "context_length": 1_048_576,
                        "max_completion_tokens": 131_072
                    },
                    "reasoning": {
                        "supported_efforts": ["max", "xhigh", "high", "medium", "low", "minimal"],
                        "default_effort": "high",
                        "default_enabled": true,
                        "mandatory": false
                    },
                    "pricing": {
                        "prompt": "0.0000025",
                        "completion": "0.000015",
                        "input_cache_read": "0.00000025",
                        "overrides": [{
                            "min_prompt_tokens": 272000,
                            "prompt": "0.000005",
                            "completion": "0.0000225"
                        }]
                    }
                },
                {
                    "id": "anthropic/claude-sonnet-5",
                    "architecture": {
                        "output_modalities": ["text"],
                        "tokenizer": "Claude"
                    },
                    "supported_parameters": ["tools"],
                    "reasoning": {
                        "supported_efforts": null,
                        "default_effort": "medium",
                        "default_enabled": true,
                        "supports_max_tokens": true,
                        "mandatory": true
                    }
                },
                {
                    "id": "text-without-tools",
                    "architecture": {"output_modalities": ["text"]},
                    "supported_parameters": ["temperature"]
                },
                {
                    "id": "image-only-with-tools",
                    "architecture": {"output_modalities": ["image"]},
                    "supported_parameters": ["tools"]
                }
            ]
        });

        let models = openrouter_models_from_response(&value).unwrap();

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].remote_model_id.as_ref(), "openai/gpt-5.6-sol");
        assert_eq!(
            models[1].remote_model_id.as_ref(),
            "anthropic/claude-sonnet-5"
        );
        assert_eq!(models[0].context_window, Some(1_048_576));
        assert_eq!(models[0].output_limit, Some(131_072));
        assert_eq!(models[0].token_counter, Some(TokenCounterKind::Tiktoken));
        assert_eq!(models[0].reasoning_codec, Some(ReasoningCodec::OpenRouter));
        assert_eq!(
            models[0].recommended_reasoning,
            Some(ReasoningSelection::High)
        );
        assert!(models[0].features.contains(&ModelFeature::ToolCall));
        assert!(models[0].features.contains(&ModelFeature::Attachment));
        assert!(models[0].features.contains(&ModelFeature::Reasoning));
        assert!(models[0].features.contains(&ModelFeature::StructuredOutput));
        assert!(models[0].features.contains(&ModelFeature::Temperature));
        assert_eq!(models[0].pricing_tiers.len(), 1);
        assert_eq!(models[0].pricing_tiers[0].minimum_input_tokens, 272_001);
        assert_eq!(
            models[0].pricing_tiers[0]
                .pricing
                .cache_read_micros_per_million,
            Some(250_000),
            "missing override fields inherit the live base rate"
        );

        let claude = &models[1];
        assert_eq!(claude.token_counter, Some(TokenCounterKind::Heuristic));
        assert!(
            !claude
                .supported_reasoning
                .contains(&ReasoningSelection::Off),
            "mandatory reasoning cannot expose an off control"
        );
        assert!(
            claude
                .supported_reasoning
                .contains(&ReasoningSelection::BudgetTokens(4_096))
        );
        assert_eq!(
            claude.recommended_reasoning,
            Some(ReasoningSelection::Medium)
        );
    }

    #[tokio::test]
    async fn openrouter_discovery_uses_filtered_ranked_authenticated_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .and(header("authorization", "Bearer openrouter-key"))
            .and(query_param("output_modalities", "text"))
            .and(query_param("supported_parameters", "tools"))
            .and(query_param("sort", "most-popular"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{
                    "id": "moonshotai/kimi-k3",
                    "architecture": {
                        "output_modalities": ["text"],
                        "tokenizer": "Other"
                    },
                    "supported_parameters": ["tools"],
                    "top_provider": {
                        "context_length": 262_144,
                        "max_completion_tokens": 32_768
                    },
                    "pricing": {
                        "prompt": "0.000003",
                        "completion": "0.000015"
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let availability = fetch_models_with_discovery(
            DiscoveryKind::OpenRouter,
            Protocol::OpenAiChat,
            "OpenRouter",
            &format!("{}/api/v1", server.uri()),
            "openrouter-key",
            None,
        )
        .await
        .unwrap();

        assert_eq!(availability.models.len(), 1);
        assert_eq!(
            availability.models[0].remote_model_id.as_ref(),
            "moonshotai/kimi-k3"
        );
        assert_eq!(availability.models[0].context_window, Some(262_144));
        assert_eq!(availability.models[0].output_limit, Some(32_768));
        assert_eq!(
            availability.models[0].token_counter,
            Some(TokenCounterKind::Heuristic)
        );
    }

    #[test]
    fn mistral_native_response_keeps_current_tool_capable_chat_models_and_aliases() {
        let value = serde_json::json!({
            "object": "list",
            "data": [
                {
                    "id": "mistral-small-2603",
                    "aliases": ["mistral-small-latest"],
                    "archived": false,
                    "max_context_length": 256_000,
                    "capabilities": {
                        "completion_chat": true,
                        "function_calling": true,
                        "vision": true,
                        "reasoning": true
                    }
                },
                {
                    "id": "devstral-2512",
                    "aliases": ["devstral-latest"],
                    "archived": true,
                    "max_context_length": 262_144,
                    "capabilities": {
                        "completion_chat": true,
                        "function_calling": true
                    }
                },
                {
                    "id": "mistral-embed",
                    "archived": false,
                    "capabilities": {
                        "completion_chat": false,
                        "function_calling": false
                    }
                },
                {
                    "id": "chat-without-tools",
                    "archived": false,
                    "capabilities": {
                        "completion_chat": true,
                        "function_calling": false
                    }
                }
            ]
        });

        let models = mistral_models_from_response(&value).unwrap();
        let ids = models
            .iter()
            .map(|model| model.remote_model_id.as_ref())
            .collect::<Vec<_>>();

        assert_eq!(ids, ["mistral-small-2603", "mistral-small-latest"]);
        for model in models {
            assert_eq!(model.context_window, Some(256_000));
            assert_eq!(
                model.features,
                vec![
                    ModelFeature::ToolCall,
                    ModelFeature::Attachment,
                    ModelFeature::Reasoning
                ]
            );
        }
    }

    #[tokio::test]
    async fn mistral_native_discovery_uses_bearer_auth_and_rich_metadata() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer mistral-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{
                    "id": "mistral-medium-2604",
                    "aliases": ["mistral-medium-latest"],
                    "archived": false,
                    "max_context_length": 262_144,
                    "capabilities": {
                        "completion_chat": true,
                        "function_calling": true,
                        "vision": true,
                        "reasoning": true
                    }
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let availability = fetch_models_with_discovery(
            DiscoveryKind::Mistral,
            Protocol::OpenAiChat,
            "Mistral AI",
            &format!("{}/v1", server.uri()),
            "mistral-key",
            None,
        )
        .await
        .unwrap();

        let ids = availability
            .models
            .iter()
            .map(|model| model.remote_model_id.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["mistral-medium-2604", "mistral-medium-latest"]);
        assert_eq!(availability.models[0].context_window, Some(262_144));
    }

    #[test]
    fn gemini_native_response_keeps_only_generate_content_models() {
        let value = serde_json::json!({
            "models": [
                {
                    "name": "models/gemini-3.6-flash",
                    "displayName": "Gemini 3.6 Flash",
                    "inputTokenLimit": 1_048_576,
                    "outputTokenLimit": 65_536,
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/text-embedding-004",
                    "inputTokenLimit": 2_048,
                    "supportedGenerationMethods": ["embedContent"]
                },
                {
                    "name": "models/veo-3.1",
                    "supportedGenerationMethods": ["generateContent"]
                }
            ]
        });

        let models = gemini_models_from_response(&value).unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].remote_model_id.as_ref(), "gemini-3.6-flash");
        assert_eq!(models[0].display_name.as_deref(), Some("Gemini 3.6 Flash"));
        assert_eq!(models[0].context_window, Some(1_048_576));
        assert_eq!(models[0].output_limit, Some(65_536));
    }

    #[tokio::test]
    async fn gemini_native_discovery_is_authenticated_and_paginated() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1beta/models"))
            .and(header("x-goog-api-key", "gemini-key"))
            .and(query_param("pageSize", "1000"))
            .and(query_param_is_missing("pageToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{
                    "name": "models/gemini-3.6-flash",
                    "inputTokenLimit": 1_048_576,
                    "outputTokenLimit": 65_536,
                    "supportedGenerationMethods": ["generateContent"]
                }],
                "nextPageToken": "next-page"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1beta/models"))
            .and(header("x-goog-api-key", "gemini-key"))
            .and(query_param("pageSize", "1000"))
            .and(query_param("pageToken", "next-page"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{
                    "name": "models/gemini-3.5-flash-lite",
                    "inputTokenLimit": 1_048_576,
                    "outputTokenLimit": 65_536,
                    "supportedGenerationMethods": ["generateContent"]
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let availability = fetch_models_with_discovery(
            DiscoveryKind::Gemini,
            Protocol::OpenAiChat,
            "Google Gemini",
            &format!("{}/v1beta/openai", server.uri()),
            "gemini-key",
            None,
        )
        .await
        .unwrap();

        let ids = availability
            .models
            .iter()
            .map(|model| model.remote_model_id.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["gemini-3.5-flash-lite", "gemini-3.6-flash"]);
    }

    #[tokio::test]
    async fn gemini_discovery_falls_back_to_compatible_models_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1beta/models"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1beta/openai/models"))
            .and(header("authorization", "Bearer gemini-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{"id": "gemini-3.6-flash", "object": "model"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let availability = fetch_models_with_discovery(
            DiscoveryKind::Gemini,
            Protocol::OpenAiChat,
            "Google Gemini",
            &format!("{}/v1beta/openai", server.uri()),
            "gemini-key",
            None,
        )
        .await
        .unwrap();

        assert_eq!(availability.models.len(), 1);
        assert_eq!(
            availability.models[0].remote_model_id.as_ref(),
            "gemini-3.6-flash"
        );
    }

    #[test]
    fn lm_studio_v1_response_parses_metadata_and_filters_embeddings() {
        // Shape validated live against LM Studio 0.3.x /api/v1/models.
        let value = serde_json::json!({
            "models": [
                {
                    "type": "embedding",
                    "key": "text-embedding-nomic-embed-text-v1.5",
                    "display_name": "Nomic Embed Text v1.5",
                    "max_context_length": 2048
                },
                {
                    "type": "llm",
                    "key": "qwen3-coder-30b",
                    "display_name": "Qwen3 Coder 30B",
                    "max_context_length": 262_144,
                    "capabilities": ["tool_use"]
                },
                {
                    "type": "vlm",
                    "key": "qwen2-vl-7b",
                    "display_name": "Qwen2 VL 7B",
                    "max_context_length": 32_768
                }
            ]
        });

        let models = lm_studio_models_from_response(&value).unwrap();

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].remote_model_id.as_ref(), "qwen3-coder-30b");
        assert_eq!(models[0].context_window, Some(262_144));
        assert_eq!(models[0].display_name.as_deref(), Some("Qwen3 Coder 30B"));
        assert_eq!(models[0].features, vec![ModelFeature::ToolCall]);
        assert_eq!(models[1].features, vec![ModelFeature::Attachment]);
    }

    #[test]
    fn lm_studio_v0_response_parses_data_items() {
        let value = serde_json::json!({
            "object": "list",
            "data": [
                {
                    "id": "granite-3.0-2b",
                    "object": "model",
                    "type": "llm",
                    "state": "not-loaded",
                    "max_context_length": 131_072
                },
                {
                    "id": "text-embedding-nomic",
                    "type": "embeddings",
                    "max_context_length": 2048
                }
            ]
        });

        let models = lm_studio_models_from_response(&value).unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].remote_model_id.as_ref(), "granite-3.0-2b");
        assert_eq!(models[0].context_window, Some(131_072));
        assert_eq!(models[0].display_name, None);
    }

    #[test]
    fn ollama_show_response_maps_context_and_capabilities() {
        let value = serde_json::json!({
            "details": {"family": "qwen3"},
            "model_info": {
                "general.architecture": "qwen3",
                "qwen3.context_length": 40_960,
                "qwen3.embedding_length": 5120
            },
            "capabilities": ["completion", "tools", "thinking"]
        });

        let model = ollama_model_from_show("qwen3:latest", &value);

        assert_eq!(model.remote_model_id.as_ref(), "qwen3:latest");
        assert_eq!(model.context_window, Some(40_960));
        assert_eq!(
            model.features,
            vec![ModelFeature::ToolCall, ModelFeature::Reasoning]
        );
    }

    #[test]
    fn server_root_strips_openai_compat_suffix() {
        assert_eq!(
            server_root("http://localhost:1234/v1"),
            "http://localhost:1234"
        );
        assert_eq!(
            server_root("http://localhost:1234/v1/"),
            "http://localhost:1234"
        );
        assert_eq!(
            server_root("http://localhost:11434"),
            "http://localhost:11434"
        );
    }

    #[test]
    fn display_name_matching_id_is_dropped() {
        let value = serde_json::json!({
            "models": [{"type": "llm", "key": "same-id", "display_name": "same-id"}]
        });

        let models = lm_studio_models_from_response(&value).unwrap();

        assert_eq!(models[0].display_name, None);
        assert_eq!(models[0].context_window, None);
    }
}
