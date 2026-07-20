use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, ScrollbarOrientation, ScrollbarState, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::app::{
    AppState, ExecutionGroup, InlineToolSelection, ItemSelection, ToolActivity, ToolStatus,
    TranscriptItem, TranscriptPosition,
};
use crate::tui::event::{CommandOutputKind, Focus, View};
use crate::tui::theme;

const QUEUED_CANCEL_LABEL: &str = "Del";

#[derive(Debug, Clone, Copy)]
pub(super) struct TranscriptRenderOptions<'a> {
    selected: ItemSelection,
    focused: bool,
    active_group_tool_selection: Option<&'a InlineToolSelection>,
    serenity_mode: bool,
    linear_output: bool,
    execution_group_expanded: bool,
    reasoning_active: bool,
    permission_command: Option<&'a str>,
    tick: u64,
}

pub(super) fn execution_group_expanded_for_item(app: &AppState, item: &TranscriptItem) -> bool {
    if app.screen_reader_mode {
        return matches!(item, TranscriptItem::ExecutionGroup(_));
    }
    match item {
        TranscriptItem::ExecutionGroup(group) => app.execution_group_is_expanded(group.id),
        _ => false,
    }
}

pub(super) fn serenity_reasoning_is_active(
    app: &AppState,
    index: usize,
    item: &TranscriptItem,
) -> bool {
    app.serenity_mode
        && app.task_state.is_busy()
        && matches!(item, TranscriptItem::ReasoningSummary { .. })
        && app.transcript.first_trailing_queued_index().checked_sub(1) == Some(index)
}

mod common;
mod diff_card;
mod hit_test;
mod layout;
mod markdown;
mod message;
mod selection;
mod tool_card;

pub(crate) use common::tool_result_body;
pub(crate) use diff_card::{diff_header_line, diff_lines};
pub(crate) use hit_test::{ExecutionGroupRowHit, TranscriptHit, hit_at, position_at};
pub(crate) use layout::TranscriptLayoutCache;
pub(crate) use markdown::{md, md_bold, render_markdown};
pub(crate) use selection::{item_line_span, selection_text_for};

use common::*;
use layout::resolved_scroll;
use message::*;
use selection::item_selection_state;
use tool_card::pending_permission_command;

pub fn render(f: &mut Frame, area: Rect, app: &AppState) {
    let content_width = area.width.saturating_sub(4).max(20) as usize;
    let viewport_height = transcript_viewport_height(area);
    let window = app
        .transcript_layout
        .view_window(app, content_width, viewport_height);

    let title = transcript_title(app);
    let block = theme::view_frame(title, matches!(app.focus, Focus::Transcript), app.view);
    let style = if matches!(app.focus, Focus::Transcript) {
        theme::input()
    } else {
        theme::panel()
    };

    let (scroll, max_scroll) = if window.total_rows == 0 {
        // Nothing visible (empty transcript or all items suppressed): the
        // welcome screen. Its lines are not card-padded to the content width,
        // so keep the legacy wrap-and-scroll Paragraph for this branch only.
        let text = empty_state(app);
        let text_rows = wrapped_rows_for_lines(&text, content_width);
        let (scroll, max_scroll) = resolved_scroll(app, text_rows, viewport_height);
        let paragraph = Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(block)
            .style(style)
            .scroll((scroll, 0));
        f.render_widget(paragraph, area);
        (scroll, max_scroll)
    } else {
        // Cached lines are pre-wrapped to exactly `content_width` cells and
        // already sliced to the viewport, so the Paragraph needs neither
        // `wrap` nor `scroll` — painting is O(viewport), not O(session).
        let paragraph = Paragraph::new(window.lines).block(block).style(style);
        f.render_widget(paragraph, area);
        (window.scroll, window.max_scroll)
    };

    if max_scroll > 0 {
        let scrollbar_area = area.inner(Margin {
            vertical: 1,
            horizontal: 1,
        });
        let mut state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll as usize)
            .viewport_content_length(viewport_height);
        let scrollbar = theme::scrollbar(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }
}

fn transcript_title(app: &AppState) -> String {
    match app.view {
        View::Plan => "Chat".to_string(),
        View::Agent => app
            .current_session_title()
            .map(|title| format!("Chat · {title}"))
            .unwrap_or_else(|| "Chat".to_string()),
    }
}

