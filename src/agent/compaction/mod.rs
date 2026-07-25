//! Context-compaction submodule: the automatic pre-flight check, the
//! `compact_context` orchestrator, and the budget/target arithmetic live here;
//! the draft, candidate, summary, and continuous-GC stages live in the child
//! modules. Most cross-file helpers stay `pub(super)` (visible within this
//! submodule only); the handful actually called from outside the cluster
//! (`run_loop`, `context_report`, `agent::tests`) are `pub(in crate::agent)`.

use super::*;
use crate::episode::EpisodeEvictionPass;

mod candidate;
mod draft;
mod gc;
mod helpers;
mod summary;
// `pub(in crate::agent)` (items inside are already agent-scoped) so the agent
// test module can construct a `CompactionDraft` to unit-test summary rendering.
pub(in crate::agent) mod types;

pub(in crate::agent) use draft::ReadAdmission;
use helpers::*;
pub(in crate::agent) use helpers::{
    CompactionCancelled, is_compaction_cancelled, message_groups, next_queued_messages,
};
use types::*;

/// The candidate-derived counters [`CompactionReport::from_candidate`] needs,
/// captured as plain values (not a `&CompactionCandidate`) because the
/// committed call site constructs its report after `candidate.messages` etc.
/// have already been moved into `Agent`'s fields.
struct CandidateReportCounts {
    messages_omitted: usize,
    tool_outputs_stubbed: usize,
    summary_source_available: bool,
    repacked: bool,
}

impl CompactionCandidate {
    fn report_counts(&self) -> CandidateReportCounts {
        CandidateReportCounts {
            messages_omitted: self.messages_omitted,
            tool_outputs_stubbed: self.tool_outputs_stubbed,
            summary_source_available: self.summary_source_available,
            repacked: self.summary_repacked,
        }
    }
}

/// The token/message counts framing a compaction outcome, shared by every
/// [`CompactionReport`] constructor.
struct CompactionFraming {
    before_tokens: usize,
    after_tokens: usize,
    target_tokens: usize,
    before_messages: usize,
    after_messages: usize,
}

impl CompactionReport {
    /// Build a report from the resolved candidate's counters, the token/message
    /// counts framing it, and the two axes (preview vs. committed,
    /// [`CompactionSummarySource`]) that distinguish the preview and committed
    /// reports. Centralizes the candidate → report field mapping so the
    /// preview and committed call sites in [`Agent::compact_context`] can never
    /// drift apart on `repacked`/`target_reached`/etc.
    fn from_candidate(
        mode: CompactionMode,
        preview: bool,
        summary_source: CompactionSummarySource,
        framing: CompactionFraming,
        counts: CandidateReportCounts,
    ) -> Self {
        Self {
            mode,
            preview,
            summary_source,
            before_tokens: framing.before_tokens,
            after_tokens: framing.after_tokens,
            target_tokens: framing.target_tokens,
            before_messages: framing.before_messages,
            after_messages: framing.after_messages,
            messages_omitted: counts.messages_omitted,
            tool_outputs_stubbed: counts.tool_outputs_stubbed,
            summary_source_available: counts.summary_source_available,
            repacked: counts.repacked,
            target_reached: framing.after_tokens <= framing.target_tokens,
        }
    }

    /// A report for a no-op compaction: nothing was omitted or stubbed, so
    /// after == before on every axis.
    fn no_change(
        mode: CompactionMode,
        preview: bool,
        before_tokens: usize,
        target_tokens: usize,
        before_messages: usize,
    ) -> Self {
        Self {
            mode,
            preview,
            summary_source: if preview {
                CompactionSummarySource::Preview
            } else {
                CompactionSummarySource::None
            },
            before_tokens,
            after_tokens: before_tokens,
            target_tokens,
            before_messages,
            after_messages: before_messages,
            messages_omitted: 0,
            tool_outputs_stubbed: 0,
            summary_source_available: false,
            repacked: false,
            target_reached: before_tokens <= target_tokens,
        }
    }
}

