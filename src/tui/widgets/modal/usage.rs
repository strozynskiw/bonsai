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
    let header_lines = vec![usage_tab_bar(tab)];
    let body_lines = usage_body_lines(dashboard, tab, usage_inner_width(area));
    let footer_lines = vec![footer_hint_line(USAGE_HINTS)];
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
    let header_lines = vec![usage_tab_bar(tab)];
    let footer_lines = vec![footer_hint_line(USAGE_HINTS)];
    let body_lines = usage_body_lines(dashboard, tab, usage_inner_width(area));
    scrollable_modal_max_scroll(area, &header_lines, &footer_lines, &body_lines)
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

/// Widest row label, fixed so the block stays aligned when a wide row is
/// hidden (`permission wait`).
const RUNTIME_LABEL_WIDTH: usize = "permission wait".len();

/// The runtime breakdown: which clock the runs consumed, then one aligned
/// label/duration/share row per non-zero category.
///
/// The policy has to come first. Sums only mean something against a named clock,
/// and the session budget charges exactly one of them — a report that shows the
/// split without saying which clock was charged is the confusion #146 exists to
/// remove. Shares are of run wall time, so a row reads as "how much of the run
/// this was".
fn runtime_lines(runtime: &crate::storage::RuntimeTiming) -> Vec<Line<'static>> {
    let palette = theme::palette();
    // Read the totals through their own type so the report cannot silently drift
    // from the persisted split.
    let crate::storage::RuntimeTimingTotals {
        wall_ms,
        critical_path_ms,
        ..
    } = runtime.totals;
    let wall_ms = wall_ms.max(0);
    let mut summary = vec![
        Span::styled(" Runtime ", theme::muted()),
        Span::styled(
            format!("clock {}", runtime.clock_label()),
            theme::label(palette.text),
        ),
    ];
    if let Some(percent) = runtime.waiting_share_percent() {
        summary.push(Span::styled(" · waiting ", theme::muted()));
        summary.push(Span::styled(
            format!("{percent}% of run wall"),
            theme::label(palette.text),
        ));
    }
    let mut lines = vec![Line::from(summary)];
    for category in crate::timing::TimingCategory::ALL {
        let ms = runtime.category_ms(category);
        if ms <= 0 {
            continue;
        }
        let role = if category.consumes_active_compute() {
            "compute"
        } else {
            "wait"
        };
        lines.push(runtime_row(category.label(), ms, wall_ms, role));
    }
    // Totals last: they are the denominator, not another category, and zero ones
    // are hidden exactly like zero categories.
    for (label, ms) in [("critical path", critical_path_ms), ("run wall", wall_ms)] {
        if ms > 0 {
            lines.push(runtime_row(label, ms, wall_ms, ""));
        }
    }
    lines
}

fn runtime_row(label: &str, ms: i64, wall_ms: i64, role: &str) -> Line<'static> {
    let palette = theme::palette();
    let share = if wall_ms > 0 {
        ms.saturating_mul(100) / wall_ms
    } else {
        0
    };
    Line::from(vec![
        Span::styled(format!("   {label:<RUNTIME_LABEL_WIDTH$}"), theme::muted()),
        Span::styled(
            format!("{:>9}", format_duration_units(ms)),
            theme::label(palette.text),
        ),
        Span::styled(format!("{share:>5}%"), theme::muted()),
        Span::styled(format!("  {role}"), theme::muted()),
    ])
}

