use unicode_segmentation::UnicodeSegmentation;

use crate::agent::{ImageAttachment, UserInput};
use crate::tui::image_paste::{ENCODED_IMAGE_MIME, EncodedImage};
use crate::tui::text_bounds;

const INPUT_HISTORY_LIMIT: usize = 200;
const EDIT_HISTORY_LIMIT: usize = 200;
const PASTE_HARD_LIMIT: usize = 256 * 1024;

/// One buffer grapheme standing in for a collapsed paste. Because it is a
/// single grapheme cluster, every grapheme-granular editing op (backspace,
/// delete, arrow motion, selection) treats a chip atomically for free — the
/// cursor can never land "inside" it. The `chips` list runs parallel to these
/// sentinels: the i-th sentinel in `text` maps to `chips[i]`. See
/// [`Composer::remove_chips_in`] for how that invariant is maintained.
pub const CHIP_SENTINEL: char = '\u{FFFC}';

/// The payload a paste chip stands in for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChipPayload {
    /// Collapsed long text paste. Held verbatim (sentinel-stripped, newline
    /// normalized); expanded back into the model text on submit.
    Text(String),
    /// A pasted image, already encoded to a bounded base64 PNG.
    Image {
        mime: String,
        base64: String,
        byte_len: usize,
        width: u32,
        height: u32,
    },
}

/// A collapsed paste shown in the composer as a compact `[Text N]` / `[Image N]`
/// label while its full payload waits to be sent on submit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerChip {
    pub label: String,
    pub payload: ChipPayload,
}

/// The composer's editable state as a portable snapshot: the raw buffer (with
/// sentinels) plus the parallel chip list. Used to stash/restore a draft or a
/// withdrawn queued message without losing chip payloads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComposerContent {
    pub text: String,
    pub chips: Vec<ComposerChip>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ComposerEditState {
    content: ComposerContent,
    cursor: usize,
    selection_anchor: Option<usize>,
    next_text_seq: u32,
    next_image_seq: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ComposerDraft {
    state: ComposerEditState,
    undo: Vec<ComposerEditState>,
    redo: Vec<ComposerEditState>,
}

/// What a submit produces: the placeholder text the user saw (slash-command
/// detection, queued rows, status summaries), the transcript form (text-chip
/// payloads spliced back in verbatim so the chat shows what was actually
/// pasted; image chips keep their labels), and the fully-expanded model input
/// (chip payloads in tags, images collected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerSubmission {
    pub display_text: String,
    pub transcript_text: String,
    pub input: UserInput,
}

/// One rendered sentinel: where its label sits in the display text and which
/// buffer grapheme it stands for. Powers index conversion between the buffer
/// (1 grapheme per chip) and the display string (the full `[Text N]` label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChipSentinel {
    buffer_index: usize,
    display_start: usize,
    label_len: usize,
}

/// The composer buffer with chip sentinels expanded to their labels, so the
/// widget layer can wrap, place the cursor, and hit-test against pure ASCII
/// where one grapheme is one column. Convert indices at the boundary with
/// [`ChipDisplay::to_display`] / [`ChipDisplay::to_buffer`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChipDisplay {
    pub text: String,
    /// Display-grapheme ranges of each chip label, for pill styling.
    pub chip_ranges: Vec<(usize, usize)>,
    sentinels: Vec<ChipSentinel>,
}

impl ChipDisplay {
    /// Map a buffer grapheme index to its display grapheme index. Each sentinel
    /// strictly before the index adds `label_len - 1` extra display cells.
    pub fn to_display(&self, buffer_idx: usize) -> usize {
        let mut extra = 0;
        for sentinel in &self.sentinels {
            if sentinel.buffer_index < buffer_idx {
                extra += sentinel.label_len - 1;
            } else {
                break;
            }
        }
        buffer_idx + extra
    }

