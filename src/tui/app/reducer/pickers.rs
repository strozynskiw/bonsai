use crate::tui::event::{AppAction, ModalKind};

use super::super::{AppState, ModelPickerPane, move_index};
use super::ActionResult;

pub(super) fn handle(app: &mut AppState, action: AppAction) -> ActionResult {
    match action {
        AppAction::ApiKeyInputChar(ch) => app.active_auth_input_mut().push(ch),
        AppAction::ApiKeyInputBackspace => {
            app.active_auth_input_mut().pop();
        }
        AppAction::ApiKeyInputPaste(text) => {
            let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
            for line in cleaned.split('\n') {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    app.active_auth_input_mut().push_str(trimmed);
                }
            }
        }
        AppAction::ApiKeyInputMoveField(delta) => {
            if app.uses_endpoint_auth_form() {
                app.provider_auth_form.provider_auth_field =
                    app.provider_auth_form.provider_auth_field.moved(delta);
            }
        }
        AppAction::ApiKeyPersistenceToggle => {
            app.provider_auth_form.credential_persistence =
                app.provider_auth_form.credential_persistence.cycled();
        }
        AppAction::ApiKeyInputSubmit => {
            // Runtime effect: `tui::runtime_actions` validates input,
            // authorizes the provider, and updates the running agent.
        }
        AppAction::AuthorizeProviderPickerMove(delta) => {
            if let Some(ModalKind::AuthorizeProviderPicker {
                providers,
                query,
                cursor,
            }) = &mut app.modal
            {
                let max = crate::tui::pickers::filter_authorize_providers(providers, query)
                    .len()
                    .saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::AuthorizeProviderPickerInputChar(ch) => {
            if let Some(ModalKind::AuthorizeProviderPicker { query, cursor, .. }) = &mut app.modal {
                query.push(ch);
                *cursor = 0;
            }
        }
        AppAction::AuthorizeProviderPickerInputBackspace => {
            if let Some(ModalKind::AuthorizeProviderPicker { query, cursor, .. }) = &mut app.modal {
                query.pop();
                *cursor = 0;
            }
        }
        AppAction::AuthorizeProviderPickerSubmit => {
            // Runtime effect: `tui::runtime_actions` dispatches the selected
            // provider through the normal /authorize path.
        }
        AppAction::UnauthorizeProviderPickerMove(delta) => {
            if let Some(ModalKind::UnauthorizeProviderPicker {
                providers,
                query,
                cursor,
            }) = &mut app.modal
            {
                let max = crate::tui::pickers::filter_authorize_providers(providers, query)
                    .len()
                    .saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::UnauthorizeProviderPickerInputChar(ch) => {
            if let Some(ModalKind::UnauthorizeProviderPicker { query, cursor, .. }) = &mut app.modal
            {
                query.push(ch);
                *cursor = 0;
            }
        }
        AppAction::UnauthorizeProviderPickerInputBackspace => {
            if let Some(ModalKind::UnauthorizeProviderPicker { query, cursor, .. }) = &mut app.modal
            {
                query.pop();
                *cursor = 0;
            }
        }
        AppAction::UnauthorizeProviderPickerSubmit => {
            // Runtime effect: `tui::runtime_actions` opens the confirm modal
            // for the selected provider.
        }
        AppAction::UnauthorizeConfirmSubmit => {
            // Runtime effect: `tui::runtime_actions` runs the normal
            // /unauthorize <provider> command path.
        }
        AppAction::ReviewScopePickerMove(delta) => {
            if let Some(ModalKind::ReviewScopePicker { cursor }) = &mut app.modal {
                let max = crate::agent::ReviewScope::all().len().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::ReviewScopePickerSubmit => {
            // Runtime effect: `tui::runtime_actions` starts the review run.
        }
        AppAction::LocalModelWizardInputChar(ch) => {
            if let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal
                && !state.loading
            {
                state.input_char(ch);
            }
        }
        AppAction::LocalModelWizardBackspace => {
            if let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal
                && !state.loading
            {
                state.backspace();
            }
        }
        AppAction::LocalModelWizardPaste(text) => {
            if let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal
                && !state.loading
            {
                state.paste(&text);
            }
        }
        AppAction::LocalModelWizardMoveField(delta) => {
            if let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal
                && !state.loading
            {
                state.move_field(delta);
            }
        }
        AppAction::LocalModelWizardMoveModel(delta) => {
            if let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal
                && !state.loading
            {
                state.move_model(delta);
            }
        }
        AppAction::LocalModelWizardCycleChoice(delta) => {
            if let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal
                && !state.loading
            {
                state.cycle_setup_choice(delta);
            }
        }
        AppAction::LocalModelWizardToggle => {
            if let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal
                && !state.loading
            {
                match state.step {
                    crate::tui::local_model_wizard::LocalModelWizardStep::Setup => {
                        state.toggle_setup_choice();
                    }
                    crate::tui::local_model_wizard::LocalModelWizardStep::SelectModels => {
                        state.toggle_selected_model();
                    }
                    crate::tui::local_model_wizard::LocalModelWizardStep::Metadata => {
                        state.toggle_tool_calls();
                    }
                    crate::tui::local_model_wizard::LocalModelWizardStep::Review => {}
                }
            }
        }
        AppAction::LocalModelWizardSubmit => {
            if let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal
                && !state.loading
            {
                match state.step {
                    crate::tui::local_model_wizard::LocalModelWizardStep::SelectModels => {
                        state.submit_selection();
                    }
                    crate::tui::local_model_wizard::LocalModelWizardStep::Metadata => {
                        state.submit_metadata();
                    }
                    crate::tui::local_model_wizard::LocalModelWizardStep::Setup
                    | crate::tui::local_model_wizard::LocalModelWizardStep::Review => {}
                }
            }
        }
        AppAction::LocalModelWizardBack => {
            if let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal
                && !state.loading
            {
                state.back();
            }
        }
        AppAction::ModelPickerInputChar(ch) => {
            app.model_picker.filter.push(ch);
            app.model_picker.active_pane = ModelPickerPane::Model;
            app.model_picker.model_offset = 0;
            app.model_picker.reasoning_offset = 0;
            // Move the modal out rather than cloning its `entries` Vec on every
            // keystroke; restore it once the cursor sync is done.
            let modal = app.modal.take();
            if let Some(ModalKind::ModelPicker { entries }) = &modal {
                app.model_picker.cursor = app.model_picker_first_model_row(entries);
                app.sync_model_picker_reasoning_cursor(entries);
            }
            app.modal = modal;
        }
        AppAction::ModelPickerInputBackspace => {
            app.model_picker.filter.pop();
            app.model_picker.active_pane = ModelPickerPane::Model;
            app.model_picker.model_offset = 0;
            app.model_picker.reasoning_offset = 0;
            let modal = app.modal.take();
            if let Some(ModalKind::ModelPicker { entries }) = &modal {
                app.model_picker.cursor = app.model_picker_first_model_row(entries);
                app.sync_model_picker_reasoning_cursor(entries);
            }
            app.modal = modal;
        }
        AppAction::ModelPickerMove(delta) => {
            let modal = app.modal.take();
            if let Some(ModalKind::ModelPicker { entries }) = &modal {
                match app.model_picker.active_pane {
                    ModelPickerPane::Provider => {
                        let max = AppState::model_picker_provider_rows(entries)
                            .len()
                            .saturating_sub(1);
                        app.model_picker.provider_cursor =
                            move_index(app.model_picker.provider_cursor, delta, max);
                        app.model_picker.cursor = app.model_picker_first_model_row(entries);
                        app.model_picker.model_offset = 0;
                        app.model_picker.reasoning_offset = 0;
                        app.sync_model_picker_reasoning_cursor(entries);
                    }
                    ModelPickerPane::Model => {
                        let max = app.model_picker_model_row_count(entries).saturating_sub(1);
                        app.model_picker.cursor = move_index(app.model_picker.cursor, delta, max);
                        app.sync_model_picker_reasoning_cursor(entries);
                    }
                    ModelPickerPane::Reasoning => {
                        if let Some(entry) = app.model_picker_selected_model(entries) {
                            let choices = AppState::model_picker_reasoning_choices(entry);
                            let max = choices.len().saturating_sub(1);
                            app.model_picker.reasoning_cursor =
                                move_index(app.model_picker.reasoning_cursor, delta, max);
                        }
                    }
                }
            }
            app.modal = modal;
        }
        AppAction::ModelPickerMovePane(delta) => {
            app.model_picker.active_pane = app.model_picker.active_pane.moved(delta);
        }
        AppAction::ModelPickerAssignShortcut(_key) => {
            // Runtime effect: persist the selected model/reasoning as a shortcut binding.
        }
        AppAction::ModelPickerSubmit => {
            // Runtime effect: `tui::runtime_actions` resolves the filtered
            // selection and switches provider/model.
        }
        AppAction::SessionPickerMove(delta) => {
            if let Some(ModalKind::SessionPicker { sessions, cursor }) = &mut app.modal {
                let max = sessions.len().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::SessionPickerSubmit | AppAction::SessionPickerDeleteSelected => {
            // Runtime effect: `tui::runtime_actions` owns storage-backed
            // session picker transitions.
        }
        AppAction::PlanPickerInputChar(ch) => {
            if let Some(ModalKind::PlanPicker { query, cursor, .. }) = &mut app.modal {
                query.push(ch);
                *cursor = 0;
            }
        }
        AppAction::PlanPickerInputBackspace => {
            if let Some(ModalKind::PlanPicker { query, cursor, .. }) = &mut app.modal {
                query.pop();
                *cursor = 0;
            }
        }
        AppAction::PlanPickerMove(delta) => {
            if let Some(ModalKind::PlanPicker {
                plans,
                query,
                cursor,
            }) = &mut app.modal
            {
                let max = AppState::plan_picker_filtered_plans(plans, query)
                    .len()
                    .saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::PlanPickerSubmit | AppAction::PlanPickerDeleteSelected => {
            // Runtime effect: `tui::runtime_actions` owns storage-backed
            // plan picker transitions.
        }
        AppAction::PlanOpenChoiceMove(delta) => {
            if let Some(ModalKind::PlanOpenChoice { cursor, .. }) = &mut app.modal {
                *cursor = move_index(*cursor, delta, 1);
            }
        }
        AppAction::StartPlanChoiceMove(delta) => {
            if let Some(ModalKind::StartPlanChoice { cursor }) = &mut app.modal {
                *cursor = move_index(*cursor, delta, 1);
            }
        }
        AppAction::PlanOpenChoiceSubmit
        | AppAction::StartPlanChoiceSubmit
        | AppAction::PlanDeleteConfirmSubmit
        | AppAction::SessionDeleteConfirmSubmit => {
            // Runtime effect: `tui::runtime_actions` owns storage-backed
            // plan open/delete operations.
        }
        AppAction::SetActiveSavedPlan(plan_id) => {
            app.active_saved_plan_session_id = plan_id;
        }
        AppAction::AgentComposerInputChar(ch) => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.input_char(ch);
            }
        }
        AppAction::AgentComposerBackspace => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.backspace();
            }
        }
        AppAction::AgentComposerPaste(text) => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.paste(&text);
            }
        }
        AppAction::AgentComposerMove(delta) => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.move_selection(delta);
            }
        }
        AppAction::AgentComposerToggle(delta) => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.toggle(delta);
            }
        }
        AppAction::AgentComposerDeleteModel => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.clear_focused_model();
            }
        }
        AppAction::AgentComposerBack => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.back();
            }
        }
        AppAction::AgentComposerNextPage => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.next_page();
            }
        }
        AppAction::AgentComposerSubmit => {
            // Runtime handles the terminal `Review` step (writes the file); for the
            // earlier steps it returns `Unhandled`, so advance the state here.
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.submit();
            }
        }
        AppAction::AgentComposerCursor(motion) => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.cursor(motion);
            }
        }
        AppAction::AgentComposerDeleteForward => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.delete_forward();
            }
        }
        AppAction::AgentComposerInsertNewline => {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal
                && !state.generating
            {
                state.insert_newline();
            }
        }
        AppAction::AgentBrowserMove(delta) => {
            if let Some(ModalKind::AgentBrowser { rows, cursor }) = &mut app.modal {
                let max = rows.len().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::ProviderManagerMove(delta) => {
            if let Some(ModalKind::ProviderManager {
                rows,
                filter,
                cursor,
                ..
            }) = &mut app.modal
            {
                let max = crate::tui::provider_manager::provider_manager_filtered(rows, filter)
                    .len()
                    .saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::ProviderManagerBeginSearch => {
            if let Some(ModalKind::ProviderManager { searching, .. }) = &mut app.modal {
                *searching = true;
            }
        }
        AppAction::ProviderManagerSearchChar(ch) => {
            if let Some(ModalKind::ProviderManager {
                filter,
                searching,
                cursor,
                ..
            }) = &mut app.modal
                && *searching
            {
                filter.push(ch);
                *cursor = 0;
            }
        }
        AppAction::ProviderManagerSearchBackspace => {
            if let Some(ModalKind::ProviderManager {
                filter,
                searching,
                cursor,
                ..
            }) = &mut app.modal
                && *searching
            {
                filter.pop();
                *cursor = 0;
            }
        }
        AppAction::ProviderManagerSearchExit => {
            if let Some(ModalKind::ProviderManager { searching, .. }) = &mut app.modal {
                *searching = false;
            }
        }
        AppAction::ProviderManagerClearFilter => {
            if let Some(ModalKind::ProviderManager {
                filter,
                searching,
                cursor,
                ..
            }) = &mut app.modal
            {
                filter.clear();
                *searching = false;
                *cursor = 0;
            }
        }
        AppAction::PermissionsManagerMove(delta) => {
            if let Some(ModalKind::PermissionsManager {
                rows,
                filter,
                cursor,
                ..
            }) = &mut app.modal
            {
                let max =
                    crate::tui::permissions_manager::permission_manager_filtered(rows, filter)
                        .len()
                        .saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::PermissionsManagerBeginSearch => {
            if let Some(ModalKind::PermissionsManager { searching, .. }) = &mut app.modal {
                *searching = true;
            }
        }
        AppAction::PermissionsManagerSearchChar(ch) => {
            if let Some(ModalKind::PermissionsManager {
                filter,
                searching,
                cursor,
                ..
            }) = &mut app.modal
                && *searching
            {
                filter.push(ch);
                *cursor = 0;
            }
        }
        AppAction::PermissionsManagerSearchBackspace => {
            if let Some(ModalKind::PermissionsManager {
                filter,
                searching,
                cursor,
                ..
            }) = &mut app.modal
                && *searching
            {
                filter.pop();
                *cursor = 0;
            }
        }
        AppAction::PermissionsManagerSearchExit => {
            if let Some(ModalKind::PermissionsManager { searching, .. }) = &mut app.modal {
                *searching = false;
            }
        }
        AppAction::PermissionsManagerClearFilter => {
            if let Some(ModalKind::PermissionsManager {
                filter,
                searching,
                cursor,
                ..
            }) = &mut app.modal
            {
                filter.clear();
                *searching = false;
                *cursor = 0;
            }
        }
        // The delete effect (storage/memory removal + list rebuild) is
        // runtime-owned; see `tui::runtime_actions`.
        AppAction::PermissionsManagerDelete => {}
        AppAction::ProviderDetailBack => {
            if let Some(ModalKind::ProviderDetail { detail }) = app.modal.take() {
                app.reduce(AppAction::OpenModal(ModalKind::ProviderManager {
                    rows: detail.return_rows,
                    filter: detail.return_filter,
                    // Return to the narrowed list with shortcuts live, not mid-type.
                    searching: false,
                    cursor: detail.return_cursor,
                }));
            }
        }
        AppAction::SkillManagerMove(delta) => {
            if let Some(ModalKind::SkillManager { rows, cursor }) = &mut app.modal {
                let max = rows.len().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
                // Changing selection resets the detail pane to the top.
                app.modal_scroll = 0;
            }
        }
        action => return ActionResult::unhandled(action),
    }
    ActionResult::Handled
}
