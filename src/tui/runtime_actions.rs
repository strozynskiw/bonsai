use std::path::Path;
use std::sync::Arc;

use futures::FutureExt as _;
use tokio::sync::{Mutex, mpsc};

use crate::agent::{Agent, ContextControlAction};
use crate::background::BackgroundTaskRegistry;
use crate::commands::{
    McpCommandRequest, apply_mcp_command, authorize_provider_with_input_as_current,
};
use crate::interaction::{
    EscalationDecision, InteractionAnswer, InteractionOutcome, InteractionService,
    PermissionDecision,
};
use crate::memory::entry::MemoryTier;
use crate::model_catalog::{ConnectionId, ModelCatalog, ModelId};
use crate::model_resolution::{
    build_provider, build_provider_for_optional_model,
    context_window_for_current_model_with_catalog, prompt_estimator_for_current_model_with_catalog,
};
use crate::model_role::{ModelShortcutBinding, ModelShortcutKey};
use crate::output::SharedSink;
use crate::plan::SharedPlanStore;
use crate::provider::{AuthInput, AuthRequirement, ProviderRegistry};
use crate::session::SessionStore;
use crate::storage::Storage;
use crate::todo::SharedTodoStore;
use crate::tool::SharedActiveSessionId;
use crate::tui::agent_composer::{
    AgentBrowserRow, AgentComposerField, AgentComposerState, AgentComposerStep, AgentOrigin,
    agent_file_path, browser_rows_with_settings,
};
use crate::tui::app::AppState;
use crate::tui::event::{
    AppAction, BusyCommandAction, CommandOutcomeEvent, CommandOutputEvent, CommandOutputKind,
    Focus, ModalKind, PromptDecision, PromptFamily, ProviderRunSelection, RuntimeEvent, TaskState,
};
use crate::tui::local_model_wizard::{LocalModelWizardState, LocalModelWizardStep};
use crate::tui::mcp::McpServerStateKind;
use crate::tui::pickers::{ModelOption, ModelSelection, ProviderSelection};
use crate::tui::run::{
    PersistenceCommandDeps, PersistenceCommandState, apply_context_control_action,
    apply_non_idle_read_only_command, clear_canvas_plan, delete_saved_plan_from_picker,
    implement_plan, implement_plan_with_context, mark_started_saved_plan, non_empty_trimmed,
    open_background_task_list, open_context_modal_with_preview, open_saved_plan,
    persist_current_theme, push_command_message, resume_session, review_changes,
    start_delete_selected_background_task, switch_model_selection,
};
use crate::tui::task::{CommandTaskDeps, TaskController};

mod memory;
use memory::{
    delete_selected_memory, open_memory_add_wizard, open_memory_edit_wizard,
    submit_memory_add_wizard, toggle_selected_memory,
};

pub(super) struct RuntimeActionDeps<'a> {
    pub(super) interaction: Arc<InteractionService>,
    pub(super) runtime_sender: mpsc::UnboundedSender<RuntimeEvent>,
    pub(super) agent: Arc<Mutex<Agent>>,
    /// Direct memory-service handle: memory-manager actions must not lock the
    /// agent (held for a run's whole duration), or Space/delete/save in a
    /// mid-run modal would freeze the event loop until the run finishes.
    pub(super) memory: Option<Arc<crate::memory::MemoryService>>,
    /// Shared approval-level holder, cycled by the `CycleApprovalLevel` keybind
    /// (Alt+M) without locking the agent mid-turn.
    pub(super) yolo_mode: crate::yolo::YoloMode,
    pub(super) session_store: Arc<Mutex<SessionStore>>,
    /// The bash command and web-domain permission managers, cloned in so the
    /// `/permissions` manager modal can delete rules without locking the agent.
    pub(super) permissions: crate::permissions::PermissionManager,
    pub(super) domain_permissions: crate::permissions::PermissionManager,
    pub(super) registry: Arc<ProviderRegistry>,
    pub(super) model_catalog: Arc<ModelCatalog>,
    pub(super) storage: &'a Storage,
    pub(super) project_root: &'a Path,
    pub(super) session_project_root: &'a Path,
    pub(super) active_session_id: SharedActiveSessionId,
    pub(super) todo_store: SharedTodoStore,
    pub(super) plan_store: SharedPlanStore,
    pub(super) sink: SharedSink,
    pub(super) background_tasks: Arc<BackgroundTaskRegistry>,
    pub(super) terminals: Arc<crate::terminal::TerminalRegistry>,
}

impl RuntimeActionDeps<'_> {
    fn command_task_deps(
        &self,
        credential_persistence: crate::session::CredentialPersistence,
        active_mode: Option<crate::agent::AgentMode>,
    ) -> CommandTaskDeps {
        CommandTaskDeps::new(
            self.agent.clone(),
            self.session_store.clone(),
            self.project_root.to_path_buf(),
            self.registry.clone(),
            self.model_catalog.clone(),
            Some(self.storage.clone()),
            credential_persistence,
            active_mode,
        )
    }

    fn persistence_deps(&self) -> PersistenceCommandDeps<'_> {
        PersistenceCommandDeps {
            storage: self.storage,
            agent: self.agent.clone(),
            memory: self.memory.clone(),
            session_store: self.session_store.clone(),
            registry: self.registry.clone(),
            model_catalog: self.model_catalog.clone(),
            todo_store: self.todo_store.clone(),
            plan_store: self.plan_store.clone(),
            project_root: self.session_project_root,
            active_session_id: self.active_session_id.clone(),
        }
    }
}

/// Spawn a fire-and-forget future, logging any panic instead of silently
/// swallowing it. Use for short-lived dispatch spawns whose handles are not
/// tracked.
fn spawn_panicked(future: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(async move {
        if let Err(panic) = std::panic::AssertUnwindSafe(future).catch_unwind().await {
            tracing::error!(?panic, "runtime action panicked");
        }
    });
}

pub(super) enum RuntimeActionResult {
    Handled,
    Unhandled(Box<AppAction>),
}

#[expect(
    clippy::too_many_lines,
    reason = "one cohesive dispatch match over every AppAction variant; splitting the arms would scatter a single routing table"
)]
pub(super) async fn handle_runtime_action(
    action: AppAction,
    app: &mut AppState,
    tasks: &mut TaskController,
    deps: RuntimeActionDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
) -> RuntimeActionResult {
    match action {
        AppAction::CycleApprovalLevel => {
            let next = app.approval_level.cycled();
            deps.yolo_mode.set_level(next);
            app.reduce(AppAction::SetApprovalLevel(next));
            persist_approval_level(app, deps.storage, next).await;
        }
        AppAction::ModePickerCycle(delta) => {
            // The runtime handler runs before the reducer, so the reducer's
            // `cycled()` mutation would be wasted — the handler would read the
            // *old* `current` and re-apply the same label. Advance `current`
            // in place here, then dispatch the resolved label through the
            // shared holder. Mirrors the `CycleApprovalLevel` pattern.
            let Some(ModalKind::ModePicker { rows, cursor }) = &mut app.modal else {
                return RuntimeActionResult::Handled;
            };
            let Some(row) = rows.get_mut(*cursor) else {
                return RuntimeActionResult::Handled;
            };
            *row = row.clone().cycled(delta);
            let (axis, label) = match &rows[*cursor] {
                crate::tui::event::ModeRow::Value {
                    axis,
                    values,
                    current,
                    ..
                } => (*axis, values.get(*current).copied().unwrap_or("")),
                crate::tui::event::ModeRow::Header(_) => return RuntimeActionResult::Handled,
            };
            apply_mode_picker_axis(app, axis, label, &deps.yolo_mode, deps.storage).await;
            if axis == crate::tui::event::ModeAxisId::SelfReview
                && let Some(mode) = crate::self_review::SelfReviewMode::parse(label)
                && !tasks.is_busy()
            {
                deps.agent.lock().await.set_self_review_mode(mode);
            }
        }
        AppAction::SettingsCycle(delta) => {
            cycle_setting(app, &deps, tasks, delta).await;
        }
        AppAction::SettingsActivate => {
            activate_setting(app, &deps, tasks).await;
        }
        AppAction::OnboardingSubmit => {
            submit_first_run_step(app, deps).await;
        }
        AppAction::SandboxToggle => {
            let Some(ModalKind::SandboxStatus { cursor }) = &app.modal else {
                return RuntimeActionResult::Handled;
            };
            let cursor = *cursor;
            let Some(sandbox) = &app.sandbox else {
                return RuntimeActionResult::Handled;
            };
            let sandbox = sandbox.clone();
            match cursor {
                0 => {
                    let enable = !sandbox.is_enabled();
                    if enable && !sandbox.backend().is_available() {
                        push_command_message(
                            app,
                            CommandOutputKind::Error,
                            &crate::commands::sandbox_unavailable_message(),
                        );
                    } else {
                        sandbox.set_enabled(enable);
                        persist_sandbox_enabled(app, deps.storage, enable).await;
                    }
                }
                1 => {
                    sandbox.set_deny_network(!sandbox.deny_network());
                    persist_sandbox_deny_network(app, deps.storage, sandbox.deny_network()).await;
                }
                _ => {}
            }
        }
        AppAction::ThemePickerMove(delta) => {
            app.reduce(AppAction::ThemePickerMove(delta));
            if let Some(ModalKind::ThemePicker { cursor, .. }) = &app.modal
                && let Some(palette) = crate::tui::theme::theme_at(*cursor)
            {
                crate::tui::theme::set_theme(palette.name);
            }
        }
        AppAction::ThemePickerSubmit => {
            submit_theme_picker(app, deps.session_store).await;
        }
        AppAction::ThemePickerCancel => {
            cancel_theme_picker(app);
        }
        AppAction::PromptDecision { family, decision } => {
            respond_to_prompt_decision(app, deps.interaction, family, decision);
        }
        AppAction::QuestionSubmit => {
            respond_to_question_submit(app, deps.interaction);
        }
        AppAction::QuestionCancel => {
            respond_to_question_cancel(app, deps.interaction);
        }
        AppAction::McpServersToggle => {
            toggle_selected_mcp_server(app, deps).await;
        }
        AppAction::McpServersReload => {
            reload_selected_mcp_server(app, deps).await;
        }
        AppAction::McpServersAuthorize => {
            authorize_selected_mcp_server(app, deps).await;
        }
        AppAction::ApiKeyInputSubmit => {
            submit_api_key_prompt(app, deps).await;
        }
        AppAction::AuthorizeProviderPickerSubmit => {
            submit_authorize_provider_picker(app, tasks, deps);
        }
        AppAction::UnauthorizeProviderPickerSubmit => {
            submit_unauthorize_provider_picker(app);
        }
        AppAction::UnauthorizeConfirmSubmit => {
            submit_unauthorize_confirm(app, tasks, deps);
        }
        AppAction::ReviewScopePickerSubmit => {
            submit_review_scope_picker(app, tasks, deps).await;
        }
        AppAction::LocalModelWizardSubmit => {
            if !submit_local_model_wizard(app, deps) {
                return RuntimeActionResult::Unhandled(Box::new(AppAction::LocalModelWizardSubmit));
            }
        }
        AppAction::AgentComposerSubmit => {
            if !submit_agent_composer(app, deps).await {
                return RuntimeActionResult::Unhandled(Box::new(AppAction::AgentComposerSubmit));
            }
        }
        AppAction::PasteFromClipboard => {
            paste_from_clipboard(app, &deps.registry, Some(deps.model_catalog.as_ref()));
        }
        AppAction::AgentComposerGenerate => {
            start_agent_composer_generate(app, deps);
        }
        AppAction::AgentComposerOpenModelPicker => {
            open_model_picker_for_composer(app, deps).await;
        }
        AppAction::AgentBrowserAdd => {
            open_agent_composer_new(app);
        }
        AppAction::AgentBrowserEdit => {
            open_agent_composer_edit(app).await;
        }
        AppAction::AgentBrowserDelete => {
            open_agent_delete_confirm(app);
        }
        AppAction::AgentBrowserToggleEnabled => {
            toggle_agent_enabled(app, deps).await;
        }
        AppAction::ProviderManagerAdd => {
            app.reduce(AppAction::OpenModal(ModalKind::LocalModelWizard {
                state: Box::default(),
            }));
        }
        AppAction::ProviderManagerEdit => {
            open_provider_manager_edit(app, &deps);
        }
        AppAction::ProviderManagerShowDetail => {
            open_provider_detail(app, &deps);
        }
        AppAction::ProviderManagerRemove => {
            provider_manager_remove_selected(app, deps);
        }
        AppAction::ProviderRemoveConfirmSubmit => {
            submit_provider_remove_confirm(app, deps);
        }
        AppAction::ProviderRemoveConfirmCancel => {
            open_provider_manager(app, &deps).await;
        }
        AppAction::SkillManagerToggleDisabled => {
            toggle_skill_disabled(app, deps).await;
        }
        AppAction::SkillManagerToggleLoaded => {
            toggle_skill_loaded(app, deps).await;
        }
        // Memory manager/wizard cursor+field+text actions are reducer-owned and
        // deliberately NOT matched here: the runtime handler runs before the
        // reducer and only Unhandled actions fall through to it, so a Handled
        // no-op arm for them would silently eat every wizard keystroke in the
        // live app.
        AppAction::MemoryManagerToggleEnabled => {
            toggle_selected_memory(app, deps).await;
        }
        AppAction::MemoryManagerDelete => {
            delete_selected_memory(app, deps).await;
        }
        AppAction::MemoryManagerAdd => {
            open_memory_add_wizard(app).await;
        }
        AppAction::MemoryManagerEdit => {
            open_memory_edit_wizard(app);
        }
        AppAction::PermissionsManagerDelete => {
            delete_selected_permission_rule(app, &deps).await;
        }
        AppAction::MemoryAddWizardSubmit => {
            submit_memory_add_wizard(app, deps).await;
        }
        AppAction::AgentDeleteConfirmSubmit => {
            submit_agent_delete_confirm(app, deps).await;
        }
        AppAction::AgentDeleteConfirmCancel => {
            reopen_agent_browser(app, &deps, 0).await;
        }
        AppAction::ModelPickerSubmit => {
            submit_model_picker(app, deps).await;
        }
        AppAction::ModelPickerAssignShortcut(key) => {
            assign_model_picker_shortcut(app, deps, key).await;
        }
        AppAction::BusyCommandSubmit => {
            submit_busy_command(app, tasks, deps, state).await;
        }
        AppAction::SessionPickerSubmit => {
            submit_session_picker(app, deps, state).await;
        }
        AppAction::SessionPickerDeleteSelected => {
            open_selected_session_delete_confirm(app);
        }
        AppAction::PlanPickerSubmit => {
            open_selected_plan_choice(app);
        }
        AppAction::PlanPickerDeleteSelected => {
            open_selected_plan_delete_confirm(app);
        }
        AppAction::PlanOpenChoiceSubmit => {
            submit_plan_open_choice(app, deps, state).await;
        }
        AppAction::StartPlanChoiceSubmit => {
            let mode = match app.selected_start_plan_mode() {
                Some(crate::tui::event::StartPlanMode::KeepContext) => {
                    crate::agent::PlanContextMode::Keep
                }
                Some(crate::tui::event::StartPlanMode::CleanContext) | None => {
                    crate::agent::PlanContextMode::Clean
                }
            };
            app.reduce(AppAction::CloseModal);
            if implement_plan_with_context(
                app,
                tasks,
                deps.agent.clone(),
                deps.sink,
                deps.todo_store,
                deps.plan_store,
                mode,
            )
            .await
            {
                mark_started_saved_plan(app, deps.storage, *state.current_session_id).await;
            }
        }
        AppAction::PlanDeleteConfirmSubmit => {
            submit_plan_delete_confirm(app, deps).await;
        }
        AppAction::SessionDeleteConfirmSubmit => {
            submit_session_delete_confirm(app, deps).await;
        }
        AppAction::PlanDiscardConfirmSubmit => {
            submit_plan_discard_confirm(app, deps).await;
        }
        AppAction::ImplementPlan => {
            if implement_plan(
                app,
                tasks,
                deps.agent.clone(),
                deps.sink,
                deps.todo_store,
                deps.plan_store,
            )
            .await
            {
                mark_started_saved_plan(app, deps.storage, *state.current_session_id).await;
            }
        }
        AppAction::OpenTaskList => {
            open_background_task_list(app, &deps.background_tasks, &deps.terminals).await;
        }
        AppAction::OpenContextModal => {
            open_context_modal_with_preview(app, deps.agent, tasks.is_busy(), true).await;
        }
        AppAction::ContextPinSelected => {
            apply_context_control_action(app, deps.agent, ContextControlAction::TogglePin).await;
        }
        AppAction::ContextDropSelected => {
            apply_context_control_action(app, deps.agent, ContextControlAction::ToggleDropNextTurn)
                .await;
        }
        AppAction::ContextStubSelected => {
            apply_context_control_action(app, deps.agent, ContextControlAction::ToggleStub).await;
        }
        AppAction::ContextRestoreSelected => {
            apply_context_control_action(
                app,
                deps.agent,
                ContextControlAction::RestoreSummarySource,
            )
            .await;
        }
        AppAction::TaskListDeleteSelected => {
            start_delete_selected_background_task(
                app,
                deps.background_tasks,
                deps.terminals,
                deps.runtime_sender,
            );
        }
        AppAction::SubtaskOverrideModel => {
            override_selected_subtask_model(app, deps).await;
        }
        AppAction::StartRefresh => {
            start_refresh(app, deps).await;
        }
        action => return RuntimeActionResult::Unhandled(Box::new(action)),
    }
    RuntimeActionResult::Handled
}

