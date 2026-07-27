//! Resolves the session's current provider/model/reasoning — plus per-subagent
//! overrides — into runnable providers, run targets, context windows, and
//! prompt estimators. Extracted from `bootstrap` so startup wiring and model
//! resolution evolve separately: everything here is pure
//! resolution over the registry/session/catalog triple, with no terminal or
//! storage concerns.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::model_catalog::{
    ConnectionId, ModelCatalog, ModelFeature, ModelId, ResolvedModel, RunTarget,
    connection_id_for_provider_id,
};
use crate::provider::{DEFAULT_CONTEXT_WINDOW_TOKENS, PromptEstimator, Provider, ProviderRegistry};
use crate::session::SessionStore;
use crate::tool::{ProjectInfoProviderState, SubagentProviderConfig};

pub(crate) fn project_info_provider_state(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
) -> ProjectInfoProviderState {
    let id = session_store.current_kind_id();
    let label = registry
        .lookup(id)
        .map(|factory| factory.metadata().display_name.as_ref())
        .unwrap_or(id);
    ProjectInfoProviderState::new(id, label, &session_store.current_session().model)
}

/// The provider/model identity the session currently runs against, for
/// per-turn usage attribution. Always sourced from the same session state as
/// [`build_provider`] so the stamped identity matches the provider actually
/// built.
pub(crate) fn active_model_identity(
    session_store: &SessionStore,
) -> crate::agent::ActiveModelIdentity {
    crate::agent::ActiveModelIdentity {
        provider_id: session_store
            .current_kind_id()
            .parse()
            .unwrap_or_else(|_| ConnectionId::fallback("unknown")),
        model: session_store.current_session().model.clone(),
    }
}

pub(crate) fn build_provider(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
) -> Box<dyn Provider> {
    let id = session_store.current_kind_id();
    let factory = registry.get(id).unwrap_or_else(|| {
        registry
            .get(crate::session::DEFAULT_PROVIDER_ID)
            .expect("default provider must exist")
    });
    let session = session_store.session(id);
    let target = run_target_for_current_model_with_catalog(registry, session_store, catalog);
    factory.build_target(session, &target)
}

/// Like [`build_provider`] but caps the reasoning effort at Medium for internal
/// delegation lanes (self-review + built-in explore/review/research/security): they do
/// focused, read-only work where high effort mostly buys latency and cold-cache
/// cost, not review quality. Never upshifts a lower session effort.
fn build_capped_provider(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
) -> Box<dyn Provider> {
    let id = session_store.current_kind_id();
    let factory = registry.get(id).unwrap_or_else(|| {
        registry
            .get(crate::session::DEFAULT_PROVIDER_ID)
            .expect("default provider must exist")
    });
    let session = session_store.session(id);
    let target = run_target_for_current_model_with_catalog_and_reasoning_cap(
        registry,
        session_store,
        catalog,
        Some(crate::provider::ReasoningSelection::Medium),
    );
    factory.build_target(session, &target)
}

/// Effort ceiling for internal lanes: high tiers drop to Medium; everything at
/// or below Medium (and non-effort selections like Default/Off) is left alone.
/// If Medium is unsupported, choose the strongest supported lower option; if
/// none exists, fall back to Default rather than emitting an unsupported value.
fn cap_internal_lane_effort(
    reasoning: crate::provider::ReasoningSelection,
    cap: crate::provider::ReasoningSelection,
    supported: &[crate::provider::ReasoningSelection],
) -> crate::provider::ReasoningSelection {
    let Some(reasoning_rank) = effort_rank(reasoning) else {
        return reasoning;
    };
    let Some(cap_rank) = effort_rank(cap) else {
        return reasoning;
    };
    if reasoning_rank <= cap_rank {
        return reasoning;
    }

    supported
        .iter()
        .copied()
        .filter_map(|candidate| {
            cap_candidate_rank(candidate)
                .filter(|rank| *rank <= cap_rank)
                .map(|rank| (rank, candidate))
        })
        .max_by_key(|(rank, _)| *rank)
        .map(|(_, candidate)| candidate)
        .unwrap_or_default()
}

fn effort_rank(reasoning: crate::provider::ReasoningSelection) -> Option<u8> {
    use crate::provider::ReasoningSelection;
    match reasoning {
        ReasoningSelection::Minimal => Some(1),
        ReasoningSelection::Low => Some(2),
        ReasoningSelection::Medium => Some(3),
        ReasoningSelection::High => Some(4),
        ReasoningSelection::XHigh => Some(5),
        ReasoningSelection::Max => Some(6),
        ReasoningSelection::Ultra => Some(7),
        _ => None,
    }
}

fn cap_candidate_rank(reasoning: crate::provider::ReasoningSelection) -> Option<u8> {
    use crate::provider::ReasoningSelection;
    match reasoning {
        ReasoningSelection::Off => Some(0),
        other => effort_rank(other),
    }
}

/// The built-in read-only lanes and the self-review reviewer, which default to
/// capped effort. Custom user agents opt into effort via frontmatter and are not
/// capped here.
fn is_internal_lane(agent: &str) -> bool {
    matches!(
        agent,
        "self-review" | "explore" | "review" | "research" | "security-review"
    )
}

/// Build a provider for a one-shot completion (e.g. the agent-composer prompt
/// generator). `model` accepts the same selector as custom agents: a one-letter
/// model shortcut or a concrete model. `None` uses the session's current model
/// directly.
pub(crate) fn build_provider_for_optional_model(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
    model: Option<&str>,
) -> Box<dyn Provider> {
    match model {
        None => build_provider(registry, session_store, catalog),
        Some(model) => {
            let override_ = crate::subagent::SubagentModelOverride::selector(
                model.to_string(),
                crate::provider::ReasoningSelection::Default,
            );
            run_config_for(registry, session_store, catalog, Some(&override_), false).provider
        }
    }
}

