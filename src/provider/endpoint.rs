//! Shared endpoint-setup logic for the protocol-keyed "compatible" providers.
//!
//! `openai-compatible`, `anthropic-compatible`, and every catalog connection
//! on the `openai-chat` / `anthropic-messages` transports discover models,
//! authorize against a base URL, and normalize that URL with the same shape —
//! only the wire details (endpoint path, auth header, error label) differ per
//! [`Protocol`]. This module owns the single copy of that logic, dispatched on
//! the protocol, so the per-provider files stay thin.

use anyhow::{Context, Result};

use crate::model_catalog::{DiscoveryKind, LiveModelAvailability};
use crate::provider::discovery::fetch_models_for_metadata;
use crate::provider::openai_catalog::{normalize_openai_base_url, with_default_scheme};
use crate::provider::{AuthorizeOutcome, Protocol, ProviderMetadata};
use crate::session::ProviderSession;

/// Human-facing label used in endpoint error messages (e.g. "OpenAI-compatible
/// endpoint returned no models"). Distinct from `metadata.display_name`.
fn endpoint_label(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiChat => "OpenAI-compatible",
        Protocol::AnthropicMessages => "Anthropic-compatible",
        Protocol::CodexResponses => "Codex",
    }
}

// ---------------------------------------------------------------------------
// Base-URL normalization
// ---------------------------------------------------------------------------

/// Normalize an endpoint base URL for the given protocol. Dispatches to the
/// protocol-specific normalizer; the two named shims below are the public
/// entry points the TUI wizard and the factories use.
fn normalize_base_url(protocol: Protocol, input: &str) -> Result<String> {
    match protocol {
        Protocol::OpenAiChat => normalize_openai_compatible_base_url(input),
        Protocol::AnthropicMessages => normalize_anthropic_compatible_base_url(input),
        Protocol::CodexResponses => {
            anyhow::bail!("Codex does not support endpoint base URL normalization")
        }
    }
}

/// Normalize an OpenAI-compatible base URL (strips `/chat/completions`,
/// `/models`). Thin wrapper over the low-level catalog helper so the dispatcher
/// and the TUI wizard share one implementation.
pub(crate) fn normalize_openai_compatible_base_url(input: &str) -> Result<String> {
    normalize_openai_base_url(input)
}

/// Normalize an Anthropic-compatible base URL (strips `/v1/messages`,
/// `/messages`, `/v1/models`, `/models`, `/v1`). Validates scheme + host and
/// defaults a scheme-less host to `https://` (loopback to `http://`).
pub(crate) fn normalize_anthropic_compatible_base_url(input: &str) -> Result<String> {
    let value = input.trim();
    if value.is_empty() {
        anyhow::bail!("Anthropic-compatible base URL cannot be empty");
    }
    let value = with_default_scheme(value);
    let url = reqwest::Url::parse(&value)
        .with_context(|| format!("Invalid Anthropic-compatible base URL '{value}'"))?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("Anthropic-compatible base URL must use http or https");
    }
    if url.host_str().is_none() {
        anyhow::bail!("Anthropic-compatible base URL must include a host");
    }

    let mut normalized = value.trim_end_matches('/').to_string();
    for suffix in ["/v1/messages", "/messages", "/v1/models", "/models", "/v1"] {
        if let Some(stripped) = normalized.strip_suffix(suffix) {
            normalized = stripped.trim_end_matches('/').to_string();
            break;
        }
    }
    Ok(normalized)
}

// ---------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------

/// Authorize an endpoint-style provider: normalize the base URL, then resolve a
/// model — using the caller's model if supplied, otherwise the first model the
/// catalog returns. Used by both the `anthropic-compatible` /
/// `openai-compatible` factories and the catalog-backed registry factory.
pub(crate) async fn authorize_endpoint(
    metadata: &ProviderMetadata,
    base_url: &str,
    api_key: Option<&str>,
    model: Option<&str>,
) -> Result<AuthorizeOutcome> {
    let protocol = metadata.protocol;
    if protocol == Protocol::CodexResponses {
        anyhow::bail!("{} does not support endpoint auth", metadata.display_name);
    }
    let label = endpoint_label(protocol);
    let base_url = normalize_base_url(protocol, base_url)?;
    let api_key = api_key.unwrap_or_default().trim().to_string();
    let model = match model.and_then(trimmed_non_empty) {
        Some(model) => model.to_string(),
        None => match fetch_models_for_metadata(metadata, &base_url, &api_key).await {
            Ok(availability) if !availability.models.is_empty() => {
                default_model_pick(&availability.remote_model_ids())
            }
            Ok(_) => {
                anyhow::bail!("{label} endpoint returned no models. Enter a model id manually.")
            }
            Err(err) => {
                anyhow::bail!(
                    "Failed to fetch {label} models from {base_url}: {err:#}. Enter a model id manually."
                )
            }
        },
    };

    Ok(AuthorizeOutcome {
        clear_existing_api_key: api_key.is_empty(),
        api_key,
        base_url: Some(base_url),
        model: Some(model),
        account_id: String::new(),
        is_fedramp: false,
    })
}

