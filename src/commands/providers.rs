use std::time::Duration;

use super::*;
use crate::model_catalog::{
    ConnectionId, ModelCatalog, ModelId, available_model_ids_for_provider,
    connection_id_for_provider_id,
};
use crate::model_role::ModelShortcutSelection;
use crate::provider::AuthRequirement;

const LIVE_MODEL_CACHE_TTL: Duration = Duration::from_secs(300);

/// Cap on a *local* endpoint's model listing during a refresh. A local server
/// that isn't running makes `list_available_models` hang on the TCP connect,
/// and because `/refresh` fans out over every provider concurrently, one dead
/// local endpoint would otherwise stall the whole batch. Remote providers keep
/// their own client-level timeouts and are left untouched.
const LOCAL_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedModelSelection {
    pub provider_id: String,
    pub connection_id: Option<ConnectionId>,
    pub model_id: Option<ModelId>,
    pub model: String,
}

impl From<&ModelShortcutSelection> for ResolvedModelSelection {
    fn from(selection: &ModelShortcutSelection) -> Self {
        Self {
            provider_id: selection.provider_id.clone(),
            connection_id: selection.connection_id.clone(),
            model_id: selection.model_id.clone(),
            model: selection.model.clone(),
        }
    }
}

fn resolved_selection_for_provider_model(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    model: String,
) -> ResolvedModelSelection {
    let resolved =
        crate::model_resolution::resolved_model_for_provider_model(catalog, provider_id, &model);
    ResolvedModelSelection {
        provider_id: provider_id.to_string(),
        connection_id: resolved
            .as_ref()
            .map(|model| model.connection_id.clone())
            .or_else(|| connection_id_for_provider_id(provider_id)),
        model_id: resolved.as_ref().map(|model| model.model_id.clone()),
        model,
    }
}

pub(crate) fn resolve_model_selection(
    registry: &ProviderRegistry,
    session: &SessionStore,
    catalog: Option<&ModelCatalog>,
    input: &str,
) -> Option<ResolvedModelSelection> {
    if let Some(selection) = resolve_connection_model(registry, catalog, input) {
        return Some(selection);
    }
    if let Some(selection) = resolve_canonical_model(registry, session, catalog, input) {
        return Some(selection);
    }

    let provider_id = session.current_kind_id();
    let factory = registry.lookup(provider_id)?;
    let provider_session = session.session(provider_id);
    let models = available_model_ids_for_provider(
        catalog,
        provider_id,
        factory.metadata(),
        &provider_session.model,
    );
    if let Ok(index) = input.parse::<usize>()
        && index >= 1
        && index <= models.len()
    {
        return Some(resolved_selection_for_provider_model(
            catalog,
            provider_id,
            models[index - 1].clone(),
        ));
    }
    if models.iter().any(|model| model == input) {
        return Some(resolved_selection_for_provider_model(
            catalog,
            provider_id,
            input.to_string(),
        ));
    }

    resolve_display_model(catalog, provider_id, input)
}

fn resolve_display_model(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    input: &str,
) -> Option<ResolvedModelSelection> {
    let connection_id = connection_id_for_provider_id(provider_id)?;
    let resolved = catalog?.resolve_connection_display_name(&connection_id, input)?;
    Some(ResolvedModelSelection {
        provider_id: provider_id.to_string(),
        connection_id: Some(resolved.connection_id),
        model_id: Some(resolved.model_id),
        model: input.to_string(),
    })
}

fn resolve_connection_model(
    registry: &ProviderRegistry,
    catalog: Option<&ModelCatalog>,
    input: &str,
) -> Option<ResolvedModelSelection> {
    let catalog = catalog?;
    let (connection, model) = input.split_once(':')?;
    let connection_id = connection.parse::<ConnectionId>().ok()?;
    let resolved = catalog.resolve_connection_model(&connection_id, model)?;
    let provider_id = connection_id.as_str();
    registry.lookup(provider_id)?;
    Some(ResolvedModelSelection {
        provider_id: provider_id.to_string(),
        connection_id: Some(connection_id),
        model_id: Some(resolved.model_id),
        model: model.to_string(),
    })
}

