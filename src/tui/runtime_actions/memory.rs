//! Memory-manager runtime actions.

use super::*;

fn selected_memory_identity(app: &AppState) -> Option<(MemoryTier, String)> {
    let Some(ModalKind::Manager(crate::tui::event::ManagerModal::MemoryManager { rows, cursor })) =
        app.modal.as_ref()
    else {
        return None;
    };
    rows.get(*cursor).map(|row| (row.tier, row.name.clone()))
}

fn selected_memory_cursor(app: &AppState) -> usize {
    match app.modal.as_ref() {
        Some(ModalKind::Manager(crate::tui::event::ManagerModal::MemoryManager {
            cursor, ..
        })) => *cursor,
        _ => 0,
    }
}

fn open_memory_manager(
    app: &mut AppState,
    rows: Vec<crate::memory::entry::MemoryEntry>,
    selection: Option<(MemoryTier, String)>,
) {
    let cursor = selection
        .and_then(|(tier, name)| {
            rows.iter()
                .position(|entry| entry.tier == tier && entry.name == name)
        })
        .unwrap_or(0)
        .min(rows.len().saturating_sub(1));
    app.reduce(AppAction::OpenModal(ModalKind::Manager(
        crate::tui::event::ManagerModal::MemoryManager { rows, cursor },
    )));
}

pub(super) async fn open_memory_add_wizard(app: &mut AppState) {
    app.reduce(AppAction::OpenModal(ModalKind::Wizard(
        crate::tui::event::WizardModal::MemoryAddWizard {
            state: Box::new(crate::tui::memory_manager::MemoryAddWizardState::default()),
        },
    )));
}

/// Reopen the wizard in edit mode, prefilled from the selected manager row
/// (rows carry the full entry, so no store lookup is needed).
pub(super) fn open_memory_edit_wizard(app: &mut AppState) {
    let Some(ModalKind::Manager(crate::tui::event::ManagerModal::MemoryManager { rows, cursor })) =
        app.modal.as_ref()
    else {
        return;
    };
    let Some(entry) = rows.get(*cursor) else {
        return;
    };
    let state = crate::tui::memory_manager::MemoryAddWizardState::for_edit(entry);
    app.reduce(AppAction::OpenModal(ModalKind::Wizard(
        crate::tui::event::WizardModal::MemoryAddWizard {
            state: Box::new(state),
        },
    )));
}

pub(super) async fn submit_memory_add_wizard(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard { state })) =
        app.modal.as_ref()
    else {
        return;
    };
    if !matches!(
        state.step,
        crate::tui::memory_manager::MemoryWizardStep::Review
    ) {
        // Pre-review, Submit advances the wizard step — reducer-owned. This
        // handler consumes the action (Handled), so the step change must be
        // re-dispatched to the reducer or Tab/Enter is dead on Details/Body.
        app.reduce(AppAction::MemoryAddWizard(
            crate::tui::event::MemoryAddWizardAction::Submit,
        ));
        return;
    }
    let tier = state.tier;
    let entry_type = state.entry_type;
    let enabled = state.enabled;
    let description = state.description.trim().to_string();
    let body = state.body.text.trim().to_string();
    let editing = state.editing.clone();

    if description.is_empty() || body.is_empty() {
        if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard { state })) =
            app.modal.as_mut()
        {
            state.error = Some("Description and body are required.".to_string());
        }
        return;
    }

    // deps.memory, not the agent's handle: the agent lock is held for a whole
    // run, and the wizard must stay usable while the agent is busy.
    let Some(memory) = deps.memory.clone() else {
        if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard { state })) =
            app.modal.as_mut()
        {
            state.error = Some("Memory is unavailable in this session.".to_string());
        }
        return;
    };
    let result = match &editing {
        // Create: slug from the description, suffixing on collisions.
        None => memory
            .store()
            .write(tier, entry_type, None, &description, &body, Some(enabled)),
        // Edit in place: same tier and name, `created` preserved.
        Some((orig_tier, orig_name)) if *orig_tier == tier => memory.store().write(
            tier,
            entry_type,
            Some(orig_name),
            &description,
            &body,
            Some(enabled),
        ),
        // Tier move: `write` with an explicit name updates in place, so an
        // unrelated same-name entry in the target tier must be refused rather
        // than clobbered. Write-first ordering: a failure between the two
        // steps leaves a duplicate, never a lost entry.
        Some((orig_tier, orig_name)) => {
            if memory.store().get_exact(tier, orig_name).is_some() {
                if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard {
                    state,
                })) = app.modal.as_mut()
                {
                    state.error = Some(format!(
                        "A {} entry named {orig_name:?} already exists; delete it first to move this one.",
                        tier.label()
                    ));
                }
                return;
            }
            let written = memory.store().write(
                tier,
                entry_type,
                Some(orig_name),
                &description,
                &body,
                Some(enabled),
            );
            if written.is_ok()
                && let Err(err) = memory.forget_exact(*orig_tier, orig_name).await
            {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!(
                        "Moved memory entry {orig_name} but could not remove the old {} copy: {err:#}",
                        orig_tier.label()
                    ),
                );
            }
            written
        }
    };
    match result {
        Ok(written) => {
            let rows = memory.store().entries();
            open_memory_manager(app, rows, Some((written.entry.tier, written.entry.name)));
        }
        Err(err) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard {
                state,
            })) = app.modal.as_mut()
            {
                state.error = Some(format!("Could not save memory: {err:#}"));
            }
        }
    }
}

pub(super) async fn toggle_selected_memory(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some((tier, name)) = selected_memory_identity(app) else {
        return;
    };
    let Some(memory) = deps.memory.clone() else {
        return;
    };
    let enabled = match app.modal.as_ref() {
        Some(ModalKind::Manager(crate::tui::event::ManagerModal::MemoryManager {
            rows,
            cursor,
        })) => rows.get(*cursor).map(|row| !row.enabled).unwrap_or(true),
        _ => true,
    };
    if let Err(err) = memory.set_enabled(tier, &name, enabled).await {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Could not update memory entry {name}: {err:#}"),
        );
        return;
    }
    let rows = memory.store().entries();
    open_memory_manager(app, rows, Some((tier, name)));
}

pub(super) async fn delete_selected_memory(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some((tier, name)) = selected_memory_identity(app) else {
        return;
    };
    let Some(memory) = deps.memory.clone() else {
        return;
    };
    if let Err(err) = memory.forget_exact(tier, &name).await {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Could not forget memory entry {name}: {err:#}"),
        );
        return;
    }
    let rows = memory.store().entries();
    let cursor = selected_memory_cursor(app);
    let selection = rows
        .get(cursor.min(rows.len().saturating_sub(1)))
        .map(|row| (row.tier, row.name.clone()));
    open_memory_manager(app, rows, selection);
}
