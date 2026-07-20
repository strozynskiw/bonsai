use super::message::*;
use super::*;

/// `[start, end)` rows of `item_index`, read from the shared layout cache —
/// the same rows the renderer paints. (The pre-cache version re-rendered every
/// item with neutral options — no selection/focus/permission — whose row
/// counts could drift from the real render when e.g. an `· awaiting
/// permission` suffix pushed a tool line across a wrap boundary; reading the
/// cache makes focus-into-view land exactly.)
pub(crate) fn item_line_span(
    app: &AppState,
    area_width: u16,
    item_index: usize,
) -> Option<(usize, usize)> {
    if item_index >= app.transcript.len() {
        return None;
    }
    let content_width = area_width.saturating_sub(4).max(20) as usize;
    app.transcript_layout
        .item_row_span(app, content_width, item_index)
}

/// Per-item selection state for the renderer. Resolves the new
/// `(anchor, caret)` position pair into the three-way enum the
/// renderer cares about. Tool cards (B1) collapse partial ranges to
/// `Whole` so the one-liner stays item-granular.
pub(super) fn item_selection_state(
    app: &AppState,
    index: usize,
    content_width: usize,
) -> ItemSelection {
    use crate::tui::app::ItemSelection;
    let Some(selection) = app.transcript_selection else {
        return ItemSelection::None;
    };
    if !selection.contains_item(index) {
        return ItemSelection::None;
    }
    let (start, end) = selection.range();
    let item = match app.transcript.get(index) {
        Some(item) => item,
        None => return ItemSelection::None,
    };
    let selection_width = content_width.saturating_sub(4).max(8);
    let total = selection_graphemes(item, selection_width);

    if !crate::copy::is_text_item(item) {
        return ItemSelection::Whole;
    }
    if start == end {
        return ItemSelection::None;
    }
    if start.item == end.item {
        if start.grapheme == 0 && end.grapheme >= total {
            ItemSelection::Whole
        } else {
            ItemSelection::Range {
                start: start.grapheme.min(total),
                end: end.grapheme.min(total),
            }
        }
    } else if start.item == index {
        // First item of a multi-item selection: from start.grapheme to
        // the end of the body.
        let s = start.grapheme.min(total);
        if s == 0 {
            ItemSelection::Whole
        } else {
            ItemSelection::Range {
                start: s,
                end: total,
            }
        }
    } else if end.item == index {
        // Last item: from 0 to end.grapheme.
        let e = end.grapheme.min(total);
        if e >= total {
            ItemSelection::Whole
        } else {
            ItemSelection::Range { start: 0, end: e }
        }
    } else {
        // Bracketed middle item: paint whole.
        ItemSelection::Whole
    }
}

/// Cells between `area.x` and the first body glyph of a card: 1 frame border +
/// 1 frame padding (see [`crate::tui::theme::view_frame`]) + 2 for the `┃ ` card
/// rail (see [`card_line`](super::message::card_line)). Used to turn a click
/// column into a body-relative column.
const BODY_COL_OFFSET: u16 = 4;

pub(super) fn resolve_position_in_item(
    item: &TranscriptItem,
    item_index: usize,
    row_in_item: usize,
    column: u16,
    area: Rect,
) -> TranscriptPosition {
    let content_width = area.width.saturating_sub(4).max(20) as usize;
    let inner_width = content_width.saturating_sub(4).max(8);
    if matches!(
        item,
        TranscriptItem::ToolActivity(_) | TranscriptItem::ExecutionGroup(_)
    ) {
        return TranscriptPosition {
            item: item_index,
            grapheme: usize::MAX,
            width: inner_width,
        };
    }
    if row_in_item <= 1 {
        // Spacer row (0) or title row (1): map to start of body.
        return TranscriptPosition {
            item: item_index,
            grapheme: 0,
            width: inner_width,
        };
    }
    let body = crate::copy::body_text_for(item);
    // Walk lines (post-wrap). Each line is at most `inner_width` cells.
    // Item rows: 0=spacer, 1=title, 2+=body lines.
    let body_lines = render_markdown(&body, inner_width);
    let mut grapheme_offset = 0usize;
    let mut rendered_line_idx = 2usize; // spacer=0, title=1, body starts at 2
    for (line_index, md_line) in body_lines.iter().enumerate() {
        if line_index > 0 {
            // Rendered block lines are distinct visible lines. Count their
            // newline in the selection stream even though it occupies no
            // cell inside either line.
            grapheme_offset += 1;
        }
        let wrapped = wrap_card_line(md_line, inner_width);
        for sub in &wrapped {
            if rendered_line_idx == row_in_item {
                // Column offset of the click within the body text. The body
                // starts `BODY_COL_OFFSET` cells right of `area.x`: the frame
                // eats 1 border + 1 padding cell (see `view_frame`) and every
                // `card_line` prefixes a 2-cell `┃ ` rail. Subtract all four so
                // a click lands on the grapheme actually drawn under it.
                let local_col =
                    (column as i32 - area.x as i32 - BODY_COL_OFFSET as i32).max(0) as usize;
                let grapheme_in_sub = cells_to_grapheme_index(sub, local_col);
                return TranscriptPosition {
                    item: item_index,
                    grapheme: grapheme_offset + grapheme_in_sub,
                    width: inner_width,
                };
            }
            grapheme_offset += line_grapheme_count(sub);
            rendered_line_idx += 1;
        }
    }
    // Past the end of the body — clamp to the last grapheme.
    let total = selection_graphemes(item, inner_width);
    TranscriptPosition {
        item: item_index,
        grapheme: total,
        width: inner_width,
    }
}

/// Plain text represented by a transcript card's rendered Markdown. This is
/// the shared selection/copy coordinate space: formatting markers are absent,
/// while visible block boundaries remain newlines.
pub(crate) fn selection_text_for(item: &TranscriptItem, width: usize) -> String {
    let body = crate::copy::body_text_for(item);
    render_markdown(&body, width.max(8))
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn selection_graphemes(item: &TranscriptItem, width: usize) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    selection_text_for(item, width).graphemes(true).count()
}

fn line_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Total grapheme count of a `Line` (summed across its spans).
pub(super) fn line_grapheme_count(line: &Line<'static>) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    line.spans
        .iter()
        .map(|span| span.content.as_ref().graphemes(true).count())
        .sum()
}

/// Count graphemes consumed in `line` up to `column` display cells,
/// measured from the start of the line content (not including the
/// `┃ ` rail). CJK / wide characters count as 2 cells.
pub(super) fn cells_to_grapheme_index(line: &Line<'static>, column: usize) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthChar;
    let mut cells = 0usize;
    let mut graphemes = 0usize;
    for span in &line.spans {
        for grapheme in span.content.as_ref().graphemes(true) {
            let w = grapheme
                .chars()
                .next()
                .map(|c| c.width().unwrap_or(0))
                .unwrap_or(0);
            if cells + w > column {
                return graphemes;
            }
            cells += w;
            graphemes += 1;
            if cells >= column {
                return graphemes;
            }
        }
    }
    graphemes
}