async fn submit_first_run_step(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(ModalKind::Onboarding { step, cursor }) = app.modal.as_ref() else {
        return;
    };
    let step = *step;
    let cursor = *cursor;
    match step {
        crate::onboarding::FirstRunStep::CredentialStorage => {
            let persistence = crate::session::CredentialPersistence::ALL
                .get(cursor)
                .copied()
                .unwrap_or_default();
            match deps.storage.begin_first_run(persistence).await {
                Ok(()) => {
                    app.credential_persistence = persistence;
                    app.provider_auth_form.credential_persistence = persistence;
                    open_first_run_step(app, crate::onboarding::FirstRunStep::Provider);
                }
                Err(error) => push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save first-run credential setting: {error:#}"),
                ),
            }
        }
        crate::onboarding::FirstRunStep::Provider => {
            // Informational step: the wizard teaches /authorize instead of
            // launching it, so Enter just advances. If the user leaves the
            // wizard here and authorizes later, the event-loop transition
            // hook resumes it at the Model step.
            open_first_run_step(app, crate::onboarding::FirstRunStep::Model);
        }
        crate::onboarding::FirstRunStep::Model => {
            // Informational step: the provider default is kept and /model is
            // taught rather than opened. Confirming the checkpoint here keeps
            // a resumed wizard from replaying the step.
            if let Err(error) = deps
                .storage
                .mark_first_run_checkpoint(crate::onboarding::FirstRunCheckpoint::ModelConfirmed)
                .await
            {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save first-run model check: {error:#}"),
                );
                return;
            }
            open_first_run_step(app, crate::onboarding::FirstRunStep::WorkspaceTrust);
        }
        crate::onboarding::FirstRunStep::WorkspaceTrust => {
            let project_id = match deps.storage.ensure_project(deps.session_project_root).await {
                Ok(project_id) => project_id,
                Err(error) => {
                    push_command_message(
                        app,
                        CommandOutputKind::Error,
                        &format!("Failed to resolve workspace trust: {error:#}"),
                    );
                    return;
                }
            };
            let gate = match crate::workspace_trust::WorkspaceTrustGate::load(
                deps.storage.clone(),
                project_id,
            )
            .await
            {
                Ok(gate) => gate,
                Err(error) => {
                    push_command_message(
                        app,
                        CommandOutputKind::Error,
                        &format!("Failed to load workspace trust: {error:#}"),
                    );
                    return;
                }
            };
            let state = if cursor == 0 {
                crate::workspace_trust::WorkspaceTrust::Trusted
            } else {
                crate::workspace_trust::WorkspaceTrust::Restricted
            };
            if let Err(error) = gate.set_state(state).await {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save workspace trust: {error:#}"),
                );
                return;
            }
            if state == crate::workspace_trust::WorkspaceTrust::Trusted {
                push_command_message(
                    app,
                    CommandOutputKind::Status,
                    "Workspace trusted. Project-owned features activate on the next launch.",
                );
            }
            open_first_run_step(app, crate::onboarding::FirstRunStep::Sandbox);
        }
        crate::onboarding::FirstRunStep::Sandbox => {
            let Some(sandbox) = app.sandbox.as_ref().cloned() else {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    "Sandbox detection is not ready; try again.",
                );
                return;
            };
            // Cursor order matches `sandbox_lines`: confined + network blocked
            // (recommended), confined + network allowed, unconfined.
            let (want_enabled, deny_network) = match cursor {
                0 => (true, true),
                1 => (true, false),
                _ => (false, true),
            };
            let sandbox_available = sandbox.backend().is_available();
            let enabled = want_enabled && sandbox_available;
            if want_enabled && !sandbox_available {
                push_command_message(
                    app,
                    CommandOutputKind::Status,
                    &crate::commands::sandbox_unavailable_message(),
                );
            }
            if let Err(error) = deps.storage.set_sandbox_enabled(enabled).await {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save sandbox setting: {error:#}"),
                );
                return;
            }
            if let Err(error) = deps.storage.set_sandbox_deny_network(deny_network).await {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save sandbox network setting: {error:#}"),
                );
                return;
            }
            if let Err(error) = deps
                .storage
                .mark_first_run_checkpoint(crate::onboarding::FirstRunCheckpoint::SandboxReviewed)
                .await
            {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save first-run sandbox check: {error:#}"),
                );
                return;
            }
            sandbox.set_enabled(enabled);
            sandbox.set_deny_network(deny_network);
            open_first_run_step(app, crate::onboarding::FirstRunStep::Autonomy);
        }
        crate::onboarding::FirstRunStep::Autonomy => {
            let level = crate::tool::ApprovalLevel::ALL
                .get(cursor)
                .copied()
                .unwrap_or_default();
            deps.yolo_mode.set_level(level);
            app.reduce(AppAction::SetApprovalLevel(level));
            if let Err(error) = deps.storage.set_approval_level(level).await {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save autonomy setting: {error:#}"),
                );
                return;
            }
            if let Err(error) = deps
                .storage
                .mark_first_run_checkpoint(crate::onboarding::FirstRunCheckpoint::AutonomyReviewed)
                .await
            {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save first-run autonomy check: {error:#}"),
                );
                return;
            }
            open_first_run_step(app, crate::onboarding::FirstRunStep::FirstPrompt);
        }
        crate::onboarding::FirstRunStep::FirstPrompt => {
            app.first_run_step = Some(crate::onboarding::FirstRunStep::FirstPrompt);
            app.reduce(AppAction::CloseModal);
            app.reduce(AppAction::SetFocus(Focus::Input));
            push_command_message(
                app,
                CommandOutputKind::Status,
                "Setup is ready. Send a prompt; onboarding completes after a successful run.",
            );
        }
    }
}

pub(crate) fn open_first_run_step(app: &mut AppState, step: crate::onboarding::FirstRunStep) {
    app.first_run_step = Some(step);
    app.reduce(AppAction::OpenModal(ModalKind::Onboarding {
        step,
        cursor: step.default_choice(),
    }));
}

/// Open (or re-open) the `/permissions` manager built from the current bash and
/// domain rule sets, preserving the active filter and clamping the cursor.
pub(super) fn open_permissions_manager(
    app: &mut AppState,
    permissions: &crate::permissions::PermissionManager,
    domain_permissions: &crate::permissions::PermissionManager,
    filter: String,
    cursor: usize,
) {
    let bash_rules = permissions.user_rules();
    let domain_rules = domain_permissions.user_rules();
    let rows = crate::tui::permissions_manager::permission_manager_rows(&bash_rules, &domain_rules);
    let visible =
        crate::tui::permissions_manager::permission_manager_filtered(&rows, &filter).len();
    let cursor = cursor.min(visible.saturating_sub(1));
    app.reduce(AppAction::OpenModal(ModalKind::PermissionsManager {
        rows,
        filter,
        searching: false,
        cursor,
    }));
}

/// Remove the selected rule from the `/permissions` manager: a persisted rule is
/// deleted from storage by id (kind-agnostic, so the owning manager reloads and
/// the sibling is refreshed), a session rule is dropped from memory by pattern.
/// The list is then rebuilt so the modal reflects the removal in place.
async fn delete_selected_permission_rule(app: &mut AppState, deps: &RuntimeActionDeps<'_>) {
    use crate::tui::permissions_manager::{RuleLane, permission_manager_filtered};

    let Some(ModalKind::PermissionsManager {
        rows,
        filter,
        cursor,
        ..
    }) = app.modal.as_ref()
    else {
        return;
    };
    let filter = filter.clone();
    let cursor = *cursor;
    let Some(row) = permission_manager_filtered(rows, &filter)
        .get(cursor)
        .map(|row| (*row).clone())
    else {
        return;
    };

    let (owner, sibling) = match row.lane {
        RuleLane::Bash => (&deps.permissions, &deps.domain_permissions),
        RuleLane::Domain => (&deps.domain_permissions, &deps.permissions),
    };

    match row.id {
        Some(id) => match owner.remove(id).await {
            Ok(true) => {
                // The delete is kind-agnostic at the storage layer, so the
                // sibling manager's cache may hold the now-deleted row; refresh
                // it (matches the `/permissions remove` text-command handler).
                if let Err(err) = sibling.refresh().await {
                    tracing::warn!(%err, "failed to refresh sibling permission cache after delete");
                }
            }
            Ok(false) => {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("No persisted permission rule #{id}."),
                );
                return;
            }
            Err(err) => {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Could not remove permission rule #{id}: {err:#}"),
                );
                return;
            }
        },
        None => {
            // Session rules have no id; drop the in-memory entry by pattern.
            owner.remove_session_rule(&row.pattern);
        }
    }

    open_permissions_manager(
        app,
        &deps.permissions,
        &deps.domain_permissions,
        filter,
        cursor,
    );
}

/// Read the system clipboard and paste it into the composer: an image (encoded
/// and gated on the active model's vision capability) when present, otherwise
/// the clipboard text. Surfaced errors and the no-vision refusal go to the meta
/// line rather than silently dropping the paste. Shared by the Ctrl+V key path
/// and the empty-bracketed-paste fallback in the event loop.
pub(super) fn paste_from_clipboard(
    app: &mut AppState,
    registry: &ProviderRegistry,
    catalog: Option<&ModelCatalog>,
) {
    use crate::tui::app::CopyNoticeKind;

    match app.clipboard().get_image() {
        Ok(Some(image)) => {
            if !crate::model_resolution::model_supports_vision(
                registry,
                catalog,
                &app.provider,
                &app.model,
            ) {
                app.set_copy_notice(
                    format!(
                        "{} can't see images — image paste ignored.",
                        crate::tui::pickers::short_model_label(&app.model)
                    ),
                    CopyNoticeKind::Error,
                );
                return;
            }
            match crate::tui::image_paste::encode_clipboard_image(
                &image,
                crate::tui::image_paste::MAX_ENCODED_IMAGE_BYTES,
            ) {
                Ok(encoded) => app.reduce(AppAction::PasteImage {
                    base64: encoded.base64,
                    byte_len: encoded.byte_len,
                    width: encoded.width,
                    height: encoded.height,
                }),
                Err(err) => app.set_copy_notice(err.to_string(), CopyNoticeKind::Error),
            }
        }
        // No image on the clipboard: fall back to text so Ctrl+V is a universal
        // paste in terminals that route it here instead of to bracketed paste.
        Ok(None) => match app.clipboard().get_text() {
            Ok(Some(text)) if !text.is_empty() => app.reduce(AppAction::PasteText(text)),
            Ok(_) => {}
            Err(err) => app.set_copy_notice(err.to_string(), CopyNoticeKind::Error),
        },
        Err(err) => app.set_copy_notice(err.to_string(), CopyNoticeKind::Error),
    }
}

/// Map the shared prompt tiers onto [`PermissionDecision`] — the outcome type
/// of every prompt family except sandbox escalation. The session tier is the
/// permission service's `AllowMatching`.
fn permission_decision(decision: PromptDecision) -> PermissionDecision {
    match decision {
        PromptDecision::AllowOnce => PermissionDecision::AllowOnce,
        PromptDecision::AllowSession => PermissionDecision::AllowMatching,
        PromptDecision::AllowProject => PermissionDecision::AllowForProject,
        PromptDecision::DenyProject => PermissionDecision::DenyForProject,
        PromptDecision::Deny => PermissionDecision::Deny,
    }
}

/// Map the shared prompt tiers onto [`EscalationDecision`]. Sandbox escapes
/// have no project tier (they are never persisted) — the keymap and the mouse
/// scrim never produce `AllowProject`/`DenyProject` for this family, so those
/// fall through to the fail-safe deny rather than silently widening (or
/// persisting) an approval.
fn escalation_decision(decision: PromptDecision) -> EscalationDecision {
    match decision {
        PromptDecision::AllowOnce => EscalationDecision::AllowOnce,
        PromptDecision::AllowSession => EscalationDecision::AllowForSession,
        PromptDecision::AllowProject | PromptDecision::DenyProject | PromptDecision::Deny => {
            EscalationDecision::Deny
        }
    }
}

/// Answer whichever approval prompt is open: extract the request id and the
/// display string from the family's modal kind, close the modal, reset the
/// requesting tool's elapsed timer, and deliver the mapped decision through
/// the interaction service. Sandbox escalations route through
/// [`respond_to_sandbox_escalation`], which additionally emits the audit
/// banner (and deliberately skips the timer reset).
fn respond_to_prompt_decision(
    app: &mut AppState,
    interaction: Arc<InteractionService>,
    family: PromptFamily,
    decision: PromptDecision,
) {
    if family == PromptFamily::SandboxEscalation {
        respond_to_sandbox_escalation(app, interaction, escalation_decision(decision));
        return;
    }
    let decision = permission_decision(decision);
    // Family-specific extractor: the matching modal kind yields the request id,
    // the string shown to the timer reset, and the log label. A mismatched
    // modal (stale action) is ignored, exactly like the per-family responders
    // this replaces.
    let (request_id, command, label) = match (family, app.modal.clone()) {
        (
            PromptFamily::Permission,
            Some(ModalKind::PermissionPrompt {
                request_id,
                command,
                ..
            }),
        ) => (request_id, command, "permission"),
        // For web domains the timer reset is best-effort for redirecting
        // WebFetch: time spent at this prompt must not count as network time.
        (
            PromptFamily::WebDomain,
            Some(ModalKind::WebDomainPrompt {
                request_id, url, ..
            }),
        ) => (request_id, url, "web domain"),
        (
            PromptFamily::ExtensionTool,
            Some(ModalKind::ExtensionToolPrompt { request_id, id, .. }),
        ) => (request_id, id, "extension tool"),
        (
            PromptFamily::HookTrust,
            Some(ModalKind::HookTrustPrompt {
                request_id, name, ..
            }),
        ) => (request_id, name, "hook trust"),
        _ => return,
    };

    app.modal = None;
    app.pending_question_visibility = false;
    app.focus = Focus::Input;
    app.reduce(AppAction::ResetPermissionToolTimer {
        command,
        at: std::time::Instant::now(),
    });
    spawn_panicked(async move {
        if let Err(err) = interaction
            .respond(request_id, InteractionOutcome::Permission(decision))
            .await
        {
            tracing::warn!(error = ?err, "failed to deliver {label} decision");
        }
    });
}

