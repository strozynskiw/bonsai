use anyhow::{Context, Result};
use serde_json::Value;

use crate::model_catalog::{AvailableModel, ModelFeature};
use crate::provider::{ModelPricing, ModelPricingTier, TokenCounterKind};

#[cfg(test)]
pub(crate) fn model_ids_from_response(value: &Value) -> Vec<String> {
    available_models_from_response(value)
        .into_iter()
        .map(|model| model.remote_model_id.to_string())
        .collect()
}

pub(crate) fn available_models_from_response(value: &Value) -> Vec<AvailableModel> {
    value
        .get("data")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(available_model_from_item).collect())
        .unwrap_or_default()
}

pub(crate) fn available_model_from_item(item: &Value) -> Option<AvailableModel> {
    let id = item.get("id").and_then(Value::as_str)?;
    // Google's OpenAI-compat layer lists ids under the native resource name
    // (`models/gemini-2.5-flash`) while its chat endpoint accepts the bare id.
    let id = id.strip_prefix("models/").unwrap_or(id);
    // A `/models` listing routinely mixes chat models with embedding, media,
    // translation, and roleplay products that Bonsai cannot use for coding.
    if !is_coding_model_id(id) {
        return None;
    }
    let (pricing, pricing_tiers) = pricing_schedule_from_model_item(item);
    Some(
        AvailableModel::with_metadata(
            id,
            context_window_from_model_item(item),
            display_name_from_model_item(item, id),
            features_from_model_item(item),
        )
        .with_token_counter(token_counter_from_model_item(item, id))
        .with_pricing_schedule(pricing, pricing_tiers),
    )
}

/// Whether a `/models` id names a model bonsai can select for coding — a
/// general-purpose or code-specialized text LLM. Returns `false` for two
/// families, each identified by an unambiguous token appearing as a whole
/// `-`/`.`/`_`-delimited segment of the id:
///
/// 1. **Other modalities** — image, video, speech (TTS/ASR), embedding, rerank,
///    OCR: bonsai cannot drive them at all.
/// 2. **Non-coding text roles** — translation, roleplay/character, captioning,
///    contact-center, meeting transcription: they emit text but are not coding
///    models, so listing them is noise in a coding-agent picker.
///
/// Deliberately conservative on both counts. It never drops a *general* LLM
/// (`qwen3.7-plus`, `qwen3-max`) — those are the coding models people actually
/// use — and it keeps vision-language *chat* models (`qwen3-vl-plus`, which
/// still emit text and tool calls). Segment-exact, not substring, so a token
/// like `mt` can't clip an unrelated id. Unknown ids default to *keep*: a stray
/// entry is cosmetic, whereas hiding a real coding model is a functional loss.
pub(crate) fn is_coding_model_id(id: &str) -> bool {
    // Non-text output modalities — bonsai can't drive these.
    const NON_TEXT_SEGMENTS: &[&str] = &[
        "image",
        "video",
        "audio",
        "voice",
        "tts",
        "asr",
        "ocr",
        "embedding",
        "embeddings",
        "embed",
        "rerank",
        "reranker",
        "realtime",
        "t2v",
        "i2v",
        "r2v",
        "t2i",
        "i2i",
        "s2s",
        "vae",
        "livetranslate",
        // Google generative-media brand names (Gemini's /models lists them
        // alongside chat models): video, image, and music generation.
        "veo",
        "imagen",
        "lyria",
        "banana", // nano-banana-* — Gemini image-generation codename
    ];
    // Text models built for a specific non-coding job. These are safe to name
    // exactly because they are dedicated single-purpose lines; a general model
    // that merely *can* translate or roleplay is not tagged with these tokens.
    const NON_CODING_TEXT_SEGMENTS: &[&str] = &[
        "mt",          // machine translation (qwen-mt-*)
        "translate",   // translation lines
        "translation", //
        "character",   // roleplay personas (qwen-*-character)
        "roleplay",    //
        "captioner",   // media captioning (qwen3-omni-*-captioner)
        "caption",     //
        "ccai",        // contact-center AI (customer-service, not coding)
        "tingwu",      // meeting transcription/summary (tongyi-tingwu-*)
        "transcribe",  //
        "transcription",
        "aqa",      // Google Attributed Question Answering
        "research", // deep-research-* agent models (research, not coding)
        "robotics", // gemini-robotics-er-* embodied reasoning
        "computer", // *-computer-use-* screen-control agents
    ];
    let lower = id.to_ascii_lowercase();
    !lower.split(['-', '.', '_', '/']).any(|segment| {
        NON_TEXT_SEGMENTS.contains(&segment) || NON_CODING_TEXT_SEGMENTS.contains(&segment)
    })
}