fn resolve_canonical_model(
    registry: &ProviderRegistry,
    session: &SessionStore,
    catalog: Option<&ModelCatalog>,
    input: &str,
) -> Option<ResolvedModelSelection> {
    let catalog = catalog?;
    let model_id = input.parse::<ModelId>().ok()?;
    let current_provider = session.current_kind_id();
    if provider_can_run_model(registry, catalog, current_provider, &model_id) {
        let connection_id = connection_id_for_provider_id(current_provider)?;
        return Some(ResolvedModelSelection {
            provider_id: current_provider.to_string(),
            connection_id: Some(connection_id),
            model_id: Some(model_id.clone()),
            model: model_id.to_string(),
        });
    }

    registry
        .all()
        .iter()
        .map(|factory| factory.metadata().id.as_ref())
        .find_map(|provider_id| {
            if !provider_can_run_model(registry, catalog, provider_id, &model_id) {
                return None;
            }
            let connection_id = connection_id_for_provider_id(provider_id)?;
            Some(ResolvedModelSelection {
                provider_id: provider_id.to_string(),
                connection_id: Some(connection_id),
                model_id: Some(model_id.clone()),
                model: model_id.to_string(),
            })
        })
}

fn provider_can_run_model(
    registry: &ProviderRegistry,
    catalog: &ModelCatalog,
    provider_id: &str,
    model_id: &ModelId,
) -> bool {
    let Some(factory) = registry.lookup(provider_id) else {
        return false;
    };
    let Some(connection_id) = connection_id_for_provider_id(&factory.metadata().id) else {
        return false;
    };
    catalog.resolve(&connection_id, model_id).is_ok()
}

pub(in crate::commands) fn authorization_input_for_provider(
    metadata: &crate::provider::ProviderMetadata,
) -> crate::provider::AuthInput {
    match metadata.auth_requirement {
        AuthRequirement::CodexCache => crate::provider::AuthInput::FromCodexCache,
        AuthRequirement::ApiKeyRequired | AuthRequirement::ApiKeyOptional => {
            crate::provider::AuthInput::FromEnv
        }
    }
}

/// Outcome of `open_model_picker`: either open the picker, or reject with a
/// single message explaining why (unknown / unauthorized provider).
pub(in crate::commands) enum ModelPickerOutcome {
    Open,
    Rejected(CommandMessage),
}

pub(in crate::commands) async fn open_model_picker(
    registry: &Arc<ProviderRegistry>,
    session_store: &mut SessionStore,
    agent: &mut Agent,
    target: Option<&str>,
    catalog: Option<&crate::model_catalog::ModelCatalog>,
) -> Result<ModelPickerOutcome> {
    if let Some(input) = target {
        let Some(factory) = resolve_provider(registry, input) else {
            return Ok(ModelPickerOutcome::Rejected(unknown_provider_error(
                input, registry,
            )));
        };
        let target_id = factory.metadata().id.to_string();
        let target_session = session_store.session(&target_id);
        if !factory.is_authorized(target_session) {
            return Ok(ModelPickerOutcome::Rejected(error(format!(
                "Authorize {} before running /model. Try `/authorize {}`.",
                factory.metadata().display_name,
                target_id
            ))));
        }

        let previous_id = session_store.current_kind_id().to_string();
        if previous_id != target_id {
            session_store.set_current_kind_id(&target_id);
            session_store.save_async().await?;
            super::rebuild_agent_provider(agent, registry, session_store, catalog);
        }
    } else {
        let has_authorized_provider = registry.all().iter().any(|factory| {
            let id = factory.metadata().id.as_ref();
            factory.is_authorized(session_store.session(id))
        });

        if !has_authorized_provider {
            return Ok(ModelPickerOutcome::Rejected(error(
                "Authorize a provider before running /model. Try `/authorize <provider>`."
                    .to_string(),
            )));
        }
    }

    Ok(ModelPickerOutcome::Open)
}

/// Refresh stale live-model caches for authorized providers concurrently.
///
/// This is used at startup so opening the model picker only reads the cache.
pub(crate) async fn refresh_stale_authorized_provider_models(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: &ModelCatalog,
) {
    let providers: Vec<_> = registry
        .all()
        .iter()
        .filter_map(|factory| {
            let provider_id = factory.metadata().id.as_ref();
            let session = session_store.providers.get(provider_id)?.clone();
            factory
                .is_authorized(&session)
                .then_some((provider_id, session))
        })
        .collect();

    futures::future::join_all(providers.iter().map(|(provider_id, session)| async move {
        if let Err(err) =
            refresh_stale_model_cache(registry, provider_id, session, Some(catalog)).await
        {
            tracing::warn!(
                provider = %provider_id,
                error = %err,
                "failed to refresh stale live models at startup"
            );
        }
    }))
    .await;
}