async fn submit_theme_picker(app: &mut AppState, session_store: Arc<Mutex<SessionStore>>) {
    let Some(ModalKind::ThemePicker { cursor, .. }) = app.modal.clone() else {
        return;
    };
    let Some(palette) = crate::tui::theme::theme_at(cursor) else {
        app.reduce(AppAction::CloseModal);
        return;
    };
    crate::tui::theme::set_theme(palette.name);
    app.reduce(AppAction::CloseModal);
    if let Err(err) = persist_current_theme(session_store).await {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Theme applied but could not be saved: {err:#}"),
        );
    }
}

fn cancel_theme_picker(app: &mut AppState) {
    let Some(ModalKind::ThemePicker { original_theme, .. }) = app.modal.clone() else {
        return;
    };
    crate::tui::theme::set_theme(&original_theme);
    app.reduce(AppAction::CloseModal);
}

fn respond_to_sandbox_escalation(
    app: &mut AppState,
    interaction: Arc<InteractionService>,
    decision: EscalationDecision,
) {
    let Some(ModalKind::SandboxEscalationPrompt {
        request_id,
        command,
        ..
    }) = app.modal.clone()
    else {
        return;
    };

    app.modal = None;
    app.pending_question_visibility = false;
    app.focus = Focus::Input;

    // Audit banner — reuse the existing status-notice mechanism so an approval is
    // durably recorded in the transcript (denials are not the audit subject).
    let scope = match decision {
        EscalationDecision::AllowOnce => Some("once"),
        EscalationDecision::AllowForSession => Some("for session"),
        EscalationDecision::Deny => None,
    };
    if let Some(scope) = scope {
        push_command_message(
            app,
            CommandOutputKind::Status,
            &format!("⚠ Sandbox escape approved ({scope}): {command}"),
        );
    }

    spawn_panicked(async move {
        if let Err(err) = interaction
            .respond(request_id, InteractionOutcome::SandboxEscalation(decision))
            .await
        {
            tracing::warn!(error = ?err, "failed to deliver sandbox escalation decision");
        }
    });
}

fn respond_to_question_submit(app: &mut AppState, interaction: Arc<InteractionService>) {
    let Some(ModalKind::QuestionPrompt {
        request_id,
        cursor,
        selected,
        multiple,
        options: _options,
        ..
    }) = app.modal.clone()
    else {
        return;
    };

    app.modal = None;
    app.focus = Focus::Input;
    // Multi-select submits are faithful: an all-deselected submit delivers the
    // empty set instead of silently substituting the cursor row. Consumers
    // decide what empty means (the question tool reports "no selected
    // options"; /bug builds a minimal bundle).
    let answer = if multiple {
        InteractionAnswer::Choices(
            selected
                .iter()
                .enumerate()
                .filter_map(|(idx, on)| on.then_some(idx))
                .collect(),
        )
    } else {
        InteractionAnswer::Choices(vec![cursor])
    };
    spawn_panicked(async move {
        if let Err(err) = interaction
            .respond(request_id, InteractionOutcome::Question(Some(answer)))
            .await
        {
            tracing::warn!(error = ?err, "failed to deliver question answer");
        }
    });
}

fn respond_to_question_cancel(app: &mut AppState, interaction: Arc<InteractionService>) {
    let Some(ModalKind::QuestionPrompt { request_id, .. }) = app.modal.clone() else {
        return;
    };

    app.modal = None;
    app.focus = Focus::Input;
    spawn_panicked(async move {
        if let Err(err) = interaction
            .respond(request_id, InteractionOutcome::Question(None))
            .await
        {
            tracing::warn!(error = ?err, "failed to deliver question cancellation");
        }
    });
    app.reduce(AppAction::CommandOutput {
        kind: CommandOutputKind::Status,
        text: "[question] Cancelled".to_string(),
    });
}

async fn toggle_selected_mcp_server(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some((server, state, cursor)) = selected_mcp_server(app) else {
        return;
    };
    let request = if state == McpServerStateKind::Disabled {
        McpCommandRequest::Enable(server.clone())
    } else {
        McpCommandRequest::Disable(server.clone())
    };
    run_mcp_modal_command(app, deps, request, server, cursor).await;
}

async fn reload_selected_mcp_server(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some((server, _state, cursor)) = selected_mcp_server(app) else {
        return;
    };
    run_mcp_modal_command(
        app,
        deps,
        McpCommandRequest::Reload(server.clone()),
        server,
        cursor,
    )
    .await;
}

async fn authorize_selected_mcp_server(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some((server, _state, _cursor)) = selected_mcp_server(app) else {
        return;
    };
    let persistence = app.credential_persistence;
    app.reduce(AppAction::CloseModal);
    let (hub, extensions) = {
        let agent = deps.agent.lock().await;
        (agent.mcp_hub().cloned(), agent.extensions().clone())
    };
    let request = McpCommandRequest::Auth {
        server,
        persistence: Some(persistence),
    };
    match apply_mcp_command(request, hub.as_deref(), extensions.as_ref()).await {
        Ok(message) => push_command_message(app, CommandOutputKind::Status, &message),
        Err(message) => push_command_message(app, CommandOutputKind::Error, &message),
    }
}

fn selected_mcp_server(app: &AppState) -> Option<(String, McpServerStateKind, usize)> {
    let ModalKind::McpServers { rows, cursor } = app.modal.as_ref()? else {
        return None;
    };
    let cursor = (*cursor).min(rows.len().saturating_sub(1));
    let row = rows.get(cursor)?;
    Some((row.name.clone(), row.state, cursor))
}

async fn run_mcp_modal_command(
    app: &mut AppState,
    deps: RuntimeActionDeps<'_>,
    request: McpCommandRequest,
    selected_server: String,
    fallback_cursor: usize,
) {
    let (hub, extensions) = {
        let agent = deps.agent.lock().await;
        (agent.mcp_hub().cloned(), agent.extensions().clone())
    };
    let result = apply_mcp_command(request, hub.as_deref(), extensions.as_ref()).await;
    match result {
        Ok(message) => push_command_message(app, CommandOutputKind::Status, &message),
        Err(message) => push_command_message(app, CommandOutputKind::Error, &message),
    }
    refresh_mcp_servers_modal(app, extensions.as_ref(), &selected_server, fallback_cursor);
}

fn refresh_mcp_servers_modal(
    app: &mut AppState,
    extensions: &crate::extension::status::ExtensionRegistry,
    selected_server: &str,
    fallback_cursor: usize,
) {
    let rows = crate::tui::mcp::mcp_server_rows(extensions);
    let cursor = rows
        .iter()
        .position(|row| row.name == selected_server)
        .unwrap_or_else(|| fallback_cursor.min(rows.len().saturating_sub(1)));
    app.reduce(AppAction::OpenModal(ModalKind::McpServers { rows, cursor }));
}

async fn submit_api_key_prompt(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(ModalKind::ApiKeyPrompt { provider_id, .. }) = app.modal.clone() else {
        return;
    };
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Authorization cannot run while the agent is running.",
        );
        return;
    }
    let Some(factory) = deps.registry.get(&provider_id) else {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Unknown provider '{provider_id}'."),
        );
        return;
    };
    let credential_persistence = app.provider_auth_form.credential_persistence;
    let requested_persistent_store = !matches!(
        credential_persistence,
        crate::session::CredentialPersistence::Session
    );
    let auth_input = match factory.metadata().auth_requirement {
        AuthRequirement::ApiKeyOptional => {
            let base_url = app.provider_auth_form.provider_base_url_input.clone();
            if base_url.trim().is_empty() {
                return;
            }
            let context_window = match app.provider_auth_form.parsed_context_window() {
                Ok(context_window) => context_window,
                Err(message) => {
                    push_command_message(app, CommandOutputKind::Error, &message);
                    return;
                }
            };
            AuthInput::OpenAiCompatible {
                base_url,
                api_key: non_empty_trimmed(app.provider_auth_form.api_key_input.clone()),
                model: non_empty_trimmed(app.provider_auth_form.provider_model_input.clone()),
                context_window,
                credential_persistence,
            }
        }
        AuthRequirement::ApiKeyRequired => {
            let api_key = app.provider_auth_form.api_key_input.clone();
            if api_key.trim().is_empty() {
                return;
            }
            AuthInput::ApiKey {
                api_key,
                persistence: credential_persistence,
            }
        }
        AuthRequirement::CodexCache => AuthInput::FromCodexCache,
    };
    app.reduce(AppAction::CloseModal);
    app.reduce(AppAction::SetTaskState(TaskState::Command));
    tracing::info!(provider = %provider_id, "submitting provider authorization");
    let generation = app.command_generation();
    let session_store = deps.session_store;
    let registry = deps.registry;
    let model_catalog = deps.model_catalog;
    let agent = deps.agent;
    let sender = deps.runtime_sender;
    spawn_panicked(async move {
        let result = async {
            // Canonical lock order: session_store before agent (see `task.rs`).
            let mut session = session_store.lock().await;
            let mutation = authorize_provider_with_input_as_current(
                &registry,
                &provider_id,
                auth_input,
                &mut session,
                Some(&model_catalog),
            )
            .await?;
            debug_assert_eq!(
                mutation.committed_state,
                crate::commands::ProviderAuthorizationState::Authorized
            );
            let mut messages = if requested_persistent_store
                && matches!(
                    session.session(&provider_id).credential_source,
                    crate::session::CredentialSource::Session
                ) {
                vec![CommandOutputEvent {
                    kind: CommandOutputKind::Status,
                    text: format!(
                        "{} unavailable; the API key is available only for this session.",
                        credential_persistence.label()
                    ),
                }]
            } else {
                Vec::new()
            };
            messages.extend(
                mutation
                    .warnings
                    .into_iter()
                    .map(|warning| CommandOutputEvent {
                        kind: CommandOutputKind::Status,
                        text: warning.to_string(),
                    }),
            );
            let provider = Some(Box::new(ProviderRunSelection {
                provider: session.provider_label().to_string(),
                model: session.current_session().model.clone(),
                reasoning: session.current_session().reasoning,
            }));
            let mut agent_guard = agent.lock().await;
            agent_guard.set_provider(
                build_provider(&registry, &session, Some(&model_catalog)),
                context_window_for_current_model_with_catalog(
                    &registry,
                    &session,
                    Some(&model_catalog),
                ) as usize,
                prompt_estimator_for_current_model_with_catalog(
                    &registry,
                    &session,
                    Some(&model_catalog),
                ),
                crate::model_resolution::active_model_identity(&session),
            );
            agent_guard.set_project_info_provider(
                crate::model_resolution::project_info_provider_state(&registry, &session),
            );
            let context_report = agent_guard.context_report();
            Ok::<_, anyhow::Error>((provider, context_report, messages))
        }
        .await;

        let event = match result {
            Ok((provider, context_report, messages)) => {
                RuntimeEvent::CommandFinished(Box::new(CommandOutcomeEvent::Applied {
                    generation: Some(generation),
                    clear_transcript: false,
                    messages,
                    provider,
                    context_report: Some(Box::new(context_report)),
                    quit: false,
                    open_modal: None,
                }))
            }
            Err(err) => RuntimeEvent::CommandFinished(Box::new(CommandOutcomeEvent::Applied {
                generation: Some(generation),
                clear_transcript: false,
                messages: vec![CommandOutputEvent {
                    kind: CommandOutputKind::Error,
                    text: format!("Authorization failed: {err:#}"),
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

fn submit_authorize_provider_picker(
    app: &mut AppState,
    tasks: &mut TaskController,
    deps: RuntimeActionDeps<'_>,
) {
    let Some(ModalKind::AuthorizeProviderPicker {
        providers,
        query,
        cursor,
    }) = app.modal.clone()
    else {
        tracing::debug!("AuthorizeProviderPickerSubmit ignored: no picker modal");
        return;
    };
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Authorization cannot run while the agent is running.",
        );
        return;
    }
    // `cursor` indexes the filtered view, so resolve the selection the same way.
    let filtered = crate::tui::pickers::filter_authorize_providers(&providers, &query);
    let Some(provider) = filtered
        .get(cursor.min(filtered.len().saturating_sub(1)))
        .copied()
    else {
        app.reduce(AppAction::CloseModal);
        return;
    };
    let selection = ProviderSelection {
        provider_id: provider.provider_id.clone(),
    };
    let input = format!("/authorize {}", selection.provider_id);
    app.reduce(AppAction::CloseModal);
    app.reduce(AppAction::SetTaskState(TaskState::Command));
    let command_deps =
        deps.command_task_deps(app.credential_persistence, app.active_persona.builtin());
    if let Err(err) = tasks.start_command(
        input,
        app.transcript.to_vec(),
        app.command_generation(),
        command_deps,
    ) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
    }
}

fn submit_unauthorize_provider_picker(app: &mut AppState) {
    let Some(ModalKind::UnauthorizeProviderPicker {
        providers,
        query,
        cursor,
    }) = app.modal.clone()
    else {
        tracing::debug!("UnauthorizeProviderPickerSubmit ignored: no picker modal");
        return;
    };
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Unauthorize cannot run while the agent is running.",
        );
        return;
    }
    // `cursor` indexes the filtered view, so resolve the selection the same way.
    let filtered = crate::tui::pickers::filter_authorize_providers(&providers, &query);
    let Some(provider) = filtered
        .get(cursor.min(filtered.len().saturating_sub(1)))
        .copied()
    else {
        app.reduce(AppAction::CloseModal);
        return;
    };
    app.reduce(AppAction::OpenModal(ModalKind::UnauthorizeConfirm {
        provider_id: provider.provider_id.clone(),
        display_name: provider.provider_label.clone(),
    }));
}

fn submit_unauthorize_confirm(
    app: &mut AppState,
    tasks: &mut TaskController,
    deps: RuntimeActionDeps<'_>,
) {
    let Some(ModalKind::UnauthorizeConfirm { provider_id, .. }) = app.modal.clone() else {
        tracing::debug!("UnauthorizeConfirmSubmit ignored: no confirm modal");
        return;
    };
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Unauthorize cannot run while the agent is running.",
        );
        return;
    }
    let input = format!("/unauthorize {provider_id}");
    app.reduce(AppAction::CloseModal);
    app.reduce(AppAction::SetTaskState(TaskState::Command));
    let command_deps =
        deps.command_task_deps(app.credential_persistence, app.active_persona.builtin());
    if let Err(err) = tasks.start_command(
        input,
        app.transcript.to_vec(),
        app.command_generation(),
        command_deps,
    ) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
    }
}

async fn submit_review_scope_picker(
    app: &mut AppState,
    tasks: &mut TaskController,
    deps: RuntimeActionDeps<'_>,
) {
    let Some(ModalKind::ReviewScopePicker { cursor }) = app.modal.clone() else {
        tracing::debug!("ReviewScopePickerSubmit ignored: no picker modal");
        return;
    };
    app.reduce(AppAction::CloseModal);
    let scopes = crate::agent::ReviewScope::all();
    let scope = scopes[cursor.min(scopes.len().saturating_sub(1))];
    review_changes(app, tasks, deps.agent, scope, deps.sink).await;
}

fn submit_local_model_wizard(app: &mut AppState, deps: RuntimeActionDeps<'_>) -> bool {
    let Some(ModalKind::LocalModelWizard { state }) = app.modal.as_ref() else {
        return true;
    };
    if state.loading {
        return true;
    }
    match state.step {
        LocalModelWizardStep::Setup => {
            let request_id = app.next_local_model_wizard_request_id();
            if let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal {
                start_local_model_fetch(state, request_id, deps.runtime_sender);
            }
            true
        }
        LocalModelWizardStep::Review => {
            start_local_model_commit(app, deps);
            true
        }
        LocalModelWizardStep::SelectModels | LocalModelWizardStep::Metadata => false,
    }
}

fn start_local_model_fetch(
    state: &mut LocalModelWizardState,
    request_id: u64,
    sender: mpsc::UnboundedSender<RuntimeEvent>,
) {
    if let Err(message) = state.validate_setup() {
        state.error = Some(message);
        return;
    }
    state.mark_fetch_started(request_id);
    let request = state.clone();
    spawn_panicked(async move {
        let result = crate::tui::local_model_wizard::fetch_wizard_models(&request)
            .await
            .map_err(|err| {
                crate::tui::event::UiError::new("Model fetch failed", format!("{err:#}"))
            });
        let _ = sender.send(RuntimeEvent::LocalModelWizardFetchFinished { request_id, result });
    });
}

