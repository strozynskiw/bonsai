use crate::model_catalog::{
    ConnectionId, ModelCatalog, ModelFeature, ModelId, connection_id_for_provider_id,
};
use crate::model_resolution::{
    normalize_reasoning_for_provider_model, resolved_model_for_provider_model,
};
use crate::model_role::ModelShortcutKey;
use crate::provider::{ModelPricing, ProviderMetadata, ReasoningSelection};
use crate::session::SessionStore;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderOption {
    pub provider_id: String,
    pub provider_label: String,
    pub authorized: bool,
    pub current: bool,
    pub uses_endpoint_auth_form: bool,
}

impl ProviderOption {
    /// Case-insensitive substring match on the label and id — the two fields a
    /// user would type to find a provider in the authorize picker.
    pub fn matches_filter(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.provider_label.to_lowercase().contains(&query)
            || self.provider_id.to_lowercase().contains(&query)
    }
}

/// The authorize picker's rows narrowed to `query`. The returned references keep
/// their original order; an empty/whitespace query returns everything. Every
/// reader (cursor clamp, move, submit, render) filters through this one helper so
/// the `cursor` — which indexes the *filtered* view — can never disagree.
pub fn filter_authorize_providers<'a>(
    providers: &'a [ProviderOption],
    query: &str,
) -> Vec<&'a ProviderOption> {
    providers
        .iter()
        .filter(|provider| provider.matches_filter(query))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelOption {
    pub provider_id: String,
    pub connection_id: String,
    pub provider_label: String,
    pub model_id: Option<String>,
    pub model: String,
    pub display_name: String,
    pub reasoning: ReasoningSelection,
    pub recommended_reasoning: Option<ReasoningSelection>,
    pub discouraged_reasoning: Vec<ReasoningSelection>,
    pub supported_reasoning: Vec<ReasoningSelection>,
    pub shortcut_bindings: Vec<(ModelShortcutKey, ReasoningSelection)>,
    pub parameter_preview: String,
    pub pricing: Option<ModelPricing>,
    /// Resolved context window (live probe first, then catalog); `None` means
    /// unknown — the runtime assumes the default window.
    pub context_window: Option<u32>,
    /// Capability badges (tools/vision/reasoning), live probe first.
    pub features: Vec<ModelFeature>,
}

