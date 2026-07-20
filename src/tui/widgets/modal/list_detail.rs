use super::common::*;
use super::*;

/// How a list-plus-detail modal arranges its two panes.
#[derive(Debug, Clone, Copy)]
pub(super) enum ListDetailSplit {
    /// List table on top, detail below (tasks, subagents).
    Vertical,
    /// List on the left, detail on the right (skills).
    Horizontal,
}

/// Configuration shared by all two-pane list/detail modals.
pub(super) struct ListDetailModal<'a> {
    pub title: &'a str,
    pub detail_title: &'a str,
    pub split: ListDetailSplit,
    pub detail_focused: bool,
    pub footer_lines: Vec<Line<'static>>,
    pub modal_scroll: u16,
}

/// Render shared list/detail chrome and delegate list shaping to the caller.
///
/// The callback receives the list and detail rectangles and returns the exact
/// detail lines rendered and measured by the shared pane.
pub(super) fn render_list_detail_modal(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    modal: ListDetailModal<'_>,
    render_list: impl FnOnce(&mut Frame, Rect, Rect) -> Vec<Line<'static>>,
) {
    let panel = theme::frame(modal.title, true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let (list_area, detail_area, footer_area) = list_detail_regions(area, modal.split);
    let detail_lines = render_list(f, list_area, detail_area);
    render_detail_pane(
        f,
        detail_area,
        modal.detail_title,
        modal.detail_focused,
        detail_lines,
        modal.modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
    f.render_widget(
        Paragraph::new(modal.footer_lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        footer_area,
    );
}

/// The `(list, detail, footer)` regions of a list-detail modal. The detail-pane
/// geometry is shared by the renderer and the scroll-clamp helper so they can't
/// drift.
pub(super) fn list_detail_regions(area: Rect, split: ListDetailSplit) -> (Rect, Rect, Rect) {
    let inner = theme::frame(String::new(), true).inner(area);
    match split {
        ListDetailSplit::Vertical => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Percentage(44),
                    Constraint::Min(3),
                    Constraint::Length(2),
                ])
                .split(inner);
            (chunks[0], chunks[1], chunks[2])
        }
        ListDetailSplit::Horizontal => {
            let body = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(2)])
                .split(inner);
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
                .split(body[0]);
            (columns[0], columns[1], body[1])
        }
    }
}

/// The inner rect of the detail pane's frame — where its content actually
/// wraps. Shared so the scroll clamp measures the same width the render draws.
pub(super) fn detail_pane_inner(detail_area: Rect) -> Rect {
    // The frame border is independent of its title and focus, so an empty
    // inactive frame yields the same inner rect the render uses.
    theme::frame(String::new(), false).inner(detail_area)
}

/// Render the framed, scrollable detail pane plus its scrollbar, clamping
/// `modal_scroll` to the content. The three list-detail modals share this so
/// the scroll/scrollbar plumbing lives in one place.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_detail_pane(
    f: &mut Frame,
    detail_area: Rect,
    title: &str,
    focused: bool,
    detail_lines: Vec<Line<'static>>,
    modal_scroll: u16,
    selection: Option<(usize, usize)>,
    body_cache: &std::cell::RefCell<Vec<Line<'static>>>,
    body_rect_cache: &std::cell::Cell<Option<Rect>>,
) {
    // Cache unwrapped detail lines and detail inner rect for mouse hit-testing.
    *body_cache.borrow_mut() = detail_lines.clone();

    let detail_frame = theme::frame(title, focused);
    let detail_inner = detail_frame.inner(detail_area);
    body_rect_cache.set(Some(detail_inner));
    let max_scroll = detail_max_scroll(detail_inner, &detail_lines);
    let scroll = modal_scroll.min(max_scroll);

    // Apply selection highlighting.
    let highlighted: Vec<Line<'static>>;
    let render_lines = if let Some((start, end)) = selection.filter(|(s, e)| s != e) {
        highlighted = modal_selection_highlight(&detail_lines, start, end);
        &highlighted
    } else {
        &detail_lines
    };

    // Pre-wrap at the pane width with the same char-granular `wrap_line` the
    // scroll metrics and the mouse position resolver use — ratatui's word
    // wrapper breaks at different columns, which would skew selection
    // hit-testing on wrapped lines.
    let wrap_width = detail_inner.width.max(1) as usize;
    let mut wrapped: Vec<Line<'static>> = Vec::new();
    for line in render_lines {
        if line.width() <= wrap_width {
            wrapped.push(line.clone());
        } else {
            wrapped.extend(wrap_line(line, wrap_width));
        }
    }

    f.render_widget(
        Paragraph::new(wrapped)
            .block(detail_frame)
            .style(theme::panel())
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        detail_area,
    );
    if max_scroll > 0 {
        let mut state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll as usize)
            .viewport_content_length(detail_inner.height.max(1) as usize);
        let scrollbar = theme::scrollbar(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, detail_inner, &mut state);
    }
}
