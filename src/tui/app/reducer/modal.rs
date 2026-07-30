use crate::tui::event::{AppAction, Focus, ModalKind, ModeRow, SubtaskListPane, UsageTab};

mod low_volume;
mod mode_picker_seeding;
use mode_picker_seeding::seed_mode_picker_rows;

use super::super::{AppState, ModelPickerPane, move_index};
use super::ActionResult;

/// Preserve a modal list's selection across a refresh: if the previously
/// selected item id still exists keep the cursor on it, otherwise clamp the old
/// cursor to the new bounds. The scroll reset stays at each call site because it
/// differs per list (the peer list intentionally has none).
fn reconcile_cursor<T, Id: PartialEq>(
    items: &[T],
    previous_cursor: usize,
    previous_id: Option<Id>,
    id_of: impl Fn(&T) -> &Id,
) -> usize {
    let max = items.len().saturating_sub(1);
    previous_id
        .and_then(|id| items.iter().position(|item| *id_of(item) == id))
        .unwrap_or_else(|| previous_cursor.min(max))
}

pub(super) fn handle(app: &mut AppState, action: AppAction) -> ActionResult {
    match action {
        AppAction::Modal(action) => return low_volume::handle(app, action),
        AppAction::OpenTaskList => {
            let (tasks, cursor) =
                if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::TaskList {
                    tasks,
                    cursor,
                })) = app.modal.clone()
                {
                    let max = tasks.len().saturating_sub(1);
                    (tasks, cursor.min(max))
                } else {
                    (app.background_tasks.clone(), 0)
                };
            app.modal_scroll = 0;
            app.pending_question_visibility = false;
            app.modal_return_focus = Some(app.focus);
            app.modal = Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::TaskList { tasks, cursor },
            ));
            app.focus = Focus::Modal;
        }
        AppAction::RefreshTaskList { tasks } => {
            app.background_tasks = tasks.clone();
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::TaskList {
                tasks: current_tasks,
                cursor,
            })) = &mut app.modal
            {
                let previous_id = current_tasks.get(*cursor).map(|task| task.id.clone());
                let previous_cursor = *cursor;
                *current_tasks = tasks;
                *cursor =
                    reconcile_cursor(current_tasks, previous_cursor, previous_id, |task| &task.id);
                if current_tasks.is_empty() || previous_cursor != *cursor {
                    app.modal_scroll = 0;
                }
            }
        }
        AppAction::TaskListMove(delta) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::TaskList {
                tasks,
                cursor,
            })) = &mut app.modal
            {
                let max = tasks.len().saturating_sub(1);
                let next = move_index(*cursor, delta, max);
                if next != *cursor {
                    *cursor = next;
                    app.modal_scroll = 0;
                }
            }
        }
        AppAction::TaskListDeleteSelected => {
            // Runtime effect: `tui::runtime_actions` owns the async
            // background-task registry.
        }
        AppAction::SetTaskListStatus(status) => {
            app.task_list_status = status;
        }
        AppAction::OpenPeerList { peers } => {
            let cursor =
                if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::PeerList {
                    cursor,
                    ..
                })) = &app.modal
                {
                    (*cursor).min(peers.len().saturating_sub(1))
                } else {
                    0
                };
            app.modal_scroll = 0;
            app.pending_question_visibility = false;
            app.modal_return_focus = Some(app.focus);
            app.modal = Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::PeerList { peers, cursor },
            ));
            app.focus = Focus::Modal;
        }
        AppAction::RefreshPeerList { peers } => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::PeerList {
                peers: current_peers,
                cursor,
            })) = &mut app.modal
            {
                // Preserve the selected peer across refreshes by session id.
                let previous_id = current_peers.get(*cursor).map(|peer| peer.id);
                let previous_cursor = *cursor;
                *current_peers = peers;
                *cursor =
                    reconcile_cursor(current_peers, previous_cursor, previous_id, |peer| &peer.id);
            }
        }
        AppAction::PeerListMove(delta) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::PeerList {
                peers,
                cursor,
            })) = &mut app.modal
            {
                let max = peers.len().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::DoctorMove(delta) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::Doctor {
                report,
                cursor,
            })) = &mut app.modal
            {
                let max = report.checks.len().saturating_sub(1);
                *cursor = move_index(*cursor, delta, max);
            }
        }
        AppAction::OpenUsageDashboard { dashboard } => {
            let tab =
                if let Some(ModalKind::Detail(crate::tui::event::DetailModal::UsageDashboard {
                    tab,
                    ..
                })) = &app.modal
                {
                    *tab
                } else {
                    UsageTab::Activity
                };
            app.modal_scroll = 0;
            app.pending_question_visibility = false;
            app.modal_return_focus = Some(app.focus);
            app.modal = Some(ModalKind::Detail(
                crate::tui::event::DetailModal::UsageDashboard { dashboard, tab },
            ));
            app.focus = Focus::Modal;
        }
        AppAction::UsageDashboardCycleTab(steps) => {
            if let Some(ModalKind::Detail(crate::tui::event::DetailModal::UsageDashboard {
                tab,
                ..
            })) = &mut app.modal
            {
                *tab = tab.cycled(steps);
                app.modal_scroll = 0;
            }
        }
        AppAction::UsageDashboardSelectTab(selected) => {
            if let Some(ModalKind::Detail(crate::tui::event::DetailModal::UsageDashboard {
                tab,
                ..
            })) = &mut app.modal
                && *tab != selected
            {
                *tab = selected;
                app.modal_scroll = 0;
            }
        }
        AppAction::RefreshMove(delta) => {
            if let Some(ModalKind::Detail(crate::tui::event::DetailModal::Refresh {
                sources,
                cursor,
                ..
            })) = &mut app.modal
            {
                let max = sources.len().saturating_sub(1);
                let next = move_index(*cursor, delta, max);
                if next != *cursor {
                    *cursor = next;
                    app.modal_scroll = 0;
                }
            }
        }
        AppAction::RefreshClose => {
            app.modal = None;
            app.modal_scroll = 0;
            app.focus = Focus::Input;
        }
        AppAction::RefreshSourceUpdate {
            generation,
            index,
            source,
        } => {
            if let Some(ModalKind::Detail(crate::tui::event::DetailModal::Refresh {
                sources,
                generation: modal_gen,
                ..
            })) = &mut app.modal
            {
                if *modal_gen != generation {
                    return ActionResult::Handled;
                }
                if index < sources.len() {
                    sources[index] = source;
                }
            }
        }
        AppAction::RefreshFinished { generation } => {
            // All sources settled — the modal stays open so the user can read
            // the diffs; closure is an explicit Esc/Enter. We keep this arm so
            // the RuntimeEvent → AppAction bridge has a reducer no-op target
            // (the task state is managed by the runtime handler).
            let _ = generation;
        }
        AppAction::OpenSubtaskList => {
            let (subtasks, cursor, pane) =
                if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::SubtaskList {
                    subtasks,
                    cursor,
                    pane,
                })) = app.modal.clone()
                {
                    let max = subtasks.len().saturating_sub(1);
                    (subtasks, cursor.min(max), pane)
                } else {
                    (app.subtasks.clone(), 0, SubtaskListPane::List)
                };
            app.modal_scroll = 0;
            app.pending_question_visibility = false;
            app.modal_return_focus = Some(app.focus);
            app.modal = Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::SubtaskList {
                    subtasks,
                    cursor,
                    pane,
                },
            ));
            app.focus = Focus::Modal;
        }
        AppAction::RefreshSubtaskList { subtasks } => {
            app.subtasks = subtasks.clone();
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::SubtaskList {
                subtasks: current,
                cursor,
                ..
            })) = &mut app.modal
            {
                let previous_id = current.get(*cursor).map(|sub| sub.id.clone());
                let previous_cursor = *cursor;
                *current = subtasks;
                *cursor = reconcile_cursor(current, previous_cursor, previous_id, |sub| &sub.id);
                if current.is_empty() || previous_cursor != *cursor {
                    app.modal_scroll = 0;
                }
            }
        }
        AppAction::SubtaskListMove(delta) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::SubtaskList {
                subtasks,
                cursor,
                pane,
            })) = &mut app.modal
            {
                *pane = SubtaskListPane::List;
                let max = subtasks.len().saturating_sub(1);
                let next = move_index(*cursor, delta, max);
                if next != *cursor {
                    *cursor = next;
                    app.modal_scroll = 0;
                }
            }
        }
        AppAction::SubtaskListSetPane(next) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::SubtaskList {
                pane,
                ..
            })) = &mut app.modal
            {
                *pane = next;
            }
        }
        AppAction::SubtaskListTogglePane => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::SubtaskList {
                pane,
                ..
            })) = &mut app.modal
            {
                *pane = pane.toggled();
            }
        }
        AppAction::SubtaskClearModelOverride => {
            // `d` reverts the selected agent to the default (session) model by
            // dropping its pending override; the next delegation inherits again.
            if let Some(agent) = app.selected_subtask().map(|sub| sub.agent.clone()) {
                app.subagent_model_overrides
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(agent.as_ref());
            }
        }
        AppAction::EpisodesMove(delta) => {
            if let Some(ModalKind::Detail(crate::tui::event::DetailModal::Episodes {
                report,
                cursor,
            })) = &mut app.modal
            {
                let max = report.episodes.len().saturating_sub(1);
                let next = move_index(*cursor, delta, max);
                if next != *cursor {
                    *cursor = next;
                    app.modal_scroll = 0;
                }
            }
        }
        AppAction::OpenModal(mut kind) => {
            app.modal_scroll = 0;
            app.pending_question_visibility = false;
            app.modal_return_focus = None;
            if let ModalKind::Detail(crate::tui::event::DetailModal::Context(report)) = &kind {
                app.init_context_view_state(report);
                app.latest_context_report = Some((**report).clone());
            }
            if let ModalKind::Detail(crate::tui::event::DetailModal::Episodes { report, .. }) =
                &kind
            {
                app.latest_context_report = Some((**report).clone());
            }
            if let ModalKind::Picker(crate::tui::event::PickerModal::ModelPicker { entries }) =
                &kind
            {
                app.cached_model_choices = entries.clone();
                app.model_picker.filter.clear();
                app.model_picker.active_pane = ModelPickerPane::Model;
                if app.pending_composer_state.is_none() {
                    app.model_picker.target = if app.pending_agent_model_override.is_some()
                        || app.pending_self_review_model
                    {
                        crate::tui::app::ModelPickerTarget::Subagent
                    } else {
                        crate::tui::app::ModelPickerTarget::Session
                    };
                }
                let composer_selection = app.pending_composer_state.as_ref().map(|state| {
                    let (selector, effort) = match app.model_picker.target {
                        crate::tui::app::ModelPickerTarget::ComposerPrimary => {
                            (state.selected_model(), state.selected_effort())
                        }
                        crate::tui::app::ModelPickerTarget::ComposerBackup => (
                            state.selected_fallback_model(),
                            state.selected_fallback_effort(),
                        ),
                        crate::tui::app::ModelPickerTarget::Session
                        | crate::tui::app::ModelPickerTarget::Subagent => (None, None),
                    };
                    (selector.map(str::to_string), effort.map(str::to_string))
                });
                let providers = AppState::model_picker_provider_rows(entries);
                let selected_connection = composer_selection
                    .as_ref()
                    .and_then(|(selector, _)| selector.as_deref())
                    .and_then(|selector| selector.split_once(':'))
                    .map(|(connection, _)| connection);
                app.model_picker.provider_cursor = selected_connection
                    .and_then(|connection| {
                        providers
                            .iter()
                            .position(|entry| entry.connection_id == connection)
                    })
                    .or_else(|| {
                        providers
                            .iter()
                            .position(|entry| entry.provider_id == app.provider)
                    })
                    .unwrap_or(0);
                app.model_picker.provider_offset = 0;
                app.model_picker.cursor = app.model_picker_first_model_row(entries);
                app.model_picker.model_offset = 0;
                app.model_picker.reasoning_offset = 0;
                if app.model_picker_selected_provider(entries).is_some() {
                    let models = app.model_picker_filtered_models(entries);
                    let reset_rows = usize::from(app.model_picker.target.reset_label().is_some());
                    let selected_model = composer_selection
                        .as_ref()
                        .and_then(|(selector, _)| selector.as_deref())
                        .and_then(|selector| selector.split_once(':'))
                        .map(|(_, model)| model);
                    let selected_model = selected_model.or_else(|| {
                        (!app.model_picker.target.is_composer()).then_some(app.model.as_str())
                    });
                    app.model_picker.cursor = selected_model
                        .and_then(|selected_model| {
                            models
                                .iter()
                                .position(|model| model.matches_model(selected_model))
                        })
                        .map(|cursor| cursor + reset_rows)
                        .unwrap_or_else(|| {
                            if app.model_picker.target.is_composer()
                                && composer_selection
                                    .as_ref()
                                    .is_some_and(|(selector, _)| selector.is_none())
                            {
                                0
                            } else {
                                app.model_picker_first_model_row(entries)
                            }
                        });
                    app.sync_model_picker_reasoning_cursor(entries);
                    if let Some((_, Some(effort))) = composer_selection
                        && let Some(entry) = app.model_picker_selected_model(entries)
                    {
                        let reasoning =
                            crate::provider::ReasoningSelection::parse(&effort).unwrap_or_default();
                        app.model_picker.reasoning_cursor =
                            AppState::model_picker_reasoning_choices(entry)
                                .iter()
                                .position(|candidate| *candidate == reasoning)
                                .unwrap_or(0);
                    }
                } else {
                    app.model_picker.reasoning_cursor = 0;
                }
            }
            if let ModalKind::Detail(crate::tui::event::DetailModal::ApiKeyPrompt {
                initial_form,
                ..
            }) = &kind
            {
                app.provider_auth_form = initial_form.clone().unwrap_or_else(|| {
                    crate::tui::app::ProviderAuthForm::with_persistence(app.credential_persistence)
                });
            }
            if let ModalKind::Picker(crate::tui::event::PickerModal::ModePicker { .. }) = &kind {
                let rows = seed_mode_picker_rows(app);
                // Open on the first cyclable value so the cursor never starts
                // on a non-selectable header.
                let cursor = first_value_row(&rows);
                kind =
                    ModalKind::Picker(crate::tui::event::PickerModal::ModePicker { rows, cursor });
            }
            clamp_open_cursor(&mut kind);
            app.modal = Some(kind);
            app.focus = Focus::Modal;
        }
        AppAction::CloseModal => {
            app.modal = None;
            app.modal_selection = None;
            app.modal_body_lines.borrow_mut().clear();
            app.modal_body_rect.set(None);
            app.pending_question_visibility = false;
            // Drop any pending subtask model-override target: closing the picker
            // (Esc) must not leak into a later normal `/model` submit.
            app.pending_agent_model_override = None;
            app.pending_self_review_model = false;
            app.task_list_status = None;
            app.provider_auth_form.clear(app.credential_persistence);
            app.model_picker.filter.clear();
            app.model_picker.active_pane = ModelPickerPane::Model;
            app.model_picker.provider_cursor = 0;
            app.model_picker.cursor = 0;
            app.model_picker.reasoning_cursor = 0;
            app.focus = app.modal_return_focus.take().unwrap_or(Focus::Input);
            // If the `/model` picker was open on behalf of the agent composer,
            // reopen the composer unchanged (cancel path). The submit path takes
            // this stash first, so it won't double-fire here.
            if let Some(state) = app.pending_composer_state.take() {
                app.modal = Some(ModalKind::Wizard(
                    crate::tui::event::WizardModal::AgentComposer { state },
                ));
                app.focus = Focus::Modal;
            }
        }
        AppAction::PromptDecision { .. } => {
            // Runtime effect: `tui::runtime_actions::respond_to_prompt_decision`
            // responds through the interaction service using the modal's
            // request id (and emits the audit banner for approved sandbox
            // escalations).
        }
        AppAction::ResetPermissionToolTimer { command, at } => {
            app.reset_permission_tool_timer(&command, at);
        }
        AppAction::QuestionMove(delta) => {
            if let Some(ModalKind::Detail(crate::tui::event::DetailModal::QuestionPrompt {
                cursor,
                options,
                ..
            })) = &mut app.modal
            {
                let max = options.len().saturating_sub(1);
                if delta.is_negative() {
                    *cursor = cursor.saturating_sub(delta.unsigned_abs() as usize);
                } else {
                    *cursor = cursor.saturating_add(delta as usize);
                }
                *cursor = (*cursor).min(max);
                app.pending_question_visibility = true;
            }
        }
        AppAction::QuestionToggle => {
            if let Some(ModalKind::Detail(crate::tui::event::DetailModal::QuestionPrompt {
                cursor,
                selected,
                multiple,
                ..
            })) = &mut app.modal
                && *multiple
                && let Some(value) = selected.get_mut(*cursor)
            {
                *value = !*value;
            }
        }
        AppAction::QuestionSubmit => {
            // Runtime effect: finalized by `tui::runtime_actions` with the
            // current request id.
        }
        AppAction::QuestionCancel => {
            // Runtime effect: finalized by `tui::runtime_actions` with the
            // current request id.
        }
        AppAction::MemoryManagerMove(delta) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::MemoryManager {
                rows,
                cursor,
            })) = &mut app.modal
            {
                let max = rows.len().saturating_sub(1);
                let next = move_index(*cursor, delta, max);
                if next != *cursor {
                    *cursor = next;
                    app.modal_scroll = 0;
                }
            }
        }
        AppAction::MemoryManagerToggleEnabled
        | AppAction::MemoryManagerDelete
        | AppAction::MemoryManagerAdd
        | AppAction::MemoryManagerEdit => {
            // Runtime effect: handled by `tui::runtime_actions`.
        }
        AppAction::MemoryAddWizard(crate::tui::event::MemoryAddWizardAction::MoveField(delta)) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard {
                state,
            })) = &mut app.modal
                && matches!(
                    state.step,
                    crate::tui::memory_manager::MemoryWizardStep::Details
                )
            {
                state.move_field(delta);
            }
        }
        AppAction::MemoryAddWizard(crate::tui::event::MemoryAddWizardAction::InputChar(ch)) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard {
                state,
            })) = &mut app.modal
            {
                state.input_char(ch);
            }
        }
        AppAction::MemoryAddWizard(crate::tui::event::MemoryAddWizardAction::Backspace) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard {
                state,
            })) = &mut app.modal
            {
                state.backspace();
            }
        }
        AppAction::MemoryAddWizard(crate::tui::event::MemoryAddWizardAction::CycleValue(delta)) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard {
                state,
            })) = &mut app.modal
                && matches!(
                    state.step,
                    crate::tui::memory_manager::MemoryWizardStep::Details
                )
            {
                match state.field {
                    crate::tui::memory_manager::MemoryWizardField::Tier => state.cycle_tier(delta),
                    crate::tui::memory_manager::MemoryWizardField::Type => state.cycle_type(delta),
                    crate::tui::memory_manager::MemoryWizardField::Enabled
                    | crate::tui::memory_manager::MemoryWizardField::Description => {}
                }
            }
        }
        AppAction::MemoryAddWizard(crate::tui::event::MemoryAddWizardAction::ToggleValue) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard {
                state,
            })) = &mut app.modal
                && matches!(
                    state.step,
                    crate::tui::memory_manager::MemoryWizardStep::Details
                )
                && matches!(
                    state.field,
                    crate::tui::memory_manager::MemoryWizardField::Enabled
                )
            {
                state.toggle_enabled();
            }
        }
        AppAction::MemoryAddWizard(crate::tui::event::MemoryAddWizardAction::Submit) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard {
                state,
            })) = &mut app.modal
                && !matches!(
                    state.step,
                    crate::tui::memory_manager::MemoryWizardStep::Review
                )
            {
                state.submit();
            }
        }
        AppAction::MemoryAddWizard(crate::tui::event::MemoryAddWizardAction::Back) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard {
                state,
            })) = &mut app.modal
            {
                state.back();
            }
        }
        AppAction::SettingsMove(delta) => {
            if let Some(ModalKind::Manager(crate::tui::event::ManagerModal::Settings {
                rows,
                cursor,
            })) = &mut app.modal
            {
                *cursor = move_settings_cursor(rows, *cursor, delta);
            }
        }
        AppAction::OnboardingMove(delta) => {
            if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::Onboarding {
                step,
                cursor,
            })) = &mut app.modal
            {
                *cursor = move_index(*cursor, delta, step.choice_count().saturating_sub(1));
            }
        }
        AppAction::OnboardingSubmit => {
            // Runtime effect: persist the selected default before closing.
        }
        action => return ActionResult::unhandled(action),
    }
    ActionResult::Handled
}

