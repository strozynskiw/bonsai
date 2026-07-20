use super::common::*;
use super::picker_common::pad_ascii;
use super::*;

/// Column widths shared by the ledger and wire tables, so header, section
/// rows, and the Raw JSON row line up (display-width, wide-grapheme safe).
const LEDGER_LABEL_WIDTH: usize = 18;
const WIRE_LABEL_WIDTH: usize = 18;
const WIRE_PATH_WIDTH: usize = 28;
/// Max preview lines shown under an expanded ledger/wire node — counted by both
/// the render walker and the line-count walker, so it must be one constant.
const CONTEXT_PREVIEW_LINE_LIMIT: usize = 8;
/// Rough chars-per-token divisor for the wire Raw-JSON token estimate.
const CHARS_PER_TOKEN: usize = 4;

/// The shared modal header, built once so rendering and every line-count
/// metric (scroll clamping, auto-follow, mouse hit-testing) agree by
/// construction. Every line here must stay short enough not to wrap at the
/// modal's width — the metrics count each entry as one visual line.
pub(super) fn context_header_lines(
    app: &AppState,
    report: &ContextReport,
    bar_width: usize,
) -> Vec<Line<'static>> {
    let p = theme::palette();
    let mode = app.context_state.view_mode;
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Which model/provider this context will be sent to.
    lines.push(Line::from(vec![
        Span::styled(
            app.model.clone(),
            theme::body(p.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  ·  {}", app.provider), theme::dim()),
    ]));
    lines.push(context_view_selector_line(mode));
    lines.push(context_usage_bar_line(report, bar_width));
    lines.push(context_usage_summary_line(report));
    if let Some(warning) =
        crate::context_view::telemetry::CostTelemetry::from_report(report).context_warning
    {
        lines.push(Line::from(Span::styled(
            format!("warning {warning}"),
            theme::body(p.error),
        )));
    }
    if let Some(last_turn) = context_last_turn_line(report) {
        lines.push(last_turn);
    }
    if let Some(verdict) = context_cache_verdict_line(report) {
        lines.push(verdict);
    }
    match mode {
        ContextViewMode::Ledger => {
            let telemetry = crate::context_view::telemetry::CostTelemetry::from_report(report);
            if let Some(drivers) = telemetry.driver_lines(report).into_iter().next() {
                lines.push(Line::from(Span::styled(drivers, theme::dim())));
            }
            if let Some(legend) = context_legend_line(report) {
                lines.push(legend);
            }
        }
        ContextViewMode::Wire => {
            if let Some(cache_plan) = context_wire_cache_plan_line(report) {
                lines.push(cache_plan);
            }
        }
        ContextViewMode::Turns => {}
    }
    lines.push(Line::from(""));
    lines.push(section_header(match mode {
        ContextViewMode::Ledger => "LEDGER",
        ContextViewMode::Wire => "WIRE",
        ContextViewMode::Turns => "TURNS",
    }));
    lines
}

/// Every line of the modal body (header + active view), in render order. The
/// single builder shared by the renderer, the scroll metrics, and mouse
/// hit-testing so their line geometry cannot drift apart.
pub(super) fn context_modal_lines(
    app: &AppState,
    report: &ContextReport,
    bar_width: usize,
) -> Vec<Line<'static>> {
    let mut lines = context_header_lines(app, report, bar_width);
    if app.context_state.view_mode.is_wire() {
        lines.extend(context_wire_lines(app, report));
    } else if app.context_state.view_mode.is_turns() {
        lines.extend(super::context_turns::context_turns_lines(app, report));
    } else {
        if report.ledger.is_empty() {
            lines.push(Line::from(Span::styled("(empty)", theme::dim())));
        }
        lines.extend(context_ledger_lines(app, report));
    }
    lines
}