impl ModelOption {
    pub(crate) fn from_provider_model(
        catalog: Option<&ModelCatalog>,
        provider_id: &str,
        provider_label: &str,
        session_store: &SessionStore,
        metadata: &ProviderMetadata,
        model: String,
    ) -> Self {
        let provider_session = session_store.session(provider_id);
        if let Some(resolved) = resolved_model_for_provider_model(catalog, provider_id, &model) {
            let canonical_model = resolved.model_id.to_string();
            let saved_reasoning = provider_session
                .model_reasoning
                .get(&canonical_model)
                .or_else(|| provider_session.model_reasoning.get(&model))
                .copied();
            // Live server metadata outranks the catalog's static values —
            // matching the run-target resolution ladder.
            let live = catalog.and_then(|catalog| {
                catalog.live_model_for_connection_model(
                    &resolved.connection_id,
                    &resolved.remote_model_id,
                )
            });
            let reasoning = saved_reasoning
                .or_else(|| live.as_ref().and_then(|model| model.recommended_reasoning))
                .or(resolved.recommended_effort)
                .unwrap_or(provider_session.reasoning);
            let parameter_preview = resolved.parameter_preview_label();
            let pricing = resolved.pricing;
            let context_window = live
                .as_ref()
                .and_then(|model| model.context_window)
                .or(resolved.context_window);
            let features = live
                .as_ref()
                .filter(|model| !model.features.is_empty())
                .map(|model| model.features.clone())
                .unwrap_or_else(|| resolved.features.clone());
            let supported_reasoning = live
                .as_ref()
                .filter(|model| !model.supported_reasoning.is_empty())
                .map(|model| model.supported_reasoning.clone())
                .unwrap_or_else(|| resolved.reasoning_selections());
            let recommended_reasoning = live
                .as_ref()
                .and_then(|model| model.recommended_reasoning)
                .or(resolved.recommended_effort);
            return Self {
                provider_id: provider_id.to_string(),
                connection_id: resolved.connection_id.to_string(),
                provider_label: provider_label.to_string(),
                model_id: Some(resolved.model_id.to_string()),
                model: model.clone(),
                display_name: live
                    .as_ref()
                    .and_then(|model| model.display_name.as_deref())
                    .unwrap_or(resolved.display_name.as_ref())
                    .to_string(),
                reasoning: normalize_reasoning_for_provider_model(
                    catalog,
                    provider_id,
                    &model,
                    metadata,
                    reasoning,
                ),
                recommended_reasoning,
                discouraged_reasoning: resolved.discouraged_efforts.clone(),
                supported_reasoning,
                shortcut_bindings: session_store.model_shortcuts_for_selection(
                    &provider_id
                        .parse()
                        .unwrap_or_else(|_| ConnectionId::fallback("unknown")),
                    &model,
                    Some(&canonical_model.parse().unwrap_or_else(|_| {
                        ModelId::fallback(&ConnectionId::fallback(provider_id))
                    })),
                ),
                parameter_preview,
                pricing,
                context_window,
                features,
            };
        }

        let shortcut_bindings = session_store.model_shortcuts_for_selection(
            &provider_id
                .parse()
                .unwrap_or_else(|_| ConnectionId::fallback("unknown")),
            &model,
            None,
        );
        let capabilities = metadata.capabilities_for_model(&model);
        let live = catalog
            .zip(connection_id_for_provider_id(provider_id))
            .and_then(|(catalog, connection_id)| {
                catalog.live_model_for_connection_model(&connection_id, &model)
            });
        let reasoning = provider_session
            .model_reasoning
            .get(&model)
            .copied()
            .or_else(|| live.as_ref().and_then(|model| model.recommended_reasoning))
            .unwrap_or(provider_session.reasoning);
        let supported_reasoning = live
            .as_ref()
            .filter(|model| !model.supported_reasoning.is_empty())
            .map(|model| model.supported_reasoning.clone())
            .unwrap_or_else(|| capabilities.reasoning.to_vec());
        Self {
            provider_id: provider_id.to_string(),
            connection_id: connection_id_for_provider_id(provider_id)
                .map(|id| id.to_string())
                .unwrap_or_else(|| provider_id.to_string()),
            provider_label: provider_label.to_string(),
            model_id: None,
            model: model.clone(),
            display_name: live
                .as_ref()
                .and_then(|model| model.display_name.as_deref())
                .unwrap_or_else(|| short_model_label(&model))
                .to_string(),
            reasoning: normalize_reasoning_for_provider_model(
                catalog,
                provider_id,
                &model,
                metadata,
                reasoning,
            ),
            recommended_reasoning: live.as_ref().and_then(|model| model.recommended_reasoning),
            discouraged_reasoning: Vec::new(),
            supported_reasoning,
            shortcut_bindings,
            parameter_preview: metadata.parameter_preview_for_model(&model),
            pricing: metadata.pricing,
            context_window: live
                .as_ref()
                .and_then(|model| model.context_window)
                .or(metadata.context_window),
            features: live.map(|model| model.features).unwrap_or_default(),
        }
    }

    pub(crate) fn display_model(&self) -> &str {
        self.model_id.as_deref().unwrap_or(&self.model)
    }

    pub(crate) fn picker_model_label(&self) -> &str {
        &self.display_name
    }

    pub(crate) fn selection_input(&self) -> Option<String> {
        self.model_id
            .as_ref()
            .map(|model_id| format!("{}:{}", self.connection_id, short_model_label(model_id)))
    }

    pub(crate) fn pricing_label(&self) -> String {
        pricing_label(self.pricing)
    }