/// Clamp a list-backed modal's opening cursor to its row count, in place.
/// One shared table instead of a bespoke clone-and-clamp block per variant:
/// a caller may pass a stale cursor (reopening a picker whose rows shrank),
/// and an out-of-range cursor must never survive into the rendered modal.
/// Variants without a cursor — or whose cursor is seeded elsewhere
/// (`ModelPicker`, `ModePicker`) — fall through untouched.
fn clamp_open_cursor(kind: &mut ModalKind) {
    let (cursor, max) = match kind {
        ModalKind::Detail(crate::tui::event::DetailModal::Episodes { report, cursor }) => {
            (cursor, report.episodes.len().saturating_sub(1))
        }
        ModalKind::Picker(crate::tui::event::PickerModal::SessionPicker { sessions, cursor }) => {
            (cursor, sessions.len().saturating_sub(1))
        }
        ModalKind::Picker(crate::tui::event::PickerModal::PlanPicker {
            plans,
            query,
            cursor,
        }) => {
            let max = AppState::plan_picker_filtered_plans(plans, query)
                .len()
                .saturating_sub(1);
            (cursor, max)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::PlanOpenChoice { cursor, .. })
        | ModalKind::Picker(crate::tui::event::PickerModal::StartPlanChoice { cursor }) => {
            (cursor, 1)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::AuthorizeProviderPicker {
            providers,
            query,
            cursor,
        })
        | ModalKind::Picker(crate::tui::event::PickerModal::UnauthorizeProviderPicker {
            providers,
            query,
            cursor,
        }) => {
            let max = crate::tui::pickers::filter_authorize_providers(providers, query)
                .len()
                .saturating_sub(1);
            (cursor, max)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
            rows,
            filter,
            cursor,
            ..
        }) => {
            let max = crate::tui::provider_manager::provider_manager_filtered(rows, filter)
                .len()
                .saturating_sub(1);
            (cursor, max)
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::McpServers { rows, cursor }) => {
            (cursor, rows.len().saturating_sub(1))
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::MemoryManager { rows, cursor }) => {
            (cursor, rows.len().saturating_sub(1))
        }
        ModalKind::Manager(crate::tui::event::ManagerModal::PermissionsManager {
            rows,
            filter,
            cursor,
            ..
        }) => {
            let max = crate::tui::permissions_manager::permission_manager_filtered(rows, filter)
                .len()
                .saturating_sub(1);
            (cursor, max)
        }
        ModalKind::Picker(crate::tui::event::PickerModal::ReviewScopePicker { cursor }) => (
            cursor,
            crate::agent::ReviewScope::all().len().saturating_sub(1),
        ),
        ModalKind::Picker(crate::tui::event::PickerModal::ThemePicker { cursor, .. }) => {
            (cursor, crate::tui::theme::theme_count().saturating_sub(1))
        }
        ModalKind::Detail(crate::tui::event::DetailModal::BusyCommand { rows, cursor, .. }) => {
            (cursor, rows.len().saturating_sub(1))
        }
        _ => return,
    };
    *cursor = (*cursor).min(max);
}

