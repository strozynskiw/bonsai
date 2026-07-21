//! Shared "switch provider/model + rebuild agent" plumbing, used by every
//! command surface that changes the active model: `/authorize`, `/unauthorize`,
//! `/model` (headless and TUI), the model picker, and the local-provider wizard.

use crate::agent::Agent;
use crate::model_catalog::ModelCatalog;
use crate::model_resolution::{
    active_model_identity, build_provider, context_window_for_current_model_with_catalog,
    normalize_reasoning_for_provider_model, project_info_provider_state,
    prompt_estimator_for_current_model_with_catalog,
};
use crate::provider::{ProviderRegistry, ReasoningSelection};
use crate::session::SessionStore;

use super::ResolvedModelSelection;

/// Point the agent at the current session's provider, context window, and prompt
/// estimator. Called after any change to the active provider or model so the
/// running agent reflects the new selection.
///
/// Deliberately preserves the conversation: `set_provider` swaps every
/// provider-linked field (provider, model identity, context budget, prompt
/// estimator, tool-schema cache, `previous_request_body`) and the message
/// history is provider-agnostic, so switching provider mid-session — via
/// `/model`, `/authorize`, `/unauthorize`, or the provider manager — must not
/// call `agent.clear()`. Clearing here silently emptied the model's context
/// while the on-screen transcript stayed, which read as amnesia to the user.
pub(crate) fn rebuild_agent_provider(
    agent: &mut Agent,
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
) {
    agent.set_provider(
        build_provider(registry, session_store, catalog),
        context_window_for_current_model_with_catalog(registry, session_store, catalog) as usize,
        prompt_estimator_for_current_model_with_catalog(registry, session_store, catalog),
        active_model_identity(session_store),
    );
    agent.set_project_info_provider(project_info_provider_state(registry, session_store));
}

/// Persist a resolved model selection into the session: set the active model
/// target, then normalize and store the reasoning for it. Returns the normalized
/// reasoning actually applied (callers compare it against what was requested to
/// detect an unsupported-reasoning fallback).
///
/// `requested_reasoning` is decided by the caller — the headless `/model` text
/// command derives it from prior session state, while the TUI picker passes the
/// user's chosen reasoning. This does **not** touch the agent or save the
/// session; callers do that (the TUI does it under a separately-held agent lock).
pub(crate) fn apply_model_selection(
    registry: &ProviderRegistry,
    session_store: &mut SessionStore,
    catalog: Option<&ModelCatalog>,
    selection: &ResolvedModelSelection,
    requested_reasoning: ReasoningSelection,
) -> ReasoningSelection {
    let metadata = registry
        .lookup(&selection.provider_id)
        .expect("model selection provider must be in registry")
        .metadata();
    session_store.set_active_model_target(
        &selection.provider_id,
        selection.connection_id.clone(),
        selection.model_id.clone(),
        &selection.model,
    );

    let current_model = session_store.session(&selection.provider_id).model.clone();
    let normalized = normalize_reasoning_for_provider_model(
        catalog,
        &selection.provider_id,
        &current_model,
        metadata,
        requested_reasoning,
    );
    let session = session_store.session_mut(&selection.provider_id);
    session.reasoning = normalized;
    session.model_reasoning.insert(current_model, normalized);
    normalized
}

/// Record an applied `/model` selection against the active working mode's
/// per-mode entry (no-op without a mode or for Review). `previous` is the
/// selection string captured before `apply_model_selection` ran.
pub(crate) fn record_active_mode_model(
    session_store: &mut SessionStore,
    active_mode: Option<crate::agent::AgentMode>,
    previous: String,
) {
    if let Some(key) = active_mode.and_then(crate::agent::AgentMode::model_key) {
        session_store.record_mode_model(key, Some(previous));
    }
}

