use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model_catalog::TransportProtocol;
use crate::provider::ReasoningSelection;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ReasoningCodec {
    #[serde(rename = "openai-compatible", alias = "open-ai-compatible")]
    OpenAiCompatible,
    /// The public OpenAI Chat Completions API uses a top-level
    /// `reasoning_effort` string. Keep it distinct from compatible gateways
    /// that accept a `reasoning: { effort }` envelope.
    #[serde(rename = "openai-chat-completions", alias = "open-ai-chat-completions")]
    OpenAiChatCompletions,
    CodexResponses,
    AnthropicThinking,
    /// Anthropic's adaptive-thinking generation (claude-sonnet-4-6 and later,
    /// claude-opus-4-7 and later, claude-sonnet-5, claude-fable-5). These
    /// models reject `{"type": "enabled", "budget_tokens": N}` with HTTP 400;
    /// thinking depth rides `output_config.effort` instead (see
    /// [`anthropic_adaptive_effort`]). Deliberate, don't fold into
    /// [`Self::AnthropicThinking`]: the wire shapes are mutually exclusive per
    /// model generation — budget payloads 400 on adaptive models, and
    /// `{"type": "adaptive"}` is unknown to pre-4.6 models.
    AnthropicAdaptive,
    /// Kimi K2.5/K2.6 use the vendor-specific top-level `thinking` toggle on
    /// their OpenAI-compatible Chat Completions endpoint.
    KimiThinking,
    ZaiThinking,
    /// GLM 5.2+ (Z.ai via OpenAI-compatible endpoints) rejects a request that
    /// carries BOTH `thinking` and `reasoning_effort` ("cannot specify both
    /// 'thinking' and 'reasoning_effort'"), which [`ZaiThinking`] sends together
    /// for older GLM. This variant sends only one field.
    ZaiReasoningEffort,
}

impl ReasoningCodec {
    pub(crate) const fn default_for_transport(transport: TransportProtocol) -> Self {
        match transport {
            TransportProtocol::OpenAiChat => Self::OpenAiCompatible,
            TransportProtocol::AnthropicMessages => Self::AnthropicThinking,
            TransportProtocol::CodexResponses => Self::CodexResponses,
        }
    }

    pub(crate) fn encode_json(self, selection: ReasoningSelection) -> Option<Value> {
        match self {
            Self::OpenAiCompatible => openai_compatible_reasoning(selection),
            Self::OpenAiChatCompletions => None,
            Self::CodexResponses => codex_responses_reasoning(selection),
            Self::AnthropicThinking => anthropic_thinking(selection),
            Self::AnthropicAdaptive => anthropic_adaptive(selection),
            Self::KimiThinking | Self::ZaiThinking | Self::ZaiReasoningEffort => None,
        }
    }

    pub(crate) fn apply_chat_completions_fields(
        self,
        body: &mut Value,
        selection: ReasoningSelection,
    ) {
        match self {
            Self::OpenAiChatCompletions => apply_openai_chat_completions_fields(body, selection),
            Self::KimiThinking => apply_kimi_thinking_fields(body, selection),
            Self::ZaiThinking => apply_zai_thinking_fields(body, selection),
            Self::ZaiReasoningEffort => apply_zai_reasoning_effort_fields(body, selection),
            Self::OpenAiCompatible
            | Self::CodexResponses
            | Self::AnthropicThinking
            | Self::AnthropicAdaptive => {
                if let Some(reasoning) = self.encode_json(selection) {
                    body["reasoning"] = reasoning;
                }
            }
        }
    }
}

fn apply_openai_chat_completions_fields(body: &mut Value, selection: ReasoningSelection) {
    let effort = match selection {
        ReasoningSelection::Default | ReasoningSelection::BudgetTokens(_) => return,
        ReasoningSelection::Off => "none",
        // `On` is the portable toggle state. The current OpenAI coding models
        // document medium as their balanced reasoning starting point.
        ReasoningSelection::On => "medium",
        ReasoningSelection::Ultra => "max",
        explicit => explicit.as_str(),
    };
    body["reasoning_effort"] = json!(effort);
}

