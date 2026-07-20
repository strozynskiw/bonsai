//! Plan canvas: the large right-hand pane of the plan view. Renders the
//! shared `PlanDoc` as a readable section canvas with its own scroll
//! position, plus an empty state explaining the workflow.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, ScrollbarOrientation, ScrollbarState, Wrap};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::plan::{Finding, PlanDoc, PlanPhase, PlanSection, PlanTask, Severity};
use crate::tui::app::{AppState, PlanPosition, PlanSelection};
use crate::tui::theme;
use crate::tui::widgets::transcript;

const BODY_INDENT: &str = "    ";
const TASK_MARK_WIDTH: usize = 4; // "[ ] "

struct WrapRun {
    text: String,
    style: Style,
}

struct WrapCell {
    run: usize,
    ch: char,
    width: usize,
}

pub fn render(f: &mut Frame, area: Rect, app: &AppState) {
    if area.is_empty() {
        return;
    }

    let content_width = area.width.saturating_sub(4).max(20) as usize;
    let mut lines = canvas_lines(app, content_width);
    apply_selection_to_lines(&mut lines, app.plan_selection, content_width);
    let viewport_height = viewport_height(area);
    // Reuse the `lines` we just built rather than re-laying-out the whole
    // canvas via `wrapped_line_count`.
    let max_scroll = lines
        .iter()
        .map(|line| wrapped_rows_for_line(line, content_width))
        .sum::<usize>()
        .saturating_sub(viewport_height) as u16;
    let scroll = app.plan_scroll.min(max_scroll);

    let title = if !app.plan.title.is_empty() {
        format!("Plan · {}", app.plan.title)
    } else if app.plan.is_empty() {
        "Plan".to_string()
    } else if app.plan.is_phased() {
        format!("Plan · {} phases", app.plan.phases.len())
    } else {
        format!("Plan · {} tasks", app.plan.tasks.len())
    };
    // Keyboard scrolling drives the canvas while the plan pane has
    // focus, so mirror that focus state on the frame.
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(theme::view_frame(
            title,
            matches!(app.focus, crate::tui::event::Focus::Plan),
            crate::tui::event::View::Plan,
        ))
        .style(theme::panel())
        .scroll((scroll, 0));
    f.render_widget(paragraph, area);

    if max_scroll > 0 {
        // Place the scrollbar 2 cells inside the right border so the track
        // glyph (┊) is clearly separated from the block border (│). The
        // 1-cell-inside position (the transcript's pattern) puts the track
        // adjacent to the border, and since both use the same `│` glyph, the
        // scrollbar disappears into a single thick line.
        let scrollbar_area = Rect {
            x: area.x + area.width.saturating_sub(2),
            y: area.y + 1,
            width: 1,
            height: area.height.saturating_sub(2),
        };
        let mut state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll as usize)
            .viewport_content_length(viewport_height);
        let scrollbar = theme::scrollbar(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }
}

pub(crate) fn max_plan_scroll(app: &AppState, area: Rect) -> u16 {
    if area.is_empty() {
        return 0;
    }
    let content_width = area.width.saturating_sub(4).max(20) as usize;
    wrapped_line_count(app, content_width).saturating_sub(viewport_height(area)) as u16
}

