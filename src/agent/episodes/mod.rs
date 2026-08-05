//! Episode tracking hooks on [`Agent`]: opening spans on user turns, hard
//! boundaries on context resets, snapshot/restore for persistence, and the
//! `/episodes` + `/ctx` reporting surface. Boundary-signal observation and
//! close resolution live in [`boundary`]. P1 is tracking only — nothing in
//! this module mutates a single wire byte.
//!
//! Every hook is inert when `episode_store` is `None` (feature disabled,
//! subagents, tests without explicit wiring).

use super::*;
use crate::context_view::EpisodeReport;
#[cfg(test)]
use crate::episode::SharedEpisodeStore;
use crate::episode::{Episode, EpisodeCloseReason, EpisodeLedger, EpisodeStatus};

mod boundary;
mod card;
mod evict;

/// Cap for the one-line `goal` captured from an episode's opening user message.
const EPISODE_GOAL_MAX_CHARS: usize = 280;

/// Collapse `text` to one whitespace-normalized line capped at
/// [`EPISODE_GOAL_MAX_CHARS`] characters, for `Episode::goal`.
pub(super) fn episode_goal_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= EPISODE_GOAL_MAX_CHARS {
        return collapsed;
    }
    collapsed.chars().take(EPISODE_GOAL_MAX_CHARS).collect()
}

/// Given `(start, end, seq)` spans, return the seqs of every span that overlaps
/// an adjacent one. Spans are sorted by `(start, end)` and swept with
/// `windows(2)`: two spans overlap when the later start falls on or before the
/// earlier end. Callers use this to make overlapping episode rows non-evictable
/// / repaired rather than splicing overlapping descending ranges.
fn overlapping_span_seqs(mut spans: Vec<(usize, usize, usize)>) -> HashSet<usize> {
    spans.sort_unstable_by_key(|(start, end, _seq)| (*start, *end));
    let mut overlapping = HashSet::new();
    for pair in spans.windows(2) {
        if pair[1].0 <= pair[0].1 {
            overlapping.insert(pair[0].2);
            overlapping.insert(pair[1].2);
        }
    }
    overlapping
}

impl Agent {
    /// Wire the episode ledger after construction (builder mirror for tests).
    #[cfg(test)]
    pub(crate) fn set_episode_store(&mut self, store: SharedEpisodeStore) {
        self.episode_store = Some(store);
    }