fn start_local_model_commit(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Local model wizard cannot commit while the agent is running.",
        );
        return;
    }

    let Some(ModalKind::LocalModelWizard { state }) = &mut app.modal else {
        return;
    };
    let catalog_input = match state.catalog_input() {
        Ok(input) => input,
        Err(message) => {
            state.error = Some(message);
            return;
        }
    };
    let api_key = non_empty_trimmed(state.api_key.clone()).unwrap_or_default();
    let credential_source = state.credential_persistence.source();
    let editing_existing = state.editing_existing;
    state.mark_commit_started();
    let provider_id = catalog_input.connection.id.to_string();
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
            let report = if editing_existing {
                crate::model_catalog::replace_local_catalog_entry(&home_dir, catalog_input)?
            } else {
                crate::model_catalog::write_local_catalog_entry(&home_dir, catalog_input)?
            };
            let model_catalog = Arc::new(
                crate::model_catalog::load_catalog_from_home_and_project(
                    &home_dir,
                    trusted_project_root.as_deref(),
                )?,
            );
            let registry = Arc::new(ProviderRegistry::from_catalog(&model_catalog));
            let default_model = report
                .model_ids
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("local catalog writer returned no models"))?;
            let connection_id = report.connection_id.clone();
            let mut messages = Vec::<CommandOutputEvent>::new();
            let provider_selection = {
                // Canonical lock order: session_store before agent (see `task.rs`).
                let mut session = session_store.lock().await;
                session.ensure_provider(&provider_id);
                session.set_active_model_target(
                    &provider_id,
                    Some(connection_id),
                    Some(default_model.clone()),
                    default_model.as_str(),
                );
                if !(editing_existing && api_key.is_empty()) {
                    let resolved_source = session
                        .set_provider_credential(
                            &provider_id,
                            api_key,
                            credential_source.clone(),
                        )
                        .await;
                    if matches!(credential_source, crate::session::CredentialSource::Keyring)
                        && matches!(resolved_source, crate::session::CredentialSource::Session)
                    {
                        messages.push(CommandOutputEvent {
                            kind: CommandOutputKind::Status,
                            text: "OS credential store unavailable; the API key is available only for this session."
                                .to_string(),
                        });
                    }
                }
                {
                    let provider_session = session.session_mut(&provider_id);
                    provider_session.base_url = model_catalog
                        .connection(&report.connection_id)
                        .map(|connection| connection.default_base_url.to_string())
                        .unwrap_or_default();
                    provider_session.account_id.clear();
                    provider_session.is_fedramp_account = false;
                    provider_session.authorized_at = Some(std::time::SystemTime::now());
                }
                session.save_async().await?;
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
                (selection, context_report)
            };
            Ok::<_, anyhow::Error>((model_catalog, registry, provider_selection, messages))
        }
        .await;

        let event = match result {
            Ok((model_catalog, registry, (provider, context_report), messages)) => {
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
                        open_modal: None,
                    }),
                }
            }
            Err(err) => RuntimeEvent::CommandFinished(Box::new(CommandOutcomeEvent::Applied {
                generation: Some(generation),
                clear_transcript: false,
                messages: vec![CommandOutputEvent {
                    kind: CommandOutputKind::Error,
                    text: format!("Local model setup failed: {err:#}"),
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
async fn open_provider_manager(app: &mut AppState, deps: &RuntimeActionDeps<'_>) {
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

fn open_provider_manager_edit(app: &mut AppState, deps: &RuntimeActionDeps<'_>) {
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
fn open_provider_detail(app: &mut AppState, deps: &RuntimeActionDeps<'_>) {
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

fn provider_manager_remove_selected(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
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

fn submit_provider_remove_confirm(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
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
fn open_provider_mutation_for_args(
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

fn provider_is_builtin(connection_id: &crate::model_catalog::ConnectionId) -> bool {
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

// ── Agent composer / browser ────────────────────────────────────────────────

/// A no-op sink for one-shot completions (the composer's prompt generator): the
/// aggregated text is read from `StreamedResponse.content`, so streamed deltas
/// are discarded.
#[derive(Default)]
struct NoopSink;
impl crate::output::OutputSink for NoopSink {}

/// Clone the selected browser row so the modal borrow is released before the
/// caller mutates `app`.
fn selected_browser_agent(app: &AppState) -> Option<AgentBrowserRow> {
    let ModalKind::AgentBrowser { rows, cursor } = app.modal.as_ref()? else {
        return None;
    };
    let row = rows.get(*cursor)?;
    Some(row.clone())
}

fn open_agent_composer_new(app: &mut AppState) {
    app.reduce(AppAction::OpenModal(ModalKind::AgentComposer {
        state: Box::new(AgentComposerState::new()),
    }));
}

/// Open the shared model picker for one composer model-chain slot. The composer
/// is stashed and reopened when the picker submits or is cancelled.
async fn open_model_picker_for_composer(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let on_model_field = matches!(
        app.modal.as_ref(),
        Some(ModalKind::AgentComposer { state })
            if matches!(state.step, AgentComposerStep::Details)
                && matches!(
                    state.field,
                    AgentComposerField::Model | AgentComposerField::BackupModel
                )
    );
    if !on_model_field {
        return;
    }
    let entries = {
        let session = deps.session_store.lock().await;
        app.sync_cached_model_choices(&session, &deps.registry, Some(&deps.model_catalog));
        app.cached_model_choices.clone()
    };
    // Stash the composer while the picker is open; the picker submit/cancel path
    // reopens it. Taking the modal here replaces it with the picker below.
    let Some(ModalKind::AgentComposer { state }) = app.modal.take() else {
        return;
    };
    let target = match state.field {
        AgentComposerField::Model => crate::tui::app::ModelPickerTarget::ComposerPrimary,
        AgentComposerField::BackupModel => crate::tui::app::ModelPickerTarget::ComposerBackup,
        _ => return,
    };
    app.pending_composer_state = Some(state);
    app.model_picker.target = target;
    app.reduce(AppAction::OpenModal(ModalKind::ModelPicker { entries }));
}

async fn open_agent_composer_edit(app: &mut AppState) {
    let Some(row) = selected_browser_agent(app) else {
        return;
    };
    if let Some(id) = row.builtin_id {
        let Some(spec) = crate::tool::builtin_agents()
            .iter()
            .find(|spec| spec.id == id)
        else {
            return;
        };
        // `/agents` opens mid-run, and a running turn holds the agent lock for
        // its whole duration — locking here would freeze the TUI until the turn
        // ends. `app.builtin_subagents` is the same shared `Arc<RwLock>` handle,
        // cloned into AppState at startup, so read it lock-free instead.
        let settings_handle = app.builtin_subagents.clone();
        let settings = match effective_builtin_subagent_settings(
            &settings_handle,
            id,
            row.source_path.as_deref(),
        ) {
            Ok(settings) => settings,
            Err(err) => {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to read legacy built-in settings: {err:#}"),
                );
                return;
            }
        };
        app.reduce(AppAction::OpenModal(ModalKind::AgentComposer {
            state: Box::new(AgentComposerState::edit_builtin_subagent(
                id,
                spec.description,
                spec.instructions(),
                &settings,
                row.source_path,
            )),
        }));
        return;
    }
    let Some(path) = row.source_path else {
        return;
    };
    let is_global = matches!(row.origin, AgentOrigin::Global);
    let loaded = tokio::task::spawn_blocking(move || load_composer_edit_state(&path, is_global))
        .await
        .map_err(|error| anyhow::anyhow!("agent read task failed: {error}"))
        .and_then(|result| result);
    match loaded {
        Ok(state) => app.reduce(AppAction::OpenModal(ModalKind::AgentComposer {
            state: Box::new(state),
        })),
        Err(err) => push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Failed to open agent for editing: {err:#}"),
        ),
    }
}

fn load_legacy_builtin_settings(
    path: &Path,
) -> anyhow::Result<crate::subagent::BuiltinSubagentSettings> {
    let content = std::fs::read_to_string(path)?;
    let parsed = crate::resource::frontmatter::parse_frontmatter::<
        crate::resource::frontmatter::AgentFrontmatter,
    >(&content)?;
    Ok(crate::subagent::BuiltinSubagentSettings {
        enabled: parsed.frontmatter.enabled,
        primary_model: parsed.frontmatter.model,
        primary_effort: parsed.frontmatter.effort,
        fallback_model: parsed.frontmatter.fallback_model,
        fallback_effort: parsed.frontmatter.fallback_effort,
    })
}

fn effective_builtin_subagent_settings(
    handle: &crate::subagent::SharedBuiltinSubagentSettings,
    id: crate::subagent::BuiltinSubagentId,
    legacy_path: Option<&Path>,
) -> anyhow::Result<crate::subagent::BuiltinSubagentSettings> {
    if let Some(settings) = handle.get(id) {
        return Ok(settings);
    }
    legacy_path
        .map(load_legacy_builtin_settings)
        .transpose()
        .map(|settings| settings.unwrap_or_default())
}

fn load_composer_edit_state(path: &Path, is_global: bool) -> anyhow::Result<AgentComposerState> {
    let content = std::fs::read_to_string(path)?;
    let parsed = crate::resource::frontmatter::parse_frontmatter::<
        crate::resource::frontmatter::AgentFrontmatter,
    >(&content)?;
    let mut state = AgentComposerState::edit(
        parsed.frontmatter.name,
        parsed.frontmatter.description,
        is_global,
        parsed.frontmatter.tools.as_deref(),
        parsed.frontmatter.model.as_deref(),
        parsed.frontmatter.effort.as_deref(),
        parsed.frontmatter.fallback_model.as_deref(),
        parsed.frontmatter.fallback_effort.as_deref(),
        parsed.frontmatter.color.as_deref(),
        parsed.frontmatter.view.as_deref(),
        parsed.body,
        path.to_path_buf(),
    );
    // Preserve reserved frontmatter the composer doesn't surface, so an edit-save
    // never silently drops it.
    state.preserved_permission = parsed.frontmatter.permission;
    state.preserved_surface = parsed.frontmatter.surface;
    state.preserved_max_turns = parsed.frontmatter.max_turns;
    state.enabled = parsed.frontmatter.enabled;
    Ok(state)
}

fn open_agent_delete_confirm(app: &mut AppState) {
    let Some(row) = selected_browser_agent(app) else {
        return;
    };
    if row.builtin_id.is_some() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Built-in agents can't be deleted.",
        );
        return;
    }
    let Some(path) = row.source_path else {
        return;
    };
    app.reduce(AppAction::OpenModal(ModalKind::AgentDeleteConfirm {
        name: row.name,
        path,
    }));
}

async fn submit_agent_delete_confirm(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(ModalKind::AgentDeleteConfirm { name, path }) = app.modal.clone() else {
        return;
    };
    let delete_path = path.clone();
    let deleted =
        tokio::task::spawn_blocking(move || crate::resource::repository::remove_file(&delete_path))
            .await
            .map_err(|error| anyhow::anyhow!("agent delete task failed: {error}"))
            .and_then(|result| result.map_err(anyhow::Error::from));
    if let Err(err) = deleted {
        app.reduce(AppAction::CloseModal);
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Failed to delete {name}: {err}"),
        );
        return;
    }
    reopen_agent_browser(app, &deps, 0).await;
}

/// Enable/disable the selected agent from the browser, keeping the cursor put.
///
/// - A custom agent flips the `enabled:` field in its own `.md`.
/// - A built-in updates its typed SQLite settings row, preserving its compiled
///   prompt, tools, limits, and delegated-only identity.
async fn toggle_agent_enabled(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(row) = selected_browser_agent(app) else {
        return;
    };
    let cursor = match app.modal.as_ref() {
        Some(ModalKind::AgentBrowser { cursor, .. }) => *cursor,
        _ => 0,
    };
    let toggled = if let Some(id) = row.builtin_id {
        // Lock-free: the browser is reachable while a turn is running, which
        // holds the agent lock for its whole duration. `app.builtin_subagents`
        // is the same shared handle, so `upsert` still publishes to every live
        // holder (the delegated-agent tool, the composer).
        let handle = app.builtin_subagents.clone();
        let mut settings =
            match effective_builtin_subagent_settings(&handle, id, row.source_path.as_deref()) {
                Ok(settings) => settings,
                Err(err) => {
                    push_command_message(
                        app,
                        CommandOutputKind::Error,
                        &format!("Failed to read legacy built-in settings: {err:#}"),
                    );
                    return;
                }
            };
        settings.enabled = !row.enabled;
        persist_builtin_subagent_settings(deps.storage, &handle, id, settings).await
    } else {
        let Some(source_path) = row.source_path.clone() else {
            return;
        };
        let currently_enabled = row.enabled;
        tokio::task::spawn_blocking(move || {
            apply_custom_agent_enabled_toggle(&source_path, currently_enabled)
        })
        .await
        .map_err(|error| anyhow::anyhow!("agent toggle task failed: {error}"))
        .and_then(|result| result)
    };
    if let Err(err) = toggled {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Failed to toggle '{}': {err:#}", row.name),
        );
        return;
    }
    reopen_agent_browser(app, &deps, cursor).await;
}

/// Rewrite a custom definition's `enabled:` field without disturbing its body.
fn apply_custom_agent_enabled_toggle(path: &Path, currently_enabled: bool) -> anyhow::Result<()> {
    crate::resource::repository::mutate_text(path, |content| {
        let content = content.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("agent definition no longer exists: {}", path.display()),
            )
        })?;
        let updated =
            crate::tui::agent_composer::set_enabled_in_markdown(content, !currently_enabled);
        Ok((
            crate::resource::repository::TextMutation::Replace(updated),
            (),
        ))
    })?;
    Ok(())
}

/// Persist first, then publish the new setting to every live agent-tool/editor
/// holder. A failed database write therefore cannot leave memory ahead of disk.
async fn persist_builtin_subagent_settings(
    storage: &Storage,
    handle: &crate::subagent::SharedBuiltinSubagentSettings,
    id: crate::subagent::BuiltinSubagentId,
    settings: crate::subagent::BuiltinSubagentSettings,
) -> anyhow::Result<()> {
    storage
        .upsert_builtin_subagent_settings(id, &settings)
        .await?;
    handle.upsert(id, settings);
    Ok(())
}

/// Reload the shared registry from disk and rebuild the browser rows. Works
/// off the app's shared handles, never the agent lock — a running turn holds
/// that lock for its entire duration.
async fn reload_agent_browser_rows(
    app: &AppState,
    deps: &RuntimeActionDeps<'_>,
) -> anyhow::Result<Vec<AgentBrowserRow>> {
    let handle = app.custom_agents.clone();
    let builtin_settings = app.builtin_subagents.snapshot();
    let project_root = deps.project_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        crate::resource::agent::reload_into(&handle, &project_root);
        browser_rows_with_settings(
            &crate::resource::agent::snapshot(&handle),
            &builtin_settings,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("agent discovery task failed: {error}"))
}

async fn reopen_agent_browser(app: &mut AppState, deps: &RuntimeActionDeps<'_>, cursor: usize) {
    match reload_agent_browser_rows(app, deps).await {
        Ok(rows) => {
            // The registry handle is already swapped; only the prompt's
            // Subagents index needs the agent. Mid-run the lock is held for
            // the whole turn, so defer to the next lock boundary.
            match deps.agent.try_lock() {
                Ok(mut agent) => agent.refresh_agents_index(),
                Err(_) => app.agents_index_refresh_pending = true,
            }
            normalize_active_persona_after_agent_reload(app);
            let cursor = cursor.min(rows.len().saturating_sub(1));
            app.reduce(AppAction::OpenModal(ModalKind::AgentBrowser {
                rows,
                cursor,
            }));
        }
        Err(error) => push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Failed to reload agents: {error:#}"),
        ),
    }
}

fn normalize_active_persona_after_agent_reload(app: &mut AppState) {
    let Some(name) = app.active_persona.custom_name() else {
        return;
    };
    let custom = crate::resource::agent::snapshot(&app.custom_agents);
    let still_selectable = custom.get(name).is_some_and(|def| {
        def.enabled && def.allows_mode() && !crate::tool::is_builtin_agent(&def.name)
    });
    if !still_selectable {
        app.active_persona = crate::agent::ActivePersona::default();
    }
}

/// Enable/disable the selected built-in skill by editing the project
/// `.bonsai/skills/.disabled` list, then reload the agent's registry so the
/// change takes effect live — the `skill` tool stops/starts offering it and the
/// prompt's Skills index re-renders, no relaunch. The manager rows are rebuilt
/// from the now-updated registry so the row's state label flips in place. No
/// chat message on success — the modal is the feedback. A project skill (not
/// disablable) is a silent no-op; only a genuine filesystem failure surfaces.
async fn toggle_skill_disabled(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let (name, disable, disablable, cursor) = match app.modal.as_ref() {
        Some(ModalKind::SkillManager { rows, cursor }) => match rows.get(*cursor) {
            Some(row) => (row.name.clone(), !row.disabled, row.disablable, *cursor),
            None => return,
        },
        _ => return,
    };
    if !disablable {
        return; // project/global skills are managed by their files
    }
    let project_root = deps.project_root.to_path_buf();
    let disabled_name = name.clone();
    let changed = tokio::task::spawn_blocking(move || {
        crate::resource::discovery::set_disabled(
            &project_root,
            &disabled_name,
            disable,
            crate::resource::discovery::ResourceKind::Skills,
        )
    })
    .await
    .map_err(|error| anyhow::anyhow!("skill update task failed: {error}"))
    .and_then(|result| result.map_err(anyhow::Error::from));
    if let Err(err) = changed {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Failed to update '{name}': {err:#}"),
        );
        return;
    }
    // Apply live: swap the shared registry off-loop (the `skill` tool reads
    // the same handle, so it stops/starts offering the skill immediately),
    // then refresh the prompt's Skills index. A running turn holds the agent
    // lock for its entire duration — locking here would freeze the TUI until
    // the turn ended — so mid-run the index refresh defers to the next lock
    // boundary (`sync_agent_state_at_lock_boundary`).
    let handle = app.skills.clone();
    let reload_root = deps.project_root.to_path_buf();
    if let Err(error) = tokio::task::spawn_blocking(move || {
        crate::resource::skill::reload_into(&handle, &reload_root);
    })
    .await
    {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Failed to reload skills: {error}"),
        );
        return;
    }
    match deps.agent.try_lock() {
        Ok(mut agent) => agent.refresh_skills_index(),
        Err(_) => app.skills_index_refresh_pending = true,
    }
    let rows = skill_rows_from_app(app);
    let cursor = cursor.min(rows.len().saturating_sub(1));
    app.reduce(AppAction::OpenModal(ModalKind::SkillManager {
        rows,
        cursor,
    }));
}

/// Load the selected skill's body into the conversation, or unload it if it is
/// already loaded — then reload the rows so the loaded marker flips. Only
/// advertised skills (user skills + active built-ins) can be loaded; loading an
/// inactive/disabled one is a silent no-op (the row shows why). No chat
/// message — the modal's loaded marker is the feedback.
async fn toggle_skill_loaded(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let (name, loaded, cursor) = match app.modal.as_ref() {
        Some(ModalKind::SkillManager { rows, cursor }) => match rows.get(*cursor) {
            Some(row) => (row.name.clone(), row.loaded, *cursor),
            None => return,
        },
        _ => return,
    };
    // Loading mutates the conversation itself, so this genuinely needs the
    // agent — but a running turn holds the lock for its whole duration, and
    // awaiting it here would freeze the TUI until the turn ends. try_lock
    // succeeds whenever the agent is idle.
    let changed = match deps.agent.try_lock() {
        Ok(mut agent) => {
            let changed = if loaded {
                agent.unload_skill(&name)
            } else {
                // Render to an owned String to end the immutable borrow before the
                // mutable push. `get` only returns advertised skills.
                match agent
                    .skills()
                    .get(&name)
                    .map(|def| def.render_for_context())
                {
                    Some(body) => {
                        agent.push_user_context_note(&body);
                        agent.mark_skill_loaded(&name);
                        true
                    }
                    None => false, // not loadable this session; no-op
                }
            };
            app.refresh_agent_mirrors(&agent);
            changed
        }
        Err(_) => {
            push_command_message(
                app,
                CommandOutputKind::Status,
                "Skills can't be loaded or unloaded while the agent is running — try again when it finishes.",
            );
            return;
        }
    };
    if !changed {
        return;
    }
    let rows = skill_rows_from_app(app);
    let cursor = cursor.min(rows.len().saturating_sub(1));
    app.reduce(AppAction::OpenModal(ModalKind::SkillManager {
        rows,
        cursor,
    }));
}

/// Rebuild the skills manager rows lock-free: the shared registry handle plus
/// the app's loaded-skills mirror (refreshed by every guard-holding site via
/// `refresh_agent_mirrors`).
fn skill_rows_from_app(app: &AppState) -> Vec<crate::tui::skill_manager::SkillRow> {
    crate::tui::skill_manager::skill_rows(
        &crate::resource::skill::snapshot(&app.skills),
        &app.loaded_skills,
    )
}

/// Returns `true` when the submit was fully handled here (the `Review` commit);
/// `false` for earlier steps, which the reducer advances.
async fn submit_agent_composer(app: &mut AppState, deps: RuntimeActionDeps<'_>) -> bool {
    let Some(ModalKind::AgentComposer { state }) = app.modal.as_ref() else {
        return true;
    };
    if state.generating {
        return true;
    }
    if !matches!(state.step, AgentComposerStep::Review) {
        return false;
    }
    commit_agent_composer(app, deps).await;
    true
}

async fn commit_agent_composer(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Can't save an agent while the agent is running.",
        );
        return;
    }
    let Some(ModalKind::AgentComposer { state }) = app.modal.as_ref() else {
        return;
    };
    if let Err(message) = state.validate_for_save() {
        if let Some(ModalKind::AgentComposer { state }) = &mut app.modal {
            state.error = Some(message);
        }
        return;
    }
    if let Some((id, settings)) = state.builtin_subagent_settings() {
        // Same shared handle as the agent's, read lock-free. The `is_busy`
        // guard above already blocks saving mid-run, but locking the agent here
        // would still be wrong the moment that guard ever narrows.
        let handle = app.builtin_subagents.clone();
        if let Err(err) =
            persist_builtin_subagent_settings(deps.storage, &handle, id, settings).await
        {
            if let Some(ModalKind::AgentComposer { state }) = &mut app.modal {
                state.error = Some(format!("Failed to save built-in settings: {err:#}"));
            }
            return;
        }
        reopen_agent_browser(app, &deps, 0).await;
        return;
    }

    let markdown = state.render_agent_md();
    let path = agent_file_path(state, deps.storage.home_dir(), deps.project_root);
    let previous_path = state.edit_target.source_path().map(Path::to_path_buf);

    let write_path = path.clone();
    let write = tokio::task::spawn_blocking(move || {
        crate::resource::repository::save_agent(previous_path.as_deref(), &write_path, &markdown)
    })
    .await
    .map_err(|error| anyhow::anyhow!("agent save task failed: {error}"))
    .and_then(|result| result.map_err(anyhow::Error::from));
    if let Err(err) = write {
        if let Some(ModalKind::AgentComposer { state }) = &mut app.modal {
            state.error = Some(format!("Failed to write {}: {err}", path.display()));
        }
        return;
    }
    reopen_agent_browser(app, &deps, 0).await;
}

fn start_agent_composer_generate(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(ModalKind::AgentComposer { state }) = app.modal.as_ref() else {
        return;
    };
    if state.generating || !matches!(state.step, AgentComposerStep::Prompt) {
        return;
    }
    if state.description.text.trim().is_empty() {
        if let Some(ModalKind::AgentComposer { state }) = &mut app.modal {
            state.error = Some("Add a description first, then generate.".to_string());
        }
        return;
    }
    let (system, user) = state.generation_messages();
    let model = state.selected_model().map(str::to_string);
    let request_id = app.next_local_model_wizard_request_id();
    if let Some(ModalKind::AgentComposer { state }) = &mut app.modal {
        state.mark_generating(request_id);
    }

    let registry = deps.registry.clone();
    let session_store = deps.session_store.clone();
    let catalog = deps.model_catalog.clone();
    let sender = deps.runtime_sender.clone();
    spawn_panicked(async move {
        let result =
            generate_agent_prompt(&registry, &session_store, &catalog, model, system, user)
                .await
                .map_err(|err| {
                    crate::tui::event::UiError::new("Prompt generation failed", format!("{err:#}"))
                });
        let _ = sender.send(RuntimeEvent::AgentComposerPromptGenerated { request_id, result });
    });
}

async fn generate_agent_prompt(
    registry: &ProviderRegistry,
    session_store: &Mutex<SessionStore>,
    catalog: &ModelCatalog,
    model: Option<String>,
    system: String,
    user: String,
) -> anyhow::Result<String> {
    use async_openai::types::chat::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestUserMessageArgs,
    };

    let provider = {
        let session = session_store.lock().await;
        build_provider_for_optional_model(registry, &session, Some(catalog), model.as_deref())
    };
    let messages = vec![
        ChatCompletionRequestMessage::System(
            ChatCompletionRequestSystemMessageArgs::default()
                .content(system)
                .build()?,
        ),
        ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content(user)
                .build()?,
        ),
    ];
    let sink: crate::output::SharedSink = Arc::new(NoopSink);
    let response = provider
        .chat_stream(
            &messages,
            &[],
            tokio_util::sync::CancellationToken::new(),
            sink,
        )
        .await?;
    let content = response.content.trim().to_string();
    if content.is_empty() {
        anyhow::bail!("the model returned an empty prompt");
    }
    Ok(content)
}

/// Open the model picker to override the selected `/subagents` agent's
/// provider/model/effort. The chosen agent name is stashed so picker submit
/// writes an override rather than switching the session model.
async fn override_selected_subtask_model(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(agent) = app.selected_subtask().map(|sub| sub.agent.clone()) else {
        return;
    };
    let entries = {
        let session = deps.session_store.lock().await;
        app.sync_cached_model_choices(&session, &deps.registry, Some(&deps.model_catalog));
        app.cached_model_choices.clone()
    };
    if entries.is_empty() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Authorize a provider before overriding a subagent model. Try `/authorize <provider>`.",
        );
        return;
    }
    app.pending_agent_model_override = Some(agent);
    app.reduce(AppAction::OpenModal(ModalKind::ModelPicker { entries }));
}