pub(crate) fn position_at(
    app: &AppState,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<PlanPosition> {
    if area.is_empty() {
        return None;
    }
    let inner = area.inner(ratatui::layout::Margin {
        vertical: 1,
        horizontal: 1,
    });
    if !inner.contains((column, row).into()) {
        return None;
    }

    let content_width = area.width.saturating_sub(4).max(20) as usize;
    let lines = canvas_lines(app, content_width);
    let target_visual_row = row.saturating_sub(inner.y).saturating_add(app.plan_scroll) as usize;
    let mut visual_row = 0usize;
    for (line_index, line) in lines.iter().enumerate() {
        let rows = wrapped_rows_for_line(line, content_width);
        if target_visual_row < visual_row + rows {
            let wrapped_row = target_visual_row - visual_row;
            let column = column.saturating_sub(inner.x) as usize;
            let grapheme = grapheme_at_visual_column(line, content_width, wrapped_row, column);
            return Some(PlanPosition {
                line: line_index,
                grapheme,
                width: content_width,
            });
        }
        visual_row += rows;
    }
    lines.last().map(|line| PlanPosition {
        line: lines.len().saturating_sub(1),
        grapheme: line_grapheme_count(line),
        width: content_width,
    })
}

pub(crate) fn selected_text(app: &AppState) -> Option<String> {
    let selection = app.plan_selection?;
    if !has_text_selection(app) {
        return None;
    }
    let lines = canvas_plain_lines(app, selection.anchor.width);
    let (start, end) = selection.range();
    if start.line >= lines.len() || end.line >= lines.len() {
        return None;
    }

    let mut selected = Vec::new();
    for (line_index, line) in lines.iter().enumerate().take(end.line + 1).skip(start.line) {
        let line_graphemes = line.graphemes(true).count();
        let from = if line_index == start.line {
            start.grapheme.min(line_graphemes)
        } else {
            0
        };
        let to = if line_index == end.line {
            end.grapheme.min(line_graphemes)
        } else {
            line_graphemes
        };
        selected.push(grapheme_slice(line, from, to));
    }

    let text = selected.join("\n");
    (!text.is_empty()).then_some(text)
}

pub(crate) fn has_text_selection(app: &AppState) -> bool {
    let Some(selection) = app.plan_selection else {
        return false;
    };
    selection.anchor != selection.caret
        && !canvas_plain_lines(app, selection.anchor.width).is_empty()
}

pub(crate) fn line_end_position(app: &AppState, position: PlanPosition) -> PlanPosition {
    let lines = canvas_plain_lines(app, position.width);
    let max_line = lines.len().saturating_sub(1);
    let line = position.line.min(max_line);
    let grapheme = lines
        .get(line)
        .map(|line| line.graphemes(true).count())
        .unwrap_or(0);
    PlanPosition {
        line,
        grapheme,
        width: position.width,
    }
}

pub(crate) fn word_selection(app: &AppState, position: PlanPosition) -> Option<PlanSelection> {
    let lines = canvas_plain_lines(app, position.width);
    let line = lines.get(position.line)?;
    if line.is_empty() {
        return None;
    }
    let cursor = position.grapheme.min(line.graphemes(true).count());
    let (start, end) = crate::tui::text_bounds::word_bounds_at(line, cursor);
    Some(PlanSelection {
        anchor: PlanPosition {
            line: position.line,
            grapheme: start,
            width: position.width,
        },
        caret: PlanPosition {
            line: position.line,
            grapheme: end,
            width: position.width,
        },
    })
}

fn viewport_height(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

fn canvas_lines(app: &AppState, width: usize) -> Vec<Line<'static>> {
    if app.plan.is_empty() {
        return empty_state(width);
    }

    render_plan_canvas(&app.plan, width)
}

fn render_plan_canvas(plan: &PlanDoc, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for section in &plan.sections {
        push_section_spacer(&mut lines);
        lines.extend(render_section(section, width));
    }
    if !plan.questions.is_empty() {
        push_block_spacer(&mut lines);
        lines.extend(render_questions(&plan.questions, width));
    }
    if !plan.phases.is_empty() {
        push_block_spacer(&mut lines);
        lines.extend(render_phases(&plan.phases, width));
    }
    if !plan.tasks.is_empty() {
        push_block_spacer(&mut lines);
        lines.extend(render_tasks(&plan.tasks, width));
    }
    if !plan.findings.is_empty() {
        push_block_spacer(&mut lines);
        lines.extend(render_findings(&plan.findings_in_severity_order(), width));
    }
    if lines.is_empty() {
        empty_state(width)
    } else {
        lines
    }
}

fn push_block_spacer(lines: &mut Vec<Line<'static>>) {
    if !lines.is_empty() {
        lines.push(Line::from(""));
    }
}

fn push_section_spacer(lines: &mut Vec<Line<'static>>) {
    if !lines.is_empty() {
        for _ in 0..2 {
            lines.push(Line::from(""));
        }
    }
}

fn render_section(section: &PlanSection, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![heading_line(&section.heading)];
    let body = strip_leading_duplicate_heading(section.body.trim(), &section.heading);
    if body.is_empty() {
        return lines;
    }

    let body_width = body_width(width);
    for line in transcript::render_markdown(body, body_width) {
        for wrapped in wrap_styled_line(&line, body_width) {
            lines.push(indent_line(wrapped, BODY_INDENT));
        }
    }
    lines
}

/// Drops a leading markdown heading (`#`..`######`) whose text equals the
/// section heading, plus the blank lines that follow it, so the body never
/// repeats the rail heading already shown above it. A sub-point with a
/// different name is preserved untouched.
fn strip_leading_duplicate_heading<'a>(body: &'a str, heading: &str) -> &'a str {
    let rest = body.trim_start();
    let end = rest.find('\n').unwrap_or(rest.len());
    let first = rest[..end].trim();
    let after = first.trim_start_matches('#');
    let is_heading = after.len() < first.len() && after.starts_with(' ');
    if is_heading && after.trim().eq_ignore_ascii_case(heading.trim()) {
        return rest[end..].trim_start();
    }
    rest
}

fn render_tasks(tasks: &[PlanTask], width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![heading_line("Tasks")];
    lines.extend(render_task_rows(tasks, width));
    lines
}

/// Renders each phase as a rail heading (with a done/total count) followed by
/// its own checklist, reusing the task-row layout so phased and flat plans look
/// consistent. Phases are separated by a blank line.
fn render_phases(phases: &[PlanPhase], width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, phase) in phases.iter().enumerate() {
        if index > 0 {
            push_block_spacer(&mut lines);
        }
        let done = phase.tasks.iter().filter(|task| task.done).count();
        lines.push(heading_line(&format!(
            "{} ({done}/{})",
            phase.name.trim(),
            phase.tasks.len()
        )));
        lines.extend(render_task_rows(&phase.tasks, width));
    }
    lines
}

/// The checklist body shared by `render_tasks` and `render_phases`: one
/// `[x]`/`[ ]`-marked, wrapped row per task, without a heading.
fn render_task_rows(tasks: &[PlanTask], width: usize) -> Vec<Line<'static>> {
    let text_width = width
        .saturating_sub(display_width(BODY_INDENT))
        .saturating_sub(TASK_MARK_WIDTH)
        .max(8);

    let mut lines = Vec::new();
    for task in tasks {
        let marker = if task.done { "[x] " } else { "[ ] " };
        let marker_style = if task.done {
            transcript::md_bold(theme::palette().success)
        } else {
            transcript::md(theme::palette().muted)
        };
        let text_style = if task.done {
            transcript::md(theme::palette().muted)
        } else {
            transcript::md(theme::palette().text)
        };
        let wrapped = wrap_text(&task.text, text_width);
        for (index, text) in wrapped.into_iter().enumerate() {
            if index == 0 {
                lines.push(Line::from(vec![
                    Span::raw(BODY_INDENT.to_string()),
                    Span::styled(marker.to_string(), marker_style),
                    Span::styled(text, text_style),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(format!("{BODY_INDENT}{}", " ".repeat(TASK_MARK_WIDTH))),
                    Span::styled(text, text_style),
                ]));
            }
        }
    }
    lines
}

