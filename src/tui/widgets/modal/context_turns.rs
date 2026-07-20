//! Turns view for the `/ctx` modal: one row per provider turn with cache
//! read/write, cost, and latency, compaction events interleaved
//! chronologically, and a per-turn cache diagnosis expandable in place.

use crate::context_view::cache_diagnosis::{
    CacheBreakCause, TurnCacheAssessment, TurnCacheDiagnosis, diagnose_turns, short_hash,
};
use crate::context_view::telemetry::{
    CostTelemetry, compact_tokens, optional_cost_micros, turn_cache_percent,
};
use crate::context_view::{CompactionEvent, UsageTurnReport};

use super::*;

/// Display order of the Turns view body: turn rows (selectable, expandable)
/// with compaction-event rows (display-only) slotted between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContextTurnRowKind {
    /// Index into `report.usage_turns`.
    Turn { turn_index: usize },
    /// Index into `report.compaction_events`.
    Compaction { event_index: usize },
}

/// Interleave the visible turn window with compaction events. Events carry a
/// wall-clock timestamp; each lands before the first turn recorded at or after
/// it. Legacy persisted events with zeroed timestamps fall back to the first
/// unclaimed turn that recorded a compaction rewrite, else the end.
pub(super) fn context_turns_row_plan(report: &ContextReport) -> Vec<ContextTurnRowKind> {
    use crate::agent::ContextRewriteKind;

    let skip = report
        .usage_turns
        .len()
        .saturating_sub(crate::tui::app::CONTEXT_TURNS_ROW_LIMIT);
    let turn_indices: Vec<usize> = (skip..report.usage_turns.len()).collect();

    let mut insertions: Vec<Vec<usize>> = vec![Vec::new(); turn_indices.len() + 1];
    let mut claimed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (event_index, event) in report.compaction_events.iter().enumerate() {
        let position = if event.occurred_at_ms > 0 {
            turn_indices
                .iter()
                .position(|&turn_index| {
                    let turn = &report.usage_turns[turn_index];
                    turn.created_at_ms > 0 && turn.created_at_ms >= event.occurred_at_ms
                })
                .unwrap_or(turn_indices.len())
        } else {
            let fallback = turn_indices.iter().enumerate().position(|(pos, &ti)| {
                report.usage_turns[ti].rewrite_kind == ContextRewriteKind::Compaction
                    && !claimed.contains(&pos)
            });
            if let Some(pos) = fallback {
                claimed.insert(pos);
            }
            fallback.unwrap_or(turn_indices.len())
        };
        insertions[position].push(event_index);
    }

    let mut plan = Vec::new();
    for (position, &turn_index) in turn_indices.iter().enumerate() {
        for &event_index in &insertions[position] {
            plan.push(ContextTurnRowKind::Compaction { event_index });
        }
        plan.push(ContextTurnRowKind::Turn { turn_index });
    }
    for &event_index in &insertions[turn_indices.len()] {
        plan.push(ContextTurnRowKind::Compaction { event_index });
    }
    plan
}

/// Planned rows with each row's line offset (relative to the first Turns body
/// line) and height, so rendering, scroll metrics, and click mapping share a
/// single geometry.
pub(super) fn context_turns_layout(
    app: &AppState,
    report: &ContextReport,
) -> Vec<(ContextTurnRowKind, usize, usize)> {
    let diagnoses = diagnose_turns(report);
    let plan = context_turns_row_plan(report);
    let mut rows = Vec::with_capacity(plan.len());
    let mut line = context_turns_preamble_line_count(report);
    for kind in plan {
        let height = match kind {
            ContextTurnRowKind::Turn { turn_index } => {
                let turn = &report.usage_turns[turn_index];
                if app.context_state.turns_expanded.contains(&turn.seq) {
                    1 + context_turn_detail_lines(turn, &diagnoses[turn_index]).len()
                } else {
                    1
                }
            }
            ContextTurnRowKind::Compaction { .. } => 1,
        };
        rows.push((kind, line, height));
        line += height;
    }
    rows
}

/// Lines before the first table row: session summary, blank, and either the
/// column caption (plus a cap note when older turns are hidden) or the empty
/// notice.
pub(super) fn context_turns_preamble_line_count(report: &ContextReport) -> usize {
    if report.usage_turns.is_empty() {
        return 3;
    }
    let capped = report.usage_turns.len() > crate::tui::app::CONTEXT_TURNS_ROW_LIMIT;
    3 + usize::from(capped)
}

