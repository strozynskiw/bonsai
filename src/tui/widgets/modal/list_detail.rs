use super::common::*;
use super::*;

/// How a list-plus-detail modal arranges its two panes.
#[derive(Debug, Clone, Copy)]
pub(super) enum ListDetailSplit {
    /// List table on top, detail below (tasks, subagents).
    Vertical,
    /// List on the left, detail on the right (skills).
    Horizontal,
    /// Prioritize the source table, stacking compact details on narrow screens.
    Refresh,
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
    let regions = list_detail_shell_regions(area);
    render_modal_chrome(f, area, modal.title, &regions, &[], &modal.footer_lines);
    if regions.body.height == 0 || regions.body.width == 0 {
        return;
    }

    let (list_area, detail_area) = list_detail_panes(regions.body, modal.split);
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
}

/// Footer hint rows every list/detail modal pins below its panes. Measurement
/// helpers split with this same constant, so the scroll clamp always measures
/// the pane the renderer draws.
pub(super) const LIST_DETAIL_FOOTER_LINES: usize = 1;

/// The shell geometry of a list/detail modal: the frame and the pinned footer,
/// with the panes owning the body.
pub(super) fn list_detail_shell_regions(area: Rect) -> ScrollableRegions {
    split_scrollable_modal(area, 0, LIST_DETAIL_FOOTER_LINES)
}

/// The `(list, detail, footer)` regions of a list-detail modal. Both the
/// renderer and the scroll-clamp helpers derive their panes here so they cannot
/// drift.
pub(super) fn list_detail_regions(area: Rect, split: ListDetailSplit) -> (Rect, Rect, Rect) {
    let regions = list_detail_shell_regions(area);
    let (list, detail) = list_detail_panes(regions.body, split);
    (list, detail, regions.footer)
}

/// Split the shell body into the list and detail panes.
pub(super) fn list_detail_panes(body: Rect, split: ListDetailSplit) -> (Rect, Rect) {
    let split_layout = |constraints: Vec<Constraint>, direction: Direction| {
        let chunks = Layout::default()
            .direction(direction)
            .constraints(constraints)
            .split(body);
        (chunks[0], chunks[1])
    };
    match split {
        ListDetailSplit::Vertical => split_layout(
            vec![Constraint::Percentage(44), Constraint::Min(3)],
            Direction::Vertical,
        ),
        ListDetailSplit::Horizontal => split_layout(
            vec![Constraint::Percentage(42), Constraint::Percentage(58)],
            Direction::Horizontal,
        ),
        ListDetailSplit::Refresh => {
            if body.width >= 90 {
                split_layout(
                    vec![Constraint::Percentage(65), Constraint::Percentage(35)],
                    Direction::Horizontal,
                )
            } else {
                split_layout(
                    vec![Constraint::Min(3), Constraint::Length(6)],
                    Direction::Vertical,
                )
            }
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

    // The pane frame, then the shared body renderer: wrapping, the scroll clamp
    // and the scrollbar are the same code the shell body uses, so the two can no
    // longer disagree.
    f.render_widget(detail_frame, detail_area);
    render_scroll_body(f, detail_inner, &detail_lines, selection, modal_scroll);
}