async fn submit_model_picker(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(ModalKind::ModelPicker { entries }) = app.modal.clone() else {
        tracing::debug!("ModelPickerSubmit ignored: no picker modal");
        return;
    };
    if submit_composer_model_picker(app, &entries) {
        return;
    }
    let Some(entry) = app.model_picker_selected_model(&entries).cloned() else {
        tracing::debug!("ModelPickerSubmit: no entry selected");
        return;
    };
    let reasoning = app.model_picker_selected_reasoning(&entry);

    // Self-review target: `/self-review model` pins the reviewer lane's model.
    // Persisted under the SelfReview settings id (not the live name-keyed
    // override map) so it survives a restart, exactly like a `/subagents` model.
    if std::mem::take(&mut app.pending_self_review_model) {
        let primary_effort = crate::model_catalog::parse_reasoning_effort(reasoning.as_str())
            .ok()
            .map(|_| reasoning.as_str().to_string());
        let settings = crate::subagent::BuiltinSubagentSettings {
            enabled: true,
            primary_model: Some(
                entry
                    .selection_input()
                    .unwrap_or_else(|| entry.model.clone()),
            ),
            primary_effort,
            fallback_model: None,
            fallback_effort: None,
        };
        match deps
            .storage
            .upsert_builtin_subagent_settings(
                crate::subagent::BuiltinSubagentId::SelfReview,
                &settings,
            )
            .await
        {
            Ok(()) => {
                app.builtin_subagents
                    .upsert(crate::subagent::BuiltinSubagentId::SelfReview, settings);
                push_command_message(
                    app,
                    CommandOutputKind::Status,
                    &format!("Self-review model set to {}.", entry.model),
                );
            }
            Err(err) => push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("Failed to set self-review model: {err:#}"),
            ),
        }
        app.reduce(AppAction::CloseModal);
        return;
    }

    // Agent-target mode: the picker was opened from `/subagents` to override one
    // agent's provider/model/effort. Record the override (no session mutation)
    // and return to the subagents view; the next delegation to that agent picks
    // it up.
    if let Some(agent) = app.pending_agent_model_override.take() {
        let connection_id = entry.connection_id.parse().ok();
        let model_id = entry
            .model_id
            .as_deref()
            .and_then(|model| model.parse().ok());
        app.subagent_model_overrides
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                agent,
                crate::subagent::SubagentModelOverride::explicit(
                    entry.provider_id.clone(),
                    connection_id,
                    model_id,
                    entry.model.clone(),
                    reasoning,
                ),
            );
        app.reduce(AppAction::CloseModal);
        app.reduce(AppAction::OpenSubtaskList);
        return;
    }

    let selection_input = entry.selection_input();
    let selection_label = selection_input
        .clone()
        .unwrap_or_else(|| entry.model.clone());
    let selection = ModelSelection {
        provider_id: entry.provider_id,
        connection_id: entry.connection_id,
        model_id: entry.model_id,
        model: entry.model,
        selection_input,
        reasoning,
    };
    if app.task_state.is_busy() {
        app.reduce(AppAction::CloseModal);
        queue_deferred_model_selection(app, format!("/model {selection_label}"), selection);
        return;
    }
    if app.first_run_step == Some(crate::onboarding::FirstRunStep::Model) {
        app.first_run_model_selection_pending = Some(app.command_generation());
    }
    app.reduce(AppAction::CloseModal);
    switch_model_selection(
        selection,
        app,
        deps.agent,
        deps.session_store,
        deps.registry,
        deps.model_catalog,
        deps.runtime_sender,
    );
}