/// Repack telemetry recorded on a [`CompactionEvent`], computed as one unit so
/// the event's parallel `Option` fields are all set together or not at all.
struct RepackTelemetry {
    id: String,
    reason: String,
    prefix_hash_before: String,
    prefix_hash_after: String,
    cacheable_prefix_tokens_before: usize,
    cacheable_prefix_tokens_after: usize,
}

impl Agent {
    /// Project the current messages into the outgoing form and estimate their
    /// token cost. The single place `prepare_context_for_model` re-derives
    /// `(request_messages, estimate)` after a step that may have changed
    /// `self.messages` (GC reclaim, compaction), so the two can never drift out
    /// of sync with each other.
    async fn reestimate_outgoing(
        &self,
        tool_schema: &ToolSchemaPayload,
        perf: &mut PreflightPerfCapture,
    ) -> (Vec<ChatCompletionRequestMessage>, PromptEstimate) {
        let request_messages = self.outgoing_messages_for(&self.messages);
        let estimate = self
            .estimate_prompt_for_preflight(
                &request_messages,
                tool_schema.tools(),
                tool_schema.model_tool_schema_tokens(),
                perf,
            )
            .await;
        (request_messages, estimate)
    }

    pub(in crate::agent) async fn prepare_context_for_model(
        &mut self,
        tool_schema: &ToolSchemaPayload,
        sink: &SharedSink,
        cancellation_token: CancellationToken,
        perf: &mut PreflightPerfCapture,
    ) -> Result<Vec<ChatCompletionRequestMessage>> {
        // Episode lifecycle: consume any pending
        // title-change boundary first — preflight is the only point where the
        // turn is guaranteed to sit at a complete group boundary — then let
        // freshly closed episodes evict when their savings individually clear
        // the rewrite-economics guard. Runs before the advisory refresh so
        // advisories are computed against the post-eviction rows and never
        // point into a removed span.
        self.apply_pending_episode_title_signal();
        self.apply_episode_evictions(
            sink,
            cancellation_token.clone(),
            EpisodeEvictionPass::CloseTime,
        )
        .await?;
        self.refresh_read_evidence_freshness().await;
        self.refresh_stale_read_advisory();
        self.refresh_peer_status().await;
        // IMPORTANT CODEX CACHE INVARIANT: for append-only providers this must
        // happen after every volatile source refresh and before projection.
        // Other providers return false and retain their proven mutable-tail
        // strategy; do not make this behavior global again.
        self.append_volatile_context_if_changed();
        // Estimate the prompt as it currently stands — existing GC stubs
        // re-asserted, pinned/pointer-target rows restored — but without
        // introducing any *new* stub. New stubs (superseded reads and old tool
        // outputs alike) rewrite mid-history bytes and break the provider prompt
        // cache from that point, so they only earn their keep once the prompt is
        // actually under context pressure. Below the GC trigger the cached
        // prefix stays byte-stable and the cache stays warm.
        self.apply_context_gc(false);
        let (mut request_messages, mut estimate) =
            self.reestimate_outgoing(tool_schema, perf).await;
        // One batched reclaim when over the trigger. Closed episodes get first
        // refusal when their aggregate savings justify a cache break or resolve
        // the pressure alone; otherwise GC handles the pass without an
        // immediately redundant episode rewrite. GC then stubs every eligible
        // superseded read and old output at once, keeping the prefix stable for
        // many warm turns until it climbs back.
        if estimate.input_tokens >= self.context_gc_trigger_tokens() {
            if self
                .apply_episode_evictions(
                    sink,
                    cancellation_token.clone(),
                    EpisodeEvictionPass::Pressure,
                )
                .await?
                > 0
            {
                self.refresh_stale_read_advisory();
                (request_messages, estimate) = self.reestimate_outgoing(tool_schema, perf).await;
            }
            if estimate.input_tokens >= self.context_gc_trigger_tokens()
                && self.apply_context_gc(true) > 0
            {
                (request_messages, estimate) = self.reestimate_outgoing(tool_schema, perf).await;
            }
        }
        // Fire only when over the trigger *and* actually above the target. On
        // small windows the fixed output reserve can push the trigger below the
        // target; without the second check, compaction would run (and emit its
        // status) every turn only to no-op against an already-within-target
        // prompt.
        let target_tokens = self.default_compaction_target_tokens();
        if estimate.input_tokens >= self.automatic_compaction_trigger_tokens()
            && estimate.input_tokens > target_tokens
        {
            sink.status("Compacting conversation…");
            let mut request = CompactionRequest::automatic(target_tokens, cancellation_token);
            if self.smol_applies_to_active_persona() {
                request = request.with_summary_policy(CompactionSummaryPolicy::DeterministicOnly);
            }
            let report = self.compact_context(request).await?;
            if report.has_changes() {
                sink.compaction_status(&compaction_done_status(
                    &report,
                    self.compaction_events.len(),
                ));
                (request_messages, estimate) = self.reestimate_outgoing(tool_schema, perf).await;
            }
        }

        let still_over_budget = estimate
            .input_tokens
            .saturating_add(self.output_reserve_tokens())
            > self.budget.context_budget_tokens;
        self.caches.last_prompt_estimate = Some(estimate.clone());
        self.caches.last_sent_prompt_estimate = Some(estimate.clone());
        self.emit_context_updated(sink);

        if still_over_budget {
            let message = format!(
                "prompt estimate {} + reserve {} exceeds context window {} ({}, {})",
                estimate.input_tokens,
                self.output_reserve_tokens(),
                self.budget.context_budget_tokens,
                estimate.source.label(),
                estimate.confidence.label()
            );
            if matches!(estimate.confidence, EstimateConfidence::High) {
                bail!("{message}");
            }
            sink.status(&format!(
                "{message}; continuing because the estimate is low confidence"
            ));
            tracing::debug!(
                message = %message,
                "continuing with heuristic context fallback"
            );
        }

        Ok(request_messages)
    }

