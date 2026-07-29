use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::tui::app::AppState;
use crate::tui::event::{AppAction, Focus, ModalKind, View};

/// Modal/picker key mappers, routed to by `map_key`. Glob-re-exported so the
/// dispatcher and tests call them by bare name exactly as before the split.
mod completion;
mod modal;
use completion::*;
use modal::*;

const COMPLETION_PAGE_SIZE: i16 = 5;

#[derive(Debug, Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "the large variant is Action(AppAction); KeyIntent is a short-lived per-keypress value, so boxing would add a hot-path allocation for no real footprint win"
)]
pub enum KeyIntent {
    Action(AppAction),
    Submit,
    SubmitReplacement(String),
    CancelOrQuit,
    Quit,
    Insert(char),
    Noop,
}

pub fn map_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return KeyIntent::Noop;
    }

    if matches!(app.modal, Some(ModalKind::Onboarding { .. })) {
        return map_onboarding_key(key);
    }

    if let Some(modal) = app.modal.as_ref()
        && let Some(family) = crate::tui::event::PromptFamily::of_modal(modal)
    {
        return map_prompt_decision_key(family, key);
    }

    if matches!(app.modal, Some(ModalKind::QuestionPrompt { .. })) {
        return map_question_prompt_key(key);
    }

    if matches!(app.modal, Some(ModalKind::ApiKeyPrompt { .. })) {
        return map_api_key_prompt_key(key, app);
    }

    if matches!(app.modal, Some(ModalKind::UnauthorizeProviderPicker { .. })) {
        return map_unauthorize_provider_picker_key(key);
    }

    if matches!(app.modal, Some(ModalKind::UnauthorizeConfirm { .. })) {
        return map_unauthorize_confirm_key(key);
    }

    if matches!(app.modal, Some(ModalKind::AuthorizeProviderPicker { .. })) {
        return map_authorize_provider_picker_key(key);
    }

    if matches!(app.modal, Some(ModalKind::ReviewScopePicker { .. })) {
        return map_review_scope_picker_key(key);
    }

    if matches!(app.modal, Some(ModalKind::LocalModelWizard { .. })) {
        return map_local_model_wizard_key(key, app);
    }

    if matches!(app.modal, Some(ModalKind::AgentBrowser { .. })) {
        return map_agent_browser_key(key);
    }

    if matches!(app.modal, Some(ModalKind::ProviderManager { .. })) {
        return map_provider_manager_key(key, app);
    }

    if matches!(app.modal, Some(ModalKind::ProviderDetail { .. })) {
        return map_provider_detail_key(key);
    }

    if matches!(app.modal, Some(ModalKind::ProviderRemoveConfirm { .. })) {
        return map_provider_remove_confirm_key(key);
    }

    if matches!(app.modal, Some(ModalKind::SkillManager { .. })) {
        return map_skill_manager_key(key);
    }

    if matches!(app.modal, Some(ModalKind::MemoryManager { .. })) {
        return map_memory_manager_key(key);
    }

    if matches!(app.modal, Some(ModalKind::PermissionsManager { .. })) {
        return map_permissions_manager_key(key, app);
    }

    if matches!(app.modal, Some(ModalKind::MemoryAddWizard { .. })) {
        return map_memory_add_wizard_key(key, app);
    }

    if matches!(app.modal, Some(ModalKind::AgentComposer { .. })) {
        return map_agent_composer_key(key, app);
    }

    if matches!(app.modal, Some(ModalKind::AgentDeleteConfirm { .. })) {
        return map_agent_delete_confirm_key(key);
    }

    if matches!(app.modal, Some(ModalKind::ModelPicker { .. })) {
        return map_model_picker_key(key, app);
    }

    if matches!(app.modal, Some(ModalKind::SessionPicker { .. })) {
        return map_session_picker_key(key);
    }

    if matches!(app.modal, Some(ModalKind::PlanPicker { .. })) {
        return map_plan_picker_key(key);
    }

    if matches!(app.modal, Some(ModalKind::PlanOpenChoice { .. })) {
        return map_plan_open_choice_key(key);
    }

    if matches!(app.modal, Some(ModalKind::StartPlanChoice { .. })) {
        return map_start_plan_choice_key(key);
    }

    if matches!(app.modal, Some(ModalKind::PlanDeleteConfirm { .. })) {
        return map_plan_delete_confirm_key(key);
    }

    if matches!(app.modal, Some(ModalKind::SessionDeleteConfirm { .. })) {
        return map_session_delete_confirm_key(key);
    }

    if matches!(app.modal, Some(ModalKind::PlanDiscardConfirm { .. })) {
        return map_plan_discard_confirm_key(key);
    }

    if matches!(app.modal, Some(ModalKind::TaskList { .. })) {
        return map_task_list_key(key);
    }

    if matches!(app.modal, Some(ModalKind::PeerList { .. })) {
        return map_peer_list_key(key);
    }

    if matches!(app.modal, Some(ModalKind::Doctor { .. })) {
        return map_doctor_key(key);
    }

    if matches!(app.modal, Some(ModalKind::Refresh { .. })) {
        return map_refresh_key(key);
    }

    if matches!(app.modal, Some(ModalKind::UsageDashboard { .. })) {
        return map_usage_dashboard_key(key);
    }

    if matches!(app.modal, Some(ModalKind::SubtaskList { .. })) {
        return map_subtask_list_key(key, app);
    }

    if matches!(app.modal, Some(ModalKind::Context(_))) {
        return map_context_modal_key(key, app);
    }

    if matches!(app.modal, Some(ModalKind::Episodes { .. })) {
        return map_episodes_key(key);
    }

    if matches!(app.modal, Some(ModalKind::McpServers { .. })) {
        return map_mcp_servers_key(key);
    }

    if matches!(app.modal, Some(ModalKind::SandboxStatus { .. })) {
        return map_sandbox_modal_key(key);
    }

    if matches!(app.modal, Some(ModalKind::ThemePicker { .. })) {
        return map_theme_picker_key(key);
    }

    if matches!(app.modal, Some(ModalKind::ModePicker { .. })) {
        return map_mode_picker_key(key);
    }

    if matches!(app.modal, Some(ModalKind::Settings { .. })) {
        return map_settings_key(key);
    }

    if matches!(app.modal, Some(ModalKind::BusyCommand { .. })) {
        return map_busy_command_key(key);
    }

    if matches!(app.modal, Some(ModalKind::PlanFindingDetail { .. })) {
        return map_plan_finding_detail_key(key, app);
    }

    if app.modal.is_some() {
        return map_generic_modal_key(key, app);
    }

    map_primary_key(key, app)
}

