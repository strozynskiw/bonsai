use std::collections::{BTreeMap, HashMap, HashSet};

use serde::de;
use serde::{Deserialize, Deserializer};

use crate::provider::{
    ModelPricing, ModelPricingSchedule, ModelPricingTier, ReasoningEffort, ReasoningOption,
    ReasoningSelection,
};

use super::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelsDevCatalog {
    models: HashMap<ModelId, ModelsDevModel>,
}

impl ModelsDevCatalog {
    pub(crate) fn len(&self) -> usize {
        self.models.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub(crate) fn model(&self, id: &ModelId) -> Option<&ModelsDevModel> {
        self.models.get(id)
    }

    fn insert_with_direct_provider_precedence(&mut self, provider_id: &str, model: ModelsDevModel) {
        let is_direct_provider = model.id.catalog_provider() == provider_id;
        if is_direct_provider {
            self.models.insert(model.id.clone(), model);
        } else {
            self.models.entry(model.id.clone()).or_insert(model);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModelModalities {
    pub input: Vec<Box<str>>,
    pub output: Vec<Box<str>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelsDevModel {
    pub id: ModelId,
    pub display_name: Box<str>,
    pub family: Option<Box<str>>,
    pub release_date: Option<Box<str>>,
    pub last_updated: Option<Box<str>>,
    pub status: Option<Box<str>>,
    pub reasoning: bool,
    pub reasoning_options: Vec<ReasoningOption>,
    pub tool_call: bool,
    pub structured_output: bool,
    pub temperature: bool,
    pub attachment: bool,
    pub modalities: ModelModalities,
    pub context_window: Option<u32>,
    pub output_limit: Option<u32>,
    pub pricing: Option<ModelPricing>,
    context_pricing: Vec<ModelPricingTier>,
}

impl ModelsDevModel {
    pub(crate) fn features(&self) -> Vec<ModelFeature> {
        let mut features = Vec::new();
        if self.tool_call {
            features.push(ModelFeature::ToolCall);
        }
        if self.reasoning {
            features.push(ModelFeature::Reasoning);
        }
        if self.structured_output {
            features.push(ModelFeature::StructuredOutput);
        }
        if self.temperature {
            features.push(ModelFeature::Temperature);
        }
        // Vision from either signal: models.dev entries are sometimes
        // internally inconsistent (e.g. `attachment: false` with `image` in
        // input modalities — kimi-for-coding/k3). A false positive surfaces
        // as a provider-side error; a false negative silently drops images,
        // so trust whichever field claims support.
        if self.attachment || self.accepts_image_input() {
            features.push(ModelFeature::Attachment);
        }
        features
    }

    fn accepts_image_input(&self) -> bool {
        self.modalities
            .input
            .iter()
            .any(|modality| modality.as_ref() == "image")
    }

    pub(crate) fn reasoning_options_for_transport(
        &self,
        transport: TransportProtocol,
    ) -> Vec<ReasoningOption> {
        if !self.reasoning {
            return Vec::new();
        }

        reasoning_options_for_transport(&self.reasoning_options, transport)
    }

    /// Return the display price for a configured context-window profile.
    #[cfg(test)]
    pub(crate) fn pricing_for_context_window(
        &self,
        context_window: Option<u32>,
    ) -> Option<ModelPricing> {
        self.pricing_schedule()
            .map(|schedule| schedule.pricing_for_context_window(context_window))
    }

    pub(crate) fn pricing_schedule(&self) -> Option<ModelPricingSchedule> {
        self.pricing
            .map(|base| ModelPricingSchedule::new(base, self.context_pricing.clone()))
    }
}

pub(crate) fn reasoning_selections_from_options(
    options: &[ReasoningOption],
) -> Vec<ReasoningSelection> {
    let mut has_toggle = false;
    let mut efforts = Vec::new();
    let mut budgets = Vec::new();
    for option in options {
        match option {
            ReasoningOption::Toggle => has_toggle = true,
            ReasoningOption::Effort(values) => {
                efforts.extend(values.iter().copied().map(ReasoningSelection::from_effort));
            }
            ReasoningOption::BudgetTokens { default, .. } => {
                budgets.push(ReasoningSelection::BudgetTokens(*default));
            }
            ReasoningOption::Unknown(_) => {}
        }
    }

    let mut selections = vec![ReasoningSelection::Default];
    if has_toggle {
        selections.push(ReasoningSelection::Off);
    }
    if has_toggle && efforts.is_empty() && budgets.is_empty() {
        selections.push(ReasoningSelection::On);
    }
    selections.extend(efforts);
    selections.extend(budgets);
    dedup_reasoning(selections)
}

pub(crate) fn reasoning_options_for_transport(
    options: &[ReasoningOption],
    transport: TransportProtocol,
) -> Vec<ReasoningOption> {
    options
        .iter()
        .filter_map(|option| match (transport, option) {
            (
                TransportProtocol::OpenAiChat,
                ReasoningOption::Toggle | ReasoningOption::Effort(_),
            ) => Some(option.clone()),
            // The public OpenAI API accepts `none`, which models.dev converts
            // to a toggle. ChatGPT-authenticated Codex advertises effort levels
            // only; omitting the field selects its default rather than turning
            // reasoning off.
            (TransportProtocol::CodexResponses, ReasoningOption::Effort(_)) => Some(option.clone()),
            // Efforts are valid on the Anthropic wire under BOTH codecs: the
            // budget-tokens generation maps them to coarse budgets
            // (`reasoning::anthropic_thinking`), and the adaptive generation
            // (claude-sonnet-4-6+) sends them via `output_config.effort`.
            // Dropping them here silently erased effort menus that targets
            // (e.g. MiniMax M2.5) explicitly declare.
            (
                TransportProtocol::AnthropicMessages,
                ReasoningOption::Toggle
                | ReasoningOption::Effort(_)
                | ReasoningOption::BudgetTokens { .. },
            ) => Some(option.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn default_budget_tokens(min: Option<u32>, max: Option<u32>) -> u32 {
    let default = min.unwrap_or(4096).max(4096);
    max.map(|max| default.min(max)).unwrap_or(default)
}

#[derive(Debug, Deserialize)]
struct RawModelsDevModel {
    id: Box<str>,
    #[serde(default)]
    name: Option<Box<str>>,
    #[serde(default)]
    family: Option<Box<str>>,
    #[serde(default)]
    release_date: Option<Box<str>>,
    #[serde(default)]
    last_updated: Option<Box<str>>,
    #[serde(default)]
    status: Option<Box<str>>,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    reasoning_options: Vec<RawModelsDevReasoningOption>,
    #[serde(default)]
    experimental: RawModelsDevExperimental,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    structured_output: bool,
    #[serde(default)]
    temperature: bool,
    #[serde(default)]
    attachment: bool,
    #[serde(default)]
    modalities: RawModelsDevModalities,
    #[serde(default)]
    limit: RawModelsDevLimit,
    #[serde(default)]
    cost: Option<RawModelsDevCost>,
}

impl RawModelsDevModel {
    fn into_model(
        self,
        source_name: &str,
        provider_id: &str,
    ) -> Result<ModelsDevModel, CatalogError> {
        let model_id = models_dev_model_id(source_name, provider_id, &self.id)?;
        let display_name = self.name.unwrap_or_else(|| model_id.model().into());
        let (pricing, context_pricing) = self
            .cost
            .map(RawModelsDevCost::into_pricing)
            .unwrap_or_default();
        // A zero limit is publisher noise, not a real window — treat it as
        // unknown (the live-availability path applies the same `> 0` floor).
        let context_window = self.limit.context.filter(|value| *value > 0);
        let output_limit = self.limit.output.filter(|value| *value > 0);
        let mode_reasoning = self.experimental.reasoning_parts();
        let reasoning_options = own_reasoning_options(
            self.reasoning_options
                .into_iter()
                .flat_map(RawModelsDevReasoningOption::into_options)
                .collect(),
            mode_reasoning,
        );

        Ok(ModelsDevModel {
            id: model_id,
            display_name,
            family: self.family,
            release_date: self.release_date,
            last_updated: self.last_updated,
            status: self.status,
            reasoning: self.reasoning,
            reasoning_options,
            tool_call: self.tool_call,
            structured_output: self.structured_output,
            temperature: self.temperature,
            attachment: self.attachment,
            modalities: ModelModalities {
                input: self.modalities.input,
                output: self.modalities.output,
            },
            context_window,
            output_limit,
            pricing,
            context_pricing,
        })
    }
}

/// Reasoning options that a model's own Models.dev row declares: its explicit
/// `reasoning_options`, or failing that whatever its experimental `modes` imply.
fn own_reasoning_options(
    options: Vec<ReasoningOption>,
    mode_reasoning: ReasoningParts,
) -> Vec<ReasoningOption> {
    if !options.is_empty() {
        return options;
    }
    reasoning_options_from_parts(mode_reasoning)
}

fn effort_rank(effort: ReasoningEffort) -> u8 {
    match effort {
        ReasoningEffort::Minimal => 0,
        ReasoningEffort::Low => 1,
        ReasoningEffort::Medium => 2,
        ReasoningEffort::High => 3,
        ReasoningEffort::XHigh => 4,
        ReasoningEffort::Max => 5,
        ReasoningEffort::Ultra => 6,
    }
}

fn effort_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Minimal => "minimal",
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
        ReasoningEffort::XHigh => "xhigh",
        ReasoningEffort::Max => "max",
        ReasoningEffort::Ultra => "ultra",
    }
}

/// Re-emit options with their efforts deduped and in ascending order, so two
/// providers that list the same capabilities compare equal regardless of order.
fn canonicalize_reasoning_options(options: &[ReasoningOption]) -> Vec<ReasoningOption> {
    options
        .iter()
        .map(|option| match option {
            ReasoningOption::Effort(efforts) => {
                let mut efforts = dedup_efforts(efforts.clone());
                efforts.sort_by_key(|effort| effort_rank(*effort));
                ReasoningOption::Effort(efforts)
            }
            other => other.clone(),
        })
        .collect()
}

/// Stable signature used to group equal option sets and break ties.
fn reasoning_options_signature(options: &[ReasoningOption]) -> String {
    canonicalize_reasoning_options(options)
        .into_iter()
        .map(|option| match option {
            ReasoningOption::Toggle => "toggle".to_string(),
            ReasoningOption::Effort(efforts) => {
                let labels: Vec<&str> =
                    efforts.iter().map(|effort| effort_label(*effort)).collect();
                format!("e:{}", labels.join(","))
            }
            ReasoningOption::BudgetTokens { min, max, .. } => {
                let render = |value: Option<u32>| {
                    value
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "None".to_string())
                };
                format!("b:{}-{}", render(min), render(max))
            }
            ReasoningOption::Unknown(name) => name.to_string(),
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// For each bare model name, the reasoning options that the most direct/home
/// providers agree on. Lets a model whose own row is silent reuse what
/// authoritative providers publish for the same model, instead of guessing.
fn build_home_reasoning(
    models: &[(String, ModelsDevModel)],
) -> HashMap<String, Vec<ReasoningOption>> {
    type Group = (Vec<ReasoningOption>, HashSet<String>);
    let mut candidates: HashMap<String, HashMap<String, Group>> = HashMap::new();
    for (provider_id, model) in models {
        if model.id.catalog_provider() != provider_id {
            continue; // router/reseller copy, not an authoritative source
        }
        if model.reasoning_options.is_empty() {
            continue;
        }
        let canonical = canonicalize_reasoning_options(&model.reasoning_options);
        let signature = reasoning_options_signature(&canonical);
        let group = candidates
            .entry(model.id.model().to_ascii_lowercase())
            .or_default()
            .entry(signature)
            .or_insert_with(|| (canonical, HashSet::new()));
        group.1.insert(provider_id.clone());
    }

    candidates
        .into_iter()
        .filter_map(|(name, groups)| {
            let winner = groups.into_iter().min_by(|left, right| {
                right
                    .1
                    .1
                    .len()
                    .cmp(&left.1.1.len())
                    .then_with(|| left.0.cmp(&right.0))
            })?;
            Some((name, winner.1.0))
        })
        .collect()
}

#[derive(Debug, Default)]
struct ReasoningParts {
    has_toggle: bool,
    efforts: Vec<ReasoningEffort>,
}

impl ReasoningParts {
    fn push_label(&mut self, value: &str) {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => self.has_toggle = true,
            value => {
                if let Some(effort) = reasoning_effort_from_models_dev(value) {
                    self.efforts.push(effort);
                }
            }
        }
    }
}

fn reasoning_options_from_parts(parts: ReasoningParts) -> Vec<ReasoningOption> {
    let mut options = Vec::new();
    if parts.has_toggle {
        options.push(ReasoningOption::Toggle);
    }
    let efforts = dedup_efforts(parts.efforts);
    if !efforts.is_empty() {
        options.push(ReasoningOption::Effort(efforts));
    }
    options
}

#[derive(Debug, Default, Deserialize)]
struct RawModelsDevExperimental {
    #[serde(default)]
    modes: BTreeMap<String, RawModelsDevMode>,
}

impl RawModelsDevExperimental {
    fn reasoning_parts(&self) -> ReasoningParts {
        let mut parts = ReasoningParts::default();
        for (mode, item) in &self.modes {
            parts.push_label(mode);
            item.provider.append_reasoning_parts(&mut parts);
        }
        parts
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawModelsDevMode {
    #[serde(default)]
    provider: RawModelsDevModeProvider,
}

#[derive(Debug, Default, Deserialize)]
struct RawModelsDevModeProvider {
    #[serde(default)]
    body: BTreeMap<String, serde_json::Value>,
}

impl RawModelsDevModeProvider {
    fn append_reasoning_parts(&self, parts: &mut ReasoningParts) {
        for key in ["reasoningEffort", "reasoning_effort", "effort"] {
            if let Some(value) = self.body.get(key).and_then(reasoning_label_from_json) {
                parts.push_label(value);
            }
        }
        for key in ["reasoning", "thinking", "output_config", "outputConfig"] {
            if let Some(value) = self.body.get(key).and_then(reasoning_label_from_json) {
                parts.push_label(value);
            }
        }
    }
}

fn reasoning_label_from_json(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::String(value) => Some(value.as_str()),
        serde_json::Value::Object(object) => [
            "effort",
            "reasoningEffort",
            "reasoning_effort",
            "thinkingLevel",
            "thinking_level",
        ]
        .into_iter()
        .find_map(|key| object.get(key).and_then(reasoning_label_from_json)),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct RawModelsDevReasoningOption {
    #[serde(rename = "type")]
    option_type: Box<str>,
    #[serde(default)]
    values: Vec<Option<Box<str>>>,
    #[serde(default, deserialize_with = "deserialize_optional_non_negative_u32")]
    min: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_optional_non_negative_u32")]
    max: Option<u32>,
}

impl RawModelsDevReasoningOption {
    fn into_options(self) -> Vec<ReasoningOption> {
        match self.option_type.as_ref() {
            "toggle" => vec![ReasoningOption::Toggle],
            "effort" => {
                let has_toggle = self
                    .values
                    .iter()
                    .any(|value| matches!(value.as_deref(), None | Some("none" | "off")));
                let efforts = dedup_efforts(
                    self.values
                        .iter()
                        .filter_map(|value| {
                            value.as_deref().and_then(reasoning_effort_from_models_dev)
                        })
                        .collect(),
                );
                let mut options = Vec::new();
                if has_toggle {
                    options.push(ReasoningOption::Toggle);
                }
                if !efforts.is_empty() {
                    options.push(ReasoningOption::Effort(efforts));
                }
                options
            }
            "budget_tokens" => vec![ReasoningOption::BudgetTokens {
                min: self.min,
                max: self.max,
                default: default_budget_tokens(self.min, self.max),
            }],
            _ => vec![ReasoningOption::Unknown(self.option_type)],
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawModelsDevModalities {
    #[serde(default)]
    input: Vec<Box<str>>,
    #[serde(default)]
    output: Vec<Box<str>>,
}

#[derive(Debug, Default, Deserialize)]
struct RawModelsDevLimit {
    #[serde(default, deserialize_with = "deserialize_optional_non_negative_u32")]
    context: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_optional_non_negative_u32")]
    output: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct RawModelsDevCost {
    #[serde(flatten)]
    rates: RawModelsDevRates,
    #[serde(default)]
    tiers: Vec<RawModelsDevCostTier>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct RawModelsDevRates {
    #[serde(
        default,
        rename = "input",
        deserialize_with = "deserialize_optional_usd_per_million_micros"
    )]
    input_micros_per_million: Option<u64>,
    #[serde(
        default,
        rename = "output",
        deserialize_with = "deserialize_optional_usd_per_million_micros"
    )]
    output_micros_per_million: Option<u64>,
    #[serde(
        default,
        rename = "cache_read",
        deserialize_with = "deserialize_optional_usd_per_million_micros"
    )]
    cache_read_micros_per_million: Option<u64>,
    #[serde(
        default,
        rename = "cache_write",
        deserialize_with = "deserialize_optional_usd_per_million_micros"
    )]
    cache_write_micros_per_million: Option<u64>,
}

impl RawModelsDevRates {
    fn pricing(self) -> Option<ModelPricing> {
        Some(
            ModelPricing::new(
                self.input_micros_per_million?,
                self.output_micros_per_million?,
            )
            .with_cache_rates(
                self.cache_read_micros_per_million,
                self.cache_write_micros_per_million,
            ),
        )
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawModelsDevCostTier {
    #[serde(flatten)]
    rates: RawModelsDevRates,
    #[serde(default)]
    tier: Option<RawModelsDevTierSelector>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawModelsDevTierSelector {
    #[serde(rename = "type")]
    kind: Box<str>,
    #[serde(default, deserialize_with = "deserialize_optional_non_negative_u32")]
    size: Option<u32>,
}

impl RawModelsDevCost {
    fn into_pricing(self) -> (Option<ModelPricing>, Vec<ModelPricingTier>) {
        let pricing = self.rates.pricing();
        let mut context_pricing = self
            .tiers
            .into_iter()
            .filter_map(|tier| {
                let selector = tier.tier?;
                if selector.kind.as_ref() != "context" {
                    return None;
                }
                let above_tokens = selector.size.filter(|size| *size > 0)?;
                let pricing = tier.rates.pricing()?;
                Some(ModelPricingTier {
                    above_input_tokens: above_tokens,
                    pricing,
                })
            })
            .collect::<Vec<_>>();
        context_pricing.sort_by_key(|tier| tier.above_input_tokens);
        (pricing, context_pricing)
    }
}

pub(crate) fn parse_models_dev_catalog(
    source_name: &str,
    content: &str,
) -> Result<ModelsDevCatalog, CatalogError> {
    // Only a corrupt envelope is fatal. Each provider block and each model row
    // deserializes independently so one malformed row (a bad price string, a
    // wrong-typed limit) skips that row instead of discarding the entire
    // catalog — which previously meant silently running on a stale cache.
    let providers: BTreeMap<String, serde_json::Value> =
        serde_json::from_str(content).map_err(|err| CatalogError::ModelsDevJson {
            source_name: source_name.to_string(),
            source: err,
        })?;
    let mut parsed: Vec<(String, ModelsDevModel)> = Vec::new();
    let mut skipped = 0usize;
    let mut inverted_limit_count = 0usize;
    let mut inverted_limit_examples = Vec::new();
    for (provider_id, provider) in providers {
        let Some(models) = provider
            .get("models")
            .and_then(serde_json::Value::as_object)
        else {
            tracing::debug!(
                provider = %provider_id,
                "skipping Models.dev provider block without a models object"
            );
            continue;
        };
        for (model_key, raw_value) in models {
            let raw_model: RawModelsDevModel = match serde_json::from_value(raw_value.clone()) {
                Ok(raw_model) => raw_model,
                Err(err) => {
                    skipped += 1;
                    tracing::debug!(
                        provider = %provider_id,
                        model = %model_key,
                        error = %err,
                        "skipping malformed Models.dev model row"
                    );
                    continue;
                }
            };
            match raw_model.into_model(source_name, &provider_id) {
                Ok(model) => {
                    if matches!(
                        (model.context_window, model.output_limit),
                        (Some(context), Some(output)) if output > context
                    ) {
                        inverted_limit_count = inverted_limit_count.saturating_add(1);
                        if inverted_limit_examples.len() < 3 {
                            inverted_limit_examples.push(model.id.to_string());
                        }
                    }
                    parsed.push((provider_id.clone(), model));
                }
                Err(err @ CatalogError::InvalidModelsDevModelId { .. }) => {
                    skipped += 1;
                    tracing::debug!(error = %err, "skipping invalid Models.dev model row");
                }
                Err(err) => return Err(err),
            }
        }
    }

    if inverted_limit_count > 0 {
        tracing::warn!(
            affected_models = inverted_limit_count,
            examples = %inverted_limit_examples.join(", "),
            "Models.dev rows report output limits above their context windows"
        );
    }

    // When a model's own row is silent on reasoning, reuse what authoritative
    // providers publish for the same model rather than leaving it blank.
    let home_reasoning = build_home_reasoning(&parsed);
    let mut catalog = ModelsDevCatalog::default();
    for (provider_id, mut model) in parsed {
        if model.reasoning
            && model.reasoning_options.is_empty()
            && let Some(options) = home_reasoning.get(&model.id.model().to_ascii_lowercase())
        {
            model.reasoning_options = options.clone();
        }
        catalog.insert_with_direct_provider_precedence(&provider_id, model);
    }

    if skipped > 0 {
        tracing::debug!(
            skipped_models = skipped,
            imported_models = catalog.len(),
            "loaded Models.dev catalog with skipped model rows"
        );
    }
    Ok(catalog)
}

fn models_dev_model_id(
    source_name: &str,
    provider_id: &str,
    model_id: &str,
) -> Result<ModelId, CatalogError> {
    let canonical = if model_id.contains('/') {
        model_id.to_string()
    } else {
        format!("{provider_id}/{model_id}")
    };
    canonical
        .parse()
        .map_err(|source| CatalogError::InvalidModelsDevModelId {
            source_name: source_name.to_string(),
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            source,
        })
}

fn reasoning_effort_from_models_dev(value: &str) -> Option<ReasoningEffort> {
    match value.trim().to_ascii_lowercase().as_str() {
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" | "x-high" => Some(ReasoningEffort::XHigh),
        "max" => Some(ReasoningEffort::Max),
        "ultra" => Some(ReasoningEffort::Ultra),
        "none" | "off" | "auto" | "default" | "" => None,
        _ => None,
    }
}

pub(crate) fn dedup_reasoning(values: Vec<ReasoningSelection>) -> Vec<ReasoningSelection> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(*value))
        .collect()
}

pub(crate) fn dedup_efforts(values: Vec<ReasoningEffort>) -> Vec<ReasoningEffort> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(*value))
        .collect()
}

pub(crate) fn deserialize_optional_non_negative_u32<'de, D>(
    deserializer: D,
) -> Result<Option<u32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<i64>::deserialize(deserializer)?;
    Ok(value.and_then(|value| u32::try_from(value).ok()))
}

fn deserialize_optional_usd_per_million_micros<'de, D>(
    deserializer: D,
) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    value
        .map(|value| usd_per_million_to_micros(&value).map_err(de::Error::custom))
        .transpose()
}

fn usd_per_million_to_micros(value: &serde_json::Value) -> Result<u64, String> {
    let price = match value {
        serde_json::Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| format!("invalid price `{number}`"))?,
        serde_json::Value::String(value) => value
            .parse::<f64>()
            .map_err(|err| format!("invalid price `{value}`: {err}"))?,
        other => return Err(format!("invalid price `{other}`")),
    };
    if !price.is_finite() || price < 0.0 {
        return Err(format!("invalid price `{price}`"));
    }
    let micros = (price * 1_000_000.0).round();
    if micros > u64::MAX as f64 {
        return Err(format!("price `{price}` is too large"));
    }
    Ok(micros as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silent_models_inherit_reasoning_from_home_providers() {
        // gpt-5.2 is silent on opencode's row but specified by the direct
        // providers (openai, azure); a router copy must not get a vote.
        // glm-5.2 has a dominant [high, max] across direct providers plus one
        // outlier — the majority should win.
        let catalog = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "openai": { "models": { "gpt-5.2": {
                "id": "gpt-5.2", "reasoning": true,
                "reasoning_options": [{"type": "effort", "values": ["none", "low", "medium", "high", "xhigh"]}]
              }}},
              "azure": { "models": { "gpt-5.2": {
                "id": "gpt-5.2", "reasoning": true,
                "reasoning_options": [{"type": "effort", "values": ["none", "low", "medium", "high", "xhigh"]}]
              }}},
              "router": { "models": { "openai/gpt-5.2": {
                "id": "openai/gpt-5.2", "reasoning": true,
                "reasoning_options": [{"type": "effort", "values": ["low"]}]
              }}},
              "opencode": { "models": { "gpt-5.2": { "id": "gpt-5.2", "reasoning": true } } },
              "zhipuai": { "models": { "glm-5.2": {
                "id": "glm-5.2", "reasoning": true,
                "reasoning_options": [{"type": "effort", "values": ["high", "max"]}]
              }}},
              "zai": { "models": { "glm-5.2": {
                "id": "glm-5.2", "reasoning": true,
                "reasoning_options": [{"type": "effort", "values": ["high", "max"]}]
              }}},
              "outlier": { "models": { "glm-5.2": {
                "id": "glm-5.2", "reasoning": true,
                "reasoning_options": [{"type": "effort", "values": ["minimal", "low"]}]
              }}},
              "opencode-go": { "models": { "glm-5.2": { "id": "glm-5.2", "reasoning": true } } },
              "vendor": { "models": { "plain": { "id": "plain", "reasoning": false } } }
            }
            "#,
        )
        .unwrap();

        let gpt = catalog.model(&model_id("opencode/gpt-5.2")).unwrap();
        assert_eq!(
            gpt.reasoning_options,
            vec![
                ReasoningOption::Toggle,
                ReasoningOption::Effort(vec![
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::XHigh,
                ])
            ]
        );

        let glm = catalog.model(&model_id("opencode-go/glm-5.2")).unwrap();
        assert_eq!(
            glm.reasoning_options,
            vec![ReasoningOption::Effort(vec![
                ReasoningEffort::High,
                ReasoningEffort::Max,
            ])]
        );

        // A silent, non-reasoning model gets nothing.
        let plain = catalog.model(&model_id("vendor/plain")).unwrap();
        assert!(plain.reasoning_options.is_empty());
    }

    fn model_id(value: &str) -> ModelId {
        value.parse().unwrap()
    }

    #[test]
    fn models_dev_parser_skips_malformed_rows_without_discarding_catalog() {
        // One bad price string and one wrong-typed row must not take down the
        // rows around them (previously the whole catalog failed and bonsai
        // silently ran on a stale cache).
        let catalog = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "openai": {
                "models": {
                  "good": { "id": "good", "limit": { "context": 400000 } },
                  "bad-price": { "id": "bad-price", "cost": { "input": "not-a-number", "output": 1 } },
                  "bad-shape": { "id": "bad-shape", "limit": "huge" }
                }
              },
              "not-a-provider": 42
            }
            "#,
        )
        .unwrap();