/// Capture a factory that mints providers and model-budget settings for nested
/// subagents. Honors a live per-agent provider/model override (set from
/// `/subagents`, keyed by agent name); absent an override it matches the parent's
/// current provider/model.
pub(crate) fn subagent_provider_factory(
    registry: Arc<ProviderRegistry>,
    session_store: Arc<Mutex<SessionStore>>,
    catalog: Option<Arc<ModelCatalog>>,
    overrides: crate::subagent::SubagentModelOverrides,
) -> crate::tool::SubagentProviderFactory {
    Arc::new(
        move |agent: String, frontmatter: crate::subagent::SubagentModelChain| {
            let registry = registry.clone();
            let session_store = session_store.clone();
            let catalog = catalog.clone();
            let overrides = overrides.clone();
            Box::pin(async move {
                // Precedence: a live `/subagents` override wins; otherwise the agent's
                // declared frontmatter override; otherwise the parent's current model.
                // Read + drop the lock before awaiting.
                let live_override = overrides
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&agent)
                    .cloned();
                let persisted = if let Some(live) = live_override {
                    crate::subagent::SubagentModelChain::single(Some(live))
                } else {
                    frontmatter
                };
                let session_store = session_store.lock().await;
                let resolved_primary = persisted.primary.as_ref().and_then(|assignment| {
                    try_run_config_for_override(
                        &registry,
                        &session_store,
                        catalog.as_deref(),
                        assignment,
                    )
                });
                let resolved_backup = persisted.backup.as_ref().and_then(|assignment| {
                    try_run_config_for_override(
                        &registry,
                        &session_store,
                        catalog.as_deref(),
                        assignment,
                    )
                });
                // An unavailable persisted primary promotes the backup before
                // inheritance.
                let inherited_parent = resolved_primary.is_none() && resolved_backup.is_none();
                let (primary, backup) = match (resolved_primary, resolved_backup) {
                    (Some(primary), backup) => (primary, backup),
                    (None, Some(backup)) => (backup, None),
                    (None, None) => (
                        parent_run_config(
                            &registry,
                            &session_store,
                            catalog.as_deref(),
                            persisted.primary.is_none() && is_internal_lane(&agent),
                        ),
                        None,
                    ),
                };
                // Runtime failover bottoms out at the parent's current model: if a
                // subagent's assigned primary AND backup both fail mid-run, it
                // inherits the parent instead of dying. Skipped when the chain
                // already resolved to the inherited parent above, so the terminal
                // model is never chained to a duplicate of itself.
                let backup = if inherited_parent {
                    backup
                } else {
                    let parent = parent_run_config(
                        &registry,
                        &session_store,
                        catalog.as_deref(),
                        is_internal_lane(&agent),
                    );
                    Some(match backup {
                        Some(existing) => existing.with_backup(Some(parent)),
                        None => parent,
                    })
                };
                primary.with_backup(backup)
            })
        },
    )
}

/// Build the provider + context budget + estimator for a subagent run.
/// Live `/subagents` overrides pin an exact provider/model. Frontmatter
/// `model_override.model` is a selector: a one-letter model shortcut first,
/// then a concrete `/model`-style model selector. Unbound shortcuts and
/// unresolved concrete selectors fall back to the parent's current model.
fn run_config_for(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
    model_override: Option<&crate::subagent::SubagentModelOverride>,
    cap_effort_to_medium: bool,
) -> SubagentProviderConfig {
    if let Some(override_) = model_override
        && let Some(config) =
            try_run_config_for_override(registry, session_store, catalog, override_)
    {
        return config;
    }

    parent_run_config(registry, session_store, catalog, cap_effort_to_medium)
}

fn try_run_config_for_override(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
    override_: &crate::subagent::SubagentModelOverride,
) -> Option<SubagentProviderConfig> {
    if let Some(provider_id) = override_.provider_id.as_deref() {
        if let Some(config) = run_config_for_selected_model(
            registry,
            session_store,
            catalog,
            SubagentModelSelection {
                provider_id,
                connection_id: override_.connection_id.as_ref(),
                model_id: override_.model_id.as_ref(),
                model: &override_.model,
                reasoning: override_.reasoning,
            },
        ) {
            return Some(config);
        }
    } else if let Ok(key) = override_
        .model
        .parse::<crate::model_role::ModelShortcutKey>()
    {
        let config =
            crate::model_role::resolve_model_shortcut(registry, session_store, catalog, key)
                .and_then(|selection| {
                    run_config_for_selected_model(
                        registry,
                        session_store,
                        catalog,
                        SubagentModelSelection {
                            provider_id: selection.provider_id.as_str(),
                            connection_id: selection.connection_id.as_ref(),
                            model_id: selection.model_id.as_ref(),
                            model: &selection.model,
                            reasoning: selection.reasoning,
                        },
                    )
                });
        if let Some(config) = config {
            return Some(config);
        }
    } else {
        let config = crate::commands::resolve_model_selection(
            registry,
            session_store,
            catalog,
            &override_.model,
        )
        .and_then(|selection| {
            run_config_for_selected_model(
                registry,
                session_store,
                catalog,
                SubagentModelSelection {
                    provider_id: &selection.provider_id,
                    connection_id: selection.connection_id.as_ref(),
                    model_id: selection.model_id.as_ref(),
                    model: &selection.model,
                    reasoning: override_.reasoning,
                },
            )
        });
        if let Some(config) = config {
            return Some(config);
        }
    }

    None
}

fn parent_run_config(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
    cap_effort_to_medium: bool,
) -> SubagentProviderConfig {
    let id = session_store.current_kind_id();
    let session = session_store.session(id);
    // Inheriting the parent model: cap effort for internal lanes so a high
    // session doesn't run every review/explore delegation at cold-cache high.
    let provider = if cap_effort_to_medium {
        build_capped_provider(registry, session_store, catalog)
    } else {
        build_provider(registry, session_store, catalog)
    };
    let context_budget_tokens =
        context_window_for_current_model_with_catalog(registry, session_store, catalog) as usize;
    let prompt_estimator =
        prompt_estimator_for_current_model_with_catalog(registry, session_store, catalog);
    SubagentProviderConfig::new(
        provider,
        context_budget_tokens,
        prompt_estimator,
        crate::subagent::provider_model_label(id, &session.model),
    )
    .with_active_model_identity(id, &session.model)
}

struct SubagentModelSelection<'a> {
    provider_id: &'a str,
    connection_id: Option<&'a ConnectionId>,
    model_id: Option<&'a ModelId>,
    model: &'a str,
    reasoning: crate::provider::ReasoningSelection,
}