/// Primary (non-modal) keymap: global chords, then focus-specific bindings in
/// priority order. Reached only when no modal is open.
fn map_primary_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    match key {
        KeyEvent {
            code: KeyCode::Char('c') | KeyCode::Char('C'),
            modifiers,
            ..
        } if modifiers.contains(KeyModifiers::SUPER) => KeyIntent::Noop,
        KeyEvent {
            code: KeyCode::Char('c') | KeyCode::Char('C'),
            modifiers,
            ..
        } if modifiers == KeyModifiers::CONTROL => KeyIntent::CancelOrQuit,
        KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => KeyIntent::Quit,
        // IDE-safe bindings come first: F1/Ctrl+Shift+P/Alt+Tab are
        // captured by VS Code, JetBrains, or the OS, so every chord has a
        // plain-terminal alternative that embedded terminals pass through.
        // Commands live in the `/` composer popover; `/keys` shows shortcuts.
        KeyEvent {
            code: KeyCode::Char('i'),
            modifiers: KeyModifiers::ALT,
            ..
        } => KeyIntent::Action(AppAction::ImplementPlan),
        KeyEvent {
            code: KeyCode::Char('t') | KeyCode::Char('T'),
            modifiers: KeyModifiers::ALT,
            ..
        } => KeyIntent::Action(AppAction::OpenTaskList),
        KeyEvent {
            code: KeyCode::Char('s') | KeyCode::Char('S'),
            modifiers: KeyModifiers::ALT,
            ..
        } => KeyIntent::Action(AppAction::OpenSubtaskList),
        KeyEvent {
            code: KeyCode::Char('m') | KeyCode::Char('M'),
            modifiers: KeyModifiers::ALT,
            ..
        } => KeyIntent::Action(AppAction::CycleApprovalLevel),
        // Copy mode: release mouse capture so the terminal does native text
        // selection (and re-enable it). Ctrl (not Alt) so it sends a clean
        // control byte on every terminal — Alt+letter inserts a diacritic on
        // macOS when Option isn't Meta. VS Code's Ctrl+G (go-to-line) is
        // editor-scoped, so it reaches the terminal here. Also on `/select`.
        KeyEvent {
            code: KeyCode::Char('g') | KeyCode::Char('G'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => KeyIntent::Action(AppAction::ToggleMouseCapture),
        KeyEvent {
            code: KeyCode::Char('t'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } => KeyIntent::Action(AppAction::SetFocus(next_focus(app))),
        // Jump the transcript back to the latest message from anywhere — most
        // importantly while typing in the composer, where plain End/G edit the
        // draft instead. Pairs with the floating jump-to-latest pill.
        KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::CONTROL,
            ..
        } => KeyIntent::Action(AppAction::ScrollBottom),
        // Completion popover navigation while it is open.
        KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Input) && completion_open(app) => {
            KeyIntent::Action(AppAction::CompletionMove(-1))
        }
        KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Input) && completion_open(app) => {
            KeyIntent::Action(AppAction::CompletionMove(1))
        }
        KeyEvent {
            code: KeyCode::PageUp,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Input) && completion_open(app) => {
            KeyIntent::Action(AppAction::CompletionMove(-COMPLETION_PAGE_SIZE))
        }
        KeyEvent {
            code: KeyCode::PageDown,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Input) && completion_open(app) => {
            KeyIntent::Action(AppAction::CompletionMove(COMPLETION_PAGE_SIZE))
        }
        KeyEvent {
            code: KeyCode::Esc, ..
        } if matches!(app.focus, Focus::Input) && completion_open(app) => {
            KeyIntent::Action(AppAction::CompletionDismiss)
        }
        // Esc progressively clears: selection first, then the draft.
        KeyEvent {
            code: KeyCode::Esc, ..
        } if matches!(app.focus, Focus::Input) => {
            if app.composer.has_selection() {
                KeyIntent::Action(AppAction::ClearSelection)
            } else if !app.input().is_empty() {
                KeyIntent::Action(AppAction::ClearInput)
            } else {
                KeyIntent::Noop
            }
        }
        KeyEvent {
            code: KeyCode::Esc, ..
        } if matches!(app.focus, Focus::Transcript)
            && matches!(app.view, View::Agent)
            && app.active_group_tool_selection.is_some() =>
        {
            KeyIntent::Action(AppAction::ClearExecutionGroupSelection)
        }
        KeyEvent {
            code: KeyCode::Char('d') | KeyCode::Char('D'),
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Transcript)
            && matches!(app.view, View::Agent)
            && app
                .selected_execution_group_tool()
                .and_then(|activity| activity.diff.as_ref())
                .is_some() =>
        {
            KeyIntent::Action(AppAction::OpenSelectedExecutionGroupDiff)
        }
        KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Transcript)
            && matches!(app.view, View::Agent)
            && app.active_group_tool_selection.is_some() =>
        {
            KeyIntent::Action(AppAction::OpenSelectedExecutionGroupTool)
        }
        KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Transcript)
            && matches!(app.view, View::Agent)
            && app.active_group_tool_selection.is_some() =>
        {
            KeyIntent::Action(AppAction::ExecutionGroupMoveSelection { delta: -1 })
        }
        KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Transcript)
            && matches!(app.view, View::Agent)
            && app.active_group_tool_selection.is_some() =>
        {
            KeyIntent::Action(AppAction::ExecutionGroupMoveSelection { delta: 1 })
        }
        KeyEvent {
            code: KeyCode::Esc, ..
        } if matches!(app.focus, Focus::Transcript) && app.transcript_selection.is_some() => {
            KeyIntent::Action(AppAction::ClearSelection)
        }
        KeyEvent {
            code: KeyCode::Esc, ..
        } if matches!(app.focus, Focus::Transcript | Focus::Todo | Focus::Plan) => {
            KeyIntent::Action(AppAction::SetFocus(Focus::Input))
        }
        KeyEvent {
            code: KeyCode::Esc, ..
        } if matches!(app.focus, Focus::Transcript) && app.transcript_focus.is_some() => {
            KeyIntent::Action(AppAction::ClearTranscriptFocus)
        }
        KeyEvent {
            code: KeyCode::Tab, ..
        } if matches!(app.focus, Focus::Input) && completion_open(app) => {
            if app.completion.active
                && let Some(action) = current_completion_action(app.input(), app)
            {
                if current_completion_is_command(app)
                    && let AppAction::CompleteInputTo(replacement) = action
                {
                    return KeyIntent::Action(AppAction::CompleteInputTo(command_tab_replacement(
                        &replacement,
                    )));
                }
                return KeyIntent::Action(action);
            }
            if app.input().starts_with('/') {
                if let Some(replacement) = complete_command_arg_from_state(app.input(), app) {
                    return KeyIntent::Action(AppAction::CompleteInputTo(replacement));
                }
                if let Some(replacement) = crate::commands::complete_command(app.input())
                    && replacement != app.input()
                {
                    return KeyIntent::Action(AppAction::CompleteInputTo(replacement));
                }
            }
            KeyIntent::Noop
        }
        KeyEvent {
            code: KeyCode::Tab, ..
        } if matches!(app.focus, Focus::Input) && app.input().starts_with('/') => {
            if let Some(replacement) = complete_command_arg_from_state(app.input(), app) {
                return KeyIntent::Action(AppAction::CompleteInputTo(replacement));
            }
            if let Some(replacement) = crate::commands::complete_command(app.input())
                && replacement != app.input()
            {
                return KeyIntent::Action(AppAction::CompleteInputTo(replacement));
            }
            KeyIntent::Noop
        }
        KeyEvent {
            code: KeyCode::Tab, ..
        } => KeyIntent::Action(AppAction::SetFocus(next_focus(app))),
        KeyEvent {
            code: KeyCode::BackTab,
            ..
        } => KeyIntent::Action(AppAction::ToggleView),
        // Word-wise editing. These must precede the plain movement arms,
        // which match any modifier combination.
        KeyEvent {
            code: KeyCode::Char('w'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::DeleteWordBack),
        KeyEvent {
            code: KeyCode::Char('u'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::DeleteToLineStart),
        KeyEvent {
            code: KeyCode::Char('k'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::DeleteToLineEnd),
        // Ctrl+V pastes an image from the clipboard (falling back to text) —
        // bracketed paste only carries text, so image paste needs an explicit
        // key. Terminals that swallow Ctrl+V into a bracketed paste still reach
        // the text path via Event::Paste.
        KeyEvent {
            code: KeyCode::Char('v'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::PasteFromClipboard),
        KeyEvent {
            code: KeyCode::Backspace,
            modifiers,
            ..
        } if matches!(app.focus, Focus::Input)
            && (modifiers.contains(KeyModifiers::CONTROL)
                || modifiers.contains(KeyModifiers::ALT)) =>
        {
            KeyIntent::Action(AppAction::DeleteWordBack)
        }
        KeyEvent {
            code: KeyCode::Left,
            modifiers,
            ..
        } if matches!(app.focus, Focus::Input)
            && (modifiers.contains(KeyModifiers::CONTROL)
                || modifiers.contains(KeyModifiers::ALT)) =>
        {
            KeyIntent::Action(AppAction::CursorWordLeft)
        }
        KeyEvent {
            code: KeyCode::Right,
            modifiers,
            ..
        } if matches!(app.focus, Focus::Input)
            && (modifiers.contains(KeyModifiers::CONTROL)
                || modifiers.contains(KeyModifiers::ALT)) =>
        {
            KeyIntent::Action(AppAction::CursorWordRight)
        }
        // History navigation. Alt+Up/Down are the IDE-safe primary (VS Code
        // captures Ctrl+P for quick-open); Ctrl+P/N remain below for plain
        // terminals. Must precede the plain Up/Down arms.
        KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::ALT,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::HistoryPrev),
        KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::ALT,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::HistoryNext),
        // Shift-extended selection. These must also precede the plain
        // movement arms or they are unreachable.
        KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ExtendCursorLeft),
        KeyEvent {
            code: KeyCode::Right,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ExtendCursorRight),
        KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ExtendCursorUp),
        KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ExtendCursorDown),
        KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ExtendCursorStart),
        KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ExtendCursorEnd),
        KeyEvent {
            code: KeyCode::Backspace,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::Backspace),
        KeyEvent {
            code: KeyCode::Delete,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::DeleteForward),
        KeyEvent {
            code: KeyCode::Delete,
            ..
        } if matches!(app.focus, Focus::Transcript) => {
            KeyIntent::Action(AppAction::CancelFocusedQueuedInput)
        }
        KeyEvent {
            code: KeyCode::Left,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::CursorLeft),
        KeyEvent {
            code: KeyCode::Right,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::CursorRight),
        KeyEvent {
            code: KeyCode::Up, ..
        } if matches!(app.focus, Focus::Input)
            && app.input().is_empty()
            && app.first_queued_input().is_some() =>
        {
            KeyIntent::Action(AppAction::WithdrawQueuedInput)
        }
        KeyEvent {
            code: KeyCode::Up, ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::CursorUp),
        KeyEvent {
            code: KeyCode::Down,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::CursorDown),
        KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::CursorStart),
        KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::CursorEnd),
        // PageUp/PageDown in the composer scroll the body when it overflows.
        // The run loop's clamp path resolves the page delta using the live
        // body height, so the keymap can stay layout-agnostic.
        KeyEvent {
            code: KeyCode::PageUp,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ComposerPage(-1)),
        KeyEvent {
            code: KeyCode::PageDown,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ComposerPage(1)),
        // Shift+PageUp/PageDown: scroll AND extend the text selection by one
        // page worth of chars (VS Code/JetBrains convention).
        KeyEvent {
            code: KeyCode::PageUp,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Input) => {
            KeyIntent::Action(AppAction::ExtendComposerByPage(-1))
        }
        KeyEvent {
            code: KeyCode::PageDown,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Input) => {
            KeyIntent::Action(AppAction::ExtendComposerByPage(1))
        }
        // Ctrl+Home: explicit scroll-to-top of the composer overflow. Plain
        // Home/End keep moving the caret. The clamp path pins
        // the offset to the valid range. (Ctrl+End is a global jump-to-latest
        // for the transcript — handled above — so the composer no longer
        // claims it.)
        KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Input) => {
            KeyIntent::Action(AppAction::SetComposerScroll(0))
        }
        KeyEvent {
            code: KeyCode::Char('p'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::HistoryPrev),
        KeyEvent {
            code: KeyCode::Char('n'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::HistoryNext),
        KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ComposerSelectAll),
        KeyEvent {
            code: KeyCode::Char('z' | 'Z'),
            modifiers,
            ..
        } if matches!(app.focus, Focus::Input)
            && modifiers.contains(KeyModifiers::CONTROL)
            && modifiers.contains(KeyModifiers::SHIFT) =>
        {
            KeyIntent::Action(AppAction::ComposerRedo)
        }
        KeyEvent {
            code: KeyCode::Char('z'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ComposerUndo),
        KeyEvent {
            code: KeyCode::Char('y'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::ComposerRedo),
        // Transcript selection extension (must precede plain scroll arms).
        KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Transcript) => {
            KeyIntent::Action(AppAction::ExtendTranscriptCursor(-1))
        }
        KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Transcript) => {
            KeyIntent::Action(AppAction::ExtendTranscriptCursor(1))
        }
        KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Transcript) => {
            KeyIntent::Action(AppAction::ExtendTranscriptCursor(i16::MIN))
        }
        KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Transcript) => {
            KeyIntent::Action(AppAction::ExtendTranscriptCursor(i16::MAX))
        }
        KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::CONTROL,
            ..
        } if matches!(app.focus, Focus::Transcript) => {
            KeyIntent::Action(AppAction::TranscriptSelectAll)
        }
        KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Todo) => KeyIntent::Action(AppAction::ScrollSidebar(-1)),
        KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Todo) => KeyIntent::Action(AppAction::ScrollSidebar(1)),
        KeyEvent {
            code: KeyCode::PageUp,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Todo) => KeyIntent::Action(AppAction::ScrollSidebar(-8)),
        KeyEvent {
            code: KeyCode::PageDown,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Todo) => KeyIntent::Action(AppAction::ScrollSidebar(8)),
        KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Todo) => KeyIntent::Action(AppAction::SetSidebarScroll(0)),
        KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Todo) => {
            KeyIntent::Action(AppAction::SetSidebarScroll(u16::MAX))
        }
        // Left/Right browse plan phases in the todo card. Only bound for a
        // multi-phase plan, so flat/single-phase/no-plan cases fall through.
        KeyEvent {
            code: KeyCode::Left,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Todo) && app.plan.phases.len() >= 2 => {
            KeyIntent::Action(AppAction::MoveTodoPhase(-1))
        }
        KeyEvent {
            code: KeyCode::Right,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Todo) && app.plan.phases.len() >= 2 => {
            KeyIntent::Action(AppAction::MoveTodoPhase(1))
        }
        // Agent transcript block navigation. In Plan view, transcript focus is
        // plain chat scrolling; block navigation stays in the full Agent view.
        KeyEvent {
            code: KeyCode::Up,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Transcript) && matches!(app.view, View::Agent) => {
            KeyIntent::Action(AppAction::MoveTranscriptFocus(-1))
        }
        KeyEvent {
            code: KeyCode::Down,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Transcript) && matches!(app.view, View::Agent) => {
            KeyIntent::Action(AppAction::MoveTranscriptFocus(1))
        }
        KeyEvent {
            code: KeyCode::Home,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Transcript) && matches!(app.view, View::Agent) => {
            KeyIntent::Action(AppAction::FocusTranscriptFirst)
        }
        KeyEvent {
            code: KeyCode::End,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Transcript) && matches!(app.view, View::Agent) => {
            KeyIntent::Action(AppAction::FocusTranscriptLast)
        }
        // Outside the input, scrolling drives the plan canvas in the plan
        // view (the main reading surface there) and the transcript otherwise.
        KeyEvent {
            code: KeyCode::Up, ..
        } => KeyIntent::Action(scroll_action(app, -1)),
        KeyEvent {
            code: KeyCode::Down,
            ..
        } => KeyIntent::Action(scroll_action(app, 1)),
        KeyEvent {
            code: KeyCode::PageUp,
            ..
        } => KeyIntent::Action(scroll_action(app, -8)),
        KeyEvent {
            code: KeyCode::PageDown,
            ..
        } => KeyIntent::Action(scroll_action(app, 8)),
        KeyEvent {
            code: KeyCode::Home,
            ..
        }
        | KeyEvent {
            code: KeyCode::Char('g'),
            modifiers: KeyModifiers::NONE,
            ..
        } if !matches!(app.focus, Focus::Input) => {
            if matches!(app.view, View::Plan) && matches!(app.focus, Focus::Plan) {
                KeyIntent::Action(AppAction::SetPlanScroll(0))
            } else {
                KeyIntent::Action(AppAction::ScrollTop)
            }
        }
        KeyEvent {
            code: KeyCode::End, ..
        }
        | KeyEvent {
            code: KeyCode::Char('G'),
            ..
        } if !matches!(app.focus, Focus::Input) => {
            if matches!(app.view, View::Plan) && matches!(app.focus, Focus::Plan) {
                // Clamped against the canvas height at draw time.
                KeyIntent::Action(AppAction::SetPlanScroll(u16::MAX))
            } else {
                KeyIntent::Action(AppAction::ScrollBottom)
            }
        }
        KeyEvent {
            code: KeyCode::Char(ch @ '1'..='2'),
            modifiers: KeyModifiers::NONE,
            ..
        } if !matches!(app.focus, Focus::Input) => {
            KeyIntent::Action(AppAction::SetView(view_for_digit(ch)))
        }
        KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::ALT,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::InsertNewline),
        KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::SHIFT,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Action(AppAction::InsertNewline),
        // Enter accepts the highlighted completion when it would change the
        // input (IDE-style picking); once the input matches, Enter submits.
        KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            ..
        } if matches!(app.focus, Focus::Input) && completion_open(app) => {
            if input_is_model_shortcut_command(app.input()) {
                return KeyIntent::Submit;
            }
            // A fully-typed command name submits as typed, even when the
            // completion highlight drifted onto a description match while
            // refining (easy to hit with /bonsai — the app name appears in
            // many command descriptions). Same "once the input matches,
            // Enter submits" rule the arms below apply.
            if crate::commands::command_description(app.input().trim()).is_some() {
                return KeyIntent::Submit;
            }
            match current_completion_action(app.input(), app) {
                Some(replacement) if current_completion_is_command(app) => {
                    if let AppAction::CompleteInputTo(replacement) = replacement {
                        KeyIntent::SubmitReplacement(replacement)
                    } else {
                        KeyIntent::Noop
                    }
                }
                Some(AppAction::CompleteInputTo(replacement)) if replacement != app.input() => {
                    KeyIntent::Action(AppAction::CompleteInputTo(replacement))
                }
                Some(AppAction::CompleteInputRange {
                    start,
                    end,
                    replacement,
                }) => {
                    if input_range_matches(app.input(), start, end, &replacement) {
                        KeyIntent::Submit
                    } else {
                        KeyIntent::Action(AppAction::CompleteInputRange {
                            start,
                            end,
                            replacement,
                        })
                    }
                }
                _ => KeyIntent::Submit,
            }
        }
        KeyEvent {
            code: KeyCode::Enter,
            ..
        } if matches!(app.focus, Focus::Input) => KeyIntent::Submit,
        KeyEvent {
            code: KeyCode::Enter,
            ..
        } if matches!(app.focus, Focus::Todo) => KeyIntent::Noop,
        // In the plan pane, Enter opens the findings detail modal (which then
        // navigates findings and jumps to evidence in-modal). No-op with none.
        KeyEvent {
            code: KeyCode::Enter,
            ..
        } if matches!(app.focus, Focus::Plan) => {
            if app.plan.findings.is_empty() {
                KeyIntent::Noop
            } else {
                KeyIntent::Action(AppAction::OpenPlanFindings)
            }
        }
        KeyEvent {
            code: KeyCode::Enter,
            ..
        } if matches!(app.focus, Focus::Transcript) && matches!(app.view, View::Agent) => {
            if app.transcript_focus.is_some() {
                KeyIntent::Action(AppAction::OpenFocusedDetail)
            } else {
                KeyIntent::Noop
            }
        }
        KeyEvent {
            code: KeyCode::Enter,
            ..
        } if matches!(app.focus, Focus::Transcript) => app
            .latest_tool_id()
            .map(AppAction::OpenToolDetail)
            .map(KeyIntent::Action)
            .unwrap_or(KeyIntent::Noop),
        KeyEvent {
            code: KeyCode::Char(ch),
            modifiers,
            ..
        } if matches!(app.focus, Focus::Input)
            && !ch.is_control()
            && !modifiers.contains(KeyModifiers::SUPER)
            // Ctrl+Alt is how crossterm reports AltGr on some layouts, so it
            // must remain text input. Ctrl-only combinations are shortcuts.
            && (modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                == modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)) =>
        {
            KeyIntent::Insert(ch)
        }
        _ => KeyIntent::Noop,
    }
}