/// The reasoning the session remembers for a resolved selection — the same
/// derivation the `/model` text command uses when no reasoning is given:
/// per-model remembered value (canonical id first), else the provider
/// session's model-appropriate default.
pub(crate) fn remembered_reasoning_for(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
    selection: &ResolvedModelSelection,
) -> ReasoningSelection {
    let metadata = registry
        .lookup(&selection.provider_id)
        .expect("selection provider must be in registry")
        .metadata();
    let resolved = crate::model_resolution::resolved_model_for_provider_model(
        catalog,
        &selection.provider_id,
        &selection.model,
    );
    if let Some(resolved) = &resolved {
        let canonical_model = resolved.model_id.to_string();
        let session = session_store.session(&selection.provider_id);
        session
            .model_reasoning
            .get(&canonical_model)
            .or_else(|| session.model_reasoning.get(&selection.model))
            .copied()
            .unwrap_or(session.reasoning)
    } else {
        session_store
            .session(&selection.provider_id)
            .reasoning_for_model(metadata, &selection.model)
    }
}

/// Resolve any `/model` input form — a role shortcut (`cheap`/`fast`/`smart`)
/// or a provider/model selector — with its reasoning: the shortcut's bound
/// reasoning, else the session's remembered reasoning for the target.
pub(crate) fn resolve_model_input_with_reasoning(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: Option<&ModelCatalog>,
    input: &str,
) -> Option<(ResolvedModelSelection, ReasoningSelection)> {
    if let Ok(key) = input.parse::<crate::model_role::ModelShortcutKey>() {
        let shortcut =
            crate::model_role::resolve_model_shortcut(registry, session_store, catalog, key)?;
        let selection = ResolvedModelSelection::from(&shortcut);
        let reasoning = shortcut.reasoning;
        return Some((selection, reasoning));
    }
    let selection = crate::commands::providers::resolve_model_selection(
        registry,
        session_store,
        catalog,
        input,
    )?;
    let reasoning = remembered_reasoning_for(registry, session_store, catalog, &selection);
    Some((selection, reasoning))
}

/// The `/model` input the given persona wants applied before it runs:
/// a built-in working mode's recorded per-mode entry, or a custom persona's
/// `model:` from its definition. `None` means "keep the current model".
pub(crate) fn desired_persona_model_input(
    persona: &crate::agent::ActivePersona,
    session_store: &SessionStore,
    custom_agents: &crate::resource::agent::SharedAgentRegistry,
) -> Option<String> {
    match persona {
        crate::agent::ActivePersona::Builtin(mode) => mode
            .model_key()
            .and_then(|key| session_store.mode_model(key))
            .map(str::to_string),
        crate::agent::ActivePersona::Custom(name) => {
            let registry = crate::resource::agent::snapshot(custom_agents);
            registry.get(name).and_then(|def| def.model.clone())
        }
    }
}

/// Apply a `/model` input to the session + agent when it differs from the
/// current active target. Does not save the session — callers persist. Returns
/// the applied model and normalized reasoning, or `None` when the input was
/// unresolvable or already current.
pub(crate) fn apply_model_input_if_different(
    registry: &ProviderRegistry,
    session_store: &mut SessionStore,
    catalog: Option<&ModelCatalog>,
    agent: &mut Agent,
    input: &str,
) -> Option<(String, ReasoningSelection)> {
    let (selection, reasoning) =
        resolve_model_input_with_reasoning(registry, session_store, catalog, input)?;
    let current = session_store.current_session();
    // The session persists the canonical model id when the catalog knows one,
    // so compare against both the display form and the canonical id.
    let model_matches = current.model == selection.model
        || selection
            .model_id
            .as_ref()
            .is_some_and(|id| current.model == id.to_string());
    if session_store.current_kind_id() == selection.provider_id && model_matches {
        return None;
    }
    let normalized = apply_model_selection(registry, session_store, catalog, &selection, reasoning);
    rebuild_agent_provider(agent, registry, session_store, catalog);
    Some((session_store.current_session().model.clone(), normalized))
}

pub(crate) fn context_window_downgrade_warning(
    old_tokens: usize,
    new_tokens: usize,
) -> Option<String> {
    (new_tokens < old_tokens).then(|| {
        format!(
            "Context window reduced from {} to {} tokens; compact if the current conversation no longer fits.",
            crate::util::format::format_tokens_k(old_tokens),
            crate::util::format::format_tokens_k(new_tokens)
        )
    })
}
