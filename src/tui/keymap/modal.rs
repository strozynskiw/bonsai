//! Key mappers for modal/picker surfaces (permission & question prompts, the
//! API-key prompt, provider/model/session/plan pickers, the local-model wizard,
//! the task list, and the context/generic modals). Split out of `keymap` to keep
//! the dispatcher and primary-key handling readable; `map_key` in the parent
//! routes to these. Self-contained: they depend only on the event types and
//! `KeyIntent`, never on the parent's primary-key/completion helpers.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::app::{AppState, ModelPickerPane};
use crate::tui::event::{
    AppAction, ModalKind, PromptDecision, PromptFamily, SubtaskListPane, UsageTab,
};
use crate::tui::memory_manager::{MemoryWizardField, MemoryWizardStep};

use super::KeyIntent;

/// Per-family key differences for the shared approval-prompt keymap. Only two
/// families deviate from the common shape, and only in these two ways.
struct PromptKeySpec {
    /// `m`/`M` as an extra alias for the session tier — the permission
    /// prompt's "allow matching" wording.
    session_matching_alias: bool,
    /// Whether the prompt offers a project tier. Sandbox escapes are never
    /// persisted, so that family deliberately has no project option.
    offers_project: bool,
}

const fn prompt_key_spec(family: PromptFamily) -> PromptKeySpec {
    match family {
        PromptFamily::Permission => PromptKeySpec {
            session_matching_alias: true,
            offers_project: true,
        },
        PromptFamily::SandboxEscalation => PromptKeySpec {
            session_matching_alias: false,
            offers_project: false,
        },
        PromptFamily::WebDomain | PromptFamily::ExtensionTool | PromptFamily::HookTrust => {
            PromptKeySpec {
                session_matching_alias: false,
                offers_project: true,
            }
        }
    }
}

