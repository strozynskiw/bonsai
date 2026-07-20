//! Episodes: default-on task-scoped context lifecycle. `BONSAI_EPISODES=0`
//! is the emergency opt-out. An [`Episode`] is one contiguous topic span of the
//! parent lane's timeline, delimited by stable context-message ids. The
//! [`EpisodeLedger`] tracks the active span, applies deterministic boundary
//! signals (title changes, hard resets), and — from P2 on — records eviction
//! and archived bytes for recall.
//!
//! Mirrors `src/todo.rs` in shape, but uses `std::sync::Mutex` (never held
//! across `.await`, precedent: `MemoryService.recalled`) so the synchronous
//! `context_report()` path can read it.

use std::collections::HashSet;
use std::sync::Arc;

use async_openai::types::chat::ChatCompletionRequestMessage;
use serde::{Deserialize, Serialize};

pub(crate) const EPISODE_REPAIR_NOTE: &str = "[Episode repair]";

/// Default-on gate for the whole episodes feature. `BONSAI_EPISODES=0`,
/// `false`, `off`, `no`, or `disabled` is an explicit kill switch. Disabled
/// means no boundary tracking, no wire mutation, no recall registration, and
/// no episode persistence — the only remaining trace is the schema migration.
pub fn episodes_enabled() -> bool {
    std::env::var("BONSAI_EPISODES").map_or(true, |value| episodes_enabled_from(Some(&value)))
}

fn episodes_enabled_from(value: Option<&str>) -> bool {
    !value.is_some_and(|value| {
        let value = value.trim();
        value == "0"
            || value.eq_ignore_ascii_case("false")
            || value.eq_ignore_ascii_case("off")
            || value.eq_ignore_ascii_case("no")
            || value.eq_ignore_ascii_case("disabled")
    })
}

/// Model-declared relationship between a title update and the active episode.
/// Missing values default conservatively to `SameTopic` for compatibility with
/// older recorded tool calls and callers that predate the explicit field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EpisodeAction {
    NewTopic,
    #[default]
    SameTopic,
}

