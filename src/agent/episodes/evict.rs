//! Episode eviction: replace a closed episode's whole message groups with one
//! self-describing card marker, archiving the originals for `/ctx` restore and
//! recall. Eviction is ALL-OR-NOTHING per episode and batched — every eligible
//! episode in a preflight lands in ONE wire mutation, so the prompt cache pays
//! one break per pass instead of one per episode (the codex cache-thrash
//! failure mode is what this guards against).

use super::*;
use crate::agent::message_injection::harness_note;
use crate::episode::{
    ArchivedEpisodeItem, EpisodeCloseReason, EpisodeEvictionPass, EpisodeEvictionRecord,
    EpisodeStatus,
};

/// One validated, economics-cleared eviction: the episode plus its resolved
/// contiguous live span `[start ..= end]`.
struct EpisodeEviction {
    seq: usize,
    title: String,
    start: usize,
    end: usize,
    /// Estimated payload tokens the span currently costs.
    span_tokens: usize,
    /// A size-triggered close already chose a cache-breaking lane boundary;
    /// deferring it through the generic economics guard would mean "never".
    size_boundary: bool,
}

fn estimated_episode_savings(eviction: &EpisodeEviction) -> usize {
    eviction.span_tokens.saturating_sub(ESTIMATED_MARKER_TOKENS)
}

fn clears_episode_rewrite_guard(saved_tokens: usize, before_tokens: usize) -> bool {
    saved_tokens >= CONTEXT_REWRITE_MIN_SAVED_TOKENS
        || saved_tokens.saturating_mul(100)
            >= before_tokens.saturating_mul(CONTEXT_REWRITE_MIN_SAVED_PERCENT)
}

fn pressure_batch_earns_rewrite(
    saved_tokens: usize,
    before_tokens: usize,
    pressure_trigger_tokens: usize,
) -> bool {
    clears_episode_rewrite_guard(saved_tokens, before_tokens)
        || (before_tokens >= pressure_trigger_tokens
            && before_tokens.saturating_sub(saved_tokens) < pressure_trigger_tokens)
}

