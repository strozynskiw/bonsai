use super::common::{ModalCaches, ModalScrollView};
use super::list_detail::{
    ListDetailSplit, detail_pane_inner, list_detail_panes, render_detail_pane,
};
use super::picker_common::*;
use super::*;
use crate::tui::mcp::{McpServerRow, McpServerStateKind};

/// Below this body width the list and the detail stack instead of sharing a row:
/// a three-column list squeezed beside a detail pane leaves neither readable.
const MCP_STACKED_BELOW_WIDTH: u16 = 72;

pub(super) fn render_mcp_servers(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    rows: &[McpServerRow],
    cursor: usize,
) {
    let header = mcp_header(rows);
    let footer = mcp_footer();
    // The shell owns the frame, the header gap and the pinned footer; mcp adds
    // only the list/detail panes below them.
    let shell = split_scrollable_modal_lines(area, &header, &footer);
    render_modal_chrome(f, area, "MCP Servers", &shell, &header, &footer);
    if shell.body.width == 0 || shell.body.height == 0 {
        return;
    }

    let (list_area, detail_area) = mcp_panes(shell.body);
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(modal_empty_body("No MCP servers detected."))
                .style(theme::panel())
                .wrap(Wrap { trim: false }),
            list_area,
        );
        return;
    }

    let cursor = cursor.min(rows.len().saturating_sub(1));
    f.render_widget(
        Paragraph::new(mcp_server_list_lines(rows, cursor, list_area))
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        list_area,
    );

    if detail_area.width > 0 && detail_area.height > 0 {
        render_detail_pane(
            f,
            detail_area,
            "Detail",
            true,
            mcp_detail_lines(&rows[cursor]),
            ModalScrollView {
                scroll: app.modal_scroll,
                selection: app.modal_selection.map(|s| s.range()),
            },
            ModalCaches {
                body: &app.modal_body_lines,
                body_rect: &app.modal_body_rect,
            },
        );
    }
}

pub(super) fn mcp_detail_max_scroll(area: Rect, rows: &[McpServerRow], cursor: usize) -> u16 {
    let Some(row) = rows.get(cursor.min(rows.len().saturating_sub(1))) else {
        return 0;
    };
    // Split exactly as the renderer does, then measure the framed pane's inner
    // rect: the clamp and the drawn body share one geometry.
    let shell = split_scrollable_modal_lines(area, &mcp_header(rows), &mcp_footer());
    let (_, detail_area) = mcp_panes(shell.body);
    if detail_area.width == 0 || detail_area.height == 0 {
        return 0;
    }
    detail_max_scroll(detail_pane_inner(detail_area), &mcp_detail_lines(row))
}

fn mcp_header(rows: &[McpServerRow]) -> Vec<Line<'static>> {
    vec![modal_subtitle(mcp_status_summary(rows))]
}

/// Rows the MCP manager needs: the shell chrome, the subtitle, the taller of the
/// two panes and the hint footer. Pane widths depend on the body width alone, so
/// they are known before the height is measured.
pub(super) fn mcp_servers_rows(area: Rect, rows: &[McpServerRow], cursor: usize) -> u16 {
    let header = mcp_header(rows);
    let footer = mcp_footer();
    let inner_width = ModalFootprint::Wide
        .width(area)
        .saturating_sub(MODAL_FRAME_COLUMNS);
    let width = inner_width.max(1) as usize;
    // The header and the six-pair footer wrap on a narrow terminal, and each costs
    // the popup a row when they do.
    let header_rows = wrapped_lines_height(&header, width) as u16;
    let footer_rows = wrapped_lines_height(&footer, width) as u16;
    if rows.is_empty() {
        return shell_modal_rows(header_rows, 1, footer_rows);
    }
    let (_, detail_area) = mcp_panes(Rect::new(0, 0, inner_width, 1));
    let detail_width = detail_pane_inner(detail_area).width.max(1) as usize;
    let cursor = cursor.min(rows.len().saturating_sub(1));
    let detail_rows = mcp_detail_lines(&rows[cursor])
        .iter()
        .map(|line| usize::from(wrapped_line_count(line, detail_width)))
        .sum::<usize>()
        // The detail pane draws its own frame inside the body.
        .saturating_add(2);
    let body_rows = rows.len().max(detail_rows).max(1) as u16;
    shell_modal_rows(header_rows, body_rows, footer_rows)
}

