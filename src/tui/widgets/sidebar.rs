use std::borrow::Cow;

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, ScrollbarOrientation, ScrollbarState, Wrap};

use crate::todo::{TodoItem, TodoStatus};
use crate::tui::app::AppState;
use crate::tui::event::Focus;
use crate::tui::theme;

pub fn render(f: &mut Frame, area: Rect, app: &AppState) {
    // The sidebar is now todo-only — the context readout lives in the header
    // chip (see `view::header_line`), so the todo card owns the whole column.
    let content_width = area.width.saturating_sub(6).max(12) as usize;
    render_todo(f, area, app, content_width);
}

pub(crate) fn optional_cost_micros(cost_micros: Option<u64>) -> String {
    cost_micros
        .map(format_cost_micros)
        .unwrap_or_else(|| "n/a".to_string())
}

pub(crate) fn format_cost_micros(cost_micros: u64) -> String {
    if cost_micros == 0 {
        return "$0".to_string();
    }
    let dollars = cost_micros as f64 / 1_000_000.0;
    if cost_micros >= 1_000_000 {
        format!("${dollars:.2}")
    } else if cost_micros >= 1_000 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.6}")
    }
}

fn render_todo(f: &mut Frame, area: Rect, app: &AppState, width: usize) {
    if sidebar_bonsai_active(app) {
        render_bonsai_todo(f, area, app);
        return;
    }
    let lines = todo_lines(app, width);
    let line_count = lines.len();
    let scroll = app.sidebar_scroll.min(max_scroll(line_count, area));
    let paragraph = Paragraph::new(lines)
        .block(theme::frame(
            todo_panel_title(app),
            matches!(app.focus, Focus::Todo),
        ))
        .wrap(Wrap { trim: false })
        .style(if matches!(app.focus, Focus::Todo) {
            theme::input()
        } else {
            theme::panel()
        })
        .scroll((scroll, 0));
    f.render_widget(paragraph, area);

    if line_count > area.height as usize {
        let scrollbar_area = area.inner(Margin {
            vertical: 1,
            horizontal: 1,
        });
        let viewport = todo_viewport_height(area);
        let max_scroll = max_scroll(line_count, area);
        let mut state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll as usize)
            .viewport_content_length(viewport)
            .content_length(max_scroll as usize + 1);
        let scrollbar = theme::scrollbar(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }
}

pub(crate) fn max_todo_scroll(app: &AppState, area: Rect) -> u16 {
    let content_width = area.width.saturating_sub(6).max(12) as usize;
    max_scroll(todo_lines(app, content_width).len(), area)
}

fn max_scroll(line_count: usize, area: Rect) -> u16 {
    line_count.saturating_sub(todo_viewport_height(area)) as u16
}

fn todo_viewport_height(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

/// Panel title for the TODO card. Any multi-phase plan carries its phase
/// position on the label (e.g. `TODO · Phase 2/4`), including when idle before
/// `/start`, so a freshly loaded plan reads `TODO · Phase 1/5`. While the user
/// browses a phase other than the active running one, a `(viewing)` marker
/// flags that the card is off the live phase. Flat and single-phase plans just
/// show `TODO`.
fn todo_panel_title(app: &AppState) -> String {
    let total = app.plan.phases.len();
    if total <= 1 {
        return "TODO".to_string();
    }
    let viewed = app.resolved_todo_phase().unwrap_or(0);
    let active = app.plan_execution.map(|execution| execution.phase_index);
    if active.is_some() && Some(viewed) != active {
        format!("TODO · Phase {}/{total} (viewing)", viewed + 1)
    } else {
        format!("TODO · Phase {}/{total}", viewed + 1)
    }
}

/// Todo items the card should show: the live `app.todo` for the active phase (or
/// a flat plan), else the viewed phase's checklist derived from the plan canvas.
/// The common path stays a borrow — only browsing a non-active phase clones.
fn viewed_todo_items(app: &AppState) -> Cow<'_, [TodoItem]> {
    match app.resolved_todo_phase() {
        Some(phase) if Some(phase) != app.plan_execution.map(|e| e.phase_index) => {
            Cow::Owned(app.plan.phase_todo_items(phase))
        }
        _ => Cow::Borrowed(app.todo.as_slice()),
    }
}