    /// Map a display grapheme index back to a buffer grapheme index. An index
    /// that lands inside a chip label snaps to the nearer chip edge, so a click
    /// on a chip places the cursor at one of its boundaries, never inside it.
    pub fn to_buffer(&self, display_idx: usize) -> usize {
        let mut extra = 0;
        for sentinel in &self.sentinels {
            let label_end = sentinel.display_start + sentinel.label_len;
            if display_idx <= sentinel.display_start {
                return display_idx - extra;
            }
            if display_idx < label_end {
                let from_start = display_idx - sentinel.display_start;
                let from_end = label_end - display_idx;
                return if from_start <= from_end {
                    sentinel.buffer_index
                } else {
                    sentinel.buffer_index + 1
                };
            }
            extra += sentinel.label_len - 1;
        }
        display_idx - extra
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Composer {
    pub text: String,
    pub cursor: usize,
    pub history: Vec<String>,
    pub history_cursor: Option<usize>,
    draft: Option<ComposerDraft>,
    pub selection_anchor: Option<usize>,
    /// Paste-chip payloads, one per [`CHIP_SENTINEL`] in `text`, in buffer order.
    pub chips: Vec<ComposerChip>,
    /// Per-draft monotonic label counters (1-based). Never renumbered on delete:
    /// a label is a stable identifier, so deleting `[Text 1]` then pasting again
    /// yields `[Text 2]`. Reset by [`Composer::clear_content`].
    next_text_seq: u32,
    next_image_seq: u32,
    undo: Vec<ComposerEditState>,
    redo: Vec<ComposerEditState>,
}

impl Composer {
    /// Replace the whole buffer with plain text, dropping any chips. Sentinels
    /// in the incoming string are stripped so a recalled history entry (which
    /// stores the placeholder labels as literal text) can never resurrect a
    /// dangling chip mapping.
    pub fn set_text(&mut self, text: String) {
        let text = if text.contains(CHIP_SENTINEL) {
            text.replace(CHIP_SENTINEL, "")
        } else {
            text
        };
        self.cursor = text_bounds::grapheme_count(&text);
        self.text = text;
        self.chips.clear();
        self.selection_anchor = None;
        self.clear_edit_history();
    }

    /// Drop the chips whose sentinel falls in the grapheme range `[start, end)`,
    /// keeping `chips` aligned with the sentinels in `text`. Must be called
    /// *before* the buffer bytes in that range are removed. Counts by char (not
    /// grapheme) so a combining mark that merged into a sentinel's cluster can't
    /// desync the mapping.
    fn remove_chips_in(&mut self, start: usize, end: usize) {
        if self.chips.is_empty() || start >= end {
            return;
        }
        let byte_start = self.byte_index_for_text_index(start);
        let byte_end = self.byte_index_for_text_index(end);
        let before = self.text[..byte_start].matches(CHIP_SENTINEL).count();
        let inside = self.text[byte_start..byte_end]
            .matches(CHIP_SENTINEL)
            .count();
        if inside > 0 {
            self.chips.drain(before..before + inside);
        }
    }

    pub fn has_selection(&self) -> bool {
        self.selection_anchor.is_some_and(|a| a != self.cursor)
    }

    pub fn selection_range(&self) -> Option<(usize, usize)> {
        self.selection_anchor.map(|a| {
            if a <= self.cursor {
                (a, self.cursor)
            } else {
                (self.cursor, a)
            }
        })
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_range()?;
        if start == end {
            return None;
        }
        let byte_start = self.byte_index_for_text_index(start);
        let byte_end = self.byte_index_for_text_index(end);
        Some(self.text[byte_start..byte_end].to_string())
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    #[cfg(test)]
    pub fn set_selection(&mut self, anchor: usize, caret: usize) {
        let total = text_bounds::grapheme_count(&self.text);
        let caret = caret.min(total);
        let anchor = anchor.min(total);
        if anchor == caret {
            self.selection_anchor = None;
        } else {
            self.selection_anchor = Some(anchor);
        }
        self.cursor = caret;
    }

    pub fn move_to(&mut self, index: usize) {
        let total = text_bounds::grapheme_count(&self.text);
        self.cursor = index.min(total);
        self.selection_anchor = None;
    }

    pub fn extend_selection_to(&mut self, index: usize) {
        let total = text_bounds::grapheme_count(&self.text);
        let caret = index.min(total);
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
        self.cursor = caret;
    }

    pub fn select_all(&mut self) {
        let total = text_bounds::grapheme_count(&self.text);
        if total == 0 {
            self.selection_anchor = None;
            return;
        }
        self.selection_anchor = Some(0);
        self.cursor = total;
    }

    pub fn select_word_at(&mut self, index: usize) {
        let total = text_bounds::grapheme_count(&self.text);
        let caret = index.min(total);
        let (start, end) = text_bounds::word_bounds_at(&self.text, caret);
        if start == end {
            self.selection_anchor = None;
            self.cursor = caret;
        } else {
            self.selection_anchor = Some(start);
            self.cursor = end;
        }
    }

    pub fn select_line_at(&mut self, index: usize) {
        let total = text_bounds::grapheme_count(&self.text);
        let caret = index.min(total);
        let (row, _) = self.cursor_row_col_at(caret);
        let start = self.line_start(row);
        let end = self.line_start(row) + self.line_length(row);
        if start == end {
            self.selection_anchor = None;
            self.cursor = caret;
        } else {
            self.selection_anchor = Some(start);
            self.cursor = end;
        }
    }

    fn replace_selection(&mut self, replacement: &str) {
        if let Some((start, end)) = self.selection_range() {
            self.remove_chips_in(start, end);
            let byte_start = self.byte_index_for_text_index(start);
            let byte_end = self.byte_index_for_text_index(end);
            self.text.replace_range(byte_start..byte_end, replacement);
            self.cursor = start + text_bounds::grapheme_count(replacement);
            self.selection_anchor = None;
        }
    }

    pub fn replace_range(&mut self, start: usize, end: usize, replacement: &str) {
        let before = self.edit_state();
        let total = text_bounds::grapheme_count(&self.text);
        let start = start.min(total);
        let end = end.min(total).max(start);
        self.remove_chips_in(start, end);
        let byte_start = self.byte_index_for_text_index(start);
        let byte_end = self.byte_index_for_text_index(end);
        self.text.replace_range(byte_start..byte_end, replacement);
        self.cursor = start + text_bounds::grapheme_count(replacement);
        self.selection_anchor = None;
        self.record_edit(before);
    }

    pub fn insert_char(&mut self, ch: char) {
        // The sentinel is reserved for chips; a stray one in the buffer would
        // desync the chip mapping. Chips only ever enter via insert_*_chip.
        if ch == CHIP_SENTINEL {
            return;
        }
        let before = self.edit_state();
        if self.has_selection() {
            self.replace_selection(&ch.to_string());
            self.record_edit(before);
            return;
        }
        let byte = self.byte_index_for_cursor();
        self.text.insert(byte, ch);
        self.cursor = text_bounds::grapheme_index_after_byte(&self.text, byte + ch.len_utf8());
        // Collapse any dangling anchor, exactly as `move_to` does. A mouse click
        // leaves a zero-width anchor (`anchor == cursor`, which `has_selection`
        // treats as no selection); inserting here advances the cursor past it,
        // which would silently promote it to a real one-char selection and make
        // the *next* keystroke overwrite this char. That was the "first letter
        // gets eaten" bug: click, type `/`, type `b` → `b`.
        self.selection_anchor = None;
        self.record_edit(before);
    }

    pub fn insert_newline(&mut self) {
        if self.has_selection() {
            self.replace_selection("\n");
            return;
        }
        self.insert_char('\n');
    }

    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = normalize_paste(text);
        if normalized.is_empty() {
            return;
        }
        if text_bounds::grapheme_count(&self.text) + text_bounds::grapheme_count(&normalized)
            > PASTE_HARD_LIMIT
        {
            return;
        }
        let before = self.edit_state();
        if self.has_selection() {
            self.replace_selection(&normalized);
            self.record_edit(before);
            return;
        }
        let byte = self.byte_index_for_cursor();
        self.text.insert_str(byte, &normalized);
        // Recompute from the byte position after the inserted text, exactly as
        // insert_char does. `cursor += grapheme_count(normalized)` overshoots
        // when the pasted text begins with a combining mark that merges with the
        // grapheme before the caret.
        self.cursor = text_bounds::grapheme_index_after_byte(&self.text, byte + normalized.len());
        // Collapse any dangling zero-width anchor left by a click; see the note
        // in `insert_char`.
        self.selection_anchor = None;
        self.record_edit(before);
    }

    pub fn backspace(&mut self) {
        let before = self.edit_state();
        if self.has_selection() {
            self.replace_selection("");
            self.record_edit(before);
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let prev = self.previous_boundary(self.cursor);
        self.remove_chips_in(prev, self.cursor);
        let byte = self.byte_index_for_text_index(prev);
        let end = self.byte_index_for_text_index(self.cursor);
        self.text.drain(byte..end);
        self.cursor = prev;
        self.record_edit(before);
    }

    pub fn delete_forward(&mut self) {
        let before = self.edit_state();
        if self.has_selection() {
            self.replace_selection("");
            self.record_edit(before);
            return;
        }
        if self.cursor >= text_bounds::grapheme_count(&self.text) {
            return;
        }
        let next = self.next_boundary(self.cursor);
        self.remove_chips_in(self.cursor, next);
        let start = self.byte_index_for_text_index(self.cursor);
        let end = self.byte_index_for_text_index(next);
        self.text.drain(start..end);
        self.record_edit(before);
    }

    pub fn move_left(&mut self) {
        if let Some((start, _end)) = self.selection_range() {
            self.cursor = start;
            self.selection_anchor = None;
            return;
        }
        if self.cursor > 0 {
            self.cursor = self.previous_boundary(self.cursor);
        }
    }

    pub fn move_right(&mut self) {
        if let Some((_start, end)) = self.selection_range() {
            self.cursor = end;
            self.selection_anchor = None;
            return;
        }
        let total = text_bounds::grapheme_count(&self.text);
        if self.cursor < total {
            self.cursor = self.next_boundary(self.cursor);
        }
    }

    pub fn extend_left(&mut self) {
        let anchor = self.selection_anchor.unwrap_or(self.cursor);
        if self.cursor > 0 {
            self.cursor = self.previous_boundary(self.cursor);
        }
        self.selection_anchor = Some(anchor);
    }

    pub fn extend_right(&mut self) {
        let anchor = self.selection_anchor.unwrap_or(self.cursor);
        let total = text_bounds::grapheme_count(&self.text);
        if self.cursor < total {
            self.cursor = self.next_boundary(self.cursor);
        }
        self.selection_anchor = Some(anchor);
    }

    pub fn move_up(&mut self) {
        self.selection_anchor = None;
        let (row, col) = self.cursor_row_col();
        if row == 0 {
            return;
        }
        let prev_row_len = self.line_length(row - 1);
        self.cursor = self.line_start(row - 1) + col.min(prev_row_len);
    }

    pub fn move_down(&mut self) {
        self.selection_anchor = None;
        let (row, col) = self.cursor_row_col();
        let total_rows = self.text.split('\n').count();
        if row + 1 >= total_rows {
            return;
        }
        let next_row_len = self.line_length(row + 1);
        self.cursor = self.line_start(row + 1) + col.min(next_row_len);
    }

    pub fn extend_up(&mut self) {
        let anchor = self.selection_anchor.unwrap_or(self.cursor);
        let (row, col) = self.cursor_row_col();
        if row == 0 {
            self.selection_anchor = Some(anchor);
            return;
        }
        let prev_row_len = self.line_length(row - 1);
        self.cursor = self.line_start(row - 1) + col.min(prev_row_len);
        self.selection_anchor = Some(anchor);
    }

    pub fn extend_down(&mut self) {
        let anchor = self.selection_anchor.unwrap_or(self.cursor);
        let (row, col) = self.cursor_row_col();
        let total_rows = self.text.split('\n').count();
        if row + 1 >= total_rows {
            self.selection_anchor = Some(anchor);
            return;
        }
        let next_row_len = self.line_length(row + 1);
        self.cursor = self.line_start(row + 1) + col.min(next_row_len);
        self.selection_anchor = Some(anchor);
    }

    pub fn move_word_left(&mut self) {
        if let Some((start, _end)) = self.selection_range() {
            self.cursor = start;
            self.selection_anchor = None;
        }
        self.cursor = text_bounds::previous_word_start(&self.text, self.cursor);
    }

    pub fn move_word_right(&mut self) {
        if let Some((_start, end)) = self.selection_range() {
            self.cursor = end;
            self.selection_anchor = None;
        }
        self.cursor = text_bounds::next_word_start(&self.text, self.cursor);
    }

    pub fn delete_word_back(&mut self) {
        let before = self.edit_state();
        if self.has_selection() {
            self.replace_selection("");
            self.record_edit(before);
            return;
        }
        let start = text_bounds::previous_word_start(&self.text, self.cursor);
        if start < self.cursor {
            self.remove_chips_in(start, self.cursor);
            let byte_start = self.byte_index_for_text_index(start);
            let byte_end = self.byte_index_for_text_index(self.cursor);
            self.text.drain(byte_start..byte_end);
            self.cursor = start;
        }
        self.record_edit(before);
    }

    pub fn delete_to_line_start(&mut self) {
        let before = self.edit_state();
        if self.has_selection() {
            self.replace_selection("");
            self.record_edit(before);
            return;
        }
        let (row, _) = self.cursor_row_col();
        let start = self.line_start(row);
        if start < self.cursor {
            self.remove_chips_in(start, self.cursor);
            let byte_start = self.byte_index_for_text_index(start);
            let byte_end = self.byte_index_for_text_index(self.cursor);
            self.text.drain(byte_start..byte_end);
            self.cursor = start;
        }
        self.record_edit(before);
    }

    pub fn delete_to_line_end(&mut self) {
        let before = self.edit_state();
        if self.has_selection() {
            self.replace_selection("");
            self.record_edit(before);
            return;
        }
        let (row, _) = self.cursor_row_col();
        let end = self.line_start(row) + self.line_length(row);
        if end > self.cursor {
            self.remove_chips_in(self.cursor, end);
            let byte_start = self.byte_index_for_text_index(self.cursor);
            let byte_end = self.byte_index_for_text_index(end);
            self.text.drain(byte_start..byte_end);
        }
        self.record_edit(before);
    }

    pub fn move_to_start(&mut self) {
        self.selection_anchor = None;
        let (row, _) = self.cursor_row_col();
        self.cursor = self.line_start(row);
    }

    pub fn move_to_end(&mut self) {
        self.selection_anchor = None;
        let (row, _) = self.cursor_row_col();
        self.cursor = self.line_start(row) + self.line_length(row);
    }

    pub fn extend_to_start(&mut self) {
        let anchor = self.selection_anchor.unwrap_or(self.cursor);
        let (row, _) = self.cursor_row_col();
        self.cursor = self.line_start(row);
        self.selection_anchor = Some(anchor);
    }

    pub fn extend_to_end(&mut self) {
        let anchor = self.selection_anchor.unwrap_or(self.cursor);
        let (row, _) = self.cursor_row_col();
        self.cursor = self.line_start(row) + self.line_length(row);
        self.selection_anchor = Some(anchor);
    }

    fn line_start(&self, row: usize) -> usize {
        if row == 0 {
            return 0;
        }
        text_bounds::line_start(&self.text, row)
    }

    fn line_length(&self, row: usize) -> usize {
        text_bounds::line_length(&self.text, row)
    }

    fn cursor_row_col(&self) -> (usize, usize) {
        self.cursor_row_col_at(self.cursor)
    }

    fn cursor_row_col_at(&self, char_index: usize) -> (usize, usize) {
        text_bounds::row_col_at(&self.text, char_index)
    }

    fn byte_index_for_cursor(&self) -> usize {
        self.byte_index_for_text_index(self.cursor)
    }

    fn byte_index_for_text_index(&self, index: usize) -> usize {
        text_bounds::byte_index_for_grapheme_index(&self.text, index)
    }

    fn previous_boundary(&self, char_index: usize) -> usize {
        char_index.saturating_sub(1)
    }

    fn next_boundary(&self, char_index: usize) -> usize {
        let total = text_bounds::grapheme_count(&self.text);
        if char_index + 1 > total {
            total
        } else {
            char_index + 1
        }
    }

    pub fn push_history(&mut self, entry: String) {
        if entry.trim().is_empty() {
            return;
        }
        if self.history.last().is_some_and(|last| last == &entry) {
            self.reset_navigation();
            return;
        }
        self.history.push(entry);
        if self.history.len() > INPUT_HISTORY_LIMIT {
            let drop = self.history.len() - INPUT_HISTORY_LIMIT;
            self.history.drain(0..drop);
        }
        self.reset_navigation();
    }

    pub fn reset_navigation(&mut self) {
        self.history_cursor = None;
        self.draft = None;
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_cursor {
            None => {
                if self.draft.is_none() {
                    self.draft = Some(ComposerDraft {
                        state: self.edit_state(),
                        undo: std::mem::take(&mut self.undo),
                        redo: std::mem::take(&mut self.redo),
                    });
                }
                let last = self.history.len() - 1;
                self.history_cursor = Some(last);
                self.set_text(self.history[last].clone());
            }
            Some(0) => {}
            Some(index) => {
                let next = index - 1;
                self.history_cursor = Some(next);
                self.set_text(self.history[next].clone());
            }
        }
    }

    pub fn history_next(&mut self) {
        match self.history_cursor {
            None => {}
            Some(index) => {
                if index + 1 >= self.history.len() {
                    self.history_cursor = None;
                    let draft = self.draft.take().unwrap_or_default();
                    self.restore_edit_state(draft.state);
                    self.undo = draft.undo;
                    self.redo = draft.redo;
                } else {
                    let next = index + 1;
                    self.history_cursor = Some(next);
                    let entry = self.history[next].clone();
                    self.set_text(entry);
                }
            }
        }
    }

    /// Move the cursor by a signed char count, extending the selection so
    /// the caller's anchor sticks. Saturates to the text bounds. The run
    /// loop computes the char count from a page delta + live body size
    /// (`pending_composer_extend` resolution).
    pub fn extend_by_chars(&mut self, char_delta: i32) {
        if char_delta == 0 {
            return;
        }
        let anchor = self.selection_anchor.unwrap_or(self.cursor);
        let total_chars = text_bounds::grapheme_count(&self.text) as i32;
        let target = if char_delta < 0 {
            self.cursor
                .saturating_sub(char_delta.unsigned_abs() as usize)
        } else {
            (self.cursor as i32 + char_delta).clamp(0, total_chars) as usize
        };
        self.cursor = target;
        self.selection_anchor = Some(anchor);
    }

    // --- Paste chips -----------------------------------------------------

    /// Snapshot the editable state (buffer + chips) for stashing a draft or a
    /// withdrawn queued message.
    pub fn content(&self) -> ComposerContent {
        ComposerContent {
            text: self.text.clone(),
            chips: self.chips.clone(),
        }
    }

    /// Restore a previously-snapshotted [`ComposerContent`], re-syncing the
    /// label counters so a subsequent paste can't reuse a live chip number.
    pub fn restore_content(&mut self, content: ComposerContent) {
        self.next_text_seq = 0;
        self.next_image_seq = 0;
        self.cursor = text_bounds::grapheme_count(&content.text);
        self.text = content.text;
        for chip in &content.chips {
            let number = label_number(&chip.label);
            match chip.payload {
                ChipPayload::Text(_) => self.next_text_seq = self.next_text_seq.max(number),
                ChipPayload::Image { .. } => self.next_image_seq = self.next_image_seq.max(number),
            }
        }
        self.chips = content.chips;
        self.selection_anchor = None;
        self.clear_edit_history();
    }

    /// Clear the buffer, chips, and per-draft label counters. Used on submit,
    /// queue, and explicit clear — the three places a draft ends.
    pub fn clear_content(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.chips.clear();
        self.selection_anchor = None;
        self.next_text_seq = 0;
        self.next_image_seq = 0;
        self.clear_edit_history();
    }

    /// Collapse a long text paste into a `[Text N: M lines]` chip. Returns
    /// `false` (inserting nothing) if the payload exceeds the hard paste limit,
    /// so the caller can surface a notice rather than silently dropping it.
    pub fn insert_text_chip(&mut self, payload: &str) -> bool {
        let normalized = normalize_paste(payload);
        if text_bounds::grapheme_count(&normalized) > PASTE_HARD_LIMIT {
            return false;
        }
        let before = self.edit_state();
        self.next_text_seq += 1;
        let lines = normalized.lines().count().max(1);
        let label = if lines == 1 {
            format!("[Text {}: 1 line]", self.next_text_seq)
        } else {
            format!("[Text {}: {lines} lines]", self.next_text_seq)
        };
        self.insert_chip(ComposerChip {
            label,
            payload: ChipPayload::Text(normalized),
        });
        self.record_edit(before);
        true
    }

    /// Insert an `[Image N]` chip for an already-encoded pasted image.
    pub fn insert_image_chip(&mut self, image: EncodedImage) {
        let before = self.edit_state();
        self.next_image_seq += 1;
        let label = format!("[Image {}]", self.next_image_seq);
        self.insert_chip(ComposerChip {
            label,
            payload: ChipPayload::Image {
                mime: ENCODED_IMAGE_MIME.to_string(),
                base64: image.base64,
                byte_len: image.byte_len,
                width: image.width,
                height: image.height,
            },
        });
        self.record_edit(before);
    }

    fn insert_chip(&mut self, chip: ComposerChip) {
        if self.has_selection() {
            self.replace_selection("");
        }
        let byte = self.byte_index_for_cursor();
        // The chip's index in `chips` is the number of sentinels before it.
        let index = self.text[..byte].matches(CHIP_SENTINEL).count();
        let mut sentinel_buf = [0u8; 4];
        let sentinel = CHIP_SENTINEL.encode_utf8(&mut sentinel_buf);
        self.text.insert_str(byte, sentinel);
        self.chips.insert(index, chip);
        self.cursor =
            text_bounds::grapheme_index_after_byte(&self.text, byte + CHIP_SENTINEL.len_utf8());
        self.selection_anchor = None;
    }

    /// Replace the whole buffer with a completion result, keeping chips when the
    /// replacement preserves their sentinels. The completion machinery only
    /// appends/rewrites the trailing word token, so sentinels normally survive;
    /// a replacement that drops them (e.g. a slash-command rewrite) drops the
    /// now-dangling chips to keep the sentinel↔chips mapping consistent.
    pub fn replace_text_keep_chips(&mut self, replacement: String) {
        let before = self.edit_state();
        let sentinel_count = replacement.matches(CHIP_SENTINEL).count();
        self.cursor = text_bounds::grapheme_count(&replacement);
        self.text = replacement;
        if sentinel_count != self.chips.len() {
            self.chips.truncate(sentinel_count.min(self.chips.len()));
        }
        self.selection_anchor = None;
        self.record_edit(before);
    }

    /// Restore the state before the most recent composer edit.
    pub fn undo(&mut self) {
        let Some(previous) = self.undo.pop() else {
            return;
        };
        let current = self.edit_state();
        push_bounded(&mut self.redo, current);
        self.restore_edit_state(previous);
    }

    /// Reapply the most recently undone composer edit.
    pub fn redo(&mut self) {
        let Some(next) = self.redo.pop() else {
            return;
        };
        let current = self.edit_state();
        push_bounded(&mut self.undo, current);
        self.restore_edit_state(next);
    }

    fn edit_state(&self) -> ComposerEditState {
        ComposerEditState {
            content: self.content(),
            cursor: self.cursor,
            selection_anchor: self.selection_anchor,
            next_text_seq: self.next_text_seq,
            next_image_seq: self.next_image_seq,
        }
    }

    fn restore_edit_state(&mut self, state: ComposerEditState) {
        self.text = state.content.text;
        self.chips = state.content.chips;
        self.cursor = state.cursor;
        self.selection_anchor = state.selection_anchor;
        self.next_text_seq = state.next_text_seq;
        self.next_image_seq = state.next_image_seq;
    }

    fn record_edit(&mut self, before: ComposerEditState) {
        if self.edit_state() == before {
            return;
        }
        push_bounded(&mut self.undo, before);
        self.redo.clear();
    }

    fn clear_edit_history(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    /// Expand chip sentinels to their labels for rendering, returning the
    /// display string plus the index map back to buffer positions. The common
    /// no-chip path returns the buffer verbatim with identity mapping.
    pub fn display(&self) -> ChipDisplay {
        if self.chips.is_empty() {
            return ChipDisplay {
                text: self.text.clone(),
                chip_ranges: Vec::new(),
                sentinels: Vec::new(),
            };
        }

        let mut text = String::with_capacity(self.text.len());
        let mut chip_ranges = Vec::with_capacity(self.chips.len());
        let mut sentinels = Vec::with_capacity(self.chips.len());
        let mut chips = self.chips.iter();
        let mut display_index = 0;

        for (buffer_index, grapheme) in self.text.graphemes(true).enumerate() {
            if grapheme.contains(CHIP_SENTINEL) {
                if let Some(chip) = chips.next() {
                    // Labels are ASCII, so char count == grapheme count == columns.
                    let label_len = chip.label.chars().count();
                    text.push_str(&chip.label);
                    chip_ranges.push((display_index, display_index + label_len));
                    sentinels.push(ChipSentinel {
                        buffer_index,
                        display_start: display_index,
                        label_len,
                    });
                    display_index += label_len;
                }
            } else {
                text.push_str(grapheme);
                display_index += 1;
            }
        }

        ChipDisplay {
            text,
            chip_ranges,
            sentinels,
        }
    }

    /// Produce the submit payload: the placeholder text the user saw (for the
    /// transcript and history) and the expanded model input. Text chips splice
    /// their payload inside `<pasted-text-N>` tags; image chips keep their
    /// `[Image N]` label in the text and contribute an [`ImageAttachment`].
    pub fn submission(&self) -> ComposerSubmission {
        expand_submission(&self.text, &self.chips)
    }
}

fn push_bounded(stack: &mut Vec<ComposerEditState>, state: ComposerEditState) {
    if stack.len() == EDIT_HISTORY_LIMIT {
        stack.remove(0);
    }
    stack.push(state);
}

impl ComposerContent {
    /// Expand a stored snapshot to its submit payload, used by the re-run path
    /// for a queued message that outlived its agent run.
    pub fn submission(&self) -> ComposerSubmission {
        expand_submission(&self.text, &self.chips)
    }
}

/// Expand buffer text + chips into the placeholder display text and the model
/// input. Shared by [`Composer::submission`] and [`ComposerContent::submission`].
fn expand_submission(text: &str, chips: &[ComposerChip]) -> ComposerSubmission {
    if chips.is_empty() {
        return ComposerSubmission {
            display_text: text.to_string(),
            transcript_text: text.to_string(),
            input: UserInput::from_text(text),
        };
    }

    let mut display_text = String::with_capacity(text.len());
    let mut transcript_text = String::with_capacity(text.len());
    let mut model_text = String::with_capacity(text.len());
    let mut images = Vec::new();
    let mut chips = chips.iter();

    for ch in text.chars() {
        if ch != CHIP_SENTINEL {
            display_text.push(ch);
            transcript_text.push(ch);
            model_text.push(ch);
            continue;
        }
        let Some(chip) = chips.next() else { continue };
        display_text.push_str(&chip.label);
        match &chip.payload {
            ChipPayload::Text(payload) => {
                let n = label_number(&chip.label);
                // The chat shows the paste itself; only the model gets the
                // tagged frame (it correlates the tag with follow-up refs).
                transcript_text.push_str(payload);
                model_text.push_str(&format!("<pasted-text-{n}>\n{payload}\n</pasted-text-{n}>"));
            }
            ChipPayload::Image {
                mime,
                base64,
                byte_len,
                ..
            } => {
                transcript_text.push_str(&chip.label);
                model_text.push_str(&chip.label);
                images.push(ImageAttachment {
                    mime: mime.clone(),
                    base64: base64.clone(),
                    byte_len: *byte_len,
                });
            }
        }
    }

    ComposerSubmission {
        display_text,
        transcript_text,
        input: UserInput {
            text: model_text,
            images,
        },
    }
}

/// Parse the chip number from a label — the first run of digits, so both the
/// bare `[Text 3]` form (older snapshots) and `[Text 3: 56 lines]` yield `3`.
fn label_number(label: &str) -> u32 {
    let digits: String = label
        .chars()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(0)
}

fn normalize_paste(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    // Strip the chip sentinel so pasted content can never inject a phantom chip
    // (which would desync the sentinel↔chips mapping).
    if normalized.contains(CHIP_SENTINEL) {
        normalized.replace(CHIP_SENTINEL, "")
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::super::AppState;
    use crate::tui::event::{AppAction, Focus};
    use crate::tui::transcript::SelectionKind;

    fn app() -> AppState {
        AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        )
    }

    #[test]
    fn click_then_type_does_not_eat_the_first_char() {
        // A mouse click on the empty composer leaves a zero-width selection
        // anchor (anchor == cursor), which `has_selection` correctly treats as
        // no selection. Inserting the first char must collapse that anchor —
        // otherwise advancing the cursor past it promotes it to a real one-char
        // selection and the second keystroke overwrites the first. This is the
        // reproduced "typing / then b eats the /" bug (live session 35).
        let mut app = app();
        app.focus = Focus::Input;
        app.composer.extend_selection_to(0); // click at offset 0 on empty input
        assert!(!app.composer.has_selection());

        app.reduce(AppAction::InputChar('/'));
        assert!(
            !app.composer.has_selection(),
            "insert must collapse the stale click anchor"
        );
        app.reduce(AppAction::InputChar('b'));

        assert_eq!(app.composer.text, "/b");
        assert_eq!(app.input(), "/b");
    }

    #[test]
    fn composer_insert_newline_appends_lf() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::InputChar('a'));
        app.reduce(AppAction::InsertNewline);
        app.reduce(AppAction::InputChar('b'));

        assert_eq!(app.composer.text, "a\nb");
        assert_eq!(app.input(), "a\nb");
    }

    #[test]
    fn composer_history_navigation_preserves_draft() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::SubmitInput("first".to_string()));
        app.reduce(AppAction::SubmitInput("second".to_string()));
        app.reduce(AppAction::InputChar('d'));
        app.reduce(AppAction::HistoryPrev);
        assert_eq!(app.composer.text, "second");
        app.reduce(AppAction::HistoryPrev);
        assert_eq!(app.composer.text, "first");
        app.reduce(AppAction::HistoryPrev);
        assert_eq!(app.composer.text, "first");
        app.reduce(AppAction::HistoryNext);
        app.reduce(AppAction::HistoryNext);
        assert_eq!(app.composer.text, "d");
    }

