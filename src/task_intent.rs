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

/// Recognize unambiguous requests for an update on the current agent task.
pub(crate) fn requests_current_task_status(prompt: &str) -> bool {
    if prompt.contains(['"', '`', '\n', '\r']) {
        return false;
    }
    let normalized = normalized_words(prompt);
    let directive = action_directive(&normalized);
    if [
        "do not",
        "don t",
        "without",
        "no status",
        "instead of",
        "unless",
        "if ",
    ]
    .iter()
    .any(|negative| directive.contains(negative))
    {
        return false;
    }
    if let Some(remainder) = [
        "give a concise status update",
        "give a brief status update",
        "give a status update",
    ]
    .iter()
    .find_map(|prefix| directive.strip_prefix(prefix))
    {
        let remainder = remainder.trim_start();
        return remainder.is_empty()
            || remainder.starts_with("then ")
            || remainder.starts_with("and continue")
            || remainder.starts_with("on the current task")
            || remainder.starts_with("on the task");
    }
    [
        "what are you working on",
        "what are you doing now",
        "what are you fixing now",
        "how is it going",
        "how its going",
        "how s it going",
    ]
    .iter()
    .any(|prefix| {
        directive == *prefix
            || directive
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with(' '))
    })
}

/// Recognize direct, unqualified human requests to avoid running commands.
pub(crate) fn requests_no_commands(prompt: &str) -> bool {
    if prompt.contains("\\`") {
        return false;
    }
    let mut unquoted = String::with_capacity(prompt.len());
    let mut in_code = false;
    for part in prompt.split('`') {
        if !in_code {
            unquoted.push_str(part);
        }
        unquoted.push(' ');
        in_code = !in_code;
    }
    if !in_code {
        return false;
    }
    let lower = unquoted.to_ascii_lowercase();
    let quote_check = lower.replace("don't", "dont");
    if quote_check.contains(['\'', '"', '`']) {
        return false;
    }
    let normalized = normalized_words(&lower.replace("read-only", "readonly"));
    let words = normalized.split_whitespace().collect::<Vec<_>>();
    if words.iter().any(|word| {
        matches!(
            *word,
            "unless"
                | "except"
                | "until"
                | "only"
                | "if"
                | "instead"
                | "hypothetically"
                | "example"
                | "ignore"
                | "disregard"
        )
    }) {
        return false;
    }
    [
        "do not run commands",
        "do not run any commands",
        "do not execute commands",
        "don t run commands",
        "don t run any commands",
        "never run commands",
        "without running commands",
        "do not modify files or run commands",
        "don t modify files or run commands",
        "do not modify any files or run commands",
        "don t modify any files or run commands",
        "do not modify other files or run commands",
        "don t modify other files or run commands",
        "do not modify any other files or run commands",
        "don t modify any other files or run commands",
    ]
    .iter()
    .any(|phrase| {
        let needle = phrase.split_whitespace().collect::<Vec<_>>();
        words
            .windows(needle.len())
            .enumerate()
            .any(|(index, window)| {
                window == needle
                    && !words[index.saturating_sub(3)..index].iter().any(|word| {
                        matches!(*word, "old" | "previous" | "earlier" | "former" | "prior")
                    })
                    && !words[index + needle.len()..].iter().any(|word| {
                        matches!(
                            *word,
                            "but"
                                | "run"
                                | "execute"
                                | "lifted"
                                | "revoked"
                                | "rescinded"
                                | "retracted"
                                | "obsolete"
                        )
                    })
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_direct_current_status_requests_without_negated_or_quoted_text() {
        for prompt in [
            "Give a concise status update, then continue and finish the original task.",
            "what are you working on now?",
            "How's it going?",
        ] {
            assert!(requests_current_task_status(prompt), "{prompt}");
        }
        for prompt in [
            "Do not give a status update",
            "Explain the phrase \"what are you working on\"",
            "What is the status of issue 42?",
            "Give a status update without continuing",
            "If asked, give a status update",
            "Give a status update for the customer",
        ] {
            assert!(!requests_current_task_status(prompt), "{prompt}");
        }
    }

    #[test]
    fn recognizes_only_unqualified_no_command_instructions() {
        for prompt in [
            "Review this, but do not modify files or run commands.",
            "Do not run commands.",
            "Please don't run commands.",
            "Explain the code without running commands.",
            "Explain `total(&[2, 3, 5])`. Do not run commands.",
            "Do not run commands; inspect `src/lib.rs`.",
            "Use read-only inspection to explain `total`. Do not modify files or run commands.",
            "Add a line to README.md. Do not modify any other files or run commands.",
            "Edit README.md, but don't modify other files or run commands.",
        ] {
            assert!(requests_no_commands(prompt), "{prompt}");
        }
        for prompt in [
            "Explain the phrase 'do not run commands'.",
            "Explain the phrase `do not run commands`.",
            "Explain `unfinished code and do not run commands.",
            "Explain \\`do not run commands\\`.",
            "Do not run commands unless I ask.",
            "Do not run commands, but run the tests.",
            "Do not modify any other files or run commands, but run the tests.",
            "Do not run commands. Actually run tests.",
            "Do not run shell commands.",
            "Ignore the words do not run commands.",
            "The old instruction do not run commands has been lifted. Use bash to print READY.",
            "The previous instruction was do not run commands.",
            "Do not run commands has been revoked. Use bash to print READY.",
            "If I say do not run commands, what happens?",
            "Do not run only shell commands; use Git.",
        ] {
            assert!(!requests_no_commands(prompt), "{prompt}");
        }
    }

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