fn mcp_footer() -> Vec<Line<'static>> {
    vec![footer_hint_line(MCP_HINTS)]
}

fn mcp_status_summary(rows: &[McpServerRow]) -> String {
    let enabled = rows
        .iter()
        .filter(|row| row.state == McpServerStateKind::Enabled)
        .count();
    let methods: usize = rows.iter().map(|row| row.tools.len()).sum();
    format!(
        "{} · {enabled} enabled · {}",
        count_label(rows.len(), "server"),
        count_label(methods, "method"),
    )
}

/// The mcp panes: the shared list/detail split with MCP's own width policy on top
/// — the list carries three short columns, so columns past the cap belong to the
/// detail. Pure over the body rect, so the renderer and
/// [`mcp_detail_max_scroll`] cannot measure different panes.
fn mcp_panes(body: Rect) -> (Rect, Rect) {
    let horizontal = body.width >= MCP_STACKED_BELOW_WIDTH;
    let split = if horizontal {
        ListDetailSplit::Horizontal
    } else {
        ListDetailSplit::Vertical
    };
    let (list, detail) = list_detail_panes(body, split);
    let cap = body.width.saturating_sub(24).clamp(1, 42);
    if !horizontal || list.width <= cap {
        return (list, detail);
    }
    let surplus = list.width - cap;
    (
        Rect { width: cap, ..list },
        Rect {
            x: list.x + cap,
            width: detail.width.saturating_add(surplus),
            ..detail
        },
    )
}

fn mcp_server_list_lines(rows: &[McpServerRow], cursor: usize, area: Rect) -> Vec<Line<'static>> {
    let visible = area.height.max(1) as usize;
    visible_picker_rows(rows.len(), cursor, visible)
        .map(|index| mcp_server_line(index == cursor, &rows[index], area.width))
        .collect()
}

fn mcp_server_line(selected: bool, row: &McpServerRow, width: u16) -> Line<'static> {
    let width = width as usize;
    let marker = if selected { "> " } else { "  " };
    let state_width = 9usize.min(width.saturating_sub(2));
    let methods_width = 9usize.min(width.saturating_sub(2 + state_width));
    let name_width = width
        .saturating_sub(2)
        .saturating_sub(2)
        .saturating_sub(state_width)
        .saturating_sub(2)
        .saturating_sub(methods_width)
        .max(6);
    let enabled = row.state != McpServerStateKind::Disabled;
    let name_style = if selected {
        theme::body(theme::palette().border_active)
    } else if enabled {
        theme::body(theme::palette().text)
    } else {
        theme::dim()
    };
    Line::from(vec![
        Span::styled(marker, theme::muted()),
        Span::styled(pad_ascii(&row.name, name_width), name_style),
        Span::styled("  ", theme::dim()),
        Span::styled(
            pad_ascii(&row.state_label, state_width),
            state_style(row.state),
        ),
        Span::styled("  ", theme::dim()),
        Span::styled(
            pad_ascii(&method_count(row.tools.len()), methods_width),
            theme::dim(),
        ),
    ])
}

