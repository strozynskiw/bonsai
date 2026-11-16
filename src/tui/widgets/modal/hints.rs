//! The one place a modal footer's key hints are written.
//!
//! Every pair lives here as a per-kind constant or selector, and the renderers draw
//! `footer_hint_line(<that constant>)` instead of listing pairs of their own. The
//! [`footer_hints`] index maps the modal on screen to its constant, so the two
//! consistency tests can compare what a popup advertises against the keymap that
//! actually binds those keys.

use super::*;
use crate::tui::event::{DetailModal, ManagerModal, PickerModal};

/// Yes/no popups: the confirm pair every one of them draws.
pub(super) const YES_NO: &[(&str, &str)] = &[("Enter/Y", "confirm"), ("Esc/N", "cancel")];

/// The MCP manager's keys, shared by the renderer and the height it measures.
pub(super) const MCP_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "servers"),
    ("Space", "toggle"),
    ("r", "reload"),
    ("a", "authorize"),
    ("PgUp/PgDn", "methods"),
    ("Esc", "close"),
];
/// The onboarding wizard's keys.
pub(super) const ONBOARDING_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "choose"),
    ("Enter", "continue"),
    ("Esc", "later"),
];
/// The `/keys` reading surface: mouse-selectable text.
pub(super) const HELP_KEYS_HINTS: &[(&str, &str)] = &[("d-click", "copy word"), ("Esc", "close")];
/// The authorize picker's keys.
pub(super) const AUTHORIZE_PICKER_HINTS: &[(&str, &str)] = &[
    ("Type", "search"),
    ("Up/Down", "move"),
    ("Enter", "authorize"),
    ("Esc", "cancel"),
];
pub(super) const UNAUTHORIZE_PICKER_HINTS: &[(&str, &str)] = &[
    ("Type", "search"),
    ("Up/Down", "move"),
    ("Enter", "unauthorize"),
    ("Esc", "cancel"),
];

/// The memory wizard's keys for the step on screen.
pub(super) fn memory_wizard_hints(
    step: crate::tui::memory_manager::MemoryWizardStep,
) -> &'static [(&'static str, &'static str)] {
    use crate::tui::memory_manager::MemoryWizardStep;
    match step {
        MemoryWizardStep::Details => MEMORY_WIZARD_DETAILS,
        MemoryWizardStep::Body => MEMORY_WIZARD_BODY,
        MemoryWizardStep::Review => MEMORY_WIZARD_REVIEW,
    }
}

/// The command board's keys. `Tab` is deliberately absent: the board is a reading
/// surface and its mapper ignores it, which the two-way hint test enforces.
pub(super) const COMMAND_HELP_HINTS: &[(&str, &str)] = &[("Enter", "run"), ("Esc", "close")];
/// The review-scope choice's keys.
pub(super) const REVIEW_SCOPE_HINTS: &[(&str, &str)] =
    &[("Up/Down", "move"), ("Enter", "review"), ("Esc", "cancel")];

/// Approval prompts that may be granted for the whole project.
const APPROVAL_PROJECT: &[(&str, &str)] = &[
    ("A/Enter", "once"),
    ("S", "session"),
    ("P", "project"),
    ("D/Esc", "deny"),
    ("N", "never"),
];
/// Approvals that cannot be scoped to the project (sandbox escalation).
const APPROVAL_SESSION: &[(&str, &str)] =
    &[("A/Enter", "once"), ("S", "session"), ("D/Esc", "deny")];

/// The approval prompt's keys, with the project/never tiers only where the popup can
/// actually grant them.
pub(super) fn approval_hints(project: bool) -> &'static [(&'static str, &'static str)] {
    if project {
        APPROVAL_PROJECT
    } else {
        APPROVAL_SESSION
    }
}

const QUESTION_SINGLE: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("Enter", "select"),
    ("Tab", "hide · /questions to reopen"),
    ("Esc", "cancel"),
];
const QUESTION_MULTI: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("Space", "toggle"),
    ("Enter", "select"),
    ("Tab", "hide · /questions to reopen"),
    ("Esc", "cancel"),
];

/// The question prompt's keys; only a multi-select question can toggle.
pub(super) fn question_hints(multiple: bool) -> &'static [(&'static str, &'static str)] {
    if multiple {
        QUESTION_MULTI
    } else {
        QUESTION_SINGLE
    }
}