fn todo_lines(app: &AppState, width: usize) -> Vec<Line<'static>> {
    let items = viewed_todo_items(app);
    if items.is_empty() {
        return vec![Line::from(Span::styled(
            "  none",
            Style::default().fg(theme::palette().dim),
        ))];
    }

    let mut lines = Vec::new();
    for todo in items.iter() {
        lines.extend(todo_item(todo, width));
    }
    lines
}

/// Scroll offset that brings the first in-progress todo item into view in the
/// todo card, clamped to `[0, max_scroll]`. Returns 0 (top) when nothing is in
/// progress. Counts rendered lines exactly as [`todo_lines`] does (same content
/// width and viewport), so the offset lines up with the paragraph scroll.
pub(crate) fn todo_in_progress_scroll(app: &AppState, area: Rect, max_scroll: u16) -> u16 {
    let content_width = area.width.saturating_sub(6).max(12) as usize;
    let viewport = todo_viewport_height(area);
    let items = viewed_todo_items(app);

    let mut line = 0usize;
    let mut start = None;
    for todo in items.iter() {
        if start.is_none() && todo.status == TodoStatus::InProgress {
            start = Some(line);
        }
        line += todo_item(todo, content_width).len();
    }

    let Some(start) = start else { return 0 };
    // Already inside the first viewport: stay at the top; otherwise put the
    // item's first line at the top of the viewport.
    let offset = if start < viewport { 0 } else { start };
    (offset as u16).min(max_scroll)
}

fn todo_item(todo: &TodoItem, width: usize) -> Vec<Line<'static>> {
    let (glyph, color, text_style) = todo_status_style(todo.status);
    let text_width = width.saturating_sub(4).max(8);
    let mut lines = Vec::new();

    for (idx, line) in todo
        .content
        .split('\n')
        .flat_map(|line| wrap_text(line, text_width))
        .enumerate()
    {
        if idx == 0 {
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", glyph), Style::default().fg(color)),
                Span::styled(line, text_style),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(line, text_style),
            ]));
        }
    }

    lines
}

/// When the todo card shows the bonsai instead of a checklist: whenever the
/// viewed list is empty (the tree is the card's resting state), plus the few
/// seconds a manual `/bonsai` replant is animating over a non-empty list.
/// Screen readers always get the textual card — colored half-blocks are noise
/// to them.
fn sidebar_bonsai_active(app: &AppState) -> bool {
    if app.screen_reader_mode {
        return false;
    }
    viewed_todo_items(app).is_empty() || app.sidebar_bonsai.is_growing()
}

/// The bonsai in the todo card: the procedurally grown tree centered in the
/// panel, with the site hero's "[ grow again ]" affordance as a dim hint once
/// growth completes.
fn render_bonsai_todo(f: &mut Frame, area: Rect, app: &AppState) {
    let focused = matches!(app.focus, Focus::Todo);
    let paragraph = Paragraph::new("")
        .block(theme::frame(todo_panel_title(app), focused))
        .style(if focused {
            theme::input()
        } else {
            theme::panel()
        });
    f.render_widget(paragraph, area);

    let inner = area.inner(Margin {
        vertical: 1,
        horizontal: 1,
    });
    if inner.height < 2 {
        return;
    }
    // Bottom row is reserved for the hint; the tree gets the rest.
    let tree_area = Rect {
        height: inner.height - 1,
        ..inner
    };
    let progress = app.bonsai_progress(&app.sidebar_bonsai);
    app.sidebar_bonsai
        .render(tree_area, f.buffer_mut(), progress);

    if progress >= 1.0 {
        let hint = "[ /bonsai ]";
        let width = (hint.len() as u16).min(inner.width);
        let hint_area = Rect {
            x: inner.x + inner.width.saturating_sub(width) / 2,
            y: inner.y + inner.height - 1,
            width,
            height: 1,
        };
        let hint = Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(theme::palette().dim),
        )));
        f.render_widget(hint, hint_area);
    }
}