/// `/refresh` — open the live refresh-status modal and spawn the async driver.
///
/// The driver refreshes models.dev and every authorized provider concurrently
/// (`FuturesUnordered`) and emits one [`RuntimeEvent::RefreshSourceCompleted`]
/// per source as it settles, so the modal updates live. A final
/// [`RuntimeEvent::RefreshAllSourcesCompleted`] marks the refresh as finished.
async fn start_refresh(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    use crate::commands::RefreshSourceState;

    let catalog = deps.model_catalog.clone();
    let has_models_dev = catalog.has_catalog_home_dir();

    // Collect authorized providers + their sessions while we still hold the
    // session store. The driver runs each concurrently afterwards.
    let registry = deps.registry.clone();
    let providers: Vec<(
        std::sync::Arc<dyn crate::provider::ProviderFactory>,
        crate::session::ProviderSession,
        String,
    )> = {
        let store = deps.session_store.clone();
        let store = store.lock().await;
        registry
            .all()
            .iter()
            .filter_map(|factory| {
                let metadata = factory.metadata();
                let session = store.providers.get(metadata.id.as_ref())?.clone();
                factory
                    .is_authorized(&session)
                    .then(|| (factory.clone(), session, metadata.display_name.to_string()))
            })
            .collect()
    };

    // Build the initial source list: models.dev first (index 0), then providers.
    // All rows start `Pending` (yellow dot).
    let mut sources: Vec<RefreshSourceState> = Vec::new();
    let models_dev_index = if has_models_dev {
        sources.push(RefreshSourceState::pending("Models.dev"));
        Some(0)
    } else {
        None
    };
    let provider_base = sources.len();
    for (_, _, display_name) in &providers {
        sources.push(RefreshSourceState::pending(display_name.clone()));
    }

    if sources.is_empty() {
        app.reduce(AppAction::CommandOutput {
            kind: crate::tui::event::CommandOutputKind::Error,
            text: "No authorized providers to refresh. Try `/authorize <provider>`.".to_string(),
        });
        return;
    }

    let generation = app.next_local_model_wizard_request_id();
    app.modal_scroll = 0;
    app.pending_question_visibility = false;
    app.modal_return_focus = Some(app.focus);
    app.modal = Some(ModalKind::Refresh {
        sources,
        cursor: 0,
        generation,
    });
    app.focus = Focus::Modal;

    let catalog = deps.model_catalog.clone();
    let sender = deps.runtime_sender.clone();

    spawn_panicked(async move {
        use futures::stream::{FuturesUnordered, StreamExt};

        let mut futures = FuturesUnordered::<
            std::pin::Pin<
                Box<dyn std::future::Future<Output = (usize, RefreshSourceState)> + Send>,
            >,
        >::new();

        // models.dev source.
        if let Some(idx) = models_dev_index {
            let catalog = catalog.clone();
            futures.push(Box::pin(async move {
                (
                    idx,
                    crate::commands::refresh_models_dev_source(&catalog).await,
                )
            }));
        }

        // Provider sources.
        for (index, (factory, session, display_name)) in providers.into_iter().enumerate() {
            let source_index = provider_base + index;
            let catalog = catalog.clone();
            futures.push(Box::pin(async move {
                let source = crate::commands::refresh_provider_source(
                    &factory,
                    &session,
                    &display_name,
                    &catalog,
                )
                .await;
                (source_index, source)
            }));
        }

        while let Some((index, source)) = futures.next().await {
            let _ = sender.send(RuntimeEvent::RefreshSourceCompleted {
                generation,
                index,
                source,
            });
        }
        let _ = sender.send(RuntimeEvent::RefreshAllSourcesCompleted { generation });
    });
}
/// Returns whether this picker is composer-owned, even if its stash is missing,
/// so an invalid composer transition can never fall through to session mutation.
fn submit_composer_model_picker(app: &mut AppState, entries: &[ModelOption]) -> bool {
    let target = app.model_picker.target;
    if !target.is_composer() {
        return false;
    }
    if app.model_picker_reset_selected() {
        let Some(mut state) = app.pending_composer_state.take() else {
            return true;
        };
        match target {
            crate::tui::app::ModelPickerTarget::ComposerPrimary => state.set_model(None),
            crate::tui::app::ModelPickerTarget::ComposerBackup => state.set_fallback_model(None),
            crate::tui::app::ModelPickerTarget::Session
            | crate::tui::app::ModelPickerTarget::Subagent => {}
        }
        app.reduce(AppAction::OpenModal(ModalKind::AgentComposer { state }));
        return true;
    }

    let Some(entry) = app.model_picker_selected_model(entries).cloned() else {
        tracing::debug!("ModelPickerSubmit: no entry selected");
        return true;
    };
    let reasoning = app.model_picker_selected_reasoning(&entry);

    // Composer-target mode writes only the explicitly targeted chain slot.
    if let Some(mut state) = app.pending_composer_state.take() {
        let selector = format!("{}:{}", entry.connection_id, entry.display_model());
        match target {
            crate::tui::app::ModelPickerTarget::ComposerPrimary => {
                state.field = AgentComposerField::Model;
                state.set_focused_model_selection(selector, reasoning);
            }
            crate::tui::app::ModelPickerTarget::ComposerBackup => {
                state.field = AgentComposerField::BackupModel;
                state.set_focused_model_selection(selector, reasoning);
            }
            crate::tui::app::ModelPickerTarget::Session
            | crate::tui::app::ModelPickerTarget::Subagent => {}
        }
        app.reduce(AppAction::OpenModal(ModalKind::AgentComposer { state }));
        return true;
    }
    true
}

async fn assign_model_picker_shortcut(
    app: &mut AppState,
    deps: RuntimeActionDeps<'_>,
    key: ModelShortcutKey,
) {
    let Some(ModalKind::ModelPicker { entries }) = app.modal.clone() else {
        tracing::debug!("ModelPickerAssignShortcut ignored: no picker modal");
        return;
    };
    let Some(entry) = app.model_picker_selected_model(&entries).cloned() else {
        tracing::debug!("ModelPickerAssignShortcut: no entry selected");
        return;
    };
    let reasoning = app.model_picker_selected_reasoning(&entry);
    let binding = ModelShortcutBinding {
        provider_id: entry
            .provider_id
            .parse()
            .unwrap_or_else(|_| ConnectionId::fallback("unknown")),
        connection_id: entry.connection_id.parse().ok(),
        model_id: entry
            .model_id
            .as_deref()
            .and_then(|model| model.parse().ok()),
        model: entry.model.clone(),
        reasoning,
    };

    let (session_snapshot, refreshed_entries, toggled_off) = {
        let mut session = deps.session_store.lock().await;
        // Toggle: pressing a shortcut key on the exact model+reasoning it
        // already points at clears it. Pressing it elsewhere reassigns the key.
        let already_bound = session.model_shortcut_binding(key).is_some_and(|bound| {
            let provider_id: ConnectionId = entry
                .provider_id
                .parse()
                .unwrap_or_else(|_| ConnectionId::fallback("unknown"));
            let model_id: Option<ModelId> = entry.model_id.as_deref().and_then(|m| m.parse().ok());
            bound.matches_selection(&provider_id, &entry.model, model_id.as_ref(), reasoning)
        });
        if already_bound {
            session.clear_model_shortcut_binding(key);
        } else {
            session.set_model_shortcut_binding(key, binding);
        }
        let refreshed_entries =
            refresh_model_picker_entries(&entries, &session, &deps.registry, &deps.model_catalog);
        app.sync_cached_model_choices(&session, &deps.registry, Some(&deps.model_catalog));
        (session.clone(), refreshed_entries, already_bound)
    };
    if let Err(err) = session_snapshot.save_async().await {
        let verb = if toggled_off { "clear" } else { "save" };
        app.reduce(AppAction::CommandOutput {
            kind: CommandOutputKind::Error,
            text: format!(
                "Failed to {verb} model shortcut {}: {err}",
                key.as_command()
            ),
        });
        return;
    }
    if let Some(ModalKind::ModelPicker { entries }) = &mut app.modal {
        *entries = refreshed_entries;
    }
}

fn refresh_model_picker_entries(
    entries: &[crate::tui::pickers::ModelOption],
    session: &SessionStore,
    registry: &ProviderRegistry,
    catalog: &ModelCatalog,
) -> Vec<crate::tui::pickers::ModelOption> {
    entries
        .iter()
        .filter_map(|entry| {
            let factory = registry.lookup(&entry.provider_id)?;
            let metadata = factory.metadata();
            Some(crate::tui::pickers::ModelOption::from_provider_model(
                Some(catalog),
                &entry.provider_id,
                &entry.provider_label,
                session,
                metadata,
                entry.model.clone(),
            ))
        })
        .collect()
}

async fn submit_busy_command(
    app: &mut AppState,
    tasks: &TaskController,
    deps: RuntimeActionDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
) {
    let Some(ModalKind::BusyCommand {
        input,
        rows,
        cursor,
    }) = app.modal.clone()
    else {
        tracing::debug!("BusyCommandSubmit ignored: no busy-command modal");
        return;
    };
    let Some(row) = rows.get(cursor.min(rows.len().saturating_sub(1))) else {
        app.reduce(AppAction::CloseModal);
        return;
    };

    match row.action {
        BusyCommandAction::QueueDeferred => {
            app.reduce(AppAction::CloseModal);
            queue_deferred_command(app, input);
        }
        BusyCommandAction::OpenReadOnly => {
            app.reduce(AppAction::CloseModal);
            // Same executor as the running composer path
            // (`handle_running_slash_command`): per-command non-idle behavior
            // is declared once in `crate::commands::busy_behavior_for` and
            // must not fork per dispatch surface.
            apply_non_idle_read_only_command(&input, app, deps.persistence_deps(), state).await;
        }
        BusyCommandAction::CancelCurrentRun => {
            app.reduce(AppAction::CloseModal);
            let queued_ids = app.queued_input_ids();
            if let Err(err) = tasks.cancel_agent_messages(&queued_ids) {
                app.reduce(AppAction::Agent(crate::tui::event::UiEvent::Error(err)));
            }
            app.reduce(AppAction::DropQueuedInput);
            tasks.cancel();
            app.reduce(AppAction::SetTaskState(TaskState::Cancelling));
        }
        BusyCommandAction::Dismiss => {
            app.reduce(AppAction::CloseModal);
        }
    }
}

fn queue_deferred_command(app: &mut AppState, input: String) {
    let label = input.trim().to_string();
    app.reduce(AppAction::QueueDeferredCommand { input, label });
}

fn queue_deferred_model_selection(app: &mut AppState, input: String, selection: ModelSelection) {
    let label = input.trim().to_string();
    app.reduce(AppAction::QueueDeferredModelSelection {
        input,
        label,
        selection,
    });
}

async fn submit_session_picker(
    app: &mut AppState,
    deps: RuntimeActionDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
) {
    let Some(ModalKind::SessionPicker { sessions, cursor }) = app.modal.clone() else {
        tracing::debug!("SessionPickerSubmit ignored: no picker modal");
        return;
    };
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Session resume cannot run while the agent is running.",
        );
        return;
    }
    let Some(session) = sessions.get(cursor.min(sessions.len().saturating_sub(1))) else {
        app.reduce(AppAction::CloseModal);
        return;
    };
    if let Err(err) = resume_session(session.id, app, deps.persistence_deps(), state).await {
        push_command_message(app, CommandOutputKind::Error, &format!("{err:#}"));
    }
}

fn open_selected_plan_choice(app: &mut AppState) {
    let Some(plan) = app.selected_saved_plan() else {
        return;
    };
    app.reduce(AppAction::OpenModal(ModalKind::PlanOpenChoice {
        plan,
        cursor: 0,
    }));
}

fn open_selected_plan_delete_confirm(app: &mut AppState) {
    let Some(plan) = app.selected_saved_plan() else {
        return;
    };
    app.reduce(AppAction::OpenModal(ModalKind::PlanDeleteConfirm { plan }));
}

async fn submit_plan_open_choice(
    app: &mut AppState,
    deps: RuntimeActionDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
) {
    let Some((plan, mode)) = app.selected_plan_open_mode() else {
        return;
    };
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Saved plans cannot be opened while the agent is running.",
        );
        return;
    }
    if let Err(err) = open_saved_plan(plan, mode, app, deps.persistence_deps(), state).await {
        push_command_message(app, CommandOutputKind::Error, &format!("{err:#}"));
    }
}

async fn submit_plan_delete_confirm(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(ModalKind::PlanDeleteConfirm { plan }) = app.modal.clone() else {
        return;
    };
    if let Err(err) =
        delete_saved_plan_from_picker(app, deps.storage, deps.session_project_root, plan).await
    {
        push_command_message(app, CommandOutputKind::Error, &format!("{err:#}"));
    }
}

fn open_selected_session_delete_confirm(app: &mut AppState) {
    let Some(session) = app.selected_session() else {
        return;
    };
    // Guard: don't allow deleting the currently active session.
    if Some(session.id) == app.current_session_id {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Cannot delete the active session.",
        );
        return;
    }
    app.reduce(AppAction::OpenModal(ModalKind::SessionDeleteConfirm {
        session,
    }));
}

async fn submit_session_delete_confirm(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(ModalKind::SessionDeleteConfirm { session }) = app.modal.clone() else {
        return;
    };
    match deps
        .storage
        .forget_session(deps.session_project_root, session.id)
        .await
    {
        Ok(crate::storage::ForgetSessionOutcome::Forgotten) => {
            if Some(session.id) == app.current_session_id {
                app.current_session_id = None;
            }
            // Refresh the session picker list, removing the deleted session
            // and excluding the active session.
            match deps
                .storage
                .recent_sessions_for_project(deps.session_project_root, 20)
                .await
            {
                Ok(sessions) => {
                    let active_id = *deps.active_session_id.lock().await;
                    let sessions: Vec<_> = sessions
                        .into_iter()
                        .filter(|s| Some(s.id) != active_id)
                        .collect();
                    app.reduce(AppAction::OpenModal(ModalKind::SessionPicker {
                        sessions,
                        cursor: 0,
                    }));
                    push_command_message(
                        app,
                        CommandOutputKind::Status,
                        &format!("Deleted session \"{}\".", session.name),
                    );
                }
                Err(err) => {
                    app.reduce(AppAction::CloseModal);
                    push_command_message(
                        app,
                        CommandOutputKind::Error,
                        &format!("Session deleted but failed to refresh picker: {err:#}"),
                    );
                }
            }
        }
        Ok(crate::storage::ForgetSessionOutcome::Live) => {
            app.reduce(AppAction::CloseModal);
            push_command_message(
                app,
                CommandOutputKind::Error,
                "Cannot delete a live session with an active heartbeat.",
            );
        }
        Ok(crate::storage::ForgetSessionOutcome::NotFound) => {
            app.reduce(AppAction::CloseModal);
            push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("Session #{} not found.", session.id),
            );
        }
        Ok(crate::storage::ForgetSessionOutcome::DifferentProject) => {
            app.reduce(AppAction::CloseModal);
            push_command_message(
                app,
                CommandOutputKind::Error,
                "Session belongs to a different project.",
            );
        }
        Err(err) => {
            app.reduce(AppAction::CloseModal);
            push_command_message(app, CommandOutputKind::Error, &format!("{err:#}"));
        }
    }
}

/// Confirmed `/discard` for a *saved* plan: delete the library record, then clear
/// the canvas. On a delete error the canvas is left intact so the user can retry
/// rather than orphaning the working plan.
async fn submit_plan_discard_confirm(app: &mut AppState, deps: RuntimeActionDeps<'_>) {
    let Some(ModalKind::PlanDiscardConfirm { saved_plan_id, .. }) = app.modal.clone() else {
        return;
    };
    app.reduce(AppAction::CloseModal);
    match deps.storage.delete_saved_plan(saved_plan_id).await {
        Ok(_) => {
            clear_canvas_plan(app, &deps.plan_store).await;
            push_command_message(
                app,
                CommandOutputKind::Status,
                &format!("Discarded plan and deleted saved #{saved_plan_id}."),
            );
        }
        Err(err) => push_command_message(app, CommandOutputKind::Error, &format!("{err:#}")),
    }
}

/// Apply a `/mode` cycle on a posture axis. The reducer has already mutated
/// the picker's local `current` index; this dispatches the resolved label
/// through the shared holder so the next shell spawn / next prompt sees the
/// change. Successful picker-only switches stay out of the transcript; invalid
/// backend states still surface as errors.
async fn apply_mode_picker_axis(
    app: &mut AppState,
    axis: crate::tui::event::ModeAxisId,
    label: &str,
    yolo_mode: &crate::yolo::YoloMode,
    storage: &crate::storage::Storage,
) {
    match axis {
        crate::tui::event::ModeAxisId::Autonomy => {
            let Some(level) = crate::tool::ApprovalLevel::parse(label) else {
                return;
            };
            yolo_mode.set_level(level);
            app.reduce(crate::tui::event::AppAction::SetApprovalLevel(level));
            persist_approval_level(app, storage, level).await;
        }
        crate::tui::event::ModeAxisId::SelfReview => {
            let Some(mode) = crate::self_review::SelfReviewMode::parse(label) else {
                return;
            };
            app.self_review_mode = mode;
        }
        crate::tui::event::ModeAxisId::SandboxConfinement => {
            let Some(sandbox) = app.sandbox.clone() else {
                return;
            };
            let enable = match label {
                "on" => true,
                "off" => false,
                _ => return,
            };
            if enable && !sandbox.backend().is_available() {
                push_command_message(
                    app,
                    crate::tui::event::CommandOutputKind::Error,
                    &crate::commands::sandbox_unavailable_message(),
                );
                return;
            }
            sandbox.set_enabled(enable);
            persist_sandbox_enabled(app, storage, enable).await;
        }
        crate::tui::event::ModeAxisId::SandboxNetwork => {
            let Some(sandbox) = app.sandbox.clone() else {
                return;
            };
            let deny = match label {
                "deny" => true,
                "allow" => false,
                _ => return,
            };
            // Mirror the confinement guard: a network deny on a backend that
            // can't enforce it would store a flag the spawn path ignores, so
            // refuse instead.
            if deny && !sandbox.backend().is_available() {
                push_command_message(
                    app,
                    crate::tui::event::CommandOutputKind::Error,
                    &crate::commands::sandbox_unavailable_message(),
                );
                return;
            }
            sandbox.set_deny_network(deny);
            persist_sandbox_deny_network(app, storage, deny).await;
        }
    }
}

