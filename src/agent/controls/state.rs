//! Maintenance of the context-control and summary-source maps: reindexing onto
//! new message positions, pruning stale entries, and the read accessors.

use super::*;

impl Agent {
    pub fn context_controls(&self) -> &HashMap<String, ContextControlState> {
        &self.context_controls
    }

    pub fn summary_sources(&self) -> &HashMap<String, Vec<ChatCompletionRequestMessage>> {
        &self.summary_sources
    }

    pub fn compaction_events(&self) -> &[CompactionEvent] {
        &self.compaction_events
    }

    pub(in crate::agent) fn reindex_message_controls(
        &mut self,
        old_to_new: &HashMap<usize, usize>,
    ) {
        let controls = std::mem::take(&mut self.context_controls);
        self.context_controls = reindex_by_message_id(controls, old_to_new, &self.message_ids);
        self.prune_context_controls_to_current_messages();
    }

    pub(in crate::agent) fn reindex_summary_sources(&mut self, old_to_new: &HashMap<usize, usize>) {
        let sources = std::mem::take(&mut self.summary_sources);
        self.summary_sources = reindex_by_message_id(sources, old_to_new, &self.message_ids);
    }

    pub(in crate::agent) fn prune_context_controls_to_current_messages(&mut self) {
        let valid_ids = self
            .messages
            .iter()
            .enumerate()
            .flat_map(|(index, message)| self.message_control_ids_for(index, message))
            .collect::<HashSet<_>>();
        self.context_controls
            .retain(|id, state| state.is_active() && valid_ids.contains(id));
    }

    pub(super) fn context_control_target_exists(&self, id: &str) -> bool {
        self.messages.iter().enumerate().any(|(index, message)| {
            self.message_control_ids_for(index, message)
                .iter()
                .any(|candidate| candidate == id)
        })
    }

    pub(super) fn remove_inactive_context_control(&mut self, id: &str) {
        if self
            .context_controls
            .get(id)
            .is_some_and(|state| !state.is_active())
        {
            self.context_controls.remove(id);
        }
    }

    pub(in crate::agent) fn clear_drop_next_turn_controls(&mut self) {
        self.context_controls.retain(|_id, state| {
            state.drop_next_turn = false;
            state.is_active()
        });
    }
}

/// Shared body of [`Agent::reindex_message_controls`] and
/// [`Agent::reindex_summary_sources`]: remap each entry's id onto its new
/// message position after a splice, dropping entries whose message no longer
/// exists. Generic over the map's value type since a control-state map and a
/// summary-source map follow the identical id-remapping rule.
fn reindex_by_message_id<V>(
    map: HashMap<String, V>,
    old_to_new: &HashMap<usize, usize>,
    message_ids: &[String],
) -> HashMap<String, V> {
    let mut reindexed = HashMap::new();
    for (id, value) in map {
        let canonical = canonical_context_control_id(&id);
        if message_ids
            .iter()
            .any(|message_id| message_id == &canonical)
        {
            reindexed.insert(canonical, value);
        } else if let Some(old_index) = message_index_from_context_id(&canonical) {
            if let Some(new_index) = old_to_new.get(&old_index).copied()
                && let Some(message_id) = message_ids.get(new_index)
            {
                reindexed.insert(message_id.to_string(), value);
            }
        } else {
            reindexed.insert(canonical, value);
        }
    }
    reindexed
}