/// Refresh a provider's cached model availability if it is absent or stale.
/// Returns whether the catalog was updated. Best-effort: a missing
/// provider/catalog/connection or a failed/empty provider response leaves the
/// existing catalog models in place and returns `false`.
pub(crate) async fn refresh_stale_model_cache(
    registry: &ProviderRegistry,
    provider_id: &str,
    session: &crate::session::ProviderSession,
    catalog: Option<&ModelCatalog>,
) -> Result<bool> {
    let Some(factory) = registry.get(provider_id) else {
        return Ok(false);
    };

    let Some(catalog) = catalog else {
        return Ok(false);
    };
    let Some(connection_id) = connection_id_for_provider_id(provider_id) else {
        return Ok(false);
    };
    if catalog.has_fresh_live_availability(&connection_id, LIVE_MODEL_CACHE_TTL) {
        tracing::debug!(
            provider = %provider_id,
            "skipping model availability refresh; live cache is fresh"
        );
        return Ok(false);
    }

    tracing::info!(
        provider = %provider_id,
        "refreshing stale model availability"
    );
    match list_available_models_bounded(factory.as_ref(), session).await {
        Ok(availability) if !availability.models.is_empty() => {
            let refreshed = availability.models.len();
            let availability = availability.with_fallback_context_window(session.context_window);
            catalog.write_live_availability(&connection_id, availability)?;
            tracing::info!(
                provider = %provider_id,
                available_models = refreshed,
                "refreshed model availability"
            );
            Ok(true)
        }
        outcome => {
            if let Err(err) = outcome {
                tracing::warn!(
                    provider = %provider_id,
                    error = %err,
                    "failed to refresh stale model availability; keeping existing catalog models"
                );
            } else {
                tracing::warn!(
                    provider = %provider_id,
                    "model availability refresh returned no models; keeping existing catalog models"
                );
            }
            Ok(false)
        }
    }
}

/// Whether `base_url` points at a server on this machine (loopback / unspecified
/// address, `localhost`, or an mDNS `*.local` name). Refresh fetches to these
/// get a short [`LOCAL_REFRESH_TIMEOUT`] so a stopped local model server fails
/// fast instead of hanging the refresh. Scheme-less URLs (e.g.
/// `localhost:11434/v1`) are accepted the same way the endpoint normalizer does.
fn is_local_endpoint(base_url: &str) -> bool {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return false;
    }
    let with_scheme = if trimmed.contains("://") {
        std::borrow::Cow::Borrowed(trimmed)
    } else {
        std::borrow::Cow::Owned(format!("http://{trimmed}"))
    };
    let Ok(url) = reqwest::Url::parse(&with_scheme) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    // `host_str` returns IPv6 literals bracketed (`[::1]`); strip them so the
    // address parses.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback() || ip.is_unspecified();
    }
    host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".local")
}

/// Fetch a provider's live model list, bounding a *local* endpoint with
/// [`LOCAL_REFRESH_TIMEOUT`] so an unreachable local server fails fast rather
/// than blocking the refresh. Remote endpoints are fetched without an added
/// timeout — their transport client already bounds the request.
async fn list_available_models_bounded(
    factory: &dyn crate::provider::ProviderFactory,
    session: &crate::session::ProviderSession,
) -> Result<crate::model_catalog::LiveModelAvailability> {
    if is_local_endpoint(&session.base_url) {
        match tokio::time::timeout(
            LOCAL_REFRESH_TIMEOUT,
            factory.list_available_models(session),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => anyhow::bail!(
                "local endpoint timed out after {}s (is the server running?)",
                LOCAL_REFRESH_TIMEOUT.as_secs()
            ),
        }
    } else {
        factory.list_available_models(session).await
    }
}