async fn persist_approval_level(
    app: &mut AppState,
    storage: &crate::storage::Storage,
    level: crate::tool::ApprovalLevel,
) {
    if let Err(error) = storage.set_approval_level(level).await {
        push_command_message(
            app,
            crate::tui::event::CommandOutputKind::Error,
            &format!("Failed to save autonomy setting: {error:#}"),
        );
    }
}

async fn persist_sandbox_enabled(
    app: &mut AppState,
    storage: &crate::storage::Storage,
    enabled: bool,
) {
    if let Err(error) = storage.set_sandbox_enabled(enabled).await {
        push_command_message(
            app,
            crate::tui::event::CommandOutputKind::Error,
            &format!("Failed to save sandbox setting: {error:#}"),
        );
    }
}

async fn persist_sandbox_deny_network(
    app: &mut AppState,
    storage: &crate::storage::Storage,
    deny_network: bool,
) {
    if let Err(error) = storage.set_sandbox_deny_network(deny_network).await {
        push_command_message(
            app,
            crate::tui::event::CommandOutputKind::Error,
            &format!("Failed to save sandbox network setting: {error:#}"),
        );
    }
}

/// Cycle the selected `/settings` `Choice` by `delta`, apply it live, then
/// reseed the screen so dependent notes/values refresh. A non-`Choice` row is
/// left alone (its activate opens a picker instead).
async fn cycle_setting(
    app: &mut AppState,
    deps: &RuntimeActionDeps<'_>,
    tasks: &mut TaskController,
    delta: i16,
) {
    let Some((id, label, cursor)) = advance_selected_choice(app, delta) else {
        return;
    };
    apply_setting(app, deps, tasks, id, &label).await;
    reseed_settings(app, deps, cursor).await;
}

/// Advance the selected `Choice` row in place, returning its id + new label +
/// cursor; `None` for a header/action row (nothing to cycle).
fn advance_selected_choice(
    app: &mut AppState,
    delta: i16,
) -> Option<(crate::tui::event::SettingId, String, usize)> {
    let Some(ModalKind::Settings { rows, cursor }) = &mut app.modal else {
        return None;
    };
    let cursor = *cursor;
    let row = rows.get_mut(cursor)?;
    if !matches!(row, crate::tui::event::SettingsRow::Choice { .. }) {
        return None;
    }
    *row = row.clone().cycled(delta);
    let label = rows[cursor].choice_label()?.to_string();
    let id = rows[cursor].id()?;
    Some((id, label, cursor))
}

/// Apply a resolved setting label through its live holder (posture axes via the
/// shared mode-axis path, SMOL via the agent, serenity via TUI state).
async fn apply_setting(
    app: &mut AppState,
    deps: &RuntimeActionDeps<'_>,
    tasks: &mut TaskController,
    id: crate::tui::event::SettingId,
    label: &str,
) {
    use crate::tui::event::{ModeAxisId, SettingId};
    let axis = match id {
        SettingId::Autonomy => Some(ModeAxisId::Autonomy),
        SettingId::SelfReview => Some(ModeAxisId::SelfReview),
        SettingId::SandboxConfinement => Some(ModeAxisId::SandboxConfinement),
        SettingId::SandboxNetwork => Some(ModeAxisId::SandboxNetwork),
        SettingId::Smol
        | SettingId::Serenity
        | SettingId::SupportLog
        | SettingId::BudgetTurns
        | SettingId::BudgetRunTime
        | SettingId::BudgetGenerationTime
        | SettingId::BudgetOutput
        | SettingId::BudgetToolTime
        | SettingId::BudgetSessionTurns
        | SettingId::BudgetSessionOutput
        | SettingId::BudgetSessionTime
        | SettingId::BudgetSessionCost
        | SettingId::CredentialStorage
        | SettingId::Model
        | SettingId::Theme => None,
    };
    if let Some(axis) = axis {
        apply_mode_picker_axis(app, axis, label, &deps.yolo_mode, deps.storage).await;
        // Mirror the /mode picker: sync self-review into the agent while idle.
        if axis == ModeAxisId::SelfReview
            && let Some(mode) = crate::self_review::SelfReviewMode::parse(label)
            && !tasks.is_busy()
        {
            deps.agent.lock().await.set_self_review_mode(mode);
        }
        return;
    }
    if id == SettingId::Smol {
        let Some(preference) = crate::smol::SmolPreference::parse(label) else {
            return;
        };
        if let Err(error) = deps.storage.set_smol_preference(preference).await {
            push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("Failed to save SMOL preference: {error:#}"),
            );
            return;
        }
        let profile = {
            let mut agent = deps.agent.lock().await;
            agent.set_smol_preference(preference);
            agent.smol_profile()
        };
        app.reduce(AppAction::SetSmolMode(profile.enabled));
        return;
    }
    if id == SettingId::Serenity {
        let on = label == "on";
        app.reduce(AppAction::SetSerenityMode(on));
        if let Err(error) = deps.storage.set_serenity_mode(on).await {
            push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("Failed to save serenity preference: {error:#}"),
            );
        }
        return;
    }
    if id == SettingId::SupportLog {
        let on = label == "on";
        // Flip the live gate first so the change takes effect this session;
        // persistence failing leaves the toggle active but unremembered.
        crate::logging::set_support_log_enabled(on);
        app.support_log_enabled = on;
        if let Err(error) = deps.storage.set_support_log_enabled(on).await {
            push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("Failed to save support log preference: {error:#}"),
            );
        }
        return;
    }
    if let Some(budget) = crate::tui::settings::update_run_budget(app.run_budget, id, label) {
        if tasks.is_busy() {
            push_command_message(
                app,
                CommandOutputKind::Error,
                "Run budgets can only be changed while the agent is idle.",
            );
            return;
        }
        match deps.storage.set_run_budget(budget).await {
            Ok(()) => {
                app.run_budget = budget;
                deps.agent.lock().await.set_run_budget(budget);
                tasks.set_max_run_duration(budget.max_run_duration());
                tasks.set_max_session_active_duration(budget.max_session_active_duration());
            }
            Err(error) => push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("Failed to save run budget setting: {error:#}"),
            ),
        }
        return;
    }
    if id == SettingId::CredentialStorage
        && let Some(persistence) = crate::session::CredentialPersistence::parse(label)
    {
        match deps.storage.set_credential_persistence(persistence).await {
            Ok(()) => {
                app.credential_persistence = persistence;
                app.provider_auth_form.credential_persistence = persistence;
            }
            Err(error) => push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("Failed to save credential storage setting: {error:#}"),
            ),
        }
    }
}

/// Rebuild the settings rows from the now-current holders (+ SMOL from the
/// agent) and reopen at the same cursor.
async fn reseed_settings(app: &mut AppState, deps: &RuntimeActionDeps<'_>, cursor: usize) {
    let smol_profile = deps.agent.lock().await.smol_profile();
    let rows = crate::tui::settings::seed_settings_rows(app, smol_profile);
    let cursor = cursor.min(rows.len().saturating_sub(1));
    app.reduce(AppAction::OpenModal(ModalKind::Settings { rows, cursor }));
}