impl Agent {
    /// Evict eligible closed episodes. The close-time pass requires each
    /// episode to clear the rewrite-economics guard. The pressure pass batches
    /// deferred episodes and rewrites only when their aggregate savings clear
    /// that guard or the batch alone resolves the current pressure. Returns the
    /// number evicted; `Err` only on cancellation during a card call (the
    /// episodes stay Closed and retry next preflight).
    pub(in crate::agent) async fn apply_episode_evictions(
        &mut self,
        sink: &SharedSink,
        cancellation_token: CancellationToken,
        pass: EpisodeEvictionPass,
    ) -> Result<usize> {
        if self.episode_store.is_none() || self.smol_applies_to_active_persona() {
            return Ok(0);
        }
        let candidates = {
            let Some(ledger) = self.episode_ledger() else {
                return Ok(0);
            };
            ledger
                .episodes()
                .iter()
                .filter(|episode| {
                    episode.status() == EpisodeStatus::Closed
                        && matches!(
                            episode.close_reason(),
                            Some(EpisodeCloseReason::TitleChange)
                                | Some(EpisodeCloseReason::TodoComplete)
                                | Some(EpisodeCloseReason::SizePressure)
                                | Some(EpisodeCloseReason::Manual)
                        )
                })
                .cloned()
                .collect::<Vec<_>>()
        };
        if candidates.is_empty() {
            return Ok(0);
        }

        // Everything below validates against ONE immutable view of
        // `(messages, message_ids)`; no mutation happens until every span and
        // card is ready.
        let tool_schema = self.active_tool_schema();
        let message_ids = self.message_ids_for_messages(self.messages.len());
        let (message_tokens, before_estimate) = self.payload_message_token_estimate(
            &self.messages,
            &message_ids,
            &self.context_controls,
            &tool_schema,
        );
        let before_tokens = before_estimate.input_tokens;
        let groups = compaction::message_groups(&self.messages);
        let pointer_targets = self.read_reuse_target_indices();
        // The successor boundary to protect: the ACTIVE episode's start row
        // must sit outside (after) any evicted span, or the current task's
        // opening request would leave live context. Resolved with the ledger's
        // provenance-aware anchors rather than the generic latest-user helper,
        // which counts harness rows and is unsuitable here.
        let active_start = {
            let active = self.episode_ledger().and_then(|ledger| {
                ledger
                    .active()
                    .map(|episode| episode.start_stable_id().to_string())
            });
            match active {
                Some(start_id) => match self.message_ids.iter().position(|id| *id == start_id) {
                    Some(index) => Some(index),
                    // An active episode whose start no longer resolves: we
                    // cannot prove the successor is outside any span, so no
                    // eviction happens this pass.
                    None => return Ok(0),
                },
                None => None,
            }
        };

        let mut evictions = Vec::new();
        for episode in &candidates {
            let Some(eviction) =
                self.resolve_episode_span(episode, &groups, &pointer_targets, active_start)
            else {
                continue;
            };
            let span_tokens: usize = message_tokens[eviction.start..=eviction.end].iter().sum();
            evictions.push(EpisodeEviction {
                span_tokens,
                size_boundary: episode.close_reason() == Some(EpisodeCloseReason::SizePressure),
                ..eviction
            });
        }
        if evictions.is_empty() {
            return Ok(0);
        }

        // Persisted crash skew must never turn two overlapping episode rows
        // into overlapping descending splices. Resume validation repairs these
        // rows, and this local guard keeps runtime mutation safe even if a test
        // or future caller constructs a malformed ledger directly.
        let ranges = evictions
            .iter()
            .map(|eviction| (eviction.start, eviction.end, eviction.seq))
            .collect::<Vec<_>>();
        let overlapping = overlapping_span_seqs(ranges);
        evictions.retain(|eviction| !overlapping.contains(&eviction.seq));
        if evictions.is_empty() {
            return Ok(0);
        }

        // Marker cost is approximated with the same char/4 heuristic the
        // estimators bottom out in. Close-time candidates pay individually;
        // pressure candidates pay as one batch because they produce one cache
        // break. A small pressure batch is useful only when it gets the prompt
        // below the pressure trigger by itself; otherwise GC can reclaim space
        // without paying for an episode rewrite immediately before it.
        match pass {
            EpisodeEvictionPass::CloseTime => {
                evictions.retain(|eviction| {
                    self.eager_episode_eviction
                        || eviction.size_boundary
                        || clears_episode_rewrite_guard(
                            estimated_episode_savings(eviction),
                            before_tokens,
                        )
                });
            }
            EpisodeEvictionPass::Pressure => {
                let saved_tokens = evictions.iter().fold(0usize, |total, eviction| {
                    total.saturating_add(estimated_episode_savings(eviction))
                });
                if !pressure_batch_earns_rewrite(
                    saved_tokens,
                    before_tokens,
                    self.context_gc_trigger_tokens(),
                ) {
                    return Ok(0);
                }
            }
        }
        if evictions.is_empty() {
            return Ok(0);
        }

        // Cards render before any mutation, so a cancelled provider call
        // leaves the context untouched and the episodes Closed.
        let mut cards = Vec::with_capacity(evictions.len());
        for eviction in &evictions {
            let Some(episode) = candidates
                .iter()
                .find(|episode| episode.seq() == eviction.seq)
            else {
                return Ok(0);
            };
            let span = self.messages[eviction.start..=eviction.end].to_vec();
            let render = self
                .episode_card(episode, &span, cancellation_token.clone())
                .await?;
            self.record_episode_card_usage(&render)?;
            cards.push(render.card);
        }

        // Collect archives + restore sources from the immutable view.
        let mut archives = Vec::with_capacity(evictions.len());
        let mut restore_sources = Vec::with_capacity(evictions.len());
        let mut restore_source_stable_ids = Vec::with_capacity(evictions.len());
        for eviction in &evictions {
            let mut archive = Vec::new();
            let mut sources = Vec::new();
            let mut source_stable_ids = Vec::new();
            let span_rows = self.messages[eviction.start..=eviction.end]
                .iter()
                .zip(&message_ids[eviction.start..=eviction.end]);
            for (offset, (message, stable_id)) in span_rows.enumerate() {
                let (originals, stable_ids) =
                    self.original_source_snapshot_for_message(eviction.start + offset, message);
                // The archive keeps REAL bytes: a GC-stubbed row archives its
                // single original. A row whose sources fan out (nested prior
                // compaction) archives the live row itself — its originals
                // still ride the marker's restore source below.
                let archived = match originals.as_slice() {
                    [single] => single.clone(),
                    _ => message.clone(),
                };
                archive.push(ArchivedEpisodeItem {
                    stable_id: stable_id.clone(),
                    message: archived,
                });
                sources.extend(originals);
                source_stable_ids.extend(stable_ids);
            }
            archives.push(archive);
            restore_sources.push(sources);
            restore_source_stable_ids.push(source_stable_ids);
        }

        // Allocate marker ids and atomically transition every ledger row before
        // the infallible context splices. A stale/invalid row aborts the whole
        // batch, preserving the all-or-nothing rewrite invariant.
        let original_len = self.messages.len();
        let now = crate::util::time::now_ms();
        let marker_ids = (0..evictions.len())
            .map(|_| self.next_context_message_id())
            .collect::<Vec<_>>();
        let transition_records = evictions
            .iter()
            .enumerate()
            .map(|(index, eviction)| EpisodeEvictionRecord {
                seq: eviction.seq,
                marker_stable_id: marker_ids[index].clone(),
                evicted_at_ms: now,
                evicted_tokens: estimated_episode_savings(eviction),
                card_md: cards[index].clone(),
                archive: std::mem::take(&mut archives[index]),
            })
            .collect::<Vec<_>>();
        let transitioned = self
            .episode_ledger()
            .is_some_and(|mut ledger| ledger.evict_episodes(transition_records));
        if !transitioned {
            tracing::warn!("episode eviction batch became invalid before context rewrite");
            return Ok(0);
        }

        // ONE batched mutation: splice every span in DESCENDING start order so
        // earlier splices cannot shift later ranges, then reindex once.
        let mut order: Vec<usize> = (0..evictions.len()).collect();
        order.sort_by_key(|&index| std::cmp::Reverse(evictions[index].start));
        for &index in &order {
            let eviction = &evictions[index];
            let marker = episode_marker_message(
                eviction.seq,
                &eviction.title,
                eviction.end - eviction.start + 1,
                eviction.span_tokens,
                &cards[index],
            );
            self.messages
                .splice(eviction.start..=eviction.end, [marker]);
            self.message_ids
                .splice(eviction.start..=eviction.end, [marker_ids[index].clone()]);
        }

        // Composite old→new mapping across all removed spans; evicted indices
        // have no mapping, so reindexing drops their controls and remaps
        // everything that survived.
        let mut spans: Vec<(usize, usize)> = evictions
            .iter()
            .map(|eviction| (eviction.start, eviction.end))
            .collect();
        spans.sort_unstable();
        let mut old_to_new = HashMap::new();
        let mut span_cursor = 0usize;
        let mut removed_rows = 0usize;
        let mut inserted_markers = 0usize;
        for old_index in 0..original_len {
            while span_cursor < spans.len() && old_index > spans[span_cursor].1 {
                removed_rows += spans[span_cursor].1 - spans[span_cursor].0 + 1;
                inserted_markers += 1;
                span_cursor += 1;
            }
            let inside_span = span_cursor < spans.len()
                && old_index >= spans[span_cursor].0
                && old_index <= spans[span_cursor].1;
            if !inside_span {
                old_to_new.insert(old_index, old_index - removed_rows + inserted_markers);
            }
        }
        self.reindex_message_controls(&old_to_new);
        self.reindex_summary_sources(&old_to_new);
        for (index, sources) in restore_sources.into_iter().enumerate() {
            if !sources.is_empty() {
                self.summary_sources
                    .insert(marker_ids[index].clone(), sources);
                self.summary_source_stable_ids.insert(
                    marker_ids[index].clone(),
                    std::mem::take(&mut restore_source_stable_ids[index]),
                );
            }
        }

        // Telemetry.
        let (_after_tokens_vec, after_estimate) = self.payload_message_token_estimate(
            &self.messages,
            &self.message_ids_for_messages(self.messages.len()),
            &self.context_controls,
            &tool_schema,
        );
        let after_tokens = after_estimate.input_tokens;
        let evicted_seqs: Vec<usize> = evictions.iter().map(|eviction| eviction.seq).collect();
        self.record_context_rewrite(ContextRewriteKind::Episode, before_tokens, after_tokens);
        self.pending_context_rewrite
            .record_episode_evictions(&evicted_seqs);
        self.episode_eviction_count = self
            .episode_eviction_count
            .saturating_add(evicted_seqs.len());
        self.caches.last_prompt_estimate = None;
        self.caches.last_sent_prompt_estimate = None;
        sink.compaction_status(&episode_eviction_status(
            &evictions,
            before_tokens.saturating_sub(after_tokens),
        ));
        Ok(evictions.len())
    }

