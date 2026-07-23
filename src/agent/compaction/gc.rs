//! Continuous context GC for tool outputs. This keeps the outgoing request
//! bounded between compaction passes by applying the same restorable stub
//! controls that `/ctx` already understands.

use super::*;

const SUPERSEDED_READ_STUB_MIN_CHARS: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextGcDecision {
    index: usize,
    reason: ContextStubReason,
}

#[derive(Debug, Clone)]
struct ContextGcCandidate {
    tool_id: String,
    decision: ContextGcDecision,
    stub: CompactionStub,
    is_new_stub: bool,
}

#[derive(Debug, Clone)]
struct ContextGcPlan {
    candidates: Vec<ContextGcCandidate>,
    before_tokens: usize,
    after_tokens: usize,
    new_stub_count: usize,
}

impl ContextGcPlan {
    fn saved_tokens(&self) -> usize {
        self.before_tokens.saturating_sub(self.after_tokens)
    }

    fn has_new_old_successful_output_stub(&self) -> bool {
        self.candidates.iter().any(|candidate| {
            candidate.is_new_stub
                && candidate.decision.reason == ContextStubReason::OldSuccessfulToolOutput
        })
    }

    fn has_new_superseded_read_stub(&self) -> bool {
        self.candidates.iter().any(|candidate| {
            candidate.is_new_stub && candidate.decision.reason == ContextStubReason::SupersededRead
        })
    }
}

impl Agent {
    /// Run continuous context GC over the stored messages.
    ///
    /// With `reclaim_old_outputs == false` (the every-turn pass) this only
    /// *re-asserts* stubs that already exist and restores the two rows that must
    /// be live (pinned rows and read-dedup pointer targets) — it never
    /// introduces a new stub, so the outgoing wire bytes stay byte-identical and
    /// the provider prompt cache stays warm. `reclaim_old_outputs == true` (the
    /// batched pressure pass, gated on context pressure) additionally stubs
    /// *new* superseded reads and old successful tool outputs; each such stub
    /// rewrites mid-history bytes and invalidates the prompt cache from that
    /// point on, so we pay that cost once per pressure episode rather than every
    /// turn. Returns the number of controls whose stub state changed.
    pub(in crate::agent) fn apply_context_gc(&mut self, reclaim_old_outputs: bool) -> usize {
        let read_reuse_targets = self.read_reuse_target_indices();
        let decisions = self.context_gc_decisions(reclaim_old_outputs, &read_reuse_targets);
        self.clear_stale_automatic_gc_controls(&read_reuse_targets);
        let plan = self.context_gc_plan(&decisions);
        if !self.should_apply_context_gc_plan(&plan) {
            return 0;
        }

        let mut changed = 0usize;
        for candidate in &plan.candidates {
            if !self.summary_sources.contains_key(&candidate.tool_id) {
                let sources = self.original_sources_for_message(
                    candidate.decision.index,
                    &candidate.stub.original,
                );
                self.summary_sources
                    .insert(candidate.tool_id.clone(), sources);
            }
            let state = self
                .context_controls
                .entry(candidate.tool_id.clone())
                .or_default();
            if !state.stubbed || state.stub_reason != Some(candidate.decision.reason) {
                changed = changed.saturating_add(1);
            }
            state.stubbed = true;
            state.stub_reason = Some(candidate.decision.reason);
        }

        self.prune_context_controls_to_current_messages();
        if changed > 0 {
            self.caches.last_prompt_estimate = None;
            self.caches.last_sent_prompt_estimate = None;
            self.record_context_rewrite(
                ContextRewriteKind::Gc,
                plan.before_tokens,
                plan.after_tokens,
            );
        }
        changed
    }