/// Fetch a provider's live models, reject an empty listing as an error, and
/// stamp the session's context window as the fallback for models that omit one.
/// Callers own the `write_live_availability` step because they differ (some diff
/// the previous snapshot first, some parse the connection id inline).
async fn fetch_availability(
    factory: &dyn crate::provider::ProviderFactory,
    session: &crate::session::ProviderSession,
) -> Result<crate::model_catalog::LiveModelAvailability> {
    let availability = list_available_models_bounded(factory, session).await?;
    if availability.models.is_empty() {
        anyhow::bail!("returned no models");
    }
    Ok(availability.with_fallback_context_window(session.context_window))
}

/// Status of a single refresh source (models.dev or one provider endpoint)
/// during a `/refresh` cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshSourceState {
    /// Human-readable name (e.g. "Models.dev", "OpenCode", "Anthropic").
    pub display_name: String,
    /// Outcome so far: `Pending` while in flight, `Ok` with a model count, or
    /// `Failed` with a short reason.
    pub status: RefreshSourceStatus,
    /// Number of models after refresh; `None` while pending or on failure.
    pub model_count: Option<usize>,
    /// Model ids present after refresh that were absent before (added).
    pub added: Vec<String>,
    /// Model ids absent after refresh that were present before (removed).
    pub removed: Vec<String>,
}

/// Outcome of a single refresh source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshSourceStatus {
    /// Fetch in flight (initial state).
    Pending,
    /// Fetch succeeded.
    Ok,
    /// Fetch failed; carries a short, secret-free reason.
    Failed(String),
}

impl RefreshSourceState {
    /// A pending row for the given display name.
    pub(crate) fn pending(display_name: impl Into<String>) -> Self {
        Self {
            display_name: display_name.into(),
            status: RefreshSourceStatus::Pending,
            model_count: None,
            added: Vec::new(),
            removed: Vec::new(),
        }
    }
}

/// Compute the added/removed model-id diff between an old and a new
/// `LiveModelAvailability` snapshot, keyed on `remote_model_id`.
fn model_id_diff(
    old: &crate::model_catalog::LiveModelAvailability,
    new: &crate::model_catalog::LiveModelAvailability,
) -> (Vec<String>, Vec<String>) {
    use std::collections::HashSet;

    let old_ids: HashSet<&str> = old
        .models
        .iter()
        .map(|m| m.remote_model_id.as_ref())
        .collect();
    let new_ids: HashSet<&str> = new
        .models
        .iter()
        .map(|m| m.remote_model_id.as_ref())
        .collect();
    let added = new_ids
        .difference(&old_ids)
        .map(|id| id.to_string())
        .collect();
    let removed = old_ids
        .difference(&new_ids)
        .map(|id| id.to_string())
        .collect();
    (added, removed)
}

/// The models.dev source separated out for the live modal. The shared catalog
/// refresh writes the cache in place; this returns the resolved outcome so the
/// driver can emit it as a `RefreshSourceState`.
pub(crate) async fn refresh_models_dev_source(catalog: &ModelCatalog) -> RefreshSourceState {
    match catalog.refresh_models_dev_metadata().await {
        Ok(Some(models)) => RefreshSourceState {
            display_name: "Models.dev".to_string(),
            status: RefreshSourceStatus::Ok,
            model_count: Some(models),
            added: Vec::new(),
            removed: Vec::new(),
        },
        Ok(None) => RefreshSourceState {
            display_name: "Models.dev".to_string(),
            status: RefreshSourceStatus::Ok,
            model_count: None,
            added: Vec::new(),
            removed: Vec::new(),
        },
        Err(_) => RefreshSourceState {
            display_name: "Models.dev".to_string(),
            status: RefreshSourceStatus::Failed(
                catalog
                    .models_dev_refresh_notice()
                    .unwrap_or_else(|| "refresh failed; using cached metadata".to_string()),
            ),
            model_count: None,
            added: Vec::new(),
            removed: Vec::new(),
        },
    }
}

