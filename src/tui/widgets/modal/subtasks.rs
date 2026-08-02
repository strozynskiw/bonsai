use super::common::*;
use super::picker_common::*;
use super::*;

const PANEL_TITLE: &str = "Subagents";
const DETAIL_TITLE: &str = "Selected Subagent";

pub(super) fn render_subtask_list(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    subtasks: &[SubagentSnapshot],
    cursor: usize,
    pane: SubtaskListPane,
) {
    let focus_hint = if pane == SubtaskListPane::Detail {
        footer_hint_line(&[
            ("Up/Down", "scroll"),
            ("PgUp/PgDn", "page"),
            ("Tab/Left", "list"),
            ("Esc", "close"),
        ])
    } else {
        footer_hint_line(&[
            ("Up/Down", "move"),
            ("PgUp/PgDn", "page"),
            ("Tab/Right", "detail"),
            ("m", "model"),
            ("d", "default"),
            ("Esc", "close"),
        ])
    };
    let footer_lines = vec![
        Line::from(Span::styled(
            "Read-only subagents delegated via the agent tool.",
            theme::dim(),
        )),
        focus_hint,
    ];
    render_list_detail_modal(
        f,
        area,
        app,
        ListDetailModal {
            title: PANEL_TITLE,
            detail_title: DETAIL_TITLE,
            split: ListDetailSplit::Horizontal,
            detail_focused: pane == SubtaskListPane::Detail,
            footer_lines,
            modal_scroll: app.modal_scroll,
        },
        |frame, table_area, detail_area| {
            let columns = subtask_columns(table_area.width as usize);
            let widths = table_widths(table_area.width as usize, subtasks, &columns);
            let mut table_lines = vec![table_header(&columns, &widths)];
            if subtasks.is_empty() {
                table_lines.push(Line::from(Span::styled("No subagents yet", theme::dim())));
            } else {
                let cursor = cursor.min(subtasks.len().saturating_sub(1));
                let visible_lines = table_area.height.saturating_sub(1).max(1) as usize;
                table_lines.extend(
                    visible_picker_rows(subtasks.len(), cursor, visible_lines).map(|index| {
                        table_row(&subtasks[index], &columns, &widths, index == cursor)
                    }),
                );
            }
            frame.render_widget(
                Paragraph::new(table_lines)
                    .style(theme::panel())
                    .wrap(Wrap { trim: false }),
                table_area,
            );
            let selected = subtasks.get(cursor.min(subtasks.len().saturating_sub(1)));
            let detail_width = detail_pane_inner(detail_area).width.max(1) as usize;
            subtask_detail_lines_full(app, selected, detail_width)
        },
    );
}

