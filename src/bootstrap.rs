use std::path::Path;
use std::sync::Arc;

use crate::model_catalog::ModelCatalog;
use crate::provider::ProviderRegistry;
use crate::session::SessionStore;
use crate::storage::Storage;
use crate::tool::SharedActiveSessionId;
use tokio::sync::Mutex;

pub(crate) use crate::tui::run::TuiRunContext;

pub(crate) async fn run_tui(
    resume: Option<crate::cli::ResumeTarget>,
    run_context: Option<TuiRunContext>,
    accessibility: crate::tui::accessibility::TuiAccessibility,
) -> anyhow::Result<()> {
    let storage = Storage::open().await?;
    let run_budget = storage.run_budget().await?;
    let default_root = std::env::current_dir()?;
    let run_context = run_context.unwrap_or_else(|| TuiRunContext::for_project(default_root));
    let project_root = run_context.workspace_root().to_path_buf();
    let session_project_root = run_context.session_project_root().to_path_buf();
    let project_id = storage.ensure_project(&session_project_root).await?;
    // Workspace trust is resolved before every project-owned configuration
    // surface, including provider and model catalog files.
    let workspace_trust =
        crate::workspace_trust::WorkspaceTrustGate::load(storage.clone(), project_id).await?;
    let workspace_trusted = matches!(
        workspace_trust.state(),
        crate::workspace_trust::WorkspaceTrust::Trusted
    );
    let model_catalog = Arc::new(crate::model_catalog::load_catalog_with_builtin_fallback(
        storage.home_dir(),
        workspace_trusted.then_some(project_root.as_path()),
    )?);
    crate::model_catalog::spawn_models_dev_refresh(
        storage.home_dir().to_path_buf(),
        model_catalog.clone(),
    );
    let registry = Arc::new(ProviderRegistry::from_catalog(&model_catalog));
    let session_store =
        SessionStore::load_with_storage_and_catalog(&storage, Some(&model_catalog)).await?;
    refresh_live_model_caches_at_startup(
        registry.clone(),
        session_store.clone(),
        model_catalog.clone(),
    );
    log_loaded_session(&registry, &session_store, storage.db_path());
    let session_store = Arc::new(Mutex::new(session_store));

    // Custom themes load synchronously here — before the event loop
    // restores the persisted theme — so a persisted custom theme resolves before
    // the first frame. Malformed files are skipped with a warning; the same
    // errors surface in-TUI the next time `/theme` triggers a rescan.
    for theme_error in crate::tui::theme::init_theme_files(&project_root, storage.home_dir()) {
        tracing::warn!(%theme_error, "skipping invalid theme file");
    }
    let (interaction, interaction_rx) = crate::interaction::InteractionService::new();
    let interaction = Arc::new(interaction);
    let active_session_id: SharedActiveSessionId = Arc::new(tokio::sync::Mutex::new(None));
    let approval_level = storage
        .approval_level()
        .await
        .unwrap_or(None)
        .unwrap_or(crate::tool::ApprovalLevel::Balanced);
    let yolo_mode = crate::yolo::YoloMode::with_level(approval_level);
    let runtime = crate::runtime::RuntimeBuilder::new(crate::runtime::RuntimeBuildRequest {
        surface: crate::runtime::SurfaceKind::Tui,
        storage,
        project_root: project_root.clone(),
        session_project_root: session_project_root.clone(),
        model_catalog,
        registry,
        session_store,
        run_budget,
        interaction,
        active_session_id,
        yolo_mode,
        current_session_id: None,
        system_prompt_suffix: None,
        workspace_trust,
    })
    .build()
    .await?;
    let tui_runtime = crate::tui::run::TuiRuntime::new(
        runtime,
        interaction_rx,
        resume,
        run_context,
        accessibility,
    );

    crate::tui::run::run_tui(tui_runtime).await
}

async fn refresh_active_codex_cache_schema(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: &ModelCatalog,
) {
    if session_store.current_kind_id() != "codex" {
        return;
    }
    let Some(factory) = registry.get("codex") else {
        return;
    };
    let session = session_store.session("codex");
    if !factory.is_authorized(session) {
        return;
    }
    let Some(connection_id) = crate::model_catalog::connection_id_for_provider_id("codex") else {
        return;
    };
    if catalog.has_current_live_availability_schema(&connection_id) {
        return;
    }
    if let Err(err) =
        crate::commands::refresh_stale_model_cache(registry, "codex", session, Some(catalog)).await
    {
        tracing::warn!(error = %err, "failed to refresh legacy Codex model metadata at startup");
    }
}

fn refresh_live_model_caches_at_startup(
    registry: Arc<ProviderRegistry>,
    session_store: SessionStore,
    catalog: Arc<ModelCatalog>,
) {
    tokio::spawn(async move {
        tokio::join!(
            refresh_active_codex_cache_schema(&registry, &session_store, &catalog),
            crate::commands::refresh_stale_authorized_provider_models(
                &registry,
                &session_store,
                &catalog,
            ),
        );
    });
}

pub(crate) use crate::tool::registry_assembly::{
    SessionTitleToolDeps, ToolRegistryDeps, build_tool_registries,
};

fn log_loaded_session(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    storage_path: &Path,
) {
    let current_provider = session_store.current_kind_id().to_string();
    let current_model = session_store.current_session().model.clone();
    let authorized_provider_ids = registry
        .all()
        .iter()
        .filter_map(|factory| {
            let id = factory.metadata().id.as_ref();
            factory
                .is_authorized(session_store.session(id))
                .then_some(id)
        })
        .collect::<Vec<_>>()
        .join(",");

    tracing::info!(
        database = %storage_path.display(),
        current_provider = %current_provider,
        current_model = %current_model,
        authorized_provider_ids = %authorized_provider_ids,
        provider_state = %session_store.provider_state_summary(),
        "loaded provider session state"
    );
}

#[cfg(test)]
mod tests {
    use crate::provider::ProviderRegistry;

    #[test]
    fn registry_has_all_builtin_providers() {
        let registry = ProviderRegistry::default_registry();
        let ids = registry.ids();
        assert!(ids.contains(&"opencode"));
        assert!(ids.contains(&"codex"));
        assert!(ids.contains(&"anthropic"));
        assert!(ids.contains(&"minimax-coding-plan"));
    }
}
