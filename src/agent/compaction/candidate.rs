//! Turning a [`CompactionDraft`] into a candidate message buffer: repacking the
//! summary at a stable boundary, applying tool-output stubs, and reindexing
//! controls and summary sources onto the new indices. Plus the token estimators
//! the draft uses.

use super::*;

/// `message_ids[index]`, falling back to a synthetic id derived from `index`
/// when the slice has no entry there (e.g. an index past the end).
fn id_at_or_synthetic(message_ids: &[String], index: usize) -> String {
    message_ids
        .get(index)
        .cloned()
        .unwrap_or_else(|| ContextNodeId::message(index).into_string())
}

impl Agent {
    /// The live message id at `index`, falling back to a synthetic id derived
    /// from `index` when the row predates stable ids (or was never assigned
    /// one).
    pub(super) fn message_id_or_synthetic(&self, index: usize) -> String {
        self.message_id_for_index(index)
            .map(str::to_string)
            .unwrap_or_else(|| ContextNodeId::message(index).into_string())
    }

    pub(super) fn compaction_candidate(
        &self,
        draft: &CompactionDraft,
        summary_text: Option<&str>,
        summary_message_id: Option<&str>,
    ) -> Result<CompactionCandidate> {
        let summary_message = summary_text.map(compaction_summary_message).transpose()?;
        let stub_by_index = draft
            .tool_outputs_to_stub
            .iter()
            .map(|stub| (stub.old_index, stub))
            .collect::<HashMap<_, _>>();
        let mut messages = Vec::with_capacity(
            draft
                .kept
                .len()
                .saturating_add(usize::from(summary_message.is_some())),
        );
        let mut old_to_new = HashMap::new();
        let mut message_ids = Vec::with_capacity(messages.capacity());
        let mut summary_index = None;
        let summary_id = summary_message_id
            .map(str::to_string)
            .unwrap_or_else(|| "msg-summary".to_string());

        for kept in &draft.kept {
            let message = stub_by_index
                .get(&kept.old_index)
                .map(|stub| stub.replacement.clone())
                .unwrap_or_else(|| kept.message.clone());
            old_to_new.insert(kept.old_index, messages.len());
            message_ids.push(self.message_id_or_synthetic(kept.old_index));
            messages.push(message);

            if kept.old_index == 0
                && let Some(summary_message) = summary_message.as_ref()
            {
                summary_index = Some(messages.len());
                message_ids.push(summary_id.clone());
                messages.push(summary_message.clone());
            }
        }

        // Safety net: index 0 is always kept today (the draft builder never omits
        // it), so the splice above normally places the summary. But if that ever
        // changes, fall back to appending it at the end rather than silently
        // dropping the entire compaction summary from the compacted context.
        if summary_index.is_none()
            && let Some(summary_message) = summary_message.as_ref()
        {
            summary_index = Some(messages.len());
            message_ids.push(summary_id.clone());
            messages.push(summary_message.clone());
        }

        let stubbed_tool_ids = draft
            .tool_outputs_to_stub
            .iter()
            .map(|stub| stub.tool_id.clone())
            .collect::<HashSet<_>>();
        let controls = self.reindexed_controls_for_candidate(
            &old_to_new,
            &messages,
            &message_ids,
            &stubbed_tool_ids,
        );
        let mut summary_sources =
            self.reindexed_sources_for_candidate(&old_to_new, &messages, &message_ids);

        if let Some(summary_index) = summary_index {
            let originals = draft.omitted_originals().cloned().collect::<Vec<_>>();
            if !originals.is_empty() {
                summary_sources.insert(id_at_or_synthetic(&message_ids, summary_index), originals);
            }
        }
        for stub in &draft.tool_outputs_to_stub {
            summary_sources
                .entry(stub.tool_id.clone())
                .or_insert_with(|| {
                    self.original_sources_for_message(stub.old_index, &stub.original)
                });
        }

        Ok(CompactionCandidate {
            messages,
            message_ids,
            controls,
            summary_sources,
            messages_omitted: draft.messages_omitted,
            tool_outputs_stubbed: draft.tool_outputs_to_stub.len(),
            summary_source_available: summary_index.is_some()
                || !draft.tool_outputs_to_stub.is_empty(),
            summary_repacked: summary_index.is_some() && !draft.omitted.is_empty(),
        })
    }