    /// Lock the ledger, recovering from poisoning like the schema cache does.
    /// `None` when the feature is unwired. The guard must never be held across
    /// an `.await` — the same store is read by the sync `context_report` path.
    pub(super) fn episode_ledger(&self) -> Option<StdMutexGuard<'_, EpisodeLedger>> {
        let store = self.episode_store.as_ref()?;
        Some(match store.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        })
    }

    /// Record a freshly pushed human user turn and open a new episode when
    /// none is active. Called from `begin_run` and `drain_queued_messages`
    /// right after the push; boundary resolution scans the stable live ids.
    pub(super) fn observe_user_turn_for_episodes(&mut self, text: &str) {
        if self.episode_store.is_none() {
            return;
        }
        let Some(last_id) = self.message_ids.last().cloned() else {
            return;
        };
        let now = crate::util::time::now_ms();
        let goal = episode_goal_line(text);
        let Some(mut ledger) = self.episode_ledger() else {
            return;
        };
        if ledger.active().is_none() {
            ledger.open_episode(last_id, String::new(), goal, now);
        }
    }

    /// Open the episode for a caller-seeded user message (plan handoff). Runs
    /// after the caller pushed its seeded message, so the span starts on it.
    pub(super) fn open_episode_for_seeded_message(&mut self, goal_text: &str) {
        self.observe_user_turn_for_episodes(goal_text);
    }

    /// Close the active episode before a hard context reset (`/clear`, plan
    /// handoff, `/start`, `/review`). Must run BEFORE the caller replaces
    /// `messages` — the resets install their new context first, so hooking the
    /// shared transient reset would be too late to resolve the span end.
    ///
    /// Bookkeeping only: no evict, no card, no rewrite telemetry. The
    /// unarchived active span is discarded with the reset and is not
    /// recallable; already-evicted historical episodes stay in the ledger so a
    /// later snapshot-replace cannot delete their persisted archives.
    pub(super) fn close_active_episode_for_hard_boundary(&mut self) {
        if self.episode_store.is_none() {
            return;
        }
        let end_id = self
            .message_ids
            .last()
            .cloned()
            .unwrap_or_else(|| format_context_message_id(0));
        let now = crate::util::time::now_ms();
        let Some(mut ledger) = self.episode_ledger() else {
            return;
        };
        ledger.clear_pending_title_signal();
        ledger.close_active(EpisodeCloseReason::HardBoundary, end_id, now);
    }

    /// Persistable copy of the ledger. `None` when the feature is unwired, so
    /// persistence writes nothing (not even a DELETE) while disabled.
    pub(crate) fn episodes_snapshot(&self) -> Option<Vec<Episode>> {
        self.episode_ledger().map(|ledger| ledger.snapshot())
    }

    /// Sequence of the episode currently owning new conversation work.
    pub(crate) fn active_episode_seq(&self) -> Option<usize> {
        self.episode_ledger()?.active().map(Episode::seq)
    }

    /// Restore the ledger from a persisted snapshot. Runs after context
    /// messages/ids are restored so the active span can be validated against
    /// live stable ids: on skew (the persisted start no longer resolves), the
    /// active row is demoted to `Closed(Manual)` and a fresh episode opens at
    /// the last human user message. Historical rows are always retained;
    /// inconsistent links transition to repaired, non-evictable rows so the
    /// next snapshot cannot silently delete their persisted archives.
    pub(crate) fn restore_episodes(&mut self, episodes: Vec<Episode>) {
        if self.episode_store.is_none() {
            return;
        }
        let now = crate::util::time::now_ms();
        let live_positions = self
            .message_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut restored = EpisodeLedger::default();
        restored.restore(episodes);
        let mut reopen_at: Option<(String, String)> = None;
        let newest_active_seq = restored
            .episodes()
            .iter()
            .filter(|episode| episode.status() == EpisodeStatus::Active)
            .map(Episode::seq)
            .max();
        let mut repaired_active = HashSet::new();
        let active_rows = restored
            .episodes()
            .iter()
            .filter(|episode| episode.status() == EpisodeStatus::Active)
            .map(|episode| (episode.seq(), episode.start_stable_id().to_string()))
            .collect::<Vec<_>>();
        for (seq, start_stable_id) in active_rows {
            let start_is_live = live_positions.contains_key(start_stable_id.as_str());
            let is_newest = newest_active_seq == Some(seq);
            if start_is_live && is_newest {
                continue;
            }
            let end_stable_id = self
                .message_ids
                .last()
                .cloned()
                .unwrap_or_else(|| format_context_message_id(0));
            restored.repair_active(seq, end_stable_id, now);
            repaired_active.insert(seq);
            if is_newest {
                reopen_at = self.last_human_user_message_index().map(|index| {
                    let (_role, text) = describe_message_full(&self.messages[index]);
                    (self.message_ids[index].clone(), episode_goal_line(&text))
                });
            }
        }

        // Closed title/manual spans must still resolve contiguously before they
        // can ever become eviction candidates. Evicted rows must retain both
        // their marker and its /ctx restore source. Any other crash skew is
        // preserved as historical, recallable data but made non-evictable.
        let mut closed_spans = Vec::new();
        let invalid_closed = restored
            .episodes()
            .iter()
            .filter_map(|episode| {
                if episode.status() == EpisodeStatus::Closed
                    && matches!(
                        episode.close_reason(),
                        Some(
                            EpisodeCloseReason::TitleChange
                                | EpisodeCloseReason::TodoComplete
                                | EpisodeCloseReason::SizePressure
                                | EpisodeCloseReason::Manual
                        )
                    )
                    && !repaired_active.contains(&episode.seq())
                {
                    let live_span = live_positions
                        .get(episode.start_stable_id())
                        .copied()
                        .zip(
                            episode
                                .end_stable_id()
                                .and_then(|id| live_positions.get(id).copied()),
                        )
                        .filter(|(start, end)| start <= end);
                    match live_span {
                        Some((start, end)) => closed_spans.push((start, end, episode.seq())),
                        None => return Some(episode.seq()),
                    }
                }
                None
            })
            .collect::<Vec<_>>();
        for seq in invalid_closed {
            restored.mark_episode_repaired(seq, now);
        }
        for seq in overlapping_span_seqs(closed_spans) {
            restored.mark_episode_repaired(seq, now);
        }
        let evicted = restored
            .episodes()
            .iter()
            .filter(|episode| episode.status() == EpisodeStatus::Evicted)
            .collect::<Vec<_>>();
        let mut valid_evicted = HashSet::new();
        let live_sources = self
            .summary_sources
            .iter()
            .filter(|(source_id, _source)| live_positions.contains_key(source_id.as_str()))
            .map(|(source_id, source)| (source_id.as_str(), source))
            .collect::<HashMap<_, _>>();
        let mut claimed_by_source = HashMap::<&str, HashSet<usize>>::new();

        // Direct markers own the entire source row. Accept only byte-exact
        // archive payloads; occurrence matching would restore an oversized span.
        for episode in &evicted {
            let Some(marker_id) = episode.marker_stable_id() else {
                continue;
            };
            let Some(source) = live_sources.get(&marker_id) else {
                continue;
            };
            let claimed = claimed_by_source.entry(marker_id).or_default();
            if claimed.is_empty() && episode_archive_equals_source(source, episode) {
                claimed.extend(0..source.len());
                valid_evicted.insert(episode.seq());
            }
        }

        // Nested markers require one metadata owner. A live direct source is
        // also an owner, even if malformed, so it cannot be masked by another
        // source claiming the same marker through sidecar metadata.
        for episode in &evicted {
            if valid_evicted.contains(&episode.seq()) {
                continue;
            }
            let Some(marker_id) = episode.marker_stable_id() else {
                continue;
            };
            let direct_owner_is_live = live_sources.contains_key(marker_id);
            let metadata_owners = live_sources
                .keys()
                .filter(|source_id| {
                    self.summary_source_stable_ids
                        .get(**source_id)
                        .is_some_and(|stable_ids| {
                            stable_ids.iter().any(|stable_id| stable_id == marker_id)
                        })
                })
                .copied()
                .collect::<Vec<_>>();
            if direct_owner_is_live || metadata_owners.len() != 1 {
                continue;
            }
            let source_id = metadata_owners[0];
            let source = live_sources[source_id];
            if claim_episode_archive_occurrence(
                source,
                episode,
                claimed_by_source.entry(source_id).or_default(),
            ) {
                valid_evicted.insert(episode.seq());
            }
        }

        // Older rows have no sidecar metadata. Preserve them only when exactly
        // one live metadata-free source contains an unclaimed archive occurrence.
        for episode in &evicted {
            if valid_evicted.contains(&episode.seq()) {
                continue;
            }
            let Some(marker_id) = episode.marker_stable_id() else {
                continue;
            };
            if live_sources.contains_key(marker_id) {
                continue;
            }
            let candidates = live_sources
                .iter()
                .filter(|(source_id, source)| {
                    self.summary_source_stable_ids
                        .get(**source_id)
                        .is_none_or(Vec::is_empty)
                        && episode_archive_occurrence_start(
                            source,
                            episode,
                            claimed_by_source.get(*source_id).unwrap_or(&HashSet::new()),
                        )
                        .is_some()
                })
                .map(|(source_id, _source)| *source_id)
                .collect::<Vec<_>>();
            if let [source_id] = candidates.as_slice()
                && claim_episode_archive_occurrence(
                    live_sources[*source_id],
                    episode,
                    claimed_by_source.entry(source_id).or_default(),
                )
            {
                valid_evicted.insert(episode.seq());
            }
        }
        let invalid_evicted = evicted
            .into_iter()
            .filter(|episode| !valid_evicted.contains(&episode.seq()))
            .map(Episode::seq)
            .collect::<Vec<_>>();
        for seq in invalid_evicted {
            restored.mark_episode_repaired(seq, now);
        }
        let Some(mut ledger) = self.episode_ledger() else {
            return;
        };
        ledger.restore(restored.snapshot());
        if let Some((start_id, goal)) = reopen_at {
            ledger.open_episode(start_id, String::new(), goal, now);
        }
    }

    /// Drop all episode state after `/clear` or `/new` rotates to a fresh
    /// persisted session. The outgoing ledger is persisted before this runs;
    /// retaining it would let the new session recall another session's archive.
    pub(crate) fn clear_episodes_for_session_rotation(&mut self) {
        if let Some(mut ledger) = self.episode_ledger() {
            ledger.restore(Vec::new());
        }
    }

    /// Index of the most recent human-submitted user message (`User` role, no
    /// provenance name — harness/peer/background notes all carry one).
    pub(super) fn last_human_user_message_index(&self) -> Option<usize> {
        self.messages
            .iter()
            .rposition(boundary::is_human_user_message)
    }

    /// Repeat-recall dedup (anti-cascade): when the model re-requests a recall
    /// page whose identical prior result is still LIVE and un-stubbed in
    /// context, the fresh bytes are replaced with a compact pointer. The check
    /// lives agent-side because the tool is context-blind; it mirrors the
    /// read-dedup liveness rule, and a first recall after a boundary is never
    /// blocked (the pointer only fires on a live identical prior result).
    pub(in crate::agent) fn recall_reuse_pointer(
        &self,
        tool_call: &ToolCall,
        content: &str,
    ) -> Option<String> {
        if self.episode_store.is_none() || tool_call.name != "recall" {
            return None;
        }
        let new_arguments = serde_json::from_str::<serde_json::Value>(&tool_call.arguments).ok()?;
        for message in &self.messages {
            let Some(call_id) = tool_message_call_id(message) else {
                continue;
            };
            if call_id == tool_call.id {
                continue;
            }
            let Some(detail) = self.tool_context_details.get(&call_id) else {
                continue;
            };
            if detail.name != "recall" {
                continue;
            }
            let prior_arguments = serde_json::from_str::<serde_json::Value>(&detail.arguments).ok();
            if prior_arguments.as_ref() != Some(&new_arguments) {
                continue;
            }
            let (_role, text) = describe_message_full(message);
            let trimmed = text.trim_start();
            if trimmed.starts_with("[Compacted tool output]")
                || trimmed.starts_with("[reused previous recall]")
            {
                continue;
            }
            if text == content {
                return Some(format!(
                    "[reused previous recall] The identical recall page (call {call_id}) is \
                     still live earlier in this conversation; its bytes were not re-emitted. \
                     Consult that earlier result — it is complete and current."
                ));
            }
        }
        None
    }

    /// A `/ctx` restore spliced `restored_id`'s summary source back into live
    /// context. If that row was an episode's eviction marker, transition the
    /// episode Evicted -> Restored and rewrite its span onto the freshly
    /// allocated live ids (restoration always allocates new ids; archive
    /// stable ids stay archive/pagination identities and are never reused).
    /// Restored episodes are not eviction candidates again — that would churn
    /// the prefix; ordinary GC/compaction manage those bytes from here on.
    pub(in crate::agent) fn note_summary_source_restored(
        &mut self,
        restored_id: &str,
        source: &[ChatCompletionRequestMessage],
        source_stable_ids: &[String],
        inserted_ids: &[String],
    ) {
        if self.episode_store.is_none() {
            return;
        }
        let (Some(first), Some(last)) = (inserted_ids.first(), inserted_ids.last()) else {
            return;
        };
        let Some(ledger) = self.episode_ledger() else {
            return;
        };
        let episodes = ledger.episodes().to_vec();
        drop(ledger);
        let mut sources = self
            .summary_sources
            .iter()
            .filter(|(source_id, _source)| self.message_ids.iter().any(|id| id == *source_id))
            .map(|(source_id, source)| (source_id.as_str(), source.as_slice()))
            .collect::<HashMap<_, _>>();
        sources.insert(restored_id, source);
        let mut source_stable_ids_by_id = self
            .summary_source_stable_ids
            .iter()
            .filter(|(source_id, _stable_ids)| sources.contains_key(source_id.as_str()))
            .map(|(source_id, stable_ids)| (source_id.as_str(), stable_ids.as_slice()))
            .collect::<HashMap<_, _>>();
        source_stable_ids_by_id.insert(restored_id, source_stable_ids);

        let direct_seq = episodes
            .iter()
            .find(|episode| {
                episode.status() == EpisodeStatus::Evicted
                    && episode.marker_stable_id() == Some(restored_id)
                    && episode_archive_equals_source(source, episode)
            })
            .map(Episode::seq);
        let Some(mut ledger) = self.episode_ledger() else {
            return;
        };
        let mut claimed = HashSet::new();
        if let Some(seq) = direct_seq {
            ledger.restore_episode_span(seq, first, last);
            claimed.extend(0..source.len());
        }

        // A later rolling compaction may fold an episode marker into its own
        // summary source. That rekeys the restore source onto the summary row,
        // so match each archived episode's exact message sequence inside the
        // restored originals and rewrite its span to the corresponding fresh ids.
        let evicted = episodes
            .iter()
            .filter(|episode| {
                episode.status() == EpisodeStatus::Evicted
                    && Some(episode.seq()) != direct_seq
                    && episode.marker_stable_id().is_some_and(|marker_id| {
                        if sources.contains_key(marker_id) {
                            return false;
                        }
                        let metadata_owners = source_stable_ids_by_id
                            .iter()
                            .filter(|(_source_id, stable_ids)| {
                                stable_ids.iter().any(|stable_id| stable_id == marker_id)
                            })
                            .map(|(source_id, _stable_ids)| *source_id)
                            .collect::<Vec<_>>();
                        if source_stable_ids.is_empty() {
                            metadata_owners.is_empty()
                                && sources
                                    .iter()
                                    .filter(|(source_id, candidate)| {
                                        source_stable_ids_by_id
                                            .get(*source_id)
                                            .is_none_or(|stable_ids| stable_ids.is_empty())
                                            && episode_archive_occurrence_start(
                                                candidate,
                                                episode,
                                                &HashSet::new(),
                                            )
                                            .is_some()
                                    })
                                    .count()
                                    == 1
                        } else {
                            metadata_owners.as_slice() == [restored_id]
                        }
                    })
            })
            .map(|episode| {
                (
                    episode.seq(),
                    episode
                        .archive()
                        .iter()
                        .map(|item| item.message.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        for (seq, archive) in evicted {
            let Some(start) =
                source
                    .windows(archive.len())
                    .enumerate()
                    .find_map(|(start, window)| {
                        let end = start + archive.len();
                        (window == archive.as_slice()
                            && (start..end).all(|index| !claimed.contains(&index)))
                        .then_some(start)
                    })
            else {
                continue;
            };
            let end = start + archive.len() - 1;
            claimed.extend(start..=end);
            if let (Some(first), Some(last)) = (inserted_ids.get(start), inserted_ids.get(end)) {
                ledger.restore_episode_span(seq, first, last);
            }
        }
    }

    /// Lightweight episode rows for `ContextReport` (the `/ctx` modal) with
    /// live spans resolved against the current stable ids.
    pub(crate) fn episode_reports(&self) -> Vec<EpisodeReport> {
        let Some(ledger) = self.episode_ledger() else {
            return Vec::new();
        };
        ledger
            .episodes()
            .iter()
            .map(|episode| {
                let start = self
                    .message_ids
                    .iter()
                    .position(|id| id == episode.start_stable_id());
                let end = episode
                    .end_stable_id()
                    .and_then(|end_id| self.message_ids.iter().position(|id| id == end_id));
                let live_span_messages = match episode.status() {
                    EpisodeStatus::Active => start.map(|start| self.messages.len() - start),
                    EpisodeStatus::Closed | EpisodeStatus::Restored => start
                        .zip(end)
                        .filter(|(start, end)| end >= start)
                        .map(|(start, end)| end - start + 1),
                    EpisodeStatus::Evicted => None,
                };
                EpisodeReport {
                    seq: episode.seq(),
                    title: episode.title().to_string(),
                    status_label: episode.status().as_db_str().to_string(),
                    close_reason_label: episode
                        .close_reason()
                        .map(|reason| reason.as_db_str().to_string()),
                    goal: episode.goal().to_string(),
                    live_span_messages,
                    evicted_tokens: episode.evicted_tokens(),
                    recall_count: episode.recall_count(),
                    archived_messages: episode.archive().len(),
                    completable: episode.is_completable(),
                    repaired: episode
                        .card_md()
                        .contains(crate::episode::EPISODE_REPAIR_NOTE),
                }
            })
            .collect()
    }

    /// Text report for the `/episodes` command.
    pub(crate) fn episodes_command_report(&self) -> String {
        if self.episode_store.is_none() {
            return "Episodes are disabled. Unset BONSAI_EPISODES or set it to 1 to enable them."
                .to_string();
        }
        let reports = self.episode_reports();
        if reports.is_empty() {
            return "No episodes tracked yet — one opens on the next user turn.".to_string();
        }
        use std::fmt::Write as _;
        let mut out = String::from("Episodes:");
        for report in &reports {
            let title = if report.title.is_empty() {
                "(untitled)".to_string()
            } else {
                format!("\"{}\"", report.title)
            };
            let _ = write!(out, "\n #{} {:<8} {title}", report.seq, report.status_label);
            if let Some(span) = report.live_span_messages {
                let _ = write!(out, " · span {span} msgs");
            }
            if let Some(reason) = &report.close_reason_label {
                let _ = write!(out, " · {reason}");
            }
            if let Some(tokens) = report.evicted_tokens {
                let _ = write!(out, " · {tokens} tokens evicted");
            }
            if report.archived_messages > 0 {
                let _ = write!(
                    out,
                    " · {} archived · recall(episode={})",
                    report.archived_messages, report.seq
                );
            }
            if report.completable {
                let _ = write!(out, " · todos complete");
            }
            if report.repaired {
                let _ = write!(out, " · repaired after resume skew");
            }
        }
        out
    }
}

fn claim_episode_archive_occurrence(
    source: &[ChatCompletionRequestMessage],
    episode: &Episode,
    claimed: &mut HashSet<usize>,
) -> bool {
    let Some(start) = episode_archive_occurrence_start(source, episode, claimed) else {
        return false;
    };
    claimed.extend(start..start + episode.archive().len());
    true
}

fn episode_archive_equals_source(
    source: &[ChatCompletionRequestMessage],
    episode: &Episode,
) -> bool {
    source.len() == episode.archive().len()
        && source
            .iter()
            .zip(episode.archive())
            .all(|(message, archived)| message == &archived.message)
}

fn episode_archive_occurrence_start(
    source: &[ChatCompletionRequestMessage],
    episode: &Episode,
    claimed: &HashSet<usize>,
) -> Option<usize> {
    let archive = episode
        .archive()
        .iter()
        .map(|item| &item.message)
        .collect::<Vec<_>>();
    if archive.is_empty() {
        return None;
    }
    source
        .windows(archive.len())
        .enumerate()
        .find_map(|(start, window)| {
            let end = start + archive.len();
            (window
                .iter()
                .zip(&archive)
                .all(|(message, archived)| message == *archived)
                && (start..end).all(|index| !claimed.contains(&index)))
            .then_some(start)
        })
}