/// The shared `{"effort": "..."}` envelope used by the OpenAI-compatible and
/// Codex reasoning codecs. Returns `None` for every non-effort selection
/// (`Default`/`Off`/`On`/`BudgetTokens`).
pub(crate) fn effort_payload(selection: ReasoningSelection) -> Option<Value> {
    match selection {
        ReasoningSelection::Minimal
        | ReasoningSelection::Low
        | ReasoningSelection::Medium
        | ReasoningSelection::High
        | ReasoningSelection::Max
        | ReasoningSelection::XHigh
        | ReasoningSelection::Ultra => Some(json!({ "effort": selection.as_str() })),
        _ => None,
    }
}

fn openai_compatible_reasoning(selection: ReasoningSelection) -> Option<Value> {
    match selection {
        // `On` is a distinct envelope; everything else is either an effort
        // value or unset, which `effort_payload` maps appropriately.
        ReasoningSelection::On => Some(json!({ "enabled": true })),
        other => effort_payload(other),
    }
}

fn apply_kimi_thinking_fields(body: &mut Value, selection: ReasoningSelection) {
    match selection {
        ReasoningSelection::Default | ReasoningSelection::BudgetTokens(_) => {}
        ReasoningSelection::Off => {
            body["thinking"] = json!({ "type": "disabled" });
        }
        ReasoningSelection::On
        | ReasoningSelection::Minimal
        | ReasoningSelection::Low
        | ReasoningSelection::Medium
        | ReasoningSelection::High
        | ReasoningSelection::Max
        | ReasoningSelection::XHigh
        | ReasoningSelection::Ultra => {
            body["thinking"] = json!({ "type": "enabled" });
        }
    }
}

fn apply_zai_thinking_fields(body: &mut Value, selection: ReasoningSelection) {
    match selection {
        ReasoningSelection::Default | ReasoningSelection::BudgetTokens(_) => {}
        ReasoningSelection::Off => {
            body["thinking"] = json!({ "type": "disabled" });
        }
        ReasoningSelection::On => {
            body["thinking"] = json!({ "type": "enabled" });
        }
        ReasoningSelection::Minimal
        | ReasoningSelection::Low
        | ReasoningSelection::Medium
        | ReasoningSelection::High
        | ReasoningSelection::Max
        | ReasoningSelection::XHigh
        | ReasoningSelection::Ultra => {
            body["thinking"] = json!({ "type": "enabled" });
            body["reasoning_effort"] = json!(selection.as_str());
        }
    }
}

/// GLM 5.2+ rejects a request carrying BOTH `thinking` and `reasoning_effort`,
/// so send only one field: `reasoning_effort` for an effort level, and the
/// `thinking` toggle only for the plain on/off cases (which carry no
/// `reasoning_effort` to conflict with). An effort level implies reasoning is
/// on, so no separate enable flag is needed. Contrast [`apply_zai_thinking_fields`],
/// which sends both for older GLM that required the explicit `thinking` enable.
fn apply_zai_reasoning_effort_fields(body: &mut Value, selection: ReasoningSelection) {
    match selection {
        ReasoningSelection::Default | ReasoningSelection::BudgetTokens(_) => {}
        ReasoningSelection::Off => {
            body["thinking"] = json!({ "type": "disabled" });
        }
        ReasoningSelection::On => {
            body["thinking"] = json!({ "type": "enabled" });
        }
        ReasoningSelection::Minimal
        | ReasoningSelection::Low
        | ReasoningSelection::Medium
        | ReasoningSelection::High
        | ReasoningSelection::Max
        | ReasoningSelection::XHigh
        | ReasoningSelection::Ultra => {
            body["reasoning_effort"] = json!(selection.as_str());
        }
    }
}

