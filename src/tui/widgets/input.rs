use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::tui::app::{AppState, CompletionCandidate, CopyNoticeKind};
use crate::tui::event::Focus;
use crate::tui::text_bounds;
use crate::tui::theme;
use crate::tui::view;

pub(crate) const PROMPT_WIDTH: u16 = 3;
const PROMPT: &str = " > ";
const CONTINUATION_PROMPT: &str = "   ";
const COMPLETION_VISIBLE_ROWS: u16 = 5;
pub(crate) const COMPLETION_HEIGHT: u16 = COMPLETION_VISIBLE_ROWS + 2;

pub fn render(f: &mut Frame, area: Rect, footer_area: Rect, app: &AppState) {
    if area.height < 2 {
        return;
    }

    let show_completion = app.completion.active && !app.completion.candidates.is_empty();
    let block = theme::frame("Input", matches!(app.focus, Focus::Input));
    let inner = block.inner(area);

    f.render_widget(block, area);

    let body_width = inner.width as usize;

    // Render against the chip-expanded display text (sentinels → `[Text N]`
    // labels) so wrapping and cursor math see pure ASCII where one grapheme is
    // one column; convert the buffer-space cursor/selection into display space.
    let display = app.composer.display();
    let display_cursor = display.to_display(app.composer.cursor);
    let selection = app
        .composer
        .selection_range()
        .map(|(s, e)| (display.to_display(s), display.to_display(e)));
    let content = if display.text.is_empty() {
        vec![placeholder_line(body_width, app.view)]
    } else {
        composer_lines(
            &display.text,
            body_width,
            selection,
            display_cursor,
            &display.chip_ranges,
        )
    };

    let total = content.len();
    let keep = total.min(body_visible_rows(area).max(1) as usize);
    let content_width = body_width.saturating_sub(PROMPT_WIDTH as usize).max(1);
    let start = compute_visible_start(
        &display.text,
        display_cursor,
        app.composer_scroll,
        app.composer_follow,
        total,
        keep,
        content_width,
    );
    let mut visible: Vec<Line<'static>> = content.into_iter().skip(start).take(keep).collect();
    if let Some(line) = visible.first_mut()
        && let Some(prompt) = line.spans.first_mut()
    {
        prompt.content = PROMPT.into();
    }

    let input = Paragraph::new(visible).style(theme::input());
    f.render_widget(input, inner);

    if footer_area.height > 0 {
        let helper = Paragraph::new(meta_line(app, footer_area.width as usize, show_completion));
        f.render_widget(helper, footer_area);
    }

    if let Some(popover_area) = active_completion_area(area, app) {
        let popover =
            render_completion_popover(app, popover_area.width as usize, popover_area.height);
        f.render_widget(Clear, popover_area);
        let widget = Paragraph::new(popover)
            .block(theme::frame(
                completion_popover_title(app, popover_area),
                false,
            ))
            .style(theme::panel())
            .wrap(Wrap { trim: false });
        f.render_widget(widget, popover_area);
    }
}

fn completion_overlay_area(area: Rect) -> Rect {
    let height = COMPLETION_HEIGHT.min(area.y);
    if height < 3 {
        return Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 0,
        };
    }

    Rect {
        x: area.x,
        y: area.y - height,
        width: area.width,
        height,
    }
}

pub(crate) fn active_completion_area(area: Rect, app: &AppState) -> Option<Rect> {
    if !app.completion.active || app.completion.candidates.is_empty() {
        return None;
    }
    let area = completion_overlay_area(area);
    (!area.is_empty()).then_some(area)
}

pub fn set_cursor(f: &mut Frame, area: Rect, app: &AppState) {
    if let Some(position) = composer_cursor_position(area, app) {
        f.set_cursor_position(position);
    }
}

fn composer_cursor_position(area: Rect, app: &AppState) -> Option<Position> {
    if !matches!(app.focus, Focus::Input) || area.width < 6 || area.height < 2 {
        return None;
    }

    let block = theme::frame("Input", true);
    let inner = block.inner(area);
    if inner.is_empty() {
        return None;
    }

    let prompt_width = PROMPT_WIDTH;
    let inner_x = inner.x;
    let inner_y = inner.y;

    if app.input().is_empty() {
        return Some(Position {
            x: inner_x + prompt_width,
            y: inner_y,
        });
    }

    let content_width = body_content_width(area) as usize;
    let display = app.composer.display();
    let cursor = display.to_display(app.composer.cursor);
    let total_visual_rows = composer_visual_row_count(&display.text, cursor, content_width);
    let body_visible = body_visible_rows(area) as usize;
    let visible_lines = total_visual_rows.min(body_visible.max(1));

    let (row, col) = composer_cursor_row_col(&display.text, cursor, content_width);
    let start = compute_visible_start(
        &display.text,
        cursor,
        app.composer_scroll,
        app.composer_follow,
        total_visual_rows,
        visible_lines.max(1),
        content_width,
    );
    if row < start || row >= start + visible_lines {
        return None;
    }
    let display_row = row.saturating_sub(start);
    let col = col.min(content_width) as u16;
    Some(Position {
        x: inner_x + prompt_width + col,
        y: inner_y + display_row as u16,
    })
}