/// Best-effort display name from the non-standard fields some OpenAI-compatible
/// hosts (OpenRouter, Together) include; a name equal to the id adds nothing.
fn display_name_from_model_item(item: &Value, id: &str) -> Option<String> {
    ["display_name", "name"]
        .into_iter()
        .find_map(|field| item.get(field).and_then(Value::as_str))
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != id)
        .map(str::to_string)
}

/// Best-effort capability sniff: OpenRouter-style `supported_parameters`
/// listing `tools` marks tool-call support, and `architecture.input_modalities`
/// (or a top-level `input_modalities`) containing `image` marks vision. Absence
/// means "unreported", so no feature is recorded rather than an explicit
/// unsupported.
fn features_from_model_item(item: &Value) -> Vec<ModelFeature> {
    let mut features = Vec::new();
    let parameters = item.get("supported_parameters").and_then(Value::as_array);
    let supports = |name: &str| {
        parameters.is_some_and(|parameters| {
            parameters
                .iter()
                .filter_map(Value::as_str)
                .any(|value| value == name)
        })
    };
    if supports("tools") {
        features.push(ModelFeature::ToolCall);
    }
    if supports("reasoning") || supports("include_reasoning") {
        features.push(ModelFeature::Reasoning);
    }
    if supports("structured_outputs") || supports("response_format") {
        features.push(ModelFeature::StructuredOutput);
    }
    if supports("temperature") {
        features.push(ModelFeature::Temperature);
    }
    if input_modalities_include_image(item) {
        features.push(ModelFeature::Attachment);
    }
    features
}

/// Whether a listing item advertises image input, via OpenRouter's
/// `architecture.input_modalities` or a flat `input_modalities` array.
fn input_modalities_include_image(item: &Value) -> bool {
    ["architecture", "input_modalities"]
        .iter()
        .filter_map(|field| item.get(field))
        // `architecture` nests the array; a flat field is the array itself.
        .filter_map(|value| value.get("input_modalities").or(Some(value)))
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(Value::as_str)
        .any(|modality| modality == "image")
}

/// Parse the per-token pricing an aggregator gateway publishes in its listing
/// (OpenRouter: `pricing.prompt` / `.completion` as USD-per-token strings). A
/// `"0"` price is a real free tier (priced at $0), not a missing price, so it
/// yields `Some`. `None` only when the pricing object or the two required rates
/// are absent or unparseable.
fn pricing_from_value(pricing: &Value) -> Option<ModelPricing> {
    let input = usd_per_token_micros_per_million(pricing.get("prompt"))?;
    let output = usd_per_token_micros_per_million(pricing.get("completion"))?;
    let cache_read = usd_per_token_micros_per_million(pricing.get("input_cache_read"));
    let cache_write = usd_per_token_micros_per_million(pricing.get("input_cache_write"));
    Some(ModelPricing::new(input, output).with_cache_rates(cache_read, cache_write))
}

fn pricing_override_from_value(pricing: &Value, base: ModelPricing) -> ModelPricing {
    let input = usd_per_token_micros_per_million(pricing.get("prompt"))
        .unwrap_or(base.input_micros_per_million);
    let output = usd_per_token_micros_per_million(pricing.get("completion"))
        .unwrap_or(base.output_micros_per_million);
    let cache_read = if pricing.get("input_cache_read").is_some() {
        usd_per_token_micros_per_million(pricing.get("input_cache_read"))
    } else {
        base.cache_read_micros_per_million
    };
    let cache_write = if pricing.get("input_cache_write").is_some() {
        usd_per_token_micros_per_million(pricing.get("input_cache_write"))
    } else {
        base.cache_write_micros_per_million
    };
    ModelPricing::new(input, output).with_cache_rates(cache_read, cache_write)
}

fn pricing_schedule_from_model_item(item: &Value) -> (Option<ModelPricing>, Vec<ModelPricingTier>) {
    let Some(pricing) = item.get("pricing") else {
        return (None, Vec::new());
    };
    let base = pricing_from_value(pricing);
    let Some(base_pricing) = base else {
        return (None, Vec::new());
    };
    let tiers = pricing
        .get("overrides")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|override_value| {
            // OpenRouter defines `min_prompt_tokens` as a strict greater-than
            // boundary, while `ModelPricingTier` is inclusive.
            let threshold = override_value
                .get("min_prompt_tokens")
                .and_then(value_as_positive_u32)?;
            let minimum_input_tokens = threshold.checked_add(1)?;
            let pricing = pricing_override_from_value(override_value, base_pricing);
            Some(ModelPricingTier {
                minimum_input_tokens,
                pricing,
            })
        })
        .collect();
    (Some(base_pricing), tiers)
}

