use std::ops::{Index, IndexMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;

use crate::tui::event::{CommandOutputKind, UiError};

/// A position inside the transcript: an item index plus a grapheme offset
/// into that item's rendered, selectable text. `width` is the card-body width
/// used to render that text, so copy uses the same Markdown layout the pointer
/// hit. `grapheme == usize::MAX` is the "whole item" sentinel used for tool
/// cards (which deliberately stay item-granular).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptPosition {
    pub item: usize,
    pub grapheme: usize,
    pub width: usize,
}

impl TranscriptPosition {
    pub fn is_whole(self) -> bool {
        self.grapheme == usize::MAX
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptSelection {
    pub anchor: TranscriptPosition,
    pub caret: TranscriptPosition,
}

impl TranscriptSelection {
    pub fn range(self) -> (TranscriptPosition, TranscriptPosition) {
        match self.anchor.item.cmp(&self.caret.item) {
            std::cmp::Ordering::Less => (self.anchor, self.caret),
            std::cmp::Ordering::Greater => (self.caret, self.anchor),
            std::cmp::Ordering::Equal => {
                if self.anchor.grapheme <= self.caret.grapheme {
                    (self.anchor, self.caret)
                } else {
                    (self.caret, self.anchor)
                }
            }
        }
    }

    /// True when the given item is inside the selection by index only -
    /// no per-grapheme resolution. Used for tool cards and for
    /// "this item is fully bracketed" detection in the renderer.
    pub fn contains_item(self, index: usize) -> bool {
        let (start, end) = self.range();
        index >= start.item && index <= end.item
    }
}

/// How a single transcript item should be painted with respect to the
/// active selection. Resolved once per item by `item_selection_state`
/// and threaded into the render pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemSelection {
    /// Not selected.
    None,
    /// The whole card paints with the selection background.
    Whole,
    /// Only graphemes `[start, end)` inside the body text are highlighted.
    /// `start < end`; both are clamped to the item's grapheme count by
    /// the renderer.
    Range { start: usize, end: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Position,
    Word,
    Line,
}

#[derive(Debug, Clone)]
pub enum TranscriptItem {
    UserMessage {
        text: String,
    },
    QueuedUserMessage {
        id: u64,
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    ReasoningSummary {
        text: String,
    },
    ToolActivity(ToolActivity),
    CommandOutput {
        kind: CommandOutputKind,
        text: String,
    },
    /// Durable, typed terminal evidence for one foreground run.
    CompletionReport(Box<crate::completion_report::CompletionReport>),
    ExecutionGroup(ExecutionGroup),
    Error {
        error: UiError,
    },
    Edit {
        text: String,
    },
    TodoUpdate {
        text: String,
    },
    /// A short internal scratch note the model emitted as pre-tool
    /// narration (e.g. "Need edit.") that was downgraded from an
    /// `AssistantMessage` so it reads as a quiet work-log breadcrumb
    /// rather than a user-facing reply. The full text stays available
    /// via the Enter→detail modal.
    WorkLog {
        text: String,
    },
    /// Inter-agent chat (peers P2): a message exchanged with another bonsai
    /// session in this project, rendered blue in the conversation flow.
    /// `session_id` is the other end — the sender when incoming, the recipient
    /// when `outgoing`.
    PeerMessage {
        /// Durable inbox row id for incoming messages. Outgoing echoes have no
        /// source id and are never part of delivery acknowledgement.
        source_message_id: Option<i64>,
        session_id: i64,
        outgoing: bool,
        text: String,
    },
}

impl TranscriptItem {
    pub(crate) fn is_suppressed(&self) -> bool {
        match self {
            Self::AssistantMessage { text }
            | Self::ReasoningSummary { text }
            | Self::WorkLog { text }
            | Self::PeerMessage { text, .. } => text.trim().is_empty(),
            Self::CommandOutput {
                kind: CommandOutputKind::Status,
                text,
            } => is_noisy_context_fallback_status(text),
            Self::ExecutionGroup(group) => group.tools.is_empty(),
            _ => false,
        }
    }
}

/// Whether a short assistant message is internal scratch / a note-to-self
/// (e.g. "Need edit.", "Use line exact.") rather than something worth
/// showing the user as a reply. Conservative: defaults to `false`, so
/// anything that looks like real narration or a genuine answer stays an
/// `AssistantMessage`. The structural caller (only reclassifying text that
/// is immediately followed by a tool group) is the real safety net — a
/// genuine final answer is never followed by a tool call, so this heuristic
/// only ever sees pre-tool narration.
pub(crate) fn is_scratch_fragment(text: &str) -> bool {
    let t = text.trim();
    // Real content: multi-line, long, or carrying code/paths.
    if t.is_empty() || t.contains('\n') || t.chars().count() > 80 {
        return false;
    }
    if t.contains("```") || looks_like_path(t) {
        return false;
    }
    let words = t.split_whitespace().count();
    if words > 8 {
        return false;
    }
    // Allowlist: genuine short user-facing messages stay assistant messages.
    const FINAL_ANSWER_LEADS: &[&str] = &[
        "Done", "Fixed", "Added", "Updated", "Removed", "Here", "I ", "I'll", "I've", "The ",
        "This ", "That ", "It ", "Yes", "No", "Sorry", "All ", "Looks", "Nothing",
    ];
    if starts_with_any_ci(t, FINAL_ANSWER_LEADS) {
        return false;
    }
    const SCRATCH_LEADS: &[&str] = &[
        "Need",
        "Needs",
        "Want",
        "Gonna",
        "Lemme",
        "Use ",
        "Do one",
        "One by one",
        "Then",
        "Next",
        "Also",
        "Maybe",
        "Probably",
        "Actually",
        "Wait",
        "Hmm",
        "Let me check",
        "Let me look",
        "Let me see",
    ];
    let ends_terminal = t.ends_with('.') || t.ends_with("...") || t.ends_with('…');
    if !ends_terminal {
        return false;
    }
    starts_with_any_ci(t, SCRATCH_LEADS)
}

fn starts_with_any_ci(text: &str, leads: &[&str]) -> bool {
    leads.iter().any(|lead| {
        text.get(..lead.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(lead))
    })
}

/// Heuristic: does the text contain a path-like token (a slash between
/// non-space chars, or a bare `name.ext`)? Such messages are real
/// answers ("Fixed config.rs."), not scratch.
fn looks_like_path(text: &str) -> bool {
    text.split_whitespace().any(|tok| {
        let tok = tok.trim_end_matches(['.', ',', ')', ':', ';']);
        (tok.contains('/') && !tok.starts_with('/'))
            || (tok.contains('.')
                && tok.rsplit('.').next().is_some_and(|ext| {
                    (1..=4).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphabetic())
                }))
    })
}

pub(crate) fn is_noisy_context_fallback_status(text: &str) -> bool {
    text.starts_with("[context] prompt estimate ")
        && text.contains("continuing with heuristic fallback")
}

#[derive(Debug, Clone)]
pub struct ExecutionGroup {
    pub id: u64,
    pub finished_at: Option<Instant>,
    pub tools: Vec<ToolActivity>,
}

impl ExecutionGroup {
    pub(crate) fn new(id: u64, _started_at: Instant) -> Self {
        Self {
            id,
            finished_at: None,
            tools: Vec::new(),
        }
    }

    pub(crate) fn tool_counts(&self) -> (usize, usize, usize, usize) {
        self.tools
            .iter()
            .fold((0, 0, 0, 0), |acc, activity| match activity.status {
                ToolStatus::Running => (acc.0, acc.1, acc.2 + 1, acc.3 + 1),
                ToolStatus::Succeeded => (acc.0 + 1, acc.1, acc.2, acc.3 + 1),
                ToolStatus::Failed => (acc.0, acc.1 + 1, acc.2, acc.3 + 1),
            })
    }

    pub(crate) fn tool_indices(&self) -> std::ops::Range<usize> {
        0..self.tools.len()
    }

    fn selected_tool_index(&self, selected_tool: usize) -> Option<usize> {
        if selected_tool < self.tools.len() {
            Some(selected_tool)
        } else {
            None
        }
    }

    pub(crate) fn selected_tool(&self, selected_tool: usize) -> Option<&ToolActivity> {
        self.selected_tool_index(selected_tool)
            .and_then(|index| self.tools.get(index))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineToolSelection {
    pub group_id: u64,
    pub selected_tool: usize,
}

#[derive(Debug, Clone)]
pub struct ToolActivity {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub status: ToolStatus,
    pub result: Option<String>,
    pub diff: Option<crate::diff::FileDiff>,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
}

impl ToolActivity {
    pub(crate) fn new(id: String, name: String, arguments: String, started_at: Instant) -> Self {
        Self {
            id,
            name,
            arguments,
            status: ToolStatus::Running,
            result: None,
            diff: None,
            started_at,
            finished_at: None,
        }
    }

    pub fn duration(&self) -> Duration {
        self.finished_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(self.started_at)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Succeeded,
    Failed,
}

crate::impl_db_enum!(ToolStatus {
    Running => "running",
    Succeeded => "succeeded",
    Failed => "failed",
} else Running);

impl ToolStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "done",
            Self::Failed => "failed",
        }
    }
}

/// Process-wide id source for [`ItemStamp::id`]. Global — never per-model —
/// so a model rebuilt from a snapshot (`TranscriptModel::from(vec)` on session
/// restore) can never mint ids that alias a previous model's items and let the
/// transcript layout cache serve stale lines under a recycled identity.
static NEXT_ITEM_ID: AtomicU64 = AtomicU64::new(0);

fn alloc_item_id() -> u64 {
    NEXT_ITEM_ID.fetch_add(1, Ordering::Relaxed)
}

/// Cache identity of one transcript item: a process-unique `id` plus a `rev`
/// that bumps on every potential mutation. The transcript layout cache
/// re-renders an item iff its stamp (or its render options) changed, so any
/// mutation path that could alter rendered output must bump here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ItemStamp {
    pub(crate) id: u64,
    pub(crate) rev: u32,
}

impl ItemStamp {
    fn fresh() -> Self {
        Self {
            id: alloc_item_id(),
            rev: 0,
        }
    }