/// Enter on a settings row: open the model/theme picker for an `Action`, or
/// cycle a `Choice` forward.
async fn activate_setting(
    app: &mut AppState,
    deps: &RuntimeActionDeps<'_>,
    tasks: &mut TaskController,
) {
    use crate::tui::event::{SettingId, SettingsRow};
    let selected = match app.modal.as_ref() {
        Some(ModalKind::Settings { rows, cursor }) => rows.get(*cursor).cloned(),
        _ => None,
    };
    match selected {
        Some(SettingsRow::Action {
            id: SettingId::Model,
            ..
        }) => {
            crate::tui::run::open_cached_model_picker(
                app,
                deps.session_store.clone(),
                deps.registry.clone(),
                deps.model_catalog.clone(),
            )
            .await;
        }
        Some(SettingsRow::Action {
            id: SettingId::Theme,
            ..
        }) => {
            crate::tui::run::open_theme_picker(app);
        }
        Some(SettingsRow::Choice { .. }) => {
            cycle_setting(app, deps, tasks, 1).await;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::CommandSandbox;
    use crate::tui::app::AppState;
    use crate::tui::event::{MODE_AXES, ModalKind, ModeAxisId, ModeRow};
    use crate::yolo::YoloMode;

    /// Build the value row for `axis` with `current` set to `idx`. The
    /// resulting picker state matches what `seed_mode_picker_rows` produces
    /// in production.
    fn value_row_for(axis: ModeAxisId, idx: usize) -> ModeRow {
        let def = MODE_AXES
            .iter()
            .flat_map(|a| a.values.iter())
            .find(|v| v.axis == axis)
            .expect("axis not declared in MODE_AXES");
        ModeRow::Value {
            axis,
            key: def.key,
            values: def.values,
            current: idx,
            note: None,
        }
    }

    fn app_with_picker(rows: Vec<ModeRow>) -> AppState {
        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        app.sandbox = Some(CommandSandbox::disabled());
        app.modal = Some(ModalKind::ModePicker { rows, cursor: 0 });
        app
    }

    fn image_clipboard(app: &mut AppState) {
        use crate::copy::ClipboardImage;
        use crate::copy::test_support::fake_clipboard_with_image;
        app.set_clipboard(fake_clipboard_with_image(ClipboardImage {
            width: 2,
            height: 2,
            rgba: vec![255u8; 2 * 2 * 4],
        }));
    }

    fn composer_model_option(model: &str) -> ModelOption {
        let mut option = crate::tui::test_utils::model_option("codex", "Codex", model);
        option.supported_reasoning = vec![
            crate::provider::ReasoningSelection::Low,
            crate::provider::ReasoningSelection::High,
        ];
        option
    }

    #[test]
    fn composer_picker_reset_clears_only_targeted_slot() {
        let mut app = AppState::new("codex", "session".to_string(), ".".to_string(), None);
        let mut state = AgentComposerState::new();
        state.field = AgentComposerField::BackupModel;
        state.model.set_text("codex:primary".to_string());
        state.effort_index = 4;
        state.fallback_model.set_text("codex:backup".to_string());
        state.fallback_effort_index = 2;
        app.pending_composer_state = Some(Box::new(state));
        app.model_picker.target = crate::tui::app::ModelPickerTarget::ComposerBackup;
        app.model_picker.cursor = 0;

        assert!(submit_composer_model_picker(&mut app, &[]));

        let Some(ModalKind::AgentComposer { state }) = app.modal.as_ref() else {
            panic!("composer should reopen after reset");
        };
        assert_eq!(state.selected_model(), Some("codex:primary"));
        assert_eq!(state.selected_effort(), Some("high"));
        assert_eq!(state.selected_fallback_model(), None);
        assert_eq!(state.selected_fallback_effort(), None);
    }

    #[test]
    fn composer_picker_submit_writes_reasoning_only_to_targeted_slot() {
        let mut app = AppState::new("codex", "session".to_string(), ".".to_string(), None);
        let mut state = AgentComposerState::new();
        state.field = AgentComposerField::Model;
        state.model.set_text("codex:primary".to_string());
        state.effort_index = 2;
        state
            .fallback_model
            .set_text("codex:old-backup".to_string());
        state.fallback_effort_index = 4;
        app.pending_composer_state = Some(Box::new(state));
        app.model_picker.target = crate::tui::app::ModelPickerTarget::ComposerBackup;
        app.model_picker.cursor = 1;
        app.model_picker.reasoning_cursor = 2;
        let entries = vec![composer_model_option("new-backup")];

        assert!(submit_composer_model_picker(&mut app, &entries));

        let Some(ModalKind::AgentComposer { state }) = app.modal.as_ref() else {
            panic!("composer should reopen after selection");
        };
        assert_eq!(state.selected_model(), Some("codex:primary"));
        assert_eq!(state.selected_effort(), Some("low"));
        assert_eq!(state.selected_fallback_model(), Some("codex:new-backup"));
        assert_eq!(state.selected_fallback_effort(), Some("high"));
    }

    #[test]
    fn agent_toggle_atomically_updates_custom_definition() {
        let home = tempfile::TempDir::new().unwrap();
        let path = home.path().join("agents/custom.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "---\nname: custom\ndescription: test\nenabled: true\n---\nprompt\n",
        )
        .unwrap();

        apply_custom_agent_enabled_toggle(&path, true).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("enabled: false"));
        assert!(content.ends_with("prompt\n"));
        assert!(
            std::fs::read_dir(path.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("resource"))
        );
    }

    #[test]
    fn builtin_editor_preserves_compiled_identity_and_only_exposes_settings() {
        let spec = crate::tool::builtin_agents()
            .iter()
            .find(|spec| spec.name == "explore")
            .unwrap();
        let settings = crate::subagent::BuiltinSubagentSettings {
            enabled: false,
            primary_model: Some("f".to_string()),
            primary_effort: Some("low".to_string()),
            fallback_model: None,
            fallback_effort: None,
        };
        let state = AgentComposerState::edit_builtin_subagent(
            spec.id,
            spec.description,
            spec.instructions(),
            &settings,
            Some(std::path::PathBuf::from("agents/explore.md")),
        );

        assert!(state.is_builtin_subagent());
        assert_eq!(state.display_name(), "explore");
        assert_eq!(state.prompt.text, spec.instructions());
        assert_eq!(state.builtin_subagent_settings(), Some((spec.id, settings)));
    }

    #[tokio::test]
    async fn builtin_setting_save_commits_database_before_live_registry() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let handle = crate::subagent::SharedBuiltinSubagentSettings::default();
        let id = crate::subagent::BuiltinSubagentId::Research;
        let settings = crate::subagent::BuiltinSubagentSettings {
            enabled: false,
            primary_model: Some("f".to_string()),
            primary_effort: Some("low".to_string()),
            fallback_model: Some("codex:openai/gpt-5.5".to_string()),
            fallback_effort: Some("high".to_string()),
        };

        persist_builtin_subagent_settings(&fixture.storage, &handle, id, settings.clone())
            .await
            .unwrap();

        assert_eq!(handle.get(id), Some(settings.clone()));
        assert_eq!(
            fixture
                .storage
                .load_builtin_subagent_settings()
                .await
                .unwrap()
                .get(&id),
            Some(&settings)
        );
        assert!(
            !fixture
                .storage
                .home_dir()
                .join("agents/research.md")
                .exists(),
            "database-backed built-in settings must not materialize Markdown"
        );

        let invalid_id = crate::subagent::BuiltinSubagentId::Review;
        let invalid = crate::subagent::BuiltinSubagentSettings {
            primary_effort: Some("high".to_string()),
            ..crate::subagent::BuiltinSubagentSettings::default()
        };
        assert!(
            persist_builtin_subagent_settings(&fixture.storage, &handle, invalid_id, invalid)
                .await
                .is_err()
        );
        assert_eq!(
            handle.get(invalid_id),
            None,
            "failed disk write must not change the live registry"
        );
    }

    #[test]
    fn effective_builtin_settings_migrate_legacy_model_but_database_wins() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("explore.md");
        std::fs::write(
            &path,
            "---\nname: explore\ndescription: legacy\nenabled: false\nmodel: legacy-model\neffort: low\n---\nCUSTOM PROMPT",
        )
        .unwrap();
        let id = crate::subagent::BuiltinSubagentId::Explore;
        let handle = crate::subagent::SharedBuiltinSubagentSettings::default();

        let legacy = effective_builtin_subagent_settings(&handle, id, Some(&path)).unwrap();
        assert!(!legacy.enabled);
        assert_eq!(legacy.primary_model.as_deref(), Some("legacy-model"));
        assert_eq!(legacy.primary_effort.as_deref(), Some("low"));

        let persisted = crate::subagent::BuiltinSubagentSettings {
            primary_model: Some("persisted-model".to_string()),
            ..crate::subagent::BuiltinSubagentSettings::default()
        };
        handle.upsert(id, persisted.clone());
        assert_eq!(
            effective_builtin_subagent_settings(&handle, id, Some(&path)).unwrap(),
            persisted
        );
    }

    #[test]
    fn custom_edit_round_trip_preserves_disabled_state() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("disabled.md");
        std::fs::write(
            &path,
            "---\nname: disabled\ndescription: test\nenabled: false\nsurface: [subagent]\n---\nprompt",
        )
        .unwrap();

        let state = load_composer_edit_state(&path, true).unwrap();
        let parsed = crate::resource::frontmatter::parse_frontmatter::<
            crate::resource::frontmatter::AgentFrontmatter,
        >(&state.render_agent_md())
        .unwrap();

        assert!(!parsed.frontmatter.enabled);
    }

    #[test]
    fn agent_reload_falls_back_when_active_persona_is_no_longer_selectable() {
        let dir = tempfile::TempDir::new().unwrap();
        let agents_dir = dir.path().join(".bonsai/agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("helper.md"),
            "---\nname: helper\ndescription: test\nenabled: false\nsurface: [mode]\n---\nprompt",
        )
        .unwrap();
        let registry =
            crate::resource::agent::AgentRegistry::load_from(dir.path(), &dir.path().join("home"));
        let mut app = AppState::new("codex", "model".to_string(), ".".to_string(), None);
        app.custom_agents = crate::resource::agent::shared_registry(registry);
        app.active_persona = crate::agent::ActivePersona::Custom("helper".to_string());

        normalize_active_persona_after_agent_reload(&mut app);

        assert_eq!(
            app.active_persona,
            crate::agent::ActivePersona::Builtin(crate::agent::AgentMode::Coding)
        );
    }

    #[test]
    fn paste_from_clipboard_makes_image_chip_for_vision_model() {
        use crate::tui::app::ChipPayload;
        use crate::tui::event::Focus;

        // Codex declares vision support at the provider level.
        let mut app = AppState::new("codex", "gpt-5.5".to_string(), ".".to_string(), None);
        app.focus = Focus::Input;
        image_clipboard(&mut app);
        let registry = ProviderRegistry::default_registry();

        paste_from_clipboard(&mut app, &registry, None);

        assert_eq!(app.composer.chips.len(), 1);
        assert!(matches!(
            app.composer.chips[0].payload,
            ChipPayload::Image { .. }
        ));
    }

    #[test]
    fn paste_from_clipboard_refuses_image_for_non_vision_model() {
        use crate::tui::event::Focus;

        // OpenCode has no vision flag and no catalog entry here.
        let mut app = AppState::new("opencode", "qwen3.7-max".to_string(), ".".to_string(), None);
        app.focus = Focus::Input;
        image_clipboard(&mut app);
        let registry = ProviderRegistry::default_registry();

        paste_from_clipboard(&mut app, &registry, None);

        assert!(app.composer.chips.is_empty());
    }

    #[tokio::test]
    async fn cycling_autonomy_writes_through_yolo_and_mirrors_into_view() {
        let mut app = app_with_picker(vec![value_row_for(ModeAxisId::Autonomy, 0)]);
        let yolo = YoloMode::with_level(crate::tool::ApprovalLevel::Balanced);
        let storage = crate::storage::test_utils::TestStorage::new().await;
        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::Autonomy,
            "ask",
            &yolo,
            &storage.storage,
        )
        .await;
        assert_eq!(yolo.level(), crate::tool::ApprovalLevel::Ask);
        assert_eq!(app.approval_level, crate::tool::ApprovalLevel::Ask);
        assert_eq!(
            storage.storage.approval_level().await.unwrap(),
            Some(crate::tool::ApprovalLevel::Ask)
        );
    }

    #[tokio::test]
    async fn cycling_self_review_mirrors_into_view() {
        let mut app = app_with_picker(vec![value_row_for(ModeAxisId::SelfReview, 0)]);
        let yolo = YoloMode::default();
        let storage = crate::storage::test_utils::TestStorage::new().await;
        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::SelfReview,
            "ask",
            &yolo,
            &storage.storage,
        )
        .await;
        assert_eq!(
            app.self_review_mode,
            crate::self_review::SelfReviewMode::Ask
        );
    }

    #[tokio::test]
    async fn successful_mode_picker_cycles_do_not_append_status_to_transcript() {
        let mut app = app_with_picker(vec![
            value_row_for(ModeAxisId::Autonomy, 0),
            value_row_for(ModeAxisId::SelfReview, 0),
            value_row_for(ModeAxisId::SandboxConfinement, 0),
            value_row_for(ModeAxisId::SandboxNetwork, 0),
        ]);
        let yolo = YoloMode::with_level(crate::tool::ApprovalLevel::Balanced);
        let storage = crate::storage::test_utils::TestStorage::new().await;

        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::Autonomy,
            "yolo",
            &yolo,
            &storage.storage,
        )
        .await;
        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::SelfReview,
            "on",
            &yolo,
            &storage.storage,
        )
        .await;
        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::SandboxConfinement,
            "off",
            &yolo,
            &storage.storage,
        )
        .await;
        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::SandboxNetwork,
            "allow",
            &yolo,
            &storage.storage,
        )
        .await;

        assert!(app.transcript.is_empty());
    }

    #[test]
    fn provider_builtin_detection_covers_catalog_builtins() {
        let codex = "codex".parse().unwrap();
        let openai_compatible = "openai-compatible".parse().unwrap();
        let custom = "local-example".parse().unwrap();

        assert!(provider_is_builtin(&codex));
        assert!(provider_is_builtin(&openai_compatible));
        assert!(!provider_is_builtin(&custom));
    }

    #[tokio::test]
    async fn cycling_confinement_calls_sandbox_set_enabled() {
        let mut app = app_with_picker(vec![value_row_for(ModeAxisId::SandboxConfinement, 0)]);
        let sandbox = CommandSandbox::disabled();
        app.sandbox = Some(sandbox.clone());
        let yolo = YoloMode::default();
        let storage = crate::storage::test_utils::TestStorage::new().await;
        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::SandboxConfinement,
            "on",
            &yolo,
            &storage.storage,
        )
        .await;
        // Disabled() sandbox uses Unavailable backend, so the runtime path
        // refuses with an error -- it does NOT enable confinement.
        assert!(
            !sandbox.is_enabled(),
            "Unavailable backend should refuse enable"
        );
        // Available backend flips the flag:
        let backend = crate::sandbox::detect_backend();
        let tmp = tempfile::tempdir().unwrap();
        let available = CommandSandbox::new(backend, tmp.path());
        app.sandbox = Some(available.clone());
        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::SandboxConfinement,
            "on",
            &yolo,
            &storage.storage,
        )
        .await;
        if available.backend().is_available() {
            assert!(available.is_enabled());
            assert_eq!(storage.storage.sandbox_enabled().await.unwrap(), Some(true));
        }
    }

    #[tokio::test]
    async fn cycling_network_calls_sandbox_set_deny_network() {
        let mut app = app_with_picker(vec![value_row_for(ModeAxisId::SandboxNetwork, 0)]);
        let yolo = YoloMode::default();
        // Unavailable backend refuses the deny (it would be unenforced) — same
        // guard as confinement.
        let unavailable = CommandSandbox::disabled();
        app.sandbox = Some(unavailable.clone());
        let storage = crate::storage::test_utils::TestStorage::new().await;
        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::SandboxNetwork,
            "deny",
            &yolo,
            &storage.storage,
        )
        .await;
        assert!(
            !unavailable.deny_network(),
            "unavailable backend should refuse network deny"
        );

        // Available backend flips the flag (and `allow` always clears it).
        let backend = crate::sandbox::detect_backend();
        let tmp = tempfile::tempdir().unwrap();
        let available = CommandSandbox::new(backend, tmp.path());
        app.sandbox = Some(available.clone());
        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::SandboxNetwork,
            "deny",
            &yolo,
            &storage.storage,
        )
        .await;
        if available.backend().is_available() {
            assert!(available.deny_network());
            assert_eq!(
                storage.storage.sandbox_deny_network().await.unwrap(),
                Some(true)
            );
            apply_mode_picker_axis(
                &mut app,
                ModeAxisId::SandboxNetwork,
                "allow",
                &yolo,
                &storage.storage,
            )
            .await;
            assert!(!available.deny_network());
            assert_eq!(
                storage.storage.sandbox_deny_network().await.unwrap(),
                Some(false)
            );
        }
    }

    /// Regression guard: the runtime handler must read the focused row's
    /// *current* label, not the axis default. A pure ModeAxisId dispatch
    /// without consulting the row would silently re-write the holder to
    /// the wrong value when the user re-opens the picker mid-cycle.
    #[tokio::test]
    async fn cycle_handler_reads_label_from_focused_row_not_axis_default() {
        let mut app = app_with_picker(vec![value_row_for(ModeAxisId::Autonomy, 4)]);
        let yolo = YoloMode::with_level(crate::tool::ApprovalLevel::Ask);
        let storage = crate::storage::test_utils::TestStorage::new().await;
        // Index 4 = Yolo in our cycle order.
        apply_mode_picker_axis(
            &mut app,
            ModeAxisId::Autonomy,
            "yolo",
            &yolo,
            &storage.storage,
        )
        .await;
        assert_eq!(yolo.level(), crate::tool::ApprovalLevel::Yolo);
    }

    /// Drive one full sandbox-escape decision through the real
    /// [`InteractionService`]: a tool-side requester blocks on `request`,
    /// the UI answers via `respond_to_sandbox_escalation`, and the decision
    /// arrives back at the requester.
    async fn deliver_escalation_decision(
        decision: EscalationDecision,
    ) -> (AppState, InteractionOutcome) {
        let (service, mut rx) = InteractionService::new();
        let service = Arc::new(service);
        let requester = {
            let service = service.clone();
            tokio::spawn(async move {
                match std::panic::AssertUnwindSafe(async {
                    service
                        .request(|request_id| {
                            crate::interaction::InteractionRequest::SandboxEscalation {
                                request_id,
                                command: "rm -rf target".to_string(),
                                origin: None,
                            }
                        })
                        .await
                })
                .catch_unwind()
                .await
                {
                    Ok(result) => result,
                    Err(panic) => {
                        tracing::error!(?panic, "sandbox escalation requester panicked");
                        Err(crate::interaction::InteractionStatus::Cancelled)
                    }
                }
            })
        };
        let request = rx.recv().await.expect("escalation request reaches the UI");
        let crate::interaction::InteractionRequest::SandboxEscalation {
            request_id,
            command,
            origin,
        } = request
        else {
            panic!("unexpected interaction request kind");
        };

        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        app.modal = Some(ModalKind::SandboxEscalationPrompt {
            request_id,
            command,
            origin,
        });
        app.focus = Focus::Modal;
        respond_to_sandbox_escalation(&mut app, service, decision);

        let outcome = requester
            .await
            .expect("requester task")
            .expect("decision must be delivered, not cancelled");
        (app, outcome)
    }

    fn transcript_audit_banner(app: &AppState) -> Option<String> {
        app.transcript.iter().find_map(|item| match item {
            crate::tui::transcript::TranscriptItem::CommandOutput { text, .. }
                if text.contains("Sandbox escape approved") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
    }

    /// The audit contract of the sandbox-escape prompt: every approval is
    /// durably recorded in the transcript with its scope and the exact
    /// command, the modal closes, and the decision reaches the waiting tool.
    #[tokio::test]
    async fn sandbox_escape_approval_audits_scope_and_delivers_decision() {
        for (decision, scope) in [
            (EscalationDecision::AllowOnce, "once"),
            (EscalationDecision::AllowForSession, "for session"),
        ] {
            let expected = std::mem::discriminant(&decision);
            let (app, outcome) = deliver_escalation_decision(decision).await;

            let banner = transcript_audit_banner(&app)
                .unwrap_or_else(|| panic!("{scope}: approval must leave an audit banner"));
            assert_eq!(
                banner,
                format!("⚠ Sandbox escape approved ({scope}): rm -rf target")
            );
            assert!(app.modal.is_none(), "{scope}: prompt must close");
            assert_eq!(app.focus, Focus::Input, "{scope}: focus returns to input");
            assert!(
                matches!(
                    outcome,
                    InteractionOutcome::SandboxEscalation(delivered)
                        if std::mem::discriminant(&delivered) == expected
                ),
                "{scope}: tool must receive the decision"
            );
        }
    }

    /// Drive one multi-select question through the real service with the given
    /// checkbox states and return the delivered answer.
    async fn deliver_multi_select(selected: Vec<bool>) -> InteractionOutcome {
        let (service, mut rx) = InteractionService::new();
        let service = Arc::new(service);
        let requester = {
            let service = service.clone();
            tokio::spawn(async move {
                match std::panic::AssertUnwindSafe(async {
                    service
                        .request(
                            |request_id| crate::interaction::InteractionRequest::Question {
                                request_id,
                                prompt: "pick".to_string(),
                                header: None,
                                options: vec![
                                    crate::interaction::QuestionOption::new("a", ""),
                                    crate::interaction::QuestionOption::new("b", ""),
                                ],
                                multiple: true,
                                origin: None,
                            },
                        )
                        .await
                })
                .catch_unwind()
                .await
                {
                    Ok(result) => result,
                    Err(panic) => {
                        tracing::error!(?panic, "question requester panicked");
                        Err(crate::interaction::InteractionStatus::Cancelled)
                    }
                }
            })
        };
        let request = rx.recv().await.expect("question reaches the UI");
        let crate::interaction::InteractionRequest::Question {
            request_id,
            prompt,
            header,
            options,
            multiple,
            origin,
        } = request
        else {
            panic!("unexpected request kind");
        };
        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        app.modal = Some(ModalKind::QuestionPrompt {
            request_id,
            prompt,
            header,
            options,
            multiple,
            origin,
            cursor: 0,
            selected,
        });
        respond_to_question_submit(&mut app, service);
        requester
            .await
            .expect("requester task")
            .expect("answer must be delivered")
    }

    /// A multi-select submit is faithful: checked boxes arrive as indices and
    /// an all-deselected submit arrives as the EMPTY set — never silently
    /// substituted with the cursor row.
    #[tokio::test]
    async fn multi_select_submit_delivers_faithful_selection() {
        let outcome = deliver_multi_select(vec![false, true]).await;
        assert!(matches!(
            outcome,
            InteractionOutcome::Question(Some(crate::interaction::InteractionAnswer::Choices(
                ref picks
            ))) if picks == &vec![1]
        ));

        let outcome = deliver_multi_select(vec![false, false]).await;
        assert!(
            matches!(
                outcome,
                InteractionOutcome::Question(Some(
                    crate::interaction::InteractionAnswer::Choices(ref picks)
                )) if picks.is_empty()
            ),
            "deselecting everything must deliver an empty selection"
        );
    }

    /// Denials are deliberately not audited (nothing escaped), but must still
    /// close the prompt and deliver the refusal so the command aborts instead
    /// of hanging on the interaction.
    #[tokio::test]
    async fn sandbox_escape_denial_delivers_without_audit_banner() {
        let (app, outcome) = deliver_escalation_decision(EscalationDecision::Deny).await;

        assert_eq!(
            transcript_audit_banner(&app),
            None,
            "a denial must not be recorded as an approval"
        );
        assert!(app.modal.is_none());
        assert_eq!(app.focus, Focus::Input);
        assert!(matches!(
            outcome,
            InteractionOutcome::SandboxEscalation(EscalationDecision::Deny)
        ));
    }
}
