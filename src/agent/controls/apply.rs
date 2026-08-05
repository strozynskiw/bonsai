//! User-facing context-control actions from the `/ctx` view: pin / drop / stub
//! toggles, restoring persisted controls, and splicing a summarized row back to
//! its original messages.

use super::*;

impl Agent {
    pub fn restore_context_controls(
        &mut self,
        controls: HashMap<String, ContextControlState>,
        summary_sources: HashMap<String, Vec<ChatCompletionRequestMessage>>,
    ) {
        self.context_controls = controls
            .into_iter()
            .filter(|(_id, state)| state.is_active())
            .map(|(id, state)| (self.canonical_context_control_id_for(&id), state))
            .collect();
        self.summary_sources = summary_sources
            .into_iter()
            .map(|(id, messages)| (self.canonical_context_control_id_for(&id), messages))
            .collect();
        self.prune_context_controls_to_current_messages();
    }

    pub fn restore_compaction_events(&mut self, events: Vec<CompactionEvent>) {
        for diagnostic in crate::context_view::compaction_sequence_diagnostics(&events) {
            tracing::warn!(
                target: "bonsai::compaction",
                expected_seq = diagnostic.expected_seq,
                observed_seq = diagnostic.observed_seq,
                "loaded legacy compaction ledger with a sequence discontinuity"
            );
        }
        self.compaction_events = events;
    }

    pub fn apply_context_control_action(
        &mut self,
        node_id: &str,
        action: ContextControlAction,
    ) -> bool {
        let id = self.canonical_context_control_id_for(node_id);
        match action {
            ContextControlAction::RestoreSummarySource => {
                self.restore_summary_sources_for_target(&id)
            }
            ContextControlAction::TogglePin => self.toggle_context_control(
                &id,
                action,
                |state| state.pinned,
                |state, enabled| state.pinned = enabled,
            ),
            ContextControlAction::ToggleDropNextTurn => self.toggle_context_control(
                &id,
                action,
                |state| state.drop_next_turn,
                |state, enabled| state.drop_next_turn = enabled,
            ),
            ContextControlAction::ToggleStub => self.toggle_context_control(
                &id,
                action,
                |state| state.stubbed,
                |state, enabled| {
                    state.stubbed = enabled;
                    state.stub_reason = enabled.then_some(ContextStubReason::User);
                },
            ),
        }
    }

    /// Shared body of the three `Toggle*` actions: resolve targets (bailing if
    /// none), flip `enabled` to the opposite of "all targets already have this
    /// flag set", then apply `set` to every target and prune it if that leaves
    /// it inactive. `get`/`set` isolate the one field each toggle flips.
    fn toggle_context_control(
        &mut self,
        id: &str,
        action: ContextControlAction,
        get: impl Fn(ContextControlState) -> bool,
        mut set: impl FnMut(&mut ContextControlState, bool),
    ) -> bool {
        let targets = self.context_control_targets_for(id, action);
        if targets.is_empty() {
            return false;
        }
        let enabled = !targets
            .iter()
            .all(|target| self.context_controls.get(target).copied().is_some_and(&get));
        for target in &targets {
            let state = self.context_controls.entry(target.clone()).or_default();
            set(state, enabled);
            self.remove_inactive_context_control(target);
        }
        true
    }

    fn restore_summary_sources_for_target(&mut self, id: &str) -> bool {
        if self.summary_sources.contains_key(id)
            && (self.message_index_for_control_id(id).is_some()
                || tool_call_id_from_context_id(id).is_some())
        {
            return self.restore_summary_source(id);
        }

        let mut restored = false;
        while let Some(target) = self
            .context_control_targets_for(id, ContextControlAction::RestoreSummarySource)
            .into_iter()
            .find(|target| self.summary_sources.contains_key(target))
        {
            if !self.restore_summary_source(&target) {
                break;
            }
            restored = true;
        }
        restored
    }

