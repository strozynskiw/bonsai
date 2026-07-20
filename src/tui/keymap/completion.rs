//! Input-completion helpers for the primary key map.
//!
//! These resolve the currently-selected completion candidate into a concrete
//! [`AppAction`], and drive Tab's command/argument completion. They are pure
//! functions over [`AppState`]; the key dispatch that calls them lives in the
//! parent module.

use crate::tui::app::AppState;
use crate::tui::event::AppAction;

pub(super) fn completion_open(app: &AppState) -> bool {
    app.completion.active && !app.completion.candidates.is_empty()
}

pub(super) fn current_completion_action(buffer: &str, app: &AppState) -> Option<AppAction> {
    let candidate = app.completion.candidates.get(app.completion.cursor)?;
    match candidate {
        crate::tui::app::CompletionCandidate::Command { label, .. } => {
            Some(AppAction::CompleteInputTo(label.clone()))
        }
        crate::tui::app::CompletionCandidate::Provider { id, .. } => {
            let (cmd, _arg) = buffer.split_once(char::is_whitespace)?;
            Some(AppAction::CompleteInputTo(format!("{cmd} {id}")))
        }
        crate::tui::app::CompletionCandidate::Model { replacement, .. } => {
            let (cmd, _arg) = buffer.split_once(char::is_whitespace)?;
            Some(AppAction::CompleteInputTo(format!("{cmd} {replacement}")))
        }
        crate::tui::app::CompletionCandidate::Argument { replacement, .. } => {
            Some(AppAction::CompleteInputTo(replacement.clone()))
        }
        crate::tui::app::CompletionCandidate::Path { path, kind } => {
            let (start, end) = app.completion.replacement_range?;
            Some(AppAction::CompleteInputRange {
                start,
                end,
                replacement: crate::tui::completion::path_replacement(path, *kind),
            })
        }
    }
}

pub(super) fn input_range_matches(
    buffer: &str,
    start: usize,
    end: usize,
    replacement: &str,
) -> bool {
    // `start`/`end` are grapheme indices (from `Completion::replacement_range`),
    // matching how `Composer::replace_range` applies them.
    let byte_start = crate::tui::text_bounds::byte_index_for_grapheme_index(buffer, start);
    let byte_end = crate::tui::text_bounds::byte_index_for_grapheme_index(buffer, end);
    buffer.get(byte_start..byte_end) == Some(replacement)
}

pub(super) fn command_tab_replacement(command: &str) -> String {
    if command_takes_arguments(command) {
        format!("{command} ")
    } else {
        command.to_string()
    }
}

/// Whether accepting `command` from the completion list should leave a
/// trailing space so argument completion can take over immediately. Derived
/// from the command registry's usage hint — a hardcoded list here drifted and
/// silently dropped commands like `/self-review`, whose bare form is a status
/// query, making an accepted-then-submitted completion look like a no-op.
pub(super) fn command_takes_arguments(command: &str) -> bool {
    crate::commands::command_usage_hint(command).is_some()
}

pub(super) fn current_completion_is_command(app: &AppState) -> bool {
    matches!(
        app.completion.candidates.get(app.completion.cursor),
        Some(crate::tui::app::CompletionCandidate::Command { .. })
    )
}

pub(super) fn complete_command_arg_from_state(buffer: &str, app: &AppState) -> Option<String> {
    let (cmd, arg) = buffer.split_once(char::is_whitespace)?;
    let arg = arg.trim();

    let matches: Vec<String> = match cmd {
        "/authorize" | "/unauthorize" => {
            let lower = arg.to_lowercase();
            app.provider_choices
                .iter()
                .filter(|choice| {
                    choice.provider_id.to_lowercase().starts_with(&lower)
                        || choice.provider_label.to_lowercase().starts_with(&lower)
                })
                .map(|choice| choice.provider_id.clone())
                .collect()
        }
        "/theme" => {
            // The `export` subcommand plus every theme name — mirrors the inline
            // completion popover (`completion::arg_completion`) so Tab and the
            // popover propose the same candidates.
            let lower = arg.to_lowercase();
            std::iter::once("export")
                .chain(crate::tui::theme::theme_names())
                .filter(|name| name.starts_with(&lower))
                .map(str::to_string)
                .collect()
        }
        "/model" => {
            let lower = arg.to_lowercase();
            app.cached_model_choices
                .iter()
                .filter(|choice| choice.starts_with_filter(&lower))
                .map(|choice| choice.display_model().to_string())
                .collect()
        }
        _ => return None,
    };
    crate::commands::complete_from_matches(matches, arg)
        .map(|completion| format!("{cmd} {completion}"))
}