/// Refresh a single provider's live model availability, returning a
/// [`RefreshSourceState`] with the added/removed model diff. Used by the
/// TUI's live `/refresh` modal driver.
pub(crate) async fn refresh_provider_source(
    factory: &std::sync::Arc<dyn crate::provider::ProviderFactory>,
    session: &crate::session::ProviderSession,
    display_name: &str,
    catalog: &ModelCatalog,
) -> RefreshSourceState {
    let metadata = factory.metadata();
    let connection_id = match metadata.id.parse::<ConnectionId>() {
        Ok(id) => id,
        Err(_) => {
            return RefreshSourceState {
                display_name: display_name.to_string(),
                status: RefreshSourceStatus::Failed("invalid connection id".to_string()),
                model_count: None,
                added: Vec::new(),
                removed: Vec::new(),
            };
        }
    };
    // Read the old snapshot before overwriting it so we can diff.
    let old = catalog
        .live_availability_snapshot()
        .into_iter()
        .find(|(id, _)| id == &connection_id)
        .map(|(_, avail)| avail);
    let result = async {
        let availability = fetch_availability(factory.as_ref(), session).await?;
        let refreshed = availability.models.len();
        let (added, removed) = match &old {
            Some(prev) => model_id_diff(prev, &availability),
            None => (Vec::new(), Vec::new()),
        };
        catalog.write_live_availability(&connection_id, availability)?;
        Ok::<(usize, Vec<String>, Vec<String>), anyhow::Error>((refreshed, added, removed))
    }
    .await;
    match result {
        Ok((count, added, removed)) => RefreshSourceState {
            display_name: display_name.to_string(),
            status: RefreshSourceStatus::Ok,
            model_count: Some(count),
            added,
            removed,
        },
        Err(err) => RefreshSourceState {
            display_name: display_name.to_string(),
            status: RefreshSourceStatus::Failed(format!("{err:#}")),
            model_count: None,
            added: Vec::new(),
            removed: Vec::new(),
        },
    }
}

/// Force-refresh live model availability for every authorized provider.
/// `/refresh` is the manual override, so unlike the picker's TTL-gated pass
/// it always hits the network. Best-effort per provider: one failing
/// endpoint doesn't block the rest, and the summary names each outcome.
///
/// This is the blocking summary-string path used by headless (`bonsai -p
/// /refresh`) and shared command fallback. The TUI uses the incremental
/// [`refresh_sources_incremental`] / [`refresh_models_dev_source`] pair so it
/// can render live per-source status.
pub(crate) async fn refresh_all_provider_models(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: &ModelCatalog,
) -> Result<String> {
    let models_dev_outcome = catalog.refresh_models_dev_metadata().await;
    let providers: Vec<_> = registry
        .all()
        .iter()
        .filter_map(|factory| {
            let metadata = factory.metadata();
            let session = session_store.providers.get(metadata.id.as_ref())?.clone();
            factory
                .is_authorized(&session)
                .then(|| (factory.clone(), session))
        })
        .collect();
    if providers.is_empty() && matches!(&models_dev_outcome, Ok(None)) {
        anyhow::bail!("No authorized providers to refresh. Try `/authorize <provider>`.");
    }

    let outcomes =
        futures::future::join_all(providers.into_iter().map(|(factory, session)| async move {
            let metadata = factory.metadata();
            let outcome = async {
                let availability = fetch_availability(factory.as_ref(), &session).await?;
                let refreshed = availability.models.len();
                let connection_id = metadata.id.parse::<ConnectionId>()?;
                catalog.write_live_availability(&connection_id, availability)?;
                Ok::<usize, anyhow::Error>(refreshed)
            }
            .await;
            (metadata.display_name.to_string(), outcome)
        }))
        .await;

    let mut lines = Vec::new();
    let mut refreshed_sources = 0usize;
    match models_dev_outcome {
        Ok(Some(models)) => {
            refreshed_sources += 1;
            lines.push(format!("Models.dev: {models} models"));
        }
        Ok(None) => {}
        Err(_) => lines.push(
            catalog
                .models_dev_refresh_notice()
                .unwrap_or_else(|| "Models.dev: refresh failed; using cached metadata".to_string()),
        ),
    }
    for (display_name, outcome) in outcomes {
        match outcome {
            Ok(models) => {
                refreshed_sources += 1;
                lines.push(format!("{display_name}: {models} models"));
            }
            Err(err) => lines.push(format!("{display_name}: failed ({err:#})")),
        }
    }
    if refreshed_sources == 0 {
        anyhow::bail!(
            "Refresh failed for every model source — {}",
            lines.join("; ")
        );
    }
    Ok(format!("Refreshed model catalogs — {}", lines.join("; ")))
}

