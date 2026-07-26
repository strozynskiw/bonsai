//! Provider-manager runtime actions.

use super::*;

// ── /providers manager ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum ProviderMutation {
    /// Delete a custom provider's catalog files, credentials, and live cache.
    Remove(crate::model_catalog::ConnectionId),
    /// Hide a built-in behind an `enabled = false` user patch (credentials kept).
    Disable(crate::model_catalog::ConnectionId),
    /// Delete the disable patch written by `Disable`.
    Enable(crate::model_catalog::ConnectionId),
}

impl ProviderMutation {
    fn connection_id(&self) -> &crate::model_catalog::ConnectionId {
        match self {
            Self::Remove(id) | Self::Disable(id) | Self::Enable(id) => id,
        }
    }
}

/// Open the `/providers` manager modal with freshly built rows.
pub(super) async fn open_provider_manager(app: &mut AppState, deps: &RuntimeActionDeps<'_>) {
    let rows = {
        let session = deps.session_store.lock().await;
        crate::tui::provider_manager::provider_manager_rows(
            &deps.registry,
            &session,
            &deps.model_catalog,
        )
    };
    app.reduce(AppAction::OpenModal(ModalKind::ProviderManager {
        rows,
        filter: String::new(),
        searching: false,
        cursor: 0,
    }));
}

fn selected_provider_row(
    app: &AppState,
) -> Option<crate::tui::provider_manager::ProviderManagerRow> {
    let Some(ModalKind::ProviderManager {
        rows,
        filter,
        cursor,
        ..
    }) = app.modal.as_ref()
    else {
        return None;
    };
    // `cursor` indexes the filtered view; map it back to the underlying row.
    crate::tui::provider_manager::provider_manager_filtered(rows, filter)
        .get(*cursor)
        .copied()
        .cloned()
}

pub(super) fn open_provider_manager_edit(app: &mut AppState, deps: &RuntimeActionDeps<'_>) {
    use crate::tui::provider_manager::ProviderOrigin;
    let Some(row) = selected_provider_row(app) else {
        return;
    };
    match row.origin {
        ProviderOrigin::Custom => {}
        // Non-editable providers open the read-only detail view — which carries
        // the /authorize + disable guidance — instead of pushing an identical
        // status line to the transcript on every keypress.
        ProviderOrigin::Project | ProviderOrigin::BuiltIn => {
            open_provider_detail(app, deps);
            return;
        }
    }
    match crate::tui::local_model_wizard::wizard_state_for_provider_id(
        &row.connection_id,
        &deps.model_catalog,
        deps.storage.home_dir(),
        app.credential_persistence,
    ) {
        Ok(state) => app.reduce(AppAction::OpenModal(ModalKind::LocalModelWizard {
            state: Box::new(state),
        })),
        Err(message) => push_command_message(app, CommandOutputKind::Error, &message),
    }
}

/// Open the read-only detail view for the selected provider, resolving its
/// metadata (env vars, endpoint, transport) and available models. The manager's
/// list and cursor ride along so Esc restores it losslessly.
pub(super) fn open_provider_detail(app: &mut AppState, deps: &RuntimeActionDeps<'_>) {
    let (row, cursor, return_rows, return_filter) = {
        let Some(ModalKind::ProviderManager {
            rows,
            filter,
            cursor,
            ..
        }) = app.modal.as_ref()
        else {
            return;
        };
        // `cursor` indexes the filtered view; resolve the underlying row there.
        let Some(row) = crate::tui::provider_manager::provider_manager_filtered(rows, filter)
            .get(*cursor)
            .copied()
            .cloned()
        else {
            return;
        };
        (row, *cursor, rows.clone(), filter.clone())
    };

    let metadata = deps
        .registry
        .lookup(&row.connection_id)
        .map(|factory| factory.metadata());
    let models = row
        .connection_id
        .parse::<crate::model_catalog::ConnectionId>()
        .map(|connection_id| {
            let fallback = metadata
                .map(|metadata| metadata.seed_model_list())
                .unwrap_or_default();
            deps.model_catalog
                .available_models_for_connection(&connection_id, fallback)
        })
        .unwrap_or_default();

    let detail = crate::tui::provider_manager::build_provider_detail(
        &row,
        metadata,
        models,
        return_rows,
        cursor,
        return_filter,
    );
    app.reduce(AppAction::OpenModal(ModalKind::ProviderDetail {
        detail: Box::new(detail),
    }));
    app.modal_scroll = 0;
}