fn run_config_for_selected_model(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
    selection: SubagentModelSelection<'_>,
) -> Option<SubagentProviderConfig> {
    let factory = registry.lookup(selection.provider_id)?;
    let metadata = factory.metadata();
    let mut session = session_store.session(selection.provider_id).clone();
    session.model = selection.model.to_string();

    let resolved = selection
        .connection_id
        .zip(selection.model_id)
        .and_then(|(connection_id, model_id)| catalog?.resolve(connection_id, model_id).ok())
        .or_else(|| {
            resolved_model_for_provider_model(catalog, selection.provider_id, selection.model)
        });
    if let Some(resolved) = resolved {
        let reasoning = normalize_reasoning_for_provider_model(
            catalog,
            selection.provider_id,
            selection.model,
            metadata,
            selection.reasoning,
        );
        session.reasoning = reasoning;
        session
            .model_reasoning
            .insert(selection.model.to_string(), reasoning);
        let base_url = runtime_base_url(
            session.base_url.as_str(),
            &metadata.default_base_url,
            resolved.default_base_url.as_ref(),
        );
        let target = resolved_run_target_with_live_context(
            catalog,
            selection.provider_id,
            &resolved,
            base_url,
            reasoning,
            session.context_window,
        );
        let provider = factory.build_target(&session, &target);
        let live_context = live_context_window_for_provider_model(
            catalog,
            selection.provider_id,
            resolved.remote_model_id.as_ref(),
        );
        let context_budget_tokens = live_context
            .or(resolved.context_window)
            .or(session.context_window)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS) as usize;
        let prompt_estimator = PromptEstimator::from_resolved_model(metadata, &session, &resolved);
        return Some(
            SubagentProviderConfig::new(
                provider,
                context_budget_tokens,
                prompt_estimator,
                crate::subagent::provider_model_label(selection.provider_id, selection.model),
            )
            .with_active_model_identity(selection.provider_id, selection.model),
        );
    }

    let reasoning = normalize_reasoning_for_provider_model(
        catalog,
        selection.provider_id,
        selection.model,
        metadata,
        selection.reasoning,
    );
    session.reasoning = reasoning;
    session
        .model_reasoning
        .insert(selection.model.to_string(), reasoning);
    let mut target = crate::provider::fallback_run_target(metadata, &session);
    apply_live_run_target_metadata(catalog, selection.provider_id, selection.model, &mut target);
    let provider = factory.build_target(&session, &target);
    let context_budget_tokens =
        fallback_context_window_for_provider_model(catalog, selection.provider_id, &session)
            .or(session.context_window)
            .or(metadata.context_window)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS) as usize;
    let prompt_estimator = PromptEstimator::from_metadata(metadata, &session);
    Some(
        SubagentProviderConfig::new(
            provider,
            context_budget_tokens,
            prompt_estimator,
            crate::subagent::provider_model_label(selection.provider_id, selection.model),
        )
        .with_active_model_identity(selection.provider_id, selection.model),
    )
}

pub(crate) fn run_target_for_current_model_with_catalog(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
) -> RunTarget {
    run_target_for_current_model_with_catalog_and_reasoning_cap(
        registry,
        session_store,
        catalog,
        None,
    )
}

fn run_target_for_current_model_with_catalog_and_reasoning_cap(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
    reasoning_cap: Option<crate::provider::ReasoningSelection>,
) -> RunTarget {
    let resolution = current_model_resolution(registry, session_store, catalog);
    if let Some(resolved) = resolution.resolved.as_ref() {
        let requested = crate::model_role::reasoning_for_resolved(resolution.session, resolved);
        let reasoning = normalize_and_cap_reasoning(
            catalog,
            resolution.provider_id,
            resolved.remote_model_id.as_ref(),
            resolution.metadata,
            requested,
            reasoning_cap,
        );
        let base_url = runtime_base_url(
            resolution.session.base_url.as_str(),
            &resolution.metadata.default_base_url,
            resolved.default_base_url.as_ref(),
        );
        return resolved_run_target_with_live_context(
            catalog,
            resolution.provider_id,
            resolved,
            base_url,
            reasoning,
            resolution.session.context_window,
        );
    }
    let mut target = crate::provider::fallback_run_target(resolution.metadata, resolution.session);
    let requested = resolution
        .session
        .model_reasoning
        .get(&resolution.session.model)
        .copied()
        .unwrap_or(resolution.session.reasoning);
    target.reasoning = normalize_and_cap_reasoning(
        catalog,
        resolution.provider_id,
        &resolution.session.model,
        resolution.metadata,
        requested,
        reasoning_cap,
    );
    apply_live_run_target_metadata(
        catalog,
        resolution.provider_id,
        &resolution.session.model,
        &mut target,
    );
    target
}

struct CurrentModelResolution<'a> {
    provider_id: &'a str,
    metadata: &'a crate::provider::ProviderMetadata,
    session: &'a crate::session::ProviderSession,
    resolved: Option<ResolvedModel>,
}

impl CurrentModelResolution<'_> {
    fn context_window(&self, catalog: Option<&ModelCatalog>) -> u32 {
        if let Some(resolved) = self.resolved.as_ref() {
            let live_context = live_context_window_for_provider_model(
                catalog,
                self.provider_id,
                resolved.remote_model_id.as_ref(),
            );
            return live_context
                .or(resolved.context_window)
                .or(self.session.context_window)
                .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS);
        }

        fallback_context_window_for_provider_model(catalog, self.provider_id, self.session)
            .or(self.session.context_window)
            .or(self.metadata.context_window)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS)
    }

    fn prompt_estimator(&self) -> PromptEstimator {
        if let Some(resolved) = self.resolved.as_ref() {
            return PromptEstimator::from_resolved_model(self.metadata, self.session, resolved);
        }
        PromptEstimator::from_metadata(self.metadata, self.session)
    }
}

fn current_model_resolution<'a>(
    registry: &'a ProviderRegistry,
    session_store: &'a SessionStore,
    catalog: Option<&ModelCatalog>,
) -> CurrentModelResolution<'a> {
    let provider_id = session_store.current_kind_id();
    let factory = registry
        .lookup(provider_id)
        .or_else(|| registry.lookup(crate::session::DEFAULT_PROVIDER_ID))
        .expect("default provider must exist");
    let session = session_store.session(provider_id);
    let resolved = resolved_active_model_for_current_provider(catalog, provider_id, session_store)
        .or_else(|| resolved_model_for_provider_model(catalog, provider_id, &session.model));
    CurrentModelResolution {
        provider_id,
        metadata: factory.metadata(),
        session,
        resolved,
    }
}

fn normalize_and_cap_reasoning(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    model: &str,
    metadata: &crate::provider::ProviderMetadata,
    requested: crate::provider::ReasoningSelection,
    reasoning_cap: Option<crate::provider::ReasoningSelection>,
) -> crate::provider::ReasoningSelection {
    let reasoning =
        normalize_reasoning_for_provider_model(catalog, provider_id, model, metadata, requested);
    let Some(cap) = reasoning_cap else {
        return reasoning;
    };
    let supported = supported_reasoning_for_provider_model(catalog, provider_id, model, metadata);
    cap_internal_lane_effort(reasoning, cap, &supported)
}

fn runtime_base_url(
    session_base_url: &str,
    metadata_default: &str,
    catalog_default: &str,
) -> Box<str> {
    let session_base_url = session_base_url.trim().trim_end_matches('/');
    let metadata_default = metadata_default.trim().trim_end_matches('/');
    let catalog_default = catalog_default.trim().trim_end_matches('/');
    if session_base_url.is_empty() || session_base_url == metadata_default {
        catalog_default.into()
    } else {
        session_base_url.into()
    }
}