pub(super) const API_KEY_PLAIN: &[(&str, &str)] = &[
    ("Ctrl+P", "storage"),
    ("Enter", "submit"),
    ("Esc", "cancel"),
];
pub(super) const API_KEY_ORIGIN: &[(&str, &str)] = &[
    ("Tab", "field"),
    ("←/→", "origin"),
    ("Ctrl+P", "storage"),
    ("Enter", "submit"),
    ("Esc", "cancel"),
];
pub(super) const API_KEY_ENDPOINT: &[(&str, &str)] = &[
    ("Tab/Shift+Tab", "field"),
    ("Ctrl+P", "storage"),
    ("Enter", "submit"),
    ("Esc", "cancel"),
];

/// Reading surfaces: scroll and dismiss.
pub(super) const SCROLL_CLOSE: &[(&str, &str)] = &[("Up/Down", "scroll"), ("Esc", "close")];
/// The usage dashboard: tabs, scroll, dismiss.
pub(super) const USAGE_HINTS: &[(&str, &str)] = &[
    ("Tab/1-6", "switch tab"),
    ("Up/Down", "scroll"),
    ("Esc", "close"),
];
/// `/sessions`, `/plans` and the pickers whose renderer draws without an
/// `AppState`; the table below shares these so the pairs still live in one file.
pub(super) const SESSION_PICKER_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("PgUp/PgDn", "page"),
    ("Enter", "resume"),
    ("Del", "delete"),
    ("Esc", "close"),
];
pub(super) const PLAN_PICKER_HINTS: &[(&str, &str)] = &[
    ("Type", "search"),
    ("Up/Down", "move"),
    ("Enter", "open"),
    ("Del", "delete"),
    ("Esc", "close"),
];
pub(super) const PLAN_OPEN_CHOICE_HINTS: &[(&str, &str)] = &[("Enter", "open"), ("Esc", "cancel")];
pub(super) const START_PLAN_CHOICE_HINTS: &[(&str, &str)] =
    &[("Enter", "start"), ("Esc", "cancel")];
pub(super) const BUDGET_WARNING_HINTS: &[(&str, &str)] = &[("Enter", "choose"), ("Esc", "go back")];
pub(super) const THEME_HINTS: &[(&str, &str)] =
    &[("Up/Down", "move"), ("Enter", "save"), ("Esc", "cancel")];
pub(super) const MODE_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("Left/Right", "cycle"),
    ("Enter/Esc", "close"),
];
pub(super) const BUSY_HINTS: &[(&str, &str)] =
    &[("Up/Down", "move"), ("Enter", "select"), ("Esc", "close")];
pub(super) const DOCTOR_HINTS: &[(&str, &str)] = &[("Up/Down", "move"), ("Enter/Esc", "close")];
pub(super) const REFRESH_HINTS: &[(&str, &str)] = &[("Up/Down", "move"), ("Esc/Enter", "close")];
pub(super) const PEERS_HINTS: &[(&str, &str)] = &[("Up/Down", "move"), ("Esc", "close")];
pub(super) const TASK_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("PgUp/PgDn", "page"),
    ("Del", "remove"),
    ("Esc", "close"),
];
pub(super) const EPISODE_HINTS: &[(&str, &str)] =
    &[("Up/Down", "move"), ("PgUp/PgDn", "page"), ("Esc", "close")];
pub(super) const SKILL_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("PgUp/PgDn", "page"),
    ("Space", "enable/disable"),
    ("Enter/l", "load/unload"),
    ("Esc", "close"),
];
pub(super) const PERMISSIONS_SEARCHING_HINTS: &[(&str, &str)] =
    &[("Esc/Enter", "done"), ("Type", "filter")];
pub(super) const PERMISSIONS_HINTS: &[(&str, &str)] =
    &[("/", "search"), ("d", "delete"), ("Esc", "close")];
pub(super) const MEMORY_MANAGER_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("PgUp/PgDn", "page detail"),
    ("Space", "enable/disable"),
    ("a", "add"),
    ("e/Enter", "edit"),
    ("d/Delete", "delete"),
    ("Esc", "close"),
];
pub(super) const PROVIDER_SEARCHING_HINTS: &[(&str, &str)] =
    &[("Enter", "detail"), ("Esc", "done"), ("Type", "filter")];