pub(super) fn provider_manager_remove_selected(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    use crate::tui::provider_manager::ProviderOrigin;
    let Some(row) = selected_provider_row(app) else {
        return;
    };
    let Ok(connection_id) = row
        .connection_id
        .parse::<crate::model_catalog::ConnectionId>()
    else {
        return;
    };
    match row.origin {
        ProviderOrigin::Custom => {
            app.reduce(AppAction::OpenModal(ModalKind::ProviderRemoveConfirm {
                connection_id: row.connection_id,
                display_name: row.display_name,
                disable_builtin: false,
            }));
        }
        ProviderOrigin::Project => push_command_message(
            app,
            CommandOutputKind::Status,
            "Project providers are managed by trusted `.bonsai/providers` and `.bonsai/models` files.",
        ),
        ProviderOrigin::BuiltIn if row.enabled => {
            app.reduce(AppAction::OpenModal(ModalKind::ProviderRemoveConfirm {
                connection_id: row.connection_id,
                display_name: row.display_name,
                disable_builtin: true,
            }));
        }
        // Re-enabling a disabled built-in needs no confirmation.
        ProviderOrigin::BuiltIn => {
            start_provider_mutation(app, deps, ProviderMutation::Enable(connection_id));
        }
    }
}

pub(super) fn submit_provider_remove_confirm(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(ModalKind::ProviderRemoveConfirm {
        connection_id,
        disable_builtin,
        ..
    }) = app.modal.as_ref()
    else {
        return;
    };
    let Ok(connection_id) = connection_id.parse::<crate::model_catalog::ConnectionId>() else {
        return;
    };
    let mutation = if *disable_builtin {
        ProviderMutation::Disable(connection_id)
    } else {
        ProviderMutation::Remove(connection_id)
    };
    start_provider_mutation(app, deps, mutation);
}

/// Handle a `/providers [subcommand]` invocation from the idle Submit arm.
pub(in crate::tui) async fn handle_providers_command(
    args: &str,
    app: &mut AppState,
    deps: RuntimeActionDeps<'_>,
) {
    let mut parts = args.split_whitespace();
    let subcommand = parts.next().unwrap_or("");
    let id_arg = parts.next().unwrap_or("");
    match (subcommand, id_arg) {
        ("" | "list", _) => open_provider_manager(app, &deps).await,
        ("add", _) => {
            app.reduce(AppAction::OpenModal(ModalKind::LocalModelWizard {
                state: Box::new(LocalModelWizardState::with_persistence(
                    app.credential_persistence,
                )),
            }));
        }
        ("edit", id) if !id.is_empty() => {
            match crate::tui::local_model_wizard::wizard_state_for_provider_id(
                id,
                &deps.model_catalog,
                deps.storage.home_dir(),
                app.credential_persistence,
            ) {
                Ok(state) => {
                    app.reduce(AppAction::OpenModal(ModalKind::LocalModelWizard {
                        state: Box::new(state),
                    }));
                }
                Err(message) => push_command_message(app, CommandOutputKind::Error, &message),
            }
        }
        ("remove" | "disable" | "enable", id) if !id.is_empty() => {
            open_provider_mutation_for_args(subcommand, id, app, deps);
        }
        _ => {
            push_command_message(
                app,
                CommandOutputKind::Error,
                "Usage: /providers [list|add|edit <id>|remove <id>|disable <id>|enable <id>]",
            );
        }
    }
}