/// Shared keymap for the five approval prompts (permission, sandbox
/// escalation, web domain, extension tool, hook trust): a/Enter allow once,
/// s allows for the session, p for the project (where offered), d denies.
///
/// q mirrors Esc — deny the pending request rather than close the modal
/// silently and leave the requesting tool call hung on the interaction.
pub(super) fn map_prompt_decision_key(family: PromptFamily, key: KeyEvent) -> KeyIntent {
    let spec = prompt_key_spec(family);
    let decide = |decision| KeyIntent::Action(AppAction::PromptDecision { family, decision });
    match key.code {
        KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Enter => {
            decide(PromptDecision::AllowOnce)
        }
        KeyCode::Char('m') | KeyCode::Char('M') if spec.session_matching_alias => {
            decide(PromptDecision::AllowSession)
        }
        KeyCode::Char('s') | KeyCode::Char('S') => decide(PromptDecision::AllowSession),
        KeyCode::Char('p') | KeyCode::Char('P') if spec.offers_project => {
            decide(PromptDecision::AllowProject)
        }
        KeyCode::Char('d') | KeyCode::Char('D') | KeyCode::Esc => decide(PromptDecision::Deny),
        KeyCode::Char('q') | KeyCode::Char('Q') => decide(PromptDecision::Deny),
        // Scroll the (possibly long) command/URL body when it wraps past the
        // viewport. The action hints stay pinned at the bottom.
        KeyCode::Up | KeyCode::PageUp => {
            KeyIntent::Action(AppAction::ScrollModal(if matches!(key.code, KeyCode::Up) {
                -1
            } else {
                -8
            }))
        }
        KeyCode::Down | KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(
            if matches!(key.code, KeyCode::Down) {
                1
            } else {
                8
            },
        )),
        KeyCode::Home => KeyIntent::Action(AppAction::ScrollModal(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ScrollModal(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_question_prompt_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::QuestionCancel),
        // q mirrors Esc — cancel the multi-select question rather than
        // closing the modal silently and leaving the request unanswered.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::QuestionCancel),
        KeyCode::Enter => KeyIntent::Action(AppAction::QuestionSubmit),
        KeyCode::Char(' ') => KeyIntent::Action(AppAction::QuestionToggle),
        KeyCode::Up => KeyIntent::Action(AppAction::QuestionMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::QuestionMove(1)),
        // Scroll the question body when it overflows the viewport.
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ScrollModal(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ScrollModal(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_api_key_prompt_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            KeyIntent::Action(AppAction::ApiKeyPersistenceToggle)
        }
        KeyCode::Enter => KeyIntent::Action(AppAction::ApiKeyInputSubmit),
        KeyCode::Down | KeyCode::Tab if app.uses_endpoint_auth_form() => {
            KeyIntent::Action(AppAction::ApiKeyInputMoveField(1))
        }
        KeyCode::Up | KeyCode::BackTab if app.uses_endpoint_auth_form() => {
            KeyIntent::Action(AppAction::ApiKeyInputMoveField(-1))
        }
        KeyCode::Backspace => KeyIntent::Action(AppAction::ApiKeyInputBackspace),
        KeyCode::Char(ch) => KeyIntent::Action(AppAction::ApiKeyInputChar(ch)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_authorize_provider_picker_key(key: KeyEvent) -> KeyIntent {
    // Type-to-filter, like the plan picker: printable keys build the search
    // query, so Esc (not `q`) is the close key.
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter => KeyIntent::Action(AppAction::AuthorizeProviderPickerSubmit),
        KeyCode::Backspace => KeyIntent::Action(AppAction::AuthorizeProviderPickerInputBackspace),
        KeyCode::Up => KeyIntent::Action(AppAction::AuthorizeProviderPickerMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::AuthorizeProviderPickerMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::AuthorizeProviderPickerMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::AuthorizeProviderPickerMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::AuthorizeProviderPickerMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::AuthorizeProviderPickerMove(i16::MAX)),
        KeyCode::Char(ch) => KeyIntent::Action(AppAction::AuthorizeProviderPickerInputChar(ch)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_review_scope_picker_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        // q mirrors Esc — close the picker without submitting a scope.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter => KeyIntent::Action(AppAction::ReviewScopePickerSubmit),
        KeyCode::Up => KeyIntent::Action(AppAction::ReviewScopePickerMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::ReviewScopePickerMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ReviewScopePickerMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ReviewScopePickerMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ReviewScopePickerMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ReviewScopePickerMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

fn wizard_choice_field_active(app: &AppState) -> bool {
    matches!(
        app.modal,
        Some(ModalKind::LocalModelWizard { ref state }) if state.setup_field_is_choice()
    )
}

pub(super) fn map_local_model_wizard_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter => KeyIntent::Action(AppAction::LocalModelWizardSubmit),
        KeyCode::Tab => KeyIntent::Action(AppAction::LocalModelWizardMoveField(1)),
        KeyCode::BackTab => KeyIntent::Action(AppAction::LocalModelWizardMoveField(-1)),
        // On a Setup choice field (Server preset, Store key) the horizontal
        // arrows cycle the options; elsewhere they keep their step semantics.
        KeyCode::Left => {
            if wizard_choice_field_active(app) {
                KeyIntent::Action(AppAction::LocalModelWizardCycleChoice(-1))
            } else {
                KeyIntent::Action(AppAction::LocalModelWizardBack)
            }
        }
        KeyCode::Right => {
            if wizard_choice_field_active(app) {
                KeyIntent::Action(AppAction::LocalModelWizardCycleChoice(1))
            } else {
                KeyIntent::Action(AppAction::LocalModelWizardMoveField(1))
            }
        }
        KeyCode::Up => {
            if matches!(
                app.modal,
                Some(ModalKind::LocalModelWizard { ref state })
                    if matches!(state.step, crate::tui::local_model_wizard::LocalModelWizardStep::Setup)
            ) {
                KeyIntent::Action(AppAction::LocalModelWizardMoveField(-1))
            } else {
                KeyIntent::Action(AppAction::LocalModelWizardMoveModel(-1))
            }
        }
        KeyCode::Down => {
            if matches!(
                app.modal,
                Some(ModalKind::LocalModelWizard { ref state })
                    if matches!(state.step, crate::tui::local_model_wizard::LocalModelWizardStep::Setup)
            ) {
                KeyIntent::Action(AppAction::LocalModelWizardMoveField(1))
            } else {
                KeyIntent::Action(AppAction::LocalModelWizardMoveModel(1))
            }
        }
        KeyCode::PageUp => KeyIntent::Action(AppAction::LocalModelWizardMoveModel(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::LocalModelWizardMoveModel(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::LocalModelWizardMoveModel(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::LocalModelWizardMoveModel(i16::MAX)),
        KeyCode::Backspace => KeyIntent::Action(AppAction::LocalModelWizardBackspace),
        KeyCode::Char(' ') => KeyIntent::Action(AppAction::LocalModelWizardToggle),
        KeyCode::Char(ch) => KeyIntent::Action(AppAction::LocalModelWizardInputChar(ch)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_agent_browser_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Up => KeyIntent::Action(AppAction::AgentBrowserMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::AgentBrowserMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::AgentBrowserMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::AgentBrowserMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::AgentBrowserMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::AgentBrowserMove(i16::MAX)),
        KeyCode::Char('a') => KeyIntent::Action(AppAction::AgentBrowserAdd),
        KeyCode::Char('e') | KeyCode::Enter => KeyIntent::Action(AppAction::AgentBrowserEdit),
        KeyCode::Char('d') | KeyCode::Delete => KeyIntent::Action(AppAction::AgentBrowserDelete),
        KeyCode::Char(' ') | KeyCode::Char('t') => {
            KeyIntent::Action(AppAction::AgentBrowserToggleEnabled)
        }
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_provider_manager_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    let (searching, filter_active) = match &app.modal {
        Some(ModalKind::ProviderManager {
            searching, filter, ..
        }) => (*searching, !filter.is_empty()),
        _ => (false, false),
    };

    // While typing a filter, bare letters build the query, so the `a`/`e`/`d`
    // management shortcuts are suspended and Esc leaves search (keeping the
    // filter applied) rather than closing.
    if searching {
        return match key.code {
            KeyCode::Esc => KeyIntent::Action(AppAction::ProviderManagerSearchExit),
            KeyCode::Enter => KeyIntent::Action(AppAction::ProviderManagerShowDetail),
            KeyCode::Backspace => KeyIntent::Action(AppAction::ProviderManagerSearchBackspace),
            KeyCode::Up => KeyIntent::Action(AppAction::ProviderManagerMove(-1)),
            KeyCode::Down => KeyIntent::Action(AppAction::ProviderManagerMove(1)),
            KeyCode::PageUp => KeyIntent::Action(AppAction::ProviderManagerMove(-8)),
            KeyCode::PageDown => KeyIntent::Action(AppAction::ProviderManagerMove(8)),
            KeyCode::Home => KeyIntent::Action(AppAction::ProviderManagerMove(i16::MIN)),
            KeyCode::End => KeyIntent::Action(AppAction::ProviderManagerMove(i16::MAX)),
            KeyCode::Char(ch) => KeyIntent::Action(AppAction::ProviderManagerSearchChar(ch)),
            _ => KeyIntent::Noop,
        };
    }

    match key.code {
        // Three-step Esc: clear an applied filter first, then close.
        KeyCode::Esc if filter_active => KeyIntent::Action(AppAction::ProviderManagerClearFilter),
        KeyCode::Esc | KeyCode::Char('q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Char('/') => KeyIntent::Action(AppAction::ProviderManagerBeginSearch),
        KeyCode::Up => KeyIntent::Action(AppAction::ProviderManagerMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::ProviderManagerMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ProviderManagerMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ProviderManagerMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ProviderManagerMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ProviderManagerMove(i16::MAX)),
        KeyCode::Char('a') => KeyIntent::Action(AppAction::ProviderManagerAdd),
        KeyCode::Enter => KeyIntent::Action(AppAction::ProviderManagerShowDetail),
        KeyCode::Char('e') => KeyIntent::Action(AppAction::ProviderManagerEdit),
        KeyCode::Char('d') | KeyCode::Delete => KeyIntent::Action(AppAction::ProviderManagerRemove),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_provider_detail_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => KeyIntent::Action(AppAction::ProviderDetailBack),
        // The detail body scrolls; there is no list selection to move here.
        KeyCode::Up | KeyCode::Char('k') => KeyIntent::Action(AppAction::ScrollModal(-1)),
        KeyCode::Down | KeyCode::Char('j') => KeyIntent::Action(AppAction::ScrollModal(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ScrollModal(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ScrollModal(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_provider_remove_confirm_key(key: KeyEvent) -> KeyIntent {
    // Same key set as the agent/plan delete confirms: Enter/y/Y confirm,
    // Esc/n/N/q cancel.
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            KeyIntent::Action(AppAction::ProviderRemoveConfirmSubmit)
        }
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
            KeyIntent::Action(AppAction::ProviderRemoveConfirmCancel)
        }
        KeyCode::Char('q') | KeyCode::Char('Q') => {
            KeyIntent::Action(AppAction::ProviderRemoveConfirmCancel)
        }
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_skill_manager_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => KeyIntent::Action(AppAction::CloseModal),
        // Up/Down move the list selection; the detail pane scrolls with PageUp/Down.
        KeyCode::Up => KeyIntent::Action(AppAction::SkillManagerMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::SkillManagerMove(1)),
        KeyCode::Home => KeyIntent::Action(AppAction::SkillManagerMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::SkillManagerMove(i16::MAX)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        KeyCode::Char(' ') => KeyIntent::Action(AppAction::SkillManagerToggleDisabled),
        KeyCode::Char('l') | KeyCode::Enter => {
            KeyIntent::Action(AppAction::SkillManagerToggleLoaded)
        }
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_memory_manager_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Up => KeyIntent::Action(AppAction::MemoryManagerMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::MemoryManagerMove(1)),
        KeyCode::Home => KeyIntent::Action(AppAction::MemoryManagerMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::MemoryManagerMove(i16::MAX)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        KeyCode::Char(' ') => KeyIntent::Action(AppAction::MemoryManagerToggleEnabled),
        KeyCode::Char('a') => KeyIntent::Action(AppAction::MemoryManagerAdd),
        KeyCode::Char('e') | KeyCode::Enter => KeyIntent::Action(AppAction::MemoryManagerEdit),
        KeyCode::Char('d') | KeyCode::Delete => KeyIntent::Action(AppAction::MemoryManagerDelete),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_memory_add_wizard_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    let Some(ModalKind::MemoryAddWizard { state }) = &app.modal else {
        return KeyIntent::Noop;
    };
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::BackTab => KeyIntent::Action(AppAction::MemoryAddWizardBack),
        KeyCode::Tab => KeyIntent::Action(AppAction::MemoryAddWizardSubmit),
        KeyCode::Enter if matches!(state.step, MemoryWizardStep::Body) => {
            KeyIntent::Action(AppAction::MemoryAddWizardInputChar('\n'))
        }
        KeyCode::Enter => KeyIntent::Action(AppAction::MemoryAddWizardSubmit),
        KeyCode::Up if matches!(state.step, MemoryWizardStep::Details) => {
            KeyIntent::Action(AppAction::MemoryAddWizardMoveField(-1))
        }
        KeyCode::Down if matches!(state.step, MemoryWizardStep::Details) => {
            KeyIntent::Action(AppAction::MemoryAddWizardMoveField(1))
        }
        KeyCode::Left if matches!(state.step, MemoryWizardStep::Details) => {
            KeyIntent::Action(AppAction::MemoryAddWizardCycleValue(-1))
        }
        KeyCode::Right if matches!(state.step, MemoryWizardStep::Details) => {
            KeyIntent::Action(AppAction::MemoryAddWizardCycleValue(1))
        }
        KeyCode::Char(' ') if matches!(state.step, MemoryWizardStep::Details) => {
            match state.field {
                MemoryWizardField::Enabled => {
                    KeyIntent::Action(AppAction::MemoryAddWizardToggleValue)
                }
                MemoryWizardField::Tier | MemoryWizardField::Type => {
                    KeyIntent::Action(AppAction::MemoryAddWizardCycleValue(1))
                }
                MemoryWizardField::Description => {
                    KeyIntent::Action(AppAction::MemoryAddWizardInputChar(' '))
                }
            }
        }
        KeyCode::Char(ch)
            if matches!(ch, 'q' | 'Q')
                && (matches!(state.step, MemoryWizardStep::Body)
                    || matches!(state.field, MemoryWizardField::Description)) =>
        {
            KeyIntent::Action(AppAction::MemoryAddWizardInputChar(ch))
        }
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Backspace => KeyIntent::Action(AppAction::MemoryAddWizardBackspace),
        KeyCode::Char(ch) => KeyIntent::Action(AppAction::MemoryAddWizardInputChar(ch)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_agent_composer_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    use crate::tui::agent_composer::{AgentComposerField, AgentComposerStep, CursorMotion};

    let Some(ModalKind::AgentComposer { state }) = &app.modal else {
        return KeyIntent::Noop;
    };
    // Ctrl+G drafts a prompt with the selected model (runtime guards the step).
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g')) {
        return KeyIntent::Action(AppAction::AgentComposerGenerate);
    }

    // Which editing context the focused field is in.
    let is_prompt = matches!(state.step, AgentComposerStep::Prompt);
    let is_details = matches!(state.step, AgentComposerStep::Details);
    let is_tools = matches!(state.step, AgentComposerStep::Tools);
    // A focused text field (Name/Description on Details, or the Prompt body).
    let is_text = is_prompt || (is_details && state.field.is_text());
    let on_value = is_details && state.field.is_value();
    let on_model = is_details
        && matches!(
            state.field,
            AgentComposerField::Model | AgentComposerField::BackupModel
        );

    let cursor = |motion| KeyIntent::Action(AppAction::AgentComposerCursor(motion));

    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        // The Prompt is a multi-line editor: Enter inserts a newline there.
        // Everywhere else Enter advances the stage, and on the Review preview it
        // saves the agent.
        KeyCode::Enter if is_prompt => KeyIntent::Action(AppAction::AgentComposerInsertNewline),
        KeyCode::Enter if on_model => KeyIntent::Action(AppAction::AgentComposerOpenModelPicker),
        KeyCode::Enter => KeyIntent::Action(AppAction::AgentComposerSubmit),
        // Tab / Shift+Tab move between stages; fields navigate with Up/Down. The
        // final stage (Review) has nowhere to advance — Enter saves it.
        KeyCode::Tab if matches!(state.step, AgentComposerStep::Review) => KeyIntent::Noop,
        KeyCode::Tab => KeyIntent::Action(AppAction::AgentComposerNextPage),
        KeyCode::BackTab => KeyIntent::Action(AppAction::AgentComposerBack),
        // Space mirrors Enter on a model row.
        KeyCode::Char(' ') if on_model => {
            KeyIntent::Action(AppAction::AgentComposerOpenModelPicker)
        }
        // `d` clears the focused model-chain slot (primary -> inherit parent,
        // backup -> no backup / failover disabled).
        KeyCode::Char('d') if on_model => KeyIntent::Action(AppAction::AgentComposerDeleteModel),
        // In a text field the caret moves; on a value field Left/Right cycle.
        KeyCode::Left if is_text => cursor(CursorMotion::Left),
        KeyCode::Right if is_text => cursor(CursorMotion::Right),
        KeyCode::Left if on_value => KeyIntent::Action(AppAction::AgentComposerToggle(-1)),
        KeyCode::Right if on_value => KeyIntent::Action(AppAction::AgentComposerToggle(1)),
        // Up/Down: caret rows in the Prompt; field / tool navigation elsewhere.
        KeyCode::Up if is_prompt => cursor(CursorMotion::Up),
        KeyCode::Down if is_prompt => cursor(CursorMotion::Down),
        KeyCode::Up if is_details || is_tools => {
            KeyIntent::Action(AppAction::AgentComposerMove(-1))
        }
        KeyCode::Down if is_details || is_tools => {
            KeyIntent::Action(AppAction::AgentComposerMove(1))
        }
        KeyCode::PageUp if is_tools => KeyIntent::Action(AppAction::AgentComposerMove(-8)),
        KeyCode::PageDown if is_tools => KeyIntent::Action(AppAction::AgentComposerMove(8)),
        KeyCode::Home if is_text => cursor(CursorMotion::Home),
        KeyCode::End if is_text => cursor(CursorMotion::End),
        KeyCode::Backspace => KeyIntent::Action(AppAction::AgentComposerBackspace),
        KeyCode::Delete if is_text => KeyIntent::Action(AppAction::AgentComposerDeleteForward),
        // Space toggles a tool row / cycles a value field, or types in text.
        KeyCode::Char(' ') if is_text => KeyIntent::Action(AppAction::AgentComposerInputChar(' ')),
        KeyCode::Char(' ') if on_value || is_tools => {
            KeyIntent::Action(AppAction::AgentComposerToggle(1))
        }
        KeyCode::Char(ch) if is_text => KeyIntent::Action(AppAction::AgentComposerInputChar(ch)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_agent_delete_confirm_key(key: KeyEvent) -> KeyIntent {
    // Same key set as the plan delete/discard confirms: Enter/y/Y confirm,
    // Esc/n/N cancel, q/Q mirrors Esc.
    match key.code {
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            KeyIntent::Action(AppAction::AgentDeleteConfirmSubmit)
        }
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
            KeyIntent::Action(AppAction::AgentDeleteConfirmCancel)
        }
        KeyCode::Char('q') | KeyCode::Char('Q') => {
            KeyIntent::Action(AppAction::AgentDeleteConfirmCancel)
        }
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_model_picker_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter => KeyIntent::Action(AppAction::ModelPickerSubmit),
        KeyCode::Backspace => KeyIntent::Action(AppAction::ModelPickerInputBackspace),
        KeyCode::Up => KeyIntent::Action(AppAction::ModelPickerMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::ModelPickerMove(1)),
        KeyCode::Left => KeyIntent::Action(AppAction::ModelPickerMovePane(-1)),
        KeyCode::Right | KeyCode::Tab => KeyIntent::Action(AppAction::ModelPickerMovePane(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ModelPickerMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ModelPickerMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ModelPickerMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ModelPickerMove(i16::MAX)),
        // With the Reasoning pane focused, bare letters assign model shortcuts
        // instead of typing into the filter. In every other pane they stay
        // filter input, so common letters keep working as search.
        KeyCode::Char(ch)
            if app.model_picker.active_pane == ModelPickerPane::Reasoning
                && !key.modifiers.intersects(
                    KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER,
                ) =>
        {
            match crate::model_role::ModelShortcutKey::new(ch) {
                Some(key) => KeyIntent::Action(AppAction::ModelPickerAssignShortcut(key)),
                None => KeyIntent::Action(AppAction::ModelPickerInputChar(ch)),
            }
        }
        KeyCode::Char(ch) => KeyIntent::Action(AppAction::ModelPickerInputChar(ch)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_session_picker_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        // q mirrors Esc — close the picker without selecting a session.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter => KeyIntent::Action(AppAction::SessionPickerSubmit),
        KeyCode::Up => KeyIntent::Action(AppAction::SessionPickerMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::SessionPickerMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::SessionPickerMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::SessionPickerMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::SessionPickerMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::SessionPickerMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_plan_picker_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter => KeyIntent::Action(AppAction::PlanPickerSubmit),
        KeyCode::Backspace => KeyIntent::Action(AppAction::PlanPickerInputBackspace),
        KeyCode::Delete => KeyIntent::Action(AppAction::PlanPickerDeleteSelected),
        KeyCode::Up => KeyIntent::Action(AppAction::PlanPickerMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::PlanPickerMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::PlanPickerMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::PlanPickerMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::PlanPickerMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::PlanPickerMove(i16::MAX)),
        KeyCode::Char(ch) => KeyIntent::Action(AppAction::PlanPickerInputChar(ch)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_plan_open_choice_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        // q mirrors Esc — close the choice prompt without opening a plan.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter => KeyIntent::Action(AppAction::PlanOpenChoiceSubmit),
        KeyCode::Up => KeyIntent::Action(AppAction::PlanOpenChoiceMove(-1)),
        KeyCode::Down | KeyCode::Tab => KeyIntent::Action(AppAction::PlanOpenChoiceMove(1)),
        KeyCode::Home => KeyIntent::Action(AppAction::PlanOpenChoiceMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::PlanOpenChoiceMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_start_plan_choice_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
            KeyIntent::Action(AppAction::CloseModal)
        }
        KeyCode::Enter => KeyIntent::Action(AppAction::StartPlanChoiceSubmit),
        KeyCode::Up => KeyIntent::Action(AppAction::StartPlanChoiceMove(-1)),
        KeyCode::Down | KeyCode::Tab => KeyIntent::Action(AppAction::StartPlanChoiceMove(1)),
        KeyCode::Home => KeyIntent::Action(AppAction::StartPlanChoiceMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::StartPlanChoiceMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_plan_delete_confirm_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
            KeyIntent::Action(AppAction::CloseModal)
        }
        // q mirrors Esc — close the confirm prompt without deleting the plan.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            KeyIntent::Action(AppAction::PlanDeleteConfirmSubmit)
        }
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_plan_discard_confirm_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
            KeyIntent::Action(AppAction::CloseModal)
        }
        // q mirrors Esc — close the confirm prompt without discarding the plan.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
            KeyIntent::Action(AppAction::PlanDiscardConfirmSubmit)
        }
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_task_list_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        // q mirrors Esc — close the task list without taking action.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Up => KeyIntent::Action(AppAction::TaskListMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::TaskListMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::TaskListMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::TaskListMove(i16::MAX)),
        KeyCode::Delete => KeyIntent::Action(AppAction::TaskListDeleteSelected),
        _ => KeyIntent::Noop,
    }
}

/// The `/peers` view keymap: view-only, so only movement and the
/// shared close keys — no per-row action.
pub(super) fn map_peer_list_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        // q mirrors Esc — the /peers view is read-only.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Up => KeyIntent::Action(AppAction::PeerListMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::PeerListMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::PeerListMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::PeerListMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::PeerListMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::PeerListMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

/// The `/doctor` view keymap: read-only diagnostics, so only movement and the
/// shared close keys — Enter also closes since there is no per-row action.
pub(super) fn map_doctor_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Up => KeyIntent::Action(AppAction::DoctorMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::DoctorMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::DoctorMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::DoctorMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::DoctorMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::DoctorMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

/// `/refresh` modal keymap: view-only status report with arrow navigation.
/// Esc/Enter/q close; Up/Down move the selection.
pub(super) fn map_refresh_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Enter => KeyIntent::Action(AppAction::RefreshClose),
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::RefreshClose),
        KeyCode::Up => KeyIntent::Action(AppAction::RefreshMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::RefreshMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::RefreshMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::RefreshMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::RefreshMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::RefreshMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

/// The `/episodes` view is read-only: arrows select an entry and page keys
/// scroll the selected entry's detail pane.
pub(super) fn map_episodes_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
            KeyIntent::Action(AppAction::CloseModal)
        }
        KeyCode::Up | KeyCode::Char('k') => KeyIntent::Action(AppAction::EpisodesMove(-1)),
        KeyCode::Down | KeyCode::Char('j') => KeyIntent::Action(AppAction::EpisodesMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::EpisodesMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::EpisodesMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

/// The `/usage` dashboard keymap: tab switching plus generic scroll/close.
pub(super) fn map_usage_dashboard_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        // q mirrors Esc — the dashboard is read-only.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => {
            KeyIntent::Action(AppAction::UsageDashboardCycleTab(1))
        }
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
            KeyIntent::Action(AppAction::UsageDashboardCycleTab(-1))
        }
        KeyCode::Char(digit @ '1'..='6') => {
            let index = digit as usize - '1' as usize;
            KeyIntent::Action(AppAction::UsageDashboardSelectTab(UsageTab::ALL[index]))
        }
        KeyCode::Up => KeyIntent::Action(AppAction::ScrollModal(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::ScrollModal(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ScrollModal(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ScrollModal(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_subtask_list_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    let pane = match app.modal.as_ref() {
        Some(ModalKind::SubtaskList { pane, .. }) => *pane,
        _ => SubtaskListPane::List,
    };

    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        // q mirrors Esc — close the subtask list without taking action.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Tab | KeyCode::BackTab => KeyIntent::Action(AppAction::SubtaskListTogglePane),
        KeyCode::Enter | KeyCode::Right if pane == SubtaskListPane::List => {
            KeyIntent::Action(AppAction::SubtaskListSetPane(SubtaskListPane::Detail))
        }
        KeyCode::Left if pane == SubtaskListPane::Detail => {
            KeyIntent::Action(AppAction::SubtaskListSetPane(SubtaskListPane::List))
        }
        KeyCode::Up if pane == SubtaskListPane::Detail => {
            KeyIntent::Action(AppAction::ScrollModal(-1))
        }
        KeyCode::Down if pane == SubtaskListPane::Detail => {
            KeyIntent::Action(AppAction::ScrollModal(1))
        }
        KeyCode::PageUp if pane == SubtaskListPane::Detail => {
            KeyIntent::Action(AppAction::ScrollModal(-8))
        }
        KeyCode::PageDown if pane == SubtaskListPane::Detail => {
            KeyIntent::Action(AppAction::ScrollModal(8))
        }
        KeyCode::Home if pane == SubtaskListPane::Detail => {
            KeyIntent::Action(AppAction::ScrollModal(i16::MIN))
        }
        KeyCode::End if pane == SubtaskListPane::Detail => {
            KeyIntent::Action(AppAction::ScrollModal(i16::MAX))
        }
        // `m` overrides the selected agent's model/effort for future runs.
        KeyCode::Char('m') | KeyCode::Char('M') => {
            KeyIntent::Action(AppAction::SubtaskOverrideModel)
        }
        // `d` clears that override — back to the default (inherited) model.
        KeyCode::Char('d') | KeyCode::Char('D') => {
            KeyIntent::Action(AppAction::SubtaskClearModelOverride)
        }
        KeyCode::Up => KeyIntent::Action(AppAction::SubtaskListMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::SubtaskListMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::SubtaskListMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::SubtaskListMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::SubtaskListMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::SubtaskListMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_context_modal_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    // Wire and Turns are read-only views: p/d/s/r context controls stay
    // ledger-only, everything else routes identically.
    if !app.context_state.view_mode.is_ledger() {
        return match key.code {
            KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
            // q mirrors Esc — close the context inspector without toggling
            // any node in the tree.
            KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
            KeyCode::Tab | KeyCode::BackTab => KeyIntent::Action(AppAction::ContextToggleView),
            KeyCode::Enter => KeyIntent::Action(AppAction::ContextToggleSelected),
            KeyCode::Right => KeyIntent::Action(AppAction::ContextExpandSelected),
            KeyCode::Left => KeyIntent::Action(AppAction::ContextCollapseSelected),
            KeyCode::Up => KeyIntent::Action(AppAction::ContextMove(-1)),
            KeyCode::Down => KeyIntent::Action(AppAction::ContextMove(1)),
            KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
            KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
            KeyCode::Home => KeyIntent::Action(AppAction::ContextMove(i16::MIN)),
            KeyCode::End => KeyIntent::Action(AppAction::ContextMove(i16::MAX)),
            _ => KeyIntent::Noop,
        };
    }
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        // q mirrors Esc — close the context inspector without toggling
        // any node in the tree.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Char('p') | KeyCode::Char('P') => KeyIntent::Action(AppAction::ContextPinSelected),
        KeyCode::Char('d') | KeyCode::Char('D') => {
            KeyIntent::Action(AppAction::ContextDropSelected)
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            KeyIntent::Action(AppAction::ContextStubSelected)
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            KeyIntent::Action(AppAction::ContextRestoreSelected)
        }
        KeyCode::Tab | KeyCode::BackTab => KeyIntent::Action(AppAction::ContextToggleView),
        KeyCode::Enter => KeyIntent::Action(AppAction::ContextToggleSelected),
        KeyCode::Right => KeyIntent::Action(AppAction::ContextExpandSelected),
        KeyCode::Left => KeyIntent::Action(AppAction::ContextCollapseSelected),
        KeyCode::Up => KeyIntent::Action(AppAction::ContextMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::ContextMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ContextMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ContextMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_sandbox_modal_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::End if key.modifiers == KeyModifiers::CONTROL => {
            KeyIntent::Action(AppAction::ScrollBottom)
        }
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Up => KeyIntent::Action(AppAction::SandboxMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::SandboxMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::SandboxMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::SandboxMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::SandboxMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::SandboxMove(i16::MAX)),
        KeyCode::Left | KeyCode::Right => KeyIntent::Action(AppAction::SandboxToggle),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_mcp_servers_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Char(' ') => KeyIntent::Action(AppAction::McpServersToggle),
        KeyCode::Char('r') | KeyCode::Char('R') => KeyIntent::Action(AppAction::McpServersReload),
        KeyCode::Char('a') | KeyCode::Char('A') => {
            KeyIntent::Action(AppAction::McpServersAuthorize)
        }
        KeyCode::Up => KeyIntent::Action(AppAction::McpServersMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::McpServersMove(1)),
        KeyCode::Home => KeyIntent::Action(AppAction::McpServersMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::McpServersMove(i16::MAX)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_theme_picker_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::ThemePickerCancel),
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::ThemePickerCancel),
        KeyCode::Enter => KeyIntent::Action(AppAction::ThemePickerSubmit),
        KeyCode::Up => KeyIntent::Action(AppAction::ThemePickerMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::ThemePickerMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ThemePickerMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ThemePickerMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ThemePickerMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ThemePickerMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

/// Posture picker keymap: Up/Down move between value rows (skipping the
/// non-selectable axis headers), Left/Right cycle the focused value, and Enter
/// cycles it forward. Cycling applies live through the shared holder, so there
/// is no separate "apply" step. Esc/q close.
pub(super) fn map_mode_picker_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        // q mirrors Esc — close the picker without changing any axis.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Up => KeyIntent::Action(AppAction::ModePickerMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::ModePickerMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ModePickerMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ModePickerMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ModePickerMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ModePickerMove(i16::MAX)),
        // Left cycles backward and Right advances the focused value.
        KeyCode::Left => KeyIntent::Action(AppAction::ModePickerCycle(-1)),
        KeyCode::Right => KeyIntent::Action(AppAction::ModePickerCycle(1)),
        KeyCode::Enter => KeyIntent::Action(AppAction::CloseModal),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_settings_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
            KeyIntent::Action(AppAction::CloseModal)
        }
        KeyCode::Up => KeyIntent::Action(AppAction::SettingsMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::SettingsMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::SettingsMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::SettingsMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::SettingsMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::SettingsMove(i16::MAX)),
        // Left/Right cycle a Choice; Space and Enter activate the row (cycle a
        // Choice forward, or open the picker for a Model/Theme Action).
        KeyCode::Left => KeyIntent::Action(AppAction::SettingsCycle(-1)),
        KeyCode::Right => KeyIntent::Action(AppAction::SettingsCycle(1)),
        KeyCode::Char(' ') | KeyCode::Enter => KeyIntent::Action(AppAction::SettingsActivate),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_onboarding_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Up => KeyIntent::Action(AppAction::OnboardingMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::OnboardingMove(1)),
        KeyCode::Home => KeyIntent::Action(AppAction::OnboardingMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::OnboardingMove(i16::MAX)),
        KeyCode::Enter => KeyIntent::Action(AppAction::OnboardingSubmit),
        _ => KeyIntent::Noop,
    }
}

pub(super) fn map_busy_command_key(key: KeyEvent) -> KeyIntent {
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter => KeyIntent::Action(AppAction::BusyCommandSubmit),
        KeyCode::Up => KeyIntent::Action(AppAction::BusyCommandMove(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::BusyCommandMove(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::BusyCommandMove(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::BusyCommandMove(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::BusyCommandMove(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::BusyCommandMove(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

/// Findings detail modal: ←/→ (or `[`/`]`) move between findings, Enter jumps
/// to the focused finding's source evidence when it still resolves in the
/// transcript (else closes), Up/Down scroll, Esc/q close.
pub(super) fn map_plan_finding_detail_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
            KeyIntent::Action(AppAction::CloseModal)
        }
        KeyCode::Left | KeyCode::Char('[') => KeyIntent::Action(AppAction::CyclePlanFinding(-1)),
        KeyCode::Right | KeyCode::Char(']') => KeyIntent::Action(AppAction::CyclePlanFinding(1)),
        KeyCode::Enter => {
            if focused_finding_has_resolvable_evidence(app) {
                KeyIntent::Action(AppAction::OpenFindingEvidence)
            } else {
                KeyIntent::Action(AppAction::CloseModal)
            }
        }
        KeyCode::Up => KeyIntent::Action(AppAction::ScrollModal(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::ScrollModal(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ScrollModal(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ScrollModal(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

fn focused_finding_has_resolvable_evidence(app: &AppState) -> bool {
    let Some(ModalKind::PlanFindingDetail { index }) = app.modal else {
        return false;
    };
    app.plan
        .findings_in_severity_order()
        .get(index)
        .is_some_and(|finding| {
            finding
                .source_ids
                .iter()
                .any(|id| app.tool_activity(id).is_some())
        })
}

/// Fallback for the simple scrollable modals (Help, ToolDetail, BlockDetail,
/// DiffPreview, Confirm). Modals that handle their own keys intercept them in
/// `map_modal_key` before this is reached.
pub(super) fn map_generic_modal_key(key: KeyEvent, app: &AppState) -> KeyIntent {
    if matches!(app.modal, Some(ModalKind::ToolDetail { .. }))
        && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
        && let Some(ModalKind::ToolDetail { tool_id }) = app.modal.clone()
    {
        return KeyIntent::Action(AppAction::OpenDiffPreview(tool_id));
    }
    match key.code {
        KeyCode::Esc => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Enter => KeyIntent::Action(AppAction::CloseModal),
        // q mirrors Esc — close the modal (Help, ToolDetail, BlockDetail,
        // DiffPreview, Confirm). Modals that handle `q` themselves
        // (PermissionPrompt, QuestionPrompt, text-input pickers) intercept
        // the key before this fallthrough fires.
        KeyCode::Char('q') | KeyCode::Char('Q') => KeyIntent::Action(AppAction::CloseModal),
        KeyCode::Up => KeyIntent::Action(AppAction::ScrollModal(-1)),
        KeyCode::Down => KeyIntent::Action(AppAction::ScrollModal(1)),
        KeyCode::PageUp => KeyIntent::Action(AppAction::ScrollModal(-8)),
        KeyCode::PageDown => KeyIntent::Action(AppAction::ScrollModal(8)),
        KeyCode::Home => KeyIntent::Action(AppAction::ScrollModal(i16::MIN)),
        KeyCode::End => KeyIntent::Action(AppAction::ScrollModal(i16::MAX)),
        _ => KeyIntent::Noop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::memory_manager::{MemoryAddWizardState, MemoryWizardField, MemoryWizardStep};

    fn wizard_app(step: MemoryWizardStep, field: MemoryWizardField) -> AppState {
        let mut app = AppState::new("codex", "model".to_string(), "workspace".to_string(), None);
        app.modal = Some(ModalKind::MemoryAddWizard {
            state: Box::new(MemoryAddWizardState {
                step,
                field,
                ..MemoryAddWizardState::default()
            }),
        });
        app
    }

    #[test]
    fn memory_wizard_accepts_q_while_editing_text() {
        let body_app = wizard_app(MemoryWizardStep::Body, MemoryWizardField::Description);
        assert!(matches!(
            map_memory_add_wizard_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                &body_app
            ),
            KeyIntent::Action(AppAction::MemoryAddWizardInputChar('q'))
        ));

        let details_app = wizard_app(MemoryWizardStep::Details, MemoryWizardField::Description);
        assert!(matches!(
            map_memory_add_wizard_key(
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
                &details_app
            ),
            KeyIntent::Action(AppAction::MemoryAddWizardInputChar('q'))
        ));
    }

    #[test]
    fn memory_wizard_still_closes_q_outside_text_editing() {
        let app = wizard_app(MemoryWizardStep::Details, MemoryWizardField::Tier);
        assert!(matches!(
            map_memory_add_wizard_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &app),
            KeyIntent::Action(AppAction::CloseModal)
        ));
    }

    #[test]
    fn memory_manager_e_and_enter_open_edit() {
        for code in [KeyCode::Char('e'), KeyCode::Enter] {
            assert!(matches!(
                map_memory_manager_key(KeyEvent::new(code, KeyModifiers::NONE)),
                KeyIntent::Action(AppAction::MemoryManagerEdit)
            ));
        }
    }
}