fn inspection_line(stats: &crate::storage::InspectionBatchingStats) -> Line<'static> {
    let palette = theme::palette();
    let share = stats
        .singleton_share_percent()
        .map_or_else(|| "n/a".to_string(), |percent| format!("{percent}%"));
    let mut spans = vec![
        Span::styled(" Inspection ", theme::muted()),
        Span::styled(
            format!(
                "{} singleton groups of {} ({share})",
                stats.singleton_groups, stats.groups
            ),
            theme::label(palette.text),
        ),
    ];
    if stats.longest_singleton_streak > 0 {
        spans.push(Span::styled(
            " · longest cheap-single streak ",
            theme::muted(),
        ));
        spans.push(Span::styled(
            format!("{} turns", stats.longest_singleton_streak),
            theme::label(palette.text),
        ));
    }
    if stats.streaks_of_three_or_more > 0 {
        spans.push(Span::styled(" · 3+ streaks ", theme::muted()));
        spans.push(Span::styled(
            format!("{}", stats.streaks_of_three_or_more),
            theme::label(palette.text),
        ));
    }
    if stats.excess_serial_turns > 0 {
        spans.push(Span::styled(" · excess serial turns ", theme::muted()));
        spans.push(Span::styled(
            format!("{}", stats.excess_serial_turns),
            theme::label(palette.text),
        ));
    }
    if stats.advisories > 0 {
        spans.push(Span::styled(" · advisories ", theme::muted()));
        spans.push(Span::styled(
            format!("{}", stats.advisories),
            theme::label(palette.text),
        ));
    }
    // Why the remaining serial turns happened: a declared dependency is not
    // waste, an unlabelled one still might be.
    let conflict = stats.count(crate::agent::InspectionBatchClass::ConflictForced);
    let unexplained = stats.unexplained_serials();
    if unexplained + conflict + stats.declared_dependent + stats.declared_independent > 0 {
        spans.push(Span::styled(" · serial ", theme::muted()));
        spans.push(Span::styled(
            format!(
                "{unexplained} unexplained · {} declared dependent · {} declared independent · {conflict} conflict-forced",
                stats.declared_dependent, stats.declared_independent
            ),
            theme::label(palette.text),
        ));
    }
    if stats.unreadable_turns > 0 {
        spans.push(Span::styled(" · unreadable rows ", theme::muted()));
        spans.push(Span::styled(
            format!("{}", stats.unreadable_turns),
            theme::label(palette.text),
        ));
    }
    Line::from(spans)
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
    if dashboard.runtime.has_data() {
        lines.extend(runtime_lines(&dashboard.runtime));
    }
    if dashboard.inspection.has_data() {
        lines.push(inspection_line(&dashboard.inspection));
    }
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

    /// Flatten a rendered line into the text a user would read.
    fn line_text(line: Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn runtime_fixture(
        policies: Vec<(crate::timing::TimingPolicy, u64)>,
    ) -> crate::storage::RuntimeTiming {
        crate::storage::RuntimeTiming::new(
            crate::storage::RuntimeTimingTotals {
                wall_ms: 100_000,
                critical_path_ms: 90_000,
                active_compute_ms: 25_000,
                waiting_ms: 75_000,
            },
            vec![
                (crate::timing::TimingCategory::ModelRequest, 20_000),
                (crate::timing::TimingCategory::ToolActive, 5_000),
                (crate::timing::TimingCategory::UserWait, 75_000),
            ],
        )
        .with_policies(policies)
    }

    #[test]
    fn runtime_block_names_the_charged_clock_and_hides_zero_rows() {
        let runtime = runtime_fixture(vec![(crate::timing::TimingPolicy::ActiveCompute, 7)]);
        let rendered: Vec<String> = runtime_lines(&runtime).into_iter().map(line_text).collect();

        assert!(rendered[0].contains("Runtime"), "{rendered:?}");
        // The clock the session budget charges comes first; without it the split
        // below is just three numbers.
        assert!(rendered[0].contains("clock active compute"), "{rendered:?}");
        assert!(rendered[0].contains("75% of run wall"), "{rendered:?}");
        // Three recorded categories plus the two totals; a zero category is hidden
        // rather than printed as a row of noise.
        assert_eq!(rendered.len(), 1 + 3 + 2, "{rendered:?}");
        assert!(
            rendered.iter().any(|line| line.contains("user wait")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("queue")),
            "{rendered:?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("permission wait")),
            "{rendered:?}"
        );
    }

    #[test]
    fn runtime_rows_align_their_duration_and_share_columns() {
        let runtime = runtime_fixture(vec![(crate::timing::TimingPolicy::ActiveCompute, 1)]);
        let lines = runtime_lines(&runtime);
        let rows: Vec<String> = lines[1..].iter().cloned().map(line_text).collect();
        assert!(!rows.is_empty());
        // Same column for every share, and the duration field starts where the
        // padded label ends — otherwise the block reads as prose again.
        let shares: Vec<usize> = rows
            .iter()
            .map(|row| row.find('%').expect("every row carries a share"))
            .collect();
        assert!(shares.windows(2).all(|pair| pair[0] == pair[1]), "{rows:?}");
        assert!(
            rows.iter()
                .all(|row| row.chars().nth(3 + RUNTIME_LABEL_WIDTH) == Some(' ')),
            "{rows:?}"
        );
    }

    #[test]
    fn mixed_policies_are_reported_as_mixed() {
        let mixed = runtime_fixture(vec![
            (crate::timing::TimingPolicy::ActiveCompute, 2),
            (crate::timing::TimingPolicy::WallClock, 1),
        ]);
        assert_eq!(mixed.clock_label(), "mixed");
        assert!(
            line_text(runtime_lines(&mixed).remove(0)).contains("clock mixed"),
            "runs recorded on two clocks must not be reported as one"
        );

        let wall = runtime_fixture(vec![(crate::timing::TimingPolicy::WallClock, 3)]);
        assert_eq!(wall.clock_label(), "wall clock");
        assert_eq!(
            crate::storage::RuntimeTiming::default().clock_label(),
            "unknown"
        );
    }

    #[test]
    fn inspection_line_reports_share_streaks_and_why_serials_happened() {
        let stats = crate::storage::InspectionBatchingStats {
            turns: 10,
            groups: 12,
            singleton_groups: 9,
            longest_singleton_streak: 4,
            streaks_of_three_or_more: 2,
            excess_serial_turns: 3,
            advisories: 1,
            classification: vec![
                (crate::agent::InspectionBatchClass::Unexplained, 6),
                (crate::agent::InspectionBatchClass::ConflictForced, 2),
            ],
            ..Default::default()
        };
        let rendered = line_text(inspection_line(&stats));
        assert!(rendered.contains("Inspection"), "{rendered}");
        assert!(
            rendered.contains("9 singleton groups of 12 (75%)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("longest cheap-single streak 4"),
            "{rendered}"
        );
        assert!(rendered.contains("excess serial turns 3"), "{rendered}");
        assert!(rendered.contains("3+ streaks 2"), "{rendered}");
        assert!(rendered.contains("advisories 1"), "{rendered}");
        // The breakdown is what lets a reader tell a dependent serial from an
        // avoidable one instead of guessing from the streak length.
        assert!(rendered.contains("6 unexplained"), "{rendered}");
        assert!(rendered.contains("2 conflict-forced"), "{rendered}");
    }

    #[test]
    fn inspection_line_degrades_without_a_cohort() {
        let rendered = line_text(inspection_line(&Default::default()));
        assert!(rendered.contains("Inspection"), "{rendered}");
        assert!(rendered.contains("n/a"), "{rendered}");
    }
}