    fn context_gc_plan(&self, decisions: &HashMap<String, ContextGcDecision>) -> ContextGcPlan {
        let message_ids = self.message_ids_for_messages(self.messages.len());
        let tool_schema = self.active_tool_schema();
        let before_tokens = self
            .payload_message_token_estimate(
                &self.messages,
                &message_ids,
                &self.context_controls,
                &tool_schema,
            )
            .1
            .input_tokens;
        // Deferred: cloning `context_controls` only earns its keep once a
        // candidate actually reaches the mutation below. On the (default) turn
        // with no new stub, this avoids a full control-map clone entirely.
        let mut candidate_controls: Option<HashMap<String, ContextControlState>> = None;
        let mut candidates = Vec::new();

        for (tool_id, decision) in decisions {
            let Some(message) = self.messages.get(decision.index) else {
                continue;
            };
            let min_chars = match decision.reason {
                ContextStubReason::SupersededRead => SUPERSEDED_READ_STUB_MIN_CHARS,
                ContextStubReason::OldSuccessfulToolOutput | ContextStubReason::User => {
                    COMPACTION_TOOL_OUTPUT_STUB_MIN_CHARS
                }
            };
            let Some(stub) = self.tool_output_stub_for_message(
                decision.index,
                message,
                min_chars,
                Some(decision.reason),
            ) else {
                continue;
            };
            let existing = self
                .context_controls
                .get(tool_id)
                .copied()
                .unwrap_or_default();
            if existing.pinned || existing.stub_reason == Some(ContextStubReason::User) {
                continue;
            }

            let is_new_stub = !existing.stubbed || existing.stub_reason != Some(decision.reason);
            let controls = candidate_controls.get_or_insert_with(|| self.context_controls.clone());
            let state = controls.entry(tool_id.clone()).or_default();
            state.stubbed = true;
            state.stub_reason = Some(decision.reason);
            candidates.push(ContextGcCandidate {
                tool_id: tool_id.clone(),
                decision: *decision,
                stub,
                is_new_stub,
            });
        }

        let new_stub_count = candidates
            .iter()
            .filter(|candidate| candidate.is_new_stub)
            .count();
        let after_tokens = if new_stub_count == 0 {
            before_tokens
        } else {
            self.payload_message_token_estimate(
                &self.messages,
                &message_ids,
                candidate_controls
                    .as_ref()
                    .unwrap_or(&self.context_controls),
                &tool_schema,
            )
            .1
            .input_tokens
        };

        ContextGcPlan {
            candidates,
            before_tokens,
            after_tokens,
            new_stub_count,
        }
    }

    fn should_apply_context_gc_plan(&self, plan: &ContextGcPlan) -> bool {
        if plan.new_stub_count == 0 {
            return true;
        }
        let saved_tokens = plan.saved_tokens();
        let saves_enough = saved_tokens >= CONTEXT_REWRITE_MIN_SAVED_TOKENS
            || saved_tokens.saturating_mul(100)
                >= plan
                    .before_tokens
                    .saturating_mul(CONTEXT_REWRITE_MIN_SAVED_PERCENT);
        if plan.has_new_old_successful_output_stub() {
            return plan.before_tokens >= self.context_gc_trigger_tokens() && saves_enough;
        }

        if plan
            .before_tokens
            .saturating_add(self.output_reserve_tokens())
            > self.budget.context_budget_tokens
        {
            return true;
        }

        // New superseded-read stubs now only reach here on the batched pressure
        // pass (reclaim_old_outputs). They still earn a cheap last-chance apply
        // once automatic compaction is imminent, but low-value old-output
        // rewrites should not sneak in on that trigger alone; compaction will
        // handle the hard reclaim path.
        saves_enough
            || (plan.has_new_superseded_read_stub()
                && plan.before_tokens >= self.automatic_compaction_trigger_tokens())
    }

    /// Restore the two kinds of automatic stub that must not remain stubbed:
    /// rows the user pinned, and read-dedup pointer targets (a pointer's text
    /// promises its full copy is live above it). Everything else stays stubbed
    /// even once its original rationale has lapsed — a stale stub is byte-stable
    /// and restorable via `/ctx`, whereas un-stubbing rewrites mid-history bytes
    /// and breaks the prompt cache. Any actual restore is a wire-byte mutation,
    /// so it is tagged in `usage_turns` like every other rewrite.
    fn clear_stale_automatic_gc_controls(&mut self, read_reuse_targets: &HashSet<usize>) {
        let pointer_target_ids: HashSet<String> = read_reuse_targets
            .iter()
            .filter_map(|&index| self.messages.get(index).and_then(tool_message_call_id))
            .map(|call_id| ContextNodeId::tool(&call_id).into_string())
            .collect();
        let ids_to_clear = self
            .context_controls
            .iter()
            .filter(|(id, state)| {
                state.stubbed
                    && state.stub_reason != Some(ContextStubReason::User)
                    && (state.pinned || pointer_target_ids.contains(*id))
            })
            .map(|(id, _state)| id.clone())
            .collect::<Vec<_>>();
        if ids_to_clear.is_empty() {
            return;
        }

        let sources_to_remove = ids_to_clear
            .iter()
            .filter(|id| self.summary_source_matches_current_target(id))
            .cloned()
            .collect::<Vec<_>>();

        for id in &ids_to_clear {
            if let Some(state) = self.context_controls.get_mut(id) {
                state.stubbed = false;
                state.stub_reason = None;
            }
        }
        for id in sources_to_remove {
            self.summary_sources.remove(&id);
        }

        self.context_controls.retain(|_id, state| state.is_active());
        self.caches.last_prompt_estimate = None;
        self.caches.last_sent_prompt_estimate = None;
        // Restoring bytes grows the prompt (saved = 0), but it is still a
        // mid-history mutation the cache-diagnostics need to see attributed.
        self.record_context_rewrite(ContextRewriteKind::Gc, 0, 0);
    }