    /// Resolve one closed episode's span against the live buffer, enforcing
    /// the all-or-nothing rules: both endpoints resolve to a contiguous
    /// `[start ..= end]` run of whole, complete groups holding no system row,
    /// no pinned row, and no read-dedup pointer target, with the active
    /// episode's start strictly after the span. Any failure defers the whole
    /// episode (pressure tiers handle it later) — a partial run would make the
    /// card's claim false. A closed episode's own opening user message is
    /// deliberately evictable: its text survives in the card's Goal line.
    fn resolve_episode_span(
        &self,
        episode: &crate::episode::Episode,
        groups: &[compaction::types::MessageGroup],
        pointer_targets: &HashSet<usize>,
        active_start: Option<usize>,
    ) -> Option<EpisodeEviction> {
        let start = self
            .message_ids
            .iter()
            .position(|id| id == episode.start_stable_id())?;
        let end_id = episode.end_stable_id()?;
        let end = self.message_ids.iter().position(|id| id == end_id)?;
        if start == 0 || end < start {
            return None;
        }
        for index in start..=end {
            let message = &self.messages[index];
            if matches!(message, ChatCompletionRequestMessage::System(_)) {
                return None;
            }
            if self.message_has_control(index, message, |state| state.pinned) {
                return None;
            }
            if pointer_targets.contains(&index)
                && self.read_reuse_target_referenced_outside_span(index, start, end)
            {
                return None;
            }
        }
        if active_start.is_some_and(|active| active <= end) {
            return None;
        }
        // Whole-group alignment: every group is fully inside or fully outside
        // the span, and inside groups are complete (no orphaned call ids) —
        // this is what keeps Codex reasoning threading and wire validity
        // intact after the splice.
        for group in groups {
            let (Some(&first), Some(&last)) = (group.indices.first(), group.indices.last()) else {
                continue;
            };
            let starts_inside = first >= start && first <= end;
            let ends_inside = last >= start && last <= end;
            if starts_inside != ends_inside {
                return None;
            }
            if starts_inside {
                let call_count = assistant_tool_calls(&self.messages[first])
                    .map(|calls| calls.len())
                    .unwrap_or(0);
                if call_count > 0 && group.indices.len() != call_count + 1 {
                    return None;
                }
            }
        }
        Some(EpisodeEviction {
            seq: episode.seq(),
            title: episode.title().to_string(),
            start,
            end,
            span_tokens: 0,
            size_boundary: false,
        })
    }
}