pub(in crate::commands) fn resolve_provider(
    registry: &ProviderRegistry,
    input: &str,
) -> Option<std::sync::Arc<dyn crate::provider::ProviderFactory>> {
    let input = input.trim();
    if let Ok(index) = input.parse::<usize>()
        && index > 0
    {
        return registry.all().get(index - 1).cloned();
    }

    registry.get(input).or_else(|| {
        registry
            .all()
            .iter()
            .find(|factory| factory.metadata().display_name.eq_ignore_ascii_case(input))
            .cloned()
    })
}

/// The "unknown provider" error, shared by `/authorize`, `/unauthorize`, and
/// `/model` when an id doesn't resolve.
pub(in crate::commands) fn unknown_provider_error(
    input: &str,
    registry: &ProviderRegistry,
) -> CommandMessage {
    error(format!(
        "Unknown provider '{}'. Available: {}",
        input,
        provider_choices(registry)
    ))
}

fn provider_choices(registry: &ProviderRegistry) -> String {
    registry
        .all()
        .iter()
        .enumerate()
        .map(|(index, factory)| {
            format!(
                "{}. {} ({})",
                index + 1,
                factory.metadata().display_name,
                factory.metadata().id
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(in crate::commands) fn authorize_provider_choices(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
) -> Vec<ProviderOption> {
    let current = session_store.current_kind_id();
    registry
        .all()
        .iter()
        .map(|factory| {
            let metadata = factory.metadata();
            ProviderOption {
                provider_id: metadata.id.to_string(),
                provider_label: metadata.display_name.to_string(),
                authorized: factory.is_authorized(session_store.session(&metadata.id)),
                current: metadata.id.as_ref() == current,
                uses_endpoint_auth_form: metadata.auth_requirement.uses_endpoint_setup(),
            }
        })
        .collect()
}

/// Provider options for `/unauthorize` — only providers that currently hold
/// stored credentials can be cleared, so the picker hides the rest.
pub(in crate::commands) fn unauthorize_provider_choices(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
) -> Vec<ProviderOption> {
    authorize_provider_choices(registry, session_store)
        .into_iter()
        .filter(|provider| provider.authorized)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_catalog::{AvailableModel, LiveModelAvailability};

    fn avail(ids: &[&str]) -> LiveModelAvailability {
        LiveModelAvailability {
            models: ids.iter().map(|id| AvailableModel::remote(*id)).collect(),
            ..LiveModelAvailability::default()
        }
    }

    #[test]
    fn is_local_endpoint_matches_loopback_and_local_hosts() {
        for url in [
            "http://localhost:11434/v1",
            "localhost:11434/v1",
            "http://127.0.0.1:1234/v1",
            "http://127.5.6.7/v1",
            "http://[::1]:8080/v1",
            "http://0.0.0.0:9999/v1",
            "http://my-box.local/v1",
            "http://LOCALHOST/v1",
        ] {
            assert!(is_local_endpoint(url), "expected local: {url}");
        }
    }

    #[test]
    fn is_local_endpoint_rejects_remote_and_empty() {
        for url in [
            "https://api.openai.com/v1",
            "https://api.anthropic.com",
            "http://192.168.1.10/v1",
            "http://example.com",
            "",
            "   ",
        ] {
            assert!(!is_local_endpoint(url), "expected remote: {url:?}");
        }
    }

    #[test]
    fn model_id_diff_finds_added_and_removed() {
        let old = avail(&["a", "b", "c"]);
        let new = avail(&["b", "c", "d"]);
        let (mut added, mut removed) = model_id_diff(&old, &new);
        added.sort();
        removed.sort();
        assert_eq!(added, vec!["d".to_string()]);
        assert_eq!(removed, vec!["a".to_string()]);
    }

    #[test]
    fn model_id_diff_no_changes() {
        let old = avail(&["x", "y"]);
        let new = avail(&["x", "y"]);
        let (added, removed) = model_id_diff(&old, &new);
        assert!(added.is_empty());
        assert!(removed.is_empty());
    }

    #[test]
    fn model_id_diff_all_new() {
        let old = avail(&[]);
        let new = avail(&["a", "b"]);
        let (mut added, removed) = model_id_diff(&old, &new);
        added.sort();
        assert_eq!(added, vec!["a".to_string(), "b".to_string()]);
        assert!(removed.is_empty());
    }
}