    #[test]
    fn composer_paste_normalizes_crlf() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::PasteText("line1\r\nline2\rline3".to_string()));
        assert_eq!(app.composer.text, "line1\nline2\nline3");
    }

    #[test]
    fn composer_short_paste_inserts_inline() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::PasteText("just a short paste".to_string()));
        assert_eq!(app.composer.text, "just a short paste");
        assert!(app.composer.chips.is_empty());
    }

    #[test]
    fn composer_long_paste_collapses_to_chip() {
        let mut app = app();
        app.focus = Focus::Input;
        let long = "line\n".repeat(10);
        app.reduce(AppAction::PasteText(long.clone()));
        assert_eq!(app.composer.chips.len(), 1);
        assert_eq!(app.composer.text, super::CHIP_SENTINEL.to_string());
        // The full payload survives for the model.
        let ChipPayload::Text(payload) = &app.composer.chips[0].payload else {
            panic!("expected a text chip");
        };
        assert_eq!(payload, &long);
    }

    #[test]
    fn composer_over_threshold_paste_with_secret_is_refused() {
        let mut app = app();
        app.focus = Focus::Input;
        // Over the line threshold AND contains a live-looking AWS key: the
        // secret gate must win, creating no chip and inserting nothing.
        let payload = "a\nb\nc\nd\nAKIAIOSFODNN7EXAMPLE\n".to_string();
        app.reduce(AppAction::PasteText(payload));
        assert!(app.composer.chips.is_empty());
        assert_eq!(app.composer.text, "");
    }

    #[test]
    fn composer_paste_starting_with_combining_mark_positions_cursor() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::InputChar('a'));
        // A pasted combining acute accent merges with the preceding 'a' into a
        // single grapheme "á"; the caret must land after that one grapheme, not
        // overshoot by the pasted text's grapheme count.
        app.reduce(AppAction::PasteText("\u{301}".to_string()));
        assert_eq!(app.composer.text, "a\u{301}");
        assert_eq!(app.composer.cursor, 1);
    }

    #[test]
    fn composer_backspace_does_not_affect_history_draft() {
        let mut app = app();
        app.focus = Focus::Input;
        app.reduce(AppAction::SubmitInput("hello".to_string()));
        app.reduce(AppAction::HistoryPrev);
        assert_eq!(app.composer.text, "hello");
        app.reduce(AppAction::Backspace);
        assert_eq!(app.composer.text, "hell");
        app.reduce(AppAction::Backspace);
        app.reduce(AppAction::Backspace);
        app.reduce(AppAction::Backspace);
        app.reduce(AppAction::Backspace);
        assert_eq!(app.composer.text, "");
        app.reduce(AppAction::HistoryNext);
        assert_eq!(app.composer.text, "");
    }

    #[test]
    fn composer_cursor_moves_left_right_and_up_down() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abc\ndef".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        assert_eq!(app.composer.cursor, 7);
        app.reduce(AppAction::CursorUp);
        assert_eq!(app.composer.cursor, 3);
        app.reduce(AppAction::CursorLeft);
        assert_eq!(app.composer.cursor, 2);
        app.reduce(AppAction::CursorRight);
        assert_eq!(app.composer.cursor, 3);
        app.reduce(AppAction::CursorEnd);
        assert_eq!(app.composer.cursor, 3);
        app.reduce(AppAction::CursorStart);
        assert_eq!(app.composer.cursor, 0);
    }

    #[test]
    fn reducer_moves_cursor_vertically_across_multiline_composer_content() {
        let mut app = app();
        app.focus = Focus::Input;
        app.composer.set_text("first\nsecond\nthird".to_string());
        app.composer.cursor = 0;

        app.reduce(AppAction::CursorDown);
        assert_eq!(app.composer.cursor, 6);
        app.reduce(AppAction::CursorUp);
        assert_eq!(app.composer.cursor, 0, "cursor clamps at the first row");

        app.composer.cursor = 16;
        app.reduce(AppAction::CursorUp);
        assert_eq!(app.composer.cursor, 9);
        app.reduce(AppAction::CursorUp);
        assert_eq!(app.composer.cursor, 3);
        app.reduce(AppAction::CursorDown);
        assert_eq!(app.composer.cursor, 9);
        app.reduce(AppAction::CursorDown);
        assert_eq!(app.composer.cursor, 16);

        app.composer.cursor = app.composer.text.chars().count();
        app.reduce(AppAction::CursorDown);
        assert_eq!(app.composer.cursor, 18, "cursor clamps at the final row");
    }

    #[test]
    fn composer_insert_at_cursor_offsets_existing_text() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abc".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        for _ in 0..3 {
            app.reduce(AppAction::CursorLeft);
        }
        app.reduce(AppAction::InputChar('X'));
        assert_eq!(app.composer.text, "Xabc");
        assert_eq!(app.composer.cursor, 1);
    }

    #[test]
    fn composer_delete_forward_removes_next_char() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abc".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::CursorStart);
        app.reduce(AppAction::DeleteForward);
        assert_eq!(app.composer.text, "bc");
        assert_eq!(app.composer.cursor, 0);
    }

    #[test]
    fn composer_insert_replaces_selection() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "hello world".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::CursorStart);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::ExtendCursorRight);
        assert!(app.composer.has_selection());
        assert_eq!(app.composer.selected_text().as_deref(), Some("hello"));
        app.reduce(AppAction::InputChar('H'));
        assert_eq!(app.composer.text, "H world");
        assert_eq!(app.composer.cursor, 1);
        assert!(!app.composer.has_selection());
    }

    #[test]
    fn composer_backspace_removes_selection() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abc".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::CursorStart);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::Backspace);
        assert_eq!(app.composer.text, "c");
        assert_eq!(app.composer.cursor, 0);
    }

    #[test]
    fn composer_delete_forward_removes_selection() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abc".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::CursorStart);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::DeleteForward);
        assert_eq!(app.composer.text, "c");
        assert_eq!(app.composer.cursor, 0);
    }

    #[test]
    fn composer_move_left_collapses_selection_to_start() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abc".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::CursorStart);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::ExtendCursorRight);
        assert!(app.composer.has_selection());
        app.reduce(AppAction::CursorLeft);
        assert!(!app.composer.has_selection());
        assert_eq!(app.composer.cursor, 0);
    }

    #[test]
    fn composer_move_right_collapses_selection_to_end() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abc".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::CursorStart);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::ExtendCursorRight);
        app.reduce(AppAction::CursorRight);
        assert!(!app.composer.has_selection());
        assert_eq!(app.composer.cursor, 2);
    }

    #[test]
    fn composer_select_all_then_typing_replaces() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abc".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::ComposerSelectAll);
        assert!(app.composer.has_selection());
        assert_eq!(app.composer.selected_text().as_deref(), Some("abc"));
        app.reduce(AppAction::InputChar('Z'));
        assert_eq!(app.composer.text, "Z");
        assert_eq!(app.composer.cursor, 1);
    }

    #[test]
    fn composer_click_position_sets_cursor_and_clears_selection() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "hello".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::ComposerSelectAll);
        app.reduce(AppAction::ComposerClick {
            char_index: 2,
            kind: SelectionKind::Position,
            column: 0,
            row: 0,
        });
        assert_eq!(app.composer.cursor, 2);
        assert!(!app.composer.has_selection());
    }

    #[test]
    fn composer_click_word_selects_word() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "hello world".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::ComposerClick {
            char_index: 7,
            kind: SelectionKind::Word,
            column: 0,
            row: 0,
        });
        assert_eq!(app.composer.selected_text().as_deref(), Some("world"));
    }

    #[test]
    fn composer_inserting_combining_mark_keeps_cursor_on_grapheme_boundary() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "e\u{301}".chars() {
            app.reduce(AppAction::InputChar(ch));
        }

        assert_eq!(app.composer.text, "e\u{301}");
        assert_eq!(app.composer.cursor, 1);

        app.reduce(AppAction::Backspace);
        assert_eq!(app.composer.text, "");
        assert_eq!(app.composer.cursor, 0);
    }

    #[test]
    fn composer_click_line_selects_line() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abc\ndef\nghi".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::ComposerClick {
            char_index: 6,
            kind: SelectionKind::Line,
            column: 0,
            row: 0,
        });
        assert_eq!(app.composer.selected_text().as_deref(), Some("def"));
    }

    #[test]
    fn composer_drag_extends_selection() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "hello world".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::CursorStart);
        app.reduce(AppAction::ComposerDrag { char_index: 5 });
        assert_eq!(app.composer.selected_text().as_deref(), Some("hello"));
    }

    #[test]
    fn composer_word_movement_and_deletion() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "alpha beta gamma".chars() {
            app.reduce(AppAction::InputChar(ch));
        }

        app.reduce(AppAction::CursorWordLeft);
        assert_eq!(app.composer.cursor, 11, "cursor lands at start of 'gamma'");
        app.reduce(AppAction::CursorWordLeft);
        assert_eq!(app.composer.cursor, 6, "cursor lands at start of 'beta'");
        app.reduce(AppAction::CursorWordRight);
        assert_eq!(app.composer.cursor, 11, "cursor lands at start of 'gamma'");

        app.reduce(AppAction::DeleteWordBack);
        assert_eq!(app.composer.text, "alpha gamma");
        assert_eq!(app.composer.cursor, 6);
    }

    #[test]
    fn composer_delete_to_line_start_and_end() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abc def".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        for _ in 0..3 {
            app.reduce(AppAction::CursorLeft);
        }

        app.reduce(AppAction::DeleteToLineEnd);
        assert_eq!(app.composer.text, "abc ");

        app.reduce(AppAction::DeleteToLineStart);
        assert_eq!(app.composer.text, "");
        assert_eq!(app.composer.cursor, 0);
    }

    // --- Paste chips -----------------------------------------------------

    use super::{ChipPayload, Composer, ComposerContent};
    use crate::tui::image_paste::EncodedImage;

    fn sample_image() -> EncodedImage {
        EncodedImage {
            base64: "QUJD".to_string(),
            byte_len: 3,
            width: 2,
            height: 2,
        }
    }

    #[test]
    fn text_chip_collapses_to_single_sentinel() {
        let mut c = Composer::default();
        assert!(c.insert_text_chip("a really long pasted block"));
        assert_eq!(c.text.chars().count(), 1);
        assert_eq!(c.text, super::CHIP_SENTINEL.to_string());
        assert_eq!(c.chips.len(), 1);
        assert_eq!(c.cursor, 1);
        assert_eq!(c.chips[0].label, "[Text 1: 1 line]");
    }

    #[test]
    fn backspace_removes_whole_chip_and_payload() {
        let mut c = Composer::default();
        c.insert_text_chip("payload");
        assert_eq!(c.chips.len(), 1);
        c.backspace();
        assert_eq!(c.text, "");
        assert!(c.chips.is_empty());
        assert_eq!(c.cursor, 0);
    }

    #[test]
    fn delete_forward_removes_chip_from_the_left() {
        let mut c = Composer::default();
        c.insert_text_chip("payload");
        c.move_to(0);
        c.delete_forward();
        assert_eq!(c.text, "");
        assert!(c.chips.is_empty());
    }

    #[test]
    fn selection_delete_removes_the_correct_chip_payload() {
        // Buffer: [Text 1] <space> [Text 2]. Deleting the first chip must drop
        // the "first" payload and leave "second" — proving remove_chips_in maps
        // ranges to the right chip.
        let mut c = Composer::default();
        c.insert_text_chip("first");
        c.insert_char(' ');
        c.insert_text_chip("second");
        assert_eq!(c.chips.len(), 2);

        c.set_selection(0, 1); // select the first sentinel
        c.backspace();

        assert_eq!(c.chips.len(), 1);
        let ChipPayload::Text(remaining) = &c.chips[0].payload else {
            panic!("expected a text chip");
        };
        assert_eq!(remaining, "second");
        assert_eq!(c.chips[0].label, "[Text 2: 1 line]");
    }

    #[test]
    fn select_all_then_type_clears_chips() {
        let mut c = Composer::default();
        c.insert_text_chip("payload");
        c.insert_char('x');
        c.select_all();
        c.insert_char('z');
        assert_eq!(c.text, "z");
        assert!(c.chips.is_empty());
    }

    #[test]
    fn label_numbering_is_monotonic_across_deletes() {
        let mut c = Composer::default();
        c.insert_text_chip("one");
        assert_eq!(c.chips[0].label, "[Text 1: 1 line]");
        c.backspace(); // delete [Text 1]
        c.insert_text_chip("two\nlines");
        // No renumbering: the next paste is [Text 2], not [Text 1] again.
        assert_eq!(c.chips[0].label, "[Text 2: 2 lines]");
    }

    #[test]
    fn submission_expands_text_chip_in_tags() {
        let mut c = Composer::default();
        c.insert_char('h');
        c.insert_char('i');
        c.insert_char(' ');
        c.insert_text_chip("line one\nline two");

        let sub = c.submission();
        assert_eq!(sub.display_text, "hi [Text 1: 2 lines]");
        // The transcript form carries the paste itself, not the placeholder.
        assert_eq!(sub.transcript_text, "hi line one\nline two");
        assert_eq!(
            sub.input.text,
            "hi <pasted-text-1>\nline one\nline two\n</pasted-text-1>"
        );
        assert!(sub.input.images.is_empty());
    }

    #[test]
    fn submission_keeps_image_label_and_collects_attachment() {
        let mut c = Composer::default();
        c.insert_char('s');
        c.insert_char('e');
        c.insert_char('e');
        c.insert_char(' ');
        c.insert_image_chip(sample_image());

        let sub = c.submission();
        assert_eq!(sub.display_text, "see [Image 1]");
        // Image chips stay collapsed everywhere; only text chips expand.
        assert_eq!(sub.transcript_text, "see [Image 1]");
        // The model text keeps the label so it can correlate with the part.
        assert_eq!(sub.input.text, "see [Image 1]");
        assert_eq!(sub.input.images.len(), 1);
        assert_eq!(sub.input.images[0].mime, "image/png");
        assert_eq!(sub.input.images[0].base64, "QUJD");
    }

    #[test]
    fn submission_without_chips_is_identity() {
        let mut c = Composer::default();
        for ch in "plain text".chars() {
            c.insert_char(ch);
        }
        let sub = c.submission();
        assert_eq!(sub.display_text, "plain text");
        assert_eq!(sub.transcript_text, "plain text");
        assert_eq!(sub.input.text, "plain text");
        assert!(sub.input.images.is_empty());
    }

    #[test]
    fn history_recall_drops_chips() {
        let mut c = Composer::default();
        c.insert_text_chip("payload");
        c.push_history("[Text 1]".to_string());
        // Recalling the stored display text must not resurrect a chip.
        c.set_text("prior entry".to_string());
        assert!(c.chips.is_empty());
        // A literal sentinel in a recalled entry is stripped defensively.
        c.set_text(format!("x{}y", super::CHIP_SENTINEL));
        assert_eq!(c.text, "xy");
        assert!(c.chips.is_empty());
    }

    #[test]
    fn draft_round_trips_chips_through_history_navigation() {
        let mut c = Composer::default();
        c.history.push("earlier".to_string());
        c.insert_text_chip("draft payload");
        assert_eq!(c.chips.len(), 1);

        c.history_prev(); // stash the chip draft, show "earlier"
        assert_eq!(c.text, "earlier");
        assert!(c.chips.is_empty());

        c.history_next(); // restore the draft
        assert_eq!(c.chips.len(), 1);
        let ChipPayload::Text(payload) = &c.chips[0].payload else {
            panic!("expected a text chip");
        };
        assert_eq!(payload, "draft payload");
    }

    #[test]
    fn undo_redo_restores_chip_payload_cursor_and_selection() {
        let mut c = Composer::default();
        c.insert_text_chip("payload");
        c.insert_char('x');
        c.set_selection(0, 1);

        c.backspace();
        assert_eq!(c.text, "x");
        assert!(c.chips.is_empty());

        c.undo();
        assert_eq!(c.text, format!("{}x", super::CHIP_SENTINEL));
        assert_eq!(c.cursor, 1);
        assert_eq!(c.selection_anchor, Some(0));
        let ChipPayload::Text(payload) = &c.chips[0].payload else {
            panic!("expected a text chip");
        };
        assert_eq!(payload, "payload");

        c.redo();
        assert_eq!(c.text, "x");
        assert_eq!(c.cursor, 0);
        assert!(c.selection_anchor.is_none());
        assert!(c.chips.is_empty());
    }

    #[test]
    fn history_navigation_restores_draft_undo_and_redo_stacks() {
        let mut c = Composer::default();
        c.history.push("earlier".to_string());
        c.insert_char('a');
        c.insert_char('b');
        c.undo();
        assert_eq!(c.text, "a");

        c.history_prev();
        assert_eq!(c.text, "earlier");
        c.history_next();
        assert_eq!(c.text, "a");

        c.redo();
        assert_eq!(c.text, "ab");
        c.undo();
        assert_eq!(c.text, "a");
    }

    #[test]
    fn editing_after_undo_discards_redo_branch() {
        let mut c = Composer::default();
        c.insert_char('a');
        c.insert_char('b');
        c.undo();
        c.insert_char('c');

        c.redo();
        assert_eq!(c.text, "ac");
    }

    #[test]
    fn withdrawn_chip_draft_starts_fresh_undo_history() {
        use crate::agent::AgentMode;

        let mut app = app();
        app.focus = Focus::Input;
        let mut queued = Composer::default();
        queued.insert_text_chip("queued payload");
        let content = queued.content();
        app.reduce(AppAction::QueueInput {
            id: 7,
            text: "[Text 1]".to_string(),
            content,
            mode: AgentMode::Coding,
        });

        app.reduce(AppAction::WithdrawQueuedInput);
        assert_eq!(app.composer.chips.len(), 1);
        app.reduce(AppAction::InputChar('!'));
        app.reduce(AppAction::ComposerUndo);
        assert_eq!(app.composer.text, super::CHIP_SENTINEL.to_string());
        assert_eq!(app.composer.chips.len(), 1);
        app.reduce(AppAction::ComposerRedo);
        assert_eq!(app.composer.text, format!("{}!", super::CHIP_SENTINEL));
    }

    #[test]
    fn restore_content_bumps_seq_past_existing_labels() {
        let mut c = Composer::default();
        // Simulate a withdrawn queued message whose chip was [Text 3].
        let content = ComposerContent {
            text: super::CHIP_SENTINEL.to_string(),
            chips: vec![super::ComposerChip {
                label: "[Text 3]".to_string(),
                payload: ChipPayload::Text("restored".to_string()),
            }],
        };
        c.restore_content(content);
        // Next paste must not collide with the restored [Text 3] — the bare
        // label form (an older snapshot) must parse the same as the current
        // `[Text N: M lines]` form.
        c.move_to_end();
        c.insert_text_chip("new");
        assert_eq!(c.chips[1].label, "[Text 4: 1 line]");
    }

    #[test]
    fn replace_text_keep_chips_preserves_matching_sentinels() {
        let mut c = Composer::default();
        c.insert_text_chip("keep me");
        // A completion that keeps the sentinel keeps the chip.
        c.replace_text_keep_chips(format!("{} tail", super::CHIP_SENTINEL));
        assert_eq!(c.chips.len(), 1);
        // A completion that drops the sentinel drops the chip.
        c.replace_text_keep_chips("/command".to_string());
        assert!(c.chips.is_empty());
    }

    #[test]
    fn display_expands_sentinels_to_labels() {
        let mut c = Composer::default();
        c.insert_char('h');
        c.insert_char('i');
        c.insert_char(' ');
        c.insert_text_chip("payload");
        c.insert_char('!');

        let display = c.display();
        assert_eq!(display.text, "hi [Text 1: 1 line]!");
        assert_eq!(display.chip_ranges, vec![(3, 19)]);
    }

    #[test]
    fn display_index_maps_round_trip_outside_labels() {
        let mut c = Composer::default();
        c.insert_char('a');
        c.insert_text_chip("payload"); // buffer index 1
        c.insert_char('b');
        // Buffer: a <sentinel> b  → display: "a[Text 1: 1 line]b"
        let display = c.display();
        assert_eq!(display.text, "a[Text 1: 1 line]b");
        // Buffer 0 ('a') → display 0; buffer 1 (chip start) → display 1;
        // buffer 2 ('b') → display 17 (after the 16-char label).
        assert_eq!(display.to_display(0), 0);
        assert_eq!(display.to_display(1), 1);
        assert_eq!(display.to_display(2), 17);
        assert_eq!(display.to_display(3), 18);
        // Inverse for non-label positions.
        assert_eq!(display.to_buffer(0), 0);
        assert_eq!(display.to_buffer(1), 1);
        assert_eq!(display.to_buffer(17), 2);
        assert_eq!(display.to_buffer(18), 3);
    }

    #[test]
    fn display_click_inside_label_snaps_to_nearer_edge() {
        let mut c = Composer::default();
        // One chip, label "[Text 1: 1 line]" at display 0..16.
        c.insert_text_chip("payload");
        let display = c.display();
        // Left half of the label snaps to the chip start (buffer 0)…
        assert_eq!(display.to_buffer(1), 0);
        assert_eq!(display.to_buffer(7), 0);
        // …right half snaps to the chip end (buffer 1).
        assert_eq!(display.to_buffer(12), 1);
        assert_eq!(display.to_buffer(15), 1);
    }
}