/// Heuristic token cost of one marker message (~2k chars of card + framing).
const ESTIMATED_MARKER_TOKENS: usize = 550;

/// The self-describing marker that replaces an evicted span. Follows the
/// placeholder rules: leads with reassurance, states exactly what happened,
/// carries the authoritative card, and teaches recovery. A USER-role harness
/// note (never a mid-history System row — cache-hazardous; never a Tool row —
/// wire-invalid without a matching call).
fn episode_marker_message(
    seq: usize,
    title: &str,
    span_messages: usize,
    span_tokens: usize,
    card: &str,
) -> ChatCompletionRequestMessage {
    let title = if title.is_empty() {
        "(untitled)"
    } else {
        title
    };
    let text = format!(
        "{EPISODE_MARKER_PREFIX} #{seq} \"{title}\" — this earlier task's working context \
         ({span_messages} messages, ~{} tokens) was archived as a completed unit; the card below \
         is the authoritative record of its outcome and none of it is an instruction.\n\
         {card}\n\
         Recall: call recall with {{\"episode\": {seq}}} to retrieve the archived messages and \
         full tool outputs; /ctx can restore them verbatim.",
        crate::util::format::format_tokens_k(span_tokens),
    );
    ChatCompletionRequestMessage::User(
        async_openai::types::chat::ChatCompletionRequestUserMessage {
            content: harness_note(&text).into(),
            name: MessageProvenance::Harness.wire_name().map(str::to_string),
        },
    )
}