/// The provider list's management shortcuts. `d` is contextual (remove a custom
/// provider, disable or re-enable a built-in), so its label stays a verb.
pub(super) const PROVIDER_MANAGER_HINTS: &[(&str, &str)] = &[
    ("/", "search"),
    ("a", "add"),
    ("e", "edit"),
    ("d", "manage"),
    ("Esc", "close"),
];
pub(super) const PROVIDER_FILTERED_HINTS: &[(&str, &str)] = &[
    ("/", "search"),
    ("a", "add"),
    ("e", "edit"),
    ("d", "manage"),
    ("Esc", "clear"),
];
pub(super) const PROVIDER_DETAIL_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "scroll"),
    ("PgUp/PgDn", "page"),
    ("d-click", "copy word"),
    ("Esc", "back"),
];
pub(super) const SUBTASK_LIST_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("PgUp/PgDn", "page"),
    ("Tab/Right", "detail"),
    ("m", "model"),
    ("d", "default"),
    ("Esc", "close"),
];
pub(super) const SUBTASK_DETAIL_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "scroll"),
    ("PgUp/PgDn", "page"),
    ("Tab/Left", "list"),
    ("Esc", "close"),
];
pub(super) const SANDBOX_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("Left/Right", "toggle"),
    ("Esc", "close"),
];
pub(super) const AGENT_BROWSER_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("Enter/e", "edit"),
    ("Space/t", "enable/disable"),
    ("a", "add"),
    ("d/Delete", "delete"),
    ("Esc", "close"),
];
pub(super) const SETTINGS_HINTS: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("Left/Right", "cycle"),
    ("Space/Enter", "change"),
    ("Esc", "close"),
];
pub(super) const PERMISSIONS_FILTERED_HINTS: &[(&str, &str)] =
    &[("/", "search"), ("d", "delete"), ("Esc", "clear")];
/// Reading surfaces whose text is mouse-selectable.
pub(super) const SCROLL_COPY: &[(&str, &str)] = &[
    ("Up/Down", "scroll"),
    ("d-click", "copy word"),
    ("Esc", "close"),
];
/// The same, plus the diff shortcut a tool card offers when it has a diff.
pub(super) const SCROLL_DIFF_COPY: &[(&str, &str)] = &[
    ("Up/Down", "scroll"),
    ("d", "diff"),
    ("d-click", "copy word"),
    ("Esc", "close"),
];

pub(super) const MODEL_PICKER_CHOOSE: &[(&str, &str)] = &[
    ("Left/Right/Tab", "pane"),
    ("Up/Down", "move"),
    ("Enter", "select"),
    // A letter types into the filter here, so say so; the assignment cue takes its
    // place in the Reasoning pane, where a letter really does bind a shortcut.
    ("letter", "filter"),
    ("Esc", "cancel"),
];
/// The same, while the Reasoning pane owns the bare-letter assignment.
const MODEL_PICKER_ASSIGN: &[(&str, &str)] = &[
    ("Left/Right/Tab", "pane"),
    ("Up/Down", "move"),
    ("Enter", "select"),
    ("letter", "assign shortcut"),
    ("Esc", "cancel"),
];

const MEMORY_WIZARD_DETAILS: &[(&str, &str)] = &[
    ("Tab", "body"),
    ("Up/Down", "field"),
    ("Space", "cycle"),
    ("Enter", "body"),
    ("Esc", "cancel"),
];
const MEMORY_WIZARD_BODY: &[(&str, &str)] = &[
    ("Tab", "review"),
    ("Enter", "newline"),
    ("Shift+Tab", "details"),
    ("Esc", "cancel"),
];
const MEMORY_WIZARD_REVIEW: &[(&str, &str)] =
    &[("Enter", "save"), ("Shift+Tab", "body"), ("Esc", "cancel")];

const AGENT_COMPOSER_REVIEW: &[(&str, &str)] =
    &[("Enter", "save"), ("Shift+Tab", "back"), ("Esc", "cancel")];