pub(super) fn context_turns_lines(app: &AppState, report: &ContextReport) -> Vec<Line<'static>> {
    let p = theme::palette();
    let telemetry = CostTelemetry::from_report(report);
    let mut lines = vec![
        Line::from(Span::styled(
            format!("session  {}", telemetry.session_summary_line(report)),
            theme::dim(),
        )),
        Line::from(""),
    ];
    if report.usage_turns.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no provider turns recorded yet)",
            theme::dim(),
        )));
        return lines;
    }
    lines.push(Line::from(Span::styled(
        format!(
            "    {:>3}  {:<9} {:<14} {:<13} {:<9} {:<10} note",
            "#", "age", "in/out", "cache", "cost", "lat/ttft"
        ),
        theme::muted(),
    )));
    let hidden = report
        .usage_turns
        .len()
        .saturating_sub(crate::tui::app::CONTEXT_TURNS_ROW_LIMIT);
    if hidden > 0 {
        lines.push(Line::from(Span::styled(
            format!("    ({hidden} earlier turns hidden)"),
            theme::dim(),
        )));
    }

    let diagnoses = diagnose_turns(report);
    let layout = context_turns_layout(app, report);
    let selected = app
        .context_state
        .turns_cursor
        .min(turn_row_count(&layout).saturating_sub(1));
    let mut turn_ordinal = 0usize;
    for (kind, _start, _height) in &layout {
        match *kind {
            ContextTurnRowKind::Turn { turn_index } => {
                let turn = &report.usage_turns[turn_index];
                let diagnosis = &diagnoses[turn_index];
                lines.push(context_turn_row_line(
                    app,
                    turn,
                    diagnosis,
                    turn_ordinal == selected,
                ));
                if app.context_state.turns_expanded.contains(&turn.seq) {
                    lines.extend(context_turn_detail_lines(turn, diagnosis));
                }
                turn_ordinal += 1;
            }
            ContextTurnRowKind::Compaction { event_index } => {
                lines.push(context_compaction_row_line(
                    &report.compaction_events[event_index],
                    p,
                ));
            }
        }
    }
    lines
}

fn turn_row_count(layout: &[(ContextTurnRowKind, usize, usize)]) -> usize {
    layout
        .iter()
        .filter(|(kind, ..)| matches!(kind, ContextTurnRowKind::Turn { .. }))
        .count()
}

fn context_turn_row_line(
    app: &AppState,
    turn: &UsageTurnReport,
    diagnosis: &TurnCacheDiagnosis,
    selected: bool,
) -> Line<'static> {
    let p = theme::palette();
    let selector = if selected { ">" } else { " " };
    let expand = if app.context_state.turns_expanded.contains(&turn.seq) {
        "-"
    } else {
        "+"
    };
    let (cache_text, cache_style) = cache_cell(turn);
    let (note, note_style) = turn_note(turn, diagnosis);
    Line::from(vec![
        Span::styled(format!("{selector} {expand} "), theme::dim()),
        Span::styled(format!("{:>3}  ", turn.seq), theme::body(p.text)),
        Span::styled(
            format!("{:<9} ", turn_age(turn.created_at_ms)),
            theme::dim(),
        ),
        Span::styled(format!("{:<14} ", io_cell(turn)), theme::body(p.text)),
        Span::styled(format!("{cache_text:<13} "), cache_style),
        Span::styled(
            format!("{:<9} ", optional_cost_micros(turn.turn_cost_micros)),
            theme::dim(),
        ),
        Span::styled(format!("{:<10} ", latency_cell(turn)), theme::dim()),
        Span::styled(note, note_style),
    ])
}