    /// Compact `1$/0.4$/5$` shorthand (input/cache/output, per million
    /// tokens) for space-tight surfaces — sidebar, status line, palette.
    pub(crate) fn pricing_compact(&self) -> String {
        pricing_compact(self.pricing)
    }

    /// `131k ctx` for a known window; an unknown window is flagged as assumed
    /// (display only — the runtime default lives in the resolution ladder).
    pub(crate) fn context_window_label(&self) -> String {
        match self.context_window {
            Some(tokens) => format!("{} ctx", compact_token_count(tokens)),
            None => format!(
                "~{} ctx (assumed)",
                compact_token_count(crate::provider::DEFAULT_CONTEXT_WINDOW_TOKENS)
            ),
        }
    }

    /// Server-reported capability icons (`⚒︎ ◉ ∴`), empty when the source
    /// reported none.
    pub(crate) fn feature_icons(&self) -> String {
        self.features
            .iter()
            .filter_map(|feature| match feature {
                ModelFeature::ToolCall => Some("⚒︎"),
                ModelFeature::Attachment => Some("◉"),
                ModelFeature::Reasoning => Some("∴"),
                ModelFeature::StructuredOutput | ModelFeature::Temperature => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub(crate) fn matches_model(&self, model: &str) -> bool {
        self.model == model || self.display_name == model || self.model_id.as_deref() == Some(model)
    }

    pub(crate) fn matches_filter(&self, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let lower = filter.to_lowercase();
        self.model.to_lowercase().contains(&lower)
            || self.display_name.to_lowercase().contains(&lower)
            || self
                .model_id
                .as_ref()
                .is_some_and(|model_id| model_id.to_lowercase().contains(&lower))
    }

    pub(crate) fn starts_with_filter(&self, filter: &str) -> bool {
        if filter.is_empty() {
            return true;
        }
        let lower = filter.to_lowercase();
        self.model.to_lowercase().starts_with(&lower)
            || self.display_name.to_lowercase().starts_with(&lower)
            || self
                .model_id
                .as_ref()
                .is_some_and(|model_id| model_id.to_lowercase().starts_with(&lower))
    }
}

fn pricing_label(pricing: Option<ModelPricing>) -> String {
    let Some(pricing) = pricing else {
        return "n/a".to_string();
    };
    format!(
        "in {} cached {} out {}",
        price_per_million(pricing.input_micros_per_million),
        optional_price_per_million(pricing.cache_read_micros_per_million),
        price_per_million(pricing.output_micros_per_million)
    )
}

/// `1$/0.4$/5$` — input/cache/output per million tokens, just the numbers
/// with a `$` suffix and `/` separators, no labels. Compact enough for the
/// sidebar, status line, and command palette. Cache falls back to `-` when
/// the model has no cache-read price.
fn pricing_compact(pricing: Option<ModelPricing>) -> String {
    let Some(pricing) = pricing else {
        return "n/a".to_string();
    };
    let cache = pricing
        .cache_read_micros_per_million
        .map(compact_price)
        .unwrap_or_else(|| "-".to_string());
    format!(
        "{}/{}/{}",
        compact_price(pricing.input_micros_per_million),
        cache,
        compact_price(pricing.output_micros_per_million)
    )
}

/// A single price as `5$` / `0.4$` — dollars per million tokens with the
/// `$` as a suffix and trailing zeros trimmed.
fn compact_price(micros: u64) -> String {
    if micros == 0 {
        return "0$".to_string();
    }
    let dollars = micros as f64 / 1_000_000.0;
    let mut value = format!("{dollars:.2}");
    while value.contains('.') && value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    format!("{value}$")
}

fn optional_price_per_million(micros: Option<u64>) -> String {
    micros
        .map(price_per_million)
        .unwrap_or_else(|| "n/a".to_string())
}

fn price_per_million(micros: u64) -> String {
    if micros == 0 {
        return "$0/M".to_string();
    }
    let dollars = micros as f64 / 1_000_000.0;
    let mut value = format!("{dollars:.6}");
    while value.contains('.') && value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.pop();
    }
    if let Some((_whole, fraction)) = value.split_once('.')
        && fraction.len() == 1
    {
        value.push('0');
    }
    format!("${value}/M")
}

/// `131072` → `131k`, `1048576` → `1.0M`; small windows print raw.
fn compact_token_count(tokens: u32) -> String {
    if tokens >= 1_000_000 {
        let millions = f64::from(tokens) / 1_000_000.0;
        format!("{millions:.1}M")
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

pub(crate) fn short_model_label(model: &str) -> &str {
    model
        .split_once('/')
        .map(|(_provider, model)| model)
        .unwrap_or(model)
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use super::*;
    use crate::model_catalog::{
        CatalogSpec, ConnectionAuth, ConnectionSpec, ModelCatalog, ModelsDevCatalog, TargetSpec,
        TransportProtocol,
    };

    fn option(provider_id: &str, provider_label: &str) -> ProviderOption {
        ProviderOption {
            provider_id: provider_id.to_string(),
            provider_label: provider_label.to_string(),
            authorized: false,
            current: false,
            uses_endpoint_auth_form: false,
        }
    }

    #[test]
    fn filter_authorize_providers_matches_label_and_id_case_insensitively() {
        let providers = vec![
            option("opencode", "OpenCode Go"),
            option("qwencloud", "Qwen Cloud API"),
            option("qwencloud-token-plan", "Qwen Cloud Token Plan"),
        ];

        // Empty query returns everything, order preserved.
        assert_eq!(filter_authorize_providers(&providers, "  ").len(), 3);
        // Label match, case-insensitive.
        assert_eq!(filter_authorize_providers(&providers, "QWEN").len(), 2);
        // Id-only fragment (not in the label) still matches.
        let token = filter_authorize_providers(&providers, "token-plan");
        assert_eq!(token.len(), 1);
        assert_eq!(token[0].provider_id, "qwencloud-token-plan");
        // No match yields an empty set.
        assert!(filter_authorize_providers(&providers, "gemini").is_empty());
    }

    #[test]
    fn provider_model_option_uses_catalog_display_alias() {
        let connection_id: crate::model_catalog::ConnectionId =
            "openai-compatible".parse().unwrap();
        let model_id: crate::model_catalog::ModelId = "openai-compatible/Qwen".parse().unwrap();
        let catalog = ModelCatalog::from_spec(CatalogSpec {
            connections: vec![ConnectionSpec {
                id: connection_id.clone(),
                enabled: true,
                display_name: "OpenAI Compatible".into(),
                auth: ConnectionAuth::OptionalApiKey,
                transport: TransportProtocol::OpenAiChat,
                default_base_url: "http://localhost:1234/v1".into(),
                api_key_env: None,
                model_env: None,
                base_url_env: None,
                default_model: None,
                default_endpoint_path: Some("chat/completions".into()),
                default_token_counter: Some(crate::provider::TokenCounterKind::Heuristic),
                reasoning_codec: None,
                prompt_cache: false,
                prompt_cache_policy: Default::default(),
                reasoning_content_echo: false,
                usage_frame_ends_stream: false,
                auth_header: None,
                discovery: Default::default(),
            }],
            targets: vec![TargetSpec {
                connection: connection_id,
                enabled: true,
                model: model_id,
                display_name: Some("Qwen".into()),
                metadata_model: None,
                remote_model: Some("qwen/qwen3.6-35b-a3b".into()),
                recommended: false,
                recommended_effort: None,
                discouraged_efforts: Vec::new(),
                is_default: false,
                transport: None,
                prompt_cache_policy: None,
                endpoint_path: None,
                context_window: Some(131_072),
                output_limit: None,
                token_counter: None,
                max_tokens: None,
                reasoning_codec: None,
                reasoning_options: None,
                features: Vec::new(),
                pricing: None,
                roles: Vec::new(),
                pinned: false,
            }],
            models_dev: ModelsDevCatalog::default(),
            connection_sources: std::collections::HashMap::new(),
        });
        let mut session_store = SessionStore::default();
        session_store.ensure_provider("openai-compatible");
        // The generic endpoint provider is a catalog connection now, so no
        // built-in metadata exists; a seedless stand-in matches what
        // `build_provider_metadata` synthesizes for it.
        static ENDPOINT_METADATA: LazyLock<crate::provider::ProviderMetadata> =
            LazyLock::new(|| {
                crate::provider::ProviderMetadata::new(
                    "openai-compatible",
                    "OpenAI Compatible",
                    "",
                    "",
                    None,
                    None,
                    None,
                    &[],
                    crate::provider::Protocol::OpenAiChat,
                    crate::provider::ProviderCapabilities::new(
                        crate::provider::NO_REASONING,
                        crate::provider::NO_PARAMETERS,
                    ),
                    "chat/completions",
                )
            });
        let metadata = &ENDPOINT_METADATA;

        let option = ModelOption::from_provider_model(
            Some(&catalog),
            "openai-compatible",
            "OpenAI Compatible",
            &session_store,
            metadata,
            "qwen/qwen3.6-35b-a3b".to_string(),
        );

        assert_eq!(option.model, "qwen/qwen3.6-35b-a3b");
        assert_eq!(option.model_id.as_deref(), Some("openai-compatible/Qwen"));
        assert_eq!(option.picker_model_label(), "Qwen");
        assert!(option.matches_filter("qwe"));
        assert_eq!(option.context_window, Some(131_072));
        assert_eq!(option.context_window_label(), "131k ctx");
    }

    #[test]
    fn context_window_label_marks_unknown_as_assumed() {
        let mut option = crate::tui::test_utils::model_option("opencode", "OpenCode Go", "m");
        assert_eq!(option.context_window_label(), "~120k ctx (assumed)");

        option.context_window = Some(200_000);
        assert_eq!(option.context_window_label(), "200k ctx");
        option.context_window = Some(1_048_576);
        assert_eq!(option.context_window_label(), "1.0M ctx");
    }

    #[test]
    fn catalog_only_provider_exposes_reasoning_in_picker() {
        // Regression guard for catalog-defined providers (no builtin metadata
        // struct): the picker must surface reasoning from the catalog target,
        // not the flat registry metadata, whose capabilities are minted with
        // NO_REASONING (`build_provider_metadata`). DeepSeek is the canary: a
        // thinking toggle plus low/medium/high/max efforts.
        let catalog = ModelCatalog::load_builtin().unwrap();
        let registry = crate::provider::ProviderRegistry::from_catalog(&catalog);
        let factory = registry.get("deepseek").unwrap();
        let mut session_store = SessionStore::default();
        session_store.ensure_provider("deepseek");
        session_store
            .session_mut("deepseek")
            .model_reasoning
            .insert(
                "deepseek/deepseek-v4-flash".to_string(),
                ReasoningSelection::High,
            );

        let option = ModelOption::from_provider_model(
            Some(&catalog),
            "deepseek",
            "DeepSeek API",
            &session_store,
            factory.metadata(),
            "deepseek/deepseek-v4-flash".to_string(),
        );

        for expected in [
            ReasoningSelection::Off,
            ReasoningSelection::Low,
            ReasoningSelection::Medium,
            ReasoningSelection::High,
            ReasoningSelection::Max,
        ] {
            assert!(
                option.supported_reasoning.contains(&expected),
                "picker lost catalog reasoning {expected:?}: {:?}",
                option.supported_reasoning
            );
        }
        // The saved effort must survive normalization instead of being clamped
        // to Default by the metadata-only path.
        assert_eq!(option.reasoning, ReasoningSelection::High);
    }

    #[test]
    fn catalog_recommendation_defaults_only_unsaved_model_effort() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let mut session_store = SessionStore::default();
        session_store.ensure_provider("opencode");
        session_store.session_mut("opencode").reasoning = ReasoningSelection::High;
        let metadata = crate::provider::metadata_for("opencode").unwrap();

        let recommended = ModelOption::from_provider_model(
            Some(&catalog),
            "opencode",
            "OpenCode Go",
            &session_store,
            metadata,
            "glm-5.2".to_string(),
        );
        assert_eq!(recommended.reasoning, ReasoningSelection::Off);
        assert_eq!(
            recommended.recommended_reasoning,
            Some(ReasoningSelection::Off)
        );
        assert!(
            recommended
                .discouraged_reasoning
                .contains(&ReasoningSelection::Default)
        );
        assert!(
            recommended
                .discouraged_reasoning
                .contains(&ReasoningSelection::Max)
        );

        session_store
            .session_mut("opencode")
            .model_reasoning
            .insert("opencode/glm-5.2".to_string(), ReasoningSelection::High);
        let saved = ModelOption::from_provider_model(
            Some(&catalog),
            "opencode",
            "OpenCode Go",
            &session_store,
            metadata,
            "glm-5.2".to_string(),
        );
        assert_eq!(saved.reasoning, ReasoningSelection::High);
    }

    #[test]
    fn codex_picker_uses_live_reasoning_choices_and_recommendation() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let connection_id: crate::model_catalog::ConnectionId = "codex".parse().unwrap();
        catalog
            .write_live_availability(
                &connection_id,
                crate::model_catalog::LiveModelAvailability {
                    models: vec![
                        crate::model_catalog::AvailableModel::with_metadata(
                            "gpt-5.5",
                            Some(272_000),
                            Some("GPT-5.5 (live)".to_string()),
                            vec![crate::model_catalog::ModelFeature::Reasoning],
                        )
                        .with_reasoning(
                            vec![
                                ReasoningSelection::Low,
                                ReasoningSelection::Medium,
                                ReasoningSelection::High,
                                ReasoningSelection::XHigh,
                                ReasoningSelection::Max,
                                ReasoningSelection::Ultra,
                            ],
                            Some(ReasoningSelection::Medium),
                        ),
                    ],
                    ..crate::model_catalog::LiveModelAvailability::default()
                },
            )
            .unwrap();
        let mut session_store = SessionStore::default();
        session_store.ensure_provider("codex");
        session_store.session_mut("codex").reasoning = ReasoningSelection::High;
        let metadata = crate::provider::metadata_for("codex").unwrap();

        let option = ModelOption::from_provider_model(
            Some(&catalog),
            "codex",
            "Codex",
            &session_store,
            metadata,
            "gpt-5.5".to_string(),
        );

        assert_eq!(option.display_name, "GPT-5.5 (live)");
        assert_eq!(option.reasoning, ReasoningSelection::Medium);
        assert_eq!(
            option.recommended_reasoning,
            Some(ReasoningSelection::Medium)
        );
        assert!(
            option
                .supported_reasoning
                .contains(&ReasoningSelection::Max)
        );
        assert!(
            option
                .supported_reasoning
                .contains(&ReasoningSelection::Ultra)
        );
        assert_eq!(option.context_window, Some(272_000));
    }

    #[test]
    fn feature_icons_render_known_capabilities_only() {
        let mut option = crate::tui::test_utils::model_option("opencode", "OpenCode Go", "m");
        assert_eq!(option.feature_icons(), "");

        option.features = vec![
            crate::model_catalog::ModelFeature::ToolCall,
            crate::model_catalog::ModelFeature::Temperature,
            crate::model_catalog::ModelFeature::Attachment,
            crate::model_catalog::ModelFeature::Reasoning,
        ];
        assert_eq!(option.feature_icons(), "⚒︎ ◉ ∴");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSelection {
    pub provider_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    pub provider_id: String,
    pub connection_id: String,
    pub model_id: Option<String>,
    pub model: String,
    pub selection_input: Option<String>,
    pub reasoning: ReasoningSelection,
}