fn resolved_run_target_with_live_context(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    resolved: &ResolvedModel,
    base_url: Box<str>,
    reasoning: crate::provider::ReasoningSelection,
    fallback_context_window: Option<u32>,
) -> crate::model_catalog::RunTarget {
    let mut target = resolved.run_target(base_url, reasoning);
    // `ResolvedModel::run_target` guards against unsupported static catalog
    // efforts. A fresh provider descriptor is more authoritative, so restore
    // the already live-normalized selection after constructing the target.
    target.reasoning = reasoning;
    apply_live_run_target_metadata(
        catalog,
        provider_id,
        resolved.remote_model_id.as_ref(),
        &mut target,
    );
    if target.context_window.is_none() {
        target.context_window = fallback_context_window;
    }
    target
}

fn apply_live_run_target_metadata(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    model: &str,
    target: &mut RunTarget,
) {
    let Some(live) = live_model_for_provider_model(catalog, provider_id, model) else {
        return;
    };
    if let Some(context_window) = live.context_window {
        target.context_window = Some(context_window);
    }
    if !live.supported_reasoning.is_empty() {
        target.reasoning_escalation = target
            .reasoning
            .next_higher_supported(&live.supported_reasoning);
    }
    target.use_responses_lite = live.use_responses_lite;
}

pub(crate) fn context_window_for_current_model_with_catalog(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
) -> u32 {
    current_model_resolution(registry, session_store, catalog).context_window(catalog)
}

pub(crate) fn prompt_estimator_for_current_model_with_catalog(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
) -> PromptEstimator {
    current_model_resolution(registry, session_store, catalog).prompt_estimator()
}

pub(crate) fn resolved_model_for_provider_model(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    model: &str,
) -> Option<ResolvedModel> {
    let connection_id = provider_id.parse::<ConnectionId>().ok()?;
    catalog?.resolve_connection_model(&connection_id, model)
}

pub(crate) fn live_model_for_provider_model(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    model: &str,
) -> Option<crate::model_catalog::AvailableModel> {
    let connection_id = provider_id.parse::<ConnectionId>().ok()?;
    let remote_model = resolved_model_for_provider_model(catalog, provider_id, model)
        .map(|resolved| resolved.remote_model_id)
        .unwrap_or_else(|| model.into());
    catalog?.live_model_for_connection_model(&connection_id, &remote_model)
}

pub(crate) fn supported_reasoning_for_provider_model(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    model: &str,
    metadata: &crate::provider::ProviderMetadata,
) -> Vec<crate::provider::ReasoningSelection> {
    if let Some(live) = live_model_for_provider_model(catalog, provider_id, model)
        && !live.supported_reasoning.is_empty()
    {
        return live.supported_reasoning;
    }
    resolved_model_for_provider_model(catalog, provider_id, model)
        .map(|resolved| resolved.reasoning_selections())
        .unwrap_or_else(|| metadata.capabilities_for_model(model).reasoning.to_vec())
}

pub(crate) fn normalize_reasoning_for_provider_model(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    model: &str,
    metadata: &crate::provider::ProviderMetadata,
    reasoning: crate::provider::ReasoningSelection,
) -> crate::provider::ReasoningSelection {
    if let Some(normalized) = live_model_for_provider_model(catalog, provider_id, model)
        .and_then(|live| live.normalize_reasoning(reasoning))
    {
        return normalized;
    }
    resolved_model_for_provider_model(catalog, provider_id, model)
        .map(|resolved| resolved.normalize_reasoning(reasoning))
        .unwrap_or_else(|| metadata.normalize_reasoning_for_model(model, reasoning))
}

/// Whether the given provider/model can accept image content parts. Vision is
/// allowed when *either* the model catalog marks the model with the
/// `attachment` feature *or* the provider's metadata declares vision support —
/// the OR keeps known-vision providers (Anthropic, Codex) usable even when a
/// catalog entry omits the feature, and a false positive merely surfaces as a
/// provider-side error rather than silently dropping the image.
pub(crate) fn model_supports_vision(
    registry: &ProviderRegistry,
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    model: &str,
) -> bool {
    let catalog_vision = resolved_model_for_provider_model(catalog, provider_id, model)
        .is_some_and(|resolved| resolved.features.contains(&ModelFeature::Attachment));
    let provider_vision = registry.lookup(provider_id).is_some_and(|factory| {
        factory
            .metadata()
            .capabilities_for_model(model)
            .supports_vision
    });
    catalog_vision || provider_vision
}

fn live_context_window_for_provider_model(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    model: &str,
) -> Option<u32> {
    let connection_id = provider_id.parse::<ConnectionId>().ok()?;
    catalog?.live_context_window_for_connection_model(&connection_id, model)
}

fn fallback_context_window_for_provider_model(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    session: &crate::session::ProviderSession,
) -> Option<u32> {
    live_context_window_for_provider_model(catalog, provider_id, &session.model)
}