/// Visual-row prefix offsets for `lines` wrapped at `width`: entry `i` is the
/// first visual row of logical line `i`, and the final entry is the total
/// visual row count. Converts between the logical space the row walkers count
/// in and the wrapped space the body paragraph scrolls in.
pub(super) fn visual_line_offsets(lines: &[Line<'static>], width: usize) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(lines.len().saturating_add(1));
    let mut total = 0usize;
    for line in lines {
        offsets.push(total);
        total = total.saturating_add(wrapped_line_count_usize(line, width));
    }
    offsets.push(total);
    offsets
}

pub(super) fn visual_line_total(lines: &[Line<'static>], width: usize) -> usize {
    lines
        .iter()
        .map(|line| wrapped_line_count_usize(line, width))
        .sum()
}

fn context_role_color(role: crate::agent::ContextRole) -> ratatui::style::Color {
    use crate::agent::ContextRole;
    let p = theme::palette();
    match role {
        ContextRole::System => p.context_system,
        ContextRole::User => p.context_user,
        ContextRole::Assistant => p.context_assistant,
        ContextRole::Tool => p.context_tool,
        ContextRole::ToolSchema => p.context_tool_schema,
    }
}

/// Per-role token totals, in bar/legend display order.
fn context_role_segments(report: &ContextReport) -> [(crate::agent::ContextRole, usize); 5] {
    use crate::agent::ContextRole;
    [
        (ContextRole::System, report.tokens_for(ContextRole::System)),
        (ContextRole::User, report.tokens_for(ContextRole::User)),
        (
            ContextRole::Assistant,
            report.tokens_for(ContextRole::Assistant),
        ),
        (ContextRole::Tool, report.tokens_for(ContextRole::Tool)),
        (
            ContextRole::ToolSchema,
            report.tokens_for(ContextRole::ToolSchema),
        ),
    ]
}

/// Segmented usage bar (solid = used per role, hollow = free).
fn context_usage_bar_line(report: &ContextReport, bar_width: usize) -> Line<'static> {
    let p = theme::palette();
    let budget = report.budget_tokens.max(1);
    let mut bar: Vec<Span<'static>> = Vec::new();
    let mut used_cells = 0usize;
    for (role, tokens) in context_role_segments(report) {
        let cells = tokens.saturating_mul(bar_width) / budget;
        if cells > 0 {
            bar.push(Span::styled(
                "█".repeat(cells),
                theme::body(context_role_color(role)),
            ));
            used_cells += cells;
        }
    }
    let free = bar_width.saturating_sub(used_cells);
    if free > 0 {
        bar.push(Span::styled("░".repeat(free), theme::body(p.dim)));
    }
    Line::from(bar)
}

/// `~101,300 / 200,000 tok (50%) · est tiktoken/high · 42 entries`
fn context_usage_summary_line(report: &ContextReport) -> Line<'static> {
    use crate::provider::{EstimateConfidence, TokenCounterKind};
    let budget = report.budget_tokens.max(1);
    let used = report.used_tokens();
    let pct = used.saturating_mul(100) / budget;
    let source = match report.estimate_source {
        TokenCounterKind::Tiktoken => "tiktoken",
        TokenCounterKind::Qwen3 => "qwen3",
        TokenCounterKind::AnthropicCountTokens => "count_tokens",
        TokenCounterKind::Heuristic => "heuristic",
    };
    let confidence = match report.estimate_confidence {
        EstimateConfidence::High => "high",
        EstimateConfidence::Low => "low",
    };
    Line::from(Span::styled(
        format!(
            "~{} / {} tok ({pct}%) · est {source}/{confidence} · {} entries",
            group_thousands(used),
            group_thousands(budget),
            report.message_count()
        ),
        theme::dim(),
    ))
}

/// Real provider counts from the last turn, when reported:
/// `last ↑42.1k ↓1.2k · cache 92% · $0.021 · saved $0.018`
fn context_last_turn_line(report: &ContextReport) -> Option<Line<'static>> {
    use crate::context_view::telemetry::compact_tokens;
    let p = theme::palette();
    let (prompt, completion) = report
        .last_input_tokens()
        .zip(report.last_completion_tokens)?;
    let mut actual = format!(
        "↑{} ↓{}",
        compact_tokens(prompt as usize),
        compact_tokens(completion as usize)
    );
    actual.push_str(" · ");
    actual.push_str(&input_cache_label(report.last_input_cache));
    actual.push_str(" · ");
    actual.push_str(&crate::tui::widgets::sidebar::optional_cost_micros(
        report.last_turn_cost_micros,
    ));
    actual.push_str(&cache_savings_suffix(report.last_turn_savings_micros));
    Some(Line::from(vec![
        Span::styled("last  ", theme::body(p.text)),
        Span::styled(actual, theme::body(p.success)),
    ]))
}