fn placeholder_line(width: usize, view: crate::tui::event::View) -> Line<'static> {
    let hint = match view {
        crate::tui::event::View::Agent => "Ask the agent to inspect, edit, test, or explain...",
        crate::tui::event::View::Plan => {
            "Describe the task — the planner drafts it on the canvas..."
        }
    };
    Line::from(vec![
        Span::styled(
            PROMPT,
            theme::block(theme::palette().border_active, theme::palette().panel_dark),
        ),
        Span::styled(fill(hint, width), theme::composer_placeholder()),
    ])
}

fn composer_lines(
    text: &str,
    width: usize,
    selection: Option<(usize, usize)>,
    cursor: usize,
    chip_ranges: &[(usize, usize)],
) -> Vec<Line<'static>> {
    let content_width = width.saturating_sub(PROMPT_WIDTH as usize).max(1);
    let text_graphemes: Vec<&str> = text.graphemes(true).collect();
    composer_visual_rows(text, cursor, content_width)
        .into_iter()
        .map(|row| {
            let chunk = &text_graphemes[row.start..row.end];
            let prompt = if row.line_index == 0 && row.first_in_logical {
                PROMPT
            } else {
                CONTINUATION_PROMPT
            };
            let span_style = theme::block(theme::palette().text, theme::palette().panel_dark);
            // Row-relative selection and chip ranges, so span painting can work
            // in local (0-based) coordinates.
            let row_selection = selection.map(|(s, e)| {
                (
                    s.max(row.start).saturating_sub(row.start),
                    e.min(row.end).saturating_sub(row.start),
                )
            });
            let row_chips: Vec<(usize, usize)> = chip_ranges
                .iter()
                .filter_map(|&(s, e)| {
                    let start = s.max(row.start);
                    let end = e.min(row.end);
                    (start < end).then(|| (start - row.start, end - row.start))
                })
                .collect();
            let text_spans =
                build_composer_line_spans(chunk, row_selection, &row_chips, span_style);
            let mut spans = vec![Span::styled(
                prompt.to_string(),
                theme::block(theme::palette().border_active, theme::palette().panel_dark),
            )];
            spans.extend(text_spans);
            Line::from(spans)
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ComposerVisualRow {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) line_index: usize,
    pub(crate) first_in_logical: bool,
}

pub(crate) fn composer_visual_rows(
    text: &str,
    cursor: usize,
    content_width: usize,
) -> Vec<ComposerVisualRow> {
    let content_width = content_width.max(1);
    let text_len = text_bounds::grapheme_count(text);
    let cursor = cursor.min(text_len);
    let mut rows = Vec::new();
    let mut line_start = 0usize;
    let mut last_line = None;

    for (line_index, raw) in text.split('\n').enumerate() {
        let line_len = text_bounds::grapheme_count(raw);
        last_line = Some((line_index, line_start, line_len));
        if line_len == 0 {
            rows.push(ComposerVisualRow {
                start: line_start,
                end: line_start,
                line_index,
                first_in_logical: true,
            });
        } else {
            for chunk_start in (0..line_len).step_by(content_width) {
                let chunk_end = (chunk_start + content_width).min(line_len);
                rows.push(ComposerVisualRow {
                    start: line_start + chunk_start,
                    end: line_start + chunk_end,
                    line_index,
                    first_in_logical: chunk_start == 0,
                });
            }
        }
        line_start += line_len + 1;
    }

    if let Some((line_index, line_start, line_len)) = last_line
        && line_len > 0
        && cursor == text_len
        && line_len.is_multiple_of(content_width)
    {
        rows.push(ComposerVisualRow {
            start: line_start + line_len,
            end: line_start + line_len,
            line_index,
            first_in_logical: false,
        });
    }

    rows
}

pub(crate) fn composer_visual_row_count(text: &str, cursor: usize, content_width: usize) -> usize {
    composer_visual_rows(text, cursor, content_width).len()
}

pub(crate) fn composer_cursor_row_col(
    text: &str,
    cursor: usize,
    content_width: usize,
) -> (usize, usize) {
    let content_width = content_width.max(1);
    let text_len = text_bounds::grapheme_count(text);
    let cursor = cursor.min(text_len);
    let rows = composer_visual_rows(text, cursor, content_width);

    if let Some((idx, _)) = rows.iter().enumerate().find(|(_, row)| row.start == cursor) {
        return (idx, 0);
    }

    for (idx, row) in rows.iter().enumerate() {
        if row.start < cursor && cursor < row.end {
            return (idx, cursor - row.start);
        }
        if cursor == row.end {
            let col = row.end.saturating_sub(row.start);
            if col >= content_width && idx + 1 < rows.len() {
                return (idx + 1, 0);
            }
            return (idx, col);
        }
    }

    (0, 0)
}

/// Paint a visual row, grouping consecutive graphemes that share a style.
/// Each grapheme is classified as selected, part of a chip label, or plain
/// text; selection wins where it overlaps a chip.
fn build_composer_line_spans(
    chunk: &[&str],
    selection: Option<(usize, usize)>,
    chip_ranges: &[(usize, usize)],
    base_style: Style,
) -> Vec<Span<'static>> {
    let style_at = |i: usize| -> Style {
        if selection.is_some_and(|(s, e)| i >= s && i < e) {
            theme::selection_block(theme::palette().text)
        } else if chip_ranges.iter().any(|&(s, e)| i >= s && i < e) {
            theme::composer_chip()
        } else {
            base_style
        }
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buffer = String::new();
    let mut current: Option<Style> = None;

    for (i, ch) in chunk.iter().enumerate() {
        let style = style_at(i);
        if current.is_some_and(|c| c != style) && !buffer.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buffer), current.unwrap()));
        }
        current = Some(style);
        buffer.push_str(ch);
    }
    if !buffer.is_empty() {
        spans.push(Span::styled(buffer, current.unwrap_or(base_style)));
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base_style));
    }
    spans
}