    fn bump(&mut self) {
        self.rev = self.rev.wrapping_add(1);
    }
}

#[derive(Debug, Default)]
pub struct TranscriptModel {
    items: Vec<TranscriptItem>,
    /// Index-parallel to `items`; every mutator keeps the two in lockstep.
    meta: Vec<ItemStamp>,
    assistant_messages: usize,
}

impl Clone for TranscriptModel {
    /// Deliberately not derived: a clone allocates fresh stamps. Stamps are
    /// cache identities, and two live models sharing ids would let a layout
    /// cache keyed on one serve rendered lines for the other.
    fn clone(&self) -> Self {
        Self {
            items: self.items.clone(),
            meta: self.items.iter().map(|_| ItemStamp::fresh()).collect(),
            assistant_messages: self.assistant_messages,
        }
    }
}

impl TranscriptModel {
    fn count_assistant_messages(items: &[TranscriptItem]) -> usize {
        items
            .iter()
            .filter(|item| matches!(item, TranscriptItem::AssistantMessage { .. }))
            .count()
    }

    const fn is_assistant_message(item: &TranscriptItem) -> bool {
        matches!(item, TranscriptItem::AssistantMessage { .. })
    }

    pub fn as_slice(&self) -> &[TranscriptItem] {
        &self.items
    }

