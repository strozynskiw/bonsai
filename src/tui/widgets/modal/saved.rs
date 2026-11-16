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
        footer_hint_line(SESSION_PICKER_HINTS),
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
            lines.extend(table_body_lines(
                &sessions.iter().collect::<Vec<_>>(),
                &columns,
                &widths,
                cursor,
                visible_count,
            ));
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
    let header = vec![modal_search_header(query, false)];
    let footer = vec![
        Line::from(Span::styled("Current project only", theme::dim())),
        footer_hint_line(PLAN_PICKER_HINTS),
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
            lines.extend(table_body_lines(
                &filtered,
                &columns,
                &widths,
                cursor,
                visible_count,
            ));
        }
        lines
    });
}

/// Max scroll the shell clamp needs for a decision popup body, so a decision the
/// terminal is too short for is still reachable (wheel / page keys).
pub(super) fn decision_max_scroll(
    modal: Rect,
    layout: (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>),
) -> u16 {
    let (header, body, footer) = layout;
    let regions = split_scrollable_modal_lines(modal, &header, &footer);
    detail_max_scroll(regions.body, &body)
}

/// Draw a decision popup through the shared shell.
pub(super) fn render_decision_popup(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    title: &str,
    layout: (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>),
) {
    let (header, body, footer) = layout;
    render_scrollable_modal(
        f,
        area,
        title,
        &header,
        &body,
        &footer,
        app.modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
}

/// Header, body and footer of the Open-Plan choice.
pub(super) fn plan_open_choice_layout(
    plan: &crate::storage::SavedPlanSummary,
    cursor: usize,
    inner_width: u16,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let source = match plan.source_session_id {
        Some(source) => format!("Saved from session #{source}"),
        None => "Source session no longer exists".to_string(),
    };
    let header = vec![
        modal_subtitle(format!("{} · {source}", plan.title)),
        modal_heading("How should this plan be opened?"),
    ];
    let options = [
        PlanOpenMode::CleanContext,
        PlanOpenMode::ResumeSourceSession,
    ]
    .iter()
    .map(|mode| (mode.label().to_string(), mode.description().to_string()))
    .collect::<Vec<_>>();
    let cursor = cursor.min(options.len().saturating_sub(1));
    let body = decision_body(&options, cursor, inner_width);
    let footer = vec![footer_hint_line(PLAN_OPEN_CHOICE_HINTS)];
    (header, body, footer)
}

pub(super) fn plan_open_choice_rows(
    area: Rect,
    plan: &crate::storage::SavedPlanSummary,
    cursor: usize,
) -> u16 {
    let inner_width = decision_inner_width(area);
    let (header, body, footer) = plan_open_choice_layout(plan, cursor, inner_width);
    decision_rows(&header, &body, &footer, inner_width)
}

pub(super) fn render_plan_open_choice(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    plan: &crate::storage::SavedPlanSummary,
    cursor: usize,
) {
    let layout = plan_open_choice_layout(plan, cursor, decision_inner_width(area));
    render_decision_popup(f, area, app, "Open Plan", layout);
}

/// Header, body and footer of the Start-Plan choice.
pub(super) fn start_plan_choice_layout(
    cursor: usize,
    inner_width: u16,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let header = vec![
        modal_subtitle("The plan canvas is ready to implement."),
        modal_heading("Which context should implementation use?"),
    ];
    let options = [StartPlanMode::CleanContext, StartPlanMode::KeepContext]
        .iter()
        .map(|mode| (mode.label().to_string(), mode.description().to_string()))
        .collect::<Vec<_>>();
    let body = decision_body(
        &options,
        cursor.min(options.len().saturating_sub(1)),
        inner_width,
    );
    let footer = vec![footer_hint_line(START_PLAN_CHOICE_HINTS)];
    (header, body, footer)
}

pub(super) fn start_plan_choice_rows(area: Rect, cursor: usize) -> u16 {
    let inner_width = decision_inner_width(area);
    let (header, body, footer) = start_plan_choice_layout(cursor, inner_width);
    decision_rows(&header, &body, &footer, inner_width)
}

pub(super) fn render_start_plan_choice(f: &mut Frame, area: Rect, app: &AppState, cursor: usize) {
    let layout = start_plan_choice_layout(cursor, decision_inner_width(area));
    render_decision_popup(f, area, app, "Start Plan", layout);
}

/// Header, body and footer of the session-budget alert.
pub(super) fn budget_warning_layout(
    usage: crate::run_budget::SessionBudgetUsage,
    cursor: usize,
    inner_width: u16,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let header = vec![
        modal_subtitle("A configured cumulative alert has been reached."),
        modal_heading("Continue this submission?"),
    ];
    let mut body = vec![section_header("Usage")];
    body.push(Line::from(format!(
        "Active clock · {} · {} spent · {} waiting",
        usage.clock_label(),
        super::usage::format_duration_units(i64::try_from(usage.active_run_ms).unwrap_or(i64::MAX)),
        super::usage::format_duration_units(i64::try_from(usage.waiting_ms).unwrap_or(i64::MAX)),
    )));
    // Reached axes first, then the upcoming ones, so the decision to continue is
    // made with the next threshold visible — not just the crossed one.
    let mut alerts = usage.alert_states();
    alerts.sort_by_key(|alert| !alert.is_reached());
    for alert in alerts {
        body.push(Line::from(format_session_alert(alert, usage.clock_label())));
    }
    body.push(Line::from(""));
    body.extend(decision_body(
        &[
            (
                "Go back".to_string(),
                "Keep the composer unchanged".to_string(),
            ),
            (
                "Continue once".to_string(),
                "Run only this retained submission".to_string(),
            ),
        ],
        cursor.min(1),
        inner_width,
    ));
    body.push(Line::from(""));
    body.push(modal_subtitle(
        "Raise or disable thresholds in /settings under Session alerts.",
    ));
    let footer = vec![footer_hint_line(BUDGET_WARNING_HINTS)];
    (header, body, footer)
}

pub(super) fn budget_warning_rows(
    area: Rect,
    usage: crate::run_budget::SessionBudgetUsage,
    cursor: usize,
) -> u16 {
    let inner_width = decision_inner_width(area);
    let (header, body, footer) = budget_warning_layout(usage, cursor, inner_width);
    decision_rows(&header, &body, &footer, inner_width)
}

pub(super) fn render_budget_warning(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    usage: crate::run_budget::SessionBudgetUsage,
    cursor: usize,
) {
    let layout = budget_warning_layout(usage, cursor, decision_inner_width(area));
    render_decision_popup(f, area, app, "Session budget alert", layout);
}

fn format_session_alert(alert: crate::run_budget::SessionBudgetAlertState, clock: &str) -> String {
    use crate::run_budget::SessionBudgetAlertState;
    let state = if alert.is_reached() {
        "reached"
    } else {
        "next"
    };
    match alert {
        SessionBudgetAlertState::BilledTokens {
            used_tokens,
            threshold_tokens,
        } => format!(
            "Billed tokens · {}/{} · {state}",
            used_tokens.map_or_else(|| "unknown".to_string(), |used| used.to_string()),
            threshold_tokens
        ),
        SessionBudgetAlertState::ProviderTurns {
            used_turns,
            threshold_turns,
        } => format!("Provider turns · {used_turns}/{threshold_turns} · {state}"),
        SessionBudgetAlertState::ActiveTime {
            used_ms,
            threshold_seconds,
        } => format!(
            "{clock} time · {}s/{threshold_seconds}s · {state}",
            used_ms / 1_000
        ),
        SessionBudgetAlertState::Cost {
            used_micros,
            threshold_micros,
        } => {
            let used = used_micros.map_or_else(
                || "unknown".to_string(),
                |used| format!("${:.6}", used as f64 / 1_000_000.0),
            );
            format!(
                "Exact cost · {used}/${:.6} · {state}",
                threshold_micros as f64 / 1_000_000.0
            )
        }
    }
}

/// The delete-plan confirm's header, body and footer.
fn plan_delete_layout(
    plan: &crate::storage::SavedPlanSummary,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let body = vec![
        Line::from(Span::styled(
            format!("Delete \"{}\" from the saved plan library?", plan.title),
            theme::body(theme::palette().text),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "The source and execution session transcripts stay on disk.",
            theme::dim(),
        )),
    ];
    (Vec::new(), body, vec![confirm_footer_hint()])
}

pub(super) fn plan_delete_confirm_rows(area: Rect, plan: &crate::storage::SavedPlanSummary) -> u16 {
    let (header, body, footer) = plan_delete_layout(plan);
    decision_rows(&header, &body, &footer, decision_inner_width(area))
}

pub(super) fn render_plan_delete_confirm(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    plan: &crate::storage::SavedPlanSummary,
) {
    render_decision_popup(f, area, app, "Delete Plan", plan_delete_layout(plan));
}

/// The delete-session confirm's header, body and footer.
fn session_delete_layout(
    session: &crate::storage::SessionSummary,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let body = vec![
        Line::from(Span::styled(
            format!("Delete session \"{}\"?", session.name),
            theme::body(theme::palette().text),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "The session transcript and all its data are permanently deleted.",
            theme::dim(),
        )),
    ];
    (Vec::new(), body, vec![confirm_footer_hint()])
}

pub(super) fn session_delete_confirm_rows(
    area: Rect,
    session: &crate::storage::SessionSummary,
) -> u16 {
    let (header, body, footer) = session_delete_layout(session);
    decision_rows(&header, &body, &footer, decision_inner_width(area))
}

pub(super) fn render_session_delete_confirm(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    session: &crate::storage::SessionSummary,
) {
    render_decision_popup(
        f,
        area,
        app,
        "Delete Session",
        session_delete_layout(session),
    );
}

/// The discard-plan confirm's header, body and footer.
fn plan_discard_layout(
    saved_plan_id: crate::storage::SavedPlanId,
    title: &str,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let body = vec![
        Line::from(Span::styled(
            format!("Discard \"{title}\" and clear the canvas?"),
            theme::body(theme::palette().text),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("This also deletes saved plan #{saved_plan_id} from the library."),
            theme::dim(),
        )),
    ];
    (Vec::new(), body, vec![confirm_footer_hint()])
}

pub(super) fn plan_discard_confirm_rows(
    area: Rect,
    saved_plan_id: crate::storage::SavedPlanId,
    title: &str,
) -> u16 {
    let (header, body, footer) = plan_discard_layout(saved_plan_id, title);
    decision_rows(&header, &body, &footer, decision_inner_width(area))
}

pub(super) fn render_plan_discard_confirm(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    saved_plan_id: crate::storage::SavedPlanId,
    title: &str,
) {
    render_decision_popup(
        f,
        area,
        app,
        "Discard Plan",
        plan_discard_layout(saved_plan_id, title),
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
            // Prose: the session title is how a session is recognized, so it wraps.
            render: CellRender::Wrapped(session_display_name),
            style: CellStyle::Text,
        },
        TableColumn {
            header: "Updated",
            width: TableWidth::Exact(8),
            render: CellRender::Value(|session| relative_age(session.updated_at_ms)),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Lifecycle",
            width: TableWidth::Fit { cap: 12 },
            render: CellRender::Value(|session| session.status.label().to_string()),
            style: CellStyle::Custom(|session| session_status_style(&session.status)),
        },
        TableColumn {
            header: "Task",
            width: TableWidth::Fit { cap: 12 },
            render: CellRender::Value(|session| {
                session
                    .latest_task
                    .as_ref()
                    .map_or_else(|| "-".to_string(), |task| task.outcome_label().to_string())
            }),
            style: CellStyle::Custom(session_task_style),
        },
        TableColumn {
            header: "Msgs",
            width: TableWidth::Fit { cap: usize::MAX },
            render: CellRender::Value(|session| session.message_count.to_string()),
            style: CellStyle::Meta,
        },
    ]
}

fn session_task_style(session: &crate::storage::SessionSummary) -> Style {
    let palette = theme::palette();
    let color = match session.latest_task.as_ref().and_then(|task| task.outcome) {
        Some(crate::storage::TaskOutcome::Succeeded) => palette.success,
        Some(crate::storage::TaskOutcome::Failed) => palette.error,
        Some(crate::storage::TaskOutcome::Blocked)
        | Some(crate::storage::TaskOutcome::Cancelled)
        | Some(crate::storage::TaskOutcome::Superseded) => palette.muted,
        Some(crate::storage::TaskOutcome::Unknown) | None => {
            if session
                .latest_task
                .as_ref()
                .is_some_and(|task| task.is_active())
            {
                palette.progress
            } else {
                palette.dim
            }
        }
    };
    status_style(color)
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
            // Prose: the plan title is how a plan is recognized, so it wraps.
            render: CellRender::Wrapped(|plan| plan.title.clone()),
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