/// Pick a sensible default model from a catalog list. The catalog is sorted
/// alphabetically, which for a known family (e.g. Anthropic) would otherwise
/// auto-pick the cheapest/least-capable entry (`haiku` < `opus` < `sonnet`).
/// Prefer the first entry that is neither a `haiku` nor a `mini` variant,
/// falling back to `models[0]` when every entry looks lightweight. Deterministic
/// and order-stable.
fn default_model_pick(models: &[String]) -> String {
    models
        .iter()
        .find(|model| {
            let lower = model.to_ascii_lowercase();
            !lower.contains("haiku") && !lower.contains("mini")
        })
        .unwrap_or(&models[0])
        .clone()
}

// ---------------------------------------------------------------------------
// Live model availability
// ---------------------------------------------------------------------------

/// Resolve the live model list for an endpoint provider, falling back through
/// the configured session model and (per protocol) a seed list. The empty-list
/// and hard-error fallbacks are intentionally asymmetric between protocols —
/// see [`empty_success_fallback`] and the `seed`-on-error arm below.
pub(crate) async fn list_available_models_for_metadata(
    metadata: &ProviderMetadata,
    session: &ProviderSession,
) -> Result<LiveModelAvailability> {
    let protocol = metadata.protocol;
    let configured_base_url = if session.base_url.trim().is_empty() {
        metadata.default_base_url.as_ref()
    } else {
        session.base_url.trim()
    };
    let base_url = normalize_base_url(protocol, configured_base_url)?;
    let configured_model = session.model.trim();

    match fetch_models_for_metadata(metadata, &base_url, &session.api_key).await {
        Ok(availability) if !availability.models.is_empty() => Ok(availability),
        Ok(_) if !configured_model.is_empty() => Ok(LiveModelAvailability::from_remote_ids([
            configured_model.to_string(),
        ])),
        Ok(_) => Ok(empty_success_fallback(metadata)),
        Err(err) if !configured_model.is_empty() => {
            tracing::warn!(
                provider = %metadata.id,
                error = %err,
                "failed to fetch endpoint models; using configured model"
            );
            Ok(LiveModelAvailability::from_remote_ids([
                configured_model.to_string()
            ]))
        }
        // Anthropic-style providers fall back to their seed list on a hard
        // fetch error when one is configured (e.g. minimax-coding-plan); the
        // OpenAI path has no seed list and surfaces the error instead.
        Err(err)
            if (protocol == Protocol::AnthropicMessages
                || metadata.discovery == DiscoveryKind::QwenCloud)
                && !metadata.seed_models.is_empty() =>
        {
            tracing::warn!(
                provider = %metadata.id,
                error = %err,
                "failed to fetch endpoint models; using seed models"
            );
            Ok(LiveModelAvailability::from_remote_ids(
                metadata.seed_model_list(),
            ))
        }
        Err(err) => Err(err),
    }
}

/// Perform the endpoint request without any configured-model or seed fallback.
pub(crate) async fn probe_reachability_for_metadata(
    metadata: &ProviderMetadata,
    session: &ProviderSession,
) -> Result<()> {
    let configured_base_url = if session.base_url.trim().is_empty() {
        metadata.default_base_url.as_ref()
    } else {
        session.base_url.trim()
    };
    let base_url = normalize_base_url(metadata.protocol, configured_base_url)?;
    fetch_models_for_metadata(metadata, &base_url, &session.api_key)
        .await
        .map(|_| ())
}

/// Fallback when the catalog fetch succeeds but returns no models and no session
/// model is configured. Anthropic providers seed from their built-in list;
/// OpenAI providers report no availability (the user enters a model manually).
fn empty_success_fallback(metadata: &ProviderMetadata) -> LiveModelAvailability {
    match metadata.protocol {
        Protocol::AnthropicMessages => {
            LiveModelAvailability::from_remote_ids(metadata.seed_model_list())
        }
        Protocol::OpenAiChat if metadata.discovery == DiscoveryKind::QwenCloud => {
            LiveModelAvailability::from_remote_ids(metadata.seed_model_list())
        }
        _ => LiveModelAvailability::default(),
    }
}