fn codex_responses_reasoning(selection: ReasoningSelection) -> Option<Value> {
    if selection == ReasoningSelection::Off {
        return None;
    }

    // Always request a streamed reasoning summary so the user sees the model
    // think in real time (as the Codex CLI does) instead of watching a silent
    // spinner for the whole time-to-first-token. Merge it onto the effort
    // envelope when an effort is selected. Explicit Off above preserves the
    // user's cost/latency opt-out.
    let mut payload = effort_payload(selection).unwrap_or_else(|| json!({}));
    payload["summary"] = json!("detailed");
    Some(payload)
}

/// The `thinking` payload for adaptive-generation Anthropic models.
fn anthropic_adaptive(selection: ReasoningSelection) -> Option<Value> {
    match selection {
        // No explicit choice: leave the field off so each model applies its
        // own default (claude-fable-5 always thinks; claude-opus-4-8 runs
        // without thinking).
        ReasoningSelection::Default => None,
        // Valid on every adaptive-generation model except claude-fable-5,
        // which 400s on an explicit disable — fable targets must simply not
        // offer Off/toggle in their reasoning options.
        ReasoningSelection::Off => Some(json!({ "type": "disabled" })),
        // Budget selections have no adaptive equivalent (`budget_tokens`
        // returns 400 on these models); request plain adaptive thinking
        // instead of a payload the API rejects. Efforts also ride the
        // adaptive envelope — their depth goes to `output_config.effort` via
        // `anthropic_adaptive_effort`.
        _ => Some(json!({ "type": "adaptive" })),
    }
}

/// The `output_config.effort` label for an effort selection on adaptive
/// Anthropic models. The API accepts low/medium/high/xhigh/max; bonsai's wider
/// effort ladder clamps into that range (minimal→low, ultra→max).
pub(crate) fn anthropic_adaptive_effort(selection: ReasoningSelection) -> Option<&'static str> {
    match selection {
        ReasoningSelection::Minimal | ReasoningSelection::Low => Some("low"),
        ReasoningSelection::Medium => Some("medium"),
        ReasoningSelection::High => Some("high"),
        ReasoningSelection::XHigh => Some("xhigh"),
        ReasoningSelection::Max | ReasoningSelection::Ultra => Some("max"),
        ReasoningSelection::Default
        | ReasoningSelection::Off
        | ReasoningSelection::On
        | ReasoningSelection::BudgetTokens(_) => None,
    }
}

