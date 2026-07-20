use super::common::*;
use super::picker_common::*;
use super::*;

pub(super) fn render_task_list(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    tasks: &[BackgroundTaskSnapshot],
    cursor: usize,
) {
    let status = app
        .task_list_status
        .clone()
        .unwrap_or_else(|| "Del stops and removes the selected task.".to_string());
    let footer_lines = vec![
        Line::from(Span::styled(status, theme::dim())),
        footer_hint_line(&[
            ("Up/Down", "move"),
            ("PgUp/PgDn", "page"),
            ("Del", "remove"),
            ("Esc", "close"),
        ]),
    ];
    render_list_detail_modal(
        f,
        area,
        app,
        ListDetailModal {
            title: "Background Tasks",
            detail_title: "Selected Task",
            split: ListDetailSplit::Vertical,
            detail_focused: false,
            footer_lines,
            modal_scroll: app.modal_scroll,
        },
        |frame, table_area, _detail_area| {
            let columns = task_columns();
            let widths = table_widths(table_area.width as usize, tasks, &columns);
            let mut table_lines = vec![table_header(&columns, &widths)];
            if tasks.is_empty() {
                table_lines.push(Line::from(Span::styled(
                    "No background tasks",
                    theme::dim(),
                )));
            } else {
                let cursor = cursor.min(tasks.len().saturating_sub(1));
                let visible_count = table_area.height.saturating_sub(1).max(1) as usize;
                table_lines.extend(
                    visible_picker_rows(tasks.len(), cursor, visible_count)
                        .map(|index| table_row(&tasks[index], &columns, &widths, index == cursor)),
                );
            }
            frame.render_widget(
                Paragraph::new(table_lines)
                    .style(theme::panel())
                    .wrap(Wrap { trim: false }),
                table_area,
            );
            tasks
                .get(cursor.min(tasks.len().saturating_sub(1)))
                .map(task_detail_lines)
                .unwrap_or_else(|| vec![Line::from(Span::styled("No task selected", theme::dim()))])
        },
    );
}

fn task_columns() -> Vec<TableColumn<BackgroundTaskSnapshot>> {
    vec![
        TableColumn {
            header: "ID",
            width: TableWidth::Fit { cap: usize::MAX },
            render: CellRender::Value(|task| task.id.clone()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "State",
            width: TableWidth::Fit { cap: 10 },
            render: CellRender::Value(|task| task.status.label().to_string()),
            style: CellStyle::Custom(|task| status_style(task_status_color(task.status))),
        },
        TableColumn {
            header: "Duration",
            width: TableWidth::Fit { cap: 8 },
            render: CellRender::Value(|task| task.duration_label()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Exit/timeout",
            width: TableWidth::Fit { cap: 16 },
            render: CellRender::Value(|task| task.exit_or_timeout_label()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Output",
            width: TableWidth::Fit { cap: 12 },
            render: CellRender::Value(|task| task.output_size_label()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Command",
            width: TableWidth::Flex { min: 0 },
            render: CellRender::Fitted(|task, width| task.compact_command(width)),
            style: CellStyle::Text,
        },
    ]
}

pub(super) fn task_status_color(status: BackgroundTaskStatus) -> Color {
    match status {
        BackgroundTaskStatus::Running => theme::palette().todo,
        BackgroundTaskStatus::Succeeded => theme::palette().success,
        BackgroundTaskStatus::Failed | BackgroundTaskStatus::TimedOut => theme::palette().error,
        BackgroundTaskStatus::Stopped => theme::palette().muted,
    }
}

pub(super) fn task_detail_lines(task: &BackgroundTaskSnapshot) -> Vec<Line<'static>> {
    task.detail()
        .lines()
        .map(|line| {
            if line.is_empty() {
                Line::from("")
            } else if let Some((label, value)) = line.split_once(':') {
                Line::from(vec![
                    Span::styled(format!("{label:<13}"), theme::muted()),
                    Span::styled(
                        value.trim_start().to_string(),
                        theme::body(theme::palette().text),
                    ),
                ])
            } else {
                Line::from(Span::styled(
                    line.to_string(),
                    theme::body(theme::palette().text),
                ))
            }
        })
        .collect()
}

pub(super) fn max_task_detail_scroll(
    area: Rect,
    tasks: &[BackgroundTaskSnapshot],
    cursor: usize,
) -> u16 {
    let Some(task) = tasks.get(cursor.min(tasks.len().saturating_sub(1))) else {
        return 0;
    };
    let (_, detail_area, _) = list_detail_regions(area, ListDetailSplit::Vertical);
    detail_max_scroll(detail_pane_inner(detail_area), &task_detail_lines(task))
}