fn composer_visual_row(text: &str, cursor: usize, content_width: usize) -> usize {
    composer_cursor_row_col(text, cursor, content_width).0
}

/// Pick the top visual row of the composer viewport. When the user hasn't
/// scrolled away from the cursor (`follow`) the cursor row is forced into
/// the visible window; otherwise the manual `scroll` offset is used,
/// clamped to the valid range.
pub(crate) fn compute_visible_start(
    text: &str,
    cursor: usize,
    scroll: u16,
    follow: bool,
    total: usize,
    keep: usize,
    content_width: usize,
) -> usize {
    if total <= keep {
        return 0;
    }
    let max = total - keep;
    if follow {
        let cursor_row = composer_visual_row(text, cursor, content_width);
        cursor_row.saturating_sub(keep - 1).min(max)
    } else {
        (scroll as usize).min(max)
    }
}

/// How many visual rows the framed composer can show.
pub(crate) fn body_visible_rows(area: Rect) -> u16 {
    theme::frame("Input", true).inner(area).height
}

/// Text content width inside the body, derived from the outer input
/// region. Uses the same framed inner width as `render`, then subtracts
/// the prompt prefix so cursor/scroll math matches rendered wrapping.
pub(crate) fn body_content_width(area: Rect) -> u16 {
    theme::frame("Input", true)
        .inner(area)
        .width
        .saturating_sub(PROMPT_WIDTH)
        .max(1)
}

fn push_sep(spans: &mut Vec<Span<'static>>, sep: &'static str) {
    spans.push(Span::styled(sep.to_string(), theme::composer_meta()));
}

/// Compact meta-line summary of the composer's paste chips, e.g.
/// `· [Text 1] 52L [Image 1] 24KB`, capped so a burst of pastes can't flood the
/// line. Empty when there are no chips.
fn composer_chip_meta_spans(app: &AppState) -> Vec<Span<'static>> {
    use crate::tui::app::ChipPayload;
    const SHOWN: usize = 3;

    let chips = &app.composer.chips;
    if chips.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    push_sep(&mut spans, " · ");
    for (i, chip) in chips.iter().take(SHOWN).enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ".to_string(), theme::composer_meta()));
        }
        let detail = match &chip.payload {
            ChipPayload::Text(text) => format!("{} {}L", chip.label, text.lines().count().max(1)),
            ChipPayload::Image { byte_len, .. } => {
                format!("{} {}KB", chip.label, byte_len.div_ceil(1024).max(1))
            }
        };
        spans.push(Span::styled(detail, theme::composer_chip()));
    }
    let extra = chips.len().saturating_sub(SHOWN);
    if extra > 0 {
        spans.push(Span::styled(format!(" +{extra}"), theme::composer_meta()));
    }
    spans
}