/// Route `/providers remove|disable|enable <id>` to the right confirm modal or
/// mutation, enforcing custom-vs-built-in semantics.
pub(super) fn open_provider_mutation_for_args(
    subcommand: &str,
    id: &str,
    app: &mut AppState,
    deps: RuntimeActionDeps<'_>,
) {
    let Ok(connection_id) = id.parse::<crate::model_catalog::ConnectionId>() else {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Invalid provider id `{id}`."),
        );
        return;
    };
    let is_builtin = provider_is_builtin(&connection_id);
    let source = deps.model_catalog.connection_source(&connection_id);
    let is_project = source == crate::model_catalog::SourceKind::Project;
    let is_custom = !is_builtin && source == crate::model_catalog::SourceKind::User;
    let display_name = deps
        .model_catalog
        .connection(&connection_id)
        .map(|connection| connection.display_name.to_string())
        .or_else(|| {
            builtin_provider_connection(&connection_id)
                .map(|connection| connection.display_name.to_string())
        })
        .unwrap_or_else(|| id.to_string());
    match subcommand {
        "remove" | "disable" | "enable" if is_project => push_command_message(
            app,
            CommandOutputKind::Error,
            &format!(
                "`{id}` is managed by trusted project catalog files; edit `.bonsai/providers` or `.bonsai/models` instead."
            ),
        ),
        "remove" if is_custom => {
            app.reduce(AppAction::OpenModal(ModalKind::ProviderRemoveConfirm {
                connection_id: id.to_string(),
                display_name,
                disable_builtin: false,
            }));
        }
        "remove" => push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("`{id}` is not a custom provider; built-ins can only be disabled."),
        ),
        "disable" if !is_custom => {
            app.reduce(AppAction::OpenModal(ModalKind::ProviderRemoveConfirm {
                connection_id: id.to_string(),
                display_name,
                disable_builtin: true,
            }));
        }
        "disable" => push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("`{id}` is a custom provider; remove it instead of disabling."),
        ),
        "enable" => start_provider_mutation(app, deps, ProviderMutation::Enable(connection_id)),
        _ => {}
    }
}

pub(super) fn provider_is_builtin(connection_id: &crate::model_catalog::ConnectionId) -> bool {
    builtin_provider_connection(connection_id).is_some()
}

fn builtin_provider_connection(
    connection_id: &crate::model_catalog::ConnectionId,
) -> Option<crate::model_catalog::ConnectionSpec> {
    crate::model_catalog::ModelCatalog::load_builtin()
        .ok()
        .and_then(|catalog| catalog.connection(connection_id).cloned())
}