    pub fn to_vec(&self) -> Vec<TranscriptItem> {
        self.items.clone()
    }

    pub fn into_vec(self) -> Vec<TranscriptItem> {
        self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.meta.clear();
        self.assistant_messages = 0;
    }

    pub fn push(&mut self, item: TranscriptItem) {
        if Self::is_assistant_message(&item) {
            self.assistant_messages = self.assistant_messages.saturating_add(1);
        }
        self.items.push(item);
        self.meta.push(ItemStamp::fresh());
    }

    pub fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = TranscriptItem>,
    {
        for item in iter {
            self.push(item);
        }
    }

    pub fn insert(&mut self, index: usize, item: TranscriptItem) {
        if Self::is_assistant_message(&item) {
            self.assistant_messages = self.assistant_messages.saturating_add(1);
        }
        self.items.insert(index, item);
        self.meta.insert(index, ItemStamp::fresh());
    }

    pub fn remove(&mut self, index: usize) -> TranscriptItem {
        let item = self.items.remove(index);
        self.meta.remove(index);
        if Self::is_assistant_message(&item) {
            self.assistant_messages = self.assistant_messages.saturating_sub(1);
        }
        item
    }

    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&TranscriptItem) -> bool,
    {
        // `Vec::retain` visits every element in order exactly once, so the
        // verdicts recorded while filtering `items` are the mask that filters
        // `meta` in lockstep.
        let mut kept = Vec::with_capacity(self.items.len());
        self.items.retain(|item| {
            let keep = f(item);
            kept.push(keep);
            keep
        });
        let mut verdicts = kept.into_iter();
        self.meta
            .retain(|_| verdicts.next().expect("verdict mask covers meta"));
        self.assistant_messages = Self::count_assistant_messages(&self.items);
    }