/// The floating "jump to latest" pill, shown only while the transcript is
/// scrolled up with content still below the fold. Returns the overlay rect and
/// its label so the renderer and the mouse hit-test share one source of truth
/// (mirrors [`crate::tui::view::header_context_chip_rect`]). `None` whenever
/// the reader is caught up — auto-following already pins the view to the
/// bottom, where the affordance would be pointless.
pub(crate) fn jump_to_bottom_pill(app: &AppState, area: Rect) -> Option<(Rect, String)> {
    if app.transcript_autoscroll_enabled() {
        return None;
    }
    let max_scroll = max_transcript_scroll(app, area);
    if max_scroll == 0 || app.current_scroll() >= max_scroll {
        return None;
    }

    let count = app.unseen_message_count();
    let full = match count {
        0 => "↓ Jump to latest (Ctrl+End)".to_string(),
        1 => "↓ 1 new message (Ctrl+End)".to_string(),
        n => format!("↓ {n} new messages (Ctrl+End)"),
    };
    if let Some(rect) = pill_rect(area, &full) {
        return Some((rect, full));
    }
    // Narrow chat panes (e.g. the plan view's chat column) can't fit the
    // labelled pill; fall back to a compact badge before giving up entirely.
    let compact = match count {
        0 => "↓ latest".to_string(),
        n => format!("↓ {n} new"),
    };
    pill_rect(area, &compact).map(|rect| (rect, compact))
}

/// Centered single-row rect for `label`, pinned just above the transcript's
/// bottom border. `None` when the frame is too small to host the pill.
fn pill_rect(area: Rect, label: &str) -> Option<Rect> {
    let pill_width = UnicodeWidthStr::width(label) as u16 + 2;
    let inner_width = area.width.saturating_sub(2);
    if area.height < 3 || inner_width < pill_width {
        return None;
    }
    Some(Rect {
        x: area.x + 1 + (inner_width - pill_width) / 2,
        y: area.bottom().saturating_sub(2),
        width: pill_width,
        height: 1,
    })
}

/// Draw the jump-to-latest pill as an overlay over the bottom of the
/// transcript. A no-op while the reader is caught up.
pub fn render_jump_to_bottom(f: &mut Frame, area: Rect, app: &AppState) {
    let Some((rect, label)) = jump_to_bottom_pill(app, area) else {
        return;
    };
    let (accent, _) = theme::view_accent(app.view);
    let style = Style::default()
        .fg(theme::palette().bg)
        .bg(accent)
        .add_modifier(Modifier::BOLD);
    f.render_widget(Clear, rect);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(format!(" {label} "), style))),
        rect,
    );
}

pub(crate) fn max_transcript_scroll(app: &AppState, area: Rect) -> u16 {
    let content_width = area.width.saturating_sub(4).max(20) as usize;
    let viewport = transcript_viewport_height(area);
    let total = match app.transcript_layout.total_rows(app, content_width) {
        // Welcome screen: its unwrapped lines still go through the legacy
        // wrap math, mirroring the empty branch of `render`.
        0 => wrapped_rows_for_lines(&empty_state(app), content_width),
        total => total,
    };
    resolved_scroll(app, total, viewport).1
}

/// Post-`Paragraph::wrap` row estimate for a single line. Only the welcome
/// screen and per-click walks inside a group card
/// ([`hit_test::execution_group_row_hit`]) still need this — cached transcript
/// lines are pre-wrapped to exactly the content width, so their row count is
/// simply `lines.len()`.
pub(super) fn wrapped_rows_for_line(line: &Line<'static>, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let line_width = line
        .spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum::<usize>();
    if line_width <= width {
        1
    } else {
        line_width.div_ceil(width)
    }
}

/// Post-wrap row count for a slice of pre-wrap `Line`s, matching the
/// `Paragraph::wrap` the renderer applies. Hit-testing must walk items in
/// this same wrapped-row space or clicks drift once any line wraps.
pub(super) fn wrapped_rows_for_lines(lines: &[Line<'static>], width: usize) -> usize {
    lines
        .iter()
        .map(|line| wrapped_rows_for_line(line, width))
        .sum()
}

pub(super) fn transcript_viewport_height(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

/// The full transcript as rendered lines, served from the layout cache.
/// Test/compat surface: the golden rendering tests call this, which makes
/// them exercise the cache path for free. Production paints go through
/// [`TranscriptLayoutCache::view_window`] and never materialize the whole
/// transcript.
#[cfg(test)]
pub(super) fn transcript_lines(app: &AppState, width: usize) -> Vec<Line<'static>> {
    let lines = app.transcript_layout.all_lines(app, width);
    if lines.is_empty() {
        empty_state(app)
    } else {
        lines
    }
}

#[cfg(test)]
mod tests;
