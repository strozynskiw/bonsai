//! The `/usage` dashboard modal: a tabbed, read-only view over the global
//! usage aggregates in [`UsageDashboard`]. This file owns the shell (tab bar,
//! scroll clamp) and the Activity tab; the other tabs live in `usage_tabs`.

use crate::context_view::telemetry::{compact_tokens, format_cost_micros};
use crate::storage::UsageDashboard;
use crate::tui::event::UsageTab;

use super::common::*;
use super::usage_heatmap::{DAY_LABELS, GUTTER_WIDTH, HeatCell, heatmap_layout};
use super::usage_tabs::usage_tab_lines;
use super::*;

pub(super) fn render_usage_dashboard(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    dashboard: &UsageDashboard,
    tab: UsageTab,
) {
    let header_lines = vec![usage_tab_bar(tab), Line::from("")];
    let body_lines = usage_body_lines(dashboard, tab, usage_inner_width(area));
    let footer_lines = vec![footer_hint_line(&[
        ("Tab/1-6", "switch tab"),
        ("Up/Down", "scroll"),
        ("Esc", "close"),
    ])];
    render_scrollable_modal(
        f,
        area,
        " Usage — all projects ",
        &header_lines,
        &body_lines,
        &footer_lines,
        app.modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
}

pub(super) fn usage_dashboard_max_scroll(
    area: Rect,
    dashboard: &UsageDashboard,
    tab: UsageTab,
) -> u16 {
    let body_lines = usage_body_lines(dashboard, tab, usage_inner_width(area));
    scrollable_modal_max_scroll(area, 2, 1, &body_lines)
}

/// The frame-inner width the body renders into — the heatmap sizes its week
/// count from this, since the modal width is a terminal percentage.
fn usage_inner_width(area: Rect) -> usize {
    theme::frame(String::new(), true).inner(area).width as usize
}

fn usage_tab_bar(active: UsageTab) -> Line<'static> {
    let palette = theme::palette();
    let mut spans = vec![Span::styled(" ", theme::panel())];
    for (index, tab) in UsageTab::ALL.iter().enumerate() {
        let label = format!(" {} {} ", index + 1, tab.title());
        if *tab == active {
            spans.push(Span::styled(
                label,
                theme::block(palette.text, palette.selection_bg).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(label, theme::dim()));
        }
        spans.push(Span::styled(" ", theme::panel()));
    }
    Line::from(spans)
}

fn usage_body_lines(
    dashboard: &UsageDashboard,
    tab: UsageTab,
    inner_width: usize,
) -> Vec<Line<'static>> {
    match tab {
        UsageTab::Activity => activity_lines(dashboard, inner_width),
        other => usage_tab_lines(dashboard, other, inner_width),
    }
}

