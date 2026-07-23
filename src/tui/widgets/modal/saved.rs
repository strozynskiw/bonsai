use super::picker_common::*;
use super::*;

pub(super) fn render_session_picker(
    f: &mut Frame,
    area: Rect,
    sessions: &[crate::storage::SessionSummary],
    cursor: usize,
) {
    let footer = vec![
        Line::from(Span::styled("Current project only", theme::dim())),
        footer_hint_line(&[
            ("Up/Down", "move"),
            ("PgUp/PgDn", "page"),
            ("Enter", "resume"),
            ("Del", "delete"),
            ("Esc", "close"),
        ]),
    ];
    render_list_picker(f, area, "Sessions", &[], &footer, |body_area| {
        let columns = session_columns();
        let widths = table_widths(body_area.width as usize, sessions, &columns);
        let mut lines = vec![table_header(&columns, &widths)];
        if sessions.is_empty() {
            lines.push(Line::from(Span::styled("No prior sessions", theme::dim())));
        } else {
            let cursor = cursor.min(sessions.len().saturating_sub(1));
            let visible_count = body_area.height.saturating_sub(1).max(1) as usize;
            lines.extend(
                visible_picker_rows(sessions.len(), cursor, visible_count)
                    .map(|index| table_row(&sessions[index], &columns, &widths, index == cursor)),
            );
        }
        lines
    });
}

pub(super) fn render_plan_picker(
    f: &mut Frame,
    area: Rect,
    plans: &[crate::storage::SavedPlanSummary],
    query: &str,
    cursor: usize,
) {
    let header = vec![Line::from(vec![
        Span::styled("Search: ", theme::muted()),
        Span::styled(query.to_string(), theme::body(theme::palette().text)),
    ])];
    let footer = vec![
        Line::from(Span::styled("Current project only", theme::dim())),
        footer_hint_line(&[
            ("Type", "search"),
            ("Up/Down", "move"),
            ("Enter", "open"),
            ("Del", "delete"),
            ("Esc", "close"),
        ]),
    ];

    let filtered = AppState::plan_picker_filtered_plans(plans, query);
    render_list_picker(f, area, "Plans", &header, &footer, |body_area| {
        let columns = plan_columns();
        let widths = table_widths(body_area.width as usize, plans, &columns);
        let mut lines = vec![table_header(&columns, &widths)];
        if filtered.is_empty() {
            let empty = if plans.is_empty() {
                "No saved plans"
            } else {
                "No matching plans"
            };
            lines.push(Line::from(Span::styled(empty, theme::dim())));
        } else {
            let cursor = cursor.min(filtered.len().saturating_sub(1));
            let visible_count = body_area.height.saturating_sub(1).max(1) as usize;
            lines.extend(
                visible_picker_rows(filtered.len(), cursor, visible_count)
                    .map(|index| table_row(filtered[index], &columns, &widths, index == cursor)),
            );
        }
        lines
    });
}

pub(super) fn render_plan_open_choice(
    f: &mut Frame,
    area: Rect,
    plan: &crate::storage::SavedPlanSummary,
    cursor: usize,
) {
    let panel = theme::frame("Open Plan", true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(inner);

    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                plan.title.clone(),
                theme::body(theme::palette().text),
            )),
            Line::from(Span::styled(
                match plan.source_session_id {
                    Some(source) => format!("Saved from session #{source}"),
                    None => "Source session no longer exists".to_string(),
                },
                theme::dim(),
            )),
        ])
        .wrap(Wrap { trim: false }),
        chunks[0],
    );

    let options = [
        PlanOpenMode::CleanContext,
        PlanOpenMode::ResumeSourceSession,
    ];
    let cursor = cursor.min(options.len().saturating_sub(1));
    let lines = options
        .iter()
        .enumerate()
        .map(|(idx, mode)| {
            picker_line(
                idx == cursor,
                mode.label().to_string(),
                Some(mode.description().to_string()),
                true,
            )
        })
        .collect::<Vec<_>>();
    f.render_widget(
        Paragraph::new(lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        chunks[1],
    );

    f.render_widget(
        Paragraph::new(footer_hint_line(&[("Enter", "open"), ("Esc", "cancel")])),
        chunks[2],
    );
}

pub(super) fn render_start_plan_choice(f: &mut Frame, area: Rect, cursor: usize) {
    let panel = theme::frame("Start Plan", true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(inner);
    f.render_widget(
        Paragraph::new("Choose the context for implementation.").style(theme::dim()),
        chunks[0],
    );
    let options = [StartPlanMode::CleanContext, StartPlanMode::KeepContext];
    let cursor = cursor.min(options.len().saturating_sub(1));
    let lines = options
        .iter()
        .enumerate()
        .map(|(idx, mode)| {
            picker_line(
                idx == cursor,
                mode.label().to_string(),
                Some(mode.description().to_string()),
                true,
            )
        })
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(lines).style(theme::panel()), chunks[1]);
    f.render_widget(
        Paragraph::new(footer_hint_line(&[("Enter", "start"), ("Esc", "cancel")])),
        chunks[2],
    );
}