/// Always-visible cache verdict for the most recent turn, colored by how the
/// turn fared (warm/partial/cold).
fn context_cache_verdict_line(report: &ContextReport) -> Option<Line<'static>> {
    use crate::context_view::cache_diagnosis::{
        TurnCacheAssessment, last_turn_diagnosis, last_turn_verdict,
    };
    let p = theme::palette();
    let verdict = last_turn_verdict(report)?;
    let style = match last_turn_diagnosis(report)?.assessment {
        TurnCacheAssessment::Warm { .. } => theme::body(p.success),
        TurnCacheAssessment::Partial { .. } => theme::body(p.todo),
        TurnCacheAssessment::Cold { .. } => theme::body(p.error),
        _ => theme::dim(),
    };
    Some(Line::from(Span::styled(verdict, style)))
}

/// One-line legend with per-role token counts and share of what's used
/// (zero-token roles are skipped; the bar already shows proportions).
fn context_legend_line(report: &ContextReport) -> Option<Line<'static>> {
    use crate::context_view::telemetry::compact_tokens;
    let used = report.used_tokens().max(1);
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (role, tokens) in context_role_segments(report) {
        if tokens == 0 {
            continue;
        }
        let share = tokens.saturating_mul(100) / used;
        if !spans.is_empty() {
            spans.push(Span::styled("  ", theme::dim()));
        }
        spans.push(Span::styled("● ", theme::body(context_role_color(role))));
        spans.push(Span::styled(
            format!("{} {} {share}%", role.label(), compact_tokens(tokens)),
            theme::dim(),
        ));
    }
    (!spans.is_empty()).then(|| Line::from(spans))
}

/// Wire-mode cache plan: where the estimated cacheable prefix ends, where the
/// volatile tail begins, and how many cache_control breakpoints the request
/// actually carries.
fn context_wire_cache_plan_line(report: &ContextReport) -> Option<Line<'static>> {
    if report.cacheable_prefix_tokens == 0 && report.volatile_tail_tokens == 0 {
        return None;
    }
    let mut plan = format!(
        "cache plan  prefix ~{} tok · volatile tail ~{} tok",
        group_thousands(report.cacheable_prefix_tokens),
        group_thousands(report.volatile_tail_tokens)
    );
    let breakpoints = report
        .payload_preview
        .as_ref()
        .map(|preview| {
            preview
                .wire_sections
                .iter()
                .map(count_wire_breakpoints)
                .sum::<usize>()
        })
        .unwrap_or(0);
    if breakpoints > 0 {
        plan.push_str(&format!(" · {breakpoints} breakpoints (ephemeral)"));
    }
    Some(Line::from(Span::styled(plan, theme::dim())))
}

fn count_wire_breakpoints(section: &crate::provider::ProviderWireSection) -> usize {
    let own = usize::from(section.cache == Some(crate::provider::WireCacheHint::Breakpoint));
    own + section
        .children
        .iter()
        .map(count_wire_breakpoints)
        .sum::<usize>()
}

pub(super) fn render_context(f: &mut Frame, area: Rect, app: &AppState, report: &ContextReport) {
    let bar_width = (area.width.saturating_sub(4) as usize).max(10);
    let lines = context_modal_lines(app, report, bar_width);
    let footer_line = context_footer_line(app);

    let frame = theme::frame("Context window", true);
    let (content_area, footer_area) = context_modal_regions(area);
    f.render_widget(
        Paragraph::new(Vec::new())
            .block(frame)
            .style(theme::panel()),
        area,
    );

    let viewport_height = content_area.height.max(1) as usize;
    // The body paragraph wraps, so scroll limits must count *visual* (wrapped)
    // rows — a logical count under-measures whenever any line wraps and the
    // cursor auto-follow then leaves the selection below the fold.
    let content_width = content_area.width.max(1) as usize;
    let total_visual_lines = visual_line_total(&lines, content_width);
    let max_scroll = total_visual_lines.saturating_sub(viewport_height) as u16;
    let scroll = app.modal_scroll.min(max_scroll);
    let panel = Paragraph::new(lines)
        .style(theme::panel())
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(panel, content_area);

    if max_scroll > 0 {
        let mut state = ScrollbarState::new(max_scroll as usize + 1)
            .position(scroll as usize)
            .viewport_content_length(viewport_height);
        let scrollbar = theme::scrollbar(ScrollbarOrientation::VerticalRight);
        f.render_stateful_widget(scrollbar, content_area, &mut state);
    }

    if footer_area.height > 0 {
        let footer = Paragraph::new(vec![footer_line])
            .style(theme::panel())
            .wrap(Wrap { trim: false });
        f.render_widget(footer, footer_area);
    }
}