/// Renders the open-questions block: a rail heading plus one `?`-marked row
/// per question, mirroring `render_tasks` so continuation lines align under the
/// text and the block reads as a sibling of the Tasks checklist.
fn render_questions(questions: &[String], width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![heading_line("Open questions")];
    let text_width = width
        .saturating_sub(display_width(BODY_INDENT))
        .saturating_sub(TASK_MARK_WIDTH)
        .max(8);

    for question in questions {
        let wrapped = wrap_text(question, text_width);
        for (index, text) in wrapped.into_iter().enumerate() {
            if index == 0 {
                lines.push(Line::from(vec![
                    Span::raw(BODY_INDENT.to_string()),
                    Span::styled(
                        "?   ".to_string(),
                        transcript::md_bold(theme::palette().plan_accent),
                    ),
                    Span::styled(text, transcript::md(theme::palette().text)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(format!("{BODY_INDENT}{}", " ".repeat(TASK_MARK_WIDTH))),
                    Span::styled(text, transcript::md(theme::palette().text)),
                ]));
            }
        }
    }
    lines
}

/// Accent color for a finding severity badge, mirroring `tool_status_style`'s
/// severity-to-color convention: Blocker=error, Major=todo, Minor=progress,
/// Nit=muted. The single source of truth — the finding-detail modal reuses it.
pub(crate) fn severity_color(severity: Severity) -> ratatui::style::Color {
    match severity {
        Severity::Blocker => theme::palette().error,
        Severity::Major => theme::palette().todo,
        Severity::Minor => theme::palette().progress,
        Severity::Nit => theme::palette().muted,
    }
}

/// Renders the `Findings` block: a rail heading plus, per finding, a colored
/// severity badge with `file:line — issue`, then a dim trace line naming the
/// associated task and source evidence. Most-severe first; resolved findings
/// render muted. Mirrors `render_questions`/`render_task_rows` layout so it
/// reads as a sibling block.
fn render_findings(findings: &[&Finding], width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![heading_line("Findings")];
    let text_width = width
        .saturating_sub(display_width(BODY_INDENT))
        .saturating_sub(TASK_MARK_WIDTH)
        .max(8);
    let continuation_indent = format!("{BODY_INDENT}{}", " ".repeat(TASK_MARK_WIDTH));

    // `findings` arrives pre-sorted (most-severe first) via
    // `PlanDoc::findings_in_severity_order`, so the canvas order matches the
    // modal/handoff order exactly.
    for finding in findings {
        let badge_color = if finding.resolved {
            theme::palette().muted
        } else {
            severity_color(finding.severity)
        };
        let text_color = if finding.resolved {
            theme::palette().muted
        } else {
            theme::palette().text
        };
        let badge = format!(
            "[{}]{} ",
            finding.severity.label(),
            if finding.resolved { " (resolved)" } else { "" }
        );
        let headline = match finding.location_label() {
            Some(loc) => format!("{loc} — {}", finding.issue.trim()),
            None => finding.issue.trim().to_string(),
        };
        // Reserve the (variable-width) badge on the first line so it can't push
        // the headline past the pane edge.
        let headline_width = width
            .saturating_sub(display_width(BODY_INDENT))
            .saturating_sub(display_width(&badge))
            .max(8);
        let wrapped = wrap_text(&headline, headline_width);
        for (index, text) in wrapped.into_iter().enumerate() {
            if index == 0 {
                lines.push(Line::from(vec![
                    Span::raw(BODY_INDENT.to_string()),
                    Span::styled(badge.clone(), transcript::md_bold(badge_color)),
                    Span::styled(text, transcript::md(text_color)),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::raw(continuation_indent.clone()),
                    Span::styled(text, transcript::md(text_color)),
                ]));
            }
        }

        let mut trace = vec![format!(
            "Task: {}",
            finding
                .task
                .as_deref()
                .unwrap_or(crate::plan::FINDING_UNASSIGNED)
        )];
        if !finding.source_ids.is_empty() {
            trace.push(format!("Evidence: {}", finding.source_ids.join(", ")));
        }
        let trace_line = format!("↳ {}", trace.join("  ·  "));
        for text in wrap_text(&trace_line, text_width) {
            lines.push(Line::from(vec![
                Span::raw(continuation_indent.clone()),
                Span::styled(text, transcript::md(theme::palette().muted)),
            ]));
        }
    }
    lines
}

fn heading_line(heading: &str) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "▌ ".to_string(),
            transcript::md_bold(theme::palette().plan_accent),
        ),
        Span::styled(
            heading.trim().to_string(),
            transcript::md_bold(theme::palette().plan_accent),
        ),
    ])
}

fn body_width(width: usize) -> usize {
    width.saturating_sub(display_width(BODY_INDENT)).max(8)
}

fn indent_line(line: Line<'static>, indent: &str) -> Line<'static> {
    if line_width(&line) == 0 {
        return Line::from("");
    }
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(indent.to_string()));
    spans.extend(line.spans);
    Line::from(spans)
}