/// The hint rows every modal footer ends with, keyed by the modal on screen.
///
/// The match is exhaustive over `ModalKind`'s payload, so a new popup cannot ship
/// without deciding what its footer says.
pub(super) fn footer_hints(app: &AppState) -> &'static [(&'static str, &'static str)] {
    let Some(kind) = app.modal.as_ref() else {
        return &[];
    };
    match kind {
        ModalKind::Wizard(crate::tui::event::WizardModal::Onboarding { .. }) => ONBOARDING_HINTS,
        ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard { state }) => {
            local_model_step_hints(state.step)
        }
        ModalKind::Wizard(crate::tui::event::WizardModal::MemoryAddWizard { state }) => {
            memory_wizard_hints(state.step)
        }
        ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer { state }) => {
            agent_composer_hints(state)
        }
        ModalKind::Detail(DetailModal::PermissionPrompt { .. })
        | ModalKind::Detail(DetailModal::WebDomainPrompt { .. })
        | ModalKind::Detail(DetailModal::ExtensionToolPrompt { .. })
        | ModalKind::Detail(DetailModal::HookTrustPrompt { .. }) => approval_hints(true),
        ModalKind::Detail(DetailModal::SandboxEscalationPrompt { .. }) => approval_hints(false),
        ModalKind::Detail(DetailModal::QuestionPrompt { multiple, .. }) => {
            question_hints(*multiple)
        }
        ModalKind::Detail(DetailModal::ApiKeyPrompt { provider_id, .. }) => {
            api_key_hints(app, provider_id)
        }
        ModalKind::Detail(DetailModal::Help) => HELP_KEYS_HINTS,
        ModalKind::Detail(DetailModal::CommandHelp) => COMMAND_HELP_HINTS,
        ModalKind::Detail(DetailModal::PerfReport { .. }) => SCROLL_CLOSE,
        ModalKind::Detail(DetailModal::ToolDetail { tool_id }) => {
            if app
                .tool_activity(tool_id)
                .is_some_and(|activity| activity.diff.is_some())
            {
                SCROLL_DIFF_COPY
            } else {
                SCROLL_COPY
            }
        }
        ModalKind::Detail(DetailModal::BlockDetail { .. })
        | ModalKind::Detail(DetailModal::DiffPreview { .. }) => SCROLL_COPY,
        ModalKind::Detail(DetailModal::PlanFindingDetail { .. }) => {
            &[("←/→", "finding"), ("Up/Down", "scroll"), ("Esc", "close")]
        }
        ModalKind::Detail(DetailModal::Context(_)) => context_hints(app),
        ModalKind::Detail(DetailModal::Refresh { .. }) => REFRESH_HINTS,
        ModalKind::Detail(DetailModal::Episodes { .. }) => EPISODE_HINTS,
        ModalKind::Detail(DetailModal::UsageDashboard { .. }) => USAGE_HINTS,
        ModalKind::Detail(DetailModal::BusyCommand { .. }) => BUSY_HINTS,
        ModalKind::Picker(PickerModal::ModelPicker { .. }) => {
            model_picker_hints(app.model_picker.active_pane)
        }
        ModalKind::Picker(PickerModal::SessionPicker { .. }) => SESSION_PICKER_HINTS,
        ModalKind::Picker(PickerModal::PlanPicker { .. }) => PLAN_PICKER_HINTS,
        ModalKind::Picker(PickerModal::PlanOpenChoice { .. }) => PLAN_OPEN_CHOICE_HINTS,
        ModalKind::Picker(PickerModal::StartPlanChoice { .. }) => START_PLAN_CHOICE_HINTS,
        ModalKind::Picker(PickerModal::BudgetWarning { .. }) => BUDGET_WARNING_HINTS,
        ModalKind::Picker(PickerModal::ReviewScopePicker { .. }) => REVIEW_SCOPE_HINTS,
        ModalKind::Picker(PickerModal::AuthorizeProviderPicker { .. }) => AUTHORIZE_PICKER_HINTS,
        ModalKind::Picker(PickerModal::UnauthorizeProviderPicker { .. }) => {
            UNAUTHORIZE_PICKER_HINTS
        }
        ModalKind::Picker(PickerModal::ThemePicker { .. }) => THEME_HINTS,
        ModalKind::Picker(PickerModal::ModePicker { .. }) => MODE_HINTS,
        ModalKind::Manager(ManagerModal::AgentBrowser { .. }) => AGENT_BROWSER_HINTS,
        ModalKind::Manager(ManagerModal::ProviderManager {
            filter, searching, ..
        }) => provider_manager_hints(*searching, filter),
        ModalKind::Manager(ManagerModal::PermissionsManager {
            filter, searching, ..
        }) => permissions_hints(*searching, filter),
        ModalKind::Manager(ManagerModal::ProviderDetail { .. }) => PROVIDER_DETAIL_HINTS,
        ModalKind::Manager(ManagerModal::SkillManager { .. }) => SKILL_HINTS,
        ModalKind::Manager(ManagerModal::MemoryManager { .. }) => MEMORY_MANAGER_HINTS,
        ModalKind::Manager(ManagerModal::Settings { .. }) => SETTINGS_HINTS,
        ModalKind::Manager(ManagerModal::McpServers { .. }) => MCP_HINTS,
        ModalKind::Manager(ManagerModal::SandboxStatus { .. }) => SANDBOX_HINTS,
        ModalKind::Manager(ManagerModal::Doctor { .. }) => DOCTOR_HINTS,
        ModalKind::Manager(ManagerModal::TaskList { .. }) => TASK_HINTS,
        ModalKind::Manager(ManagerModal::PeerList { .. }) => PEERS_HINTS,
        ModalKind::Manager(ManagerModal::SubtaskList { pane, .. }) => subtask_hints(*pane),
        ModalKind::Confirm(crate::tui::event::ConfirmModal::Unauthorize { .. })
        | ModalKind::Confirm(crate::tui::event::ConfirmModal::PlanDelete { .. })
        | ModalKind::Confirm(crate::tui::event::ConfirmModal::SessionDelete { .. })
        | ModalKind::Confirm(crate::tui::event::ConfirmModal::PlanDiscard { .. })
        | ModalKind::Confirm(crate::tui::event::ConfirmModal::ProviderRemove { .. })
        | ModalKind::Confirm(crate::tui::event::ConfirmModal::AgentDelete { .. }) => YES_NO,
    }
}