    fn summary_source_matches_current_target(&self, id: &str) -> bool {
        let Some(source) = self.summary_sources.get(id) else {
            return false;
        };
        let [original] = source.as_slice() else {
            return false;
        };
        self.messages.iter().enumerate().any(|(index, message)| {
            message == original
                && self
                    .message_control_ids_for(index, message)
                    .iter()
                    .any(|candidate| candidate == id)
        })
    }

    fn context_gc_decisions(
        &self,
        reclaim_old_outputs: bool,
        read_reuse_targets: &HashSet<usize>,
    ) -> HashMap<String, ContextGcDecision> {
        let mut decisions = HashMap::new();
        for index in self.superseded_read_indices() {
            if read_reuse_targets.contains(&index) {
                continue;
            }
            let Some(call_id) = self.messages.get(index).and_then(tool_message_call_id) else {
                continue;
            };
            let tool_id = ContextNodeId::tool(&call_id).into_string();
            // Keep an existing superseded stub asserted every turn, but only
            // introduce a *new* one on the batched pressure pass: adding a stub
            // here rewrites mid-history bytes and invalidates the provider
            // prompt cache from that point on, exactly like a new old-output
            // stub. Match on the reason (not just `stubbed`) so an entry stubbed
            // as an old output that later becomes superseded stays on the
            // old-output branch instead of flipping reason and churning.
            let already_stubbed_superseded =
                self.context_controls.get(&tool_id).is_some_and(|state| {
                    state.stubbed && state.stub_reason == Some(ContextStubReason::SupersededRead)
                });
            if !reclaim_old_outputs && !already_stubbed_superseded {
                continue;
            }
            decisions.insert(
                tool_id,
                ContextGcDecision {
                    index,
                    reason: ContextStubReason::SupersededRead,
                },
            );
        }

        let protected_tail = self.protected_tail_message_indices();
        for (index, message) in self.messages.iter().enumerate() {
            if protected_tail.contains(&index) || read_reuse_targets.contains(&index) {
                continue;
            }
            let Some(call_id) = tool_message_call_id(message) else {
                continue;
            };
            let tool_id = ContextNodeId::tool(&call_id).into_string();
            if decisions.contains_key(&tool_id) {
                continue;
            }
            if !self.old_successful_tool_output_is_gc_eligible(index, message, &call_id) {
                continue;
            }
            // Keep already-stubbed outputs stubbed so the prefix stays stable,
            // but only stub a *new* one when reclaiming under pressure: adding a
            // stub here is what breaks the prompt cache.
            let already_stubbed = self
                .context_controls
                .get(&tool_id)
                .is_some_and(|state| state.stubbed);
            if !reclaim_old_outputs && !already_stubbed {
                continue;
            }
            decisions.insert(
                tool_id,
                ContextGcDecision {
                    index,
                    reason: ContextStubReason::OldSuccessfulToolOutput,
                },
            );
        }
        decisions
    }

    fn protected_tail_message_indices(&self) -> HashSet<usize> {
        let groups = message_groups(&self.messages);
        let protected_groups = last_group_indices(&groups, COMPACTION_PROTECTED_TAIL_GROUPS);
        groups
            .into_iter()
            .enumerate()
            .filter(|(index, _group)| protected_groups.contains(index))
            .flat_map(|(_index, group)| group.indices.into_iter())
            .collect()
    }

    fn old_successful_tool_output_is_gc_eligible(
        &self,
        index: usize,
        message: &ChatCompletionRequestMessage,
        call_id: &str,
    ) -> bool {
        if index == 0 || self.message_has_control(index, message, |state| state.pinned) {
            return false;
        }
        let Some(detail) = self.tool_context_details.get(call_id) else {
            return false;
        };
        if !successful_tool_output_can_be_stubbed(detail) {
            return false;
        }
        let Ok(value) = serde_json::to_value(message) else {
            return false;
        };
        let original = message_content_text(&value);
        !original.trim_start().starts_with("[Compacted tool output]")
            && original.chars().count() >= COMPACTION_TOOL_OUTPUT_STUB_MIN_CHARS
    }
}

fn successful_tool_output_can_be_stubbed(detail: &ToolContextDetail) -> bool {
    match &detail.result {
        ToolContextResult::Command {
            exit_code,
            timed_out,
            ..
        } => exit_code == &Some(0) && !timed_out,
        // `recall` joins the read-shaped allowlist deliberately: recalled
        // archive pages must stay GC-stubbable or every recall becomes
        // permanent dead weight (the archive keeps serving future recalls).
        ToolContextResult::Text { .. } => matches!(
            detail.name.as_str(),
            "read" | "grep" | "glob" | "symbol_search" | "recall"
        ),
        ToolContextResult::BackgroundTaskStarted { .. }
        | ToolContextResult::SubagentStarted { .. }
        | ToolContextResult::Edit { .. }
        | ToolContextResult::Image { .. } => false,
    }
}