    pub(super) fn automatic_compaction_trigger_tokens(&self) -> usize {
        let percent = if self.smol_applies_to_active_persona() {
            50
        } else {
            AUTO_COMPACTION_TRIGGER_PERCENT
        };
        self.usable_window_fraction_tokens(percent)
    }

    /// Prompt size at which continuous GC starts reclaiming old tool outputs.
    /// Below it, history is left byte-stable so the provider prompt cache stays
    /// warm; see [`CONTEXT_GC_TRIGGER_PERCENT`].
    pub(super) fn context_gc_trigger_tokens(&self) -> usize {
        let percent = if self.smol_applies_to_active_persona() {
            35
        } else {
            CONTEXT_GC_TRIGGER_PERCENT
        };
        self.usable_window_fraction_tokens(percent)
    }

    /// `percent` of the usable input window (the context budget minus the output
    /// reserve), the shared basis for the GC and compaction triggers. Falls back
    /// to the full budget on windows too small to carry the reserve.
    fn usable_window_fraction_tokens(&self, percent: usize) -> usize {
        let usable_input_tokens = self
            .budget
            .context_budget_tokens
            .saturating_sub(self.output_reserve_tokens());
        let basis = if usable_input_tokens == 0 {
            self.budget.context_budget_tokens
        } else {
            usable_input_tokens
        };
        basis
            .saturating_mul(percent)
            .checked_div(100)
            .unwrap_or(basis)
            .max(1)
    }