fn anthropic_thinking(selection: ReasoningSelection) -> Option<Value> {
    // Anthropic takes an explicit `budget_tokens` rather than an effort label,
    // so map each effort level to a coarse budget. Without this, selecting an
    // effort on an Anthropic model silently disabled thinking entirely.
    // TODO: drive these budgets from model metadata once per-model
    // thinking budgets land.
    let budget_tokens = match selection {
        ReasoningSelection::Default | ReasoningSelection::Off => return None,
        ReasoningSelection::BudgetTokens(tokens) => tokens,
        ReasoningSelection::On => 4096,
        ReasoningSelection::Minimal => 1024,
        ReasoningSelection::Low => 4096,
        ReasoningSelection::Medium => 8192,
        ReasoningSelection::High => 16384,
        ReasoningSelection::XHigh => 24576,
        ReasoningSelection::Max => 32768,
        ReasoningSelection::Ultra => 49152,
    };
    Some(json!({
        "type": "enabled",
        "budget_tokens": budget_tokens,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_payload_emits_for_each_effort() {
        for selection in [
            ReasoningSelection::Minimal,
            ReasoningSelection::Low,
            ReasoningSelection::Medium,
            ReasoningSelection::High,
            ReasoningSelection::Max,
            ReasoningSelection::XHigh,
            ReasoningSelection::Ultra,
        ] {
            assert_eq!(
                effort_payload(selection),
                Some(json!({ "effort": selection.as_str() })),
                "effort variant {selection:?} should emit an effort payload"
            );
        }
    }

    #[test]
    fn effort_payload_returns_none_for_non_effort_selections() {
        assert_eq!(effort_payload(ReasoningSelection::Default), None);
        assert_eq!(effort_payload(ReasoningSelection::Off), None);
        assert_eq!(effort_payload(ReasoningSelection::On), None);
        assert_eq!(effort_payload(ReasoningSelection::BudgetTokens(1024)), None);
    }

    #[test]
    fn openai_compatible_on_emits_enabled_envelope() {
        assert_eq!(
            openai_compatible_reasoning(ReasoningSelection::On),
            Some(json!({ "enabled": true }))
        );
        assert_eq!(openai_compatible_reasoning(ReasoningSelection::Off), None);
        assert_eq!(
            openai_compatible_reasoning(ReasoningSelection::High),
            Some(json!({ "effort": "high" }))
        );
    }

    #[test]
    fn openai_chat_completions_uses_top_level_reasoning_effort() {
        for (selection, expected) in [
            (ReasoningSelection::Off, "none"),
            (ReasoningSelection::On, "medium"),
            (ReasoningSelection::High, "high"),
            (ReasoningSelection::Max, "max"),
            (ReasoningSelection::Ultra, "max"),
        ] {
            let mut body = json!({});
            ReasoningCodec::OpenAiChatCompletions
                .apply_chat_completions_fields(&mut body, selection);
            assert_eq!(body, json!({ "reasoning_effort": expected }));
        }

        let mut body = json!({});
        ReasoningCodec::OpenAiChatCompletions
            .apply_chat_completions_fields(&mut body, ReasoningSelection::Default);
        assert_eq!(body, json!({}));
    }

    #[test]
    fn openai_chat_completions_codec_parses_from_kebab_case() {
        assert_eq!(
            serde_json::from_str::<ReasoningCodec>("\"openai-chat-completions\"").unwrap(),
            ReasoningCodec::OpenAiChatCompletions
        );
    }

    #[test]
    fn kimi_thinking_uses_vendor_toggle_without_inventing_effort_fields() {
        let mut enabled = json!({});
        ReasoningCodec::KimiThinking
            .apply_chat_completions_fields(&mut enabled, ReasoningSelection::High);
        assert_eq!(enabled, json!({ "thinking": { "type": "enabled" } }));

        let mut disabled = json!({});
        ReasoningCodec::KimiThinking
            .apply_chat_completions_fields(&mut disabled, ReasoningSelection::Off);
        assert_eq!(disabled, json!({ "thinking": { "type": "disabled" } }));

        let mut default = json!({});
        ReasoningCodec::KimiThinking
            .apply_chat_completions_fields(&mut default, ReasoningSelection::Default);
        assert_eq!(default, json!({}));
    }

    #[test]
    fn zai_thinking_applies_thinking_fields_for_effort_and_toggle() {
        let mut body = json!({});
        ReasoningCodec::ZaiThinking
            .apply_chat_completions_fields(&mut body, ReasoningSelection::Max);
        assert_eq!(
            body,
            json!({
                "thinking": { "type": "enabled" },
                "reasoning_effort": "max",
            })
        );

        let mut body = json!({});
        ReasoningCodec::ZaiThinking
            .apply_chat_completions_fields(&mut body, ReasoningSelection::Off);
        assert_eq!(body, json!({ "thinking": { "type": "disabled" } }));

        let mut body = json!({});
        ReasoningCodec::ZaiThinking
            .apply_chat_completions_fields(&mut body, ReasoningSelection::Default);
        assert_eq!(body, json!({}));
    }

    #[test]
    fn zai_reasoning_effort_never_sends_thinking_and_effort_together() {
        // GLM 5.2 rejects both fields at once; an effort level sends only
        // `reasoning_effort`.
        let mut body = json!({});
        ReasoningCodec::ZaiReasoningEffort
            .apply_chat_completions_fields(&mut body, ReasoningSelection::High);
        assert_eq!(body, json!({ "reasoning_effort": "high" }));
        assert!(
            body.get("thinking").is_none(),
            "must not also send `thinking`"
        );

        // The plain on/off toggles carry no effort, so `thinking` alone is safe.
        let mut body = json!({});
        ReasoningCodec::ZaiReasoningEffort
            .apply_chat_completions_fields(&mut body, ReasoningSelection::Off);
        assert_eq!(body, json!({ "thinking": { "type": "disabled" } }));

        let mut body = json!({});
        ReasoningCodec::ZaiReasoningEffort
            .apply_chat_completions_fields(&mut body, ReasoningSelection::Default);
        assert_eq!(body, json!({}));
    }

    #[test]
    fn zai_reasoning_effort_codec_parses_from_kebab_case() {
        assert_eq!(
            serde_json::from_str::<ReasoningCodec>("\"zai-reasoning-effort\"").unwrap(),
            ReasoningCodec::ZaiReasoningEffort
        );
    }

    #[test]
    fn anthropic_thinking_maps_each_effort_to_a_budget() {
        for (selection, budget) in [
            (ReasoningSelection::Minimal, 1024),
            (ReasoningSelection::Low, 4096),
            (ReasoningSelection::Medium, 8192),
            (ReasoningSelection::High, 16384),
            (ReasoningSelection::XHigh, 24576),
            (ReasoningSelection::Max, 32768),
            (ReasoningSelection::Ultra, 49152),
        ] {
            assert_eq!(
                anthropic_thinking(selection),
                Some(json!({ "type": "enabled", "budget_tokens": budget })),
                "budget for {selection:?}"
            );
        }
        assert_eq!(anthropic_thinking(ReasoningSelection::Default), None);
        assert_eq!(anthropic_thinking(ReasoningSelection::Off), None);
    }

    #[test]
    fn anthropic_adaptive_never_emits_budget_tokens() {
        // Adaptive-generation Anthropic models 400 on `budget_tokens`; every
        // selection must map to adaptive/disabled/absent.
        assert_eq!(anthropic_adaptive(ReasoningSelection::Default), None);
        assert_eq!(
            anthropic_adaptive(ReasoningSelection::Off),
            Some(json!({ "type": "disabled" }))
        );
        for selection in [
            ReasoningSelection::On,
            ReasoningSelection::Low,
            ReasoningSelection::XHigh,
            ReasoningSelection::Ultra,
            ReasoningSelection::BudgetTokens(8192),
        ] {
            assert_eq!(
                anthropic_adaptive(selection),
                Some(json!({ "type": "adaptive" })),
                "selection {selection:?} must ride the adaptive envelope"
            );
        }
    }

    #[test]
    fn anthropic_adaptive_effort_clamps_to_api_range() {
        assert_eq!(
            anthropic_adaptive_effort(ReasoningSelection::Minimal),
            Some("low")
        );
        assert_eq!(
            anthropic_adaptive_effort(ReasoningSelection::Medium),
            Some("medium")
        );
        assert_eq!(
            anthropic_adaptive_effort(ReasoningSelection::XHigh),
            Some("xhigh")
        );
        assert_eq!(
            anthropic_adaptive_effort(ReasoningSelection::Ultra),
            Some("max")
        );
        assert_eq!(anthropic_adaptive_effort(ReasoningSelection::Off), None);
        assert_eq!(
            anthropic_adaptive_effort(ReasoningSelection::BudgetTokens(4096)),
            None
        );
    }

    #[test]
    fn anthropic_adaptive_codec_parses_from_kebab_case() {
        assert_eq!(
            serde_json::from_str::<ReasoningCodec>("\"anthropic-adaptive\"").unwrap(),
            ReasoningCodec::AnthropicAdaptive
        );
    }

    #[test]
    fn codex_responses_requests_summary_except_explicit_off() {
        // No effort selected → still stream the summary at the server default.
        assert_eq!(
            codex_responses_reasoning(ReasoningSelection::Default),
            Some(json!({ "summary": "detailed" }))
        );
        assert_eq!(codex_responses_reasoning(ReasoningSelection::Off), None);
        assert_eq!(
            codex_responses_reasoning(ReasoningSelection::On),
            Some(json!({ "summary": "detailed" }))
        );
        // Effort selected → merge effort with the streamed summary.
        assert_eq!(
            codex_responses_reasoning(ReasoningSelection::Max),
            Some(json!({ "effort": ReasoningSelection::Max.as_str(), "summary": "detailed" }))
        );
    }
}