impl EpisodeAction {
    pub(crate) const WIRE_VALUES: &'static [&'static str] = &["new_topic", "same_topic"];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EpisodeEvictionPass {
    CloseTime,
    Pressure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeStatus {
    Active,
    Closed,
    Evicted,
    Restored,
}

crate::impl_db_enum!(EpisodeStatus {
    Active => "active",
    Closed => "closed",
    Evicted => "evicted",
    Restored => "restored",
} else Active);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeCloseReason {
    TitleChange,
    HardBoundary,
    Manual,
}

crate::impl_db_enum!(EpisodeCloseReason {
    TitleChange => "title_change",
    HardBoundary => "hard_boundary",
    Manual => "manual",
} else Manual);

/// One archived model-facing message, keyed by the stable context id it had
/// when it was evicted. The stable id is the archive/pagination identity only;
/// a later `/ctx` restore allocates fresh live ids and never reuses these.
#[derive(Debug, Clone, PartialEq)]
pub struct ArchivedEpisodeItem {
    pub stable_id: String,
    pub message: ChatCompletionRequestMessage,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Episode {
    /// 1-based per session.
    seq: usize,
    title: String,
    status: EpisodeStatus,
    /// Opening user message, one line, capped.
    goal: String,
    /// Rendered episode card (also the marker body once evicted). Empty until P2.
    card_md: String,
    close_reason: Option<EpisodeCloseReason>,
    start_stable_id: String,
    end_stable_id: Option<String>,
    /// Stable id of the spliced card marker while `status == Evicted`.
    marker_stable_id: Option<String>,
    files_touched: Vec<String>,
    opened_at_ms: i64,
    closed_at_ms: Option<i64>,
    evicted_at_ms: Option<i64>,
    /// Estimator delta of the evict rewrite.
    evicted_tokens: Option<usize>,
    recall_count: usize,
    /// Armed by a todowrite where every item is completed/cancelled. Card
    /// annotation + future close heuristic; never closes by itself in v1.
    completable: bool,
    /// Populated at evict; the recall tool's source. Empty while live.
    archive: Vec<ArchivedEpisodeItem>,
}

/// Storage-boundary representation. Runtime code receives an [`Episode`]
/// with private fields and can only mutate it through [`EpisodeLedger`]
/// transitions; persistence may hydrate skewed historical rows for the resume
/// repair pass to quarantine.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PersistedEpisode {
    pub seq: usize,
    pub title: String,
    pub status: EpisodeStatus,
    pub goal: String,
    pub card_md: String,
    pub close_reason: Option<EpisodeCloseReason>,
    pub start_stable_id: String,
    pub end_stable_id: Option<String>,
    pub marker_stable_id: Option<String>,
    pub files_touched: Vec<String>,
    pub opened_at_ms: i64,
    pub closed_at_ms: Option<i64>,
    pub evicted_at_ms: Option<i64>,
    pub evicted_tokens: Option<usize>,
    pub recall_count: usize,
    pub completable: bool,
    pub archive: Vec<ArchivedEpisodeItem>,
}

/// Complete payload for the atomic `Closed -> Evicted` transition.
#[derive(Debug)]
pub(crate) struct EpisodeEvictionRecord {
    pub seq: usize,
    pub marker_stable_id: String,
    pub evicted_at_ms: i64,
    pub evicted_tokens: usize,
    pub card_md: String,
    pub archive: Vec<ArchivedEpisodeItem>,
}

impl Episode {
    fn open(seq: usize, start_stable_id: String, title: String, goal: String, now_ms: i64) -> Self {
        Self {
            seq,
            title,
            status: EpisodeStatus::Active,
            goal,
            card_md: String::new(),
            close_reason: None,
            start_stable_id,
            end_stable_id: None,
            marker_stable_id: None,
            files_touched: Vec::new(),
            opened_at_ms: now_ms,
            closed_at_ms: None,
            evicted_at_ms: None,
            evicted_tokens: None,
            recall_count: 0,
            completable: false,
            archive: Vec::new(),
        }
    }

    /// Hydrate one persistence record. Validation happens during agent resume,
    /// where live stable ids and summary-source links are available.
    pub(crate) fn from_persisted(record: PersistedEpisode) -> Self {
        Self {
            seq: record.seq,
            title: record.title,
            status: record.status,
            goal: record.goal,
            card_md: record.card_md,
            close_reason: record.close_reason,
            start_stable_id: record.start_stable_id,
            end_stable_id: record.end_stable_id,
            marker_stable_id: record.marker_stable_id,
            files_touched: record.files_touched,
            opened_at_ms: record.opened_at_ms,
            closed_at_ms: record.closed_at_ms,
            evicted_at_ms: record.evicted_at_ms,
            evicted_tokens: record.evicted_tokens,
            recall_count: record.recall_count,
            completable: record.completable,
            archive: record.archive,
        }
    }

    pub(crate) fn seq(&self) -> usize {
        self.seq
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn status(&self) -> EpisodeStatus {
        self.status
    }

    pub(crate) fn goal(&self) -> &str {
        &self.goal
    }

    pub(crate) fn card_md(&self) -> &str {
        &self.card_md
    }

    pub(crate) fn close_reason(&self) -> Option<EpisodeCloseReason> {
        self.close_reason
    }

    pub(crate) fn start_stable_id(&self) -> &str {
        &self.start_stable_id
    }

    pub(crate) fn end_stable_id(&self) -> Option<&str> {
        self.end_stable_id.as_deref()
    }

    pub(crate) fn marker_stable_id(&self) -> Option<&str> {
        self.marker_stable_id.as_deref()
    }

    pub(crate) fn files_touched(&self) -> &[String] {
        &self.files_touched
    }

    pub(crate) fn opened_at_ms(&self) -> i64 {
        self.opened_at_ms
    }

    pub(crate) fn closed_at_ms(&self) -> Option<i64> {
        self.closed_at_ms
    }

    pub(crate) fn evicted_at_ms(&self) -> Option<i64> {
        self.evicted_at_ms
    }

    pub(crate) fn evicted_tokens(&self) -> Option<usize> {
        self.evicted_tokens
    }

    pub(crate) fn recall_count(&self) -> usize {
        self.recall_count
    }

    pub(crate) fn is_completable(&self) -> bool {
        self.completable
    }

    pub(crate) fn archive(&self) -> &[ArchivedEpisodeItem] {
        &self.archive
    }

    fn rename(&mut self, title: String) {
        self.title = title;
    }

    fn reanchor(&mut self, start_stable_id: String, title: String, goal: String) {
        self.start_stable_id = start_stable_id;
        self.title = title;
        self.goal = goal;
        self.completable = false;
    }

    fn mark_completable(&mut self) {
        self.completable = true;
    }

    fn close(&mut self, reason: EpisodeCloseReason, end_stable_id: String, now_ms: i64) -> bool {
        if self.status != EpisodeStatus::Active || end_stable_id.is_empty() {
            return false;
        }
        self.status = EpisodeStatus::Closed;
        self.close_reason = Some(reason);
        self.end_stable_id = Some(end_stable_id);
        self.closed_at_ms = Some(now_ms.max(self.opened_at_ms));
        true
    }

    fn set_closed_files_touched(&mut self, files_touched: Vec<String>) -> bool {
        if self.status != EpisodeStatus::Closed {
            return false;
        }
        self.files_touched = files_touched;
        true
    }

    fn can_evict(&self, record: &EpisodeEvictionRecord) -> bool {
        self.seq == record.seq
            && self.status == EpisodeStatus::Closed
            && self.end_stable_id.is_some()
            && self.closed_at_ms.is_some()
            && !record.marker_stable_id.is_empty()
            && !record.archive.is_empty()
    }

    fn evict(&mut self, record: EpisodeEvictionRecord) {
        debug_assert!(
            self.can_evict(&record),
            "episode eviction must be validated before transition"
        );
        if self.status != EpisodeStatus::Closed
            || self.end_stable_id.is_none()
            || self.closed_at_ms.is_none()
            || record.marker_stable_id.is_empty()
            || record.archive.is_empty()
        {
            return;
        }
        self.status = EpisodeStatus::Evicted;
        self.marker_stable_id = Some(record.marker_stable_id);
        self.evicted_at_ms = Some(record.evicted_at_ms.max(self.opened_at_ms));
        self.evicted_tokens = Some(record.evicted_tokens);
        self.card_md = record.card_md;
        self.archive = record.archive;
    }

    fn restore_span(&mut self, start_stable_id: &str, end_stable_id: &str) -> bool {
        if self.status != EpisodeStatus::Evicted
            || self.archive.is_empty()
            || start_stable_id.is_empty()
            || end_stable_id.is_empty()
        {
            return false;
        }
        self.status = EpisodeStatus::Restored;
        self.marker_stable_id = None;
        self.start_stable_id = start_stable_id.to_string();
        self.end_stable_id = Some(end_stable_id.to_string());
        true
    }

    fn record_recall(&mut self) -> bool {
        if self.archive.is_empty() {
            return false;
        }
        self.recall_count = self.recall_count.saturating_add(1);
        true
    }

    fn mark_restored_after_repair(&mut self, now_ms: i64) -> bool {
        if self.status == EpisodeStatus::Active {
            return false;
        }
        self.status = EpisodeStatus::Restored;
        self.marker_stable_id = None;
        self.evicted_at_ms = Some(
            self.evicted_at_ms
                .unwrap_or_else(|| now_ms.max(self.opened_at_ms)),
        );
        if !self.card_md.contains(EPISODE_REPAIR_NOTE) {
            if !self.card_md.is_empty() {
                self.card_md.push_str("\n\n");
            }
            self.card_md.push_str(
                "[Episode repair] Persisted lifecycle links were inconsistent on resume; the \
                 archive was preserved and automatic re-eviction was disabled.",
            );
        }
        true
    }
}

/// A recorded title-bearing tool success awaiting episode resolution. The
/// observer hook runs before the tool-result message is appended, so only the
/// signal is captured here; the rename/close decision happens at the next
/// preflight, when the complete assistant + tool-result group exists in context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingEpisodeTitleSignal {
    pub title: String,
    pub call_id: String,
    pub action: EpisodeAction,
}

#[derive(Debug, Default)]
pub struct EpisodeLedger {
    episodes: Vec<Episode>,
    /// Set by the title observer, consumed in preflight. Never persisted — it
    /// re-derives from the next signal after a resume.
    pending_title_signal: Option<PendingEpisodeTitleSignal>,
}

pub type SharedEpisodeStore = Arc<std::sync::Mutex<EpisodeLedger>>;

impl EpisodeLedger {
    pub fn episodes(&self) -> &[Episode] {
        &self.episodes
    }

    pub fn episode(&self, seq: usize) -> Option<&Episode> {
        self.episodes.iter().find(|episode| episode.seq() == seq)
    }

    pub fn active(&self) -> Option<&Episode> {
        self.episodes
            .iter()
            .find(|episode| episode.status() == EpisodeStatus::Active)
    }

    /// Open a new active episode. Callers must have closed any prior active
    /// episode first; a stray active row is closed defensively as `Manual`
    /// rather than corrupting the one-active invariant.
    pub fn open_episode(
        &mut self,
        start_stable_id: String,
        title: String,
        goal: String,
        now_ms: i64,
    ) -> usize {
        if self.active().is_some() {
            self.close_active(EpisodeCloseReason::Manual, start_stable_id.clone(), now_ms);
        }
        let seq = self.episodes.iter().map(Episode::seq).max().unwrap_or(0) + 1;
        self.episodes
            .push(Episode::open(seq, start_stable_id, title, goal, now_ms));
        seq
    }

    /// Close the active episode (bookkeeping only — zero wire-byte change).
    /// Returns the closed episode's seq, or `None` when nothing was active.
    pub fn close_active(
        &mut self,
        reason: EpisodeCloseReason,
        end_stable_id: String,
        now_ms: i64,
    ) -> Option<usize> {
        let episode = self
            .episodes
            .iter_mut()
            .find(|episode| episode.status() == EpisodeStatus::Active)?;
        episode
            .close(reason, end_stable_id, now_ms)
            .then(|| episode.seq())
    }

    pub fn rename_active(&mut self, title: String) -> bool {
        let Some(episode) = self
            .episodes
            .iter_mut()
            .find(|episode| episode.status() == EpisodeStatus::Active)
        else {
            return false;
        };
        episode.rename(title);
        true
    }

    pub fn reanchor_active(
        &mut self,
        start_stable_id: String,
        title: String,
        goal: String,
    ) -> bool {
        let Some(episode) = self
            .episodes
            .iter_mut()
            .find(|episode| episode.status() == EpisodeStatus::Active)
        else {
            return false;
        };
        episode.reanchor(start_stable_id, title, goal);
        true
    }

    pub fn mark_active_completable(&mut self) -> bool {
        let Some(episode) = self
            .episodes
            .iter_mut()
            .find(|episode| episode.status() == EpisodeStatus::Active)
        else {
            return false;
        };
        episode.mark_completable();
        true
    }

    pub fn set_closed_files_touched(&mut self, seq: usize, files_touched: Vec<String>) -> bool {
        self.episodes
            .iter_mut()
            .find(|episode| episode.seq() == seq)
            .is_some_and(|episode| episode.set_closed_files_touched(files_touched))
    }

    pub fn repair_active(&mut self, seq: usize, end_stable_id: String, now_ms: i64) -> bool {
        self.episodes
            .iter_mut()
            .find(|episode| episode.seq() == seq)
            .is_some_and(|episode| episode.close(EpisodeCloseReason::Manual, end_stable_id, now_ms))
    }

    /// Atomically validate and apply a batch of `Closed -> Evicted`
    /// transitions. No episode changes when any record is invalid.
    pub fn evict_episodes(&mut self, records: Vec<EpisodeEvictionRecord>) -> bool {
        let mut seen = HashSet::new();
        let all_valid = records.iter().all(|record| {
            seen.insert(record.seq)
                && self
                    .episodes
                    .iter()
                    .find(|episode| episode.seq() == record.seq)
                    .is_some_and(|episode| episode.can_evict(record))
        });
        if !all_valid {
            return false;
        }
        for record in records {
            if let Some(episode) = self
                .episodes
                .iter_mut()
                .find(|episode| episode.seq() == record.seq)
            {
                episode.evict(record);
            }
        }
        true
    }

    pub fn restore_episode_span(
        &mut self,
        seq: usize,
        start_stable_id: &str,
        end_stable_id: &str,
    ) -> bool {
        self.episodes
            .iter_mut()
            .find(|episode| episode.seq() == seq)
            .is_some_and(|episode| episode.restore_span(start_stable_id, end_stable_id))
    }

    pub fn mark_episode_repaired(&mut self, seq: usize, now_ms: i64) -> bool {
        self.episodes
            .iter_mut()
            .find(|episode| episode.seq() == seq)
            .is_some_and(|episode| episode.mark_restored_after_repair(now_ms))
    }

    pub fn record_recall(&mut self, seq: usize) -> bool {
        self.episodes
            .iter_mut()
            .find(|episode| episode.seq() == seq)
            .is_some_and(Episode::record_recall)
    }

    #[cfg(test)]
    pub fn pending_title_signal(&self) -> Option<&PendingEpisodeTitleSignal> {
        self.pending_title_signal.as_ref()
    }

    pub fn set_pending_title_signal(&mut self, pending: PendingEpisodeTitleSignal) {
        self.pending_title_signal = Some(pending);
    }

    pub fn take_pending_title_signal(&mut self) -> Option<PendingEpisodeTitleSignal> {
        self.pending_title_signal.take()
    }

    pub fn clear_pending_title_signal(&mut self) {
        self.pending_title_signal = None;
    }

    /// Replace the whole ledger from a persisted snapshot. Historical episodes
    /// are preserved verbatim; `pending_title_signal` stays unset and
    /// re-derives from the next title-bearing tool.
    pub fn restore(&mut self, episodes: Vec<Episode>) {
        self.episodes = episodes;
        self.pending_title_signal = None;
    }

    pub fn snapshot(&self) -> Vec<Episode> {
        self.episodes.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episodes_are_default_on_with_explicit_false_opt_outs() {
        assert!(episodes_enabled_from(None));
        assert!(episodes_enabled_from(Some("1")));
        assert!(episodes_enabled_from(Some("true")));
        assert!(episodes_enabled_from(Some("unexpected")));
        for value in ["0", "false", "FALSE", "off", "no", "disabled"] {
            assert!(!episodes_enabled_from(Some(value)), "{value}");
        }
    }

    fn archived_user(stable_id: &str) -> ArchivedEpisodeItem {
        ArchivedEpisodeItem {
            stable_id: stable_id.to_string(),
            message: ChatCompletionRequestMessage::User(
                async_openai::types::chat::ChatCompletionRequestUserMessage {
                    content: "archived bytes".into(),
                    name: None,
                },
            ),
        }
    }

    fn eviction_record(seq: usize, marker: &str, archive_id: &str) -> EpisodeEvictionRecord {
        EpisodeEvictionRecord {
            seq,
            marker_stable_id: marker.to_string(),
            evicted_at_ms: 30,
            evicted_tokens: 100,
            card_md: "## Episode card".to_string(),
            archive: vec![archived_user(archive_id)],
        }
    }

    #[test]
    fn episode_status_db_strings_roundtrip() {
        for (status, db) in [
            (EpisodeStatus::Active, "active"),
            (EpisodeStatus::Closed, "closed"),
            (EpisodeStatus::Evicted, "evicted"),
            (EpisodeStatus::Restored, "restored"),
        ] {
            assert_eq!(status.as_db_str(), db);
            assert_eq!(EpisodeStatus::from_db_str(db), status);
        }
        assert_eq!(EpisodeStatus::from_db_str("bogus"), EpisodeStatus::Active);
    }

    #[test]
    fn episode_close_reason_db_strings_roundtrip() {
        for (reason, db) in [
            (EpisodeCloseReason::TitleChange, "title_change"),
            (EpisodeCloseReason::HardBoundary, "hard_boundary"),
            (EpisodeCloseReason::Manual, "manual"),
        ] {
            assert_eq!(reason.as_db_str(), db);
            assert_eq!(EpisodeCloseReason::from_db_str(db), reason);
        }
    }

    #[test]
    fn ledger_open_close_keeps_one_active_and_monotonic_seq() {
        let mut ledger = EpisodeLedger::default();
        let first = ledger.open_episode("msg-1".into(), String::new(), "task A".into(), 10);
        assert_eq!(first, 1);
        assert_eq!(ledger.active().map(Episode::seq), Some(1));

        // Opening over a stray active row closes it as Manual instead of
        // leaving two active episodes.
        let second = ledger.open_episode("msg-5".into(), "B".into(), "task B".into(), 20);
        assert_eq!(second, 2);
        let first = ledger.episode(1).expect("episode 1 kept");
        assert_eq!(first.status(), EpisodeStatus::Closed);
        assert_eq!(first.close_reason(), Some(EpisodeCloseReason::Manual));
        assert_eq!(first.end_stable_id(), Some("msg-5"));
        assert_eq!(ledger.active().map(Episode::seq), Some(2));

        ledger.close_active(EpisodeCloseReason::HardBoundary, "msg-9".into(), 30);
        assert!(ledger.active().is_none());
        assert_eq!(
            ledger.episode(2).and_then(Episode::close_reason),
            Some(EpisodeCloseReason::HardBoundary)
        );
    }

    #[test]
    fn ledger_close_clamps_timestamp_to_open_time() {
        let mut ledger = EpisodeLedger::default();
        ledger.open_episode("msg-1".into(), String::new(), String::new(), 100);
        ledger.close_active(EpisodeCloseReason::Manual, "msg-2".into(), 50);
        let episode = ledger.episode(1).expect("episode kept");
        assert_eq!(episode.closed_at_ms(), Some(100), "closed_at >= opened_at");
    }

    #[test]
    fn ledger_restore_replaces_episodes_and_clears_transients() {
        let mut ledger = EpisodeLedger::default();
        ledger.open_episode("msg-1".into(), String::new(), String::new(), 1);
        ledger.set_pending_title_signal(PendingEpisodeTitleSignal {
            title: "next".into(),
            call_id: "call-1".into(),
            action: EpisodeAction::NewTopic,
        });
        ledger.restore(Vec::new());
        assert!(ledger.episodes().is_empty());
        assert!(ledger.pending_title_signal().is_none());
    }

    #[test]
    fn ledger_rejects_out_of_order_lifecycle_transitions() {
        let mut ledger = EpisodeLedger::default();
        let seq = ledger.open_episode("msg-1".into(), String::new(), String::new(), 10);

        assert!(!ledger.mark_episode_repaired(seq, 20));
        assert!(!ledger.evict_episodes(vec![eviction_record(seq, "msg-marker", "msg-1")]));
        assert_eq!(
            ledger.episode(seq).map(Episode::status),
            Some(EpisodeStatus::Active)
        );

        assert_eq!(
            ledger.close_active(EpisodeCloseReason::TitleChange, "msg-2".into(), 20),
            Some(seq)
        );
        assert!(!ledger.restore_episode_span(seq, "msg-new-1", "msg-new-2"));
        assert!(ledger.evict_episodes(vec![eviction_record(seq, "msg-marker", "msg-1")]));
        assert_eq!(
            ledger.episode(seq).map(Episode::status),
            Some(EpisodeStatus::Evicted)
        );
        assert!(ledger.record_recall(seq));
        assert_eq!(ledger.episode(seq).map(Episode::recall_count), Some(1));

        assert!(ledger.restore_episode_span(seq, "msg-new-1", "msg-new-2"));
        let episode = ledger.episode(seq).expect("episode remains recorded");
        assert_eq!(episode.status(), EpisodeStatus::Restored);
        assert_eq!(episode.marker_stable_id(), None);
        assert_eq!(episode.start_stable_id(), "msg-new-1");
        assert_eq!(episode.end_stable_id(), Some("msg-new-2"));
        assert!(
            ledger
                .close_active(EpisodeCloseReason::Manual, "msg-3".into(), 40)
                .is_none()
        );
    }

    #[test]
    fn ledger_eviction_batch_is_atomic_and_rejects_duplicate_rows() {
        let mut ledger = EpisodeLedger::default();
        let first = ledger.open_episode("msg-1".into(), String::new(), String::new(), 1);
        ledger.close_active(EpisodeCloseReason::Manual, "msg-2".into(), 2);
        let second = ledger.open_episode("msg-3".into(), String::new(), String::new(), 3);
        ledger.close_active(EpisodeCloseReason::Manual, "msg-4".into(), 4);

        let mut invalid = eviction_record(second, "msg-marker-2", "msg-3");
        invalid.archive.clear();
        assert!(!ledger.evict_episodes(vec![
            eviction_record(first, "msg-marker-1", "msg-1"),
            invalid,
        ]));
        assert!(
            ledger
                .episodes()
                .iter()
                .all(|episode| episode.status() == EpisodeStatus::Closed)
        );

        assert!(!ledger.evict_episodes(vec![
            eviction_record(first, "msg-marker-1", "msg-1"),
            eviction_record(first, "msg-marker-duplicate", "msg-1"),
        ]));
        assert!(ledger.evict_episodes(vec![
            eviction_record(first, "msg-marker-1", "msg-1"),
            eviction_record(second, "msg-marker-2", "msg-3"),
        ]));
        assert!(
            ledger
                .episodes()
                .iter()
                .all(|episode| episode.status() == EpisodeStatus::Evicted)
        );
    }
}
