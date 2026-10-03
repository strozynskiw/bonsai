//! Classification of short human prompts that refer back to an established
//! task instead of defining a new goal.

/// Whether a human prompt starts a new goal, resumes it, or authorizes its
/// referenced action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskPromptKind {
    NewGoal,
    Continuation,
    ActionContinuation,
}

impl TaskPromptKind {
    /// Classify only bare continuation directives. A prompt that adds details
    /// remains a new goal so those details are not discarded during resume.
    pub(crate) fn classify(prompt: &str) -> Self {
        match action_directive(&normalized_words(prompt)) {
            "do it"
            | "do it please"
            | "go ahead"
            | "go ahead please"
            | "make it happen"
            | "make it happen please" => Self::ActionContinuation,
            "continue"
            | "continue please"
            | "continue the task"
            | "go on"
            | "go on please"
            | "keep going"
            | "keep going please"
            | "keep working"
            | "keep working please"
            | "keep working on it"
            | "carry on"
            | "carry on please"
            | "proceed"
            | "proceed please"
            | "resume"
            | "resume it"
            | "resume please"
            | "resume the task"
            | "retry"
            | "retry please"
            | "try again"
            | "try again please"
            | "give it another try"
            | "give it another go"
            | "pick up where you left off" => Self::Continuation,
            _ => Self::NewGoal,
        }
    }

    pub(crate) const fn is_continuation(self) -> bool {
        matches!(self, Self::Continuation | Self::ActionContinuation)
    }

    /// Whether the continuation requires an observable action even when the
    /// prior task's completion contract was informational or read-only.
    pub(crate) const fn requests_action(self) -> bool {
        matches!(self, Self::ActionContinuation)
    }
}

/// Lowercase words and replace punctuation with single spaces.
pub(crate) fn normalized_words(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    for character in text.to_lowercase().chars() {
        if character.is_alphanumeric() {
            normalized.push(character);
        } else {
            normalized.push(' ');
        }
    }
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove polite request preambles without removing substantive directives.
pub(crate) fn action_directive(normalized: &str) -> &str {
    let mut directive = normalized;
    loop {
        let mut stripped = None;
        for prefix in [
            "please ",
            "pls ",
            "can you ",
            "could you ",
            "would you ",
            "i need you to ",
            "we need to ",
            "lets ",
            "let us ",
            "ok ",
            "okay ",
            "also ",
            "no to ",
            "dobra ",
            "prosze ",
            "proszę ",
            "czy mozesz ",
            "czy możesz ",
        ] {
            if let Some(remainder) = directive.strip_prefix(prefix) {
                stripped = Some(remainder);
                break;
            }
        }
        match stripped {
            Some(remainder) => directive = remainder,
            None => return directive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_retry_phrases_as_continuations() {
        for prompt in [
            "continue",
            "OK, continue.",
            "please resume the task",
            "try again",
            "okay, retry please",
            "pick up where you left off",
        ] {
            assert_eq!(
                TaskPromptKind::classify(prompt),
                TaskPromptKind::Continuation,
                "prompt: {prompt:?}"
            );
        }
    }

    #[test]
    fn classifies_terse_execution_approvals_as_action_continuations() {
        for prompt in ["DO it", "please go ahead", "make it happen please"] {
            assert_eq!(
                TaskPromptKind::classify(prompt),
                TaskPromptKind::ActionContinuation,
                "prompt: {prompt:?}"
            );
        }
    }

    #[test]
    fn prompts_with_new_details_remain_new_goals() {
        for prompt in [
            "try again with lower reasoning",
            "continue with the parser fix",
            "retry only the failed test",
            "ok fix it",
        ] {
            assert_eq!(
                TaskPromptKind::classify(prompt),
                TaskPromptKind::NewGoal,
                "prompt: {prompt:?}"
            );
        }
    }
}