    /// Compact stored chat context while keeping omitted originals available
    /// through `/ctx` restore controls.
    ///
    /// # Errors
    ///
    /// Returns an error when a required hidden-summary provider call fails or
    /// when cancellation is requested before the compaction can be applied.
    pub async fn compact_context(
        &mut self,
        request: CompactionRequest,
    ) -> Result<CompactionReport> {
        if request.cancellation_token.is_cancelled() {
            return Err(CompactionCancelled.into());
        }
        let tool_schema = self.active_tool_schema();
        let before_messages = self.messages.len();
        let before_tokens = self
            .prompt_estimator
            .estimate_prompt_with_tool_schema_tokens(
                &self.outgoing_messages_for(&self.messages),
                tool_schema.tools(),
                tool_schema.model_tool_schema_tokens(),
            )
            .await
            .input_tokens;
        let target_tokens = request
            .target_tokens
            .unwrap_or_else(|| self.default_compaction_target_tokens())
            .max(1);
        let draft = self.build_compaction_draft(target_tokens);
        if !draft.has_changes() {
            return Ok(CompactionReport::no_change(
                request.mode,
                request.preview,
                before_tokens,
                target_tokens,
                before_messages,
            ));
        }

        let deterministic_summary = self.deterministic_compaction_summary(&draft).await;
        if request.preview {
            let candidate =
                self.compaction_candidate(&draft, Some(&deterministic_summary), None)?;
            let after_tokens = self
                .estimate_candidate_tokens(
                    &candidate.messages,
                    &candidate.message_ids,
                    &candidate.controls,
                    &tool_schema,
                )
                .await;
            return Ok(CompactionReport::from_candidate(
                request.mode,
                true,
                CompactionSummarySource::Preview,
                CompactionFraming {
                    before_tokens,
                    after_tokens,
                    target_tokens: draft.target_tokens,
                    before_messages,
                    after_messages: candidate.messages.len(),
                },
                candidate.report_counts(),
            ));
        }

        if request.cancellation_token.is_cancelled() {
            return Err(CompactionCancelled.into());
        }

        let mut summary_source = CompactionSummarySource::None;
        let summary_text = if draft.omitted.is_empty() {
            None
        } else {
            match request.summary_policy {
                CompactionSummaryPolicy::DeterministicOnly => {
                    summary_source = CompactionSummarySource::Deterministic;
                    Some(deterministic_summary)
                }
                CompactionSummaryPolicy::ProviderOnly
                | CompactionSummaryPolicy::ProviderThenDeterministic => {
                    match self
                        .provider_compaction_summary(&draft, request.cancellation_token.clone())
                        .await
                    {
                        Ok(summary) => {
                            summary_source = CompactionSummarySource::Provider;
                            Some(summary)
                        }
                        Err(err)
                            if request.summary_policy
                                == CompactionSummaryPolicy::ProviderThenDeterministic
                                && !request.cancellation_token.is_cancelled()
                                && !is_compaction_cancelled(&err) =>
                        {
                            tracing::debug!(%err, "provider compaction summary failed; using deterministic fallback");
                            summary_source = CompactionSummarySource::Deterministic;
                            Some(deterministic_summary)
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
        };

        let summary_message_id = summary_text
            .as_ref()
            .map(|_summary| self.next_context_message_id());
        let candidate = self.compaction_candidate(
            &draft,
            summary_text.as_deref(),
            summary_message_id.as_deref(),
        )?;
        let after_tokens = self
            .estimate_candidate_tokens(
                &candidate.messages,
                &candidate.message_ids,
                &candidate.controls,
                &tool_schema,
            )
            .await;
        // Captured before the fields below are moved out of `candidate`.
        let counts = candidate.report_counts();
        let after_messages = candidate.messages.len();
        let event_seq = self.compaction_events.len() + 1;
        let before_message_ids = self.message_ids_for_messages(self.messages.len());
        // Compute repack telemetry as one all-or-nothing unit so the six event
        // fields below can't be half-populated.
        let repack = candidate.summary_repacked.then(|| RepackTelemetry {
            id: format!("repack-{event_seq}"),
            reason: request.mode.repack_reason_label().to_string(),
            prefix_hash_before: self.context_payload_fingerprint_for_controls(
                &self.messages,
                &before_message_ids,
                &self.context_controls,
            ),
            prefix_hash_after: self.context_payload_fingerprint_for_controls(
                &candidate.messages,
                &candidate.message_ids,
                &candidate.controls,
            ),
            cacheable_prefix_tokens_before: self.cacheable_prefix_tokens_for_controls(
                &self.messages,
                &before_message_ids,
                &self.context_controls,
                &tool_schema,
            ),
            cacheable_prefix_tokens_after: self.cacheable_prefix_tokens_for_controls(
                &candidate.messages,
                &candidate.message_ids,
                &candidate.controls,
                &tool_schema,
            ),
        });
        self.messages = candidate.messages;
        self.message_ids = candidate.message_ids;
        self.context_controls = candidate.controls;
        self.summary_sources = candidate.summary_sources;
        self.caches.last_prompt_estimate = None;
        self.caches.last_sent_prompt_estimate = None;

        self.compaction_events.push(CompactionEvent {
            seq: event_seq,
            occurred_at_ms: crate::util::time::now_ms(),
            before_tokens,
            after_tokens,
            messages_omitted: counts.messages_omitted,
            tool_outputs_stubbed: counts.tool_outputs_stubbed,
            summary_available: counts.summary_source_available,
            repack_id: repack.as_ref().map(|r| r.id.clone()),
            repack_reason: repack.as_ref().map(|r| r.reason.clone()),
            prefix_hash_before: repack.as_ref().map(|r| r.prefix_hash_before.clone()),
            prefix_hash_after: repack.as_ref().map(|r| r.prefix_hash_after.clone()),
            cacheable_prefix_tokens_before: repack
                .as_ref()
                .map(|r| r.cacheable_prefix_tokens_before),
            cacheable_prefix_tokens_after: repack.as_ref().map(|r| r.cacheable_prefix_tokens_after),
        });
        self.record_context_rewrite(request.mode.rewrite_kind(), before_tokens, after_tokens);

        Ok(CompactionReport::from_candidate(
            request.mode,
            false,
            summary_source,
            CompactionFraming {
                before_tokens,
                after_tokens,
                target_tokens: draft.target_tokens,
                before_messages,
                after_messages,
            },
            counts,
        ))
    }

    pub(in crate::agent) fn default_compaction_target_tokens(&self) -> usize {
        let percent = if self.smol_applies_to_active_persona() {
            35
        } else {
            COMPACTION_TARGET_PERCENT
        };
        let pct_target = self
            .budget
            .context_budget_tokens
            .saturating_mul(percent)
            .checked_div(100)
            .unwrap_or(self.budget.context_budget_tokens);
        // Never aim above the input budget the output reserve leaves free, or on
        // small windows compaction couldn't bring the prompt back under the limit
        // (input + reserve <= window) and the run would bail despite compacting.
        let budget_safe = self
            .budget
            .context_budget_tokens
            .saturating_sub(self.output_reserve_tokens());
        pct_target.min(budget_safe).max(1)
    }

    fn output_reserve_tokens(&self) -> usize {
        let budget = self.budget.context_budget_tokens;
        if self.smol_applies_to_active_persona() {
            return budget.checked_div(8).unwrap_or(0).clamp(512, 2_000);
        }
        // Cap the output reserve so it never exceeds ~67% of the context
        // window. A fixed 16k reserve is larger than the entire window for
        // small models (e.g. 10k tokens), causing the budget check to fail
        // on every turn. For larger windows (≥24k) the cap is above 16k so
        // the default reserve is kept intact.
        let max_for_window = ((budget / 3) * 2).max(1024);
        DEFAULT_OUTPUT_RESERVE_TOKENS.min(max_for_window)
    }
}