pub(super) fn render_plan_delete_confirm(
    f: &mut Frame,
    area: Rect,
    plan: &crate::storage::SavedPlanSummary,
) {
    let panel = theme::frame("Delete Plan", true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Delete \"{}\" from the saved plan library?", plan.title),
                theme::body(theme::palette().text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "The source and execution session transcripts stay on disk.",
                theme::dim(),
            )),
        ])
        .style(theme::panel())
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(footer_hint_line(&[
            ("Enter/Y", "delete"),
            ("Esc/N", "cancel"),
        ])),
        chunks[1],
    );
}

pub(super) fn render_session_delete_confirm(
    f: &mut Frame,
    area: Rect,
    session: &crate::storage::SessionSummary,
) {
    let panel = theme::frame("Delete Session", true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Delete session \"{}\"?", session.name),
                theme::body(theme::palette().text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "The session transcript and all its data are permanently deleted.",
                theme::dim(),
            )),
        ])
        .style(theme::panel())
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(footer_hint_line(&[
            ("Enter/Y", "delete"),
            ("Esc/N", "cancel"),
        ])),
        chunks[1],
    );
}

pub(super) fn render_plan_discard_confirm(
    f: &mut Frame,
    area: Rect,
    saved_plan_id: crate::storage::SavedPlanId,
    title: &str,
) {
    let panel = theme::frame("Discard Plan", true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Discard \"{title}\" and clear the canvas?"),
                theme::body(theme::palette().text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!("This also deletes saved plan #{saved_plan_id} from the library."),
                theme::dim(),
            )),
        ])
        .style(theme::panel())
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(footer_hint_line(&[
            ("Enter/Y", "discard"),
            ("Esc/N", "cancel"),
        ])),
        chunks[1],
    );
}

fn session_columns() -> Vec<TableColumn<crate::storage::SessionSummary>> {
    vec![
        TableColumn {
            header: "ID",
            width: TableWidth::Fit { cap: usize::MAX },
            render: CellRender::Value(|session| session.id.to_string()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Session",
            width: TableWidth::Flex { min: 6 },
            render: CellRender::Value(session_display_name),
            style: CellStyle::Text,
        },
        TableColumn {
            header: "Updated",
            width: TableWidth::Exact(8),
            render: CellRender::Value(|session| relative_age(session.updated_at_ms)),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Status",
            width: TableWidth::Fit { cap: 12 },
            render: CellRender::Value(|session| session.status.label().to_string()),
            style: CellStyle::Custom(|session| session_status_style(&session.status)),
        },
        TableColumn {
            header: "Msgs",
            width: TableWidth::Fit { cap: usize::MAX },
            render: CellRender::Value(|session| session.message_count.to_string()),
            style: CellStyle::Meta,
        },
    ]
}

pub(super) fn session_title(session: &crate::storage::SessionSummary) -> String {
    let summary = session.summary.trim();
    if summary.is_empty() {
        let name = session.name.trim();
        if name.is_empty() {
            format!("Untitled session #{}", session.id)
        } else {
            name.to_string()
        }
    } else {
        summary.to_string()
    }
}

pub(super) fn session_display_name(session: &crate::storage::SessionSummary) -> String {
    session_title(session)
}

fn plan_columns() -> Vec<TableColumn<crate::storage::SavedPlanSummary>> {
    vec![
        TableColumn {
            header: "ID",
            width: TableWidth::Fit { cap: usize::MAX },
            render: CellRender::Value(|plan| plan.id.to_string()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Plan",
            width: TableWidth::Flex { min: 8 },
            render: CellRender::Value(|plan| plan.title.clone()),
            style: CellStyle::Text,
        },
        TableColumn {
            header: "Branch",
            width: TableWidth::Fit { cap: 16 },
            render: CellRender::Value(|plan| plan.branch.as_deref().unwrap_or("-").to_string()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Updated",
            width: TableWidth::Exact(8),
            render: CellRender::Value(|plan| relative_age(plan.updated_at_ms)),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Status",
            width: TableWidth::Fit { cap: 12 },
            render: CellRender::Value(|plan| plan.status.label().to_string()),
            style: CellStyle::Custom(|plan| plan_status_style(&plan.status)),
        },
        TableColumn {
            header: "Items",
            width: TableWidth::Fit { cap: 9 },
            render: CellRender::Value(plan_item_label),
            style: CellStyle::Meta,
        },
    ]
}

pub(super) fn plan_item_label(plan: &crate::storage::SavedPlanSummary) -> String {
    format!("{}s/{}t", plan.section_count, plan.task_count)
}