/// Index of the first selectable value row, used as the initial cursor when
/// the `/mode` picker opens so it never starts on a non-cyclable header.
fn first_value_row(rows: &[ModeRow]) -> usize {
    rows.iter().position(ModeRow::is_value).unwrap_or(0)
}

/// Move the `/mode` cursor by `delta` over value rows only, skipping headers
/// so the selection never lands on a non-cyclable axis label. Clamps to the
/// first/last value row.
pub(super) fn move_mode_cursor(rows: &[ModeRow], current: usize, delta: i16) -> usize {
    let values: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.is_value())
        .map(|(index, _)| index)
        .collect();
    let Some(last) = values.len().checked_sub(1) else {
        return current;
    };
    // Position of `current` among the value rows: the value at or after the
    // cursor, falling back to the last one if the cursor sits past them all.
    let pos = values
        .iter()
        .position(|&index| index >= current)
        .unwrap_or(last);
    values[move_index(pos, delta, last)]
}

/// Move the `/settings` cursor among selectable rows, skipping section headers
/// (same shape as [`move_mode_cursor`]).
fn move_settings_cursor(
    rows: &[crate::tui::event::SettingsRow],
    current: usize,
    delta: i16,
) -> usize {
    let selectable: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.is_selectable())
        .map(|(index, _)| index)
        .collect();
    let Some(last) = selectable.len().checked_sub(1) else {
        return current;
    };
    let pos = selectable
        .iter()
        .position(|&index| index >= current)
        .unwrap_or(last);
    selectable[move_index(pos, delta, last)]
}