    /// Downgrade the `AssistantMessage` at `index` to a `WorkLog` note in
    /// place, keeping its index (so transcript focus/selection stay valid)
    /// and the assistant-message counter accurate. No-op if the item at
    /// `index` is not an `AssistantMessage`.
    pub(crate) fn reclassify_assistant_as_worklog(&mut self, index: usize) {
        if let Some(item) = self.items.get_mut(index)
            && let TranscriptItem::AssistantMessage { text } = item
        {
            *item = TranscriptItem::WorkLog {
                text: std::mem::take(text),
            };
            self.assistant_messages = self.assistant_messages.saturating_sub(1);
            self.bump(index);
        }
    }

    pub fn get(&self, index: usize) -> Option<&TranscriptItem> {
        self.items.get(index)
    }

    /// Pessimistic revision tracking: a mutable borrow is treated as a
    /// mutation, so the layout cache re-renders this item next frame. A
    /// `get_mut` that ends up not writing costs one spurious re-render of one
    /// item — always sound, and the hot streaming-append path really does
    /// write on every call.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut TranscriptItem> {
        self.bump(index);
        self.items.get_mut(index)
    }

    pub fn last(&self) -> Option<&TranscriptItem> {
        self.items.last()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, TranscriptItem> {
        self.items.iter()
    }

    /// Pessimistic like [`Self::get_mut`], but the iterator can't tell which
    /// items the caller writes, so every stamp bumps and the whole transcript
    /// re-renders once. Reserved for rare whole-transcript passes (permission
    /// timer resets); index-precise paths must use `get_mut` instead.
    pub fn iter_mut(&mut self) -> std::slice::IterMut<'_, TranscriptItem> {
        for stamp in &mut self.meta {
            stamp.bump();
        }
        self.items.iter_mut()
    }

    /// Stamps for every item, index-parallel to [`Self::as_slice`]. Cache
    /// identity for the transcript layout cache.
    pub(crate) fn stamps(&self) -> &[ItemStamp] {
        debug_assert_eq!(self.items.len(), self.meta.len());
        &self.meta
    }

    fn bump(&mut self, index: usize) {
        if let Some(stamp) = self.meta.get_mut(index) {
            stamp.bump();
        }
    }

    pub(crate) fn assistant_message_count(&self) -> usize {
        self.assistant_messages
    }

    pub(crate) fn push_item(
        &mut self,
        item: TranscriptItem,
        transcript_focus: &mut Option<usize>,
        transcript_selection: &mut Option<TranscriptSelection>,
    ) {
        if item.is_suppressed() {
            return;
        }

        if matches!(item, TranscriptItem::QueuedUserMessage { .. }) {
            self.push(item);
            return;
        }

        let index = self.first_trailing_queued_index();
        if index == self.items.len() {
            self.push(item);
        } else {
            shift_positions_after_insert(transcript_focus, transcript_selection, index);
            self.insert(index, item);
        }
    }

    pub(crate) fn selected_text(&self, selection: Option<TranscriptSelection>) -> Option<String> {
        let selection = selection?;
        let (start, end) = selection.range();
        let mut text = String::new();
        let mut first = true;
        for index in start.item..=end.item {
            let Some(item) = self.items.get(index) else {
                break;
            };
            let slice = transcript_slice_for(item, index, start, end);
            if slice.is_empty() {
                continue;
            }
            if !first {
                text.push_str("\n\n");
            }
            text.push_str(&slice);
            first = false;
        }
        (!text.is_empty()).then_some(text)
    }

    pub(crate) fn has_text_selection(selection: Option<TranscriptSelection>) -> bool {
        selection.is_some_and(|selection| {
            let (start, end) = selection.range();
            start.item != end.item || start.grapheme != end.grapheme
        })
    }

    pub(crate) fn collect_text(&self) -> Option<String> {
        crate::copy::collect_transcript_text(self.as_slice(), crate::copy::CopyTarget::All)
    }

    pub(crate) fn first_trailing_queued_index(&self) -> usize {
        let mut index = self.items.len();
        while index > 0
            && matches!(
                self.items[index - 1],
                TranscriptItem::QueuedUserMessage { .. }
            )
        {
            index -= 1;
        }
        index
    }
}