/// The local-model wizard keeps its submit verb step-specific on the first hint row.
const LOCAL_MODEL_BASE_TEST: &[(&str, &str)] = &[
    ("Enter", "test connection / load models"),
    ("Esc", "cancel"),
    ("Left", "back"),
];
const LOCAL_MODEL_BASE_METADATA: &[(&str, &str)] =
    &[("Enter", "metadata"), ("Esc", "cancel"), ("Left", "back")];
const LOCAL_MODEL_BASE_REVIEW: &[(&str, &str)] =
    &[("Enter", "write"), ("Esc", "cancel"), ("Left", "back")];

const LOCAL_MODEL_SETUP: &[(&str, &str)] = &[
    ("Tab/Up/Down", "field"),
    ("←/→", "cursor / option"),
    ("Home/End", "text start/end"),
];
const LOCAL_MODEL_SELECT: &[(&str, &str)] = &[
    ("Up/Down", "move"),
    ("Space", "select"),
    ("Type", "add manual model"),
];
const LOCAL_MODEL_METADATA: &[(&str, &str)] = &[
    ("Up/Down", "model"),
    ("Tab", "field"),
    ("Space", "toggle tools"),
];
const LOCAL_MODEL_REVIEW: &[(&str, &str)] = &[("Review", "only")];

/// The first (step-independent) hint row of the local-model wizard.
pub(super) fn local_model_base_hints(
    step: crate::tui::local_model_wizard::LocalModelWizardStep,
) -> &'static [(&'static str, &'static str)] {
    use crate::tui::local_model_wizard::LocalModelWizardStep;
    match step {
        LocalModelWizardStep::Setup | LocalModelWizardStep::SelectModels => LOCAL_MODEL_BASE_TEST,
        LocalModelWizardStep::Metadata => LOCAL_MODEL_BASE_METADATA,
        LocalModelWizardStep::Review => LOCAL_MODEL_BASE_REVIEW,
    }
}

pub(super) fn local_model_step_hints(
    step: crate::tui::local_model_wizard::LocalModelWizardStep,
) -> &'static [(&'static str, &'static str)] {
    use crate::tui::local_model_wizard::LocalModelWizardStep;
    match step {
        LocalModelWizardStep::Setup => LOCAL_MODEL_SETUP,
        LocalModelWizardStep::SelectModels => LOCAL_MODEL_SELECT,
        LocalModelWizardStep::Metadata => LOCAL_MODEL_METADATA,
        LocalModelWizardStep::Review => LOCAL_MODEL_REVIEW,
    }
}

/// The provider list's keys: while typing, only the search-exit hint is relevant;
/// otherwise the management shortcuts plus how to start or clear a search. One
/// chooser, called by the renderer and by [`footer_hints`], so the branch exists
/// once.
pub(super) fn provider_manager_hints(
    searching: bool,
    filter: &str,
) -> &'static [(&'static str, &'static str)] {
    if searching {
        PROVIDER_SEARCHING_HINTS
    } else if filter.is_empty() {
        PROVIDER_MANAGER_HINTS
    } else {
        PROVIDER_FILTERED_HINTS
    }
}

