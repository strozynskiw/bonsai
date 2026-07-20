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
use crate::provider::openai_catalog::available_models_from_response;
use crate::provider::{Protocol, ProviderMetadata, sse};

/// Per-request timeout for native discovery probes. Local servers answer in
/// milliseconds; a probe that takes longer is down or wedged, and the caller
/// falls back to the generic listing (or its own fallback ladder) instead of
/// hanging the picker.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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
    if metadata.discovery == DiscoveryKind::Static {
        return Ok(LiveModelAvailability::from_remote_ids(
            metadata.seed_model_list(),
        ));
    }
    fetch_models_with_discovery(
        metadata.discovery,
        metadata.protocol,
        &metadata.display_name,
        base_url,
        api_key,
        metadata.auth_header.as_deref(),
    )
    .await
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
        DiscoveryKind::LmStudio => fetch_lm_studio_models(base_url, api_key).await,
        DiscoveryKind::Ollama => fetch_ollama_models(base_url).await,
    };
    match native {
        Ok(availability) => Ok(availability),
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