/// Substring of `item` that falls inside the selection `[start, end]`.
/// Whole-item endpoints and bracketed middle items return the visible body.
fn transcript_slice_for(
    item: &TranscriptItem,
    index: usize,
    start: TranscriptPosition,
    end: TranscriptPosition,
) -> String {
    let width = if index == start.item {
        start.width
    } else {
        end.width
    };
    let body = crate::tui::widgets::transcript::selection_text_for(item, width);
    let total = body.graphemes(true).count();
    let same_item = start.item == end.item;
    let start_whole = start.is_whole();
    let end_whole = end.is_whole();

    if same_item {
        let s = start.grapheme.min(total);
        let e = end.grapheme.min(total);
        grapheme_substring(&body, s, e).to_string()
    } else if index == start.item && !start_whole {
        let s = start.grapheme.min(total);
        grapheme_substring(&body, s, total).to_string()
    } else if index == end.item && !end_whole {
        let e = end.grapheme.min(total);
        grapheme_substring(&body, 0, e).to_string()
    } else {
        body
    }
}

fn grapheme_substring(text: &str, start_grapheme: usize, end_grapheme: usize) -> &str {
    let (lo, _) = text
        .grapheme_indices(true)
        .nth(start_grapheme)
        .unwrap_or((text.len(), ""));
    let (hi, _) = text
        .grapheme_indices(true)
        .nth(end_grapheme)
        .unwrap_or((text.len(), ""));
    if hi <= lo { "" } else { &text[lo..hi] }
}

impl From<Vec<TranscriptItem>> for TranscriptModel {
    fn from(items: Vec<TranscriptItem>) -> Self {
        let assistant_messages = Self::count_assistant_messages(&items);
        let meta = items.iter().map(|_| ItemStamp::fresh()).collect();
        Self {
            items,
            meta,
            assistant_messages,
        }
    }
}

impl AsRef<[TranscriptItem]> for TranscriptModel {
    fn as_ref(&self) -> &[TranscriptItem] {
        self.as_slice()
    }
}

impl Index<usize> for TranscriptModel {
    type Output = TranscriptItem;

    fn index(&self, index: usize) -> &Self::Output {
        &self.items[index]
    }
}

impl IndexMut<usize> for TranscriptModel {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        self.bump(index);
        &mut self.items[index]
    }
}

impl<'a> IntoIterator for &'a TranscriptModel {
    type Item = &'a TranscriptItem;
    type IntoIter = std::slice::Iter<'a, TranscriptItem>;

    fn into_iter(self) -> Self::IntoIter {
        self.items.iter()
    }
}

