//! User-facing context-control actions from the `/ctx` view: pin / drop / stub
//! toggles, restoring persisted controls, and splicing a summarized row back to
//! its original messages.

use super::*;

#[derive(Debug, Clone, Copy)]
enum ExchangeControlExpansion {
    Drop,
    Restore,
}

impl Agent {
    pub fn restore_context_controls(
        &mut self,
        controls: HashMap<String, ContextControlState>,
        summary_sources: HashMap<String, Vec<ChatCompletionRequestMessage>>,
        summary_source_stable_ids: HashMap<String, Vec<String>>,
    ) {
        // Source bytes are being replaced; their provenance must be restored
        // explicitly with the corresponding native snapshot, never inherited.
        self.native_source_response_bindings.clear();
        self.context_controls = controls
            .into_iter()
            .filter(|(_id, state)| state.is_active())
            .map(|(id, state)| (self.canonical_context_control_id_for(&id), state))
            .collect();
        self.summary_sources = summary_sources
            .into_iter()
            .map(|(id, messages)| (self.canonical_context_control_id_for(&id), messages))
            .collect();
        self.summary_source_stable_ids = summary_source_stable_ids
            .into_iter()
            .map(|(id, stable_ids)| (self.canonical_context_control_id_for(&id), stable_ids))
            .collect();
        self.normalize_dropped_tool_exchanges();
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
        let targets = if matches!(action, ContextControlAction::ToggleDropNextTurn) {
            self.expand_tool_exchange_targets(&targets, ExchangeControlExpansion::Drop)
        } else {
            targets
        };
        if targets.is_empty() {
            return false;
        }
        let enabled = !targets
            .iter()
            .all(|target| self.context_controls.get(target).copied().is_some_and(&get));
        let targets = if matches!(action, ContextControlAction::ToggleDropNextTurn) && !enabled {
            self.expand_tool_exchange_targets(&targets, ExchangeControlExpansion::Restore)
        } else {
            targets
        };
        for target in &targets {
            let state = self.context_controls.entry(target.clone()).or_default();
            set(state, enabled);
            self.remove_inactive_context_control(target);
        }
        true
    }

    /// A tool exchange is indivisible on the wire. Materialize its drop flags
    /// so request projection, the context ledger, and read/recall liveness all
    /// agree. Scan to the next assistant boundary to cover older histories
    /// with image/user attachments between sibling tool results.
    fn expand_tool_exchange_targets(
        &self,
        targets: &[String],
        expansion: ExchangeControlExpansion,
    ) -> Vec<String> {
        if targets.is_empty() {
            return Vec::new();
        }
        let mut expanded = targets.to_vec();
        let mut seen = targets.iter().cloned().collect::<HashSet<_>>();
        let mut exchanges = Vec::new();
        for (index, message) in self.messages.iter().enumerate() {
            let Some(calls) = assistant_tool_calls(message).filter(|calls| !calls.is_empty())
            else {
                continue;
            };
            let call_ids = calls
                .iter()
                .map(|call| call.call_id.as_str())
                .collect::<HashSet<_>>();
            let end = self.messages[index + 1..]
                .iter()
                .position(|message| matches!(message, ChatCompletionRequestMessage::Assistant(_)))
                .map_or(self.messages.len(), |offset| index + 1 + offset);
            let rows = std::iter::once(index)
                .chain((index + 1..end).filter(|&result_index| {
                    tool_message_call_id(&self.messages[result_index])
                        .is_some_and(|call_id| call_ids.contains(call_id.as_str()))
                }))
                .collect::<Vec<_>>();
            let exchange_ids = rows
                .iter()
                .copied()
                .flat_map(|row| self.message_control_ids_for(row, &self.messages[row]))
                .collect::<Vec<_>>();
            let dependencies = rows
                .into_iter()
                .flat_map(|row| self.reuse_source_call_ids(&self.messages[row]))
                .map(|call_id| ContextNodeId::tool(&call_id).into_string())
                .collect::<Vec<_>>();
            exchanges.push((exchange_ids, dependencies));
        }
        // A retained pointer cannot supply bytes its source drop removes.
        // Include dependent exchanges transitively. Inspect stored pointer
        // metadata even when already dropped so toggling the source back on
        // restores the same complete set.
        loop {
            let mut changed = false;
            for (ids, dependencies) in &exchanges {
                let member_selected = ids.iter().any(|id| seen.contains(id));
                let selected = match expansion {
                    ExchangeControlExpansion::Drop => {
                        member_selected || dependencies.iter().any(|id| seen.contains(id))
                    }
                    ExchangeControlExpansion::Restore => member_selected,
                };
                if selected {
                    // Restoring a pointer also restores its sources, but does
                    // not revive unrelated downstream pointers the user hid.
                    let sources = matches!(expansion, ExchangeControlExpansion::Restore)
                        .then_some(dependencies.as_slice())
                        .unwrap_or_default();
                    for id in ids.iter().chain(sources) {
                        if seen.insert(id.clone()) {
                            expanded.push(id.clone());
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        expanded
    }

    pub(in crate::agent) fn normalize_dropped_tool_exchanges(&mut self) {
        let dropped = self
            .context_controls
            .iter()
            .filter(|(_, state)| state.drop_next_turn)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in self.expand_tool_exchange_targets(&dropped, ExchangeControlExpansion::Drop) {
            self.context_controls.entry(id).or_default().drop_next_turn = true;
        }
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
        if let Some(bindings) = self.native_source_response_bindings.get(id)
            && self
                .summary_sources
                .get(id)
                .is_none_or(|source| source.len() != bindings.len())
        {
            tracing::error!(
                source_id = id,
                "native response provenance does not align with restored originals"
            );
            return false;
        }
        let Some(source) = self.summary_sources.remove(id) else {
            return false;
        };
        let source_stable_ids = self
            .summary_source_stable_ids
            .remove(id)
            .unwrap_or_default();
        let original_len = self.messages.len();
        let inserted_len = source.len();
        let inserted_ids = self.allocate_context_message_ids(inserted_len);
        let response_ids = self
            .native_source_response_bindings
            .remove(id)
            .unwrap_or_else(|| vec![None; inserted_len]);
        for ((message_id, response_id), message) in
            inserted_ids.iter().zip(response_ids).zip(&source)
        {
            if matches!(message, ChatCompletionRequestMessage::Assistant(_))
                && let Some(response_id) = response_id
            {
                self.native_response_bindings
                    .insert(message_id.clone(), response_id);
            }
        }
        self.note_summary_source_restored(id, &source, &source_stable_ids, &inserted_ids);
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