fn mcp_detail_lines(row: &McpServerRow) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                row.name.clone(),
                theme::body(theme::palette().border_active),
            ),
            Span::styled("  ", theme::dim()),
            Span::styled(row.state_label.clone(), state_style(row.state)),
            state_detail_span(row),
        ]),
        Line::from(vec![
            Span::styled("source ", theme::muted()),
            Span::styled(row.source.clone(), theme::body(theme::palette().text)),
            Span::styled("  risk ", theme::muted()),
            Span::styled(row.risk.clone(), theme::dim()),
            Span::styled("  batching ", theme::muted()),
            Span::styled(row.batching.clone(), theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("capabilities ", theme::muted()),
            Span::styled(row.capabilities.join(", "), theme::dim()),
        ]),
    ];

    if !row.detail.trim().is_empty() {
        lines.push(Line::from(vec![
            Span::styled("detail ", theme::muted()),
            Span::styled(row.detail.clone(), theme::dim()),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Methods", theme::body(theme::palette().text)),
        Span::styled(format!(" ({})", row.tools.len()), theme::dim()),
    ]));

    if row.tools.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No methods discovered.",
            theme::dim(),
        )));
        return lines;
    }

    for tool in &row.tools {
        let mut spans = vec![
            Span::styled("  ", theme::dim()),
            Span::styled(tool.name.clone(), theme::body(theme::palette().text)),
        ];
        if !tool.description.trim().is_empty() {
            spans.push(Span::styled("  ", theme::dim()));
            spans.push(Span::styled(tool.description.clone(), theme::dim()));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn state_detail_span(row: &McpServerRow) -> Span<'static> {
    row.state_detail
        .as_ref()
        .map(|detail| Span::styled(format!("  {detail}"), theme::dim()))
        .unwrap_or_else(|| Span::raw(""))
}

fn method_count(count: usize) -> String {
    count_label(count, "method")
}

fn count_label(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

fn state_style(state: McpServerStateKind) -> Style {
    let palette = theme::palette();
    match state {
        McpServerStateKind::Enabled => theme::body(palette.success),
        McpServerStateKind::Disabled => theme::dim(),
        McpServerStateKind::Failed => theme::body(palette.error),
        McpServerStateKind::Degraded => theme::body(palette.progress),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::mcp::McpToolRow;
    use ratatui::{Terminal, backend::TestBackend};

    fn row_with_tools(count: usize) -> McpServerRow {
        McpServerRow {
            name: "filesystem".to_string(),
            state: McpServerStateKind::Enabled,
            state_label: "enabled".to_string(),
            state_detail: None,
            source: "project".to_string(),
            capabilities: vec!["tools".to_string()],
            batching: "serialized-access".to_string(),
            risk: "read + write".to_string(),
            detail: String::new(),
            tools: (0..count)
                .map(|index| McpToolRow {
                    name: format!("tool-{index:02}"),
                    description: "does a thing".to_string(),
                })
                .collect(),
        }
    }

    fn render(app: &AppState, area: Rect, rows: &[McpServerRow]) -> String {
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        terminal
            .draw(|frame| render_mcp_servers(frame, area, app, rows, 0))
            .expect("mcp modal should render");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn narrow_footers_keep_the_close_hint() {
        // A 90x20 terminal gives the wide tier an 82x18 popup; the hint row is
        // wider than the frame there, so it has to wrap rather than clip.
        let area = Rect::new(0, 0, 82, 18);
        let rows = vec![row_with_tools(2)];
        let app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        let text = render(&app, area, &rows);
        assert!(
            text.contains("Esc"),
            "the footer keeps its close key: {text}"
        );
        assert!(
            text.contains("close"),
            "the footer keeps its close label: {text}"
        );
    }

    #[test]
    fn detail_scroll_measures_the_framed_pane_it_draws() {
        let area = Rect::new(0, 0, 100, 20);
        let rows = vec![row_with_tools(20)];
        let max_scroll = mcp_detail_max_scroll(area, &rows, 0);
        assert!(max_scroll > 0, "a long method list scrolls");

        let mut app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        let top = render(&app, area, &rows);
        assert!(top.contains("tool-00"), "the first method draws: {top}");
        assert!(
            !top.contains("tool-19"),
            "the last method starts below the fold: {top}"
        );

        app.modal_scroll = max_scroll;
        let bottom = render(&app, area, &rows);
        assert!(
            bottom.contains("tool-19"),
            "the reported clamp reaches the last method: {bottom}"
        );
    }

    #[test]
    fn narrow_bodies_stack_the_list_above_the_detail() {
        let stacked = mcp_panes(Rect::new(0, 0, 60, 20));
        assert!(
            stacked.1.y > stacked.0.y,
            "the detail sits below the list: {stacked:?}"
        );
        assert_eq!(stacked.0.width, 60, "the stacked list spans the body");

        let side_by_side = mcp_panes(Rect::new(0, 0, 120, 20));
        assert_eq!(
            side_by_side.0.y, side_by_side.1.y,
            "wide bodies share one row: {side_by_side:?}"
        );
        assert!(
            side_by_side.0.width <= 42,
            "the list keeps its column cap: {side_by_side:?}"
        );
        assert_eq!(
            side_by_side.0.width + side_by_side.1.width,
            120,
            "the panes still fill the body"
        );
    }

    #[test]
    fn an_empty_registry_states_it_in_the_list_pane() {
        let area = Rect::new(0, 0, 90, 16);
        let app = AppState::new("codex", "m".to_string(), ".".to_string(), None);
        let text = render(&app, area, &[]);
        assert!(text.contains("No MCP servers detected."), "{text}");
        assert_eq!(mcp_detail_max_scroll(area, &[], 0), 0);
    }
}