pub(super) fn context_view_selector_line(mode: ContextViewMode) -> Line<'static> {
    let p = theme::palette();
    let active = theme::body(p.text).add_modifier(Modifier::BOLD);
    let inactive = theme::muted();
    Line::from(vec![
        Span::styled("View  ", theme::dim()),
        Span::styled(
            "Ledger",
            if mode == ContextViewMode::Ledger {
                active
            } else {
                inactive
            },
        ),
        Span::styled(" | ", theme::dim()),
        Span::styled(
            "Wire",
            if mode == ContextViewMode::Wire {
                active
            } else {
                inactive
            },
        ),
        Span::styled(" | ", theme::dim()),
        Span::styled(
            "Turns",
            if mode == ContextViewMode::Turns {
                active
            } else {
                inactive
            },
        ),
    ])
}

#[derive(Debug)]
struct ContextListRow {
    index: usize,
    depth: Option<usize>,
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct ContextListLayout {
    lines: Vec<Line<'static>>,
    rows: Vec<ContextListRow>,
}

impl ContextListLayout {
    fn selected_span(&self, selected: usize) -> Option<(usize, usize)> {
        let position = self.rows.iter().position(|row| row.index == selected)?;
        let selected_row = &self.rows[position];
        let mut end = selected_row.end;
        if let Some(depth) = selected_row.depth {
            for row in &self.rows[position + 1..] {
                match row.depth {
                    Some(child_depth) if child_depth > depth => end = row.end,
                    _ => break,
                }
            }
        }
        Some((selected_row.start, end))
    }

    fn row_index_at(&self, line: usize) -> Option<usize> {
        self.rows
            .iter()
            .find(|row| row.start == line)
            .map(|row| row.index)
    }
}

fn context_ledger_layout(app: &AppState, report: &ContextReport) -> ContextListLayout {
    let p = theme::palette();
    let rows = context_ledger_rows(app, report);
    let budget = report.budget_tokens.max(1);
    let mut lines = Vec::new();
    let mut layout_rows = Vec::with_capacity(rows.len());
    for (index, (depth, node)) in rows.iter().enumerate() {
        let start = lines.len();
        let selected = index == app.context_state.cursor.min(rows.len().saturating_sub(1));
        let selector = if selected { ">" } else { " " };
        let expand = if node.children.is_empty() {
            " "
        } else if app.context_state.expanded.contains(node.id.as_str()) {
            "-"
        } else {
            "+"
        };
        let indent = "  ".repeat(*depth);
        let share = node
            .tokens
            .saturating_mul(100)
            .checked_div(budget)
            .unwrap_or(0);
        let kind = node.kind.label();
        let inclusion = node.inclusion.label();
        let control = report.control_for(node.id.as_str());
        let markers =
            context_control_markers(control, report.summary_source_available(node.id.as_str()));
        let retention = control
            .stubbed
            .then_some(control.stub_reason)
            .flatten()
            .map(|reason| reason.label())
            .unwrap_or("");
        let label_style = match node.inclusion {
            crate::agent::ContextInclusion::Included => theme::body(p.text),
            crate::agent::ContextInclusion::PendingNextTurn => theme::body(p.user),
            crate::agent::ContextInclusion::NotSent => theme::body(p.muted),
            crate::agent::ContextInclusion::Adjustment => theme::body(p.todo),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{selector} {indent}{expand} "), theme::dim()),
            Span::styled(pad_ascii(&node.label, LEDGER_LABEL_WIDTH), label_style),
            Span::styled(format!(" {:<9}", markers), theme::body(p.todo)),
            Span::styled(format!(" {:<21}", retention), theme::dim()),
            Span::styled(format!(" {:<10}", inclusion), theme::dim()),
            Span::styled(
                format!("{:>8} tok", group_thousands(node.tokens)),
                theme::dim(),
            ),
            Span::styled(format!(" {:>3}%", share), theme::dim()),
            Span::styled(format!("  {:<11}", kind), theme::muted()),
        ]));
        if context_node_sources_visible(app, node) {
            lines.extend(context_source_lines(node, &indent));
        }
        if context_node_preview_visible(app, node) {
            for raw in node.preview.lines().take(CONTEXT_PREVIEW_LINE_LIMIT) {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {indent}  "), theme::dim()),
                    Span::styled(raw.to_string(), theme::body(p.muted)),
                ]));
            }
        }
        layout_rows.push(ContextListRow {
            index,
            depth: Some(*depth),
            start,
            end: lines.len(),
        });
    }
    ContextListLayout {
        lines,
        rows: layout_rows,
    }
}