fn input_is_model_shortcut_command(input: &str) -> bool {
    crate::model_role::ModelShortcutKey::from_command(input.trim()).is_some()
}

fn scroll_action(app: &AppState, delta: i16) -> AppAction {
    if matches!(app.focus, Focus::Todo) {
        AppAction::ScrollSidebar(delta)
    } else if matches!(app.focus, Focus::Plan) {
        AppAction::ScrollPlan(delta)
    } else {
        AppAction::ScrollCurrent(delta)
    }
}

fn view_for_digit(ch: char) -> View {
    match ch {
        '1' => View::Agent,
        _ => View::Plan,
    }
}

fn next_focus(app: &AppState) -> Focus {
    match (app.focus, app.view) {
        (Focus::Input, _) => Focus::Transcript,
        (Focus::Transcript, View::Plan) => Focus::Plan,
        (Focus::Transcript, View::Agent) if app.todo_focus_available => Focus::Todo,
        (Focus::Transcript, View::Agent) => Focus::Input,
        (Focus::Todo, _) | (Focus::Plan, _) => Focus::Input,
        (other, _) => other,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::diff::{DiffStatus, FileDiff};
    use crate::interaction::QuestionOption;
    use crate::provider::ReasoningSelection;
    use crate::tui::app::{
        CompletionCandidate, ContextViewMode, ExecutionGroup, InlineToolSelection, ModelPickerPane,
        ToolActivity, ToolStatus, TranscriptItem, TranscriptPosition, TranscriptSelection,
    };
    use crate::tui::event::{ModalKind, PromptDecision, PromptFamily, SubtaskListPane};
    use crate::tui::pickers::{ModelOption, ProviderOption};
    use crate::tui::test_utils::{app, input_app_with_provider_choices, input_app_with_text};

    #[test]
    fn web_domain_prompt_keys_map_to_decisions() {
        let mut app = app();
        app.modal = Some(ModalKind::WebDomainPrompt {
            request_id: 1,
            url: "https://example.com/p".to_string(),
            host: "example.com".to_string(),
            redirected_from: None,
            origin: None,
        });
        let press =
            |app: &AppState, code: KeyCode| map_key(KeyEvent::new(code, KeyModifiers::NONE), app);
        assert!(matches!(
            press(&app, KeyCode::Char('a')),
            KeyIntent::Action(AppAction::PromptDecision {
                family: PromptFamily::WebDomain,
                decision: PromptDecision::AllowOnce,
            })
        ));
        assert!(matches!(
            press(&app, KeyCode::Enter),
            KeyIntent::Action(AppAction::PromptDecision {
                family: PromptFamily::WebDomain,
                decision: PromptDecision::AllowOnce,
            })
        ));
        assert!(matches!(
            press(&app, KeyCode::Char('s')),
            KeyIntent::Action(AppAction::PromptDecision {
                family: PromptFamily::WebDomain,
                decision: PromptDecision::AllowSession,
            })
        ));
        assert!(matches!(
            press(&app, KeyCode::Char('p')),
            KeyIntent::Action(AppAction::PromptDecision {
                family: PromptFamily::WebDomain,
                decision: PromptDecision::AllowProject,
            })
        ));
        // "Never": the persistent per-project deny, offered wherever `p` is.
        for code in [KeyCode::Char('n'), KeyCode::Char('N')] {
            assert!(
                matches!(
                    press(&app, code),
                    KeyIntent::Action(AppAction::PromptDecision {
                        family: PromptFamily::WebDomain,
                        decision: PromptDecision::DenyProject,
                    })
                ),
                "{code:?} should map to a per-project never"
            );
        }
        for code in [KeyCode::Char('d'), KeyCode::Esc, KeyCode::Char('q')] {
            assert!(
                matches!(
                    press(&app, code),
                    KeyIntent::Action(AppAction::PromptDecision {
                        family: PromptFamily::WebDomain,
                        decision: PromptDecision::Deny,
                    })
                ),
                "{code:?} should deny"
            );
        }
        // Scroll keys still drive the modal viewport.
        assert!(matches!(
            press(&app, KeyCode::Down),
            KeyIntent::Action(AppAction::ScrollModal(1))
        ));
    }

    #[test]
    fn permission_prompt_never_key_persists_but_sandbox_escape_ignores_it() {
        let press =
            |app: &AppState, code: KeyCode| map_key(KeyEvent::new(code, KeyModifiers::NONE), app);

        // The permission prompt offers a project scope, so `n` denies-for-project.
        let mut perm = app();
        perm.modal = Some(ModalKind::PermissionPrompt {
            request_id: 1,
            command: "rm secrets".to_string(),
            origin: None,
        });
        assert!(matches!(
            press(&perm, KeyCode::Char('n')),
            KeyIntent::Action(AppAction::PromptDecision {
                family: PromptFamily::Permission,
                decision: PromptDecision::DenyProject,
            })
        ));

        // Sandbox escapes are never persisted: `n` has no per-project deny to
        // map to, so it stays a no-op rather than silently denying.
        let mut escape = app();
        escape.modal = Some(ModalKind::SandboxEscalationPrompt {
            request_id: 2,
            command: "curl https://example.com".to_string(),
            origin: None,
            kind: crate::interaction::SandboxEscalationKind::SandboxOnly,
        });
        assert!(matches!(
            press(&escape, KeyCode::Char('n')),
            KeyIntent::Noop
        ));
    }

    #[test]
    fn usage_dashboard_keys_switch_tabs_and_scroll() {
        use crate::tui::event::UsageTab;
        let mut app = app();
        app.modal = Some(ModalKind::UsageDashboard {
            dashboard: Box::new(crate::storage::UsageDashboard {
                today: crate::storage::LocalToday {
                    day: "2026-07-10".to_string(),
                    julian_day: 2_461_231,
                    weekday: 5,
                },
                days: Vec::new(),
                models: Vec::new(),
                top_sessions: Vec::new(),
                session_stats: Default::default(),
                projects: Vec::new(),
                status_counts: Vec::new(),
                tools: Vec::new(),
                self_review: Default::default(),
                lifetime: Default::default(),
            }),
            tab: UsageTab::Activity,
        });
        let press =
            |app: &AppState, code: KeyCode| map_key(KeyEvent::new(code, KeyModifiers::NONE), app);
        assert!(matches!(
            press(&app, KeyCode::Tab),
            KeyIntent::Action(AppAction::UsageDashboardCycleTab(1))
        ));
        assert!(matches!(
            press(&app, KeyCode::BackTab),
            KeyIntent::Action(AppAction::UsageDashboardCycleTab(-1))
        ));
        assert!(matches!(
            press(&app, KeyCode::Char('l')),
            KeyIntent::Action(AppAction::UsageDashboardCycleTab(1))
        ));
        assert!(matches!(
            press(&app, KeyCode::Char('h')),
            KeyIntent::Action(AppAction::UsageDashboardCycleTab(-1))
        ));
        assert!(matches!(
            press(&app, KeyCode::Char('4')),
            KeyIntent::Action(AppAction::UsageDashboardSelectTab(UsageTab::Sessions))
        ));
        assert!(matches!(
            press(&app, KeyCode::Down),
            KeyIntent::Action(AppAction::ScrollModal(1))
        ));
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            assert!(
                matches!(press(&app, code), KeyIntent::Action(AppAction::CloseModal)),
                "{code:?} should close"
            );
        }
    }

    fn saved_plan(id: i64, title: &str) -> crate::storage::SavedPlanSummary {
        crate::storage::SavedPlanSummary {
            id: crate::storage::SavedPlanId::from_raw(id),
            project_path: "/tmp/project".to_string(),
            title: title.to_string(),
            source_session_id: Some(crate::storage::SessionId::from_raw(id)),
            branch: Some("main".to_string()),
            status: crate::storage::SavedPlanStatus::Draft,
            execution_session_id: None,
            saved_at_ms: 1_000,
            updated_at_ms: 1_000,
            section_count: 1,
            task_count: 1,
        }
    }

    fn session_summary(id: i64, name: &str) -> crate::storage::SessionSummary {
        crate::storage::SessionSummary {
            id: crate::storage::SessionId::from_raw(id),
            project_path: "/tmp/project".to_string(),
            name: name.to_string(),
            summary: String::new(),
            provider_id: "codex".to_string(),
            model: "test-model".to_string(),
            reasoning: crate::provider::ReasoningSelection::default(),
            status: crate::storage::SessionStatus::Active,
            terminal_reason: None,
            updated_at_ms: 1_000,
            message_count: 1,
            prompt_token_count: 0,
            completion_token_count: 0,
            cache_read_input_token_count: 0,
            cache_creation_input_token_count: 0,
            cache_measured_input_token_count: 0,
            cost_micros: 0,
            no_cache_cost_micros: 0,
            source_plan_id: None,
        }
    }

    fn context_report() -> crate::agent::ContextReport {
        crate::agent::ContextReport {
            budget_tokens: 120_000,
            entries: Vec::new(),
            ledger: vec![crate::agent::ContextNode {
                id: "system".into(),
                kind: crate::agent::ContextNodeKind::Persona,
                inclusion: crate::agent::ContextInclusion::Included,
                role: Some(crate::agent::ContextRole::System),
                label: "System prompt".to_string(),
                tokens: 12,
                chars: 6,
                bytes: 6,
                source: crate::provider::TokenCounterKind::Heuristic,
                confidence: crate::provider::EstimateConfidence::Low,
                preview: "system".to_string(),
                sources: Vec::new(),
                children: vec![crate::agent::ContextNode {
                    id: "persona".into(),
                    kind: crate::agent::ContextNodeKind::Persona,
                    inclusion: crate::agent::ContextInclusion::Included,
                    role: Some(crate::agent::ContextRole::System),
                    label: "Persona".to_string(),
                    tokens: 6,
                    chars: 7,
                    bytes: 7,
                    source: crate::provider::TokenCounterKind::Heuristic,
                    confidence: crate::provider::EstimateConfidence::Low,
                    preview: "persona".to_string(),
                    sources: Vec::new(),
                    children: Vec::new(),
                }],
            }],
            estimate_source: crate::provider::TokenCounterKind::Heuristic,
            estimate_confidence: crate::provider::EstimateConfidence::Low,
            prompt_estimate_tokens: 12,
            tool_schema_tokens: 0,
            last_prompt_tokens: None,
            last_completion_tokens: None,
            last_input_cache: None,
            last_turn_cost_micros: None,
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
            session_input_cache: None,
            session_cost_micros: None,
            ..Default::default()
        }
    }

    fn select_command_completion(app: &mut AppState, label: &str) {
        let Some(index) = app.completion.candidates.iter().position(|candidate| {
            matches!(
                candidate,
                CompletionCandidate::Command { label: candidate_label, .. }
                    if candidate_label == label
            )
        }) else {
            panic!("expected {label} in completion candidates");
        };
        app.completion.cursor = index;
    }

    fn select_provider_completion(app: &mut AppState, id: &str) {
        let Some(index) = app.completion.candidates.iter().position(|candidate| {
            matches!(
                candidate,
                CompletionCandidate::Provider {
                    id: candidate_id,
                    ..
                } if candidate_id == id
            )
        }) else {
            panic!("expected {id} in provider completion candidates");
        };
        app.completion.cursor = index;
    }

    fn set_path_completion(app: &mut AppState, path: &str, start: usize, end: usize) {
        app.completion.active = true;
        app.completion.cursor = 0;
        app.completion.replacement_range = Some((start, end));
        app.completion.candidates = vec![CompletionCandidate::Path {
            path: path.to_string(),
            kind: crate::tui::path_search::PathKind::File,
        }];
    }

    #[test]
    fn shift_left_extends_selection_in_input() {
        let app = input_app_with_text("hello");

        let intent = map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::ExtendCursorLeft)
        ));
    }

    #[test]
    fn ctrl_left_jumps_by_word_in_input() {
        let app = input_app_with_text("hello world");

        let intent = map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::CursorWordLeft)
        ));
    }

    #[test]
    fn ctrl_w_deletes_previous_word() {
        let app = input_app_with_text("hello world");

        let intent = map_key(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &app,
        );

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::DeleteWordBack)
        ));
    }

    #[test]
    fn ctrl_modified_chars_are_not_inserted() {
        let app = input_app_with_text("hi");

        let intent = map_key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
            &app,
        );

        assert!(matches!(intent, KeyIntent::Noop));
    }

    #[test]
    fn multiline_input_maps_plain_vertical_arrows_to_cursor_actions() {
        let app = input_app_with_text("first\nsecond");

        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);

        assert!(matches!(up, KeyIntent::Action(AppAction::CursorUp)));
        assert!(matches!(down, KeyIntent::Action(AppAction::CursorDown)));
    }

    #[test]
    fn up_with_empty_input_withdraws_first_queued_message() {
        let mut app = input_app_with_text("");
        app.queued_inputs.push(crate::tui::app::QueuedInput {
            id: 1,
            text: "queued edit".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: crate::agent::AgentMode::Coding,
        });

        let intent = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::WithdrawQueuedInput)
        ));
    }

    #[test]
    fn digits_switch_views_outside_input_but_type_inside() {
        let mut app = app();
        app.focus = Focus::Transcript;

        let intent = map_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetView(View::Plan))
        ));

        app.focus = Focus::Input;
        let intent = map_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE), &app);
        assert!(matches!(intent, KeyIntent::Insert('2')));
    }

    #[test]
    fn modified_and_control_chars_do_not_enter_composer() {
        let mut app = app();
        app.focus = Focus::Input;

        for key in [
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::SUPER),
            KeyEvent::new(KeyCode::Char('\u{1b}'), KeyModifiers::NONE),
        ] {
            assert!(matches!(map_key(key, &app), KeyIntent::Noop));
        }

        let alt_gr = KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );
        assert!(matches!(map_key(alt_gr, &app), KeyIntent::Insert('@')));
    }

    #[test]
    fn shift_tab_switches_agents_and_plan_focus_scrolls_canvas() {
        let mut app = app();
        app.focus = Focus::Transcript;

        let intent = map_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &app);
        assert!(matches!(intent, KeyIntent::Action(AppAction::ToggleView)));

        app.view = View::Plan;
        app.focus = Focus::Plan;
        let intent = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::ScrollPlan(1))
        ));

        let intent = map_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::ALT), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::ImplementPlan)
        ));
    }

    #[test]
    fn alt_m_cycles_approval_level() {
        let app = app();

        let intent = map_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::ALT), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::CycleApprovalLevel)
        ));
    }

    #[test]
    fn ctrl_g_toggles_mouse_capture() {
        let app = app();
        for code in [KeyCode::Char('g'), KeyCode::Char('G')] {
            let intent = map_key(KeyEvent::new(code, KeyModifiers::CONTROL), &app);
            assert!(
                matches!(intent, KeyIntent::Action(AppAction::ToggleMouseCapture)),
                "{code:?} with Ctrl should toggle mouse capture"
            );
        }
    }

    #[test]
    fn tab_cycles_window_focus() {
        let mut app = app();
        app.view = View::Plan;
        app.focus = Focus::Input;

        let intent = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Transcript))
        ));
    }

    #[test]
    fn agent_focus_cycle_includes_todo_when_sidebar_is_visible() {
        let mut app = app();
        app.view = View::Agent;
        app.todo_focus_available = true;
        app.focus = Focus::Input;

        let input = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);
        assert!(matches!(
            input,
            KeyIntent::Action(AppAction::SetFocus(Focus::Transcript))
        ));

        app.focus = Focus::Transcript;
        let transcript = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);
        assert!(matches!(
            transcript,
            KeyIntent::Action(AppAction::SetFocus(Focus::Todo))
        ));

        app.focus = Focus::Todo;
        let todo = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);
        assert!(matches!(
            todo,
            KeyIntent::Action(AppAction::SetFocus(Focus::Input))
        ));
    }

    #[test]
    fn agent_focus_cycle_skips_todo_when_sidebar_is_hidden() {
        let mut app = app();
        app.view = View::Agent;
        app.todo_focus_available = false;
        app.focus = Focus::Transcript;

        let intent = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Input))
        ));
    }

    #[test]
    fn ctrl_t_in_agent_view_cycles_to_todo_when_sidebar_is_visible() {
        let mut app = app();
        app.view = View::Agent;
        app.todo_focus_available = true;
        app.focus = Focus::Transcript;

        let intent = map_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &app,
        );

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Todo))
        ));
    }

    #[test]
    fn agent_transcript_keys_drive_block_focus() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Transcript;
        app.transcript_focus = Some(1);

        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        assert!(matches!(
            up,
            KeyIntent::Action(AppAction::MoveTranscriptFocus(-1))
        ));

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::MoveTranscriptFocus(1))
        ));

        let home = map_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &app);
        assert!(matches!(
            home,
            KeyIntent::Action(AppAction::FocusTranscriptFirst)
        ));

        let end = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &app);
        assert!(matches!(
            end,
            KeyIntent::Action(AppAction::FocusTranscriptLast)
        ));

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::OpenFocusedDetail)
        ));

        let delete = map_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), &app);
        assert!(matches!(
            delete,
            KeyIntent::Action(AppAction::CancelFocusedQueuedInput)
        ));
    }

    #[test]
    fn agent_transcript_enter_without_focus_is_noop() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Transcript;

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        assert!(matches!(enter, KeyIntent::Noop));
    }

    #[test]
    fn inline_execution_group_navigation_and_open_detail() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Transcript;
        app.active_group_tool_selection = Some(InlineToolSelection {
            group_id: 1,
            selected_tool: 0,
        });
        app.transcript
            .push(TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 1,
                finished_at: None,
                tools: vec![
                    ToolActivity {
                        id: "call-1".to_string(),
                        name: "bash".to_string(),
                        arguments: "{\"command\":\"echo hi\"}".to_string(),
                        status: ToolStatus::Failed,
                        result: Some("ok".to_string()),
                        diff: None,
                        started_at: std::time::Instant::now(),
                        finished_at: Some(std::time::Instant::now()),
                    },
                    ToolActivity {
                        id: "call-2".to_string(),
                        name: "read".to_string(),
                        arguments: "{\"file_path\":\"a\"}".to_string(),
                        status: ToolStatus::Succeeded,
                        result: Some("ok".to_string()),
                        diff: None,
                        started_at: std::time::Instant::now(),
                        finished_at: Some(std::time::Instant::now()),
                    },
                ],
            }));

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ExecutionGroupMoveSelection { delta: 1 })
        ));

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::OpenSelectedExecutionGroupTool)
        ));

        let esc = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(
            esc,
            KeyIntent::Action(AppAction::ClearExecutionGroupSelection)
        ));
    }

    #[test]
    fn inline_execution_group_opens_diff_preview() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Transcript;
        app.active_group_tool_selection = Some(InlineToolSelection {
            group_id: 1,
            selected_tool: 0,
        });
        app.transcript
            .push(TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 1,
                finished_at: None,
                tools: vec![ToolActivity {
                    id: "call-1".to_string(),
                    name: "write".to_string(),
                    arguments: "{\"file_path\":\"a\"}".to_string(),
                    status: ToolStatus::Running,
                    result: None,
                    diff: Some(FileDiff {
                        path: "a".to_string(),
                        status: DiffStatus::Modified,
                        hunks: vec![],
                        truncated: false,
                        old_size: Some(0),
                        new_size: 1,
                        added_lines: 0,
                        removed_lines: 0,
                        additional_files: Box::default(),
                    }),
                    started_at: std::time::Instant::now(),
                    finished_at: None,
                }],
            }));

        let preview = map_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &app);
        assert!(matches!(
            preview,
            KeyIntent::Action(AppAction::OpenSelectedExecutionGroupDiff)
        ));
    }

    #[test]
    fn transcript_scroll_keys_still_scroll_when_not_in_inline_group_selection() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Transcript;

        let page_up = map_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_up,
            KeyIntent::Action(AppAction::ScrollCurrent(-8))
        ));

        let page_down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_down,
            KeyIntent::Action(AppAction::ScrollCurrent(8))
        ));

        let home = map_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &app);
        assert!(matches!(
            home,
            KeyIntent::Action(AppAction::FocusTranscriptFirst)
        ));

        let end = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &app);
        assert!(matches!(
            end,
            KeyIntent::Action(AppAction::FocusTranscriptLast)
        ));
    }

    #[test]
    fn shift_up_down_keep_transcript_text_selection() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Transcript;

        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT), &app);
        assert!(matches!(
            up,
            KeyIntent::Action(AppAction::ExtendTranscriptCursor(-1))
        ));

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ExtendTranscriptCursor(1))
        ));
    }

    #[test]
    fn composer_undo_and_redo_shortcuts_map_in_input_focus() {
        let app = input_app_with_text("hello");

        let undo = map_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
            &app,
        );
        let ctrl_y = map_key(
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL),
            &app,
        );
        let ctrl_shift_z = map_key(
            KeyEvent::new(
                KeyCode::Char('Z'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            &app,
        );

        assert!(matches!(undo, KeyIntent::Action(AppAction::ComposerUndo)));
        assert!(matches!(ctrl_y, KeyIntent::Action(AppAction::ComposerRedo)));
        assert!(matches!(
            ctrl_shift_z,
            KeyIntent::Action(AppAction::ComposerRedo)
        ));
    }

    #[test]
    fn ctrl_c_cancels_when_selection_exists() {
        let mut app = input_app_with_text("hello");
        app.composer.set_selection(0, 5);

        let intent = map_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &app,
        );

        assert!(matches!(intent, KeyIntent::CancelOrQuit));
    }

    #[test]
    fn ctrl_c_cancels_without_selection() {
        let app = input_app_with_text("hello");

        let intent = map_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &app,
        );

        assert!(matches!(intent, KeyIntent::CancelOrQuit));
    }

    #[test]
    fn super_c_is_ignored_when_selection_exists() {
        let mut app = input_app_with_text("hello");
        app.composer.set_selection(0, 5);

        let intent = map_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER), &app);

        assert!(matches!(intent, KeyIntent::Noop));
    }

    #[test]
    fn ctrl_shift_c_is_ignored_for_copy() {
        let mut app = app();
        app.focus = Focus::Transcript;
        app.transcript_selection = Some(TranscriptSelection {
            anchor: TranscriptPosition {
                item: 0,
                grapheme: 0,
                width: 80,
            },
            caret: TranscriptPosition {
                item: 0,
                grapheme: 5,
                width: 80,
            },
        });

        let intent = map_key(
            KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            &app,
        );

        assert!(matches!(intent, KeyIntent::Noop));
    }

    #[test]
    fn plan_view_transcript_focus_scrolls_chat() {
        let mut app = app();
        app.view = View::Plan;
        app.focus = Focus::Transcript;

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ScrollCurrent(1))
        ));

        let page_down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_down,
            KeyIntent::Action(AppAction::ScrollCurrent(8))
        ));

        let home = map_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &app);
        assert!(matches!(home, KeyIntent::Action(AppAction::ScrollTop)));

        let end = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &app);
        assert!(matches!(end, KeyIntent::Action(AppAction::ScrollBottom)));
    }

    #[test]
    fn ctrl_t_in_plan_view_cycles_focus_through_plan() {
        let mut app = app();
        app.view = View::Plan;
        app.focus = Focus::Input;

        // Input → Transcript
        let intent = map_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &app,
        );
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Transcript))
        ));
        app.focus = Focus::Transcript;

        // Transcript → Plan (only in plan view)
        let intent = map_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &app,
        );
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Plan))
        ));
        app.focus = Focus::Plan;

        // Plan → Input
        let intent = map_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &app,
        );
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Input))
        ));
    }

    #[test]
    fn ctrl_t_in_agent_view_skips_plan() {
        // In agent view the plan canvas doesn't exist, so the cycle
        // stays Input ↔ Transcript (regression guard).
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Transcript;

        let intent = map_key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &app,
        );
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Input))
        ));
    }

    #[test]
    fn up_down_scroll_plan_when_plan_focused() {
        let mut app = app();
        app.view = View::Plan;
        app.focus = Focus::Plan;

        let intent = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::ScrollPlan(1))
        ));

        let intent = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::ScrollPlan(-1))
        ));
    }

    #[test]
    fn alt_i_emits_implement_plan() {
        // Regression guard: with the keyboard enhancement flags pushed,
        // terminals deliver Alt+I as a single KeyEvent with the ALT
        // modifier. The keymap must map it to ImplementPlan.
        let app = app();
        let intent = map_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::ALT), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::ImplementPlan)
        ));
    }

    #[test]
    fn alt_t_emits_open_task_list() {
        let app = app();

        let intent = map_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT), &app);

        assert!(matches!(intent, KeyIntent::Action(AppAction::OpenTaskList)));
    }

    #[test]
    fn task_list_modal_keys_drive_selection_detail_and_delete() {
        let mut app = app();
        app.modal = Some(ModalKind::TaskList {
            tasks: Vec::new(),
            cursor: 0,
        });

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::TaskListMove(1))
        ));

        let page = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(page, KeyIntent::Action(AppAction::ScrollModal(8))));

        let delete = map_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), &app);
        assert!(matches!(
            delete,
            KeyIntent::Action(AppAction::TaskListDeleteSelected)
        ));
    }

    #[test]
    fn subtask_list_modal_keys_switch_panes_and_scroll_detail() {
        let mut app = app();
        app.modal = Some(ModalKind::SubtaskList {
            subtasks: Vec::new(),
            cursor: 0,
            pane: SubtaskListPane::List,
        });

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::SubtaskListMove(1))
        ));

        let right = map_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &app);
        assert!(matches!(
            right,
            KeyIntent::Action(AppAction::SubtaskListSetPane(SubtaskListPane::Detail))
        ));

        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);
        assert!(matches!(
            tab,
            KeyIntent::Action(AppAction::SubtaskListTogglePane)
        ));

        app.modal = Some(ModalKind::SubtaskList {
            subtasks: Vec::new(),
            cursor: 0,
            pane: SubtaskListPane::Detail,
        });

        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        assert!(matches!(up, KeyIntent::Action(AppAction::ScrollModal(-1))));

        let page = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(page, KeyIntent::Action(AppAction::ScrollModal(8))));

        for code in [KeyCode::Char('m'), KeyCode::Char('d')] {
            let intent = map_key(KeyEvent::new(code, KeyModifiers::NONE), &app);
            assert!(matches!(intent, KeyIntent::Noop));
        }

        let left = map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &app);
        assert!(matches!(
            left,
            KeyIntent::Action(AppAction::SubtaskListSetPane(SubtaskListPane::List))
        ));
    }

    #[test]
    fn plan_picker_keys_search_open_and_delete() {
        let mut app = app();
        app.modal = Some(ModalKind::PlanPicker {
            plans: vec![saved_plan(1, "Plan library")],
            query: String::new(),
            cursor: 0,
        });

        let input = map_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &app);
        assert!(matches!(
            input,
            KeyIntent::Action(AppAction::PlanPickerInputChar('p'))
        ));

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::PlanPickerSubmit)
        ));

        let delete = map_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), &app);
        assert!(matches!(
            delete,
            KeyIntent::Action(AppAction::PlanPickerDeleteSelected)
        ));
    }

    #[test]
    fn plan_open_choice_and_delete_confirm_keys_submit_typed_actions() {
        let mut app = app();
        app.modal = Some(ModalKind::PlanOpenChoice {
            plan: saved_plan(1, "Plan library"),
            cursor: 0,
        });

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::PlanOpenChoiceSubmit)
        ));

        app.modal = Some(ModalKind::PlanDeleteConfirm {
            plan: saved_plan(1, "Plan library"),
        });
        let confirm = map_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &app);
        assert!(matches!(
            confirm,
            KeyIntent::Action(AppAction::PlanDeleteConfirmSubmit)
        ));
    }

    #[test]
    fn start_plan_choice_keys_submit_typed_actions() {
        let mut app = app();
        app.modal = Some(ModalKind::StartPlanChoice { cursor: 0 });

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::StartPlanChoiceSubmit)
        ));
        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::StartPlanChoiceMove(1))
        ));
    }

    #[test]
    fn session_picker_keys_resume_and_delete() {
        let mut app = app();
        app.modal = Some(ModalKind::SessionPicker {
            sessions: vec![session_summary(1, "Test session")],
            cursor: 0,
        });

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::SessionPickerSubmit)
        ));

        let delete = map_key(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE), &app);
        assert!(matches!(
            delete,
            KeyIntent::Action(AppAction::SessionPickerDeleteSelected)
        ));
    }

    #[test]
    fn session_delete_confirm_keys_submit_or_cancel() {
        let mut app = app();
        app.modal = Some(ModalKind::SessionDeleteConfirm {
            session: session_summary(1, "Test session"),
        });

        for code in [KeyCode::Char('y'), KeyCode::Char('Y'), KeyCode::Enter] {
            let intent = map_key(KeyEvent::new(code, KeyModifiers::NONE), &app);
            assert!(
                matches!(
                    intent,
                    KeyIntent::Action(AppAction::SessionDeleteConfirmSubmit)
                ),
                "{code:?} should confirm"
            );
        }
        for code in [
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Char('q'),
            KeyCode::Char('Q'),
            KeyCode::Esc,
        ] {
            let intent = map_key(KeyEvent::new(code, KeyModifiers::NONE), &app);
            assert!(
                matches!(intent, KeyIntent::Action(AppAction::CloseModal)),
                "{code:?} should cancel"
            );
        }
    }

    #[test]
    fn agent_delete_confirm_matches_plan_confirm_key_set() {
        // All confirm dialogs speak the same dialect: Enter/y/Y confirm,
        // Esc/n/N cancel, q/Q mirrors Esc.
        let mut app = app();
        app.modal = Some(ModalKind::AgentDeleteConfirm {
            name: "explorer".to_string(),
            path: std::path::PathBuf::from(".bonsai/agents/explorer.md"),
        });

        for code in [KeyCode::Char('y'), KeyCode::Char('Y'), KeyCode::Enter] {
            let intent = map_key(KeyEvent::new(code, KeyModifiers::NONE), &app);
            assert!(
                matches!(
                    intent,
                    KeyIntent::Action(AppAction::AgentDeleteConfirmSubmit)
                ),
                "{code:?} should confirm"
            );
        }
        for code in [
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Char('q'),
            KeyCode::Char('Q'),
            KeyCode::Esc,
        ] {
            let intent = map_key(KeyEvent::new(code, KeyModifiers::NONE), &app);
            assert!(
                matches!(
                    intent,
                    KeyIntent::Action(AppAction::AgentDeleteConfirmCancel)
                ),
                "{code:?} should cancel"
            );
        }
    }

    #[test]
    fn model_picker_page_keys_move_by_page() {
        // The largest list in the app pages like every other picker.
        let mut app = app();
        app.modal = Some(ModalKind::ModelPicker {
            entries: Vec::new(),
        });

        let up = map_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &app);
        assert!(matches!(
            up,
            KeyIntent::Action(AppAction::ModelPickerMove(-8))
        ));
        let down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ModelPickerMove(8))
        ));
    }

    #[test]
    fn legacy_escape_i_does_not_emit_implement_plan() {
        // Documents the legacy VT100 path on terminals that don't speak
        // the keyboard protocol: crossterm surfaces `ESC i` as two
        // separate events with no ALT modifier. The first is a plain Esc
        // (Noop on empty input); the second is a plain `i` that the
        // input arm absorbs. The fix is at the terminal layer
        // (PushKeyboardEnhancementFlags in run.rs); this test pins the
        // legacy behaviour so a regression in the keymap that ate the
        // first ESC into ClearInput would surface.
        let app = input_app_with_text("");

        let esc = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(esc, KeyIntent::Noop));

        let i = map_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &app);
        assert!(matches!(i, KeyIntent::Insert('i')));
    }

    #[test]
    fn home_end_in_plan_focus_set_plan_scroll() {
        let mut app = app();
        app.view = View::Plan;
        app.focus = Focus::Plan;

        let intent = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetPlanScroll(u16::MAX))
        ));

        let intent = map_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetPlanScroll(0))
        ));

        let intent = map_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetPlanScroll(0))
        ));

        let intent = map_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE), &app);
        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetPlanScroll(u16::MAX))
        ));
    }

    #[test]
    fn completion_popover_cycles_and_dismisses() {
        let app = input_app_with_text("/he");
        assert!(app.completion.active, "typing /he should open completion");

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::CompletionMove(1))
        ));

        let esc = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(
            esc,
            KeyIntent::Action(AppAction::CompletionDismiss)
        ));
    }

    #[test]
    fn completion_page_keys_move_by_popover_chunk() {
        let app = input_app_with_text("/");
        assert!(app.completion.active, "typing / should open completion");

        let page_up = map_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_up,
            KeyIntent::Action(AppAction::CompletionMove(-5))
        ));

        let page_down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_down,
            KeyIntent::Action(AppAction::CompletionMove(5))
        ));
    }

    #[test]
    fn enter_submits_exact_model_shortcut_command_with_completion_open() {
        let mut app = input_app_with_text("/c");
        app.completion.active = true;
        app.completion.cursor = 0;
        app.completion.candidates = vec![CompletionCandidate::Command {
            label: "/clear".to_string(),
            description: "Clear conversation".to_string(),
        }];

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        assert!(matches!(enter, KeyIntent::Submit));
    }

    #[test]
    fn tab_accepts_selected_command_completion_without_submitting() {
        let mut app = input_app_with_text("/ct");
        select_command_completion(&mut app, "/ctx");

        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);

        assert!(matches!(
            tab,
            KeyIntent::Action(AppAction::CompleteInputTo(replacement)) if replacement == "/ctx"
        ));
    }

    #[test]
    fn tab_accepts_authorize_command_with_space_and_keeps_provider_completion() {
        let mut app = input_app_with_provider_choices("/");
        select_command_completion(&mut app, "/authorize");

        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);

        let KeyIntent::Action(AppAction::CompleteInputTo(replacement)) = tab else {
            panic!("expected Tab to accept /authorize into the composer");
        };
        assert_eq!(replacement, "/authorize ");

        app.reduce(AppAction::CompleteInputTo(replacement));

        assert!(
            app.completion.active,
            "provider argument completion should stay open"
        );
        assert!(app.completion.candidates.iter().any(|candidate| matches!(
            candidate,
            CompletionCandidate::Provider { id, display }
                if id == "opencode" && display == "OpenCode Go"
        )));
    }

    /// Regression: the trailing-space list used to be hardcoded and drifted —
    /// `/self-review` was missing, so accepting it and pressing Enter ran the
    /// bare status form instead of offering its modes.
    #[test]
    fn tab_accepts_self_review_with_space_and_offers_its_modes() {
        let mut app = input_app_with_text("/self");
        select_command_completion(&mut app, "/self-review");

        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);

        let KeyIntent::Action(AppAction::CompleteInputTo(replacement)) = tab else {
            panic!("expected Tab to accept /self-review into the composer");
        };
        assert_eq!(replacement, "/self-review ");

        app.reduce(AppAction::CompleteInputTo(replacement));

        assert!(
            app.completion.active,
            "self-review mode completion should open"
        );
        assert!(app.completion.candidates.iter().any(|candidate| matches!(
            candidate,
            CompletionCandidate::Argument { label, .. } if label == "on"
        )));
    }

    /// Every command whose argument popover exists must complete with the
    /// trailing space that lets it open — pinned to the registry usage hints
    /// so the two surfaces cannot drift apart again.
    #[test]
    fn every_command_with_a_usage_hint_completes_with_a_trailing_space() {
        for command in crate::commands::COMMANDS {
            let replacement = super::completion::command_tab_replacement(command.name);
            assert_eq!(
                replacement.ends_with(' '),
                command.usage_hint.is_some(),
                "{}: trailing space must track its usage hint",
                command.name
            );
        }
    }

    #[test]
    fn tab_accepts_sandbox_command_with_space_and_keeps_option_completion() {
        let mut app = input_app_with_text("/sand");
        select_command_completion(&mut app, "/sandbox");

        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);

        let KeyIntent::Action(AppAction::CompleteInputTo(replacement)) = tab else {
            panic!("expected Tab to accept /sandbox into the composer");
        };
        assert_eq!(replacement, "/sandbox ");

        app.reduce(AppAction::CompleteInputTo(replacement));

        assert!(
            app.completion.active,
            "sandbox option completion should stay open"
        );
        assert!(app.completion.candidates.iter().any(|candidate| matches!(
            candidate,
            CompletionCandidate::Argument { label, detail, replacement }
                if label == "status" && detail == "option" && replacement == "/sandbox status"
        )));
    }

    #[test]
    fn enter_submits_selected_command_completion() {
        let mut app = input_app_with_text("/");
        select_command_completion(&mut app, "/autonomy");

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        assert!(matches!(
            enter,
            KeyIntent::SubmitReplacement(replacement) if replacement == "/autonomy"
        ));
    }

    #[test]
    fn enter_submits_fully_typed_command_over_drifted_highlight() {
        // Typing an exact command name must run that command even when the
        // completion cursor sits on another candidate — cursor-follow across
        // refinement can leave the highlight on a description match ("bonsai"
        // appears in several command descriptions, so /bonsai reliably shows
        // this drift).
        let mut app = input_app_with_text("/bonsai");
        assert!(app.completion.active, "typing /bonsai opens completion");
        if let Some(other) = app.completion.candidates.iter().position(|candidate| {
            !matches!(
                candidate,
                CompletionCandidate::Command { label, .. } if label == "/bonsai"
            )
        }) {
            app.completion.cursor = other;
        }

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        assert!(
            matches!(enter, KeyIntent::Submit),
            "exact command should submit as typed, got {enter:?}"
        );
    }

    #[test]
    fn enter_submits_selected_authorize_command_without_trailing_space() {
        let mut app = input_app_with_provider_choices("/");
        select_command_completion(&mut app, "/authorize");

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        assert!(matches!(
            enter,
            KeyIntent::SubmitReplacement(replacement) if replacement == "/authorize"
        ));
    }

    #[test]
    fn enter_submits_authorize_provider_picker() {
        let mut app = app();
        app.modal = Some(ModalKind::AuthorizeProviderPicker {
            providers: vec![ProviderOption {
                provider_id: "opencode".to_string(),
                provider_label: "OpenCode Go".to_string(),
                authorized: false,
                current: false,
                uses_endpoint_auth_form: false,
            }],
            query: String::new(),
            cursor: 0,
        });
        app.focus = Focus::Modal;

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::AuthorizeProviderPickerSubmit)
        ));
    }

    #[test]
    fn authorize_provider_picker_letters_type_into_filter() {
        let mut app = app();
        app.modal = Some(ModalKind::AuthorizeProviderPicker {
            providers: Vec::new(),
            query: String::new(),
            cursor: 0,
        });
        app.focus = Focus::Modal;

        // `q` no longer closes — it types, like the plan/model pickers.
        let typed = map_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &app);
        assert!(matches!(
            typed,
            KeyIntent::Action(AppAction::AuthorizeProviderPickerInputChar('q'))
        ));
        let esc = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(esc, KeyIntent::Action(AppAction::CloseModal)));
    }

    #[test]
    fn enter_submits_unauthorize_provider_picker() {
        let mut app = app();
        app.modal = Some(ModalKind::UnauthorizeProviderPicker {
            providers: vec![ProviderOption {
                provider_id: "opencode".to_string(),
                provider_label: "OpenCode Go".to_string(),
                authorized: true,
                current: true,
                uses_endpoint_auth_form: false,
            }],
            query: String::new(),
            cursor: 0,
        });
        app.focus = Focus::Modal;

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::UnauthorizeProviderPickerSubmit)
        ));
    }

    #[test]
    fn unauthorize_provider_picker_letters_type_into_filter() {
        let mut app = app();
        app.modal = Some(ModalKind::UnauthorizeProviderPicker {
            providers: Vec::new(),
            query: String::new(),
            cursor: 0,
        });
        app.focus = Focus::Modal;

        // `q` types into the filter, mirroring the authorize picker.
        let typed = map_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &app);
        assert!(matches!(
            typed,
            KeyIntent::Action(AppAction::UnauthorizeProviderPickerInputChar('q'))
        ));
        let esc = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(esc, KeyIntent::Action(AppAction::CloseModal)));
    }

    #[test]
    fn unauthorize_confirm_keys_submit_or_close() {
        let mut app = app();
        app.modal = Some(ModalKind::UnauthorizeConfirm {
            provider_id: "opencode".to_string(),
            display_name: "OpenCode Go".to_string(),
        });
        app.focus = Focus::Modal;

        for code in [KeyCode::Enter, KeyCode::Char('y'), KeyCode::Char('Y')] {
            let intent = map_key(KeyEvent::new(code, KeyModifiers::NONE), &app);
            assert!(matches!(
                intent,
                KeyIntent::Action(AppAction::UnauthorizeConfirmSubmit)
            ));
        }
        for code in [
            KeyCode::Esc,
            KeyCode::Char('n'),
            KeyCode::Char('N'),
            KeyCode::Char('q'),
        ] {
            let intent = map_key(KeyEvent::new(code, KeyModifiers::NONE), &app);
            assert!(matches!(intent, KeyIntent::Action(AppAction::CloseModal)));
        }
    }

    #[test]
    fn provider_manager_slash_toggles_search_mode_routing() {
        use crate::tui::provider_manager::{ProviderManagerRow, ProviderOrigin};
        let manager = |filter: &str, searching: bool| {
            Some(ModalKind::ProviderManager {
                rows: vec![ProviderManagerRow {
                    connection_id: "qwencloud".to_string(),
                    display_name: "Qwen Cloud API".to_string(),
                    origin: ProviderOrigin::BuiltIn,
                    enabled: true,
                    authorized: false,
                    current: false,
                    model_count: 0,
                    discovery: crate::model_catalog::DiscoveryKind::Generic,
                    base_url: String::new(),
                    credential_label: None,
                    auth_hint: None,
                }],
                filter: filter.to_string(),
                searching,
                cursor: 0,
            })
        };
        let mut app = app();
        app.focus = Focus::Modal;

        // Idle: `d` is the delete shortcut, `/` begins search.
        app.modal = manager("", false);
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::ProviderManagerRemove)
        ));
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::ProviderManagerBeginSearch)
        ));

        // Searching: the same `d` now types into the filter; Esc leaves search.
        app.modal = manager("", true);
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::ProviderManagerSearchChar('d'))
        ));
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::ProviderManagerSearchExit)
        ));

        // Filter applied but not typing: Esc clears it before closing.
        app.modal = manager("qwen", false);
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::ProviderManagerClearFilter)
        ));
    }

    #[test]
    fn permissions_manager_slash_toggles_search_mode_routing() {
        use crate::tui::permissions_manager::{PermissionRuleRow, RuleLane};
        let manager = |filter: &str, searching: bool| {
            Some(ModalKind::PermissionsManager {
                rows: vec![PermissionRuleRow {
                    lane: RuleLane::Bash,
                    source: crate::permissions::RuleSource::Project,
                    pattern: "make *".to_string(),
                    permission: crate::permissions::Permission::Allow,
                    id: Some(1),
                }],
                filter: filter.to_string(),
                searching,
                cursor: 0,
            })
        };
        let mut app = app();
        app.focus = Focus::Modal;

        // Idle: `d` deletes the selected rule, `/` begins search.
        app.modal = manager("", false);
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::PermissionsManagerDelete)
        ));
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::PermissionsManagerBeginSearch)
        ));

        // Searching: the same `d` now types into the filter; Esc leaves search.
        app.modal = manager("", true);
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::PermissionsManagerSearchChar('d'))
        ));
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::PermissionsManagerSearchExit)
        ));

        // Filter applied but not typing: Esc clears it before closing.
        app.modal = manager("make", false);
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::PermissionsManagerClearFilter)
        ));
    }

    #[test]
    fn local_model_wizard_keys_route_to_wizard_actions() {
        let mut app = app();
        app.modal = Some(ModalKind::LocalModelWizard {
            state: Box::default(),
        });
        app.focus = Focus::Modal;

        let typed = map_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &app);
        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);
        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        let space = map_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &app);

        assert!(matches!(
            typed,
            KeyIntent::Action(AppAction::LocalModelWizardInputChar('q'))
        ));
        assert!(matches!(
            tab,
            KeyIntent::Action(AppAction::LocalModelWizardMoveField(1))
        ));
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::LocalModelWizardSubmit)
        ));
        assert!(matches!(
            space,
            KeyIntent::Action(AppAction::LocalModelWizardToggle)
        ));
    }

    #[test]
    fn tab_accepts_selected_provider_completion_with_id() {
        let mut app = input_app_with_provider_choices("/authorize ");
        select_provider_completion(&mut app, "opencode");

        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);

        assert!(matches!(
            tab,
            KeyIntent::Action(AppAction::CompleteInputTo(replacement))
                if replacement == "/authorize opencode"
        ));
    }

    #[test]
    fn tab_accepts_model_completion_with_id() {
        let mut app = input_app_with_text("/model ");
        app.cached_model_choices = vec![ModelOption {
            provider_id: "opencode".to_string(),
            connection_id: "opencode".to_string(),
            provider_label: "OpenCode Go".to_string(),
            model_id: Some("opencode/qwen3.7-max".to_string()),
            model: "qwen3.7-max".to_string(),
            display_name: "qwen3.7-max".to_string(),
            reasoning: ReasoningSelection::default(),
            recommended_reasoning: None,
            discouraged_reasoning: Vec::new(),
            supported_reasoning: Vec::new(),
            shortcut_bindings: Vec::new(),
            parameter_preview: "default parameters".to_string(),
            pricing: None,
            context_window: None,
            features: Vec::new(),
            metadata_sources: crate::model_catalog::ResolvedModelMetadataSources::default(),
            catalog_drift: Vec::new(),
            unverified: false,
        }];
        app.reduce(AppAction::InputChar('q'));

        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);

        assert!(matches!(
            tab,
            KeyIntent::Action(AppAction::CompleteInputTo(replacement))
                if replacement == "/model opencode:qwen3.7-max"
        ));
    }

    #[test]
    fn enter_submits_when_selected_completion_matches_input() {
        let app = input_app_with_text("/copy");
        assert!(
            app.completion.active,
            "exact command should keep completion open"
        );

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        // A fully-typed command submits as typed (KeyIntent::Submit), which
        // the Submit arm reads back from the composer — same outcome as the
        // old SubmitReplacement path, but immune to highlight drift.
        assert!(matches!(enter, KeyIntent::Submit), "got {enter:?}");
    }

    #[test]
    fn enter_accepts_selected_path_completion_when_it_changes_token() {
        let mut app = input_app_with_text("read @src/ma");
        let start = app.input().find('@').expect("test input has mention");
        let end = app.input().len();
        set_path_completion(&mut app, "src/main.rs", start, end);

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::CompleteInputRange { start: actual_start, end: actual_end, replacement })
                if actual_start == start && actual_end == end && replacement == "@src/main.rs"
        ));
    }

    #[test]
    fn enter_submits_when_selected_path_completion_matches_token() {
        let mut app = input_app_with_text("read @src/main.rs");
        let start = app.input().find('@').expect("test input has mention");
        let end = app.input().len();
        set_path_completion(&mut app, "src/main.rs", start, end);

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        assert!(matches!(enter, KeyIntent::Submit));
    }

    #[test]
    fn esc_clears_input_when_no_completion_is_open() {
        let app = input_app_with_text("plain text");

        let intent = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);

        assert!(matches!(intent, KeyIntent::Action(AppAction::ClearInput)));
    }

    #[test]
    fn esc_in_agent_view_transcript_focus_returns_to_input() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Transcript;

        let intent = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Input))
        ));
    }

    #[test]
    fn esc_in_agent_view_transcript_focus_with_focused_item_returns_to_input() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Transcript;
        app.transcript_focus = Some(0);

        let intent = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Input))
        ));
    }

    #[test]
    fn esc_in_agent_view_todo_focus_returns_to_input() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Todo;

        let intent = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Input))
        ));
    }

    #[test]
    fn esc_in_plan_view_transcript_focus_still_clears_transcript_focus() {
        let mut app = app();
        app.view = View::Plan;
        app.focus = Focus::Transcript;
        app.transcript_focus = Some(0);

        let intent = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Input))
        ));
    }

    #[test]
    fn esc_in_plan_view_plan_focus_returns_to_input() {
        let mut app = app();
        app.view = View::Plan;
        app.focus = Focus::Plan;

        let intent = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::SetFocus(Focus::Input))
        ));
    }

    #[test]
    fn left_right_browse_phases_only_when_phased() {
        let mut app = app();
        app.focus = Focus::Todo;

        // No phases: Left/Right are inert on the todo card.
        let left = map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &app);
        assert!(matches!(left, KeyIntent::Noop), "{left:?}");
        let right = map_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &app);
        assert!(matches!(right, KeyIntent::Noop), "{right:?}");

        // Two phases: Left/Right browse phases.
        app.plan = crate::plan::PlanDoc {
            phases: vec![
                crate::plan::PlanPhase {
                    name: "a".to_string(),
                    tasks: Vec::new(),
                },
                crate::plan::PlanPhase {
                    name: "b".to_string(),
                    tasks: Vec::new(),
                },
            ],
            ..Default::default()
        };
        let left = map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &app);
        assert!(
            matches!(left, KeyIntent::Action(AppAction::MoveTodoPhase(-1))),
            "{left:?}"
        );
        let right = map_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &app);
        assert!(
            matches!(right, KeyIntent::Action(AppAction::MoveTodoPhase(1))),
            "{right:?}"
        );
    }

    #[test]
    fn esc_cancels_question_prompt() {
        let mut app = app();
        app.modal = Some(ModalKind::QuestionPrompt {
            request_id: 1,
            prompt: "pick".to_string(),
            header: None,
            options: vec![QuestionOption {
                label: "Yes".to_string(),
                description: String::new(),
                preselected: false,
            }],
            multiple: false,
            origin: None,
            cursor: 0,
            selected: vec![false],
        });

        let intent = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::QuestionCancel)
        ));
    }

    #[test]
    fn page_up_down_in_composer_scroll_instead_of_transcript() {
        let app = input_app_with_text("a multiline\ndraft message\nspanning a few\nrows");

        let up = map_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &app);
        assert!(matches!(up, KeyIntent::Action(AppAction::ComposerPage(-1))));

        let down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ComposerPage(1))
        ));
    }

    #[test]
    fn shift_page_in_composer_extends_selection() {
        let app = input_app_with_text("a multiline\ndraft message\nspanning a few\nrows");

        let up = map_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT), &app);
        assert!(matches!(
            up,
            KeyIntent::Action(AppAction::ExtendComposerByPage(-1))
        ));

        let down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ExtendComposerByPage(1))
        ));
    }

    #[test]
    fn ctrl_home_scrolls_composer_ctrl_end_jumps_transcript() {
        let app = input_app_with_text("a multiline\ndraft message\nspanning a few\nrows");

        // Ctrl+Home still scrolls the composer's draft overflow to the top.
        let home = map_key(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL), &app);
        assert!(matches!(
            home,
            KeyIntent::Action(AppAction::SetComposerScroll(0))
        ));

        // Ctrl+End is now a global jump-to-latest for the transcript, so it
        // fires even from the composer (where plain End edits the draft).
        let end = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL), &app);
        assert!(matches!(end, KeyIntent::Action(AppAction::ScrollBottom)));
    }

    #[test]
    fn ctrl_end_jumps_transcript_from_any_focus() {
        for focus in [Focus::Input, Focus::Transcript, Focus::Todo, Focus::Plan] {
            let mut app = app();
            app.focus = focus;
            let end = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL), &app);
            assert!(
                matches!(end, KeyIntent::Action(AppAction::ScrollBottom)),
                "Ctrl+End should jump the transcript to latest from {focus:?}"
            );
        }
    }

    #[test]
    fn page_keys_outside_composer_still_scroll_transcript() {
        let mut app = app();
        app.focus = Focus::Transcript;

        let up = map_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &app);
        // Transcript scroll via scroll_action(-8).
        assert!(matches!(
            up,
            KeyIntent::Action(AppAction::ScrollCurrent(-8))
        ));

        let down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ScrollCurrent(8))
        ));
    }

    #[test]
    fn context_modal_keys_route_to_tree_navigation() {
        let mut app = app();
        app.modal = Some(ModalKind::Context(Box::new(context_report())));
        app.focus = Focus::Modal;

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(down, KeyIntent::Action(AppAction::ContextMove(1))));

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::ContextToggleSelected)
        ));

        let right = map_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &app);
        assert!(matches!(
            right,
            KeyIntent::Action(AppAction::ContextExpandSelected)
        ));
    }

    #[test]
    fn episodes_modal_keys_select_rows_and_scroll_details() {
        let mut app = app();
        app.modal = Some(ModalKind::Episodes {
            report: Box::new(context_report()),
            cursor: 0,
        });
        app.focus = Focus::Modal;

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::EpisodesMove(1))
        ));

        let page_down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_down,
            KeyIntent::Action(AppAction::ScrollModal(8))
        ));

        let close = map_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &app);
        assert!(matches!(close, KeyIntent::Action(AppAction::CloseModal)));

        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::EpisodesMove(1))
        ));
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::EpisodesMove(-1))
        ));
    }

    #[test]
    fn provider_detail_uses_arrow_keys_for_scrolling() {
        assert!(matches!(
            map_provider_detail_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            KeyIntent::Action(AppAction::ScrollModal(-1))
        ));
        assert!(matches!(
            map_provider_detail_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            KeyIntent::Action(AppAction::ScrollModal(1))
        ));
        assert!(matches!(
            map_provider_detail_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
            KeyIntent::Action(AppAction::ScrollModal(1))
        ));
        assert!(matches!(
            map_provider_detail_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
            KeyIntent::Action(AppAction::ScrollModal(-1))
        ));
    }

    #[test]
    fn context_wire_keys_route_to_modal_scrolling() {
        let mut app = app();
        app.modal = Some(ModalKind::Context(Box::new(context_report())));
        app.focus = Focus::Modal;
        app.context_state.view_mode = ContextViewMode::Wire;

        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        assert!(matches!(up, KeyIntent::Action(AppAction::ContextMove(-1))));

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(down, KeyIntent::Action(AppAction::ContextMove(1))));

        let page_down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_down,
            KeyIntent::Action(AppAction::ScrollModal(8))
        ));

        let home = map_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &app);
        assert!(matches!(
            home,
            KeyIntent::Action(AppAction::ContextMove(i16::MIN))
        ));

        let end = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &app);
        assert!(matches!(
            end,
            KeyIntent::Action(AppAction::ContextMove(i16::MAX))
        ));

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::ContextToggleSelected)
        ));

        let pin = map_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &app);
        assert!(matches!(pin, KeyIntent::Noop));
    }

    #[test]
    fn context_turns_keys_route_like_wire() {
        let mut app = app();
        app.modal = Some(ModalKind::Context(Box::new(context_report())));
        app.focus = Focus::Modal;
        app.context_state.view_mode = ContextViewMode::Turns;

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(down, KeyIntent::Action(AppAction::ContextMove(1))));

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::ContextToggleSelected)
        ));

        let cycle = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);
        assert!(matches!(
            cycle,
            KeyIntent::Action(AppAction::ContextToggleView)
        ));

        let legacy = map_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), &app);
        assert!(matches!(legacy, KeyIntent::Noop));

        // Context controls stay ledger-only.
        for control in ['p', 'd', 's', 'r'] {
            let key = map_key(
                KeyEvent::new(KeyCode::Char(control), KeyModifiers::NONE),
                &app,
            );
            assert!(matches!(key, KeyIntent::Noop), "{control} should be inert");
        }
    }

    #[test]
    fn sandbox_modal_keys_route_to_picker_and_global_jump() {
        let mut app = app();
        app.modal = Some(ModalKind::SandboxStatus { cursor: 0 });
        app.focus = Focus::Modal;

        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        assert!(matches!(up, KeyIntent::Action(AppAction::SandboxMove(-1))));

        let page_down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_down,
            KeyIntent::Action(AppAction::SandboxMove(8))
        ));

        let home = map_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &app);
        assert!(matches!(
            home,
            KeyIntent::Action(AppAction::SandboxMove(i16::MIN))
        ));

        let end = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &app);
        assert!(matches!(
            end,
            KeyIntent::Action(AppAction::SandboxMove(i16::MAX))
        ));

        let ctrl_end = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL), &app);
        assert!(matches!(
            ctrl_end,
            KeyIntent::Action(AppAction::ScrollBottom)
        ));
    }

    #[test]
    fn mcp_modal_keys_route_to_server_navigation_and_detail_scroll() {
        let mut app = app();
        app.modal = Some(ModalKind::McpServers {
            rows: Vec::new(),
            cursor: 0,
        });
        app.focus = Focus::Modal;

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::McpServersMove(1))
        ));

        let page_down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_down,
            KeyIntent::Action(AppAction::ScrollModal(8))
        ));

        let space = map_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &app);
        assert!(matches!(
            space,
            KeyIntent::Action(AppAction::McpServersToggle)
        ));

        let reload = map_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &app);
        assert!(matches!(
            reload,
            KeyIntent::Action(AppAction::McpServersReload)
        ));

        let authorize = map_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &app);
        assert!(matches!(
            authorize,
            KeyIntent::Action(AppAction::McpServersAuthorize)
        ));

        let home = map_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &app);
        assert!(matches!(
            home,
            KeyIntent::Action(AppAction::McpServersMove(i16::MIN))
        ));

        let esc = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(esc, KeyIntent::Action(AppAction::CloseModal)));
    }

    #[test]
    fn settings_keys_route_move_cycle_and_activate() {
        let mut app = app();
        app.modal = Some(ModalKind::Settings {
            rows: Vec::new(),
            cursor: 0,
        });
        app.focus = Focus::Modal;

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::SettingsMove(1))
        ));

        let left = map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &app);
        assert!(matches!(
            left,
            KeyIntent::Action(AppAction::SettingsCycle(-1))
        ));

        let right = map_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &app);
        assert!(matches!(
            right,
            KeyIntent::Action(AppAction::SettingsCycle(1))
        ));

        let space = map_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &app);
        assert!(matches!(
            space,
            KeyIntent::Action(AppAction::SettingsActivate)
        ));

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::SettingsActivate)
        ));

        let esc = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(esc, KeyIntent::Action(AppAction::CloseModal)));
    }

    #[test]
    fn onboarding_keys_choose_submit_or_defer() {
        let mut app = app();
        app.modal = Some(ModalKind::Onboarding {
            step: crate::onboarding::FirstRunStep::CredentialStorage,
            cursor: 0,
        });
        app.focus = Focus::Modal;

        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::OnboardingMove(1))
        ));
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::OnboardingSubmit)
        ));
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::CloseModal)
        ));
    }

    #[test]
    fn skill_manager_keys_route_move_scroll_toggle_and_load() {
        let mut app = app();
        app.modal = Some(ModalKind::SkillManager {
            rows: Vec::new(),
            cursor: 0,
        });
        app.focus = Focus::Modal;

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::SkillManagerMove(1))
        ));

        // PageDown scrolls the detail pane, not the list selection.
        let page_down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_down,
            KeyIntent::Action(AppAction::ScrollModal(8))
        ));

        let space = map_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &app);
        assert!(matches!(
            space,
            KeyIntent::Action(AppAction::SkillManagerToggleDisabled)
        ));

        let load = map_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &app);
        assert!(matches!(
            load,
            KeyIntent::Action(AppAction::SkillManagerToggleLoaded)
        ));

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::SkillManagerToggleLoaded)
        ));

        let esc = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(esc, KeyIntent::Action(AppAction::CloseModal)));
    }

    #[test]
    fn todo_focus_scroll_keys_route_to_todo() {
        let mut app = app();
        app.focus = Focus::Todo;

        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        assert!(matches!(
            up,
            KeyIntent::Action(AppAction::ScrollSidebar(-1))
        ));

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ScrollSidebar(1))
        ));

        let page_up = map_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_up,
            KeyIntent::Action(AppAction::ScrollSidebar(-8))
        ));

        let page_down = map_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), &app);
        assert!(matches!(
            page_down,
            KeyIntent::Action(AppAction::ScrollSidebar(8))
        ));

        let home = map_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &app);
        assert!(matches!(
            home,
            KeyIntent::Action(AppAction::SetSidebarScroll(0))
        ));

        let end = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &app);
        assert!(matches!(
            end,
            KeyIntent::Action(AppAction::SetSidebarScroll(u16::MAX))
        ));
    }

    #[test]
    fn enter_with_todo_focus_is_noop() {
        let mut app = app();
        app.focus = Focus::Todo;

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);

        assert!(matches!(enter, KeyIntent::Noop));
    }

    /// `q` (and Shift+q) must close every modal that does not capture it for
    /// a different action (permission/question prompts and the three
    /// text-input pickers). This helper walks the matrix; the assertion is
    /// the same in every branch so the test reads as a single invariant.
    fn assert_q_closes_modal(modal: ModalKind) {
        let mut state = app();
        state.modal = Some(modal);

        let lower = map_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &state,
        );
        assert!(
            matches!(lower, KeyIntent::Action(AppAction::CloseModal)),
            "expected `q` to close the modal, got {lower:?}",
        );

        let upper = map_key(
            KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE),
            &state,
        );
        assert!(
            matches!(upper, KeyIntent::Action(AppAction::CloseModal)),
            "expected `Q` to close the modal, got {upper:?}",
        );
    }

    #[test]
    fn q_closes_every_modal_via_fallthrough() {
        // Modal kinds that flow through the fallthrough arm because they
        // have no per-modal `if matches!` block in `map_key`. `Confirm` is
        // dead-coded (no constructor) so it is excluded.
        assert_q_closes_modal(ModalKind::Help);
        assert_q_closes_modal(ModalKind::CommandHelp);
        assert_q_closes_modal(ModalKind::ToolDetail {
            tool_id: "call-1".to_string(),
        });
        assert_q_closes_modal(ModalKind::BlockDetail { item_index: 0 });
        assert_q_closes_modal(ModalKind::DiffPreview {
            tool_id: "call-1".to_string(),
        });

        // Modal kinds that have their own per-modal arm but delegate `q` to
        // CloseModal so the binding works the same as Esc. PlanPicker and
        // AuthorizeProviderPicker are deliberately excluded: their per-modal
        // arms own a search box, so `q` must type into the query (mirrors
        // ModelPicker and ApiKeyPrompt).
        assert_q_closes_modal(ModalKind::SessionPicker {
            sessions: Vec::new(),
            cursor: 0,
        });
        assert_q_closes_modal(ModalKind::PlanOpenChoice {
            plan: saved_plan(1, "Plan library"),
            cursor: 0,
        });
        assert_q_closes_modal(ModalKind::PlanDeleteConfirm {
            plan: saved_plan(1, "Plan library"),
        });
        assert_q_closes_modal(ModalKind::ReviewScopePicker { cursor: 0 });
        assert_q_closes_modal(ModalKind::TaskList {
            tasks: Vec::new(),
            cursor: 0,
        });
        assert_q_closes_modal(ModalKind::PeerList {
            peers: Vec::new(),
            cursor: 0,
        });
        assert_q_closes_modal(ModalKind::Context(Box::new(context_report())));
    }

    #[test]
    fn peer_list_arrows_move_and_page() {
        let mut state = app();
        state.modal = Some(ModalKind::PeerList {
            peers: Vec::new(),
            cursor: 0,
        });
        let intent = |code| map_key(KeyEvent::new(code, KeyModifiers::NONE), &state);
        assert!(matches!(
            intent(KeyCode::Down),
            KeyIntent::Action(AppAction::PeerListMove(1))
        ));
        assert!(matches!(
            intent(KeyCode::Up),
            KeyIntent::Action(AppAction::PeerListMove(-1))
        ));
        assert!(matches!(
            intent(KeyCode::PageDown),
            KeyIntent::Action(AppAction::PeerListMove(8))
        ));
        assert!(matches!(
            intent(KeyCode::PageUp),
            KeyIntent::Action(AppAction::PeerListMove(-8))
        ));
    }

    #[test]
    fn q_does_not_close_permission_prompt() {
        // `q` mirrors Esc on a permission prompt: deny the bash command
        // rather than close the modal and leave the invocation half-allowed.
        let mut app = app();
        app.modal = Some(ModalKind::PermissionPrompt {
            request_id: 1,
            command: "rm -rf /".to_string(),
            origin: None,
        });

        let intent = map_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::PromptDecision {
                family: PromptFamily::Permission,
                decision: PromptDecision::Deny,
            })
        ));
    }

    #[test]
    fn q_does_not_close_question_prompt() {
        // `q` mirrors Esc on a question prompt: cancel the question rather
        // than close the modal and leave the request unanswered.
        let mut app = app();
        app.modal = Some(ModalKind::QuestionPrompt {
            request_id: 1,
            prompt: "pick".to_string(),
            header: None,
            options: vec![QuestionOption {
                label: "Yes".to_string(),
                description: String::new(),
                preselected: false,
            }],
            multiple: false,
            origin: None,
            cursor: 0,
            selected: vec![false],
        });

        let intent = map_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &app);

        assert!(matches!(
            intent,
            KeyIntent::Action(AppAction::QuestionCancel)
        ));
    }

    #[test]
    fn shift_q_also_closes_modal() {
        // Terminals deliver Shift+q as KeyCode::Char('Q') with the SHIFT
        // modifier. The fallthrough arm matches both cases so the binding
        // works the same regardless of which key the terminal surfaces.
        let mut app = app();
        app.modal = Some(ModalKind::Context(Box::new(context_report())));

        let intent = map_key(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT), &app);

        assert!(matches!(intent, KeyIntent::Action(AppAction::CloseModal)));
    }

    #[test]
    fn q_inside_input_still_types_q() {
        // Regression: the new modal-close binding must not leak into the
        // composer. When no modal is open and focus is Input, `q` is
        // absorbed by the per-char insert arm and turns into Insert('q').
        let app = input_app_with_text("");

        let intent = map_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &app);

        assert!(matches!(intent, KeyIntent::Insert('q')));
    }

    #[test]
    fn q_types_into_text_input_modals() {
        // The three pickers that own a text field must keep `q` typing
        // rather than closing the modal, otherwise users lose the ability
        // to filter the list.
        let mut api_state = app();
        api_state.modal = Some(ModalKind::ApiKeyPrompt {
            provider_id: "opencode".to_string(),
            initial_form: None,
        });
        let api = map_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &api_state,
        );
        assert!(matches!(
            api,
            KeyIntent::Action(AppAction::ApiKeyInputChar('q'))
        ));

        let mut model_state = app();
        model_state.modal = Some(ModalKind::ModelPicker {
            entries: Vec::new(),
        });
        let model = map_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &model_state,
        );
        assert!(matches!(
            model,
            KeyIntent::Action(AppAction::ModelPickerInputChar('q'))
        ));

        let mut plan_state = app();
        plan_state.modal = Some(ModalKind::PlanPicker {
            plans: Vec::new(),
            query: String::new(),
            cursor: 0,
        });
        let plan = map_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
            &plan_state,
        );
        assert!(matches!(
            plan,
            KeyIntent::Action(AppAction::PlanPickerInputChar('q'))
        ));
    }

    #[test]
    fn agent_composer_model_field_is_picker_only() {
        let mut state = crate::tui::agent_composer::AgentComposerState::new();
        state.field = crate::tui::agent_composer::AgentComposerField::Model;
        let mut app = app();
        app.modal = Some(ModalKind::AgentComposer {
            state: Box::new(state),
        });

        for code in [KeyCode::Enter, KeyCode::Char(' ')] {
            let intent = map_key(KeyEvent::new(code, KeyModifiers::NONE), &app);
            assert!(matches!(
                intent,
                KeyIntent::Action(AppAction::AgentComposerOpenModelPicker)
            ));
        }

        let letter = map_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), &app);
        assert!(matches!(letter, KeyIntent::Noop));

        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);
        assert!(matches!(
            tab,
            KeyIntent::Action(AppAction::AgentComposerNextPage)
        ));
    }

    #[test]
    fn model_picker_shortcut_keys_assign_only_in_reasoning_pane() {
        let mut app = app();
        app.modal = Some(ModalKind::ModelPicker {
            entries: Vec::new(),
        });

        // Default (Model) pane: bare letters type into the filter so common
        // search input keeps working.
        app.model_picker.active_pane = ModelPickerPane::Model;
        for ch in ['c', 'f', 's', 'z'] {
            let intent = map_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &app);
            assert!(matches!(
                intent,
                KeyIntent::Action(AppAction::ModelPickerInputChar(input)) if input == ch
            ));
        }

        // Reasoning pane: bare letters assign the selected model (plus the
        // reasoning row under the cursor) to that shortcut.
        app.model_picker.active_pane = ModelPickerPane::Reasoning;
        for ch in ['c', 'f', 's', 'z'] {
            let key = crate::model_role::ModelShortcutKey::new(ch).unwrap();
            let intent = map_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &app);
            assert!(matches!(
                intent,
                KeyIntent::Action(AppAction::ModelPickerAssignShortcut(assigned)) if assigned == key
            ));
        }

        // Non-letter chars still type into the filter, even in the Reasoning pane.
        let other = map_key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE), &app);
        assert!(matches!(
            other,
            KeyIntent::Action(AppAction::ModelPickerInputChar('-'))
        ));

        // Alt is not a shortcut modifier: Alt+c is plain filter input everywhere.
        let alt_c = map_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT), &app);
        assert!(matches!(
            alt_c,
            KeyIntent::Action(AppAction::ModelPickerInputChar('c'))
        ));
    }

    #[test]
    fn tab_and_arrows_move_between_endpoint_auth_fields() {
        let mut app = app();
        // Endpoint-form providers are catalog connections now; the form flag
        // rides on the provider choices seeded by `/authorize`.
        app.provider_choices = vec![crate::tui::pickers::ProviderOption {
            provider_id: "local-endpoint".to_string(),
            provider_label: "Local Endpoint".to_string(),
            authorized: false,
            current: false,
            uses_endpoint_auth_form: true,
        }];
        app.modal = Some(ModalKind::ApiKeyPrompt {
            provider_id: "local-endpoint".to_string(),
            initial_form: None,
        });

        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);
        assert!(matches!(
            tab,
            KeyIntent::Action(AppAction::ApiKeyInputMoveField(1))
        ));
        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ApiKeyInputMoveField(1))
        ));

        let back_tab = map_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &app);
        assert!(matches!(
            back_tab,
            KeyIntent::Action(AppAction::ApiKeyInputMoveField(-1))
        ));
        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        assert!(matches!(
            up,
            KeyIntent::Action(AppAction::ApiKeyInputMoveField(-1))
        ));
    }

    #[test]
    fn ctrl_q_does_not_quit_when_modal_open() {
        // Regression: Ctrl+q is the global Quit binding (line 289). When a
        // modal is open the keymap must short-circuit through the modal
        // arm so Ctrl+q reaches the modal's intent path (Noop for the
        // catch-all). Pin this so a future refactor that reorders the
        // bottom match before the modal checks does not silently let
        // Ctrl+q quit out of an open dialog.
        let mut app = app();
        app.modal = Some(ModalKind::Context(Box::new(context_report())));

        let intent = map_key(
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL),
            &app,
        );

        assert!(matches!(intent, KeyIntent::Action(AppAction::CloseModal)));
    }

    #[test]
    fn model_picker_left_right_move_panes_from_every_pane() {
        let mut app = app();
        app.modal = Some(ModalKind::ModelPicker {
            entries: Vec::new(),
        });

        // On the default Model pane, Left/Right move between panes.
        app.model_picker.active_pane = ModelPickerPane::Model;
        let left = map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &app);
        let right = map_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &app);
        assert!(matches!(
            left,
            KeyIntent::Action(AppAction::ModelPickerMovePane(-1))
        ));
        assert!(matches!(
            right,
            KeyIntent::Action(AppAction::ModelPickerMovePane(1))
        ));

        // On the Reasoning pane, Left/Right also move panes (not cycle reasoning choices);
        // Up/Down handle variant cycling within the pane.
        app.model_picker.active_pane = ModelPickerPane::Reasoning;
        let left = map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &app);
        let right = map_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &app);
        assert!(matches!(
            left,
            KeyIntent::Action(AppAction::ModelPickerMovePane(-1))
        ));
        assert!(matches!(
            right,
            KeyIntent::Action(AppAction::ModelPickerMovePane(1))
        ));
        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            up,
            KeyIntent::Action(AppAction::ModelPickerMove(-1))
        ));
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ModelPickerMove(1))
        ));

        // Tab still advances panes from the Reasoning pane (wraps to Provider).
        let tab = map_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &app);
        assert!(matches!(
            tab,
            KeyIntent::Action(AppAction::ModelPickerMovePane(1))
        ));
    }

    #[test]
    fn mode_picker_keys_move_cycle_close() {
        let mut app = app();
        app.modal = Some(ModalKind::ModePicker {
            rows: Vec::new(),
            cursor: 0,
        });
        app.focus = Focus::Modal;

        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        assert!(matches!(
            up,
            KeyIntent::Action(AppAction::ModePickerMove(-1))
        ));

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ModePickerMove(1))
        ));

        let right = map_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &app);
        assert!(matches!(
            right,
            KeyIntent::Action(AppAction::ModePickerCycle(1))
        ));

        let left = map_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &app);
        assert!(matches!(
            left,
            KeyIntent::Action(AppAction::ModePickerCycle(-1))
        ));

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(enter, KeyIntent::Action(AppAction::CloseModal)));

        let esc = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(esc, KeyIntent::Action(AppAction::CloseModal)));

        let q = map_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &app);
        assert!(matches!(q, KeyIntent::Action(AppAction::CloseModal)));

        let home = map_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &app);
        assert!(matches!(
            home,
            KeyIntent::Action(AppAction::ModePickerMove(i16::MIN))
        ));

        let end = map_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &app);
        assert!(matches!(
            end,
            KeyIntent::Action(AppAction::ModePickerMove(i16::MAX))
        ));
    }

    #[test]
    fn theme_tab_completion_reaches_export_subcommand() {
        // No built-in theme starts with "exp", so this only completes if the
        // Tab path includes the `export` subcommand (regression for the popover
        // and Tab paths diverging).
        let app = input_app_with_text("/theme exp");
        assert_eq!(
            complete_command_arg_from_state("/theme exp", &app).as_deref(),
            Some("/theme export")
        );
    }

    #[test]
    fn theme_picker_keys_move_submit_and_cancel() {
        let mut app = app();
        app.modal = Some(ModalKind::ThemePicker {
            cursor: 0,
            original_theme: "forest".to_string(),
        });
        app.focus = Focus::Modal;

        let up = map_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &app);
        assert!(matches!(
            up,
            KeyIntent::Action(AppAction::ThemePickerMove(-1))
        ));

        let down = map_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &app);
        assert!(matches!(
            down,
            KeyIntent::Action(AppAction::ThemePickerMove(1))
        ));

        let enter = map_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &app);
        assert!(matches!(
            enter,
            KeyIntent::Action(AppAction::ThemePickerSubmit)
        ));

        let esc = map_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &app);
        assert!(matches!(
            esc,
            KeyIntent::Action(AppAction::ThemePickerCancel)
        ));

        let q = map_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &app);
        assert!(matches!(q, KeyIntent::Action(AppAction::ThemePickerCancel)));
    }
}
