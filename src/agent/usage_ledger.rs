//! Session token-usage and cost ledger: real usage/cost accounting
//! ([`SessionUsage`]), per-turn diagnostics fed into it
//! ([`UsageTurnDiagnostics`]), and the pending context-rewrite accumulator
//! ([`PendingContextRewrite`]) that compaction and continuous GC report
//! into. Fields are `pub(super)` so `lifecycle`, `builder`, and `mod.rs`
//! itself can read and build them.

use super::*;

impl UsageTurn {
    /// If this turn reported prompt tokens but no cache breakdown, treat it as
    /// fully-uncached input so `cache_measured_input_tokens` stays aligned with
    /// `prompt_tokens` — the invariant the session cache-hit-rate denominator
    /// depends on. No-op otherwise (already measured, or no usage reported).
    fn mark_fully_uncached_if_unmeasured(&mut self) {
        if self.prompt_tokens.is_some() && self.cache_measured_input_tokens.is_none() {
            self.cache_read_input_tokens = Some(0);
            self.cache_creation_input_tokens = Some(0);
            self.cache_measured_input_tokens = self.prompt_tokens;
            // Keep the cache-diagnosis field in lockstep with the counters: a
            // backfilled turn is 0% read, not unknown, so it renders `read 0%`
            // rather than `read n/a` in /ctx. Mirrors the live path, which sets
            // `actual_cache_read_percent = Some(0)` for an inferred-uncached turn.
            self.actual_cache_read_percent = Some(0);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct PendingContextRewrite {
    pub(super) kind: ContextRewriteKind,
    pub(super) saved_tokens: Option<usize>,
    /// Episodes evicted since the last provider turn. `episode_seq` attributes
    /// the turn's prefix churn only while exactly one episode caused it; a
    /// second eviction before the flush clears it back to `None`.
    pub(super) episode_evictions: usize,
    pub(super) episode_seq: Option<usize>,
}

impl PendingContextRewrite {
    pub(super) fn record(&mut self, kind: ContextRewriteKind, saved_tokens: usize) {
        if matches!(kind, ContextRewriteKind::None) {
            return;
        }
        self.saved_tokens = Some(self.saved_tokens.unwrap_or(0).saturating_add(saved_tokens));
        if kind.priority() >= self.kind.priority() {
            self.kind = kind;
        }
    }

    /// Note which episodes an applied eviction batch removed, for the
    /// single-episode attribution rule above.
    pub(super) fn record_episode_evictions(&mut self, evicted_seqs: &[usize]) {
        self.episode_evictions = self.episode_evictions.saturating_add(evicted_seqs.len());
        self.episode_seq = match (self.episode_evictions, evicted_seqs) {
            (1, [seq]) => Some(*seq),
            _ => None,
        };
    }

    pub(super) fn take(&mut self) -> Self {
        let current = *self;
        *self = Self::default();
        current
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct UsageTurnDiagnostics {
    pub(super) interrupted: bool,
    pub(super) finish_reason: Option<crate::provider::FinishReason>,
    pub(super) reasoning_chars: usize,
    pub(super) provider_attempts: Vec<ProviderAttemptReport>,
    pub(super) execution_lane: ExecutionLane,
    pub(super) provider_id: Option<String>,
    pub(super) model: Option<String>,
    pub(super) effective_reasoning: crate::provider::ReasoningSelection,
    pub(super) prompt_estimate: Option<PromptEstimate>,
    pub(super) tool_schema_hash: Option<String>,
    pub(super) tool_schema_names: Vec<String>,
    pub(super) request_body_bytes: Option<usize>,
    pub(super) request_body_hash: Option<String>,
    pub(super) cache_mechanism: Option<String>,
    pub(super) cache_route_fingerprint: Option<String>,
    pub(super) local_reusable_prefix_tokens: Option<usize>,
    pub(super) local_reusable_prefix_percent: Option<u64>,
    pub(super) cacheable_prefix_tokens: Option<usize>,
    pub(super) volatile_tail_tokens: Option<usize>,
    pub(super) context_window_tokens: Option<usize>,
    pub(super) rewrite: PendingContextRewrite,
    pub(super) created_at_ms: i64,
    pub(super) latency_ms: Option<u64>,
    pub(super) ttft_ms: Option<u64>,
    pub(super) prefix_hash: Option<String>,
}

/// Token-usage and cost accounting for a session: the most recent response plus
/// running session totals. Reset together via [`SessionUsage::default`], so no
/// individual field can be forgotten when clearing or restoring a session.
#[derive(Debug, Clone)]
pub(super) struct SessionUsage {
    /// Real token usage from the most recent provider response, when reported.
    pub(super) last_usage: Option<TokenUsage>,
    /// Cumulative real token usage across the session.
    pub(super) prompt_tokens: u64,
    pub(super) completion_tokens: u64,
    pub(super) cache_read_input_tokens: u64,
    pub(super) cache_creation_input_tokens: u64,
    pub(super) cache_measured_input_tokens: u64,
    /// Sticky once any turn has reported cache counters. Lets a later turn that
    /// reports usage *without* a cache breakdown be folded in as fully-uncached
    /// input, keeping `cache_measured_input_tokens` aligned with `prompt_tokens`
    /// so the session hit-rate denominator stays honest. Stays false for
    /// providers that never report cache, preserving the `cache n/a` display.
    pub(super) saw_input_cache: bool,
    /// Lanes whose provider has reported cache counters. A missing cache field
    /// becomes a 0% miss only after the same lane established support.
    pub(super) cache_reporting_lanes: HashSet<(ExecutionLaneKind, String)>,
    /// Real provider-priced cost for the most recent response, when known.
    pub(super) last_turn_cost_micros: Option<u64>,
    /// Cumulative provider-priced cost across turns whose pricing is known.
    pub(super) cost_micros: u64,
    /// What the most recent response would have cost with no prompt cache, when
    /// priced. Paired with [`last_turn_cost_micros`](Self::last_turn_cost_micros)
    /// to derive the turn's cache savings.
    pub(super) last_turn_no_cache_cost_micros: Option<u64>,
    /// Cumulative no-cache cost across priced turns. Accumulating this beside
    /// [`cost_micros`](Self::cost_micros) lets session savings net the
    /// cache-write premium of early turns against the reads of later ones.
    pub(super) no_cache_cost_micros: u64,
    /// False once a provider turn cannot be priced exactly. The known subtotal
    /// still accumulates for later priced turns; this flag only gates derived
    /// exact-session metrics such as cache savings.
    pub(super) cost_complete: bool,
    /// Per-provider-response usage diagnostics, oldest first.
    pub(super) usage_turns: Vec<UsageTurn>,
}

impl Default for SessionUsage {
    fn default() -> Self {
        Self {
            last_usage: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_measured_input_tokens: 0,
            saw_input_cache: false,
            cache_reporting_lanes: HashSet::new(),
            last_turn_cost_micros: None,
            cost_micros: 0,
            last_turn_no_cache_cost_micros: None,
            no_cache_cost_micros: 0,
            // A fresh session has no unpriced usage yet, so cost is complete.
            cost_complete: true,
            usage_turns: Vec::new(),
        }
    }
}

impl SessionUsage {
    /// Record real token usage from a provider response into the last-turn and
    /// session totals. `pricing` prices the turn when the model has pricing
    /// metadata; its absence marks the session cost as no longer fully known.
    #[cfg(test)]
    pub(super) fn record(
        &mut self,
        usage: Option<TokenUsage>,
        pricing: Option<ModelPricing>,
        diagnostics: UsageTurnDiagnostics,
    ) {
        let schedule = pricing.map(ModelPricingSchedule::flat);
        self.record_with_pricing_schedule(usage, schedule.as_ref(), diagnostics);
    }

    pub(super) fn record_with_pricing_schedule(
        &mut self,
        usage: Option<TokenUsage>,
        pricing: Option<&ModelPricingSchedule>,
        diagnostics: UsageTurnDiagnostics,
    ) {
        let pricing = pricing.map(|schedule| {
            usage
                .map(|usage| schedule.pricing_for_usage(usage))
                .unwrap_or_else(|| schedule.base())
        });
        self.last_usage = usage;
        self.last_turn_cost_micros = None;
        self.last_turn_no_cache_cost_micros = None;
        let mut turn = self.usage_turn_for(usage, pricing, diagnostics);
        let Some(usage) = usage else {
            self.cost_complete = false;
            self.usage_turns.push(turn);
            return;
        };

        self.prompt_tokens += u64::from(usage.prompt_tokens);
        self.completion_tokens += u64::from(usage.completion_tokens);
        if let Some(input_cache) = usage.input_cache {
            let lane_key = (turn.lane_kind, turn.lane_id.clone());
            if self.cache_reporting_lanes.insert(lane_key.clone()) {
                self.backfill_uncached_usage_turns_for_lane(lane_key.0, &lane_key.1);
            }
            if !self.saw_input_cache {
                // First cache-reporting turn: back-fill any earlier turns that
                // reported usage without a cache breakdown so the measured
                // denominator reflects total session input, not just input from
                // here on. `prompt_tokens` already includes this turn, so the
                // leading uncached prefix is everything but this turn's input.
                self.saw_input_cache = true;
                self.cache_measured_input_tokens = self
                    .prompt_tokens
                    .saturating_sub(input_cache.total_input_tokens);
            }
            self.cache_read_input_tokens = self
                .cache_read_input_tokens
                .saturating_add(input_cache.read_tokens);
            self.cache_creation_input_tokens = self
                .cache_creation_input_tokens
                .saturating_add(input_cache.creation_tokens);
            self.cache_measured_input_tokens = self
                .cache_measured_input_tokens
                .saturating_add(input_cache.total_input_tokens);
        } else if self.saw_input_cache {
            // Usage was reported but the provider omitted the cache breakdown for
            // this turn (e.g. a cold turn). Treat it as fully-uncached input so
            // the measured denominator keeps tracking total input.
            self.cache_measured_input_tokens = self
                .cache_measured_input_tokens
                .saturating_add(u64::from(usage.prompt_tokens));
        }
        self.last_turn_cost_micros = pricing.map(|pricing| pricing.cost_micros_for_usage(usage));
        if let Some(cost) = self.last_turn_cost_micros {
            self.cost_micros = self.cost_micros.saturating_add(cost);
            turn.turn_cost_micros = Some(cost);
        } else {
            self.cost_complete = false;
        }
        self.last_turn_no_cache_cost_micros =
            pricing.map(|pricing| pricing.no_cache_cost_micros_for_usage(usage));
        if let Some(no_cache) = self.last_turn_no_cache_cost_micros {
            self.no_cache_cost_micros = self.no_cache_cost_micros.saturating_add(no_cache);
            turn.no_cache_cost_micros = Some(no_cache);
        }
        self.usage_turns.push(turn);
    }

    fn usage_turn_for(
        &self,
        usage: Option<TokenUsage>,
        pricing: Option<ModelPricing>,
        diagnostics: UsageTurnDiagnostics,
    ) -> UsageTurn {
        let status = if diagnostics.interrupted {
            UsageTurnStatus::Interrupted
        } else if usage.is_some() {
            UsageTurnStatus::Reported
        } else {
            UsageTurnStatus::Missing
        };
        let input_cache = usage.and_then(|usage| usage.input_cache);
        let prompt_tokens = usage.map(|usage| u64::from(usage.prompt_tokens));
        let inferred_uncached_turn = usage.is_some()
            && input_cache.is_none()
            && self.cache_reporting_lanes.iter().any(|(kind, id)| {
                *kind == diagnostics.execution_lane.kind && id == &diagnostics.execution_lane.id
            });
        let estimated_prompt_tokens = diagnostics
            .prompt_estimate
            .as_ref()
            .map(|estimate| estimate.input_tokens);
        let expected_total = estimated_prompt_tokens.or_else(|| {
            Some(
                diagnostics
                    .cacheable_prefix_tokens?
                    .saturating_add(diagnostics.volatile_tail_tokens?),
            )
        });
        let expected_cacheable_percent = diagnostics
            .cacheable_prefix_tokens
            .zip(expected_total)
            .and_then(|(cacheable, total)| percent_u64(cacheable as u64, total as u64));
        let actual_cache_read_percent = input_cache
            .and_then(|cache| percent_u64(cache.read_tokens, cache.total_input_tokens))
            .or_else(|| inferred_uncached_turn.then_some(0));
        let lane_seq = self
            .usage_turns
            .iter()
            .filter(|turn| {
                turn.lane_kind == diagnostics.execution_lane.kind
                    && turn.lane_id == diagnostics.execution_lane.id
            })
            .count()
            + 1;
        UsageTurn {
            seq: self.usage_turns.len() + 1,
            lane_kind: diagnostics.execution_lane.kind,
            lane_id: diagnostics.execution_lane.id,
            lane_seq,
            parent_tool_call_id: None,
            launch_group_id: None,
            status,
            finish_reason: diagnostics.finish_reason,
            reasoning_chars: diagnostics.reasoning_chars,
            provider_attempts: diagnostics.provider_attempts,
            provider_id: diagnostics.provider_id,
            model: diagnostics.model,
            effective_reasoning: Some(diagnostics.effective_reasoning),
            prompt_tokens,
            completion_tokens: usage.map(|usage| u64::from(usage.completion_tokens)),
            cache_read_input_tokens: input_cache
                .map(|cache| cache.read_tokens)
                .or_else(|| inferred_uncached_turn.then_some(0)),
            cache_creation_input_tokens: input_cache
                .map(|cache| cache.creation_tokens)
                .or_else(|| inferred_uncached_turn.then_some(0)),
            cache_measured_input_tokens: input_cache
                .map(|cache| cache.total_input_tokens)
                .or_else(|| inferred_uncached_turn.then_some(prompt_tokens.unwrap_or(0))),
            turn_cost_micros: usage
                .and_then(|usage| pricing.map(|pricing| pricing.cost_micros_for_usage(usage))),
            no_cache_cost_micros: usage.and_then(|usage| {
                pricing.map(|pricing| pricing.no_cache_cost_micros_for_usage(usage))
            }),
            estimated_prompt_tokens,
            estimate_source: diagnostics
                .prompt_estimate
                .as_ref()
                .map(|estimate| estimate.source),
            estimate_confidence: diagnostics
                .prompt_estimate
                .as_ref()
                .map(|estimate| estimate.confidence),
            tool_schema_tokens: diagnostics
                .prompt_estimate
                .as_ref()
                .map(|estimate| estimate.tool_schema_tokens),
            tool_schema_hash: diagnostics.tool_schema_hash,
            tool_schema_names: diagnostics.tool_schema_names,
            request_body_bytes: diagnostics.request_body_bytes,
            request_body_hash: diagnostics.request_body_hash,
            cache_mechanism: diagnostics.cache_mechanism,
            cache_route_fingerprint: diagnostics.cache_route_fingerprint,
            expected_cacheable_percent,
            actual_cache_read_percent,
            local_reusable_prefix_tokens: diagnostics.local_reusable_prefix_tokens,
            local_reusable_prefix_percent: diagnostics.local_reusable_prefix_percent,
            cacheable_prefix_tokens: diagnostics.cacheable_prefix_tokens,
            volatile_tail_tokens: diagnostics.volatile_tail_tokens,
            context_window_tokens: diagnostics.context_window_tokens,
            rewrite_kind: diagnostics.rewrite.kind,
            rewrite_saved_tokens: diagnostics.rewrite.saved_tokens,
            episode_seq: diagnostics.rewrite.episode_seq,
            created_at_ms: diagnostics.created_at_ms,
            latency_ms: diagnostics.latency_ms,
            ttft_ms: diagnostics.ttft_ms,
            prefix_hash: diagnostics.prefix_hash,
            inspection_executed: 0,
            inspection_reused: 0,
            inspection_rejected: 0,
            inspection_returned_chars: 0,
            inspection_avoided_chars: 0,
            delegated_parent_overlap: 0,
        }
    }

    /// Overwrite the session totals from persisted state (no last-turn data).
    pub(super) fn restore(
        &mut self,
        prompt_tokens: u64,
        completion_tokens: u64,
        cost_micros: Option<u64>,
        no_cache_cost_micros: Option<u64>,
        input_cache: Option<InputCacheUsage>,
    ) {
        self.last_usage = None;
        self.last_turn_cost_micros = None;
        self.last_turn_no_cache_cost_micros = None;
        self.prompt_tokens = prompt_tokens;
        self.completion_tokens = completion_tokens;
        self.cache_read_input_tokens = input_cache.map(|usage| usage.read_tokens).unwrap_or(0);
        self.cache_creation_input_tokens =
            input_cache.map(|usage| usage.creation_tokens).unwrap_or(0);
        self.cache_measured_input_tokens = input_cache
            .map(|usage| usage.total_input_tokens)
            .unwrap_or(0);
        self.saw_input_cache = input_cache.is_some();
        self.cache_reporting_lanes.clear();
        self.cost_micros = cost_micros.unwrap_or(0);
        // Legacy snapshots have no persisted baseline. Falling back to actual
        // cost preserves their historical zero-savings behavior without
        // affecting new snapshots, which persist the exact no-cache total.
        self.no_cache_cost_micros = no_cache_cost_micros.unwrap_or(self.cost_micros);
        self.cost_complete = cost_micros.is_some();
    }

    pub(super) fn restore_turns(&mut self, turns: Vec<UsageTurn>) {
        if turns.iter().any(|turn| turn.turn_cost_micros.is_none()) {
            self.cost_complete = false;
        }
        if let Some((turn_cost, turn_no_cache_cost)) = priced_turn_cost_totals(&turns)
            && turn_cost == self.cost_micros
        {
            // IMPORTANT ACCOUNTING INVARIANT: persisted per-turn pricing is the
            // source of truth when it covers the restored actual-cost subtotal.
            // Never retain an older session-level no-cache baseline in that case.
            self.no_cache_cost_micros = turn_no_cache_cost;
        }
        self.usage_turns = turns;
        self.cache_reporting_lanes = self
            .usage_turns
            .iter()
            .filter(|turn| turn.cache_measured_input_tokens.is_some())
            .map(|turn| (turn.lane_kind, turn.lane_id.clone()))
            .collect();
        for (kind, id) in self.cache_reporting_lanes.clone() {
            self.backfill_uncached_usage_turns_for_lane(kind, &id);
        }
    }

    pub(super) fn absorb_turns(&mut self, turns: &[UsageTurn]) {
        if turns.iter().any(|turn| turn.turn_cost_micros.is_none()) {
            self.cost_complete = false;
        }
        let reporting_lanes = turns
            .iter()
            .filter(|turn| turn.cache_measured_input_tokens.is_some())
            .map(|turn| (turn.lane_kind, turn.lane_id.clone()))
            .collect::<HashSet<_>>();
        for (kind, id) in &reporting_lanes {
            if self.cache_reporting_lanes.insert((*kind, id.clone())) {
                self.backfill_uncached_usage_turns_for_lane(*kind, id);
            }
        }
        for turn in turns {
            let mut turn = turn.clone();
            turn.seq = self.usage_turns.len() + 1;
            if self
                .cache_reporting_lanes
                .contains(&(turn.lane_kind, turn.lane_id.clone()))
            {
                turn.mark_fully_uncached_if_unmeasured();
            }
            self.usage_turns.push(turn);
        }
    }

    pub(super) fn record_inspection(
        &mut self,
        lane: &ExecutionLane,
        admission: &ReadAdmissionMetadata,
    ) {
        let Some(turn) = self
            .usage_turns
            .iter_mut()
            .rev()
            .find(|turn| turn.lane_kind == lane.kind && turn.lane_id == lane.id)
        else {
            return;
        };
        match admission.outcome {
            InspectionOutcome::Executed => turn.inspection_executed += 1,
            InspectionOutcome::Reused => turn.inspection_reused += 1,
            InspectionOutcome::Rejected => turn.inspection_rejected += 1,
        }
        turn.inspection_returned_chars = turn
            .inspection_returned_chars
            .saturating_add(admission.returned_chars);
        turn.inspection_avoided_chars = turn
            .inspection_avoided_chars
            .saturating_add(admission.avoided_chars);
    }

    pub(super) fn record_delegated_parent_overlap(&mut self, lane: &ExecutionLane) {
        if let Some(turn) = self
            .usage_turns
            .iter_mut()
            .rev()
            .find(|turn| turn.lane_kind == lane.kind && turn.lane_id == lane.id)
        {
            turn.delegated_parent_overlap = turn.delegated_parent_overlap.saturating_add(1);
        }
    }

    fn backfill_uncached_usage_turns_for_lane(
        &mut self,
        lane_kind: ExecutionLaneKind,
        lane_id: &str,
    ) {
        for turn in self
            .usage_turns
            .iter_mut()
            .filter(|turn| turn.lane_kind == lane_kind && turn.lane_id == lane_id)
        {
            turn.mark_fully_uncached_if_unmeasured();
        }
    }

    /// Fold nested-agent totals into this session without replacing last-turn
    /// data. Nested calls are not a visible parent model turn, but they still
    /// contribute to cumulative `/ctx`, `/cost`, and persisted usage totals.
    pub(super) fn absorb_totals(&mut self, totals: UsageTotals) {
        if totals.prompt_tokens == 0
            && totals.completion_tokens == 0
            && totals.cost_micros == Some(0)
            && totals.input_cache.is_none()
        {
            return;
        }

        let previous_prompt_tokens = self.prompt_tokens;
        self.prompt_tokens = self.prompt_tokens.saturating_add(totals.prompt_tokens);
        self.completion_tokens = self
            .completion_tokens
            .saturating_add(totals.completion_tokens);

        if let Some(input_cache) = totals.input_cache {
            if !self.saw_input_cache {
                self.saw_input_cache = true;
                self.cache_measured_input_tokens = previous_prompt_tokens;
            }
            self.cache_read_input_tokens = self
                .cache_read_input_tokens
                .saturating_add(input_cache.read_tokens);
            self.cache_creation_input_tokens = self
                .cache_creation_input_tokens
                .saturating_add(input_cache.creation_tokens);
            self.cache_measured_input_tokens = self
                .cache_measured_input_tokens
                .saturating_add(input_cache.total_input_tokens);
        } else if self.saw_input_cache {
            self.cache_measured_input_tokens = self
                .cache_measured_input_tokens
                .saturating_add(totals.prompt_tokens);
        }

        if let Some(cost) = totals.cost_micros {
            self.cost_micros = self.cost_micros.saturating_add(cost);
            self.no_cache_cost_micros = self
                .no_cache_cost_micros
                .saturating_add(totals.no_cache_cost_micros.unwrap_or(cost));
        } else {
            self.cost_complete = false;
        }
    }

    pub(super) fn input_cache(&self) -> Option<InputCacheUsage> {
        (self.cache_measured_input_tokens > 0).then_some(InputCacheUsage::new(
            self.cache_read_input_tokens,
            self.cache_creation_input_tokens,
            self.cache_measured_input_tokens,
        ))
    }

    /// Prompt-cache savings for the most recent turn, when priced. Clamps to 0
    /// on a cache-*write* turn (cache creation bills above the input rate).
    pub(super) fn last_turn_savings_micros(&self) -> Option<u64> {
        match (
            self.last_turn_no_cache_cost_micros,
            self.last_turn_cost_micros,
        ) {
            (Some(no_cache), Some(cost)) => Some(no_cache.saturating_sub(cost)),
            _ => None,
        }
    }

    /// Cumulative prompt-cache savings across the session, when cost is known.
    pub(super) fn session_savings_micros(&self) -> Option<u64> {
        self.cost_complete
            .then(|| self.no_cache_cost_micros.saturating_sub(self.cost_micros))
    }

    /// Cumulative known cost across priced turns.
    ///
    /// Returns `None` only when no priced turn has contributed any subtotal and
    /// exact cost is already known to be incomplete. A later valid provider usage
    /// response makes the known subtotal visible again instead of leaving the
    /// session stuck at `n/a`.
    pub(super) fn session_cost_micros(&self) -> Option<u64> {
        (self.cost_complete || self.cost_micros > 0).then_some(self.cost_micros)
    }

    /// Cumulative cost only when every provider turn had usage and pricing.
    pub(super) fn exact_session_cost_micros(&self) -> Option<u64> {
        self.cost_complete.then_some(self.cost_micros)
    }

    /// Foreground parent provider turns durably recorded across resumes.
    /// Delegated/self-review/compaction lanes retain their own bounded policies
    /// and are excluded so concurrent nested work cannot race this counter.
    pub(super) fn session_turn_count(&self) -> usize {
        self.usage_turns
            .iter()
            .filter(|turn| turn.lane_kind == ExecutionLaneKind::Parent)
            .count()
    }

    /// Streamed reasoning and assistant characters across persisted foreground
    /// attempts, including failed retries that still consumed provider output.
    pub(super) fn session_output_chars(&self) -> usize {
        self.usage_turns
            .iter()
            .filter(|turn| turn.lane_kind == ExecutionLaneKind::Parent)
            .flat_map(|turn| &turn.provider_attempts)
            .fold(0usize, |total, attempt| {
                total
                    .saturating_add(attempt.assistant_chars)
                    .saturating_add(attempt.reasoning_chars)
            })
    }

    pub(super) fn totals(&self) -> UsageTotals {
        UsageTotals {
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            cost_micros: self.session_cost_micros(),
            no_cache_cost_micros: self
                .session_cost_micros()
                .map(|_| self.no_cache_cost_micros),
            input_cache: self.input_cache(),
        }
    }

    pub(super) fn turn_reports(&self) -> Vec<UsageTurnReport> {
        self.usage_turns.iter().map(UsageTurnReport::from).collect()
    }
}

fn priced_turn_cost_totals(turns: &[UsageTurn]) -> Option<(u64, u64)> {
    let mut saw_priced_turn = false;
    let mut actual = 0u64;
    let mut no_cache = 0u64;
    for turn in turns {
        match (turn.turn_cost_micros, turn.no_cache_cost_micros) {
            (Some(turn_actual), Some(turn_no_cache)) => {
                saw_priced_turn = true;
                actual = actual.saturating_add(turn_actual);
                no_cache = no_cache.saturating_add(turn_no_cache);
            }
            (Some(_), None) => return None,
            (None, _) => {}
        }
    }
    saw_priced_turn.then_some((actual, no_cache))
}

fn percent_u64(part: u64, total: u64) -> Option<u64> {
    (total > 0).then(|| part.saturating_mul(100).checked_div(total).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::{SessionUsage, UsageTurnDiagnostics};
    use crate::agent::{
        ContextRewriteKind, ExecutionLane, ExecutionLaneKind, UsageTotals, UsageTurn,
        UsageTurnStatus,
    };
    use crate::provider::{
        InputCacheUsage, ModelPricing, ModelPricingSchedule, ModelPricingTier, TokenUsage,
    };

    fn pricing() -> ModelPricing {
        ModelPricing::new(3_000_000, 15_000_000).with_cache_rates(Some(300_000), Some(3_750_000))
    }

    /// A turn that reads most of its prompt from the cache.
    fn read_heavy() -> TokenUsage {
        TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            input_cache: Some(InputCacheUsage::new(900_000, 0, 1_000_000)),
        }
    }

    /// A turn that writes the whole prompt into the cache (the priced premium).
    fn write_heavy() -> TokenUsage {
        TokenUsage {
            prompt_tokens: 500_000,
            completion_tokens: 0,
            input_cache: Some(InputCacheUsage::new(0, 500_000, 500_000)),
        }
    }

    #[test]
    fn read_heavy_turn_reports_positive_savings() {
        let mut usage = SessionUsage::default();
        usage.record(
            Some(read_heavy()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );
        let saved = usage
            .last_turn_savings_micros()
            .expect("a priced turn has known savings");
        assert!(saved > 0);
        assert_eq!(usage.session_savings_micros(), Some(saved));
    }

    #[test]
    fn write_heavy_turn_clamps_savings_to_zero() {
        // Cache creation bills above the input rate, so this turn costs more than
        // the no-cache baseline — per-turn savings clamp to zero rather than go
        // negative.
        let mut usage = SessionUsage::default();
        usage.record(
            Some(write_heavy()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );
        assert_eq!(usage.last_turn_savings_micros(), Some(0));
    }

    #[test]
    fn tiered_pricing_uses_actual_prompt_tokens_for_the_whole_turn() {
        let base = ModelPricing::new(2_000_000, 12_000_000).with_cache_rates(Some(200_000), None);
        let high = ModelPricing::new(4_000_000, 18_000_000).with_cache_rates(Some(400_000), None);
        let schedule = ModelPricingSchedule::new(
            base,
            vec![ModelPricingTier {
                above_input_tokens: 200_000,
                pricing: high,
            }],
        );
        let high_tier_usage = TokenUsage {
            prompt_tokens: 300_000,
            completion_tokens: 100_000,
            input_cache: Some(InputCacheUsage::new(100_000, 0, 300_000)),
        };
        let mut usage = SessionUsage::default();

        usage.record_with_pricing_schedule(
            Some(high_tier_usage),
            Some(&schedule),
            UsageTurnDiagnostics::default(),
        );

        assert_eq!(
            usage.last_turn_cost_micros,
            Some(high.cost_micros_for_usage(high_tier_usage))
        );
        assert_ne!(
            usage.last_turn_cost_micros,
            Some(base.cost_micros_for_usage(high_tier_usage))
        );
    }

    #[test]
    fn session_savings_net_write_then_read() {
        let mut usage = SessionUsage::default();
        usage.record(
            Some(write_heavy()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        ); // premium turn
        usage.record(
            Some(read_heavy()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        ); // saving turn
        let net = usage.session_savings_micros().expect("priced session");
        // The session total nets the early write premium against the later read.
        assert_eq!(
            net,
            usage.no_cache_cost_micros.saturating_sub(usage.cost_micros)
        );
        assert!(net > 0);
    }

    #[test]
    fn unpriced_turn_has_no_savings() {
        let mut usage = SessionUsage::default();
        usage.record(Some(read_heavy()), None, UsageTurnDiagnostics::default());
        assert_eq!(usage.last_turn_savings_micros(), None);
        assert_eq!(usage.session_savings_micros(), None);
        assert_eq!(usage.exact_session_cost_micros(), None);
        assert_eq!(usage.totals().cost_micros, None);
    }

    #[test]
    fn exact_cost_requires_every_turn_to_be_priced() {
        let mut usage = SessionUsage::default();
        usage.record(
            Some(read_heavy()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );
        assert_eq!(usage.exact_session_cost_micros(), Some(usage.cost_micros));

        usage.record(None, Some(pricing()), UsageTurnDiagnostics::default());

        assert_eq!(usage.exact_session_cost_micros(), None);
    }

    #[test]
    fn priced_turns_keep_counting_after_missing_usage_gap() {
        let mut usage = SessionUsage::default();
        let first = no_cache_breakdown();
        let second = read_heavy();
        usage.record(
            Some(first),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );
        usage.record(None, Some(pricing()), UsageTurnDiagnostics::default());
        usage.record(
            Some(second),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );

        let expected_cost = pricing()
            .cost_micros_for_usage(first)
            .saturating_add(pricing().cost_micros_for_usage(second));
        let totals = usage.totals();
        assert_eq!(totals.prompt_tokens, 1_400_000);
        assert_eq!(totals.completion_tokens, 0);
        assert_eq!(totals.cost_micros, Some(expected_cost));
        assert_eq!(usage.session_savings_micros(), None);
    }

    /// A turn that reports usage but no cache breakdown (cold turn / provider
    /// omitted the field).
    fn no_cache_breakdown() -> TokenUsage {
        TokenUsage {
            prompt_tokens: 400_000,
            completion_tokens: 0,
            input_cache: None,
        }
    }

    fn usage_turn_without_cache(prompt_tokens: u64) -> UsageTurn {
        UsageTurn {
            seq: 1,
            lane_kind: ExecutionLaneKind::Parent,
            lane_id: "local".to_string(),
            lane_seq: 1,
            parent_tool_call_id: None,
            launch_group_id: None,
            status: UsageTurnStatus::Reported,
            finish_reason: None,
            reasoning_chars: 0,
            provider_attempts: Vec::new(),
            provider_id: None,
            model: None,
            effective_reasoning: None,
            prompt_tokens: Some(prompt_tokens),
            completion_tokens: Some(0),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            cache_measured_input_tokens: None,
            turn_cost_micros: None,
            no_cache_cost_micros: None,
            estimated_prompt_tokens: None,
            estimate_source: None,
            estimate_confidence: None,
            tool_schema_tokens: None,
            tool_schema_hash: None,
            tool_schema_names: Vec::new(),
            request_body_bytes: None,
            request_body_hash: None,
            cache_mechanism: None,
            cache_route_fingerprint: None,
            expected_cacheable_percent: None,
            actual_cache_read_percent: None,
            local_reusable_prefix_tokens: None,
            local_reusable_prefix_percent: None,
            cacheable_prefix_tokens: None,
            volatile_tail_tokens: None,
            context_window_tokens: None,
            rewrite_kind: ContextRewriteKind::None,
            rewrite_saved_tokens: None,
            episode_seq: None,
            created_at_ms: 0,
            latency_ms: None,
            ttft_ms: None,
            prefix_hash: None,
            inspection_executed: 0,
            inspection_reused: 0,
            inspection_rejected: 0,
            inspection_returned_chars: 0,
            inspection_avoided_chars: 0,
            delegated_parent_overlap: 0,
        }
    }

    #[test]
    fn session_budget_usage_counts_persisted_turns_and_retry_output() {
        let mut usage = SessionUsage::default();
        let mut turn = usage_turn_without_cache(10);
        turn.provider_attempts = vec![
            crate::agent::ProviderAttemptReport {
                attempt: 1,
                outcome: crate::agent::ProviderAttemptOutcome::Failed,
                latency_ms: 10,
                assistant_chars: 7,
                reasoning_chars: 3,
                finish_reason: None,
                error_class: Some("rate limit".to_string()),
                backoff_ms: Some(1),
                prompt_tokens: None,
                completion_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                cache_measured_input_tokens: None,
            },
            crate::agent::ProviderAttemptReport {
                attempt: 2,
                outcome: crate::agent::ProviderAttemptOutcome::Completed,
                latency_ms: 20,
                assistant_chars: 11,
                reasoning_chars: 5,
                finish_reason: Some(crate::provider::FinishReason::Stop),
                error_class: None,
                backoff_ms: None,
                prompt_tokens: Some(10),
                completion_tokens: Some(4),
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                cache_measured_input_tokens: None,
            },
        ];
        let mut nested = turn.clone();
        nested.seq = 2;
        nested.lane_kind = ExecutionLaneKind::Subagent;
        nested.lane_id = "child".to_string();
        nested.provider_attempts[0].assistant_chars = 1_000;
        usage.usage_turns.push(turn);
        usage.usage_turns.push(nested);

        assert_eq!(usage.session_turn_count(), 1);
        assert_eq!(usage.session_output_chars(), 26);
    }

    #[test]
    fn absorption_preserves_lane_sequences_and_combined_order() {
        let mut usage = SessionUsage::default();
        let parent = ExecutionLane::parent("session-7");
        usage.record(
            Some(no_cache_breakdown()),
            Some(pricing()),
            UsageTurnDiagnostics {
                execution_lane: parent.clone(),
                ..Default::default()
            },
        );

        let children = (1..=4)
            .map(|index| {
                let mut turn = usage_turn_without_cache(10);
                turn.lane_kind = ExecutionLaneKind::Subagent;
                turn.lane_id = format!("sub-{index}");
                turn.lane_seq = 1;
                turn.prefix_hash = Some("shared-prefix".to_string());
                turn
            })
            .collect::<Vec<_>>();
        usage.absorb_turns(&children);
        usage.record(
            Some(no_cache_breakdown()),
            Some(pricing()),
            UsageTurnDiagnostics {
                execution_lane: parent,
                ..Default::default()
            },
        );

        assert_eq!(usage.usage_turns.last().unwrap().seq, 6);
        assert_eq!(usage.usage_turns.last().unwrap().lane_seq, 2);
        let child_lanes = usage
            .usage_turns
            .iter()
            .filter(|turn| turn.lane_kind == ExecutionLaneKind::Subagent)
            .map(|turn| turn.lane_id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(child_lanes.len(), 4);
        assert!(
            usage
                .usage_turns
                .iter()
                .filter(|turn| turn.lane_kind == ExecutionLaneKind::Subagent)
                .all(|turn| turn.lane_seq == 1
                    && turn.prefix_hash.as_deref() == Some("shared-prefix"))
        );
    }

    #[test]
    fn cache_then_uncached_turn_keeps_denominator_aligned() {
        let mut usage = SessionUsage::default();
        usage.record(
            Some(read_heavy()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );
        usage.record(
            Some(no_cache_breakdown()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );

        // The cold turn folds into the measured denominator as fully-uncached, so
        // it tracks total input and correctly drags the hit rate down.
        let cache = usage.input_cache().expect("session reported cache");
        assert_eq!(cache.total_input_tokens, 1_000_000 + 400_000);
        assert_eq!(cache.read_tokens, 900_000);
        assert_eq!(cache.total_input_tokens, usage.prompt_tokens);
    }

    #[test]
    fn leading_uncached_turn_is_backfilled_into_denominator() {
        // A backend that reports cache only once warm (turn 1 cold/None, turn 2
        // reports it). The cold prefix must still count toward the denominator,
        // or the hit rate reads optimistically high.
        let mut usage = SessionUsage::default();
        usage.record(
            Some(no_cache_breakdown()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        ); // 400k, None
        usage.record(
            Some(read_heavy()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        ); // 1_000k, read 900k

        let cache = usage.input_cache().expect("session reported cache");
        // Denominator = total session input, not just the cache-reporting turn.
        assert_eq!(cache.total_input_tokens, 400_000 + 1_000_000);
        assert_eq!(cache.total_input_tokens, usage.prompt_tokens);
        assert_eq!(cache.read_tokens, 900_000);
        // Honest hit rate: 900k / 1_400k = 64%, not the optimistic 90%.
        assert_eq!(cache.hit_rate_percent(), Some(642));
        // The backfilled leading turn renders `read 0%`, not `read n/a`: its
        // diagnosis field must match its now-zeroed cache counters. The warm
        // turn keeps its measured 900k/1000k = 90%.
        assert_eq!(usage.usage_turns[0].actual_cache_read_percent, Some(0));
        assert_eq!(usage.usage_turns[1].actual_cache_read_percent, Some(90));
    }

    #[test]
    fn session_without_any_cache_reporting_stays_unknown() {
        let mut usage = SessionUsage::default();
        usage.record(
            Some(no_cache_breakdown()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );
        usage.record(
            Some(no_cache_breakdown()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );
        // No turn ever reported cache, so the figure stays `n/a`, not `0%`.
        assert_eq!(usage.input_cache(), None);
    }

    #[test]
    fn restore_preserves_historical_savings_and_accrues_new_savings() {
        let mut usage = SessionUsage::default();
        // Resume a session that previously saved 555,000 micros through cache.
        usage.restore(1_000_000, 0, Some(2_445_000), Some(3_000_000), None);
        assert_eq!(usage.session_savings_micros(), Some(555_000));
        // A post-resume read-heavy turn accrues real savings on top.
        usage.record(
            Some(read_heavy()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );
        let net = usage
            .session_savings_micros()
            .expect("priced resumed session");
        assert!(net > 555_000);
        assert_eq!(
            net,
            usage.no_cache_cost_micros.saturating_sub(usage.cost_micros)
        );
    }

    #[test]
    fn absorbed_nested_totals_contribute_to_session_totals() {
        let mut usage = SessionUsage::default();
        let base_usage = no_cache_breakdown();
        usage.record(
            Some(base_usage),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );
        usage.absorb_totals(UsageTotals {
            prompt_tokens: 10,
            completion_tokens: 5,
            cost_micros: Some(42),
            no_cache_cost_micros: Some(42),
            input_cache: Some(InputCacheUsage::new(6, 2, 10)),
        });

        let totals = usage.totals();
        assert_eq!(totals.prompt_tokens, 400_010);
        assert_eq!(totals.completion_tokens, 5);
        assert_eq!(
            totals.input_cache,
            Some(InputCacheUsage::new(6, 2, 400_010))
        );
        assert_eq!(
            totals.cost_micros,
            Some(pricing().cost_micros_for_usage(base_usage) + 42)
        );
        assert_eq!(usage.session_savings_micros(), Some(0));
    }

    #[test]
    fn child_cache_reporting_does_not_backfill_parent_turn_rows() {
        let mut usage = SessionUsage::default();
        usage.record(
            Some(no_cache_breakdown()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );

        usage.absorb_totals(UsageTotals {
            prompt_tokens: 10,
            completion_tokens: 0,
            cost_micros: Some(0),
            no_cache_cost_micros: Some(0),
            input_cache: Some(InputCacheUsage::new(8, 0, 10)),
        });
        let mut child = UsageTurn {
            cache_read_input_tokens: Some(8),
            cache_creation_input_tokens: Some(0),
            cache_measured_input_tokens: Some(10),
            ..usage_turn_without_cache(10)
        };
        child.lane_kind = ExecutionLaneKind::Subagent;
        child.lane_id = "sub-1".to_string();
        usage.absorb_turns(&[child]);

        assert_eq!(usage.usage_turns[0].cache_read_input_tokens, None);
        assert_eq!(usage.usage_turns[0].cache_measured_input_tokens, None);
        assert_eq!(usage.usage_turns[1].cache_read_input_tokens, Some(8));
        assert_eq!(usage.usage_turns[1].cache_measured_input_tokens, Some(10));
    }

    #[test]
    fn uncached_child_lane_stays_unknown_when_only_parent_reports_cache() {
        let mut usage = SessionUsage::default();
        usage.record(
            Some(read_heavy()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );

        usage.absorb_totals(UsageTotals {
            prompt_tokens: 25,
            completion_tokens: 0,
            cost_micros: Some(0),
            no_cache_cost_micros: Some(0),
            input_cache: None,
        });
        let mut child = usage_turn_without_cache(25);
        child.lane_kind = ExecutionLaneKind::Subagent;
        child.lane_id = "sub-1".to_string();
        usage.absorb_turns(&[child]);

        assert_eq!(usage.usage_turns[1].cache_read_input_tokens, None);
        assert_eq!(usage.usage_turns[1].cache_creation_input_tokens, None);
        assert_eq!(usage.usage_turns[1].cache_measured_input_tokens, None);
    }

    #[test]
    fn restored_turns_do_not_invent_lane_cache_from_aggregate() {
        let mut usage = SessionUsage::default();
        usage.restore(
            100,
            0,
            Some(10),
            Some(20),
            Some(InputCacheUsage::new(80, 0, 100)),
        );

        usage.restore_turns(vec![usage_turn_without_cache(100)]);

        assert_eq!(usage.usage_turns[0].cache_read_input_tokens, None);
        assert_eq!(usage.usage_turns[0].cache_creation_input_tokens, None);
        assert_eq!(usage.usage_turns[0].cache_measured_input_tokens, None);
    }

    #[test]
    fn restored_unpriced_turn_keeps_exact_cost_unknown() {
        let mut usage = SessionUsage::default();
        usage.restore(100, 0, Some(10), Some(10), None);

        usage.restore_turns(vec![usage_turn_without_cache(100)]);

        assert_eq!(usage.session_cost_micros(), Some(10));
        assert_eq!(usage.exact_session_cost_micros(), None);
    }

    #[test]
    fn restored_priced_turns_repair_stale_no_cache_aggregate() {
        let mut usage = SessionUsage::default();
        usage.restore(200, 20, Some(300), Some(50), None);
        let mut first = usage_turn_without_cache(100);
        first.turn_cost_micros = Some(100);
        first.no_cache_cost_micros = Some(240);
        let mut second = usage_turn_without_cache(100);
        second.turn_cost_micros = Some(200);
        second.no_cache_cost_micros = Some(360);

        usage.restore_turns(vec![first, second]);

        assert_eq!(usage.totals().cost_micros, Some(300));
        assert_eq!(usage.totals().no_cache_cost_micros, Some(600));
    }

    #[test]
    fn absorbed_unpriced_turn_keeps_exact_cost_unknown() {
        let mut usage = SessionUsage::default();
        usage.record(
            Some(no_cache_breakdown()),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );

        usage.absorb_turns(&[usage_turn_without_cache(25)]);

        assert_eq!(usage.exact_session_cost_micros(), None);
    }

    #[test]
    fn absorbed_unreported_nested_usage_keeps_known_subtotal() {
        let mut usage = SessionUsage::default();
        let base_usage = no_cache_breakdown();
        usage.record(
            Some(base_usage),
            Some(pricing()),
            UsageTurnDiagnostics::default(),
        );
        usage.absorb_totals(UsageTotals {
            prompt_tokens: 0,
            completion_tokens: 0,
            cost_micros: None,
            no_cache_cost_micros: None,
            input_cache: None,
        });

        let totals = usage.totals();
        assert_eq!(totals.prompt_tokens, 400_000);
        assert_eq!(totals.completion_tokens, 0);
        assert_eq!(
            totals.cost_micros,
            Some(pricing().cost_micros_for_usage(base_usage))
        );
        assert_eq!(usage.session_savings_micros(), None);
    }
}