pub(super) fn context_ledger_lines(app: &AppState, report: &ContextReport) -> Vec<Line<'static>> {
    context_ledger_layout(app, report).lines
}

fn context_wire_layout(app: &AppState, report: &ContextReport) -> ContextListLayout {
    let Some(preview) = &report.payload_preview else {
        return ContextListLayout {
            lines: vec![Line::from(Span::styled(
                "Wire preview is not available.",
                theme::dim(),
            ))],
            rows: Vec::new(),
        };
    };
    let mut lines = vec![context_wire_endpoint_line(preview)];
    let rows = context_wire_rows(app, report);
    let mut layout_rows = Vec::with_capacity(rows.len().saturating_add(1));
    for (index, (depth, section)) in rows.iter().enumerate() {
        let start = lines.len();
        lines.push(context_wire_section_line(
            app,
            index,
            rows.len(),
            *depth,
            section,
        ));
        if context_wire_detail_visible(section, &app.context_state.wire_expanded) {
            let indent = "  ".repeat(*depth);
            lines.extend(wire_detail_lines(
                &section.preview,
                &format!("  {indent}  "),
            ));
        }
        layout_rows.push(ContextListRow {
            index,
            depth: Some(*depth),
            start,
            end: lines.len(),
        });
    }
    let body = serde_json::to_string_pretty(&preview.body).unwrap_or_else(|_err| "{}".to_string());
    let raw_start = lines.len();
    lines.push(context_wire_raw_json_line(app, rows.len(), &body));
    if app
        .context_state
        .wire_expanded
        .contains(CONTEXT_WIRE_RAW_JSON_ID)
    {
        lines.extend(wire_detail_lines(&body, "    "));
    }
    layout_rows.push(ContextListRow {
        index: rows.len(),
        depth: None,
        start: raw_start,
        end: lines.len(),
    });
    ContextListLayout {
        lines,
        rows: layout_rows,
    }
}

pub(super) fn context_wire_lines(app: &AppState, report: &ContextReport) -> Vec<Line<'static>> {
    context_wire_layout(app, report).lines
}

pub(super) fn context_wire_endpoint_line(
    preview: &crate::provider::ProviderRequestPreview,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(preview.method, theme::body(theme::palette().text)),
        Span::styled(" ", theme::dim()),
        Span::styled(preview.endpoint.clone(), theme::dim()),
    ])
}

pub(super) fn context_wire_section_line(
    app: &AppState,
    index: usize,
    row_count: usize,
    depth: usize,
    section: &crate::provider::ProviderWireSection,
) -> Line<'static> {
    let p = theme::palette();
    let selected = index == app.context_state.wire_cursor.min(row_count);
    let selector = if selected { ">" } else { " " };
    let expand = context_wire_expand_marker(section, &app.context_state.wire_expanded);
    let indent = "  ".repeat(depth);
    let mut spans = vec![
        Span::styled(format!("{selector} {indent}{expand} "), theme::dim()),
        Span::styled(
            pad_ascii(&section.label, WIRE_LABEL_WIDTH),
            theme::body(p.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", pad_ascii(&section.provider_path, WIRE_PATH_WIDTH)),
            theme::muted(),
        ),
        Span::styled(
            format!(" {:>7} tok", group_thousands(section.token_estimate)),
            theme::dim(),
        ),
        Span::styled(
            format!(
                " · {} chars · {} bytes",
                group_thousands(section.chars),
                group_thousands(section.bytes),
            ),
            theme::dim(),
        ),
    ];
    if let Some((tag, style)) = context_wire_cache_tag(section.cache) {
        spans.push(Span::styled(tag, style));
    }
    Line::from(spans)
}

fn context_wire_cache_tag(
    cache: Option<crate::provider::WireCacheHint>,
) -> Option<(&'static str, Style)> {
    use crate::provider::WireCacheHint;
    let p = theme::palette();
    match cache? {
        WireCacheHint::Breakpoint => Some(("  ◆ cache-bp", theme::body(p.progress))),
        WireCacheHint::CachedPrefix => Some(("  ▐ cached", theme::body(p.success))),
        WireCacheHint::Volatile => Some(("  ~ volatile", theme::body(p.todo))),
    }
}