fn token_counter_from_model_item(item: &Value, id: &str) -> Option<TokenCounterKind> {
    let tokenizer = item
        .get("architecture")
        .and_then(|architecture| architecture.get("tokenizer"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let id = id.to_ascii_lowercase();
    if tokenizer == "gpt" || id.starts_with("openai/") {
        Some(TokenCounterKind::Tiktoken)
    } else if tokenizer.contains("qwen3") || id.contains("qwen3") {
        Some(TokenCounterKind::Qwen3)
    } else {
        None
    }
}

/// Convert a USD-per-token rate (number or numeric string) into micro-USD per
/// million tokens, the catalog's pricing unit. Negative/non-finite → `None`.
fn usd_per_token_micros_per_million(value: Option<&Value>) -> Option<u64> {
    let raw = value?;
    let usd_per_token = raw
        .as_f64()
        .or_else(|| raw.as_str()?.trim().parse::<f64>().ok())?;
    if !usd_per_token.is_finite() || usd_per_token < 0.0 {
        return None;
    }
    // per-million (×1e6) then micro-USD (×1e6).
    Some((usd_per_token * 1_000_000.0 * 1_000_000.0).round() as u64)
}

fn context_window_from_model_item(item: &Value) -> Option<u32> {
    [
        "context_window",
        "context_length",
        "max_context_length",
        "max_context_tokens",
    ]
    .into_iter()
    .find_map(|field| item.get(field).and_then(value_as_positive_u32))
    .or_else(|| {
        item.get("limit")
            .and_then(|limit| limit.get("context"))
            .and_then(value_as_positive_u32)
    })
    .or_else(|| {
        item.get("limits")
            .and_then(|limits| limits.get("context"))
            .and_then(value_as_positive_u32)
    })
}

fn value_as_positive_u32(value: &Value) -> Option<u32> {
    let parsed = value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .or_else(|| value.as_str()?.trim().parse::<u32>().ok())?;
    (parsed > 0).then_some(parsed)
}

/// Prepend a default URL scheme to a scheme-less authority: `http://` only for
/// loopback hosts (local dev), `https://` otherwise — so a typo like
/// `api.vendor.com` can't send the API key over plaintext.
pub(crate) fn with_default_scheme(value: &str) -> String {
    if value.contains("://") {
        return value.to_string();
    }
    let authority = value.split('/').next().unwrap_or(value);
    let host = authority
        .rsplit_once(':')
        .map(|(host, _port)| host)
        .unwrap_or(authority);
    if matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]") {
        format!("http://{value}")
    } else {
        format!("https://{value}")
    }
}

