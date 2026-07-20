use super::AppState;
use crate::tui::completion::Completion;
use crate::tui::path_search::PathKind;

#[derive(Debug, Clone, Default)]
pub struct CompletionState {
    pub candidates: Vec<CompletionCandidate>,
    pub cursor: usize,
    pub active: bool,
    pub replacement_range: Option<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionCandidate {
    Command {
        label: String,
        description: String,
    },
    Provider {
        id: String,
        display: String,
    },
    Model {
        provider_id: String,
        model: String,
        replacement: String,
        /// Compact `in$/cache$/out$` shorthand shown in the picker detail.
        pricing: String,
    },
    Argument {
        label: String,
        detail: String,
        replacement: String,
    },
    Path {
        path: String,
        kind: PathKind,
    },
}

impl AppState {
    pub fn recompute_completion(&mut self) {
        if self.modal.is_some() {
            self.completion = CompletionState::default();
            return;
        }
        let completion = crate::tui::completion::compute_completion(self);
        match completion {
            Some(Completion {
                candidates,
                replacement: _replacement,
                replacement_range,
            }) => {
                let active = !candidates.is_empty();
                // Always highlight the first (best) match as the list refines
                // while typing, so Enter accepts the top candidate. Arrow keys
                // move the selection from there via `CompletionMove`, which does
                // not recompute — so navigation is only reset by editing the
                // query, never by moving within the list.
                self.completion = CompletionState {
                    candidates,
                    cursor: 0,
                    active,
                    replacement_range,
                };
            }
            None => {
                self.completion = CompletionState::default();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::event::{AppAction, Focus};

    fn app() -> AppState {
        AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        )
    }

    #[test]
    fn completion_cursor_cycles_and_wraps() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::InputChar('/'));
        app.reduce(AppAction::InputChar('m'));
        assert!(app.completion.active);
        let total = app.completion.candidates.len();
        assert!(total > 1, "expected several slash-command candidates");
        // Typing `/m` reset the selection to the first entry, so the wrap below
        // starts from a known top-of-list position.
        assert_eq!(app.completion.cursor, 0);

        app.reduce(AppAction::CompletionMove(-1));
        assert_eq!(
            app.completion.cursor,
            total - 1,
            "moving up from the first entry wraps to the last"
        );

        app.reduce(AppAction::CompletionMove(1));
        assert_eq!(app.completion.cursor, 0, "moving down wraps back to first");
    }

    #[test]
    fn completion_page_move_clamps_to_menu_bounds() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::InputChar('/'));
        let total = app.completion.candidates.len();
        assert_eq!(
            total, 30,
            "top-level completion should keep a deep scrollable list, capped at 30"
        );

        app.reduce(AppAction::CompletionMove(5));
        assert_eq!(app.completion.cursor, 5, "paging scrolls past the window");

        app.reduce(AppAction::CompletionMove(100));
        assert_eq!(app.completion.cursor, total - 1, "over-page clamps to last");

        app.reduce(AppAction::CompletionMove(-100));
        assert_eq!(app.completion.cursor, 0);
    }

    #[test]
    fn completion_accept_dismisses_popover() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::InputChar('/'));
        assert!(app.completion.active);

        app.reduce(AppAction::CompleteInputTo("/copy".to_string()));

        assert_eq!(app.input(), "/copy");
        assert!(!app.completion.active);
    }

    #[test]
    fn submit_input_clears_completion_state() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::InputChar('/'));
        assert!(app.completion.active);

        app.reduce(AppAction::SubmitInput("/copy".to_string()));

        assert_eq!(app.input(), "");
        assert!(!app.completion.active);
    }

    #[test]
    fn clear_input_resets_composer_and_completion() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "/he".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        assert!(app.completion.active);

        app.reduce(AppAction::ClearInput);

        assert_eq!(app.composer.text, "");
        assert_eq!(app.input(), "");
        assert!(!app.completion.active);
    }

    #[test]
    fn completion_populates_when_typing_slash_command() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::InputChar('/'));
        app.reduce(AppAction::InputChar('h'));
        app.reduce(AppAction::InputChar('e'));
        assert!(app.completion.active, "completion should be active");
        assert!(!app.completion.candidates.is_empty());
    }

    #[test]
    fn completion_includes_start_command() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "/sta".chars() {
            app.reduce(AppAction::InputChar(ch));
        }

        assert!(app.completion.active, "completion should be active");
        assert!(app.completion.candidates.iter().any(|candidate| matches!(
            candidate,
            CompletionCandidate::Command { label, description }
                if label == "/start" && description.contains("Implement the current plan")
        )));
    }

    #[test]
    fn completion_matches_command_description_case_insensitively() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "/VISUAL".chars() {
            app.reduce(AppAction::InputChar(ch));
        }

        assert!(
            app.completion.active,
            "description match should open completion"
        );
        assert!(app.completion.candidates.iter().any(|candidate| matches!(
            candidate,
            CompletionCandidate::Command { label, .. } if label == "/ctx"
        )));
    }

    #[test]
    fn completion_cursor_resets_to_top_after_refine() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::InputChar('/'));
        assert!(app.completion.candidates.len() > 1);
        // Move the selection off the first entry...
        app.completion.cursor = 2;

        // ...then refine the query by typing: the highlight snaps back to the
        // first (best) match so Enter accepts the top candidate.
        app.reduce(AppAction::InputChar('a'));

        assert!(app.completion.active);
        assert_eq!(app.completion.cursor, 0);
    }

    #[test]
    fn completion_closes_after_trailing_space_for_command_without_args() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "/help ".chars() {
            app.reduce(AppAction::InputChar(ch));
        }

        assert!(!app.completion.active);
    }

    #[test]
    fn completion_clears_for_non_slash_input() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::InputChar('h'));
        app.reduce(AppAction::InputChar('i'));
        assert!(
            !app.completion.active,
            "non-slash input should not activate completion"
        );
    }
}
