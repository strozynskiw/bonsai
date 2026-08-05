//! The `/ctx` report builder: snapshots the current (or previewed) context
//! window into a [`ContextReport`] of per-message entries, the token ledger,
//! and usage reconciliation.

use super::*;

impl Agent {
    /// Snapshot the context window for the `/ctx` view: one entry per message
    /// the model currently sees (post-compaction), with role, estimated tokens,
    /// and a readable content preview.
    pub fn context_report(&self) -> ContextReport {
        self.context_report_for_messages(
            &self.messages,
            self.messages.len(),
            &[],
            &ContextPreviewInput::default(),
            self.mode,
        )
    }

    pub async fn context_report_with_preview(&self, preview: ContextPreviewInput) -> ContextReport {
        let mut messages = self.messages.clone();
        let target_mode = preview.target_mode.unwrap_or(self.mode);
        if target_mode != self.mode {
            set_system_message(&mut messages, self.system_message_for_mode(target_mode));
        }
        let base_message_count = messages.len();
        let mut pending = Vec::new();

        if let Some(draft) = preview
            .composer_draft
            .as_deref()
            .map(str::trim)
            .filter(|draft| !draft.is_empty() && !draft.starts_with('/'))
        {
            let expanded =
                preview_mentions_for_context(&self.project_root, &self.read_tracker, draft).await;
            if let Some(message) = user_request_message(&expanded) {
                pending.push(PendingContextMessage {
                    index: messages.len(),
                    label: "Composer draft".to_string(),
                    kind: ContextNodeKind::ComposerDraft,
                });
                messages.push(message);
            }
        }

        for message in self.pending_background_context_messages().await {
            if let Some(request) = user_request_message(&message) {
                pending.push(PendingContextMessage {
                    index: messages.len(),
                    label: background_context_label(&message),
                    kind: ContextNodeKind::Background,
                });
                messages.push(request);
            }
        }

        for queued in &preview.queued_inputs {
            let expanded =
                preview_mentions_for_context(&self.project_root, &self.read_tracker, &queued.text)
                    .await;
            if let Some(message) = user_request_message(&expanded) {
                let label = queued
                    .id
                    .map(|id| format!("Queued input #{id}"))
                    .unwrap_or_else(|| "Queued input".to_string());
                pending.push(PendingContextMessage {
                    index: messages.len(),
                    label,
                    kind: ContextNodeKind::QueuedInput,
                });
                messages.push(message);
            }
        }

        self.context_report_for_messages(
            &messages,
            base_message_count,
            &pending,
            &preview,
            target_mode,
        )
    }