pub(crate) fn normalize_openai_base_url(input: &str) -> Result<String> {
    let mut value = input.trim().trim_end_matches('/').to_string();
    if value.is_empty() {
        anyhow::bail!("OpenAI-compatible base URL cannot be empty");
    }
    value = with_default_scheme(&value);

    let lower = value.to_ascii_lowercase();
    for suffix in ["/chat/completions", "/models"] {
        if lower.ends_with(suffix) {
            let len = value.len().saturating_sub(suffix.len());
            value.truncate(len);
            value = value.trim_end_matches('/').to_string();
            break;
        }
    }

    let parsed = reqwest::Url::parse(&value)
        .with_context(|| format!("Invalid OpenAI-compatible base URL '{value}'"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        anyhow::bail!("OpenAI-compatible base URL must use http or https");
    }
    if parsed.host_str().is_none() {
        anyhow::bail!("OpenAI-compatible base URL must include a host");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_parse_from_openai_data_response() {
        let models = model_ids_from_response(
            &serde_json::json!({"data": [{"id": "live"}, {"missing": true}]}),
        );

        assert_eq!(models, vec!["live"]);
    }

    #[test]
    fn coding_classifier_keeps_coding_models_and_drops_the_rest() {
        // General-purpose + code-specialized chat ids across vendors — all kept.
        for id in [
            "qwen3.7-plus",
            "qwen3-coder-plus",
            "qwen3-max",
            "gemini-3.1-pro-preview",
            "grok-build-0.1",
            "deepseek-v4-flash",
            "glm-5.2",
            // Vision-language *chat* models still emit text/tools → kept.
            "qwen3-vl-plus",
            "qwen-vl-max",
        ] {
            assert!(is_coding_model_id(id), "should keep {id}");
        }

        // Other modalities from Qwen Cloud's /models listing — all dropped.
        for id in [
            "qwen-image-2.0-pro",
            "wan2.7-image",
            "z-image-turbo",
            "happyhorse-1.1-t2v",
            "happyhorse-1.0-video-edit",
            "qwen3-tts-flash",
            "fun-asr-realtime",
            "qwen-audio-3.0-realtime-plus",
            "qwen3-s2s-flash-realtime",
            "qwen3-livetranslate-flash",
            "qwen-vl-ocr-2025-11-20",
            "text-embedding-v4",
            "tongyi-embedding-vision-plus",
            "qwen3-rerank",
        ] {
            assert!(!is_coding_model_id(id), "should drop {id}");
        }

        // Text models built for a non-coding job — also dropped.
        for id in [
            "qwen-mt-plus",
            "qwen-flash-character",
            "qwen-plus-character",
            "qwen3-omni-30b-a3b-captioner",
            "tongyi-tingwu-slp",
            "ccai-pro",
        ] {
            assert!(!is_coding_model_id(id), "should drop non-coding {id}");
        }

        // Gemini's /models mixes in generative media and non-coding agent
        // lines — all dropped.
        for id in [
            "veo-3.1-lite-generate-preview",
            "imagen-4.0-generate-001",
            "lyria-3-pro-preview",
            "nano-banana-pro-preview",
            "aqa",
            "deep-research-max-preview-04-2026",
            "gemini-robotics-er-1.6-preview",
            "gemini-2.5-computer-use-preview-10-2025",
        ] {
            assert!(!is_coding_model_id(id), "should drop non-coding {id}");
        }
    }

    #[test]
    fn available_models_filters_to_coding_models() {
        let models = available_models_from_response(&serde_json::json!({
            "data": [
                {"id": "qwen3.7-plus"},
                {"id": "qwen-image-2.0-pro"},
                {"id": "qwen3-tts-flash"},
                {"id": "text-embedding-v4"},
                {"id": "qwen-mt-turbo"},
                {"id": "qwen3-coder-plus"}
            ]
        }));

        let ids: Vec<_> = models
            .iter()
            .map(|model| model.remote_model_id.to_string())
            .collect();
        assert_eq!(ids, vec!["qwen3.7-plus", "qwen3-coder-plus"]);
    }

    #[test]
    fn available_models_strip_google_resource_prefix() {
        // Gemini's OpenAI-compat `/models` returns native resource names;
        // targets store bare ids, so the prefix must go at parse time.
        let models = available_models_from_response(&serde_json::json!({
            "data": [
                {"id": "models/gemini-3.1-flash-lite"},
                {"id": "gemini-2.5-pro"}
            ]
        }));

        let ids: Vec<_> = models
            .iter()
            .map(|model| model.remote_model_id.to_string())
            .collect();
        assert_eq!(ids, vec!["gemini-3.1-flash-lite", "gemini-2.5-pro"]);
    }

    #[test]
    fn available_models_parse_context_windows_from_common_fields() {
        let models = available_models_from_response(&serde_json::json!({
            "data": [
                {"id": "ctx-window", "context_window": 32768},
                {"id": "ctx-length", "context_length": "65536"},
                {"id": "nested-limit", "limit": {"context": 131072}},
                {"id": "none"}
            ]
        }));

        assert_eq!(models[0].remote_model_id.as_ref(), "ctx-window");
        assert_eq!(models[0].context_window, Some(32_768));
        assert_eq!(models[1].context_window, Some(65_536));
        assert_eq!(models[2].context_window, Some(131_072));
        assert_eq!(models[3].context_window, None);
    }

    #[test]
    fn openrouter_pricing_and_image_modality_parse_from_listing() {
        // Shape validated live against OpenRouter /api/v1/models (kimi-k3 row).
        let models = available_models_from_response(&serde_json::json!({
            "data": [{
                "id": "moonshotai/kimi-k3",
                "context_length": 262_144,
                "supported_parameters": [
                    "tools",
                    "reasoning",
                    "structured_outputs",
                    "temperature"
                ],
                "architecture": { "input_modalities": ["text", "image"], "output_modalities": ["text"] },
                "pricing": {
                    "prompt": "0.000003",
                    "completion": "0.000015",
                    "input_cache_read": "0.0000003",
                    "input_cache_write": "0.00000375",
                    "overrides": [{
                        "min_prompt_tokens": 250000,
                        "prompt": "0.000006",
                        "completion": "0.0000225"
                    }]
                }
            }]
        }));

        assert_eq!(models.len(), 1);
        let pricing = models[0].pricing.expect("gateway pricing parses");
        // 0.000003 USD/token → 3 USD/M → 3_000_000 micro-USD/M.
        assert_eq!(pricing.input_micros_per_million, 3_000_000);
        assert_eq!(pricing.output_micros_per_million, 15_000_000);
        assert_eq!(pricing.cache_read_micros_per_million, Some(300_000));
        assert!(models[0].features.contains(&ModelFeature::ToolCall));
        assert!(models[0].features.contains(&ModelFeature::Attachment));
        assert!(models[0].features.contains(&ModelFeature::Reasoning));
        assert!(models[0].features.contains(&ModelFeature::StructuredOutput));
        assert!(models[0].features.contains(&ModelFeature::Temperature));
        assert_eq!(models[0].pricing_tiers.len(), 1);
        let tier = models[0].pricing_tiers[0];
        assert_eq!(tier.minimum_input_tokens, 250_001);
        assert_eq!(tier.pricing.input_micros_per_million, 6_000_000);
        assert_eq!(tier.pricing.output_micros_per_million, 22_500_000);
        assert_eq!(
            tier.pricing.cache_read_micros_per_million,
            Some(300_000),
            "unspecified override rates inherit from the live base"
        );
        assert_eq!(tier.pricing.cache_write_micros_per_million, Some(3_750_000));
    }

    #[test]
    fn openrouter_tokenizer_metadata_selects_safe_local_counters() {
        let models = available_models_from_response(&serde_json::json!({
            "data": [
                {
                    "id": "openai/gpt-5.6-sol",
                    "architecture": {"tokenizer": "GPT"}
                },
                {
                    "id": "qwen/qwen3-coder-next",
                    "architecture": {"tokenizer": "Qwen3"}
                },
                {
                    "id": "anthropic/claude-sonnet-5",
                    "architecture": {"tokenizer": "Claude"}
                }
            ]
        }));

        assert_eq!(models[0].token_counter, Some(TokenCounterKind::Tiktoken));
        assert_eq!(models[1].token_counter, Some(TokenCounterKind::Qwen3));
        assert_eq!(models[2].token_counter, None);
    }

    #[test]
    fn free_gateway_model_is_priced_at_zero_not_unpriced() {
        // OpenRouter free tiers list "0" prices — a real $0, not a missing price.
        let models = available_models_from_response(&serde_json::json!({
            "data": [{
                "id": "poolside/laguna-m.1:free",
                "pricing": { "prompt": "0", "completion": "0" }
            }]
        }));

        let pricing = models[0]
            .pricing
            .expect("free model still carries $0 pricing");
        assert_eq!(pricing.input_micros_per_million, 0);
        assert_eq!(pricing.output_micros_per_million, 0);
    }

    #[test]
    fn missing_pricing_object_leaves_price_none() {
        let models = available_models_from_response(
            &serde_json::json!({ "data": [{ "id": "bare-model" }] }),
        );
        assert_eq!(models[0].pricing, None);
    }

    #[test]
    fn normalize_base_url_accepts_endpoint_urls() {
        assert_eq!(
            normalize_openai_base_url("localhost:11434/v1/chat/completions").unwrap(),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalize_openai_base_url("https://example.test/v1/models/").unwrap(),
            "https://example.test/v1"
        );
    }

    #[test]
    fn scheme_less_host_defaults_to_https_except_loopback() {
        // A scheme-less remote host must not downgrade to plaintext http.
        assert_eq!(
            normalize_openai_base_url("api.vendor.com/v1").unwrap(),
            "https://api.vendor.com/v1"
        );
        assert_eq!(
            with_default_scheme("api.vendor.com:8443"),
            "https://api.vendor.com:8443"
        );
        // Loopback keeps http for local dev.
        assert_eq!(
            with_default_scheme("localhost:11434"),
            "http://localhost:11434"
        );
        assert_eq!(
            with_default_scheme("127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
        // Explicit scheme is preserved.
        assert_eq!(
            with_default_scheme("http://example.test"),
            "http://example.test"
        );
    }
}