pub(super) fn context_wire_raw_json_line(
    app: &AppState,
    row_count: usize,
    body: &str,
) -> Line<'static> {
    let p = theme::palette();
    let raw_selected = app.context_state.wire_cursor == row_count;
    let raw_selector = if raw_selected { ">" } else { " " };
    let raw_expand = if app
        .context_state
        .wire_expanded
        .contains(CONTEXT_WIRE_RAW_JSON_ID)
    {
        "-"
    } else {
        "+"
    };
    Line::from(vec![
        // Same 4-cell prefix as a top-level section row so the columns align.
        Span::styled(format!("{raw_selector} {raw_expand} "), theme::dim()),
        Span::styled(
            pad_ascii("Raw JSON", WIRE_LABEL_WIDTH),
            theme::body(p.text).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", pad_ascii("$", WIRE_PATH_WIDTH)),
            theme::muted(),
        ),
        Span::styled(
            format!(" {:>7} tok", group_thousands(wire_token_estimate(body))),
            theme::dim(),
        ),
        Span::styled(
            format!(
                " · {} chars · {} bytes",
                group_thousands(body.chars().count()),
                group_thousands(body.len())
            ),
            theme::dim(),
        ),
    ])
}

pub(super) fn context_wire_expand_marker(
    section: &crate::provider::ProviderWireSection,
    expanded: &std::collections::HashSet<String>,
) -> &'static str {
    if section.children.is_empty() && section.preview.is_empty() {
        " "
    } else if expanded.contains(&section.id) {
        "-"
    } else {
        "+"
    }
}

pub(super) fn context_wire_detail_visible(
    section: &crate::provider::ProviderWireSection,
    expanded: &std::collections::HashSet<String>,
) -> bool {
    section.children.is_empty() && expanded.contains(&section.id) && !section.preview.is_empty()
}

pub(super) fn wire_detail_lines(text: &str, indent: &str) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![Line::from(vec![
            Span::styled(indent.to_string(), theme::dim()),
            Span::styled("(empty)", theme::muted()),
        ])];
    }
    text.lines()
        .map(|line| {
            Line::from(vec![
                Span::styled(indent.to_string(), theme::dim()),
                Span::styled(line.to_string(), theme::body(theme::palette().muted)),
            ])
        })
        .collect()
}

pub(super) fn wire_token_estimate(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.chars().count().saturating_div(CHARS_PER_TOKEN).max(1)
    }
}

pub(super) fn context_footer_line(app: &AppState) -> Line<'static> {
    match app.context_state.view_mode {
        ContextViewMode::Wire => footer_hint_line(&[
            ("Enter", "expand/collapse"),
            ("p/d/s/r", "disabled"),
            ("Tab", "turns"),
        ]),
        ContextViewMode::Turns => {
            footer_hint_line(&[("Enter", "expand/collapse"), ("Tab", "ledger")])
        }
        ContextViewMode::Ledger => footer_hint_line(&[
            ("Enter", "expand/collapse"),
            ("p", "pin"),
            ("d", "drop"),
            ("s", "stub"),
            ("r", "restore"),
            ("Tab", "wire"),
        ]),
    }
}