impl<'a> IntoIterator for &'a mut TranscriptModel {
    type Item = &'a mut TranscriptItem;
    type IntoIter = std::slice::IterMut<'a, TranscriptItem>;

    fn into_iter(self) -> Self::IntoIter {
        // Routes through `iter_mut` so the pessimistic bump-all revision
        // tracking applies to `for item in &mut model` loops too.
        self.iter_mut()
    }
}

fn shift_positions_after_insert(
    transcript_focus: &mut Option<usize>,
    transcript_selection: &mut Option<TranscriptSelection>,
    index: usize,
) {
    if let Some(focus) = *transcript_focus
        && focus >= index
    {
        *transcript_focus = Some(focus.saturating_add(1));
    }
    if let Some(selection) = transcript_selection {
        shift_position_after_insert(&mut selection.anchor, index);
        shift_position_after_insert(&mut selection.caret, index);
    }
}

fn shift_position_after_insert(position: &mut TranscriptPosition, index: usize) {
    if position.item >= index {
        position.item = position.item.saturating_add(1);
    }
}

#[cfg(test)]
mod stamp_tests {
    use super::{TranscriptItem, TranscriptModel};

    fn text_item(text: &str) -> TranscriptItem {
        TranscriptItem::AssistantMessage {
            text: text.to_string(),
        }
    }

    #[test]
    fn stamps_stay_lockstep_across_mutators() {
        let mut model = TranscriptModel::default();
        model.push(text_item("a"));
        model.push(text_item("b"));
        model.insert(1, text_item("c"));
        assert_eq!(model.stamps().len(), model.len());

        let ids: Vec<u64> = model.stamps().iter().map(|stamp| stamp.id).collect();
        assert_eq!(
            ids.iter().collect::<std::collections::HashSet<_>>().len(),
            ids.len(),
            "ids must be unique"
        );

        // Removal drops the stamp at the same index.
        let before = model.stamps().to_vec();
        model.remove(1);
        assert_eq!(model.stamps(), &[before[0], before[2]]);

        // Retain filters stamps by the same mask as items.
        model.push(text_item("keep"));
        let keep_stamp = model.stamps()[model.len() - 1];
        model.retain(
            |item| matches!(item, TranscriptItem::AssistantMessage { text } if text == "keep"),
        );
        assert_eq!(model.stamps(), &[keep_stamp]);

        model.clear();
        assert!(model.stamps().is_empty());
    }

    #[test]
    fn get_mut_bumps_only_its_index() {
        let mut model = TranscriptModel::default();
        model.push(text_item("a"));
        model.push(text_item("b"));
        let before = model.stamps().to_vec();

        model.get_mut(1);

        let after = model.stamps();
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1].id, before[1].id);
        assert_eq!(after[1].rev, before[1].rev + 1);
    }

    #[test]
    fn iter_mut_bumps_every_stamp() {
        let mut model = TranscriptModel::default();
        model.push(text_item("a"));
        model.push(text_item("b"));
        let before = model.stamps().to_vec();

        for _item in &mut model {}

        for (stamp, old) in model.stamps().iter().zip(&before) {
            assert_eq!(stamp.id, old.id);
            assert_eq!(stamp.rev, old.rev + 1);
        }
    }

    #[test]
    fn reclassify_bumps_the_reclassified_index() {
        let mut model = TranscriptModel::default();
        model.push(text_item("Need edit."));
        let before = model.stamps()[0];

        model.reclassify_assistant_as_worklog(0);

        assert_eq!(model.stamps()[0].rev, before.rev + 1);
    }

    #[test]
    fn from_vec_and_clone_allocate_fresh_ids() {
        let items = vec![text_item("a"), text_item("b")];
        let first = TranscriptModel::from(items.clone());
        let second = TranscriptModel::from(items);
        let cloned = first.clone();

        let mut seen = std::collections::HashSet::new();
        for model in [&first, &second, &cloned] {
            for stamp in model.stamps() {
                assert!(
                    seen.insert(stamp.id),
                    "id {} reused across models",
                    stamp.id
                );
            }
        }
        assert_eq!(cloned.len(), first.len());
        assert_eq!(cloned.assistant_message_count(), 2);
    }
}