    pub(super) fn context_report_for_messages(
        &self,
        messages: &[ChatCompletionRequestMessage],
        base_message_count: usize,
        pending_messages: &[PendingContextMessage],
        preview: &ContextPreviewInput,
        mode: AgentMode,
    ) -> ContextReport {
        let tool_schema = self.tool_schema_for_mode(mode);
        let message_ids = self.message_ids_for_messages(messages.len());
        let projection = self.context_projection_with_estimate_for_controls(
            messages,
            &message_ids,
            &self.context_controls,
            &tool_schema,
        );
        let volatile_terminal_message = self.volatile_terminal_context_message();
        let volatile_terminal_tokens = projection.volatile_terminal_tokens();
        let message_tokens = projection.source_message_tokens().to_vec();
        let report_estimate = projection.estimate().cloned().unwrap_or_else(|| {
            PromptEstimate::heuristic(
                0,
                tool_schema.report_tool_schema_tokens(),
                TokenCounterKind::Heuristic,
            )
        });
        let row_metadata = ContextTokenMetadata::from_estimate(&report_estimate);
        let mut estimate = report_estimate.clone();
        if mode == self.mode
            && messages.len() == self.messages.len()
            && pending_messages.is_empty()
            && let Some(last_estimate) = &self.caches.last_prompt_estimate
        {
            estimate = last_estimate.clone();
        }
        let pending_by_index = pending_messages
            .iter()
            .map(|message| (message.index, message))
            .collect::<HashMap<_, _>>();
        let summary_source_counts = self
            .summary_sources
            .iter()
            .map(|(id, messages)| (id.clone(), messages.len()))
            .collect::<HashMap<_, _>>();
        let message_inclusions = projection.message_inclusions();
        let mut entries = messages
            .iter()
            .zip(message_tokens.iter().copied())
            .map(|(message, tokens)| {
                let (role, text) = describe_message(message);
                ContextEntry { role, tokens, text }
            })
            .collect::<Vec<_>>();
        if let Some(message) = volatile_terminal_message.as_ref() {
            let (role, text) = describe_message(message);
            entries.push(ContextEntry {
                role,
                tokens: volatile_terminal_tokens,
                text,
            });
        }
        if estimate.tool_schema_tokens > 0 {
            entries.push(ContextEntry {
                role: ContextRole::ToolSchema,
                tokens: estimate.tool_schema_tokens,
                text: format!(
                    "{} active tool definitions: {}",
                    tool_schema.names().len(),
                    tool_schema.names().join(", ")
                ),
            });
        }
        let mut ledger = context_ledger_for_messages(ContextLedgerBuildInput {
            messages,
            message_ids: &message_ids,
            message_tokens: &message_tokens,
            tools: tool_schema.tools(),
            estimate: &estimate,
            row_metadata,
            base_message_count,
            pending_by_index: &pending_by_index,
            preview,
            persona: self.system_prompt_for_mode(mode),
            project_context: self.project_context.as_ref(),
            prompt_estimator: &self.prompt_estimator,
            tool_context_details: &self.tool_context_details,
            mention_read_evidence: &self.read_evidence.mention_read_evidence,
            summary_source_counts: &summary_source_counts,
            message_inclusions: &message_inclusions,
        });
        if let Some(message) = volatile_terminal_message.as_ref() {
            let (_role, text) = describe_message(message);
            append_volatile_terminal_ledger_node(
                &mut ledger,
                &text,
                volatile_terminal_tokens,
                row_metadata,
            );
        }
        add_framing_adjustment(&mut ledger, estimate.input_tokens, &estimate);
        let payload_preview = self
            .caches
            .last_perf_report
            .as_ref()
            .and_then(|report| report.prompt.request_preview.clone());
        let (cacheable_prefix_tokens, volatile_tail_tokens) = prompt_cache_token_totals(
            messages,
            &message_tokens,
            base_message_count,
            self.system_prompt_for_mode(mode),
            self.project_context.as_ref(),
            &self.prompt_estimator,
            estimate.tool_schema_tokens,
        );
        let volatile_tail_tokens = volatile_tail_tokens.saturating_add(volatile_terminal_tokens);
        ContextReport {
            budget_tokens: self.budget.context_budget_tokens,
            entries,
            ledger,
            estimate_source: estimate.source,
            estimate_confidence: estimate.confidence,
            prompt_estimate_tokens: estimate.input_tokens,
            tool_schema_tokens: estimate.tool_schema_tokens,
            cacheable_prefix_tokens,
            volatile_tail_tokens,
            last_prompt_tokens: self.usage.last_usage.map(|u| u.prompt_tokens),
            last_completion_tokens: self.usage.last_usage.map(|u| u.completion_tokens),
            last_input_cache: self.usage.last_usage.and_then(|u| u.input_cache),
            last_turn_cost_micros: self.usage.last_turn_cost_micros,
            last_turn_savings_micros: self.usage.last_turn_savings_micros(),
            session_prompt_tokens: self.usage.prompt_tokens,
            session_completion_tokens: self.usage.completion_tokens,
            session_input_cache: self.session_input_cache(),
            session_cost_micros: self.usage.session_cost_micros(),
            session_savings_micros: self.usage.session_savings_micros(),
            controls: self.context_controls.clone(),
            summary_sources: self.summary_sources.keys().cloned().collect(),
            compaction_events: self.compaction_events.clone(),
            episodes: self.episode_reports(),
            usage_turns: self.usage.turn_reports(),
            payload_preview,
            reconciliation: self.context_reconciliation(),
        }
    }

    pub(super) fn context_reconciliation(&self) -> Option<ContextUsageReconciliation> {
        let estimate = self.caches.last_sent_prompt_estimate.as_ref()?;
        let usage = self.usage.last_usage?;
        Some(ContextUsageReconciliation {
            estimated_prompt_tokens: estimate.input_tokens,
            actual_prompt_tokens: usage.prompt_tokens,
            actual_completion_tokens: usage.completion_tokens,
            provider_delta_tokens: i64::from(usage.prompt_tokens)
                - i64::try_from(estimate.input_tokens).unwrap_or(i64::MAX),
            estimate_source: estimate.source,
            estimate_confidence: estimate.confidence,
        })
    }
}

fn append_volatile_terminal_ledger_node(
    ledger: &mut Vec<ContextNode>,
    text: &str,
    tokens: usize,
    metadata: ContextTokenMetadata,
) {
    let node = ContextNode::leaf(
        ContextNodeId::raw("terminal-live"),
        ContextNodeKind::Background,
        ContextInclusion::Included,
        Some(ContextRole::User),
        "Live terminal state",
        tokens,
        text,
        metadata,
    )
    .with_source(ContextSourceRef::new(
        crate::context_view::ContextSourceKind::BackgroundTask,
        "terminal-live",
        "replaceable live terminal state",
    ));

    if let Some(index) = ledger
        .iter()
        .position(|root| root.kind == ContextNodeKind::ChatRoot)
    {
        let root = ledger.remove(index);
        let mut children = root.children;
        children.push(node);
        ledger.insert(
            index,
            aggregate_context_node(
                root.id,
                ContextNodeKind::ChatRoot,
                ContextInclusion::Included,
                None,
                "Chat",
                metadata,
                children,
            ),
        );
        return;
    }

    let index = ledger
        .iter()
        .position(|root| root.kind == ContextNodeKind::ToolsRoot)
        .unwrap_or(ledger.len());
    ledger.insert(
        index,
        aggregate_context_node(
            ContextNodeId::raw("root-chat"),
            ContextNodeKind::ChatRoot,
            ContextInclusion::Included,
            None,
            "Chat",
            metadata,
            vec![node],
        ),
    );
}