    pub(super) fn reindexed_controls_for_candidate(
        &self,
        old_to_new: &HashMap<usize, usize>,
        messages: &[ChatCompletionRequestMessage],
        message_ids: &[String],
        stubbed_tool_ids: &HashSet<String>,
    ) -> HashMap<String, ContextControlState> {
        let valid_ids = valid_context_control_ids(messages, message_ids);
        let mut reindexed = HashMap::new();
        for (id, state) in &self.context_controls {
            let mut state = *state;
            let canonical = canonical_context_control_id(id);
            let target_id = if let Some(old_index) = self.message_index_for_control_id(&canonical) {
                let Some(new_index) = old_to_new.get(&old_index).copied() else {
                    continue;
                };
                id_at_or_synthetic(message_ids, new_index)
            } else {
                canonical
            };
            if stubbed_tool_ids.contains(&target_id) {
                state.stubbed = false;
                state.stub_reason = None;
            }
            if state.is_active() && valid_ids.contains(&target_id) {
                reindexed.insert(target_id, state);
            }
        }
        reindexed
    }

    pub(super) fn reindexed_sources_for_candidate(
        &self,
        old_to_new: &HashMap<usize, usize>,
        messages: &[ChatCompletionRequestMessage],
        message_ids: &[String],
    ) -> HashMap<String, Vec<ChatCompletionRequestMessage>> {
        let valid_ids = valid_context_control_ids(messages, message_ids);
        let mut reindexed = HashMap::new();
        for (id, source) in &self.summary_sources {
            let canonical = canonical_context_control_id(id);
            if let Some(old_index) = self.message_index_for_control_id(&canonical) {
                if let Some(new_index) = old_to_new.get(&old_index).copied() {
                    reindexed.insert(id_at_or_synthetic(message_ids, new_index), source.clone());
                }
            } else if valid_ids.contains(&canonical) {
                reindexed.insert(canonical, source.clone());
            }
        }
        reindexed
    }

    pub(super) async fn estimate_candidate_tokens(
        &self,
        messages: &[ChatCompletionRequestMessage],
        message_ids: &[String],
        controls: &HashMap<String, ContextControlState>,
        tool_schema: &ToolSchemaPayload,
    ) -> usize {
        let projection = self.context_projection_for_controls(messages, message_ids, controls);
        self.prompt_estimator
            .estimate_prompt_with_tool_schema_tokens(
                projection.control_messages(),
                tool_schema.tools(),
                tool_schema.model_tool_schema_tokens(),
            )
            .await
            .input_tokens
    }

    pub(in crate::agent) fn payload_message_token_estimate(
        &self,
        messages: &[ChatCompletionRequestMessage],
        message_ids: &[String],
        controls: &HashMap<String, ContextControlState>,
        tool_schema: &ToolSchemaPayload,
    ) -> (Vec<usize>, PromptEstimate) {
        let projection = self.context_projection_with_estimate_for_controls(
            messages,
            message_ids,
            controls,
            tool_schema,
        );
        let estimate = projection.estimate().cloned().unwrap_or_else(|| {
            PromptEstimate::heuristic(
                0,
                tool_schema.report_tool_schema_tokens(),
                TokenCounterKind::Heuristic,
            )
        });
        (projection.source_message_tokens().to_vec(), estimate)
    }

    pub(super) fn cacheable_prefix_tokens_for_controls(
        &self,
        messages: &[ChatCompletionRequestMessage],
        message_ids: &[String],
        controls: &HashMap<String, ContextControlState>,
        tool_schema: &ToolSchemaPayload,
    ) -> usize {
        let (message_tokens, _estimate) =
            self.payload_message_token_estimate(messages, message_ids, controls, tool_schema);
        prompt_cache_token_totals(
            messages,
            &message_tokens,
            messages.len(),
            system_prompt(self.mode),
            self.project_context.as_ref(),
            &self.prompt_estimator,
            tool_schema.report_tool_schema_tokens(),
        )
        .0
    }

    pub(super) fn context_payload_fingerprint_for_controls(
        &self,
        messages: &[ChatCompletionRequestMessage],
        message_ids: &[String],
        controls: &HashMap<String, ContextControlState>,
    ) -> String {
        let projection = self.context_projection_for_controls(messages, message_ids, controls);
        let mut hasher = blake3::Hasher::new();
        for message in projection.control_messages() {
            match serde_json::to_vec(&message) {
                Ok(raw) => hasher.update(&raw),
                // Serialization of a derived message effectively never fails, but
                // if it did, feed the Debug bytes so two different unserializable
                // messages can't collide to an identical fingerprint.
                Err(_) => hasher.update(format!("{message:?}").as_bytes()),
            };
            hasher.update(b"\n");
        }
        let hex = hasher.finalize().to_hex().to_string();
        hex[..16].to_string()
    }
}