pub(super) fn context_modal_regions(area: Rect) -> (Rect, Rect) {
    let inner = theme::frame(String::new(), true).inner(area);
    if inner.height <= 1 {
        return (inner, Rect::default());
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    (chunks[0], chunks[1])
}

pub(super) fn context_control_markers(
    control: crate::agent::ContextControlState,
    source_available: bool,
) -> String {
    let mut markers = String::new();
    if control.pinned {
        markers.push('P');
    }
    if control.drop_next_turn {
        markers.push('D');
    }
    if control.stubbed {
        markers.push('S');
    }
    if source_available {
        markers.push('R');
    }
    if markers.is_empty() {
        "-".to_string()
    } else {
        markers
    }
}

/// The selected row's block in *logical* lines: `(start, end)` with `start`
/// the row's own line and `end` (exclusive) past its visible detail — sources,
/// preview, and any deeper child rows. `end == start + 1` for a collapsed row.
/// Callers convert to visual rows via [`visual_line_offsets`].
pub(super) fn context_selected_span(
    app: &AppState,
    report: &ContextReport,
) -> Option<(usize, usize)> {
    if app.context_state.view_mode.is_wire() {
        return context_wire_selected_span(app, report);
    }
    if app.context_state.view_mode.is_turns() {
        return super::context_turns::context_turns_selected_span(app, report);
    }
    let layout = context_ledger_layout(app, report);
    let selected = app
        .context_state
        .cursor
        .min(layout.rows.len().saturating_sub(1));
    let header = context_header_line_count(app, report);
    layout
        .selected_span(selected)
        .map(|(start, end)| (header.saturating_add(start), header.saturating_add(end)))
}

pub(super) fn context_ledger_row_index_at(
    app: &AppState,
    report: &ContextReport,
    target_line: usize,
) -> Option<usize> {
    context_ledger_layout(app, report).row_index_at(target_line)
}

pub(super) fn context_wire_row_index_at(
    app: &AppState,
    report: &ContextReport,
    target_line: usize,
) -> Option<usize> {
    report.payload_preview.as_ref()?;
    let header = context_header_line_count(app, report);
    target_line
        .checked_sub(header)
        .and_then(|line| context_wire_layout(app, report).row_index_at(line))
}

fn context_wire_selected_span(app: &AppState, report: &ContextReport) -> Option<(usize, usize)> {
    report.payload_preview.as_ref()?;
    let layout = context_wire_layout(app, report);
    let selected = app
        .context_state
        .wire_cursor
        .min(layout.rows.len().saturating_sub(1));
    let header = context_header_line_count(app, report);
    layout
        .selected_span(selected)
        .map(|(start, end)| (header.saturating_add(start), header.saturating_add(end)))
}

pub(super) fn wrapped_line_count_usize(line: &Line<'static>, width: usize) -> usize {
    usize::from(wrapped_line_count(line, width)).max(1)
}

/// Header height for scroll metrics — the same builder the renderer uses, so
/// the count cannot drift. The bar width does not affect the line count (the
/// bar is always exactly one line).
pub(super) fn context_header_line_count(app: &AppState, report: &ContextReport) -> usize {
    context_header_lines(app, report, 10).len()
}

pub(super) fn context_ledger_rows<'a>(
    app: &'a AppState,
    report: &'a ContextReport,
) -> Vec<(usize, &'a crate::agent::ContextNode)> {
    let mut rows = Vec::new();
    for node in &report.ledger {
        collect_context_rows(node, 0, &app.context_state.expanded, &mut rows);
    }
    rows
}

pub(super) fn context_wire_rows<'a>(
    app: &'a AppState,
    report: &'a ContextReport,
) -> Vec<(usize, &'a crate::provider::ProviderWireSection)> {
    let mut rows = Vec::new();
    if let Some(preview) = &report.payload_preview {
        for section in &preview.wire_sections {
            collect_context_wire_rows(section, 0, &app.context_state.wire_expanded, &mut rows);
        }
    }
    rows
}

pub(super) fn collect_context_wire_rows<'a>(
    section: &'a crate::provider::ProviderWireSection,
    depth: usize,
    expanded: &std::collections::HashSet<String>,
    rows: &mut Vec<(usize, &'a crate::provider::ProviderWireSection)>,
) {
    rows.push((depth, section));
    if expanded.contains(&section.id) {
        for child in &section.children {
            collect_context_wire_rows(child, depth + 1, expanded, rows);
        }
    }
}

pub(super) fn context_node_preview_visible(
    app: &AppState,
    node: &crate::agent::ContextNode,
) -> bool {
    node.children.is_empty()
        && app.context_state.expanded.contains(node.id.as_str())
        && !node.preview.is_empty()
}

pub(super) fn context_node_sources_visible(
    app: &AppState,
    node: &crate::agent::ContextNode,
) -> bool {
    app.context_state.expanded.contains(node.id.as_str()) && !node.sources.is_empty()
}

const CONTEXT_SOURCE_LINE_LIMIT: usize = 3;

pub(super) fn context_source_lines(
    node: &crate::agent::ContextNode,
    indent: &str,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for source in node.sources.iter().take(CONTEXT_SOURCE_LINE_LIMIT) {
        lines.push(Line::from(vec![
            Span::styled(format!("  {indent}  "), theme::dim()),
            Span::styled("source: ", theme::dim()),
            Span::styled(
                context_source_label(source),
                theme::body(theme::palette().muted),
            ),
        ]));
    }
    if node.sources.len() > CONTEXT_SOURCE_LINE_LIMIT {
        lines.push(Line::from(vec![
            Span::styled(format!("  {indent}  "), theme::dim()),
            Span::styled(
                format!(
                    "source: +{} more",
                    node.sources.len().saturating_sub(CONTEXT_SOURCE_LINE_LIMIT)
                ),
                theme::dim(),
            ),
        ]));
    }
    lines
}

fn context_source_label(source: &crate::agent::ContextSourceRef) -> String {
    let mut label = source.label.clone();
    if let Some(detail) = &source.detail {
        label.push_str(" · ");
        label.push_str(detail);
    }
    if source.restorable {
        label.push_str(" · restorable");
    }
    label
}