fn activity_lines(dashboard: &UsageDashboard, inner_width: usize) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let mut lines = Vec::new();

    let mut stats = vec![
        Span::styled(" Lifetime ", theme::muted()),
        Span::styled(
            format!(
                "{} tokens",
                compact_tokens(dashboard.lifetime.total_tokens().max(0) as usize)
            ),
            theme::label(palette.text),
        ),
    ];
    if let Some(peak) = dashboard.peak_day() {
        stats.push(Span::styled(" · Peak ", theme::muted()));
        stats.push(Span::styled(
            format!(
                "{} ({})",
                compact_tokens(peak.total_tokens().max(0) as usize),
                short_day_label(&peak.day)
            ),
            theme::label(palette.text),
        ));
    }
    stats.push(Span::styled(" · Streak ", theme::muted()));
    stats.push(Span::styled(
        format!("{}d", dashboard.current_streak_days()),
        theme::label(palette.text),
    ));
    if dashboard.session_stats.longest_duration_ms > 0 {
        stats.push(Span::styled(" · Longest session ", theme::muted()));
        stats.push(Span::styled(
            format_duration_units(dashboard.session_stats.longest_duration_ms),
            theme::label(palette.text),
        ));
    }
    lines.push(Line::from(stats));
    if dashboard.self_review.runs > 0 {
        let rebuttal_rate = dashboard
            .self_review
            .rebutted
            .saturating_mul(100)
            .checked_div(dashboard.self_review.runs)
            .unwrap_or(0);
        let calibration = if rebuttal_rate > 50 {
            " · review prompt needs calibration"
        } else {
            ""
        };
        lines.push(Line::from(vec![
            Span::styled(" Self-review ", theme::muted()),
            Span::styled(
                format!(
                    "{} runs · {} catches · {} fixed · {} rebutted ({rebuttal_rate}%) · {} findings · {}{calibration}",
                    dashboard.self_review.runs,
                    dashboard.self_review.runs_with_findings,
                    dashboard.self_review.fixed,
                    dashboard.self_review.rebutted,
                    dashboard.self_review.findings,
                    format_duration_units(dashboard.self_review.reviewer_duration_ms),
                ),
                theme::label(palette.text),
            ),
        ]));
    }
    if let Some(warning) = quality_evidence_warning(dashboard.quality_evidence) {
        lines.push(Line::from(Span::styled(
            format!(" Quality evidence · {warning}"),
            theme::body(palette.error),
        )));
    }
    lines.push(Line::from(""));

    let layout = heatmap_layout(&dashboard.days, &dashboard.today, inner_width);

    // Month labels, positioned over their week columns.
    let mut months = " ".repeat(GUTTER_WIDTH);
    for (week, label) in &layout.month_labels {
        let column = GUTTER_WIDTH + week * layout.pitch;
        if column >= months.len() {
            months.push_str(&" ".repeat(column - months.len()));
            months.push_str(label);
        }
    }
    lines.push(Line::from(Span::styled(months, theme::muted())));

    for (weekday, label) in DAY_LABELS.iter().enumerate() {
        let mut spans = vec![Span::styled(format!("{label} "), theme::muted())];
        for week in 0..layout.weeks {
            spans.push(heat_cell_span(layout.grid[week][weekday], layout.pitch));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::from(""));
    let mut legend = vec![
        Span::styled(" ".repeat(GUTTER_WIDTH), theme::panel()),
        Span::styled("Less ", theme::dim()),
        Span::styled("░", theme::dim()),
    ];
    for level in 1..=4 {
        legend.push(heat_cell_span(HeatCell::Level(level), 1));
    }
    legend.push(Span::styled(" More", theme::dim()));
    lines.push(Line::from(legend));
    lines.push(Line::from(Span::styled(
        format!(
            " {} active days · {} model turns · heatmap counts direct turns only (nested subagents show in totals)",
            dashboard.days.len(),
            dashboard.days.iter().map(|day| day.turns).sum::<i64>(),
        ),
        theme::dim(),
    )));
    lines
}

fn quality_evidence_warning(integrity: crate::storage::QualityEvidenceIntegrity) -> Option<String> {
    let verification = integrity.quarantined_verification_runs;
    let self_review = integrity.quarantined_self_review_runs;
    (verification > 0 || self_review > 0).then(|| {
        format!(
            "{verification} verification + {self_review} self-review cross-session duplicate rows quarantined"
        )
    })
}

fn heat_cell_span(cell: HeatCell, pitch: usize) -> Span<'static> {
    let palette = theme::palette();
    let pad = if pitch == 2 { " " } else { "" };
    match cell {
        HeatCell::Future => Span::styled(format!(" {pad}"), theme::panel()),
        HeatCell::Empty => Span::styled(format!("░{pad}"), theme::dim()),
        HeatCell::Level(level) => Span::styled(
            format!("█{pad}"),
            theme::body(theme::mix(
                palette.dim,
                palette.border_active,
                f32::from(level) / 4.0,
            )),
        ),
    }
}

