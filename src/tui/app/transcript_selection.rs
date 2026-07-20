use super::*;
use crate::tui::text_bounds;
use unicode_segmentation::UnicodeSegmentation;

impl AppState {
    pub(super) fn apply_transcript_click(&mut self, position: TranscriptPosition, extend: bool) {
        if !self.is_selectable_item(position.item) {
            self.transcript_selection = None;
            return;
        }
        if extend && let Some(existing) = self.transcript_selection {
            self.transcript_selection = Some(TranscriptSelection {
                anchor: existing.anchor,
                caret: position,
            });
        } else {
            self.transcript_selection = Some(TranscriptSelection {
                anchor: position,
                caret: position,
            });
        }
    }

    pub(super) fn apply_transcript_drag(&mut self, position: TranscriptPosition) {
        if !self.is_selectable_item(position.item) {
            return;
        }
        let anchor = self
            .transcript_selection
            .map(|existing| existing.anchor)
            .unwrap_or(position);
        self.transcript_selection = Some(TranscriptSelection {
            anchor,
            caret: position,
        });
    }

    pub(super) fn is_selectable_item(&self, item_index: usize) -> bool {
        self.transcript.get(item_index).is_some()
    }

    /// Move the caret one rendered line up/down through the transcript,
    /// extending the selection. `delta == 0` is a no-op. Wraps across
    /// item boundaries: stepping past the end of an item lands on the
    /// start of the next (or the end of the previous on PageUp).
    pub(super) fn extend_transcript_cursor(&mut self, delta: i16) {
        if delta == 0 {
            return;
        }
        let caret = self
            .transcript_selection
            .map(|s| s.caret)
            .unwrap_or(TranscriptPosition {
                item: 0,
                grapheme: 0,
                width: self.transcript_layout.selection_width().unwrap_or(80),
            });
        let target = self.advance_transcript_position(caret, delta);
        let anchor = self.transcript_selection.map(|s| s.anchor).unwrap_or(caret);
        self.transcript_selection = Some(TranscriptSelection {
            anchor,
            caret: target,
        });
    }

    /// Move `position` by `delta` rendered lines, wrapping across items.
    /// Tool cards behave like a single line each (B1). Returns the same
    /// position if the transcript is empty. The returned `grapheme` field
    /// is an actual grapheme offset into the item's body text (not an
    /// abstract line index), so downstream consumers in
    /// `item_selection_state` and `transcript_slice_for` can use it
    /// directly.
    fn advance_transcript_position(
        &self,
        position: TranscriptPosition,
        delta: i16,
    ) -> TranscriptPosition {
        if self.transcript.is_empty() || delta == 0 {
            return position;
        }
        let sign = if delta > 0 { 1i32 } else { -1i32 };
        let mut current = position;
        let mut steps = delta.unsigned_abs() as i32;
        while steps > 0 {
            let Some(item) = self.transcript.get(current.item) else {
                break;
            };
            let body = crate::tui::widgets::transcript::selection_text_for(item, current.width);

            // Compute line-start grapheme offsets within the body.
            // lines[0] = 0 (first body line). lines[n] = grapheme offset
            // after the n-th \n. Tool cards (empty body) have one line.
            let line_starts: Vec<usize> = if body.is_empty() {
                vec![0usize]
            } else {
                let mut starts = vec![0usize];
                for (offset, g) in body.graphemes(true).enumerate() {
                    if g == "\n" {
                        starts.push(offset + 1);
                    }
                }
                starts
            };

            // Map current grapheme offset back to a logical line index.
            let cur_line = line_starts
                .iter()
                .rposition(|&s| s <= current.grapheme)
                .unwrap_or(0);

            if sign > 0 {
                let last_line = line_starts.len() - 1;
                if cur_line < last_line {
                    current.grapheme = line_starts[cur_line + 1];
                } else if current.item + 1 < self.transcript.len() {
                    current.item += 1;
                    current.grapheme = 0;
                } else {
                    current.grapheme = body.graphemes(true).count();
                }
            } else if cur_line > 0 {
                current.grapheme = line_starts[cur_line - 1];
            } else if current.item > 0 {
                current.item -= 1;
                let prev_item = &self.transcript[current.item];
                let prev_body =
                    crate::tui::widgets::transcript::selection_text_for(prev_item, current.width);
                let prev_starts: Vec<usize> = if prev_body.is_empty() {
                    vec![0usize]
                } else {
                    let mut starts = vec![0usize];
                    for (offset, g) in prev_body.graphemes(true).enumerate() {
                        if g == "\n" {
                            starts.push(offset + 1);
                        }
                    }
                    starts
                };
                current.grapheme = *prev_starts.last().unwrap_or(&0);
            } else {
                current.grapheme = 0;
                break;
            }
            steps -= 1;
        }
        current
    }

    pub(super) fn select_all_transcript(&mut self) {
        if self.transcript.is_empty() {
            self.transcript_selection = None;
            return;
        }
        let last = self.transcript.len() - 1;
        let width = self.transcript_layout.selection_width().unwrap_or(80);
        let last_graphemes =
            crate::tui::widgets::transcript::selection_text_for(&self.transcript[last], width)
                .graphemes(true)
                .count();
        self.transcript_selection = Some(TranscriptSelection {
            anchor: TranscriptPosition {
                item: 0,
                grapheme: 0,
                width,
            },
            caret: TranscriptPosition {
                item: last,
                grapheme: last_graphemes,
                width,
            },
        });
    }

    pub(super) fn move_transcript_focus(&mut self, delta: i16) {
        if self.transcript.is_empty() || delta == 0 {
            return;
        }
        let Some(current) = self
            .transcript_focus
            .filter(|index| *index < self.transcript.len())
        else {
            self.set_transcript_focus(self.transcript.len() - 1);
            return;
        };
        let max = self.transcript.len() - 1;
        let next = if delta.is_negative() {
            current.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            current.saturating_add(delta as usize)
        }
        .min(max);
        self.set_transcript_focus(next);
    }

    pub(super) fn set_transcript_focus(&mut self, index: usize) {
        self.transcript_focus = Some(index);
        self.transcript_autoscroll = false;
        self.scroll_transcript_focus_into_view = true;
    }

    pub(super) fn apply_transcript_word_select(&mut self, position: TranscriptPosition) {
        let Some(item) = self.transcript.get(position.item) else {
            return;
        };
        let body = crate::tui::widgets::transcript::selection_text_for(item, position.width);
        if body.is_empty() {
            return;
        }
        let total = body.graphemes(true).count();
        let cursor = position.grapheme.min(total);
        let (start, end) = text_bounds::word_bounds_at(&body, cursor);
        self.transcript_selection = Some(TranscriptSelection {
            anchor: TranscriptPosition {
                item: position.item,
                grapheme: start,
                width: position.width,
            },
            caret: TranscriptPosition {
                item: position.item,
                grapheme: end,
                width: position.width,
            },
        });
    }

    pub(super) fn apply_transcript_line_select(&mut self, position: TranscriptPosition) {
        let Some(item) = self.transcript.get(position.item) else {
            return;
        };
        let body = crate::tui::widgets::transcript::selection_text_for(item, position.width);
        if body.is_empty() {
            return;
        }
        let total = body.graphemes(true).count();
        let cursor = position.grapheme.min(total);
        let (start, end) = text_bounds::line_bounds_at(&body, cursor);
        self.transcript_selection = Some(TranscriptSelection {
            anchor: TranscriptPosition {
                item: position.item,
                grapheme: start,
                width: position.width,
            },
            caret: TranscriptPosition {
                item: position.item,
                grapheme: end,
                width: position.width,
            },
        });
    }
}