#[cfg(test)]
mod tool_status_tests {
    use super::{ToolStatus, TranscriptItem, TranscriptModel, is_scratch_fragment};

    #[test]
    fn tool_status_db_strings_roundtrip() {
        for (status, db, label) in [
            (ToolStatus::Running, "running", "running"),
            (ToolStatus::Succeeded, "succeeded", "done"),
            (ToolStatus::Failed, "failed", "failed"),
        ] {
            assert_eq!(status.as_db_str(), db);
            assert_eq!(ToolStatus::from_db_str(db), status);
            assert_eq!(status.label(), label);
        }
        assert_eq!(ToolStatus::from_db_str("unknown"), ToolStatus::Running);
    }

    #[test]
    fn transcript_model_tracks_assistant_message_count() {
        let mut transcript = TranscriptModel::default();
        transcript.push(TranscriptItem::AssistantMessage {
            text: "one".to_string(),
        });
        transcript.push(TranscriptItem::CommandOutput {
            kind: crate::tui::event::CommandOutputKind::Status,
            text: "status".to_string(),
        });
        transcript.insert(
            1,
            TranscriptItem::AssistantMessage {
                text: "two".to_string(),
            },
        );
        assert_eq!(transcript.assistant_message_count(), 2);

        transcript.remove(0);
        assert_eq!(transcript.assistant_message_count(), 1);

        transcript.retain(|item| !matches!(item, TranscriptItem::AssistantMessage { .. }));
        assert_eq!(transcript.assistant_message_count(), 0);

        let transcript = TranscriptModel::from(vec![
            TranscriptItem::AssistantMessage {
                text: "loaded".to_string(),
            },
            TranscriptItem::ReasoningSummary {
                text: "thinking".to_string(),
            },
        ]);
        assert_eq!(transcript.assistant_message_count(), 1);
    }

    #[test]
    fn is_scratch_fragment_flags_telegraphic_notes() {
        // The real leaks observed live.
        for note in [
            "Need edit.",
            "Need tail.",
            "Need builder field.",
            "Use line exact.",
            "Do one by one exact.",
        ] {
            assert!(is_scratch_fragment(note), "{note:?} should be scratch");
        }
    }

    #[test]
    fn is_scratch_fragment_keeps_genuine_messages() {
        // Genuine short answers, code/path references, questions, long
        // narration, and verb-led status lines must stay assistant messages.
        for keep in [
            "Done.",
            "Fixed the typo in config.rs.",
            "All tests pass.",
            "Refactoring the parser to handle nested groups.",
            "Running tests.",
            "Run tests.",
            "Checking format.",
            "Running the test suite now.",
            "Which file did you mean?",
            "I'll update the reducer next.",
            "",
            "Need to edit a few files,\nthen run the tests.", // multi-line
        ] {
            assert!(
                !is_scratch_fragment(keep),
                "{keep:?} should NOT be treated as scratch"
            );
        }
    }

    #[test]
    fn reclassify_assistant_as_worklog_swaps_in_place_and_decrements_count() {
        let mut transcript = TranscriptModel::default();
        transcript.push(TranscriptItem::UserMessage {
            text: "go".to_string(),
        });
        transcript.push(TranscriptItem::AssistantMessage {
            text: "Need edit.".to_string(),
        });
        assert_eq!(transcript.assistant_message_count(), 1);

        transcript.reclassify_assistant_as_worklog(1);

        assert_eq!(transcript.assistant_message_count(), 0);
        assert!(matches!(
            transcript.get(1),
            Some(TranscriptItem::WorkLog { text }) if text == "Need edit."
        ));
        // The user message at index 0 is untouched (index stability).
        assert!(matches!(
            transcript.get(0),
            Some(TranscriptItem::UserMessage { .. })
        ));

        // No-op on a non-assistant item.
        transcript.reclassify_assistant_as_worklog(0);
        assert!(matches!(
            transcript.get(0),
            Some(TranscriptItem::UserMessage { .. })
        ));
    }
}