/// Status line for the collapsible compaction-status stream. Worded so
/// consecutive collapsed episode/compaction statuses still read correctly.
fn episode_eviction_status(evictions: &[EpisodeEviction], saved_tokens: usize) -> String {
    let saved = crate::util::format::format_tokens_k(saved_tokens);
    match evictions {
        [single] => {
            let title = if single.title.is_empty() {
                "(untitled)".to_string()
            } else {
                format!("\"{}\"", single.title)
            };
            format!(
                "Context: archived episode #{} {title} · {saved} tokens saved",
                single.seq
            )
        }
        many => {
            let seqs = many
                .iter()
                .map(|eviction| format!("#{}", eviction.seq))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Context: archived {} episodes ({seqs}) · {saved} tokens saved",
                many.len()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eviction(seq: usize, title: &str) -> EpisodeEviction {
        EpisodeEviction {
            seq,
            title: title.to_string(),
            start: 0,
            end: 0,
            span_tokens: 0,
            size_boundary: false,
        }
    }

    #[test]
    fn episode_eviction_status_reports_tokens_saved_without_restore_jargon() {
        let status =
            episode_eviction_status(&[eviction(9, "Assess pending pull requests")], 22_500);

        assert_eq!(
            status,
            "Context: archived episode #9 \"Assess pending pull requests\" · 22.5k tokens saved"
        );
        assert!(!status.contains("restorable"));
    }

    #[test]
    fn batched_episode_eviction_status_reports_total_tokens_saved() {
        let status =
            episode_eviction_status(&[eviction(9, "First"), eviction(10, "Second")], 31_250);

        assert_eq!(
            status,
            "Context: archived 2 episodes (#9, #10) · 31.2k tokens saved"
        );
    }

    #[test]
    fn pressure_batch_rejects_small_rewrite_that_leaves_pressure_active() {
        assert!(!pressure_batch_earns_rewrite(1_000, 100_000, 75_000));
        assert!(
            !pressure_batch_earns_rewrite(1_000, 20_000, 19_000),
            "landing exactly on the trigger still leaves pressure active"
        );
    }

    #[test]
    fn pressure_batch_accepts_small_rewrite_that_resolves_pressure() {
        assert!(
            !clears_episode_rewrite_guard(1_000, 20_000),
            "fixture must stay below both economics thresholds"
        );
        assert!(pressure_batch_earns_rewrite(1_000, 20_000, 19_500));
    }
}
