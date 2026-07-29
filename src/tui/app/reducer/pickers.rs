use crate::tui::event::{AppAction, ModalKind};

use super::super::{AppState, ModelPickerPane, move_index};
use super::ActionResult;

pub(super) fn handle(app: &mut AppState, action: AppAction) -> ActionResult {
    match action {
        AppAction::ApiKeyInputChar(ch) => {
            if let Some(input) = app.active_auth_input_mut() {
                input.push(ch);
            }
        }
        AppAction::ApiKeyInputBackspace => {
            if let Some(input) = app.active_auth_input_mut() {
                input.pop();
            }
        }
        AppAction::ApiKeyInputPaste(text) => {
            let cleaned = text.replace("\r\n", "\n").replace('\r', "\n");
            for line in cleaned.split('\n') {
                let trimmed = line.trim();
                if !trimmed.is_empty()
                    && let Some(input) = app.active_auth_input_mut()
                {
                    input.push_str(trimmed);
                }
            }
        }
        AppAction::ApiKeyInputMoveField(delta) => {
            if app.uses_structured_auth_form() {
                let endpoint_form = app.uses_endpoint_auth_form();
                let has_origins = !app.provider_auth_form.origins.is_empty();
                app.provider_auth_form.provider_auth_field = app
                    .provider_auth_form
                    .provider_auth_field
                    .moved(delta, endpoint_form, has_origins);
            }
        }
        AppAction::ApiKeyOriginCycle(delta) => app.provider_auth_form.cycle_origin(delta),
        AppAction::ApiKeyPersistenceToggle => {
            app.provider_auth_form.credential_persistence =
                app.provider_auth_form.credential_persistence.cycled();
        }
        AppAction::ApiKeyInputSubmit => {
            // Runtime effect: `tui::runtime_actions` validates input,
            // authorizes the provider, and updates the running agent.
        }
        AppAction::AuthorizeProviderPickerMove(delta) => {
            if let Some(ModalKind::Picker(
                crate::tui::event::PickerModal::AuthorizeProviderPicker {
                    providers,
                    query,
                    cursor,
                },
            )) = &mut app.modal
            {
                let max = crate::tui::pickers::filter_authorize_providers(providers, query)
                    .len()
                    .saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::AuthorizeProviderPickerInputChar(ch) => {
            if let Some(ModalKind::Picker(
                crate::tui::event::PickerModal::AuthorizeProviderPicker { query, cursor, .. },
            )) = &mut app.modal
            {
                query.push(ch);
                *cursor = 0;
            }
        }
        AppAction::AuthorizeProviderPickerInputBackspace => {
            if let Some(ModalKind::Picker(
                crate::tui::event::PickerModal::AuthorizeProviderPicker { query, cursor, .. },
            )) = &mut app.modal
            {
                query.pop();
                *cursor = 0;
            }
        }
        AppAction::AuthorizeProviderPickerSubmit => {
            // Runtime effect: `tui::runtime_actions` dispatches the selected
            // provider through the normal /authorize path.
        }
        AppAction::UnauthorizeProviderPickerMove(delta) => {
            if let Some(ModalKind::Picker(
                crate::tui::event::PickerModal::UnauthorizeProviderPicker {
                    providers,
                    query,
                    cursor,
                },
            )) = &mut app.modal
            {
                let max = crate::tui::pickers::filter_authorize_providers(providers, query)
                    .len()
                    .saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::UnauthorizeProviderPickerInputChar(ch) => {
            if let Some(ModalKind::Picker(
                crate::tui::event::PickerModal::UnauthorizeProviderPicker { query, cursor, .. },
            )) = &mut app.modal
            {
                query.push(ch);
                *cursor = 0;
            }
        }
        AppAction::UnauthorizeProviderPickerInputBackspace => {
            if let Some(ModalKind::Picker(
                crate::tui::event::PickerModal::UnauthorizeProviderPicker { query, cursor, .. },
            )) = &mut app.modal
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
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::ReviewScopePicker {
                cursor,
            })) = &mut app.modal
            {
                let max = crate::agent::ReviewScope::all().len().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::ReviewScopePickerSubmit => {
            // Runtime effect: `tui::runtime_actions` starts the review run.
        }
        AppAction::LocalModelWizard(crate::tui::event::LocalModelWizardAction::InputChar(ch)) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard {
                state,
            })) = &mut app.modal
                && !state.loading
            {
                state.input_char(ch);
            }
        }
        AppAction::LocalModelWizard(crate::tui::event::LocalModelWizardAction::Backspace) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard {
                state,
            })) = &mut app.modal
                && !state.loading
            {
                state.backspace();
            }
        }
        AppAction::LocalModelWizard(crate::tui::event::LocalModelWizardAction::Paste(text)) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard {
                state,
            })) = &mut app.modal
                && !state.loading
            {
                state.paste(&text);
            }
        }
        AppAction::LocalModelWizard(crate::tui::event::LocalModelWizardAction::MoveField(
            delta,
        )) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard {
                state,
            })) = &mut app.modal
                && !state.loading
            {
                state.move_field(delta);
            }
        }
        AppAction::LocalModelWizard(crate::tui::event::LocalModelWizardAction::MoveModel(
            delta,
        )) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard {
                state,
            })) = &mut app.modal
                && !state.loading
            {
                state.move_model(delta);
            }
        }
        AppAction::LocalModelWizard(crate::tui::event::LocalModelWizardAction::CycleChoice(
            delta,
        )) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard {
                state,
            })) = &mut app.modal
                && !state.loading
            {
                state.cycle_setup_choice(delta);
            }
        }
        AppAction::LocalModelWizard(crate::tui::event::LocalModelWizardAction::Toggle) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard {
                state,
            })) = &mut app.modal
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
        AppAction::LocalModelWizard(crate::tui::event::LocalModelWizardAction::Submit) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard {
                state,
            })) = &mut app.modal
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
        AppAction::LocalModelWizard(crate::tui::event::LocalModelWizardAction::Back) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard {
                state,
            })) = &mut app.modal
                && !state.loading
            {
                state.back();
            }
        }
        AppAction::ModelPicker(crate::tui::event::ModelPickerAction::InputChar(ch)) => {
            app.model_picker.filter.push(ch);
            app.model_picker.active_pane = ModelPickerPane::Model;
            app.model_picker.model_offset = 0;
            app.model_picker.reasoning_offset = 0;
            // Move the modal out rather than cloning its `entries` Vec on every
            // keystroke; restore it once the cursor sync is done.
            let modal = app.modal.take();
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker {
                entries,
            })) = &modal
            {
                app.model_picker.cursor = app.model_picker_first_model_row(entries);
                app.sync_model_picker_reasoning_cursor(entries);
            }
            app.modal = modal;
        }
        AppAction::ModelPicker(crate::tui::event::ModelPickerAction::InputBackspace) => {
            app.model_picker.filter.pop();
            app.model_picker.active_pane = ModelPickerPane::Model;
            app.model_picker.model_offset = 0;
            app.model_picker.reasoning_offset = 0;
            let modal = app.modal.take();
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker {
                entries,
            })) = &modal
            {
                app.model_picker.cursor = app.model_picker_first_model_row(entries);
                app.sync_model_picker_reasoning_cursor(entries);
            }
            app.modal = modal;
        }
        AppAction::ModelPicker(crate::tui::event::ModelPickerAction::Move(delta)) => {
            let modal = app.modal.take();
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker {
                entries,
            })) = &modal
            {
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
        AppAction::ModelPicker(crate::tui::event::ModelPickerAction::MovePane(delta)) => {
            app.model_picker.active_pane = app.model_picker.active_pane.moved(delta);
        }
        AppAction::ModelPicker(crate::tui::event::ModelPickerAction::AssignShortcut(_key)) => {
            // Runtime effect: persist the selected model/reasoning as a shortcut binding.
        }
        AppAction::ModelPicker(crate::tui::event::ModelPickerAction::Submit) => {
            // Runtime effect: `tui::runtime_actions` resolves the filtered
            // selection and switches provider/model.
        }
        AppAction::SessionPickerMove(delta) => {
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::SessionPicker {
                sessions,
                cursor,
            })) = &mut app.modal
            {
                let max = sessions.len().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::SessionPickerSubmit | AppAction::SessionPickerDeleteSelected => {
            // Runtime effect: `tui::runtime_actions` owns storage-backed
            // session picker transitions.
        }
        AppAction::PlanPicker(crate::tui::event::PlanPickerAction::InputChar(ch)) => {
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::PlanPicker {
                query,
                cursor,
                ..
            })) = &mut app.modal
            {
                query.push(ch);
                *cursor = 0;
            }
        }
        AppAction::PlanPicker(crate::tui::event::PlanPickerAction::InputBackspace) => {
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::PlanPicker {
                query,
                cursor,
                ..
            })) = &mut app.modal
            {
                query.pop();
                *cursor = 0;
            }
        }
        AppAction::PlanPicker(crate::tui::event::PlanPickerAction::Move(delta)) => {
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::PlanPicker {
                plans,
                query,
                cursor,
            })) = &mut app.modal
            {
                let max = AppState::plan_picker_filtered_plans(plans, query)
                    .len()
                    .saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::PlanPicker(crate::tui::event::PlanPickerAction::Submit)
        | AppAction::PlanPicker(crate::tui::event::PlanPickerAction::DeleteSelected) => {
            // Runtime effect: `tui::runtime_actions` owns storage-backed
            // plan picker transitions.
        }
        AppAction::PlanOpenChoiceMove(delta) => {
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::PlanOpenChoice {
                cursor,
                ..
            })) = &mut app.modal
            {
                *cursor = move_index(*cursor, delta, 1);
            }
        }
        AppAction::StartPlanChoiceMove(delta) => {
            if let Some(ModalKind::Picker(crate::tui::event::PickerModal::StartPlanChoice {
                cursor,
            })) = &mut app.modal
            {
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
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::InputChar(ch)) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.input_char(ch);
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::Backspace) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.backspace();
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::Paste(text)) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.paste(&text);
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::Move(delta)) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.move_selection(delta);
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::Toggle(delta)) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.toggle(delta);
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::DeleteModel) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.clear_focused_model();
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::Back) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.back();
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::NextPage) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.next_page();
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::Submit) => {
            // Runtime handles the terminal `Review` step (writes the file); for the
            // earlier steps it returns `Unhandled`, so advance the state here.
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.submit();
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::Cursor(motion)) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.cursor(motion);
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::DeleteForward) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.delete_forward();
            }
        }
        AppAction::AgentComposer(crate::tui::event::AgentComposerAction::InsertNewline) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state })) =
                &mut app.modal
                && !state.generating
            {
                state.insert_newline();
            }
        }
        AppAction::AgentBrowserMove(delta) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::AgentBrowser {
                rows,
                cursor,
            })) = &mut app.modal
            {
                let max = rows.len().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::ProviderManager(crate::tui::event::ProviderManagerAction::Move(delta)) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
                rows,
                filter,
                cursor,
                ..
            })) = &mut app.modal
            {
                let max = crate::tui::provider_manager::provider_manager_filtered(rows, filter)
                    .len()
                    .saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::ProviderManager(crate::tui::event::ProviderManagerAction::BeginSearch) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
                searching,
                ..
            })) = &mut app.modal
            {
                *searching = true;
            }
        }
        AppAction::ProviderManager(crate::tui::event::ProviderManagerAction::SearchChar(ch)) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
                filter,
                searching,
                cursor,
                ..
            })) = &mut app.modal
                && *searching
            {
                filter.push(ch);
                *cursor = 0;
            }
        }
        AppAction::ProviderManager(crate::tui::event::ProviderManagerAction::SearchBackspace) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
                filter,
                searching,
                cursor,
                ..
            })) = &mut app.modal
                && *searching
            {
                filter.pop();
                *cursor = 0;
            }
        }
        AppAction::ProviderManager(crate::tui::event::ProviderManagerAction::SearchExit) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
                searching,
                ..
            })) = &mut app.modal
            {
                *searching = false;
            }
        }
        AppAction::ProviderManager(crate::tui::event::ProviderManagerAction::ClearFilter) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
                filter,
                searching,
                cursor,
                ..
            })) = &mut app.modal
            {
                filter.clear();
                *searching = false;
                *cursor = 0;
            }
        }
        AppAction::PermissionsManager(crate::tui::event::PermissionsManagerAction::Move(delta)) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::PermissionsManager {
                rows,
                filter,
                cursor,
                ..
            })) = &mut app.modal
            {
                let max =
                    crate::tui::permissions_manager::permission_manager_filtered(rows, filter)
                        .len()
                        .saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::PermissionsManager(crate::tui::event::PermissionsManagerAction::BeginSearch) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::PermissionsManager {
                searching,
                ..
            })) = &mut app.modal
            {
                *searching = true;
            }
        }
        AppAction::PermissionsManager(crate::tui::event::PermissionsManagerAction::SearchChar(
            ch,
        )) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::PermissionsManager {
                filter,
                searching,
                cursor,
                ..
            })) = &mut app.modal
                && *searching
            {
                filter.push(ch);
                *cursor = 0;
            }
        }
        AppAction::PermissionsManager(
            crate::tui::event::PermissionsManagerAction::SearchBackspace,
        ) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::PermissionsManager {
                filter,
                searching,
                cursor,
                ..
            })) = &mut app.modal
                && *searching
            {
                filter.pop();
                *cursor = 0;
            }
        }
        AppAction::PermissionsManager(crate::tui::event::PermissionsManagerAction::SearchExit) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::PermissionsManager {
                searching,
                ..
            })) = &mut app.modal
            {
                *searching = false;
            }
        }
        AppAction::PermissionsManager(crate::tui::event::PermissionsManagerAction::ClearFilter) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::PermissionsManager {
                filter,
                searching,
                cursor,
                ..
            })) = &mut app.modal
            {
                filter.clear();
                *searching = false;
                *cursor = 0;
            }
        }
        // The delete effect (storage/memory removal + list rebuild) is
        // runtime-owned; see `tui::runtime_actions`.
        AppAction::PermissionsManager(crate::tui::event::PermissionsManagerAction::Delete) => {}
        AppAction::ProviderDetailBack => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderDetail {
                detail,
            })) = app.modal.take()
            {
                app.reduce(AppAction::OpenModal(ModalKind::Manager(
                    crate::tui::event::ManagerModal::ProviderManager {
                        rows: detail.return_rows,
                        filter: detail.return_filter,
                        // Return to the narrowed list with shortcuts live, not mid-type.
                        searching: false,
                        cursor: detail.return_cursor,
                    },
                )));
            }
        }
        AppAction::SkillManagerMove(delta) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::SkillManager {
                rows,
                cursor,
            })) = &mut app.modal
            {
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