    fn context_control_targets_for(&self, id: &str, action: ContextControlAction) -> Vec<String> {
        if self.context_control_target_allowed(id, action) {
            return vec![id.to_string()];
        }

        let report = self.context_report();
        let Some(node) = find_context_node_by_id(&report.ledger, id) else {
            return Vec::new();
        };
        let mut targets = Vec::new();
        let mut seen = HashSet::new();
        self.collect_context_control_targets(node, action, &mut seen, &mut targets);
        targets
    }

    fn collect_context_control_targets(
        &self,
        node: &ContextNode,
        action: ContextControlAction,
        seen: &mut HashSet<String>,
        targets: &mut Vec<String>,
    ) {
        let id = self.canonical_context_control_id_for(node.id.as_str());
        if self.context_control_target_allowed(&id, action) && seen.insert(id.clone()) {
            targets.push(id);
        }
        for child in &node.children {
            self.collect_context_control_targets(child, action, seen, targets);
        }
    }

    fn context_control_target_allowed(&self, id: &str, action: ContextControlAction) -> bool {
        match action {
            ContextControlAction::TogglePin => self.context_control_target_exists(id),
            ContextControlAction::ToggleDropNextTurn => {
                self.context_control_target_exists(id) && !self.context_control_target_is_system(id)
            }
            ContextControlAction::ToggleStub => {
                is_tool_context_id(id) && self.context_control_target_exists(id)
            }
            ContextControlAction::RestoreSummarySource => self.summary_sources.contains_key(id),
        }
    }

    fn context_control_target_is_system(&self, id: &str) -> bool {
        self.message_index_for_control_id(id)
            .and_then(|index| self.messages.get(index))
            .is_some_and(|message| describe_message_full(message).0 == ContextRole::System)
    }

    pub(super) fn restore_summary_source(&mut self, id: &str) -> bool {
        if let Some(index) = self.message_index_for_control_id(id) {
            return self.restore_summary_source_at(id, index);
        };
        let Some(call_id) = tool_call_id_from_context_id(id) else {
            return false;
        };
        let Some(index) = self
            .messages
            .iter()
            .position(|message| tool_message_call_id(message).as_deref() == Some(call_id))
        else {
            return false;
        };
        self.restore_summary_source_at(id, index)
    }

    pub(super) fn restore_summary_source_at(&mut self, id: &str, index: usize) -> bool {
        if index >= self.messages.len() {
            return false;
        }
        let Some(source) = self.summary_sources.remove(id) else {
            return false;
        };
        let original_len = self.messages.len();
        let inserted_len = source.len();
        let inserted_ids = self.allocate_context_message_ids(inserted_len);
        self.note_summary_source_restored(id, &source, &inserted_ids);
        self.messages.splice(index..=index, source);
        self.message_ids.splice(index..=index, inserted_ids.clone());
        self.context_controls.remove(id);
        // If the restored row was an episode eviction marker, transition its
        // episode explicitly (Evicted -> Restored) and rewrite the ledger span
        // onto the newly allocated live ids — control pruning alone cannot
        // detect this restore.
        let mut old_to_new = HashMap::new();
        for old_index in 0..original_len {
            if old_index == index {
                continue;
            }
            let new_index = if old_index < index {
                old_index
            } else if inserted_len == 0 {
                old_index - 1
            } else {
                old_index + inserted_len - 1
            };
            old_to_new.insert(old_index, new_index);
        }
        self.reindex_message_controls(&old_to_new);
        self.reindex_summary_sources(&old_to_new);
        self.caches.last_prompt_estimate = None;
        true
    }
}

fn find_context_node_by_id<'a>(nodes: &'a [ContextNode], id: &str) -> Option<&'a ContextNode> {
    for node in nodes {
        if node.id.as_str() == id {
            return Some(node);
        }
        if let Some(found) = find_context_node_by_id(&node.children, id) {
            return Some(found);
        }
    }
    None
}