fn resolved_active_model_for_current_provider(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    session_store: &SessionStore,
) -> Option<ResolvedModel> {
    let catalog = catalog?;
    let expected_connection = connection_id_for_provider_id(provider_id)?;
    let (active_connection, active_model) = session_store.active_target_ids()?;
    if active_connection != &expected_connection {
        return None;
    }
    catalog.resolve(active_connection, active_model).ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::{
        build_provider, cap_internal_lane_effort, context_window_for_current_model_with_catalog,
        is_internal_lane, run_config_for, run_target_for_current_model_with_catalog,
        run_target_for_current_model_with_catalog_and_reasoning_cap, subagent_provider_factory,
    };
    use crate::provider::ProviderRegistry;
    use crate::session::{ProviderSession, SessionStore};

    fn custom_openai_catalog() -> crate::model_catalog::ModelCatalog {
        let connection_id = "local-openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        let model_id = "local/qwen3-coder"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        crate::model_catalog::ModelCatalog::from_spec(crate::model_catalog::CatalogSpec {
            connections: vec![crate::model_catalog::ConnectionSpec {
                id: connection_id.clone(),
                enabled: true,
                display_name: "Local OpenAI".into(),
                auth: crate::model_catalog::ConnectionAuth::OptionalApiKey,
                transport: crate::model_catalog::TransportProtocol::OpenAiChat,
                default_base_url: "http://localhost:11434/v1".into(),
                api_key_env: Some("LOCAL_OPENAI_API_KEY".into()),
                model_env: Some("LOCAL_OPENAI_MODEL".into()),
                base_url_env: Some("LOCAL_OPENAI_BASE_URL".into()),
                default_model: Some(model_id.clone()),
                default_endpoint_path: Some("chat/completions".into()),
                default_token_counter: Some(crate::provider::TokenCounterKind::Tiktoken),
                models_dev_provider: None,
                reasoning_codec: None,
                prompt_cache: false,
                prompt_cache_policy: Default::default(),
                reasoning_content_echo: false,
                usage_frame_ends_stream: false,
                auth_header: None,
                discovery: Default::default(),
            }],
            targets: vec![crate::model_catalog::TargetSpec {
                connection: connection_id,
                enabled: true,
                model: model_id,
                display_name: None,
                metadata_model: None,
                remote_model: Some("qwen3-coder:latest".into()),
                recommended: true,
                recommended_effort: None,
                discouraged_efforts: Vec::new(),
                is_default: true,
                transport: None,
                prompt_cache_policy: None,
                endpoint_path: None,
                context_window: Some(131_072),
                output_limit: None,
                token_counter: None,
                max_tokens: None,
                reasoning_codec: None,
                reasoning_options: None,
                features: vec![crate::model_catalog::ModelFeature::ToolCall],
                pricing: None,
                roles: Vec::new(),
                pinned: false,
            }],
            models_dev: crate::model_catalog::ModelsDevCatalog::default(),
            connection_sources: std::collections::HashMap::new(),
        })
    }

    fn unspecified_context_openai_catalog() -> crate::model_catalog::ModelCatalog {
        let connection_id = "local-openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        let model_id = "local/no-context"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        crate::model_catalog::ModelCatalog::from_spec(crate::model_catalog::CatalogSpec {
            connections: vec![crate::model_catalog::ConnectionSpec {
                id: connection_id.clone(),
                enabled: true,
                display_name: "Local OpenAI".into(),
                auth: crate::model_catalog::ConnectionAuth::OptionalApiKey,
                transport: crate::model_catalog::TransportProtocol::OpenAiChat,
                default_base_url: "http://localhost:11434/v1".into(),
                api_key_env: None,
                model_env: None,
                base_url_env: None,
                default_model: Some(model_id.clone()),
                default_endpoint_path: Some("chat/completions".into()),
                default_token_counter: Some(crate::provider::TokenCounterKind::Tiktoken),
                models_dev_provider: None,
                reasoning_codec: None,
                prompt_cache: false,
                prompt_cache_policy: Default::default(),
                reasoning_content_echo: false,
                usage_frame_ends_stream: false,
                auth_header: None,
                discovery: Default::default(),
            }],
            targets: vec![crate::model_catalog::TargetSpec {
                connection: connection_id,
                enabled: true,
                model: model_id,
                display_name: None,
                metadata_model: None,
                remote_model: None,
                recommended: true,
                recommended_effort: None,
                discouraged_efforts: Vec::new(),
                is_default: true,
                transport: None,
                prompt_cache_policy: None,
                endpoint_path: None,
                context_window: None,
                output_limit: None,
                token_counter: None,
                max_tokens: None,
                reasoning_codec: None,
                reasoning_options: None,
                features: vec![crate::model_catalog::ModelFeature::ToolCall],
                pricing: None,
                roles: Vec::new(),
                pinned: false,
            }],
            models_dev: crate::model_catalog::ModelsDevCatalog::default(),
            connection_sources: std::collections::HashMap::new(),
        })
    }

    /// A `local-openai` catalog with two models: `local/qwen3-coder` (131072) and a
    /// smaller `local/mini` (16384), so an override test can assert the budget
    /// switches with the model.
    fn two_model_catalog() -> crate::model_catalog::ModelCatalog {
        let connection_id = "local-openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        let qwen = "local/qwen3-coder"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        let mini = "local/mini"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        let target = |model: crate::model_catalog::ModelId, ctx: u32, is_default: bool| {
            crate::model_catalog::TargetSpec {
                connection: connection_id.clone(),
                enabled: true,
                model,
                display_name: None,
                metadata_model: None,
                remote_model: None,
                recommended: is_default,
                recommended_effort: None,
                discouraged_efforts: Vec::new(),
                is_default,
                transport: None,
                prompt_cache_policy: None,
                endpoint_path: None,
                context_window: Some(ctx),
                output_limit: None,
                token_counter: None,
                max_tokens: None,
                reasoning_codec: None,
                reasoning_options: None,
                features: vec![crate::model_catalog::ModelFeature::ToolCall],
                pricing: None,
                roles: Vec::new(),
                pinned: false,
            }
        };
        crate::model_catalog::ModelCatalog::from_spec(crate::model_catalog::CatalogSpec {
            connections: vec![crate::model_catalog::ConnectionSpec {
                id: connection_id.clone(),
                enabled: true,
                display_name: "Local OpenAI".into(),
                auth: crate::model_catalog::ConnectionAuth::OptionalApiKey,
                transport: crate::model_catalog::TransportProtocol::OpenAiChat,
                default_base_url: "http://localhost:11434/v1".into(),
                api_key_env: None,
                model_env: None,
                base_url_env: None,
                default_model: Some(qwen.clone()),
                default_endpoint_path: Some("chat/completions".into()),
                default_token_counter: Some(crate::provider::TokenCounterKind::Tiktoken),
                models_dev_provider: None,
                reasoning_codec: None,
                prompt_cache: false,
                prompt_cache_policy: Default::default(),
                reasoning_content_echo: false,
                usage_frame_ends_stream: false,
                auth_header: None,
                discovery: Default::default(),
            }],
            targets: vec![target(qwen, 131_072, true), target(mini, 16_384, false)],
            models_dev: crate::model_catalog::ModelsDevCatalog::default(),
            connection_sources: std::collections::HashMap::new(),
        })
    }

    fn high_max_reasoning_catalog() -> crate::model_catalog::ModelCatalog {
        let connection_id = "local-openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        let model_id = "local/high-max"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        crate::model_catalog::ModelCatalog::from_spec(crate::model_catalog::CatalogSpec {
            connections: vec![crate::model_catalog::ConnectionSpec {
                id: connection_id.clone(),
                enabled: true,
                display_name: "Local OpenAI".into(),
                auth: crate::model_catalog::ConnectionAuth::OptionalApiKey,
                transport: crate::model_catalog::TransportProtocol::OpenAiChat,
                default_base_url: "http://localhost:11434/v1".into(),
                api_key_env: None,
                model_env: None,
                base_url_env: None,
                default_model: Some(model_id.clone()),
                default_endpoint_path: Some("chat/completions".into()),
                default_token_counter: Some(crate::provider::TokenCounterKind::Tiktoken),
                models_dev_provider: None,
                reasoning_codec: None,
                prompt_cache: false,
                prompt_cache_policy: Default::default(),
                reasoning_content_echo: false,
                usage_frame_ends_stream: false,
                auth_header: None,
                discovery: Default::default(),
            }],
            targets: vec![crate::model_catalog::TargetSpec {
                connection: connection_id,
                enabled: true,
                model: model_id,
                display_name: None,
                metadata_model: None,
                remote_model: None,
                recommended: true,
                recommended_effort: None,
                discouraged_efforts: Vec::new(),
                is_default: true,
                transport: None,
                prompt_cache_policy: None,
                endpoint_path: None,
                context_window: Some(131_072),
                output_limit: None,
                token_counter: None,
                max_tokens: None,
                reasoning_codec: None,
                reasoning_options: Some(vec![crate::provider::ReasoningOption::Effort(vec![
                    crate::provider::ReasoningEffort::High,
                    crate::provider::ReasoningEffort::Max,
                ])]),
                features: vec![crate::model_catalog::ModelFeature::ToolCall],
                pricing: None,
                roles: Vec::new(),
                pinned: false,
            }],
            models_dev: crate::model_catalog::ModelsDevCatalog::default(),
            connection_sources: std::collections::HashMap::new(),
        })
    }

    #[test]
    fn context_window_uses_endpoint_session_fallback_for_unknown_model() {
        let catalog = two_model_catalog();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let mut store = SessionStore::default();
        store.ensure_provider("local-openai");
        store.set_current_kind_id("local-openai");
        let session = store.session_mut("local-openai");
        session.model = "live-only".to_string();
        session.context_window = Some(32_768);

        assert_eq!(
            context_window_for_current_model_with_catalog(&registry, &store, Some(&catalog)),
            32_768
        );
    }

    #[test]
    fn endpoint_session_fallback_applies_to_resolved_model_without_context() {
        let catalog = unspecified_context_openai_catalog();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let mut store = SessionStore::default();
        store.ensure_provider("local-openai");
        store.set_current_kind_id("local-openai");
        let session = store.session_mut("local-openai");
        session.model = "no-context".to_string();
        session.context_window = Some(32_768);

        assert_eq!(
            context_window_for_current_model_with_catalog(&registry, &store, Some(&catalog)),
            32_768
        );
        assert_eq!(
            run_target_for_current_model_with_catalog(&registry, &store, Some(&catalog))
                .context_window,
            Some(32_768)
        );
    }

    #[test]
    fn live_api_context_overrides_endpoint_session_fallback() {
        let catalog = two_model_catalog();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let connection_id = "local-openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        catalog
            .write_live_availability(
                &connection_id,
                crate::model_catalog::LiveModelAvailability {
                    models: vec![
                        crate::model_catalog::AvailableModel::remote_with_context_window(
                            "live-only",
                            Some(65_536),
                        ),
                    ],
                    ..crate::model_catalog::LiveModelAvailability::default()
                },
            )
            .unwrap();
        let mut store = SessionStore::default();
        store.ensure_provider("local-openai");
        store.set_current_kind_id("local-openai");
        let session = store.session_mut("local-openai");
        session.model = "live-only".to_string();
        session.context_window = Some(32_768);

        assert_eq!(
            context_window_for_current_model_with_catalog(&registry, &store, Some(&catalog)),
            65_536
        );
    }

    #[test]
    fn live_api_context_overrides_static_target_context() {
        let catalog = custom_openai_catalog();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let connection_id = "local-openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        catalog
            .write_live_availability(
                &connection_id,
                crate::model_catalog::LiveModelAvailability {
                    models: vec![
                        crate::model_catalog::AvailableModel::remote_with_context_window(
                            "qwen3-coder:latest",
                            Some(262_144),
                        ),
                    ],
                    ..crate::model_catalog::LiveModelAvailability::default()
                },
            )
            .unwrap();
        let mut store = SessionStore::default();
        store.ensure_provider("local-openai");
        store.set_current_kind_id("local-openai");
        store.session_mut("local-openai").model = "qwen3-coder:latest".to_string();

        assert_eq!(
            context_window_for_current_model_with_catalog(&registry, &store, Some(&catalog)),
            262_144
        );
        assert_eq!(
            run_target_for_current_model_with_catalog(&registry, &store, Some(&catalog))
                .context_window,
            Some(262_144)
        );
    }

    #[test]
    fn live_codex_reasoning_options_override_static_catalog() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let connection_id = "codex"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        catalog
            .write_live_availability(
                &connection_id,
                crate::model_catalog::LiveModelAvailability {
                    models: vec![{
                        let mut model = crate::model_catalog::AvailableModel::remote("gpt-5.5")
                            .with_reasoning(
                                vec![
                                    crate::provider::ReasoningSelection::Low,
                                    crate::provider::ReasoningSelection::Medium,
                                    crate::provider::ReasoningSelection::High,
                                    crate::provider::ReasoningSelection::XHigh,
                                    crate::provider::ReasoningSelection::Max,
                                    crate::provider::ReasoningSelection::Ultra,
                                ],
                                Some(crate::provider::ReasoningSelection::Medium),
                            );
                        model.use_responses_lite = true;
                        model
                    }],
                    ..crate::model_catalog::LiveModelAvailability::default()
                },
            )
            .unwrap();
        let mut store = SessionStore::default();
        store.ensure_provider("codex");
        store.set_current_kind_id("codex");
        let session = store.session_mut("codex");
        session.model = "gpt-5.5".to_string();
        session.reasoning = crate::provider::ReasoningSelection::Medium;
        session.model_reasoning.insert(
            "gpt-5.5".to_string(),
            crate::provider::ReasoningSelection::Medium,
        );

        let target = run_target_for_current_model_with_catalog(&registry, &store, Some(&catalog));
        assert_eq!(
            target.reasoning,
            crate::provider::ReasoningSelection::Medium
        );
        assert_eq!(
            target.reasoning_escalation,
            Some(crate::provider::ReasoningSelection::High)
        );
        assert!(target.use_responses_lite);
        assert_eq!(
            build_provider(&registry, &store, Some(&catalog)).reasoning_escalation(),
            Some(crate::provider::ReasoningSelection::High)
        );
    }

    #[test]
    fn run_config_for_honors_override_model_and_falls_back_to_base() {
        let catalog = two_model_catalog();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let connection_id = "local-openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        let qwen_model_id = "local/qwen3-coder"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        let mini_model_id = "local/mini"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "local-openai".parse().unwrap(),
            ProviderSession::new(
                String::new(),
                "http://localhost:11434/v1".to_string(),
                "local/qwen3-coder".to_string(),
            ),
        );
        let shortcut_key = crate::model_role::ModelShortcutKey::new('f').unwrap();
        let mut store = SessionStore::from_persistent_parts(
            "local-openai".to_string(),
            Some(connection_id.clone()),
            Some(qwen_model_id.clone()),
            providers,
            Default::default(),
            std::collections::BTreeMap::from([(
                shortcut_key,
                crate::model_role::ModelShortcutBinding {
                    provider_id: "local-openai".parse().unwrap(),
                    connection_id: Some(connection_id.clone()),
                    model_id: Some(mini_model_id),
                    model: "local/mini".to_string(),
                    reasoning: crate::provider::ReasoningSelection::Default,
                },
            )]),
            String::new(),
            Default::default(),
        );

        // No override -> the parent's current model + budget.
        let base = run_config_for(&registry, &store, Some(&catalog), None, false);
        assert_eq!(base.model, "local-openai:local/qwen3-coder");
        assert_eq!(base.context_budget_tokens, 131_072);
        assert_eq!(
            base.active_model_identity,
            Some(crate::agent::ActiveModelIdentity {
                provider_id: "local-openai".parse().unwrap(),
                model: "local/qwen3-coder".to_string(),
            })
        );

        // Override to the smaller model -> its model + budget.
        let over = crate::subagent::SubagentModelOverride::selector(
            "local/mini".to_string(),
            crate::provider::ReasoningSelection::Default,
        );
        let overridden = run_config_for(&registry, &store, Some(&catalog), Some(&over), false);
        assert_eq!(overridden.model, "local-openai:local/mini");
        assert_eq!(overridden.context_budget_tokens, 16_384);
        assert_eq!(
            overridden.active_model_identity,
            Some(crate::agent::ActiveModelIdentity {
                provider_id: "local-openai".parse().unwrap(),
                model: "local/mini".to_string(),
            })
        );

        // Live picker overrides pin the selected provider/catalog target rather
        // than resolving against the current provider.
        let explicit = crate::subagent::SubagentModelOverride::explicit(
            "local-openai".to_string(),
            Some("local-openai".parse().unwrap()),
            Some("local/mini".parse().unwrap()),
            "local/mini".to_string(),
            crate::provider::ReasoningSelection::Default,
        );
        let explicit_config =
            run_config_for(&registry, &store, Some(&catalog), Some(&explicit), false);
        assert_eq!(explicit_config.model, "local-openai:local/mini");
        assert_eq!(explicit_config.context_budget_tokens, 16_384);

        // Shortcut selectors use the user's picker binding.
        let shortcut_override = crate::subagent::SubagentModelOverride::selector(
            "f".to_string(),
            crate::provider::ReasoningSelection::Default,
        );
        let shortcut_config = run_config_for(
            &registry,
            &store,
            Some(&catalog),
            Some(&shortcut_override),
            false,
        );
        assert_eq!(shortcut_config.model, "local-openai:local/mini");
        assert_eq!(shortcut_config.context_budget_tokens, 16_384);

        // Shortcut selectors are live: user-defined agents with `model: f`
        // pick up the current assignment without editing the agent file.
        store.set_model_shortcut_binding(
            shortcut_key,
            crate::model_role::ModelShortcutBinding {
                provider_id: "local-openai".parse().unwrap(),
                connection_id: Some(connection_id.clone()),
                model_id: Some(qwen_model_id),
                model: "local/qwen3-coder".to_string(),
                reasoning: crate::provider::ReasoningSelection::Default,
            },
        );
        let reassigned_shortcut_config = run_config_for(
            &registry,
            &store,
            Some(&catalog),
            Some(&shortcut_override),
            false,
        );
        assert_eq!(
            reassigned_shortcut_config.model,
            "local-openai:local/qwen3-coder"
        );
        assert_eq!(reassigned_shortcut_config.context_budget_tokens, 131_072);

        // Legacy role names are no longer aliases. Persisted role bindings
        // migrate to c/f/s, so callers should use `f` rather than `fast`.
        let legacy_name = crate::subagent::SubagentModelOverride::selector(
            "fast".to_string(),
            crate::provider::ReasoningSelection::Default,
        );
        let legacy_name_fallback =
            run_config_for(&registry, &store, Some(&catalog), Some(&legacy_name), false);
        assert_eq!(legacy_name_fallback.model, "local-openai:local/qwen3-coder");
        assert_eq!(legacy_name_fallback.context_budget_tokens, 131_072);

        // Unbound letters have no implicit default; they fall back to the
        // parent's active model.
        let unbound_shortcut = crate::subagent::SubagentModelOverride::selector(
            "s".to_string(),
            crate::provider::ReasoningSelection::Default,
        );
        let shortcut_fallback = run_config_for(
            &registry,
            &store,
            Some(&catalog),
            Some(&unbound_shortcut),
            false,
        );
        assert_eq!(shortcut_fallback.model, "local-openai:local/qwen3-coder");
        assert_eq!(shortcut_fallback.context_budget_tokens, 131_072);

        // An unresolvable override (model the provider doesn't expose) -> base.
        let bogus = crate::subagent::SubagentModelOverride::selector(
            "nope/nope".to_string(),
            crate::provider::ReasoningSelection::Default,
        );
        let fallback = run_config_for(&registry, &store, Some(&catalog), Some(&bogus), false);
        assert_eq!(fallback.model, "local-openai:local/qwen3-coder");
        assert_eq!(fallback.context_budget_tokens, 131_072);
    }

    #[test]
    fn internal_lane_effort_cap_downshifts_high_but_never_upshifts() {
        use crate::provider::ReasoningSelection;
        let supported = [
            ReasoningSelection::Off,
            ReasoningSelection::Low,
            ReasoningSelection::Medium,
            ReasoningSelection::High,
            ReasoningSelection::XHigh,
            ReasoningSelection::Max,
        ];
        // High tiers cap at Medium.
        assert_eq!(
            cap_internal_lane_effort(
                ReasoningSelection::High,
                ReasoningSelection::Medium,
                &supported
            ),
            ReasoningSelection::Medium
        );
        assert_eq!(
            cap_internal_lane_effort(
                ReasoningSelection::XHigh,
                ReasoningSelection::Medium,
                &supported
            ),
            ReasoningSelection::Medium
        );
        assert_eq!(
            cap_internal_lane_effort(
                ReasoningSelection::Max,
                ReasoningSelection::Medium,
                &supported
            ),
            ReasoningSelection::Medium
        );
        // At or below Medium (and non-effort selections) are left untouched.
        for kept in [
            ReasoningSelection::Low,
            ReasoningSelection::Medium,
            ReasoningSelection::Minimal,
            ReasoningSelection::Default,
            ReasoningSelection::Off,
        ] {
            assert_eq!(
                cap_internal_lane_effort(kept, ReasoningSelection::Medium, &supported),
                kept
            );
        }
    }

    #[test]
    fn internal_lane_effort_cap_chooses_supported_lower_value() {
        use crate::provider::ReasoningSelection;

        assert_eq!(
            cap_internal_lane_effort(
                ReasoningSelection::High,
                ReasoningSelection::Medium,
                &[ReasoningSelection::Low, ReasoningSelection::High]
            ),
            ReasoningSelection::Low
        );
        assert_eq!(
            cap_internal_lane_effort(
                ReasoningSelection::High,
                ReasoningSelection::Medium,
                &[ReasoningSelection::Off, ReasoningSelection::High]
            ),
            ReasoningSelection::Off
        );
        assert_eq!(
            cap_internal_lane_effort(
                ReasoningSelection::High,
                ReasoningSelection::Medium,
                &[ReasoningSelection::High, ReasoningSelection::Max]
            ),
            ReasoningSelection::Default
        );
    }

    #[test]
    fn capped_run_target_never_emits_unsupported_medium_effort() {
        let catalog = high_max_reasoning_catalog();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let mut store = SessionStore::default();
        store.ensure_provider("local-openai");
        store.set_current_kind_id("local-openai");
        let session = store.session_mut("local-openai");
        session.model = "local/high-max".to_string();
        session.reasoning = crate::provider::ReasoningSelection::High;

        let target = run_target_for_current_model_with_catalog_and_reasoning_cap(
            &registry,
            &store,
            Some(&catalog),
            Some(crate::provider::ReasoningSelection::Medium),
        );

        assert_eq!(
            target.reasoning,
            crate::provider::ReasoningSelection::Default
        );
    }

    #[test]
    fn internal_lane_detection_matches_builtin_read_only_lanes() {
        assert!(is_internal_lane("self-review"));
        assert!(is_internal_lane("explore"));
        assert!(is_internal_lane("review"));
        assert!(is_internal_lane("research"));
        assert!(is_internal_lane("security-review"));
        assert!(!is_internal_lane("my-custom-agent"));
    }

    #[test]
    fn build_provider_uses_catalog_remote_model_for_codex_canonical_id() {
        let registry = ProviderRegistry::default_registry();
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let mut store = SessionStore::default();
        store.ensure_provider("codex");
        store.set_current_kind_id("codex");
        let session = store.session_mut("codex");
        session.api_key = "codex-token".to_string();
        session.account_id = "account".to_string();
        session.model = "openai/gpt-5.5".to_string();

        let provider = build_provider(&registry, &store, Some(&catalog));
        let preview = provider.preview_request(&[], &[]).unwrap();

        assert_eq!(
            preview.endpoint,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(preview.body["model"], serde_json::json!("gpt-5.5"));
    }

    #[test]
    fn build_provider_uses_catalog_transport_for_opencode_mapped_model() {
        let registry = ProviderRegistry::default_registry();
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let mut store = SessionStore::default();
        store.ensure_provider("opencode");
        store.set_current_kind_id("opencode");
        let session = store.session_mut("opencode");
        session.api_key = "opencode-token".to_string();
        session.model = "opencode/qwen3.7-max".to_string();

        let provider = build_provider(&registry, &store, Some(&catalog));
        let preview = provider.preview_request(&[], &[]).unwrap();

        assert_eq!(preview.endpoint, "https://opencode.ai/zen/go/v1/messages");
        assert_eq!(preview.body["model"], serde_json::json!("qwen3.7-max"));
        assert_eq!(preview.body["max_tokens"], serde_json::json!(65536));
    }

    #[test]
    fn build_provider_uses_catalog_only_opencode_zen_connector() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let mut store = SessionStore::default();
        store.ensure_provider("opencode-zen");
        store.set_current_kind_id("opencode-zen");
        let session = store.session_mut("opencode-zen");
        session.api_key = "opencode-token".to_string();
        session.model = "opencode-zen/grok-code".to_string();

        let provider = build_provider(&registry, &store, Some(&catalog));
        let preview = provider.preview_request(&[], &[]).unwrap();

        assert_eq!(
            preview.endpoint,
            "https://opencode.ai/zen/v1/chat/completions"
        );
        assert_eq!(preview.body["model"], serde_json::json!("grok-code"));
    }

    #[test]
    fn build_provider_uses_custom_catalog_connection() {
        let catalog = custom_openai_catalog();
        let registry = ProviderRegistry::from_catalog(&catalog);
        let connection_id = "local-openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        let model_id = "local/qwen3-coder"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "local-openai".parse().unwrap(),
            ProviderSession::new(
                String::new(),
                "http://localhost:11434/v1".to_string(),
                "local/qwen3-coder".to_string(),
            ),
        );
        let store = SessionStore::from_persistent_parts(
            "local-openai".to_string(),
            Some(connection_id),
            Some(model_id),
            providers,
            Default::default(),
            Default::default(),
            String::new(),
            Default::default(),
        );

        let provider = build_provider(&registry, &store, Some(&catalog));
        let preview = provider.preview_request(&[], &[]).unwrap();

        assert_eq!(
            preview.endpoint,
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            preview.body["model"],
            serde_json::json!("qwen3-coder:latest")
        );
    }

    #[tokio::test]
    async fn subagent_provider_factory_reads_live_session_state() {
        let registry = Arc::new(ProviderRegistry::default_registry());
        let mut store = SessionStore::default();
        store.ensure_provider("openai-compatible");
        store.set_current_kind_id("openai-compatible");
        {
            let session = store.session_mut("openai-compatible");
            session.base_url = "https://example.test/v1".to_string();
            session.model = "old-model".to_string();
        }
        let store = Arc::new(Mutex::new(store));
        let factory = subagent_provider_factory(
            registry,
            store.clone(),
            None,
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        );

        {
            let mut store = store.lock().await;
            store.session_mut("openai-compatible").model = "new-model".to_string();
        }

        let config = factory(
            "explore".to_string(),
            crate::subagent::SubagentModelChain::default(),
        )
        .await;
        let preview = config.provider.preview_request(&[], &[]).unwrap();
        assert_eq!(preview.body["model"], serde_json::json!("new-model"));
    }

    #[tokio::test]
    async fn subagent_provider_factory_carries_catalog_context_budget() {
        let catalog = Arc::new(custom_openai_catalog());
        let registry = Arc::new(ProviderRegistry::from_catalog(&catalog));
        let connection_id = "local-openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        let model_id = "local/qwen3-coder"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "local-openai".parse().unwrap(),
            ProviderSession::new(
                String::new(),
                "http://localhost:11434/v1".to_string(),
                "local/qwen3-coder".to_string(),
            ),
        );
        let store = SessionStore::from_persistent_parts(
            "local-openai".to_string(),
            Some(connection_id),
            Some(model_id),
            providers,
            Default::default(),
            Default::default(),
            String::new(),
            Default::default(),
        );
        let factory = subagent_provider_factory(
            registry,
            Arc::new(Mutex::new(store)),
            Some(catalog),
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        );

        let config = factory(
            "explore".to_string(),
            crate::subagent::SubagentModelChain::default(),
        )
        .await;
        assert_eq!(config.context_budget_tokens, 131_072);
    }
}
