//! Headless `/providers` management: list, remove, disable, and enable
//! catalog providers from the command surface. The TUI routes `/providers`
//! through its own modal flow (`tui::runtime_actions::handle_providers_command`);
//! this module covers `bonsai -p` and other non-interactive callers, where the
//! process exits after the command so no in-process registry rebuild is needed.

use super::{CommandMessage, error, status};
use crate::model_catalog::{CatalogPaths, ConnectionId, ModelCatalog};
use crate::provider::ProviderRegistry;
use crate::session::SessionStore;
use crate::tui::provider_manager::{ProviderOrigin, provider_manager_rows};

pub(crate) async fn providers_command_messages(
    args: &[&str],
    session_store: &mut SessionStore,
    registry: &ProviderRegistry,
    catalog: Option<&ModelCatalog>,
) -> Vec<CommandMessage> {
    let home_dir = match crate::storage::BonsaiPaths::discover() {
        Ok(paths) => paths.home_dir().to_path_buf(),
        Err(err) => return vec![error(format!("Provider catalog unavailable: {err:#}"))],
    };
    let Some(catalog) = catalog else {
        return vec![error("Provider catalog unavailable".to_string())];
    };

    match args {
        [] | ["list"] => {
            vec![status(format_provider_list(
                registry,
                session_store,
                catalog,
            ))]
        }
        ["add"] | ["edit", ..] => vec![error(
            "Adding or editing providers is interactive — run /providers inside the bonsai TUI."
                .to_string(),
        )],
        ["remove" | "disable" | "enable", id]
            if id
                .parse::<ConnectionId>()
                .ok()
                .is_some_and(|connection_id| {
                    catalog.connection(&connection_id).is_some()
                        && catalog.connection_source(&connection_id)
                            == crate::model_catalog::SourceKind::Project
                }) =>
        {
            vec![error(format!(
                "`{id}` is managed by trusted project catalog files; edit `.bonsai/providers` or `.bonsai/models` instead."
            ))]
        }
        ["remove", id] => match id.parse::<ConnectionId>() {
            Ok(connection_id) => {
                remove_provider(&home_dir, &connection_id, session_store, registry, catalog).await
            }
            Err(err) => vec![error(format!("Invalid provider id `{id}`: {err}"))],
        },
        ["disable", id] => set_provider_enabled(&home_dir, id, false),
        ["enable", id] => set_provider_enabled(&home_dir, id, true),
        _ => vec![error(
            "Usage: /providers [list|add|edit <id>|remove <id>|disable <id>|enable <id>]"
                .to_string(),
        )],
    }
}

pub(crate) fn format_provider_list(
    registry: &ProviderRegistry,
    session_store: &SessionStore,
    catalog: &ModelCatalog,
) -> String {
    let rows = provider_manager_rows(registry, session_store, catalog);
    let mut lines = vec!["Providers:".to_string()];
    for (index, row) in rows.iter().enumerate() {
        let origin = match row.origin {
            ProviderOrigin::BuiltIn => "built-in",
            ProviderOrigin::Custom => "custom",
            ProviderOrigin::Project => "project",
        };
        let current = if row.current { " (current)" } else { "" };
        let hint = row
            .auth_hint
            .as_deref()
            .map(|hint| format!(" — {hint}"))
            .unwrap_or_default();
        lines.push(format!(
            "{}. {} ({}){current} — {origin} · {}{hint}",
            index + 1,
            row.display_name,
            row.connection_id,
            row.status_label(),
        ));
    }
    if let Some(notice) = catalog.models_dev_refresh_notice() {
        lines.push(format!("Catalog notice: {notice}"));
    }
    lines.push(
        "Use /providers add|edit in the TUI; remove/disable/enable work here too.".to_string(),
    );
    lines.join("\n")
}

async fn remove_provider(
    home_dir: &std::path::Path,
    connection_id: &ConnectionId,
    session_store: &mut SessionStore,
    registry: &ProviderRegistry,
    catalog: &ModelCatalog,
) -> Vec<CommandMessage> {
    let was_current = session_store
        .current_kind_id()
        .eq_ignore_ascii_case(connection_id.as_str());
    if let Err(err) = session_store
        .clear_provider_credential(connection_id.as_str())
        .await
    {
        return vec![error(format!(
            "Remove failed before the provider was changed: {err:#}"
        ))];
    }
    // Persist the conservative unauthorized state before deleting catalog
    // files. If a later step fails, restart cannot resolve an old credential
    // reference and silently re-authorize the provider.
    if let Err(err) = session_store.save_allowing_auth_clear_async().await {
        return vec![error(format!(
            "Stored credentials were removed, but the unauthorized provider state could not be saved: {err:#}"
        ))];
    }
    if let Err(err) = crate::model_catalog::remove_local_catalog_entry(home_dir, connection_id) {
        return vec![error(format!(
            "Stored credentials were removed, but provider catalog removal failed: {err:#}"
        ))];
    }
    let mut messages = vec![status(format!("Removed provider `{connection_id}`."))];
    if let Err(err) = catalog.clear_live_availability(connection_id) {
        messages.push(error(format!(
            "Removed, but the live-model cache could not be cleared: {err:#}"
        )));
    }
    session_store.providers.remove(connection_id.as_str());
    if was_current {
        // Pre-mutation registry still lists the removed provider; skip it when
        // picking the fallback.
        let fallback = registry
            .all()
            .iter()
            .filter(|factory| !factory.metadata().is_known_id(connection_id.as_str()))
            .find(|factory| {
                session_store
                    .providers
                    .get(factory.metadata().id.as_ref())
                    .is_some_and(|provider| factory.is_authorized(provider))
            })
            .map(|factory| factory.metadata().id.to_string());
        if let Some(fallback) = fallback {
            session_store.ensure_provider(&fallback);
            session_store.set_current_kind_id(&fallback);
            messages.push(status(format!(
                "Active provider removed; switched to `{fallback}`."
            )));
        }
    }
    if let Err(err) = session_store.save_allowing_auth_clear_async().await {
        messages.push(error(format!("Failed to save session state: {err:#}")));
    }
    messages
}

fn set_provider_enabled(
    home_dir: &std::path::Path,
    id: &str,
    enabled: bool,
) -> Vec<CommandMessage> {
    let connection_id = match id.parse::<ConnectionId>() {
        Ok(connection_id) => connection_id,
        Err(err) => return vec![error(format!("Invalid provider id `{id}`: {err}"))],
    };
    // Refuse disabling custom providers up front for a clearer message than
    // the writer's shared-file error.
    if !enabled
        && CatalogPaths::from_home_dir(home_dir)
            .provider_dir
            .join(format!("{id}.toml"))
            .exists()
    {
        return vec![error(format!(
            "`{id}` is a custom provider; remove it instead of disabling."
        ))];
    }
    match crate::model_catalog::set_builtin_connection_enabled(home_dir, &connection_id, enabled) {
        Ok(_) => {
            let verb = if enabled { "Enabled" } else { "Disabled" };
            vec![status(format!("{verb} provider `{connection_id}`."))]
        }
        Err(err) => vec![error(format!(
            "{} failed: {err:#}",
            if enabled { "Enable" } else { "Disable" }
        ))],
    }
}