pub(super) fn saturating_u16(value: usize) -> u16 {
    value.min(u16::MAX as usize) as u16
}

pub(super) fn collect_context_rows<'a>(
    node: &'a crate::agent::ContextNode,
    depth: usize,
    expanded: &std::collections::HashSet<String>,
    rows: &mut Vec<(usize, &'a crate::agent::ContextNode)>,
) {
    rows.push((depth, node));
    if expanded.contains(node.id.as_str()) {
        for child in &node.children {
            collect_context_rows(child, depth + 1, expanded, rows);
        }
    }
}

pub(super) fn input_cache_label(input_cache: Option<crate::provider::InputCacheUsage>) -> String {
    input_cache
        .and_then(|usage| usage.hit_rate_percent())
        .map(|percent| format!("cache {percent}%"))
        .unwrap_or_else(|| "cache n/a".to_string())
}

/// `" · saved $X"` when the prompt cache saved money, else empty.
pub(super) fn cache_savings_suffix(savings_micros: Option<u64>) -> String {
    match savings_micros {
        Some(micros) if micros > 0 => format!(
            " · saved {}",
            crate::tui::widgets::sidebar::format_cost_micros(micros)
        ),
        _ => String::new(),
    }
}

/// Format an integer with thousands separators (e.g. `120,000`).
pub(super) fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_view::{ContextSourceKind, ContextSourceRef};

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn expanded_ledger_rows_render_source_details() {
        let mut app = AppState::new("codex", "gpt-5".to_string(), ".".to_string(), None);
        app.context_state.expanded.insert("msg-0".to_string());
        let report = ContextReport {
            budget_tokens: 120_000,
            ledger: vec![crate::agent::ContextNode {
                id: "msg-0".into(),
                kind: crate::agent::ContextNodeKind::ChatMessage,
                inclusion: crate::agent::ContextInclusion::Included,
                role: Some(crate::agent::ContextRole::User),
                label: "User message 1".to_string(),
                tokens: 10,
                chars: 5,
                bytes: 5,
                source: crate::provider::TokenCounterKind::Heuristic,
                confidence: crate::provider::EstimateConfidence::Low,
                preview: String::new(),
                sources: vec![
                    ContextSourceRef::new(
                        ContextSourceKind::ContextMessage,
                        "msg-0",
                        "context message msg-0",
                    )
                    .with_detail("user")
                    .restorable(),
                ],
                children: Vec::new(),
            }],
            ..ContextReport::default()
        };

        let lines = context_ledger_lines(&app, &report);
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");

        assert!(text.contains("source: context message msg-0 · user · restorable"));
        assert_eq!(
            context_modal_lines(&app, &report, 10).len(),
            context_header_line_count(&app, &report) + lines.len()
        );
    }

    #[test]
    fn visual_line_offsets_count_wrapped_rows() {
        let lines = vec![
            Line::from("short"),
            Line::from("x".repeat(25)), // wraps to 3 visual rows at width 10
            Line::from("tail"),
        ];
        let offsets = visual_line_offsets(&lines, 10);
        assert_eq!(offsets, vec![0, 1, 4, 5]);
        assert_eq!(visual_line_total(&lines, 10), 5);
    }

    #[test]
    fn context_selected_span_covers_expanded_leaf_preview() {
        let mut app = AppState::new("codex", "gpt-5".to_string(), ".".to_string(), None);
        app.context_state.expanded.insert("msg-0".to_string());
        let report = ContextReport {
            budget_tokens: 120_000,
            ledger: vec![crate::agent::ContextNode {
                id: "msg-0".into(),
                kind: crate::agent::ContextNodeKind::ChatMessage,
                inclusion: crate::agent::ContextInclusion::Included,
                role: Some(crate::agent::ContextRole::User),
                label: "User message 1".to_string(),
                tokens: 10,
                chars: 5,
                bytes: 5,
                source: crate::provider::TokenCounterKind::Heuristic,
                confidence: crate::provider::EstimateConfidence::Low,
                preview: "one\ntwo\nthree".to_string(),
                sources: Vec::new(),
                children: Vec::new(),
            }],
            ..ContextReport::default()
        };

        let header = context_header_line_count(&app, &report);
        let span = context_selected_span(&app, &report);
        // Row line + 3 preview lines.
        assert_eq!(span, Some((header, header + 4)));
    }
}