/// Compaction event row, chronologically slotted between turn rows.
fn context_compaction_row_line(
    event: &CompactionEvent,
    p: &crate::tui::theme::Palette,
) -> Line<'static> {
    let mut parts = vec![format!(
        "compaction #{} · {}→{} tok",
        event.seq,
        compact_tokens(event.before_tokens),
        compact_tokens(event.after_tokens)
    )];
    let mut changes = Vec::new();
    if event.messages_omitted > 0 {
        changes.push(format!("{} summarized", event.messages_omitted));
    }
    if event.tool_outputs_stubbed > 0 {
        changes.push(format!("{} stubbed", event.tool_outputs_stubbed));
    }
    if !changes.is_empty() {
        parts.push(changes.join(", "));
    }
    if let (Some(before), Some(after)) = (
        event.cacheable_prefix_tokens_before,
        event.cacheable_prefix_tokens_after,
    ) {
        parts.push(format!(
            "prefix {}→{}",
            compact_tokens(before),
            compact_tokens(after)
        ));
    }
    if let (Some(before), Some(after)) = (
        event.prefix_hash_before.as_deref(),
        event.prefix_hash_after.as_deref(),
    ) {
        parts.push(format!("hash {}→{}", short_hash(before), short_hash(after)));
    }
    if let Some(reason) = event.repack_reason.as_deref() {
        parts.push(reason.to_string());
    }
    if event.summary_available {
        parts.push("R".to_string());
    }
    Line::from(vec![
        Span::styled("    ── ", theme::label(p.progress)),
        Span::styled(parts.join(" · "), theme::muted()),
    ])
}

/// Expanded per-turn detail: the diagnosis sentence, estimate-vs-actual
/// reconciliation, and the cache-prefix figures this turn carried.
pub(super) fn context_turn_detail_lines(
    turn: &UsageTurnReport,
    diagnosis: &TurnCacheDiagnosis,
) -> Vec<Line<'static>> {
    let p = theme::palette();
    let indent = "       ";
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{indent}└ "), theme::dim()),
        Span::styled(diagnosis.summary.clone(), theme::body(p.muted)),
    ])];
    lines.push(Line::from(vec![
        Span::styled(format!("{indent}  "), theme::dim()),
        Span::styled(
            format!(
                "lane {}:{} #{}{}{}",
                turn.lane_kind.as_db_str(),
                turn.lane_id,
                turn.lane_seq,
                turn.parent_tool_call_id
                    .as_deref()
                    .map(|id| format!(" · parent tool {id}"))
                    .unwrap_or_default(),
                turn.launch_group_id
                    .as_deref()
                    .map(|id| format!(" · group {id}"))
                    .unwrap_or_default()
            ),
            theme::dim(),
        ),
    ]));
    if let Some(estimate_line) = estimate_detail(turn) {
        lines.push(Line::from(vec![
            Span::styled(format!("{indent}  "), theme::dim()),
            Span::styled(estimate_line, theme::dim()),
        ]));
    }
    if let Some(prefix_line) = prefix_detail(turn) {
        lines.push(Line::from(vec![
            Span::styled(format!("{indent}  "), theme::dim()),
            Span::styled(prefix_line, theme::dim()),
        ]));
    }
    if let Some(wire_line) = wire_detail(turn) {
        lines.push(Line::from(vec![
            Span::styled(format!("{indent}  "), theme::dim()),
            Span::styled(wire_line, theme::dim()),
        ]));
    }
    lines
}

/// `est 41.8k (tiktoken/high confidence) vs actual 42.1k (+312)` — the
/// per-turn form of the estimate reconciliation.
fn estimate_detail(turn: &UsageTurnReport) -> Option<String> {
    let estimated = turn.estimated_prompt_tokens?;
    let mut text = format!("est {}", compact_tokens(estimated));
    if let Some(source) = turn.estimate_source {
        let confidence = turn
            .estimate_confidence
            .map(|confidence| format!("/{}", confidence.label()))
            .unwrap_or_default();
        text.push_str(&format!(" ({}{confidence})", source.label()));
    }
    if let Some(actual) = turn.cache_measured_input_tokens.or(turn.prompt_tokens) {
        let delta = actual as i64 - estimated as i64;
        let sign = if delta >= 0 { "+" } else { "" };
        text.push_str(&format!(
            " vs actual {} ({sign}{delta})",
            compact_tokens(actual as usize)
        ));
    }
    Some(text)
}