/// `"2026-07-10"` → `"Jul 10"`; falls back to the raw string when it isn't a
/// SQLite date.
fn short_day_label(day: &str) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mut parts = day.splitn(3, '-');
    let (Some(_year), Some(month), Some(dom)) = (parts.next(), parts.next(), parts.next()) else {
        return day.to_string();
    };
    match (month.parse::<usize>(), dom.trim_start_matches('0')) {
        (Ok(month @ 1..=12), dom) if !dom.is_empty() => format!("{} {dom}", MONTHS[month - 1]),
        _ => day.to_string(),
    }
}

/// Human duration from milliseconds: the two largest nonzero units of
/// d/h/m ("12h 27m", "1d 3h", "47m"), dropping to seconds ("42s") and
/// milliseconds ("480ms") for short spans such as tool calls.
pub(super) fn format_duration_units(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let seconds = ms / 1_000;
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        let rest = minutes % 60;
        return if rest > 0 {
            format!("{hours}h {rest}m")
        } else {
            format!("{hours}h")
        };
    }
    let days = hours / 24;
    let rest = hours % 24;
    if rest > 0 {
        format!("{days}d {rest}h")
    } else {
        format!("{days}d")
    }
}

/// A one-line unicode sparkline of `values` scaled to their maximum. Zero
/// values render as a baseline dot so gaps stay visible.
pub(super) fn sparkline(values: &[i64], max_width: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() || max_width == 0 {
        return String::new();
    }
    let start = values.len().saturating_sub(max_width);
    let window = &values[start..];
    let max = window.iter().copied().max().unwrap_or(0).max(1);
    window
        .iter()
        .map(|value| {
            if *value <= 0 {
                '·'
            } else {
                let index = ((*value as u128 * (BARS.len() as u128 - 1)) / max as u128) as usize;
                BARS[index.min(BARS.len() - 1)]
            }
        })
        .collect()
}

/// Cost with a trailing `~` marker when part of the total is unknown.
pub(super) fn approximate_cost(cost_micros: i64, unknown: i64) -> String {
    let cost = format_cost_micros(cost_micros.max(0) as u64);
    if unknown > 0 {
        format!("{cost}~")
    } else {
        cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_units_pick_two_largest() {
        assert_eq!(format_duration_units(480), "480ms");
        assert_eq!(format_duration_units(42_000), "42s");
        assert_eq!(format_duration_units(47 * 60_000), "47m");
        assert_eq!(format_duration_units(44_820_000), "12h 27m");
        assert_eq!(format_duration_units(27 * 3_600_000), "1d 3h");
        assert_eq!(format_duration_units(24 * 3_600_000), "1d");
        assert_eq!(format_duration_units(-5), "0ms");
    }

    #[test]
    fn sparkline_scales_and_marks_gaps() {
        assert_eq!(sparkline(&[0, 1, 4, 8], 10), "·▁▄█");
        // Wider than the budget: keeps the most recent window.
        assert_eq!(sparkline(&[9, 9, 1, 8], 2), "▁█");
        assert_eq!(sparkline(&[], 10), "");
    }

    #[test]
    fn short_day_labels_are_human() {
        assert_eq!(short_day_label("2026-07-10"), "Jul 10");
        assert_eq!(short_day_label("2026-01-05"), "Jan 5");
        assert_eq!(short_day_label("not-a-date"), "not-a-date");
    }

    #[test]
    fn quality_evidence_warning_is_visible_only_for_quarantined_rows() {
        assert_eq!(
            quality_evidence_warning(crate::storage::QualityEvidenceIntegrity::default()),
            None
        );
        assert_eq!(
            quality_evidence_warning(crate::storage::QualityEvidenceIntegrity {
                quarantined_verification_runs: 7,
                quarantined_self_review_runs: 2,
            })
            .as_deref(),
            Some("7 verification + 2 self-review cross-session duplicate rows quarantined")
        );
    }
}
