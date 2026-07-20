use crate::commands::{RefreshSourceState, RefreshSourceStatus};
use crate::tui::theme;

use super::common::*;
use super::picker_common::*;
use super::*;

/// `/refresh` — a live per-source status modal. Each row shows a colored
/// status dot (yellow while pending, green on success, red on failure), the
/// source name, model count, and added/removed deltas. The selected row's
/// added/removed model ids are shown in the detail pane below.
pub(super) fn render_refresh(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    sources: &[RefreshSourceState],
    cursor: usize,
) {
    let all_done = !sources.is_empty()
        && sources
            .iter()
            .all(|s| !matches!(s.status, RefreshSourceStatus::Pending));
    let status_line = if all_done {
        "Refresh complete. Esc or Enter to close."
    } else {
        "Refreshing…"
    };
    let footer_lines = vec![
        Line::from(Span::styled(status_line, theme::dim())),
        footer_hint_line(&[("Up/Down", "move"), ("Esc/Enter", "close")]),
    ];

    render_list_detail_modal(
        f,
        area,
        app,
        ListDetailModal {
            title: "Refresh Model Catalogs",
            detail_title: "Model Changes",
            split: ListDetailSplit::Vertical,
            detail_focused: false,
            footer_lines,
            modal_scroll: app.modal_scroll,
        },
        |frame, table_area, _detail_area| {
            let columns = refresh_columns();
            let widths = table_widths(table_area.width as usize, sources, &columns);
            let mut table_lines = vec![table_header(&columns, &widths)];
            if sources.is_empty() {
                table_lines.push(Line::from(Span::styled(
                    "No sources to refresh",
                    theme::dim(),
                )));
            } else {
                let cursor = cursor.min(sources.len().saturating_sub(1));
                let visible_count = table_area.height.saturating_sub(1).max(1) as usize;
                table_lines.extend(
                    visible_picker_rows(sources.len(), cursor, visible_count).map(|index| {
                        table_row(&sources[index], &columns, &widths, index == cursor)
                    }),
                );
            }
            frame.render_widget(
                Paragraph::new(table_lines)
                    .style(theme::panel())
                    .wrap(Wrap { trim: false }),
                table_area,
            );
            sources
                .get(cursor.min(sources.len().saturating_sub(1)))
                .map(refresh_detail_lines)
                .unwrap_or_else(|| {
                    vec![Line::from(Span::styled("No source selected", theme::dim()))]
                })
        },
    );
}

fn refresh_columns() -> Vec<TableColumn<RefreshSourceState>> {
    vec![
        TableColumn {
            header: "",
            width: TableWidth::Fit { cap: 3 },
            render: CellRender::Value(|source| status_dot(&source.status).to_string()),
            style: CellStyle::Custom(|source| status_style(source_status_color(&source.status))),
        },
        TableColumn {
            header: "Source",
            width: TableWidth::Fit { cap: 20 },
            render: CellRender::Value(|source| source.display_name.clone()),
            style: CellStyle::Text,
        },
        TableColumn {
            header: "Status",
            width: TableWidth::Fit { cap: 10 },
            render: CellRender::Value(|source| status_label(&source.status).to_string()),
            style: CellStyle::Custom(|source| status_style(source_status_color(&source.status))),
        },
        TableColumn {
            header: "Models",
            width: TableWidth::Fit { cap: 10 },
            render: CellRender::Value(|source| {
                source
                    .model_count
                    .map(|n| n.to_string())
                    .unwrap_or_default()
            }),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Changes",
            width: TableWidth::Flex { min: 0 },
            render: CellRender::Value(changes_label),
            style: CellStyle::Meta,
        },
    ]
}

fn status_dot(status: &RefreshSourceStatus) -> &'static str {
    match status {
        RefreshSourceStatus::Pending => "●",
        RefreshSourceStatus::Ok => "●",
        RefreshSourceStatus::Failed(_) => "●",
    }
}

fn status_label(status: &RefreshSourceStatus) -> &'static str {
    match status {
        RefreshSourceStatus::Pending => "pending",
        RefreshSourceStatus::Ok => "ok",
        RefreshSourceStatus::Failed(_) => "failed",
    }
}

fn changes_label(source: &RefreshSourceState) -> String {
    let added = source.added.len();
    let removed = source.removed.len();
    if added == 0 && removed == 0 {
        return String::new();
    }
    let mut parts = Vec::new();
    if added > 0 {
        parts.push(format!("+{added}"));
    }
    if removed > 0 {
        parts.push(format!("−{removed}"));
    }
    parts.join(" ")
}

fn source_status_color(status: &RefreshSourceStatus) -> Color {
    match status {
        RefreshSourceStatus::Pending => theme::palette().todo,
        RefreshSourceStatus::Ok => theme::palette().success,
        RefreshSourceStatus::Failed(_) => theme::palette().error,
    }
}

fn refresh_detail_lines(source: &RefreshSourceState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if let RefreshSourceStatus::Failed(reason) = &source.status {
        lines.push(Line::from(vec![
            Span::styled("Error:", theme::muted()),
            Span::styled(" ", theme::dim()),
            Span::styled(reason.clone(), theme::body(theme::palette().error)),
        ]));
        lines.push(Line::from(""));
    }

    if source.added.is_empty() && source.removed.is_empty() {
        lines.push(Line::from(Span::styled("No model changes", theme::dim())));
        return lines;
    }

    if !source.added.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("Added ({}):", source.added.len()),
            theme::body(theme::palette().success),
        )));
        for model in &source.added {
            lines.push(Line::from(Span::styled(
                format!("  + {model}"),
                theme::body(theme::palette().success),
            )));
        }
    }

    if !source.removed.is_empty() {
        if !source.added.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!("Removed ({}):", source.removed.len()),
            theme::body(theme::palette().error),
        )));
        for model in &source.removed {
            lines.push(Line::from(Span::styled(
                format!("  - {model}"),
                theme::dim(),
            )));
        }
    }

    lines
}

pub(super) fn max_refresh_detail_scroll(
    area: Rect,
    sources: &[RefreshSourceState],
    cursor: usize,
) -> u16 {
    let Some(source) = sources.get(cursor.min(sources.len().saturating_sub(1))) else {
        return 0;
    };
    let (_, detail_area, _) = list_detail_regions(area, ListDetailSplit::Vertical);
    detail_max_scroll(
        detail_pane_inner(detail_area),
        &refresh_detail_lines(source),
    )
}
