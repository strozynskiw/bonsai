use super::picker_common::*;
use super::*;
use crate::tui::mcp::{McpServerRow, McpServerStateKind};

struct McpRegions {
    header: Rect,
    body: Rect,
    list: Rect,
    detail: Rect,
    footer: Rect,
}

pub(super) fn render_mcp_servers(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    rows: &[McpServerRow],
    cursor: usize,
) {
    let panel = theme::frame("MCP Servers", true).style(theme::panel());
    f.render_widget(Clear, area);
    f.render_widget(panel, area);

    let regions = mcp_regions(area);
    if regions.body.width == 0 || regions.body.height == 0 {
        return;
    }

    if regions.header.height > 0 {
        f.render_widget(
            Paragraph::new(mcp_header_lines(rows))
                .style(theme::panel())
                .wrap(Wrap { trim: false }),
            regions.header,
        );
    }

    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                "No MCP servers detected.",
                theme::dim(),
            ))])
            .style(theme::panel()),
            regions.body,
        );
        render_mcp_footer(f, regions.footer);
        return;
    }

    let cursor = cursor.min(rows.len().saturating_sub(1));
    f.render_widget(
        Paragraph::new(mcp_server_list_lines(rows, cursor, regions.list))
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        regions.list,
    );

    if regions.detail.width > 0 && regions.detail.height > 0 {
        render_mcp_detail(f, regions.detail, app, &rows[cursor]);
    }

    render_mcp_footer(f, regions.footer);
}

pub(super) fn mcp_detail_max_scroll(area: Rect, rows: &[McpServerRow], cursor: usize) -> u16 {
    let Some(row) = rows.get(cursor.min(rows.len().saturating_sub(1))) else {
        return 0;
    };
    let detail = mcp_regions(area).detail;
    if detail.width == 0 || detail.height == 0 {
        return 0;
    }
    detail_max_scroll(detail, &mcp_detail_lines(row))
}

fn mcp_regions(area: Rect) -> McpRegions {
    let inner = theme::frame("MCP Servers", true).inner(area);
    if inner.width == 0 || inner.height == 0 {
        return McpRegions {
            header: Rect::default(),
            body: Rect::default(),
            list: Rect::default(),
            detail: Rect::default(),
            footer: Rect::default(),
        };
    }

    let footer_height = 2.min(inner.height);
    let header_height = if inner.height > footer_height + 3 {
        2
    } else {
        0
    };
    let vertical = if header_height > 0 {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header_height),
                Constraint::Min(1),
                Constraint::Length(footer_height),
            ])
            .split(inner)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(footer_height)])
            .split(inner)
    };

    let (header, body, footer) = if header_height > 0 {
        (vertical[0], vertical[1], vertical[2])
    } else {
        (Rect::default(), vertical[0], vertical[1])
    };
    let (list, detail) = split_mcp_body(body);

    McpRegions {
        header,
        body,
        list,
        detail,
        footer,
    }
}

fn split_mcp_body(body: Rect) -> (Rect, Rect) {
    if body.width >= 72 {
        let max_list_width = body.width.saturating_sub(24).clamp(1, 42);
        let min_list_width = 28.min(max_list_width);
        let proposed = body.width.saturating_mul(34) / 100;
        let list_width = proposed.clamp(min_list_width, max_list_width);
        let gap_width = body.width.saturating_sub(list_width).min(2);
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(list_width),
                Constraint::Length(gap_width),
                Constraint::Min(1),
            ])
            .split(body);
        (chunks[0], chunks[2])
    } else {
        let gap_height = body.height.saturating_sub(1).min(1);
        let max_list_height = body.height.saturating_sub(4).clamp(1, 10);
        let min_list_height = 3.min(max_list_height);
        let proposed = body.height / 2;
        let list_height = proposed.clamp(min_list_height, max_list_height);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(list_height),
                Constraint::Length(gap_height),
                Constraint::Min(1),
            ])
            .split(body);
        (chunks[0], chunks[2])
    }
}

fn mcp_header_lines(rows: &[McpServerRow]) -> Vec<Line<'static>> {
    let enabled = rows
        .iter()
        .filter(|row| row.state == McpServerStateKind::Enabled)
        .count();
    let methods: usize = rows.iter().map(|row| row.tools.len()).sum();
    vec![
        Line::from(vec![
            Span::styled(
                count_label(rows.len(), "server"),
                theme::body(theme::palette().text),
            ),
            Span::styled("  ", theme::dim()),
            Span::styled(
                format!("{enabled} enabled"),
                theme::body(theme::palette().success),
            ),
            Span::styled("  ", theme::dim()),
            Span::styled(count_label(methods, "method"), theme::dim()),
        ]),
        Line::from(""),
    ]
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

fn render_mcp_detail(f: &mut Frame, area: Rect, app: &AppState, row: &McpServerRow) {
    let lines = mcp_detail_lines(row);
    let max_scroll = detail_max_scroll(area, &lines);
    let scroll = app.modal_scroll.min(max_scroll);

    // Cache the unwrapped detail lines and pane rect so mouse clicks resolve
    // to text positions for selection/copy.
    *app.modal_body_lines.borrow_mut() = lines.clone();
    app.modal_body_rect.set(Some(area));

    let highlighted: Vec<Line<'static>>;
    let render_lines = if let Some((start, end)) = app
        .modal_selection
        .map(|s| s.range())
        .filter(|(s, e)| s != e)
    {
        highlighted = modal_selection_highlight(&lines, start, end);
        &highlighted
    } else {
        &lines
    };
    // Pre-wrap char-granular (like the shared detail pane) so the on-screen
    // rows match the selection resolver and the scroll metric.
    let wrap_width = area.width.max(1) as usize;
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
            .style(theme::panel())
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );

    if max_scroll > 0 {
        let mut state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll as usize)
            .viewport_content_length(area.height.max(1) as usize);
        let scrollbar = theme::scrollbar(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, area, &mut state);
    }
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

fn render_mcp_footer(f: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let hints = footer_hint_line(&[
        ("Up/Down", "servers"),
        ("Space", "toggle"),
        ("r", "reload"),
        ("a", "authorize"),
        ("PgUp/PgDn", "methods"),
        ("Esc", "close"),
    ]);
    let footer = if area.height == 1 {
        vec![hints]
    } else {
        vec![Line::from(""), hints]
    };
    f.render_widget(Paragraph::new(footer).style(theme::panel()), area);
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