fn meta_line(app: &AppState, width: usize, completion_open: bool) -> Line<'static> {
    let view = app.view;

    // A transient notice (the Ctrl+C exit prompt or a clipboard confirmation)
    // takes over the whole meta line: the standing context and the keybind
    // hints are momentarily irrelevant, and collapsing to just the status dot
    // plus the message keeps the line from overflowing instead of stacking the
    // notice on top of a full line. The shutdown prompt wins over a copy
    // notice — it is the more time-sensitive of the two.
    // Precedence: the time-sensitive shutdown prompt wins, then the actionable
    // session hint, then a copy notice, then the low-key resume toast. All are
    // ephemeral — none of these are transcript rows.
    let notice = app
        .shutdown_notice
        .as_ref()
        .map(|text| {
            (
                text.clone(),
                theme::body(theme::palette().edit).add_modifier(Modifier::BOLD),
            )
        })
        .or_else(|| {
            app.session_hint
                .as_ref()
                .map(|hint| (hint.text.clone(), theme::body(theme::palette().todo)))
        })
        .or_else(|| {
            app.copy_notice.as_ref().map(|n| {
                let style = match n.kind {
                    CopyNoticeKind::Status => theme::body(theme::palette().success),
                    CopyNoticeKind::Error => theme::body(theme::palette().error),
                };
                (n.text.clone(), style)
            })
        })
        .or_else(|| {
            app.session_toast
                .as_ref()
                .map(|toast| (toast.text.clone(), theme::body(theme::palette().muted)))
        });
    if let Some((text, style)) = notice {
        let mut spans = vec![Span::styled(" ".to_string(), theme::composer_meta())];
        spans.extend(view::status_dot_spans(app));
        spans.push(Span::styled("  ".to_string(), theme::composer_meta()));
        spans.push(Span::styled(text, style));
        truncate_spans(&mut spans, width);
        return Line::from(spans);
    }

    // Left: status dot (spinner when busy, ● when idle), then the agent that
    // receives the prompt and the model it runs on. The dot is a bare glyph
    // colored from the palette so it reads as the status without a pill
    // background.
    // Per-persona accent + label: the active persona's declared color/name
    // (built-in or custom), falling back to the view accent when unset.
    let accent = app
        .persona_color_spec()
        .and_then(|spec| theme::persona_color(&spec))
        .unwrap_or_else(|| theme::view_accent(view).0);
    let agent = app.persona_label();
    let mut left = vec![Span::styled(" ".to_string(), theme::composer_meta())];
    left.extend(view::status_dot_spans(app));
    left.push(Span::styled("  ".to_string(), theme::composer_meta()));
    left.push(Span::styled(
        agent,
        theme::composer_meta()
            .fg(accent)
            .add_modifier(Modifier::BOLD),
    ));
    push_sep(&mut left, " · ");
    left.push(Span::styled(
        crate::tui::pickers::short_model_label(&app.model).to_string(),
        theme::composer_meta().add_modifier(Modifier::BOLD),
    ));
    // Reasoning is only worth a token when it deviates from the model default —
    // attach it to the model so a terse value like `thinking` has context.
    if app.reasoning != crate::provider::ReasoningSelection::Default {
        left.push(Span::styled(
            format!(" ({})", reasoning_meta_label(app.reasoning)),
            theme::dim(),
        ));
    }
    if app.smol_mode {
        push_sep(&mut left, " · ");
        left.push(view::smol_marker_span());
    }
    if app.serenity_mode {
        push_sep(&mut left, " · ");
        left.push(view::serenity_marker_span());
    }
    // Only surface the approval level when it deviates from the quiet `ask`
    // default (the agent label already names the active agent).
    if !app.approval_level.is_ask() {
        push_sep(&mut left, " · ");
        left.push(view::approval_marker_span(app.approval_level));
    }
    // The sandbox is an execution boundary, so show its posture even when it is
    // off or unavailable.
    if let Some(sandbox) = app.sandbox.as_ref() {
        push_sep(&mut left, " · ");
        left.push(view::sandbox_marker_span(sandbox));
    }
    left.extend(composer_chip_meta_spans(app));
    if let Some(queued) = app.first_queued_input() {
        push_sep(&mut left, "  |  ");
        let extra = app.queued_inputs.len().saturating_sub(1);
        let suffix = if extra == 0 {
            String::new()
        } else {
            format!(" +{extra}")
        };
        left.push(Span::styled(
            format!(
                "queued: {}{}",
                view::truncate_phase(&queued.text, 40),
                suffix
            ),
            theme::dim(),
        ));
    }
    if app.pending_peer_inbox > 0 {
        // Parked peer messages waiting for the next turn (peers). Blue to match
        // the peer conversation lane; self-heals to 0 once a turn drains them.
        push_sep(&mut left, "  |  ");
        left.push(Span::styled(
            format!("⇄ peers waiting: {}", app.pending_peer_inbox),
            Style::default().fg(theme::palette().peer),
        ));
    }

    // Right: contextual key hints. Kept terse — the full list is `/keys`. The
    // Tab/Shift+Tab hints live on the main screen, so the default state here just
    // shows the running build; only the transient copy/completion states add keys.
    let hints: Vec<(&str, &str)> = if app.has_copyable_selection() {
        vec![("/copy", " row  "), ("Esc", " clear")]
    } else if completion_open {
        vec![
            ("↑/↓", " choose  "),
            ("Tab", " accept  "),
            ("Esc", " dismiss"),
        ]
    } else {
        // Trailing version tag keeps the running build visible at a glance; a
        // self-update notice takes its place for the rest of the session.
        const VERSION_HINT: &str = concat!("v", env!("CARGO_PKG_VERSION"));
        vec![("", app.update_notice.as_deref().unwrap_or(VERSION_HINT))]
    };
    let mut right = Vec::with_capacity(hints.len() * 2);
    for (key, label) in hints {
        right.push(Span::styled(
            key.to_string(),
            theme::composer_meta().add_modifier(Modifier::BOLD),
        ));
        right.push(Span::styled(label.to_string(), theme::composer_meta()));
    }

    let right_used = spans_width(&right);
    if right_used >= width {
        truncate_spans(&mut right, width);
        return Line::from(right);
    }

    let left_budget = width.saturating_sub(right_used + 2);
    truncate_spans(&mut left, left_budget);
    let left_used = spans_width(&left);
    left.push(Span::styled(
        " ".repeat(width.saturating_sub(left_used + right_used)),
        theme::composer_meta(),
    ));
    left.extend(right);
    Line::from(left)
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

fn truncate_spans(spans: &mut Vec<Span<'static>>, width: usize) {
    let mut remaining = width;
    spans.retain_mut(|span| {
        if remaining == 0 {
            return false;
        }
        let span_width = UnicodeWidthStr::width(span.content.as_ref());
        if span_width > remaining {
            let mut used = 0;
            let truncated = span
                .content
                .graphemes(true)
                .take_while(|grapheme| {
                    let next = used + UnicodeWidthStr::width(*grapheme);
                    if next > remaining {
                        return false;
                    }
                    used = next;
                    true
                })
                .collect::<String>();
            span.content = truncated.into();
            remaining = remaining.saturating_sub(used);
        } else {
            remaining = remaining.saturating_sub(span_width);
        }
        true
    });
}