fn subtask_columns(_width: usize) -> Vec<TableColumn<SubagentSnapshot>> {
    vec![
        TableColumn {
            header: "ID",
            width: TableWidth::Fit { cap: usize::MAX },
            render: CellRender::Value(|sub| sub.id.to_string()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "State",
            width: TableWidth::Fit { cap: 10 },
            render: CellRender::Value(|sub| sub.status.label().to_string()),
            style: CellStyle::Custom(|sub| status_style(subtask_status_color(sub.status))),
        },
        TableColumn {
            header: "Agent",
            width: TableWidth::Fit { cap: 16 },
            render: CellRender::Value(|sub| sub.agent.to_string()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Model",
            width: TableWidth::Flex { min: 0 },
            render: CellRender::Value(subtask_model_cell),
            style: CellStyle::Meta,
        },
    ]
}

/// The Model-column cell: the model the run used, or `—` before its provider is
/// minted.
fn subtask_model_cell(sub: &SubagentSnapshot) -> String {
    sub.model
        .as_deref()
        .map(str::to_string)
        .unwrap_or_else(|| "—".to_string())
}

fn subtask_status_color(status: SubagentStatus) -> Color {
    match status {
        SubagentStatus::Running => theme::palette().todo,
        SubagentStatus::Succeeded => theme::palette().success,
        SubagentStatus::Failed | SubagentStatus::TimedOut => theme::palette().error,
        SubagentStatus::Cancelled => theme::palette().muted,
    }
}

pub(super) fn subtask_detail_lines(
    sub: &SubagentSnapshot,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for line in sub.detail().lines() {
        push_wrapped_line(&mut lines, subtask_detail_line(sub, line), wrap_width);
    }
    lines
}

/// The full detail body the pane renders: the selected subagent's detail plus
/// its pending model override (read live from the shared store). Render and the
/// scroll-clamp metric must build from this exact list, or the override block
/// scrolls out of reach — see the regression test.
pub(super) fn subtask_detail_lines_full(
    app: &AppState,
    selected: Option<&SubagentSnapshot>,
    wrap_width: usize,
) -> Vec<Line<'static>> {
    let Some(sub) = selected else {
        return vec![Line::from(Span::styled(
            "No subagent selected",
            theme::dim(),
        ))];
    };
    let mut lines = subtask_detail_lines(sub, wrap_width);
    if let Some(over) = app
        .subagent_model_overrides
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(sub.agent.as_ref())
        .cloned()
    {
        lines.push(Line::from(""));
        push_wrapped_line(
            &mut lines,
            // The override map is keyed by agent name, so it applies to every
            // future delegation to this agent — not just the selected row. Say so,
            // or the per-row UI implies a narrower scope than `m`/`d` actually have.
            Line::from(Span::styled(
                format!(
                    "Override (all {} runs): {}",
                    sub.agent,
                    over.display_label()
                ),
                theme::label(theme::palette().tool),
            )),
            wrap_width,
        );
    }
    lines
}

fn subtask_detail_line(sub: &SubagentSnapshot, line: &str) -> Line<'static> {
    if line.is_empty() {
        Line::from("")
    } else if let Some((label, value)) = line.split_once(':') {
        if label.contains(' ') {
            // Not a "Label: value" header line (e.g. a `file:line` body).
            Line::from(symbol_spans(line, theme::body(theme::palette().text)))
        } else if value.is_empty() && is_subtask_detail_section(label) {
            Line::from(Span::styled(
                label.to_string(),
                theme::label(theme::palette().tool),
            ))
        } else {
            let mut spans = vec![Span::styled(format!("{label:<9}"), theme::muted())];
            let value_style = if label == "Status" {
                status_style(subtask_status_color(sub.status))
            } else {
                theme::body(theme::palette().text)
            };
            spans.extend(symbol_spans(value.trim_start(), value_style));
            Line::from(spans)
        }
    } else {
        Line::from(symbol_spans(line, theme::body(theme::palette().text)))
    }
}

fn push_wrapped_line(rows: &mut Vec<Line<'static>>, line: Line<'static>, width: usize) {
    if line.width() <= width {
        rows.push(line);
    } else {
        rows.extend(wrap_line(&line, width));
    }
}

fn is_subtask_detail_section(label: &str) -> bool {
    matches!(label, "Prompt" | "Activity" | "Result")
}

fn symbol_spans(text: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buffer = String::new();
    let mut in_symbol = false;

    for ch in text.chars() {
        if ch == '`' {
            push_symbol_span(&mut spans, &mut buffer, in_symbol, base_style);
            in_symbol = !in_symbol;
        } else {
            buffer.push(ch);
        }
    }
    push_symbol_span(&mut spans, &mut buffer, in_symbol, base_style);

    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base_style));
    }
    spans
}

fn push_symbol_span(
    spans: &mut Vec<Span<'static>>,
    buffer: &mut String,
    in_symbol: bool,
    base_style: Style,
) {
    if buffer.is_empty() {
        return;
    }
    let style = if in_symbol {
        theme::label(theme::palette().path)
    } else {
        base_style
    };
    spans.push(Span::styled(std::mem::take(buffer), style));
}

pub(super) fn max_subtask_detail_scroll(
    app: &AppState,
    area: Rect,
    subtasks: &[SubagentSnapshot],
    cursor: usize,
) -> u16 {
    let (_, detail_area, _) = list_detail_regions(area, ListDetailSplit::Horizontal);
    let detail_inner = detail_pane_inner(detail_area);
    let wrap_width = detail_inner.width.max(1) as usize;
    let selected = subtasks.get(cursor.min(subtasks.len().saturating_sub(1)));
    detail_max_scroll(
        detail_inner,
        &subtask_detail_lines_full(app, selected, wrap_width),
    )
}