/// The permissions list's keys, chosen the same way as the provider list's.
pub(super) fn permissions_hints(
    searching: bool,
    filter: &str,
) -> &'static [(&'static str, &'static str)] {
    if searching {
        PERMISSIONS_SEARCHING_HINTS
    } else if filter.is_empty() {
        PERMISSIONS_HINTS
    } else {
        PERMISSIONS_FILTERED_HINTS
    }
}

/// The subagent list's keys, which follow the focused pane.
pub(super) fn subtask_hints(
    pane: crate::tui::event::SubtaskListPane,
) -> &'static [(&'static str, &'static str)] {
    if pane == crate::tui::event::SubtaskListPane::Detail {
        SUBTASK_DETAIL_HINTS
    } else {
        SUBTASK_LIST_HINTS
    }
}

/// The model picker's keys: the Reasoning pane is the one that assigns shortcuts
/// from a bare letter.
pub(super) fn model_picker_hints(
    pane: crate::tui::app::ModelPickerPane,
) -> &'static [(&'static str, &'static str)] {
    if pane == crate::tui::app::ModelPickerPane::Reasoning {
        MODEL_PICKER_ASSIGN
    } else {
        MODEL_PICKER_CHOOSE
    }
}

/// The authorize prompt's keys: the form shape decides which fields exist.
pub(super) fn api_key_hints(
    app: &AppState,
    provider_id: &str,
) -> &'static [(&'static str, &'static str)] {
    if app.provider_uses_endpoint_auth_form(provider_id) {
        API_KEY_ENDPOINT
    } else if app.provider_auth_form.origins.is_empty() {
        API_KEY_PLAIN
    } else {
        API_KEY_ORIGIN
    }
}

pub(super) fn context_hints(app: &AppState) -> &'static [(&'static str, &'static str)] {
    match app.context_state.view_mode {
        crate::tui::app::ContextViewMode::Wire => &[
            ("Up/Down", "move"),
            ("Enter", "expand/collapse"),
            ("Tab/Shift+Tab", "turns"),
            ("Esc", "close"),
        ],
        crate::tui::app::ContextViewMode::Turns => &[
            ("Up/Down", "move"),
            ("Enter", "expand/collapse"),
            ("Tab/Shift+Tab", "ledger"),
            ("Esc", "close"),
        ],
        crate::tui::app::ContextViewMode::Ledger => &[
            ("Up/Down", "move"),
            ("Enter", "expand/collapse"),
            ("p/d/s/r", "edit"),
            ("Tab/Shift+Tab", "wire"),
            ("Esc", "close"),
        ],
    }
}

pub(super) fn agent_composer_hints(
    state: &crate::tui::agent_composer::AgentComposerState,
) -> &'static [(&'static str, &'static str)] {
    use crate::tui::agent_composer::{AgentComposerField, AgentComposerStep};
    if state.is_builtin_subagent() {
        return match state.step {
            AgentComposerStep::Details => {
                &[("Enter", "choose"), ("d", "delete"), ("Tab", "review")]
            }
            AgentComposerStep::Review => AGENT_COMPOSER_REVIEW,
            AgentComposerStep::Tools | AgentComposerStep::Prompt => &[],
        };
    }
    match state.step {
        AgentComposerStep::Details => match state.field {
            AgentComposerField::Name | AgentComposerField::Description => {
                &[("Left/Right", "edit"), ("Tab", "tools")]
            }
            AgentComposerField::DefinitionKind
            | AgentComposerField::Location
            | AgentComposerField::Color
            | AgentComposerField::View => &[("Left/Right", "cycle"), ("Tab", "tools")],
            AgentComposerField::Model | AgentComposerField::BackupModel => {
                &[("Enter", "choose"), ("d", "delete"), ("Tab", "tools")]
            }
        },
        AgentComposerStep::Tools => &[("Up/Down", "move"), ("Space", "toggle"), ("Tab", "prompt")],
        AgentComposerStep::Prompt => &[
            ("Enter", "newline"),
            ("Ctrl+G", "extend description"),
            ("Tab", "review"),
        ],
        AgentComposerStep::Review => AGENT_COMPOSER_REVIEW,
    }
}
