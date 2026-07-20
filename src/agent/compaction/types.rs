//! Intermediate stages of a compaction pass: the draft (what would be
//! omitted/stubbed/kept if applied) and the candidate (a draft resolved into
//! concrete new messages, ids, and controls). Types are `pub(super)` so the
//! sibling `candidate`/`draft`/`gc`/`helpers`/`summary` modules can read and
//! build them.

use super::*;

#[derive(Debug, Clone)]
pub(in crate::agent) struct CompactionDraft {
    pub(in crate::agent) omitted: Vec<CompactionOmittedMessage>,
    pub(super) kept: Vec<CompactionKeptMessage>,
    pub(super) messages_omitted: usize,
    pub(in crate::agent) tool_outputs_to_stub: Vec<CompactionStub>,
    pub(super) target_tokens: usize,
}

impl CompactionDraft {
    pub(super) fn has_changes(&self) -> bool {
        !self.omitted.is_empty() || !self.tool_outputs_to_stub.is_empty()
    }

    /// Every original message that would be summarized away, across all
    /// omitted groups, in omission order. Callers that need owned messages add
    /// `.cloned()`.
    pub(super) fn omitted_originals(&self) -> impl Iterator<Item = &ChatCompletionRequestMessage> {
        self.omitted.iter().flat_map(|item| item.originals.iter())
    }

    /// Minimal draft (only `omitted` set; empty kept/stubs) for unit-testing
    /// summary rendering without running a full compaction pass. Keeps the real
    /// fields private.
    #[cfg(test)]
    pub(in crate::agent) fn with_omitted_for_test(omitted: Vec<CompactionOmittedMessage>) -> Self {
        Self {
            omitted,
            kept: Vec::new(),
            messages_omitted: 0,
            tool_outputs_to_stub: Vec::new(),
            target_tokens: 1_000,
        }
    }
}

#[derive(Debug, Clone)]
pub(in crate::agent) struct CompactionOmittedMessage {
    pub(in crate::agent) originals: Vec<ChatCompletionRequestMessage>,
}

#[derive(Debug, Clone)]
pub(super) struct CompactionKeptMessage {
    pub(super) old_index: usize,
    pub(super) message: ChatCompletionRequestMessage,
}

#[derive(Debug, Clone)]
pub(in crate::agent) struct CompactionStub {
    pub(in crate::agent) old_index: usize,
    pub(super) tool_id: String,
    pub(super) original: ChatCompletionRequestMessage,
    pub(super) replacement: ChatCompletionRequestMessage,
}

#[derive(Debug, Clone)]
pub(super) struct CompactionCandidate {
    pub(super) messages: Vec<ChatCompletionRequestMessage>,
    pub(super) message_ids: Vec<String>,
    pub(super) controls: HashMap<String, ContextControlState>,
    pub(super) summary_sources: HashMap<String, Vec<ChatCompletionRequestMessage>>,
    pub(super) messages_omitted: usize,
    pub(super) tool_outputs_stubbed: usize,
    pub(super) summary_source_available: bool,
    pub(super) summary_repacked: bool,
}

#[derive(Debug, Clone)]
pub(in crate::agent) struct MessageGroup {
    pub(in crate::agent) indices: Vec<usize>,
}