fn trimmed_non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use super::*;
    use crate::provider::test_utils::provider_session;

    #[test]
    fn normalize_openai_base_url_accepts_localhost_and_endpoint_urls() {
        assert_eq!(
            normalize_openai_compatible_base_url("localhost:11434/v1").unwrap(),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalize_openai_compatible_base_url("http://localhost:11434/v1/chat/completions")
                .unwrap(),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            normalize_openai_compatible_base_url("https://example.test/v1/models/").unwrap(),
            "https://example.test/v1"
        );
    }

    #[test]
    fn normalize_anthropic_base_url_strips_messages_suffixes() {
        assert_eq!(
            normalize_anthropic_compatible_base_url("localhost:8080").unwrap(),
            "http://localhost:8080"
        );
        assert_eq!(
            normalize_anthropic_compatible_base_url("https://example.test/v1/messages").unwrap(),
            "https://example.test"
        );
        assert_eq!(
            normalize_anthropic_compatible_base_url("https://example.test/v1/").unwrap(),
            "https://example.test"
        );
    }

    #[test]
    fn normalize_anthropic_base_url_rejects_empty_and_hostless() {
        assert!(normalize_anthropic_compatible_base_url("   ").is_err());
        // An explicit scheme with no host fails the host check. (A scheme-less
        // host like `ftp://x` is *not* rejected — it gets `http://` prepended,
        // matching the original behavior.)
        assert!(normalize_anthropic_compatible_base_url("http://").is_err());
    }

    #[test]
    fn normalize_dispatcher_rejects_codex() {
        assert!(normalize_base_url(Protocol::CodexResponses, "https://x").is_err());
    }

    #[test]
    fn empty_success_fallback_is_protocol_specific() {
        // OpenAI: no models, no seed → empty availability.
        static SEEDLESS_OPENAI: LazyLock<ProviderMetadata> = LazyLock::new(|| {
            ProviderMetadata::new(
                "test-openai-compatible",
                "Test OpenAI Compatible",
                "",
                "",
                None,
                None,
                None,
                &[],
                Protocol::OpenAiChat,
                crate::provider::ProviderCapabilities::new(
                    crate::provider::NO_REASONING,
                    crate::provider::NO_PARAMETERS,
                ),
                "chat/completions",
            )
        });
        assert!(
            empty_success_fallback(&SEEDLESS_OPENAI)
                .remote_model_ids()
                .is_empty()
        );
        // Anthropic (minimax) has a seed list → fallback yields the seeds.
        assert!(
            !empty_success_fallback(
                &crate::provider::minimax_coding_plan::MINIMAX_CODING_PLAN_METADATA
            )
            .remote_model_ids()
            .is_empty()
        );

        let qwen = ProviderMetadata::new(
            "qwen-test",
            "Qwen Test",
            "https://example.test/v1",
            "qwen3.7-plus",
            None,
            None,
            None,
            &["qwen3.7-plus", "qwen3.7-max"],
            Protocol::OpenAiChat,
            crate::provider::ProviderCapabilities::new(
                crate::provider::NO_REASONING,
                crate::provider::NO_PARAMETERS,
            ),
            "chat/completions",
        )
        .with_discovery(DiscoveryKind::QwenCloud);
        assert_eq!(
            empty_success_fallback(&qwen).remote_model_ids(),
            ["qwen3.7-plus", "qwen3.7-max"]
        );
    }

    #[tokio::test]
    async fn qwen_discovery_error_falls_back_to_curated_seeds() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let metadata = ProviderMetadata::new(
            "qwen-test",
            "Qwen Test",
            &format!("http://{address}/v1"),
            "qwen3.7-plus",
            None,
            None,
            None,
            &["qwen3.7-plus", "qwen3.7-max"],
            Protocol::OpenAiChat,
            crate::provider::ProviderCapabilities::new(
                crate::provider::NO_REASONING,
                crate::provider::NO_PARAMETERS,
            ),
            "chat/completions",
        )
        .with_discovery(DiscoveryKind::QwenCloud);
        let session = provider_session("test-key", &format!("http://{address}/v1"), "");

        let available = list_available_models_for_metadata(&metadata, &session)
            .await
            .unwrap();

        assert_eq!(
            available.remote_model_ids(),
            ["qwen3.7-plus", "qwen3.7-max"]
        );
    }

    #[tokio::test]
    async fn reachability_probe_never_treats_configured_model_fallback_as_online() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let metadata = ProviderMetadata::new(
            "test-openai-compatible",
            "Test OpenAI Compatible",
            "",
            "",
            None,
            None,
            None,
            &[],
            Protocol::OpenAiChat,
            crate::provider::ProviderCapabilities::new(
                crate::provider::NO_REASONING,
                crate::provider::NO_PARAMETERS,
            ),
            "chat/completions",
        );
        let session = provider_session("test-key", &format!("http://{address}/v1"), "local-model");

        let available = list_available_models_for_metadata(&metadata, &session)
            .await
            .unwrap();
        assert_eq!(available.remote_model_ids(), ["local-model"]);
        assert!(
            probe_reachability_for_metadata(&metadata, &session)
                .await
                .is_err()
        );
    }
}