fn todo_status_style(status: TodoStatus) -> (&'static str, Color, Style) {
    match status {
        TodoStatus::Pending => (
            "○",
            theme::palette().muted,
            Style::default().fg(theme::palette().text),
        ),
        TodoStatus::InProgress => (
            "•",
            theme::palette().todo,
            Style::default().fg(theme::palette().text),
        ),
        TodoStatus::Completed => (
            "✓",
            theme::palette().success,
            Style::default().fg(theme::palette().muted),
        ),
        TodoStatus::Cancelled => (
            "×",
            theme::palette().dim,
            Style::default().fg(theme::palette().dim),
        ),
    }
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let next_len =
            current.chars().count() + usize::from(!current.is_empty()) + word.chars().count();
        if next_len > width && !current.is_empty() {
            lines.push(current);
            current = String::new();
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::todo::TodoItem;
    use crate::tui::app::AppState;

    #[test]
    fn todo_frame_highlights_when_focused() {
        // The sidebar is todo-only now — the whole column is the todo card, so
        // its frame lights up at the sidebar's top-left when focus is on it.
        let area = Rect::new(0, 0, 34, 20);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.focus = Focus::Todo;
        app.todo = vec![TodoItem {
            content: "Scroll this task".to_string(),
            status: TodoStatus::InProgress,
        }];

        terminal
            .draw(|frame| render(frame, area, &app))
            .expect("sidebar should render");

        let buffer = terminal.backend().buffer();
        let palette = theme::palette();
        assert_eq!(buffer[(area.x, area.y)].fg, palette.border_active);
    }

    #[test]
    fn empty_todo_shows_bonsai_tree_with_replant_hint() {
        let area = Rect::new(0, 0, 34, 24);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        // Fresh app: no todos, sidebar bonsai grows on startup.
        // Fast-forward to fully grown so we can assert the replant hint.
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.sidebar_bonsai = crate::tui::widgets::bonsai::BonsaiGrowth::grown();

        terminal
            .draw(|frame| render(frame, area, &app))
            .expect("sidebar should render");

        let buffer = terminal.backend().buffer();
        let mut half_blocks = 0;
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                let symbol = buffer[(x, y)].symbol();
                if matches!(symbol, "▀" | "▄") {
                    half_blocks += 1;
                }
                text.push_str(symbol);
            }
            text.push('\n');
        }
        assert!(
            half_blocks > 50,
            "a grown tree should paint substantial pixel art, got {half_blocks}"
        );
        assert!(
            text.contains("[ /bonsai ]"),
            "grown tree should advertise the replant command, got {text:?}"
        );
    }

    #[test]
    fn screen_reader_mode_keeps_textual_todo_card() {
        let area = Rect::new(0, 0, 34, 24);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.screen_reader_mode = true;

        terminal
            .draw(|frame| render(frame, area, &app))
            .expect("sidebar should render");

        let buffer = terminal.backend().buffer();
        for y in 0..area.height {
            for x in 0..area.width {
                let symbol = buffer[(x, y)].symbol();
                assert!(
                    !matches!(symbol, "▀" | "▄"),
                    "screen readers should never get pixel art, found {symbol} at ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn todo_panel_title_shows_phase_for_multi_phase_plan() {
        use crate::plan::{PlanDoc, PlanPhase};
        use crate::tui::app::PlanExecution;

        let phase = |name: &str| PlanPhase {
            name: name.to_string(),
            tasks: Vec::new(),
        };
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );

        // Flat / no-plan: plain label.
        assert_eq!(todo_panel_title(&app), "TODO");

        // Idle multi-phase plan (before /start): the phase position is always
        // shown, defaulting to phase 1.
        app.plan = PlanDoc {
            phases: vec![phase("a"), phase("b"), phase("c"), phase("d")],
            ..Default::default()
        };
        assert_eq!(todo_panel_title(&app), "TODO · Phase 1/4");

        // Phased run, phase 2 of 4 (1-based), not browsing.
        app.plan_execution = Some(PlanExecution { phase_index: 1 });
        assert_eq!(todo_panel_title(&app), "TODO · Phase 2/4");

        // Browsing a non-active phase while running: marked `(viewing)`.
        app.todo_phase_view = Some(3);
        assert_eq!(todo_panel_title(&app), "TODO · Phase 4/4 (viewing)");

        // Browsing back to the active phase drops the marker.
        app.todo_phase_view = Some(1);
        assert_eq!(todo_panel_title(&app), "TODO · Phase 2/4");

        // A single-phase plan is not "multiple phases" — stays plain.
        app.plan = PlanDoc {
            phases: vec![phase("only")],
            ..Default::default()
        };
        app.plan_execution = Some(PlanExecution { phase_index: 0 });
        app.todo_phase_view = None;
        assert_eq!(todo_panel_title(&app), "TODO");
    }

    #[test]
    fn viewed_phase_renders_plan_checklist_not_live_todo() {
        use crate::plan::{PlanDoc, PlanPhase, PlanTask};
        use crate::tui::app::PlanExecution;

        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.plan = PlanDoc {
            phases: vec![
                PlanPhase {
                    name: "one".to_string(),
                    tasks: vec![PlanTask {
                        text: "phase one task".to_string(),
                        done: false,
                    }],
                },
                PlanPhase {
                    name: "two".to_string(),
                    tasks: vec![PlanTask {
                        text: "phase two task".to_string(),
                        done: false,
                    }],
                },
            ],
            ..Default::default()
        };
        // Active phase 0 with a distinct live runtime list.
        app.plan_execution = Some(PlanExecution { phase_index: 0 });
        app.todo = vec![TodoItem {
            content: "LIVE active item".to_string(),
            status: TodoStatus::InProgress,
        }];
        // Browsing phase 1 shows that phase's plan checklist, not `app.todo`.
        app.todo_phase_view = Some(1);

        let text = lines_text(&todo_lines(&app, 40));
        assert!(text.contains("phase two task"), "{text}");
        assert!(!text.contains("LIVE active item"), "{text}");

        // Following the active phase again shows the live list.
        app.todo_phase_view = None;
        let text = lines_text(&todo_lines(&app, 40));
        assert!(text.contains("LIVE active item"), "{text}");
    }

    #[test]
    fn todo_in_progress_scroll_brings_late_item_into_view() {
        let area = Rect::new(0, 0, 34, 10);
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        // 30 one-line items; the in-progress one sits well below the viewport.
        app.todo = (0..30)
            .map(|idx| TodoItem {
                content: format!("Task {idx}"),
                status: if idx == 20 {
                    TodoStatus::InProgress
                } else {
                    TodoStatus::Pending
                },
            })
            .collect();

        let max = max_todo_scroll(&app, area);
        let offset = todo_in_progress_scroll(&app, area, max);
        let viewport = todo_viewport_height(area);
        assert!(offset > 0, "should scroll down to reach a late item");
        assert!(offset <= max, "offset {offset} must stay within max {max}");
        // The in-progress item (line 20) ends up inside the viewport.
        assert!(
            offset <= 20 && 20 < offset + viewport as u16,
            "offset {offset}"
        );
    }

    /// Flatten rendered todo lines back to text for content assertions.
    fn lines_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn todo_scroll_clamps_to_max_in_short_sidebar() {
        // The todo card owns the whole sidebar column now; a tall list in a
        // short column is where off-by-one scroll bugs surface.
        let area = Rect::new(0, 0, 34, 10);
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        // 30 one-line items — far more than the todo viewport.
        app.todo = (0..30)
            .map(|idx| TodoItem {
                content: format!("Task {idx}"),
                status: TodoStatus::Pending,
            })
            .collect();

        app.sidebar_scroll = u16::MAX;
        let todo_area = area;
        let max = max_todo_scroll(&app, todo_area);
        app.clamp_sidebar_scroll(max);
        assert!(
            app.sidebar_scroll <= max,
            "sidebar_scroll {} should not exceed max_scroll {}",
            app.sidebar_scroll,
            max
        );
        assert!(
            max > 0,
            "a 30-item list in a small viewport should have a positive max_scroll"
        );
        // max_scroll = line_count − viewport_height, and viewport_height
        // accounts for the 2-line frame border — never negative.
        let viewport = todo_viewport_height(todo_area);
        assert!(
            max as usize + viewport <= 30,
            "max_scroll ({max}) + viewport ({viewport}) should not exceed line_count (30)"
        );
    }
}