fn reasoning_meta_label(reasoning: crate::provider::ReasoningSelection) -> &'static str {
    match reasoning {
        crate::provider::ReasoningSelection::On => "thinking",
        other => other.as_str(),
    }
}

fn fill(text: &str, width: usize) -> String {
    let mut value: String = text.chars().take(width).collect();
    let len = value.chars().count();
    if len < width {
        value.push_str(&" ".repeat(width - len));
    }
    value
}

fn truncate(text: &str, width: usize) -> String {
    text.chars().take(width).collect()
}

/// The popover frame title, extended with a `cursor/total` position when the
/// candidate list is longer than the visible window so scrolling is evident.
fn completion_popover_title(app: &AppState, area: Rect) -> String {
    let total = app.completion.candidates.len();
    let visible = area.height.saturating_sub(2).min(COMPLETION_VISIBLE_ROWS) as usize;
    if total > visible {
        format!("Tab to accept · {}/{total}", app.completion.cursor + 1)
    } else {
        "Tab to accept".to_string()
    }
}

fn render_completion_popover(app: &AppState, width: usize, height: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let max_lines = height.saturating_sub(2).min(COMPLETION_VISIBLE_ROWS) as usize;
    let candidates = &app.completion.candidates;
    let cursor = app.completion.cursor;
    let total = candidates.len();
    if total == 0 {
        return vec![Line::from(Span::styled(
            "No completions".to_string(),
            theme::dim(),
        ))];
    }
    let start = cursor.saturating_sub(max_lines.saturating_sub(1));
    let end = (start + max_lines).min(total);
    let rows: Vec<(usize, String, String)> = candidates
        .iter()
        .enumerate()
        .take(end)
        .skip(start)
        .map(|(idx, candidate)| {
            let (label, detail) = match candidate {
                CompletionCandidate::Command { label, description } => {
                    (label.clone(), description.clone())
                }
                CompletionCandidate::Provider { display, id } => {
                    (display.clone(), format!("provider · {id}"))
                }
                CompletionCandidate::Model {
                    provider_id,
                    model,
                    replacement: _,
                    pricing,
                } => (model.clone(), format!("{provider_id} · {pricing}")),
                CompletionCandidate::Argument { label, detail, .. } => {
                    (label.clone(), detail.clone())
                }
                CompletionCandidate::Path { path, kind } => {
                    let label = if matches!(kind, crate::tui::path_search::PathKind::Directory)
                        && !path.ends_with('/')
                    {
                        format!("{path}/")
                    } else {
                        path.clone()
                    };
                    let detail = match kind {
                        crate::tui::path_search::PathKind::File => "file",
                        crate::tui::path_search::PathKind::Directory => "directory",
                    };
                    (label, detail.to_string())
                }
            };
            (idx, label, detail)
        })
        .collect();
    let inner_width = width.saturating_sub(2);
    let marker_width = 2usize;
    let gap_width = 2usize;
    let max_label_width = inner_width.saturating_sub(marker_width + gap_width + 8);
    let label_width = rows
        .iter()
        .map(|(_, label, _)| label.chars().count())
        .max()
        .unwrap_or(0)
        .min(max_label_width);
    let detail_width = inner_width.saturating_sub(marker_width + label_width + gap_width);

    for (idx, label, detail) in rows {
        let marker = if idx == cursor { "> " } else { "  " };
        let label_text = fill(&label, label_width);
        let detail_text = truncate(&detail, detail_width);
        lines.push(Line::from(vec![
            Span::styled(marker, theme::muted()),
            Span::styled(label_text, theme::body(theme::palette().text)),
            Span::styled("  ", theme::dim()),
            Span::styled(detail_text, theme::dim()),
        ]));
    }
    lines
}