/// Apply a provider mutation off-thread, then reload the catalog/registry and
/// rebuild the agent provider — mirroring `start_local_model_commit`. The
/// manager modal reopens with fresh rows via the outcome's `open_modal`.
fn start_provider_mutation(
    app: &mut AppState,
    deps: RuntimeActionDeps<'_>,
    mutation: ProviderMutation,
) {
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Provider changes cannot run while the agent is running.",
        );
        return;
    }
    let home_dir = deps.storage.home_dir().to_path_buf();
    let trusted_project_root = deps
        .model_catalog
        .trusted_project_root()
        .map(Path::to_path_buf);
    let session_store = deps.session_store;
    let agent = deps.agent;
    let sender = deps.runtime_sender;

    app.reduce(AppAction::CloseModal);
    app.reduce(AppAction::SetTaskState(TaskState::Command));
    let generation = app.command_generation();
    spawn_panicked(async move {
        let result = async {
            let status = match &mutation {
                ProviderMutation::Remove(id) => {
                    {
                        let mut session = session_store.lock().await;
                        session.clear_provider_credential(id.as_str()).await?;
                        // Make unauthorized state durable before catalog files
                        // disappear. A later catalog error is retryable and
                        // cannot resurrect a stored credential on restart.
                        session.save_allowing_auth_clear_async().await?;
                    }
                    crate::model_catalog::remove_local_catalog_entry(&home_dir, id)?;
                    format!("Removed provider `{id}`.")
                }
                ProviderMutation::Disable(id) => {
                    crate::model_catalog::set_builtin_connection_enabled(&home_dir, id, false)?;
                    format!("Disabled provider `{id}`.")
                }
                ProviderMutation::Enable(id) => {
                    crate::model_catalog::set_builtin_connection_enabled(&home_dir, id, true)?;
                    format!("Enabled provider `{id}`.")
                }
            };
            let model_catalog = Arc::new(crate::model_catalog::load_catalog_from_home_and_project(
                &home_dir,
                trusted_project_root.as_deref(),
            )?);
            let registry = Arc::new(ProviderRegistry::from_catalog(&model_catalog));
            let mut messages = vec![CommandOutputEvent {
                kind: CommandOutputKind::Status,
                text: status,
            }];
            let (provider_selection, context_report, rows) = {
                // Canonical lock order: session_store before agent (see `task.rs`).
                let mut session = session_store.lock().await;
                let previous_id = session.current_kind_id().to_string();
                if let ProviderMutation::Remove(id) = &mutation {
                    // Credential cleanup committed before the catalog change;
                    // now drop the provider row and derived availability.
                    let _ = model_catalog.clear_live_availability(id);
                    session.providers.remove(id.as_str());
                }
                // The active provider may have been removed or disabled; fall
                // back to the first authorized provider, then the registry
                // head. `previous_id` is captured before the mutation because
                // `current_kind_id()` silently falls back to the default once
                // the provider's session row is gone.
                let was_current = previous_id
                    .eq_ignore_ascii_case(mutation.connection_id().as_str())
                    && !matches!(mutation, ProviderMutation::Enable(_));
                if was_current || registry.lookup(session.current_kind_id()).is_none() {
                    let fallback = registry
                        .all()
                        .iter()
                        .find(|factory| {
                            session
                                .providers
                                .get(factory.metadata().id.as_ref())
                                .is_some_and(|provider| factory.is_authorized(provider))
                        })
                        .map(|factory| factory.metadata().id.to_string())
                        .or_else(|| registry.ids().first().map(|id| (*id).to_string()));
                    if let Some(fallback) = fallback {
                        session.ensure_provider(&fallback);
                        session.set_current_kind_id(&fallback);
                        messages.push(CommandOutputEvent {
                            kind: CommandOutputKind::Status,
                            text: format!("Active provider changed; switched to `{fallback}`."),
                        });
                    }
                }
                // Removal drops stored credentials, so the save must be allowed
                // to clear auth (mirrors `unauthorize_provider`).
                session.save_allowing_auth_clear_async().await?;
                let selection = ProviderRunSelection {
                    provider: session.provider_label().to_string(),
                    model: session.current_session().model.clone(),
                    reasoning: session.current_session().reasoning,
                };
                let mut agent_guard = agent.lock().await;
                crate::commands::rebuild_agent_provider(
                    &mut agent_guard,
                    &registry,
                    &session,
                    Some(&model_catalog),
                );
                let context_report = agent_guard.context_report();
                let rows = crate::tui::provider_manager::provider_manager_rows(
                    &registry,
                    &session,
                    &model_catalog,
                );
                (selection, context_report, rows)
            };
            Ok::<_, anyhow::Error>((
                model_catalog,
                registry,
                provider_selection,
                context_report,
                messages,
                rows,
            ))
        }
        .await;

        let event = match result {
            Ok((model_catalog, registry, provider, context_report, messages, rows)) => {
                RuntimeEvent::CatalogReloaded {
                    model_catalog,
                    registry,
                    outcome: Box::new(CommandOutcomeEvent::Applied {
                        generation: Some(generation),
                        clear_transcript: false,
                        messages,
                        provider: Some(Box::new(provider)),
                        context_report: Some(Box::new(context_report)),
                        quit: false,
                        open_modal: Some(ModalKind::ProviderManager {
                            rows,
                            filter: String::new(),
                            searching: false,
                            cursor: 0,
                        }),
                    }),
                }
            }
            Err(err) => RuntimeEvent::CommandFinished(Box::new(CommandOutcomeEvent::Applied {
                generation: Some(generation),
                clear_transcript: false,
                messages: vec![CommandOutputEvent {
                    kind: CommandOutputKind::Error,
                    text: format!("Provider change failed: {err:#}"),
                }],
                provider: None,
                context_report: None,
                quit: false,
                open_modal: None,
            })),
        };
        let _ = sender.send(event);
    });
}
