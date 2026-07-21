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
            split: ListDetailSplit::Vertical,
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
                // Each row renders 1–3 lines (the table row plus wrapped
                // prompt continuation), so the viewport must window by line
                // count, not row count.
                let prompt_width = widths.last().copied().unwrap_or(0);
                let heights = subtasks
                    .iter()
                    .map(|sub| prompt_preview_lines(&sub.prompt, prompt_width, 3).len())
                    .collect::<Vec<_>>();
                table_lines.extend(
                    visible_weighted_rows(&heights, cursor, visible_lines).flat_map(|index| {
                        subtask_list_row_lines(&subtasks[index], &columns, &widths, index == cursor)
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

fn subtask_columns(width: usize) -> Vec<TableColumn<SubagentSnapshot>> {
    // Reserve room for the other columns before spending width on the model
    // name, then keep it within a readable band.
    const MODEL_CAP_RESERVED: usize = 48;
    const MODEL_CAP_MIN: usize = 18;
    const MODEL_CAP_MAX: usize = 42;
    let model_cap = width
        .saturating_sub(MODEL_CAP_RESERVED)
        .clamp(MODEL_CAP_MIN, MODEL_CAP_MAX);
    vec![
        TableColumn {
            header: "ID",
            width: TableWidth::Fit { cap: usize::MAX },
            render: CellRender::Value(|sub| sub.id.clone()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "State",
            width: TableWidth::Fit { cap: 10 },
            render: CellRender::Value(|sub| sub.status.label().to_string()),
            style: CellStyle::Custom(|sub| status_style(subtask_status_color(sub.status))),
        },
        TableColumn {
            header: "Elapsed",
            width: TableWidth::Fit { cap: 8 },
            render: CellRender::Value(|sub| sub.duration_label()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Agent",
            width: TableWidth::Fit { cap: 12 },
            render: CellRender::Value(|sub| sub.agent.clone()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Model",
            width: TableWidth::Fit { cap: model_cap },
            render: CellRender::Value(subtask_model_cell),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Prompt",
            width: TableWidth::Flex { min: 0 },
            render: CellRender::Fitted(subtask_prompt_first_line),
            style: CellStyle::Text,
        },
    ]
}

/// The Model-column cell: the model the run used, or `—` before its provider is
/// minted.
fn subtask_model_cell(sub: &SubagentSnapshot) -> String {
    sub.model.clone().unwrap_or_else(|| "—".to_string())
}

fn subtask_list_row_lines(
    sub: &SubagentSnapshot,
    columns: &[TableColumn<SubagentSnapshot>],
    widths: &[usize],
    selected: bool,
) -> Vec<Line<'static>> {
    let text_style = theme::body(theme::palette().text);
    let meta_style = theme::muted();
    let prompt_width = widths.last().copied().unwrap_or(0);
    let prompt_lines = prompt_preview_lines(&sub.prompt, prompt_width, 3);
    let mut lines = vec![table_row(sub, columns, widths, selected)];

    let prompt_offset = table_column_offset(widths, widths.len().saturating_sub(1));
    for prompt in prompt_lines.into_iter().skip(1) {
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(prompt_offset), meta_style),
            Span::styled(prompt, text_style),
        ]));
    }

    lines
}

fn prompt_preview_lines(prompt: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let prompt = prompt.replace(['\n', '\r', '\t'], " ");
    let (wrapped, _) = wrapped_input_field(Vec::new(), &prompt, Style::default(), width as u16);
    let mut lines = wrapped.iter().map(line_plain_text).collect::<Vec<String>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            *last = truncate_ascii(&format!("{last}..."), width);
        }
    }
    lines
}

fn subtask_prompt_first_line(sub: &SubagentSnapshot, width: usize) -> String {
    prompt_preview_lines(&sub.prompt, width, 1)
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn line_plain_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
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
        .get(&sub.agent)
        .cloned()
    {
        lines.push(Line::from(""));
        push_wrapped_line(
            &mut lines,
            Line::from(Span::styled(
                format!("Override (next run): {}", over.display_label()),
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
    let (_, detail_area, _) = list_detail_regions(area, ListDetailSplit::Vertical);
    let detail_inner = detail_pane_inner(detail_area);
    let wrap_width = detail_inner.width.max(1) as usize;
    let selected = subtasks.get(cursor.min(subtasks.len().saturating_sub(1)));
    detail_max_scroll(
        detail_inner,
        &subtask_detail_lines_full(app, selected, wrap_width),
    )
}