fn wrap_styled_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line_width(line) <= width {
        return vec![line.clone()];
    }

    let mut runs: Vec<WrapRun> = Vec::new();
    for span in &line.spans {
        if span.content.is_empty() {
            continue;
        }
        if let Some(last) = runs.last_mut()
            && last.style == span.style
        {
            last.text.push_str(span.content.as_ref());
        } else {
            runs.push(WrapRun {
                text: span.content.to_string(),
                style: span.style,
            });
        }
    }

    let cells = runs
        .iter()
        .enumerate()
        .flat_map(|(run_index, run)| {
            run.text.chars().map(move |ch| WrapCell {
                run: run_index,
                ch,
                width: ch.width().unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();

    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < cells.len() {
        let mut line_end = cursor;
        let mut current_width = 0usize;
        let mut last_break = None;
        while line_end < cells.len() {
            let cell = &cells[line_end];
            if current_width + cell.width > width {
                break;
            }
            current_width += cell.width;
            if matches!(cell.ch, ' ' | '\t') {
                last_break = Some(line_end + 1);
            }
            line_end += 1;
        }

        if line_end == cursor {
            line_end = cursor + 1;
            last_break = None;
        }

        let overflowed = line_end < cells.len();
        let slice_end = match last_break {
            Some(break_at) if overflowed && break_at < line_end => break_at,
            _ => line_end,
        };

        out.push(line_from_cells(&cells[cursor..slice_end], &runs));
        cursor = slice_end;
    }

    if out.is_empty() {
        vec![Line::from("")]
    } else {
        out
    }
}

fn line_from_cells(cells: &[WrapCell], runs: &[WrapRun]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut current_run = usize::MAX;
    let mut current_text = String::new();

    for cell in cells {
        if cell.run != current_run {
            if !current_text.is_empty() {
                let style = runs[current_run].style;
                spans.push(Span::styled(current_text, style));
            }
            current_run = cell.run;
            current_text = String::new();
        }
        current_text.push(cell.ch);
    }
    if !current_text.is_empty() {
        let style = runs[current_run].style;
        spans.push(Span::styled(current_text, style));
    }
    Line::from(spans)
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    if words.is_empty() {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    for word in words {
        let word_width = display_width(word);
        let separator_width = usize::from(!current.is_empty());
        if !current.is_empty() && display_width(&current) + separator_width + word_width > width {
            lines.push(current);
            current = String::new();
        }
        if word_width > width {
            if !current.is_empty() {
                lines.push(current);
                current = String::new();
            }
            lines.extend(split_long_word(word, width));
        } else {
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn split_long_word(word: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    let width = width.max(1);

    for grapheme in word.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if current_width > 0 && current_width + grapheme_width > width {
            lines.push(current);
            current = String::new();
            current_width = 0;
        }
        current.push_str(grapheme);
        current_width += grapheme_width;
        if current_width >= width {
            lines.push(current);
            current = String::new();
            current_width = 0;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn line_width(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

fn canvas_plain_lines(app: &AppState, content_width: usize) -> Vec<String> {
    canvas_lines(app, content_width.max(1))
        .iter()
        .map(line_text)
        .collect()
}

fn line_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn line_grapheme_count(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.graphemes(true).count())
        .sum()
}

fn grapheme_at_visual_column(
    line: &Line<'static>,
    content_width: usize,
    wrapped_row: usize,
    column: usize,
) -> usize {
    let target_display_column = wrapped_row
        .saturating_mul(content_width.max(1))
        .saturating_add(column);
    let text = line_text(line);
    let mut display_column = 0usize;
    for (index, grapheme) in text.graphemes(true).enumerate() {
        let next = display_column + display_width(grapheme);
        if target_display_column < next {
            return index;
        }
        display_column = next;
    }
    text.graphemes(true).count()
}

fn apply_selection_to_lines(
    lines: &mut [Line<'static>],
    selection: Option<PlanSelection>,
    content_width: usize,
) {
    let Some(selection) = selection else {
        return;
    };
    if selection.anchor.width != content_width || selection.caret.width != content_width {
        return;
    }
    let (start, end) = selection.range();
    if start == end {
        return;
    }

    for (line_index, line) in lines.iter_mut().enumerate() {
        if line_index < start.line || line_index > end.line {
            continue;
        }
        let line_graphemes = line_grapheme_count(line);
        let from = if line_index == start.line {
            start.grapheme.min(line_graphemes)
        } else {
            0
        };
        let to = if line_index == end.line {
            end.grapheme.min(line_graphemes)
        } else {
            line_graphemes
        };
        if from < to {
            *line = select_line_range(line, from, to);
        }
    }
}

fn select_line_range(line: &Line<'static>, from: usize, to: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for span in &line.spans {
        let text = span.content.as_ref();
        let span_len = text.graphemes(true).count();
        let span_start = cursor;
        let span_end = cursor + span_len;
        if to <= span_start || from >= span_end {
            spans.push(span.clone());
        } else {
            let selected_from = from.saturating_sub(span_start);
            let selected_to = (to.saturating_sub(span_start)).min(span_len);
            push_plan_selection_spans(&mut spans, text, span.style, selected_from, selected_to);
        }
        cursor = span_end;
    }
    Line::from(spans)
}

fn push_plan_selection_spans(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    selected_from: usize,
    selected_to: usize,
) {
    let before = grapheme_slice(text, 0, selected_from);
    if !before.is_empty() {
        spans.push(Span::styled(before, style));
    }
    let selected = grapheme_slice(text, selected_from, selected_to);
    if !selected.is_empty() {
        spans.push(Span::styled(
            selected,
            style.patch(theme::selection_block(theme::palette().text)),
        ));
    }
    let after = grapheme_slice(text, selected_to, text.graphemes(true).count());
    if !after.is_empty() {
        spans.push(Span::styled(after, style));
    }
}

fn grapheme_slice(text: &str, from: usize, to: usize) -> String {
    text.graphemes(true)
        .skip(from)
        .take(to.saturating_sub(from))
        .collect()
}

/// Number of visual rows the canvas will occupy after `Paragraph::wrap` is
/// applied at render time. The pre-wrap `Line` count returned by
/// `canvas_lines` is the *logical* count; the `scroll` value in
/// `Paragraph::scroll((scroll, 0))` is applied *after* wrapping, so the
/// `max_scroll` clamp must agree with the wrapped count. Otherwise any line
/// that exceeds `content_width` adds invisible extra rows, and the user
/// lands a few rows short of the bottom on `G`/auto-follow.
fn wrapped_line_count(app: &AppState, content_width: usize) -> usize {
    canvas_lines(app, content_width)
        .iter()
        .map(|line| wrapped_rows_for_line(line, content_width))
        .sum()
}

fn wrapped_rows_for_line(line: &Line<'static>, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let line_width = line_width(line);
    if line_width <= width {
        1
    } else {
        line_width.div_ceil(width)
    }
}

/// Indent shared by every empty-state line.
const EMPTY_STATE_INDENT: &str = "  ";
/// The relevant plan commands, listed one per table row.
const EMPTY_STATE_ROWS: &[(&str, &str)] = &[
    ("/start", "hand the plan to the coding agent"),
    ("/save", "save the current plan"),
    ("/discard", "delete the canvas plan"),
    ("/export", "write the plan to a file"),
    ("Shift+Tab", "switch back to the coding agent"),
];

/// The empty-canvas brief: a short intro plus the relevant commands rendered as
/// a bordered table — a small structured artifact so the blank canvas reads as a
/// canvas, not a void. Falls back to a plain aligned list when the canvas is too
/// narrow for the table to fit without wrapping.
fn empty_state(width: usize) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let heading = transcript::md_bold(palette.border_active);
    let intro = transcript::md(palette.muted);
    let border = transcript::md(palette.muted);
    let cmd_style = transcript::md_bold(palette.success);
    let desc_style = transcript::md(palette.dim);

    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{EMPTY_STATE_INDENT}No plan yet"),
            heading,
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!(
                "{EMPTY_STATE_INDENT}Describe a task in the composer and the planner drafts the"
            ),
            intro,
        )),
        Line::from(Span::styled(
            format!(
                "{EMPTY_STATE_INDENT}implementation plan on this canvas, exploring the codebase as it goes."
            ),
            intro,
        )),
        Line::from(""),
    ];

    let (cmd_col, desc_col) = empty_state_column_widths();
    let table_width = display_width(EMPTY_STATE_INDENT) + cmd_col + desc_col + 3;
    // Render the table only when it fits exactly. Narrow panes use a plain
    // aligned list so the block border never wraps into the next visual row.
    if width < table_width {
        for (cmd, desc) in EMPTY_STATE_ROWS {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{EMPTY_STATE_INDENT}{}", padded_cell(cmd, cmd_col)),
                    cmd_style,
                ),
                Span::styled((*desc).to_string(), desc_style),
            ]));
        }
        return lines;
    }

    let rule = |left: char, mid: char, right: char| {
        Line::from(Span::styled(
            format!(
                "{EMPTY_STATE_INDENT}{left}{}{mid}{}{right}",
                "─".repeat(cmd_col),
                "─".repeat(desc_col),
            ),
            border,
        ))
    };
    lines.push(rule('╭', '┬', '╮'));
    for (cmd, desc) in EMPTY_STATE_ROWS {
        lines.push(Line::from(vec![
            Span::styled(format!("{EMPTY_STATE_INDENT}│"), border),
            Span::styled(padded_cell(cmd, cmd_col), cmd_style),
            Span::styled("│".to_string(), border),
            Span::styled(padded_cell(desc, desc_col), desc_style),
            Span::styled("│".to_string(), border),
        ]));
    }
    lines.push(rule('╰', '┴', '╯'));
    lines
}

fn empty_state_column_widths() -> (usize, usize) {
    let max_cmd = EMPTY_STATE_ROWS
        .iter()
        .map(|(cmd, _)| display_width(cmd))
        .max()
        .unwrap_or(0);
    let max_desc = EMPTY_STATE_ROWS
        .iter()
        .map(|(_, desc)| display_width(desc))
        .max()
        .unwrap_or(0);
    (max_cmd + 2, max_desc + 2)
}

fn padded_cell(text: &str, width: usize) -> String {
    let text_width = display_width(text);
    let trailing = width.saturating_sub(text_width + 1);
    format!(" {text}{}", " ".repeat(trailing))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppState {
        AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        )
    }

    fn rendered(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn line_index(text: &[String], needle: &str) -> usize {
        text.iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("expected line containing {needle:?}, got: {text:?}"))
    }

    #[test]
    fn empty_plan_briefs_the_workflow_without_the_old_essay() {
        let app = app();
        let text = rendered(&canvas_lines(&app, 60));

        assert!(text.iter().any(|line| line.contains("No plan yet")));
        assert!(
            text.iter().any(|line| line.contains("planner drafts")),
            "empty canvas should explain what plan mode does: {text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("composer")),
            "empty canvas should point at the composer: {text:?}"
        );
        // The relevant commands are listed one per row in the table.
        assert!(
            text.iter()
                .any(|line| line.contains("/start") && line.contains("coding agent")),
            "empty canvas should list the /start command: {text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("/save"))
                && text.iter().any(|line| line.contains("/discard"))
                && text.iter().any(|line| line.contains("/export")),
            "empty canvas should list /save, /discard, and /export: {text:?}"
        );
        // Rendered as a bordered table so the blank canvas reads as a canvas.
        assert!(
            text.iter()
                .any(|line| line.contains('╭') || line.contains('┬')),
            "empty canvas should draw the command table border: {text:?}"
        );
        assert!(!text.iter().any(|line| line.contains("Alt+I")));
        // The verbose tool-name essay stays gone — keep it a brief, not a manual.
        assert!(
            !text.iter().any(|line| line.contains("plan_set_title")),
            "the tool-name essay should be gone: {text:?}"
        );
        assert!(
            text.len() <= 14,
            "empty state should stay a brief, got {} lines: {text:?}",
            text.len()
        );
    }

    #[test]
    fn findings_render_with_severity_badge_and_trace() {
        let mut app = app();
        app.plan.edit().set_title("Plan");
        app.plan.edit().add_task("Fix it");
        app.plan.edit().add_finding(crate::plan::Finding {
            severity: crate::plan::Severity::Blocker,
            file: Some("src/foo.rs".to_string()),
            line: Some(42),
            issue: "data loss".to_string(),
            required_fix: "flush".to_string(),
            acceptance_tests: vec![],
            source_ids: vec!["call-1".to_string()],
            task: Some("Fix it".to_string()),
            resolved: false,
        });

        let text = rendered(&canvas_lines(&app, 70));
        assert!(text.iter().any(|line| line.contains("Findings")));
        let badge = line_index(&text, "[BLOCKER]");
        assert!(
            text[badge].contains("src/foo.rs:42 — data loss"),
            "badge row should carry location + issue: {:?}",
            text[badge]
        );
        assert!(
            text.iter()
                .any(|line| line.contains("Task: Fix it") && line.contains("Evidence: call-1")),
            "a trace line should name the task and evidence: {text:?}"
        );
    }

    #[test]
    fn empty_plan_table_fits_canvas_without_wrapping() {
        let app = app();
        // A bordered row must never exceed the content width, or the Paragraph's
        // wrap would split the table. (Prose/list rows are allowed to wrap.)
        // Narrow widths fall back to a list, so only check where the table shows.
        let is_table_line = |line: &str| line.chars().any(|c| "╭╮╰╯┬┴│─".contains(c));
        for width in [30usize, 42, 48, 60, 100] {
            for line in rendered(&canvas_lines(&app, width)) {
                if is_table_line(&line) {
                    assert!(
                        display_width(&line) <= width,
                        "table line {line:?} ({} cols) exceeds width {width}",
                        display_width(&line)
                    );
                }
            }
        }
    }

    #[test]
    fn empty_plan_table_columns_align_to_longest_command() {
        let app = app();
        let text = rendered(&canvas_lines(&app, 60));
        let top = text
            .iter()
            .find(|line| line.contains('╭'))
            .unwrap_or_else(|| panic!("expected table top border in {text:?}"));
        let top_columns = border_columns(top, &['╭', '┬', '╮']);
        let max_cmd = EMPTY_STATE_ROWS
            .iter()
            .map(|(cmd, _)| display_width(cmd))
            .max()
            .unwrap_or(0);
        let max_desc = EMPTY_STATE_ROWS
            .iter()
            .map(|(_, desc)| display_width(desc))
            .max()
            .unwrap_or(0);
        let expected_width = display_width(EMPTY_STATE_INDENT) + max_cmd + max_desc + 7;

        assert_eq!(display_width(top), expected_width);

        for line in text.iter().filter(|line| line.contains('│')) {
            assert_eq!(
                border_columns(line, &['│']),
                top_columns,
                "table row should align with the header border: {line:?}"
            );
        }
        let bottom = text
            .iter()
            .find(|line| line.contains('╰'))
            .unwrap_or_else(|| panic!("expected table bottom border in {text:?}"));
        assert_eq!(border_columns(bottom, &['╰', '┴', '╯']), top_columns);
    }

    fn border_columns(line: &str, border_chars: &[char]) -> Vec<usize> {
        line.chars()
            .enumerate()
            .filter_map(|(index, ch)| border_chars.contains(&ch).then_some(index))
            .collect()
    }

    #[test]
    fn populated_plan_renders_markdown_and_tasks() {
        let mut app = app();
        app.plan.edit().set_title("Demo plan");
        app.plan
            .edit()
            .set_section("Goal", "Ship the plan canvas polish.");
        app.plan
            .edit()
            .set_section("Approach", "| Step | File |\n| --- | --- |\n| 1 | a.rs |");
        app.plan.edit().add_task("Do the thing");
        app.plan.edit().check_task("Do the thing");

        let text = rendered(&canvas_lines(&app, 80));

        assert!(
            !text.iter().any(|line| line.contains("Demo plan")),
            "title should not also be rendered inside the canvas body"
        );
        assert!(
            text.iter().any(|line| line.contains("▌ Goal")),
            "section headings should use the plan rail/accent treatment: {text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("▌ Approach")),
            "all sections should render as accented blocks: {text:?}"
        );
        let goal = line_index(&text, "▌ Goal");
        let approach = line_index(&text, "▌ Approach");
        assert_eq!(
            text[approach - 1],
            "",
            "section break should leave a blank row directly above the next heading: {text:?}"
        );
        assert_eq!(
            text[approach - 2],
            "",
            "section break should be two blank rows wider than the Tasks gap: {text:?}"
        );
        assert!(approach > goal, "sections should keep plan order: {text:?}");
        assert!(
            text.iter()
                .any(|line| line.contains("Step") && line.contains("│")),
            "section tables should render with borders: {text:?}"
        );
        let tasks = line_index(&text, "▌ Tasks");
        assert!(
            tasks > approach,
            "tasks should render as a distinct final block: {text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("[x] Do the thing")),
            "completed tasks should use custom checklist rendering: {text:?}"
        );
    }

    #[test]
    fn section_body_heading_matching_section_is_not_duplicated() {
        let mut app = app();
        app.plan.edit().set_section("Goal", "## Goal\n\nShip it.");

        let text = rendered(&canvas_lines(&app, 80));
        let goal_lines = text.iter().filter(|line| line.contains("Goal")).count();
        assert_eq!(
            goal_lines, 1,
            "section heading should render once, not duplicated by a body heading: {text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("▌ Goal")),
            "the rail heading should remain: {text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("Ship it.")),
            "body content after the stripped heading should remain: {text:?}"
        );
    }

    #[test]
    fn section_body_subheading_with_different_name_is_preserved() {
        let mut app = app();
        app.plan
            .edit()
            .set_section("Approach", "### Edge cases\n\n- empty plan");

        let text = rendered(&canvas_lines(&app, 80));
        assert!(
            text.iter().any(|line| line.contains("Edge cases")),
            "a sub-point heading with a different name should survive: {text:?}"
        );
    }

    #[test]
    fn open_questions_render_as_their_own_block() {
        let mut app = app();
        app.plan.edit().set_section("Goal", "Ship it.");
        app.plan.edit().add_question("Which auth method?");
        app.plan.edit().add_task("Wire it up");

        let text = rendered(&canvas_lines(&app, 80));
        let questions = line_index(&text, "▌ Open questions");
        let tasks = line_index(&text, "▌ Tasks");
        assert!(
            text.iter()
                .any(|line| line.contains("?   Which auth method?")),
            "questions should render with a ? marker: {text:?}"
        );
        assert!(
            questions < tasks,
            "open questions should render before the tasks block: {text:?}"
        );
    }

    #[test]
    fn task_continuation_lines_align_under_task_text() {
        let mut app = app();
        app.plan.edit().add_task(
            "Audit the plan canvas renderer with a deliberately long task that wraps cleanly",
        );

        let text = rendered(&canvas_lines(&app, 42));
        let first = line_index(&text, "[ ] Audit");
        assert!(
            text[first].starts_with("    [ ] "),
            "first task line should include the checkbox prefix: {text:?}"
        );
        assert!(
            text[first + 1].starts_with("        "),
            "wrapped task continuation should align under text, not under the checkbox: {text:?}"
        );
        assert!(
            !text[first + 1].contains("[ ]"),
            "continuation lines should not repeat the checkbox: {text:?}"
        );
    }

    #[test]
    fn dense_plan_render_keeps_section_blocks_separated() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        app.plan.edit().set_title("Dense migration plan");
        app.plan.edit().set_section(
            "Goal",
            "Move the canvas to plan-specific rendering while keeping `/start` markdown stable.",
        );
        app.plan.edit().set_section(
            "Approach",
            "- Keep the transcript renderer untouched\n\
             - Render section bodies through markdown\n\
             1. Add helpers\n\
             2. Update tests\n\n\
             | Area | Check |\n\
             | --- | --- |\n\
             | Canvas | section cards |\n\
             | Scroll | visual rows |",
        );
        app.plan.edit().set_section(
            "Risks",
            "- Wrapped rows must still drive scroll limits\n\
             - Tables should stay readable inside the indented body",
        );
        app.plan
            .edit()
            .add_task("Render section headings with a rail");
        app.plan
            .edit()
            .add_task("Verify checklist wrapping with enough detail to span multiple rows");

        let area = Rect::new(0, 0, 100, 36);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, area, &app))
            .expect("dense plan should render");

        let buffer = terminal.backend().buffer().clone();
        let rendered = buffer_to_string(&buffer, area);
        assert!(rendered.contains("▌ Goal"));
        assert!(rendered.contains("▌ Approach"));
        assert!(rendered.contains("▌ Risks"));
        assert!(rendered.contains("▌ Tasks"));
        assert!(rendered.contains("│ Area"));
        assert!(rendered.contains("[ ] Render section headings"));

        let rows = rendered
            .lines()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for heading in ["▌ Approach", "▌ Risks", "▌ Tasks"] {
            let index = line_index(&rows, heading);
            let previous_body = rows[index - 1]
                .trim_start_matches('│')
                .trim_end_matches('│')
                .trim();
            assert!(
                index > 0 && previous_body.is_empty(),
                "heading {heading:?} should be separated from the prior body:\n{rendered}"
            );
        }
    }

    #[test]
    fn plan_title_appears_in_frame_header() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        app.plan.edit().set_title("Add per-provider rate limiting");
        app.plan
            .edit()
            .add_task("Wire limiters into the provider registry");

        let area = Rect::new(0, 0, 80, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, area, &app))
            .expect("plan with title should render");

        let buffer = terminal.backend().buffer().clone();
        let top_row: String = (0..area.width).map(|x| buffer[(x, 0)].symbol()).collect();

        assert!(
            top_row.contains("Add per-provider rate limiting"),
            "plan title should appear in the frame's top row, got: {top_row:?}"
        );
    }

    #[test]
    fn plan_frame_falls_back_to_task_count_when_title_missing() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        app.plan.edit().set_section("Goal", "Do the thing.");
        app.plan.edit().add_task("One");

        let area = Rect::new(0, 0, 80, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, area, &app))
            .expect("plan without title should render");

        let buffer = terminal.backend().buffer().clone();
        let top_row: String = (0..area.width).map(|x| buffer[(x, 0)].symbol()).collect();

        assert!(
            top_row.contains("1 tasks"),
            "frame should fall back to the task count when no title is set, got: {top_row:?}"
        );
    }

    #[test]
    fn plan_frame_active_border_uses_focus_plan() {
        use crate::tui::event::Focus;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        app.plan.edit().set_title("Active");
        app.view = crate::tui::event::View::Plan;
        app.focus = Focus::Plan;
        let area = Rect::new(0, 0, 80, 20);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, area, &app))
            .expect("plan with focus should render");

        let buffer = terminal.backend().buffer().clone();
        // Top-left corner glyph ─ should carry the plan accent colour when
        // the canvas is focused. We don't pin the exact RGB (theme may
        // shift) — just check it's *not* the muted/dim colour used when
        // the canvas is unfocused.
        let active = theme::palette().plan_accent;
        let corner = &buffer[(0, 0)];
        assert_eq!(
            corner.fg, active,
            "plan canvas corner should light up to plan_accent when focused, got {:?}",
            corner.fg
        );

        // And when not focused, the same glyph reverts to the border colour.
        app.focus = Focus::Transcript;
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, area, &app))
            .expect("plan without focus should render");
        let buffer = terminal.backend().buffer().clone();
        let corner = &buffer[(0, 0)];
        assert_ne!(
            corner.fg, active,
            "plan canvas corner should not be plan_accent when chat pane is focused"
        );
    }

    #[test]
    fn max_scroll_grows_with_content() {
        let mut app = app();
        for index in 0..40 {
            app.plan.edit().add_task(&format!("Task number {index}"));
        }
        let area = Rect::new(0, 0, 60, 12);
        assert!(max_plan_scroll(&app, area) > 0);
    }

    #[test]
    fn max_scroll_accounts_for_wrapped_lines() {
        let mut app = app();
        let long = "a".repeat(200);
        app.plan.edit().add_task(&long);
        let area = Rect::new(0, 0, 60, 12);
        let content_width = 60_usize.saturating_sub(4);
        let wrapped = wrapped_line_count(&app, content_width);
        assert!(wrapped > 1, "long task must create multiple visual rows");

        let max = max_plan_scroll(&app, area);
        let viewport = viewport_height(area);
        assert!(
            (max as usize) + viewport >= wrapped,
            "max_scroll {max} + viewport {viewport} must cover the wrapped rows {wrapped}"
        );
    }

    #[test]
    fn auto_follow_lands_on_last_wrapped_row() {
        // run.rs auto-follows plan edits with SetPlanScroll(u16::MAX); the
        // clamp must land on the last visible row, not the pre-wrap count.
        let mut app = app();
        let long = "b".repeat(300);
        app.plan.edit().add_task(&long);
        let area = Rect::new(0, 0, 60, 12);
        app.plan_scroll = u16::MAX;
        app.clamp_plan_scroll(crate::tui::widgets::plan::max_plan_scroll(&app, area));
        let content_width = 60_usize.saturating_sub(4);
        let wrapped = wrapped_line_count(&app, content_width);
        let viewport = viewport_height(area);
        let last_visible = app.plan_scroll as usize + viewport - 1;
        assert!(
            last_visible >= wrapped - 1,
            "auto-follow must reach the last wrapped row: last_visible={last_visible}, wrapped={wrapped}"
        );
    }

    #[test]
    fn scrollbar_renders_when_plan_overflows() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app();
        app.plan.edit().set_title("Demo plan");
        for index in 0..20 {
            app.plan.edit().add_task(&format!("Task number {index}"));
        }
        let area = Rect::new(0, 0, 80, 12);
        assert!(
            max_plan_scroll(&app, area) > 0,
            "plan must overflow the viewport for the scrollbar to appear"
        );

        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test backend");
        terminal
            .draw(|frame| render(frame, area, &app))
            .expect("plan with overflow should render");
        let buffer = terminal.backend().buffer().clone();

        // The scrollbar lives at column area.width - 2 (2 cells inside the
        // right border) so the track (┊) is clearly separated from the block
        // border (│). The thumb and track share the column.
        let scrollbar_col = area.width.saturating_sub(2);
        let canvas_inner_rows = 1..area.height.saturating_sub(1);
        let has_thumb = canvas_inner_rows
            .clone()
            .any(|y| buffer[(scrollbar_col, y)].symbol() == "┃");
        let has_track = canvas_inner_rows
            .clone()
            .any(|y| buffer[(scrollbar_col, y)].symbol() == "┊");
        // The right border (column area.width - 1) must be the rounded-border
        // glyph, not a scrollbar glyph, confirming the scrollbar is separate.
        let border_col = area.width.saturating_sub(1);
        let border_is_border = canvas_inner_rows
            .clone()
            .all(|y| buffer[(border_col, y)].symbol() == "│");
        assert!(
            has_thumb,
            "expected a scrollbar thumb (┃) in the scrollbar column; got buffer:\n{}",
            buffer_to_string(&buffer, area)
        );
        assert!(
            has_track,
            "expected the scrollbar track (┊) in the scrollbar column; got buffer:\n{}",
            buffer_to_string(&buffer, area)
        );
        assert!(
            border_is_border,
            "right border column should be │, not a scrollbar glyph; got buffer:\n{}",
            buffer_to_string(&buffer, area)
        );
    }

    fn buffer_to_string(buffer: &ratatui::buffer::Buffer, area: Rect) -> String {
        let mut out = String::new();
        for y in 0..area.height {
            let mut line = String::new();
            for x in 0..area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}