fn prefix_detail(turn: &UsageTurnReport) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(hash) = turn.prefix_hash.as_deref() {
        parts.push(format!("prefix hash {}", short_hash(hash)));
    }
    if let Some(cacheable) = turn.cacheable_prefix_tokens {
        parts.push(format!("cacheable ~{}", compact_tokens(cacheable)));
    }
    if let Some(volatile) = turn.volatile_tail_tokens {
        parts.push(format!("volatile ~{}", compact_tokens(volatile)));
    }
    if let Some(window) = turn.context_window_tokens {
        parts.push(format!("window {}", compact_tokens(window)));
    }
    if let Some(schema) = turn.tool_schema_tokens {
        let hash = turn
            .tool_schema_hash
            .as_deref()
            .map(|hash| format!(" {}", short_hash(hash)))
            .unwrap_or_default();
        parts.push(format!("schema {}{hash}", compact_tokens(schema)));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn wire_detail(turn: &UsageTurnReport) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(bytes) = turn.request_body_bytes {
        let hash = turn
            .request_body_hash
            .as_deref()
            .map(|hash| format!(" {}", short_hash(hash)))
            .unwrap_or_default();
        parts.push(format!("request {}{hash}", format_bytes(bytes)));
    }
    if let Some(mechanism) = turn.cache_mechanism.as_deref() {
        parts.push(format!("cache hint {mechanism}"));
    }
    if let Some(expected) = turn.expected_cacheable_percent {
        let actual = turn
            .actual_cache_read_percent
            .map(|percent| format!("{percent}%"))
            .unwrap_or_else(|| "n/a".to_string());
        parts.push(format!("cache expected {expected}% vs read {actual}"));
    }
    if let Some(route) = turn.cache_route_fingerprint.as_deref() {
        parts.push(format!("route {}", short_hash(route)));
    }
    if let Some(percent) = turn.local_reusable_prefix_percent {
        parts.push(format!("wire prefix {percent}% bytes"));
    }
    if !turn.tool_schema_names.is_empty() {
        parts.push(format!(
            "tools {}",
            compact_tool_names(&turn.tool_schema_names)
        ));
    }
    let inspections = turn
        .inspection_executed
        .saturating_add(turn.inspection_reused)
        .saturating_add(turn.inspection_rejected);
    if inspections > 0 || turn.delegated_parent_overlap > 0 {
        parts.push(format!(
            "inspection exec {} / reuse {} / reject {} · returned {} chars · avoided {} chars · delegated overlap {}",
            turn.inspection_executed,
            turn.inspection_reused,
            turn.inspection_rejected,
            turn.inspection_returned_chars,
            turn.inspection_avoided_chars,
            turn.delegated_parent_overlap
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn compact_tool_names(names: &[String]) -> String {
    const MAX_NAMES: usize = 5;
    let mut shown = names.iter().take(MAX_NAMES).cloned().collect::<Vec<_>>();
    if names.len() > MAX_NAMES {
        shown.push(format!("+{} more", names.len() - MAX_NAMES));
    }
    shown.join(", ")
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        return format!("{kib:.1} KiB");
    }
    format!("{:.1} MiB", kib / 1024.0)
}

fn io_cell(turn: &UsageTurnReport) -> String {
    let input = turn
        .cache_measured_input_tokens
        .or(turn.prompt_tokens)
        .map(|tokens| compact_tokens(tokens as usize))
        .unwrap_or_else(|| "n/a".to_string());
    let output = turn
        .completion_tokens
        .map(|tokens| compact_tokens(tokens as usize))
        .unwrap_or_else(|| "n/a".to_string());
    format!("↑{input} ↓{output}")
}

fn cache_cell(turn: &UsageTurnReport) -> (String, Style) {
    let p = theme::palette();
    let Some(percent) = turn_cache_percent(turn) else {
        return ("n/a".to_string(), theme::dim());
    };
    let mut text = format!("{percent:>3}%");
    if let Some(written) = turn
        .cache_creation_input_tokens
        .filter(|written| *written > 0)
    {
        text.push_str(&format!(" +{}w", compact_tokens(written as usize)));
    }
    let style = if percent >= 80 {
        theme::body(p.success)
    } else if percent > 0 {
        theme::body(p.todo)
    } else {
        theme::body(p.error)
    };
    (text, style)
}

fn latency_cell(turn: &UsageTurnReport) -> String {
    let Some(latency) = turn.latency_ms else {
        return "n/a".to_string();
    };
    let ttft = turn
        .ttft_ms
        .map(format_ms)
        .unwrap_or_else(|| "?".to_string());
    format!("{}/{ttft}", format_ms(latency))
}

fn format_ms(ms: u64) -> String {
    if ms >= 10_000 {
        format!("{}s", ms / 1_000)
    } else if ms >= 1_000 {
        format!("{}.{}s", ms / 1_000, (ms % 1_000) / 100)
    } else {
        format!("{ms}ms")
    }
}

fn turn_age(created_at_ms: i64) -> String {
    if created_at_ms <= 0 {
        return "n/a".to_string();
    }
    super::picker_common::relative_age(created_at_ms)
}

/// Short note column: assessment + attributed cause; the full sentence lives
/// in the expanded detail.
fn turn_note(turn: &UsageTurnReport, diagnosis: &TurnCacheDiagnosis) -> (String, Style) {
    use crate::agent::UsageTurnStatus;
    let p = theme::palette();
    if matches!(
        turn.finish_reason.as_ref(),
        Some(crate::provider::FinishReason::Length)
    ) {
        return (
            format!(
                "LENGTH · reasoning {} chars",
                compact_tokens(turn.reasoning_chars)
            ),
            theme::body(p.error),
        );
    }
    match &diagnosis.assessment {
        TurnCacheAssessment::FirstTurn => ("first turn".to_string(), theme::dim()),
        TurnCacheAssessment::Warm { .. } => ("warm".to_string(), theme::body(p.success)),
        TurnCacheAssessment::Partial { cause, .. } => (
            format!("partial · {}", short_cause(turn, cause)),
            theme::body(p.todo),
        ),
        TurnCacheAssessment::Cold { cause } => (
            format!("COLD · {}", short_cause(turn, cause)),
            theme::body(p.error),
        ),
        TurnCacheAssessment::NoCacheData => ("cache n/a".to_string(), theme::dim()),
        TurnCacheAssessment::NoUsage => match turn.status {
            UsageTurnStatus::Interrupted => ("interrupted".to_string(), theme::muted()),
            _ => ("missing usage".to_string(), theme::muted()),
        },
    }
}

fn short_cause(turn: &UsageTurnReport, cause: &CacheBreakCause) -> String {
    match cause {
        CacheBreakCause::Rewrite(kind) => {
            let saved = turn
                .rewrite_saved_tokens
                .filter(|saved| *saved > 0)
                .map(|saved| format!(" -{}", compact_tokens(saved)))
                .unwrap_or_default();
            format!("{}{saved}", kind.as_db_str())
        }
        CacheBreakCause::Compaction { event_seq } => format!("compaction #{event_seq}"),
        CacheBreakCause::PrefixChurn => "prefix changed".to_string(),
        CacheBreakCause::IdleExpiry { .. } => "idle expiry".to_string(),
        CacheBreakCause::RouteChanged => "route changed".to_string(),
        CacheBreakCause::RequestPrefixChanged { reusable_percent } => {
            format!("wire prefix {reusable_percent}%")
        }
        CacheBreakCause::BackendMiss { .. } => "backend miss".to_string(),
        CacheBreakCause::Unknown => "cause unknown".to_string(),
    }
}

/// Absolute logical-line span of the selected turn row (for auto-scroll),
/// header included; the end (exclusive) covers the expanded per-turn detail.
pub(super) fn context_turns_selected_span(
    app: &AppState,
    report: &ContextReport,
) -> Option<(usize, usize)> {
    let layout = context_turns_layout(app, report);
    let selected = app
        .context_state
        .turns_cursor
        .min(turn_row_count(&layout).saturating_sub(1));
    let header = context_header_line_count(app, report);
    let mut turn_ordinal = 0usize;
    for (kind, start, height) in &layout {
        if matches!(kind, ContextTurnRowKind::Turn { .. }) {
            if turn_ordinal == selected {
                let start = header.saturating_add(*start);
                return Some((start, start.saturating_add(*height)));
            }
            turn_ordinal += 1;
        }
    }
    None
}

/// Map a body line back to the ordinal of the turn row at that line (for
/// mouse clicks). Compaction rows and detail lines resolve to nothing.
pub(super) fn context_turns_row_index_at(
    app: &AppState,
    report: &ContextReport,
    target_line: usize,
) -> Option<usize> {
    let layout = context_turns_layout(app, report);
    let header = context_header_line_count(app, report);
    let target = target_line.checked_sub(header)?;
    let mut turn_ordinal = 0usize;
    for (kind, start, _height) in &layout {
        if matches!(kind, ContextTurnRowKind::Turn { .. }) {
            if target == *start {
                return Some(turn_ordinal);
            }
            turn_ordinal += 1;
        }
    }
    None
}