        assert_eq!(catalog.len(), 1);
        assert!(catalog.model(&model_id("openai/good")).is_some());
    }

    #[test]
    fn models_dev_parser_treats_zero_limits_as_unknown() {
        let catalog = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "openai": {
                "models": {
                  "zeroed": { "id": "zeroed", "limit": { "context": 0, "output": 0 } }
                }
              }
            }
            "#,
        )
        .unwrap();

        let model = catalog.model(&model_id("openai/zeroed")).unwrap();
        assert_eq!(model.context_window, None);
        assert_eq!(model.output_limit, None);
    }

    #[test]
    fn models_dev_parser_accepts_missing_optional_fields_and_converts_costs() {
        let catalog = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "empty": {
                "models": {
                  "tiny": { "id": "tiny" }
                }
              },
              "openai": {
                "models": {
                  "gpt-5": {
                    "id": "gpt-5",
                    "name": "GPT-5",
                    "family": "gpt",
                    "reasoning": true,
                    "tool_call": true,
                    "structured_output": true,
                    "temperature": false,
                    "attachment": true,
                    "release_date": "2025-08-07",
                    "last_updated": "2025-08-07",
                    "modalities": {
                      "input": ["text", "image"],
                      "output": ["text"]
                    },
                    "limit": { "context": 400000, "output": 128000 },
                    "cost": {
                      "input": 1.25,
                      "output": 10,
                      "cache_read": 0.125,
                      "cache_write": "2.5"
                    }
                  }
                }
              }
            }
            "#,
        )
        .unwrap();

        assert_eq!(catalog.len(), 2);

        let gpt = catalog.model(&model_id("openai/gpt-5")).unwrap();
        assert_eq!(gpt.display_name.as_ref(), "GPT-5");
        assert_eq!(gpt.family.as_deref(), Some("gpt"));
        assert_eq!(gpt.release_date.as_deref(), Some("2025-08-07"));
        assert_eq!(gpt.context_window, Some(400_000));
        assert_eq!(gpt.output_limit, Some(128_000));
        assert_eq!(
            gpt.pricing,
            Some(
                ModelPricing::new(1_250_000, 10_000_000)
                    .with_cache_rates(Some(125_000), Some(2_500_000))
            )
        );
        assert_eq!(
            gpt.features(),
            vec![
                ModelFeature::ToolCall,
                ModelFeature::Reasoning,
                ModelFeature::StructuredOutput,
                ModelFeature::Attachment
            ]
        );

        let tiny = catalog.model(&model_id("empty/tiny")).unwrap();
        assert_eq!(tiny.display_name.as_ref(), "tiny");
        assert_eq!(tiny.context_window, None);
        assert_eq!(tiny.pricing, None);
        assert!(tiny.features().is_empty());
    }

    #[test]
    fn models_dev_parser_selects_context_pricing_tiers() {
        let catalog = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "openai": {
                "models": {
                  "gpt-current": {
                    "id": "gpt-current",
                    "cost": {
                      "input": 5,
                      "output": 30,
                      "cache_read": 0.5,
                      "cache_write": 6.25,
                      "tiers": [
                        {
                          "input": 10,
                          "output": 45,
                          "cache_read": 1,
                          "cache_write": 12.5,
                          "tier": { "type": "context", "size": 272000 }
                        },
                        {
                          "input": 99,
                          "output": 99,
                          "tier": { "type": "service", "size": 1 }
                        }
                      ]
                    }
                  }
                }
              }
            }
            "#,
        )
        .unwrap();

        let model = catalog.model(&model_id("openai/gpt-current")).unwrap();
        let base = ModelPricing::new(5_000_000, 30_000_000)
            .with_cache_rates(Some(500_000), Some(6_250_000));
        let long = ModelPricing::new(10_000_000, 45_000_000)
            .with_cache_rates(Some(1_000_000), Some(12_500_000));

        assert_eq!(model.pricing_for_context_window(None), Some(base));
        assert_eq!(model.pricing_for_context_window(Some(272_000)), Some(base));
        assert_eq!(model.pricing_for_context_window(Some(272_001)), Some(long));
        assert_eq!(
            model.pricing_for_context_window(Some(1_050_000)),
            Some(long)
        );
        let schedule = model.pricing_schedule().expect("pricing schedule");
        assert_eq!(schedule.pricing_for_prompt_tokens(272_000), base);
        assert_eq!(schedule.pricing_for_prompt_tokens(272_001), long);
    }

    #[test]
    fn codex_transport_drops_public_api_reasoning_toggle() {
        let options = vec![
            ReasoningOption::Toggle,
            ReasoningOption::Effort(vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ]),
        ];

        assert_eq!(
            reasoning_options_for_transport(&options, TransportProtocol::CodexResponses),
            vec![ReasoningOption::Effort(vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ])]
        );
        assert_eq!(
            reasoning_options_for_transport(&options, TransportProtocol::OpenAiChat),
            options
        );
    }

    #[test]
    fn image_input_modality_grants_attachment_despite_false_flag() {
        // models.dev entries can be internally inconsistent — e.g.
        // kimi-for-coding/k3 ships attachment=false alongside an `image`
        // input modality. The modality must win so images aren't silently
        // blocked for a vision-capable model.
        let catalog = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "kimi-for-coding": {
                "models": {
                  "k3": {
                    "id": "k3",
                    "attachment": false,
                    "modalities": { "input": ["text", "image", "video"], "output": ["text"] }
                  },
                  "text-only": {
                    "id": "text-only",
                    "attachment": false,
                    "modalities": { "input": ["text"], "output": ["text"] }
                  }
                }
              }
            }
            "#,
        )
        .unwrap();

        let k3 = catalog.model(&model_id("kimi-for-coding/k3")).unwrap();
        assert!(k3.features().contains(&ModelFeature::Attachment));
        let text_only = catalog
            .model(&model_id("kimi-for-coding/text-only"))
            .unwrap();
        assert!(!text_only.features().contains(&ModelFeature::Attachment));
    }

    #[test]
    fn models_dev_direct_provider_facts_override_router_duplicates() {
        let catalog = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "requesty": {
                "models": {
                  "xai/grok-4": {
                    "id": "xai/grok-4",
                    "name": "Router Grok",
                    "limit": { "context": 1 }
                  }
                }
              },
              "xai": {
                "models": {
                  "grok-4": {
                    "id": "grok-4",
                    "name": "Direct Grok",
                    "limit": { "context": 256000 }
                  }
                }
              }
            }
            "#,
        )
        .unwrap();

        let model = catalog.model(&model_id("xai/grok-4")).unwrap();

        assert_eq!(model.display_name.as_ref(), "Direct Grok");
        assert_eq!(model.context_window, Some(256_000));
    }
}