pub fn composer_position_at(
    area: Rect,
    text: &str,
    cursor: usize,
    scroll: u16,
    follow: bool,
    mouse_col: u16,
    mouse_row: u16,
) -> Option<usize> {
    if area.width < 6 || area.height < 2 {
        return None;
    }
    let block = theme::frame("Input", true);
    let inner = block.inner(area);
    if inner.is_empty() {
        return None;
    }

    let body_height = inner.height;
    if body_height == 0 {
        return None;
    }

    let row_in_body = (mouse_row as i32 - inner.y as i32).max(0) as usize;
    if row_in_body >= body_height as usize {
        return None;
    }

    let col_in_text = (mouse_col as i32 - inner.x as i32 - PROMPT_WIDTH as i32).max(0) as usize;

    let content_width = body_content_width(area) as usize;
    let rows = composer_visual_rows(text, cursor, content_width);
    let total_lines = rows.len();
    let body_visible = body_visible_rows(area) as usize;
    let visible_lines = total_lines.min(body_visible.max(1));

    let start = compute_visible_start(
        text,
        cursor,
        scroll,
        follow,
        total_lines,
        visible_lines.max(1),
        content_width,
    );
    let row_index = start + row_in_body;
    let row = rows.get(row_index)?;
    let line_length = row.end.saturating_sub(row.start);
    let clamped_col = col_in_text.min(line_length);
    Some(row.start + clamped_col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::{AppState, CopyNotice, CopyNoticeKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn completion_popover_scrolls_window_to_cursor_and_shows_position() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.completion.active = true;
        app.completion.candidates = (0..12)
            .map(|i| crate::tui::app::CompletionCandidate::Path {
                path: format!("src/file_{i:02}.rs"),
                kind: crate::tui::path_search::PathKind::File,
            })
            .collect();
        app.completion.cursor = 7;

        let popover_area = Rect::new(0, 10, 40, COMPLETION_HEIGHT);
        assert_eq!(
            completion_popover_title(&app, popover_area),
            "Tab to accept · 8/12",
            "an overflowing list surfaces the cursor position in the title"
        );

        let lines = render_completion_popover(&app, 40, COMPLETION_HEIGHT);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts.len(), 5, "window stays at the visible row count");
        assert!(
            texts[4].starts_with("> ") && texts[4].contains("file_07"),
            "cursor row is the window's last row: {texts:?}"
        );
        assert!(
            texts[0].contains("file_03"),
            "window slides down to keep the cursor visible: {texts:?}"
        );

        app.completion.candidates.truncate(3);
        app.completion.cursor = 0;
        assert_eq!(
            completion_popover_title(&app, popover_area),
            "Tab to accept",
            "a fully visible list keeps the plain title"
        );
    }

    #[test]
    fn position_at_first_line_clicks_within_text() {
        let a = area(60, 6);
        let text = "hello world";
        let pos = composer_position_at(a, text, text.chars().count(), 0, true, 7, 1);
        assert_eq!(
            pos,
            Some(2),
            "click at column 7 inside 'hello world' -> char 2"
        );
    }

    #[test]
    fn position_at_clamps_to_line_end() {
        let a = area(20, 6);
        let text = "hi";
        let pos = composer_position_at(a, text, 0, 0, true, 100, 1);
        assert_eq!(pos, Some(2), "far click clamps to end of line");
    }

    #[test]
    fn position_at_cursor_row_picks_correct_logical_line_offset() {
        let a = area(60, 3);
        let text = "abc\ndef";
        let pos = composer_position_at(a, text, 7, 0, true, 6, 1);
        assert_eq!(
            pos,
            Some(5),
            "the single viewport row maps through to the cursor's logical line"
        );
    }

    #[test]
    fn position_at_returns_none_outside_single_body_row() {
        let a = area(60, 3);
        let text = "hi";
        let pos = composer_position_at(a, text, 0, 0, true, 4, 2);
        assert!(pos.is_none(), "the lower border is not composer text");
    }

    #[test]
    fn composer_lines_highlight_selection() {
        let lines = composer_lines("hello world", 60, Some((6, 11)), 11, &[]);
        let combined: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(combined, " > hello world");

        let has_selection_bg = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.style.bg == Some(theme::palette().selection_bg));
        assert!(
            has_selection_bg,
            "selection range should produce at least one span with the selection background"
        );
    }

    #[test]
    fn composer_selection_does_not_bleed_onto_the_row_below() {
        // Regression for "text selection extends one line below the cursor":
        // selecting exactly the first logical line must not paint any
        // selection background on the following visual row.
        let lines = composer_lines("first line\nsecond line", 60, Some((0, 10)), 10, &[]);
        assert!(lines.len() >= 2, "expected at least two visual rows");

        let row_has_selection = |line: &Line<'static>| {
            line.spans
                .iter()
                .any(|span| span.style.bg == Some(theme::palette().selection_bg))
        };

        assert!(
            row_has_selection(&lines[0]),
            "the first line should be highlighted"
        );
        assert!(
            !row_has_selection(&lines[1]),
            "the row below the selection end must not be highlighted"
        );
    }

    #[test]
    fn meta_line_shows_copy_command_hint_when_selection_exists() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.composer.set_text("hello".to_string());
        app.composer.set_selection(0, 5);

        let text = line_text(&meta_line(&app, 100, false));

        assert!(text.contains("/copy row"));
        assert!(text.contains("Esc clear"));
        assert!(!text.contains("Enter send"));
    }

    #[test]
    fn meta_line_shows_copy_notice() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.copy_notice = Some(CopyNotice {
            text: "Copied selected text.".to_string(),
            kind: CopyNoticeKind::Status,
            expires_at_tick: 99,
        });

        let text = line_text(&meta_line(&app, 120, false));

        assert!(text.starts_with(' '));
        assert!(text.contains("Copied selected text."));
    }

    #[test]
    fn meta_line_shows_parked_peer_inbox_badge() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );

        // No parked messages → no badge.
        let text = line_text(&meta_line(&app, 130, false));
        assert!(!text.contains("peers waiting"));

        // Parked messages → badge with the count.
        app.pending_peer_inbox = 2;
        let text = line_text(&meta_line(&app, 130, false));
        assert!(text.contains("peers waiting: 2"));

        // Drained on the next turn → badge clears.
        app.pending_peer_inbox = 0;
        let text = line_text(&meta_line(&app, 130, false));
        assert!(!text.contains("peers waiting"));
    }

    #[test]
    fn normal_meta_line_points_to_command_popup_not_named_commands() {
        let app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );

        let text = line_text(&meta_line(&app, 130, false));

        assert!(text.starts_with(' '));
        assert!(!text.contains("/ popup"));
        assert!(!text.contains("/sessions"));
        assert!(!text.contains("/resume"));
    }

    #[test]
    fn meta_line_renders_auto_accept_policy_in_caution_color() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.approval_level = crate::tool::ApprovalLevel::AutoAccept;

        let line = meta_line(&app, 130, false);
        let text = line_text(&line);
        let marker = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "auto-accept")
            .expect("auto-accept policy marker should render");

        assert!(text.contains("auto-accept"));
        assert_eq!(marker.style.fg, Some(theme::palette().todo));
        assert!(marker.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn meta_line_renders_yolo_policy_in_error_color() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.approval_level = crate::tool::ApprovalLevel::Yolo;

        let line = meta_line(&app, 130, false);
        let text = line_text(&line);
        let marker = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "yolo")
            .expect("yolo policy marker should render");

        assert!(text.contains("yolo"));
        assert_eq!(marker.style.fg, Some(theme::palette().error));
        assert!(marker.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn meta_line_attaches_reasoning_to_model() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.reasoning = crate::provider::ReasoningSelection::On;

        let text = line_text(&meta_line(&app, 130, false));

        assert!(text.contains("test-model (thinking)"));
        assert!(!text.contains(" · on · "));
    }

    #[test]
    fn meta_line_renders_active_sandbox_marker() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        let sandbox = crate::sandbox::CommandSandbox::new(
            crate::sandbox::SandboxBackend::SeatbeltExec,
            std::path::Path::new("."),
        );
        sandbox.set_enabled(true);
        app.sandbox = Some(sandbox);

        let line = meta_line(&app, 130, false);
        let text = line_text(&line);
        let marker = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "sandbox no-net")
            .expect("sandbox marker should render when active");

        assert!(text.contains("sandbox no-net"));
        assert_eq!(marker.style.fg, Some(theme::palette().success));
        assert!(marker.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn meta_line_renders_inactive_sandbox_marker() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.sandbox = Some(crate::sandbox::CommandSandbox::disabled());

        let line = meta_line(&app, 130, false);
        let text = line_text(&line);
        let marker = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "sandbox none")
            .expect("sandbox marker should render when unavailable");

        assert!(text.contains("sandbox none"));
        assert_eq!(marker.style.fg, Some(theme::palette().dim));
    }

    #[test]
    fn meta_line_hides_policy_marker_on_default() {
        let app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );

        // Default is the quiet common case — no policy marker in the meta line.
        let line = meta_line(&app, 130, false);
        assert!(!line_text(&line).contains("auto-accept"));
    }

    #[test]
    fn narrow_meta_line_stays_within_width_and_keeps_contextual_hints() {
        let app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );

        let line = meta_line(&app, 24, false);
        let text = line_text(&line);

        assert_eq!(spans_width(&line.spans), 24);
        // The default right-hand hint is the running build tag (the Tab/Shift+Tab
        // hints live on the main screen). Derived from the same source as the
        // implementation so a version bump does not break this test.
        assert!(
            text.contains(concat!("v", env!("CARGO_PKG_VERSION"))),
            "right hints should stay visible: {text:?}"
        );
        assert!(
            !text.contains("test-model"),
            "left status should truncate first: {text:?}"
        );
    }

    #[test]
    fn span_truncation_uses_terminal_width_for_wide_graphemes() {
        let mut spans = vec![Span::styled(
            "模型🙂abc".to_string(),
            theme::composer_meta(),
        )];

        truncate_spans(&mut spans, 5);

        assert_eq!(spans_width(&spans), 4);
        assert_eq!(line_text(&Line::from(spans)), "模型");
    }

    #[test]
    fn cursor_is_hidden_when_manual_scroll_shows_another_row() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.composer.set_text("first\nsecond".to_string());
        app.composer.cursor = 0;
        app.composer_scroll = 1;
        app.composer_follow = false;

        assert!(composer_cursor_position(area(40, 3), &app).is_none());
    }

    #[test]
    fn render_shows_two_content_rows_and_footer_below_border() {
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.composer.set_text("first row\nsecond row".to_string());

        terminal
            .draw(|frame| {
                render(frame, Rect::new(0, 0, 40, 4), Rect::new(0, 4, 40, 1), &app);
                set_cursor(frame, Rect::new(0, 0, 40, 4), &app);
            })
            .expect("composer should render");

        let buffer = terminal.backend().buffer();
        let row = |y| (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert!(row(1).contains("> first row"), "{}", row(1));
        assert!(row(2).contains("second row"), "{}", row(2));
        assert!(
            row(3).contains('╰'),
            "composer border should end before footer"
        );
        assert!(
            row(4).contains(concat!("v", env!("CARGO_PKG_VERSION"))),
            "footer hints should render below border: {}",
            row(4)
        );
        assert_eq!(
            terminal
                .get_cursor_position()
                .expect("cursor should render")
                .y,
            2
        );
    }

    #[test]
    fn composer_lines_wraps_long_lines() {
        // content_width = width - 3 = 20 - 3 = 17
        let lines = composer_lines(&"a".repeat(40), 20, None, 40, &[]);
        let combined: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        let a_count = combined.chars().filter(|c| *c == 'a').count();
        assert_eq!(a_count, 40, "all 40 chars must be rendered, not truncated");
        assert_eq!(lines.len(), 3, "40 chars / 17 per row = 3 visual rows");
        assert!(combined.starts_with(" > "), "first row carries the prompt");
        assert!(
            combined.contains("   aaa"),
            "continuation rows carry the indent prompt"
        );
    }

    #[test]
    fn composer_lines_renders_cursor_row_at_exact_wrap_boundary() {
        let text = "a".repeat(34);
        let lines = composer_lines(&text, 20, None, text.chars().count(), &[]);
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert_eq!(
            rendered,
            vec![
                format!(" > {}", "a".repeat(17)),
                format!("   {}", "a".repeat(17)),
                "   ".to_string(),
            ],
            "exact-width input needs an empty continuation row for the cursor"
        );
    }

    #[test]
    fn cursor_after_last_char_sits_in_the_trailing_gap_not_on_it() {
        // Typing the last character of a word leaves the cursor in the empty
        // cell *after* the word, not overlapping the last glyph. `set_cursor`
        // renders at `inner_x + PROMPT_WIDTH + col`, so the column must equal
        // the grapheme cursor (5 for "hello"), giving cell index 5 — the gap.
        let content_width = 40;
        assert_eq!(
            composer_cursor_row_col("hello", 5, content_width),
            (0, 5),
            "cursor at end of word must land one cell past the last glyph"
        );
        // Mid-word the column tracks the grapheme index exactly (no off-by-one).
        assert_eq!(
            composer_cursor_row_col("hello world", 5, content_width),
            (0, 5)
        );
        assert_eq!(composer_cursor_row_col("a", 1, content_width), (0, 1));
    }

    #[test]
    fn composer_visual_row_tracks_wrapped_rows() {
        let width = 20;
        let content_width = width - 2;
        // 40 chars on a single logical line → 3 visual rows (18 + 18 + 4)
        let text = "a".repeat(40);
        assert_eq!(composer_visual_row(&text, 0, content_width), 0);
        assert_eq!(composer_visual_row(&text, 17, content_width), 0);
        assert_eq!(composer_visual_row(&text, 18, content_width), 1);
        assert_eq!(composer_visual_row(&text, 36, content_width), 2);
    }

    #[test]
    fn composer_visual_row_does_not_double_count_newline_after_full_row() {
        let content_width = 18;
        let text = format!("{}\nb", "a".repeat(content_width));
        assert_eq!(
            composer_cursor_row_col(&text, content_width, content_width),
            (1, 0),
            "cursor before a newline after a full wrapped row stays on the next visual row"
        );
        assert_eq!(
            composer_cursor_row_col(&text, content_width + 1, content_width),
            (1, 0),
            "cursor after that newline must not skip a visual row"
        );
    }

    #[test]
    fn compute_visible_start_follows_cursor_when_follow_is_set() {
        let text = "a".repeat(40);
        // 40 chars / 18 wide = 3 visual rows; keep = 2.
        // Cursor at char 36 (row 2) with follow must land the viewport
        // on row 1 so the cursor row is the last visible row.
        let start = compute_visible_start(&text, 36, 0, true, 3, 2, 18);
        assert_eq!(start, 1);
    }

    #[test]
    fn compute_visible_start_uses_manual_scroll_when_follow_is_off() {
        let text = "a".repeat(40);
        let start = compute_visible_start(&text, 36, 1, false, 3, 2, 18);
        assert_eq!(start, 1, "manual scroll offset wins over cursor pin");
    }

    #[test]
    fn compute_visible_start_clamps_manual_scroll_to_max() {
        let text = "a".repeat(40);
        // total=3, keep=2 → max=1; scroll=99 must clamp.
        let start = compute_visible_start(&text, 0, 99, false, 3, 2, 18);
        assert_eq!(start, 1);
    }

    #[test]
    fn compute_visible_start_zero_when_content_fits() {
        let start = compute_visible_start("hi", 2, 99, true, 1, 6, 18);
        assert_eq!(start, 0);
    }

    #[test]
    fn position_at_wrapped_cursor_row_picks_wrapped_offset() {
        let a = area(24, 3);
        let content_width = body_content_width(a) as usize;
        let text = "a".repeat(content_width + 5);
        let pos =
            composer_position_at(a, &text, text.chars().count(), 0, true, PROMPT_WIDTH + 3, 1);

        assert_eq!(
            pos,
            Some(content_width + 1),
            "the visible wrapped row maps to its source character offset"
        );
    }

    #[test]
    fn body_visible_rows_matches_framed_inner_height() {
        assert_eq!(body_visible_rows(area(20, 3)), 1);
        assert_eq!(body_visible_rows(area(20, 4)), 2);
        assert_eq!(body_visible_rows(area(20, 10)), 8);
        assert_eq!(body_visible_rows(area(20, 2)), 0);
    }

    #[test]
    fn body_content_width_subtracts_borders_and_prompt() {
        // Outer width 60 → 56 inside the padded block → 53 after the prompt.
        assert_eq!(body_content_width(area(60, 10)), 53);
        assert_eq!(body_content_width(area(0, 10)), 1, "saturates to 1");
    }
}
