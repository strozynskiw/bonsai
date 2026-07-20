//! Body lines for the non-Activity `/usage` tabs: Models, Cost, Sessions,
//! Tools, and Cache. Pure `Vec<Line>` builders over [`UsageDashboard`] so the
//! shell in `usage.rs` renders and scroll-clamps them uniformly.

use crate::context_view::telemetry::{compact_tokens, format_cost_micros};
use crate::storage::{ModelUsage, UsageDashboard};
use crate::tui::event::UsageTab;

use super::picker_common::relative_age;
use super::usage::{approximate_cost, format_duration_units, sparkline};
use super::*;

pub(super) fn usage_tab_lines(
    dashboard: &UsageDashboard,
    tab: UsageTab,
    inner_width: usize,
) -> Vec<Line<'static>> {
    match tab {
        UsageTab::Activity => Vec::new(), // rendered by `usage.rs`
        UsageTab::Models => model_lines(dashboard, inner_width),
        UsageTab::Cost => cost_lines(dashboard, inner_width),
        UsageTab::Sessions => session_lines(dashboard),
        UsageTab::Tools => tool_lines(dashboard),
        UsageTab::Cache => cache_lines(dashboard, inner_width),
    }
}

fn tokens_label(tokens: i64) -> String {
    compact_tokens(tokens.max(0) as usize)
}

fn metric_line(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!(" {label:<16}"), theme::muted()),
        Span::styled(value, theme::body(theme::palette().text)),
    ])
}

fn section_line(title: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {title}"),
        theme::label(theme::palette().tool),
    ))
}

fn empty_notice(text: &'static str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(format!(" {text}"), theme::dim()))]
}

fn model_lines(dashboard: &UsageDashboard, inner_width: usize) -> Vec<Line<'static>> {
    if dashboard.models.is_empty() {
        return empty_notice("No recorded model usage yet.");
    }
    let mut lines = Vec::new();
    let name_width = dashboard
        .models
        .iter()
        .map(|model| model_label(model).len())
        .chain(std::iter::once("MODEL".len()))
        .max()
        .unwrap_or(0)
        .min(inner_width.saturating_sub(46).max(12));

    let header = format!(
        " {:<name_width$}  {:>7}  {:>7}  {:>10}  {:>4}  {:>5}  LAST USED",
        "MODEL", "IN", "OUT", "COST", "SESS", "CACHE"
    );
    lines.push(Line::from(Span::styled(
        header,
        theme::muted().add_modifier(Modifier::BOLD),
    )));
    for model in &dashboard.models {
        let mut label = model_label(model);
        label.truncate(name_width);
        let row = format!(
            " {:<name_width$}  {:>7}  {:>7}  {:>10}  {:>4}  {:>5}  {}",
            label,
            tokens_label(model.input_tokens),
            tokens_label(model.output_tokens),
            approximate_cost(model.cost_micros, model.unknown_cost_turns),
            model.sessions,
            model
                .cache_hit_percent()
                .map(|percent| format!("{percent}%"))
                .unwrap_or_else(|| "n/a".to_string()),
            relative_age(model.last_used_ms),
        );
        lines.push(Line::from(Span::styled(
            row,
            theme::body(theme::palette().text),
        )));
    }

    let fallback_turns: i64 = dashboard
        .models
        .iter()
        .map(|model| model.fallback_attributed_turns)
        .sum();
    if fallback_turns > 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                " ≈ {fallback_turns} older turns attributed to each session's last-used model (recorded before per-turn attribution)"
            ),
            theme::dim(),
        )));
    }
    if dashboard
        .models
        .iter()
        .any(|model| model.unknown_cost_turns > 0)
    {
        lines.push(Line::from(Span::styled(
            " ~ cost is a floor: some turns had no pricing data",
            theme::dim(),
        )));
    }
    lines
}

fn model_label(model: &ModelUsage) -> String {
    if model.provider_id.is_empty() {
        model.model.clone()
    } else {
        format!("{} ({})", model.model, model.provider_id)
    }
}

fn cost_lines(dashboard: &UsageDashboard, inner_width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let lifetime = &dashboard.lifetime;
    lines.push(metric_line(
        "Total spend",
        approximate_cost(lifetime.cost_micros, lifetime.unknown_cost_sessions),
    ));
    if lifetime.unknown_cost_sessions > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "                  {} sessions had unpriced turns; their spend is not counted",
                lifetime.unknown_cost_sessions
            ),
            theme::dim(),
        )));
    }

    let window_cost = |window: i64| -> i64 {
        dashboard
            .window_days(window)
            .map(|day| day.cost_micros)
            .sum()
    };
    let cost_30d = window_cost(30);
    lines.push(metric_line(
        "Last 30 days",
        format!(
            "{} ({}/day)",
            format_cost_micros(cost_30d.max(0) as u64),
            format_cost_micros((cost_30d.max(0) / 30) as u64)
        ),
    ));
    lines.push(metric_line(
        "Last 7 days",
        format_cost_micros(window_cost(7).max(0) as u64),
    ));

    let savings = dashboard.savings_micros();
    let no_cache_total = dashboard
        .days
        .iter()
        .map(|day| day.cost_micros + day.savings_micros)
        .sum::<i64>();
    let savings_percent = if no_cache_total > 0 {
        savings.saturating_mul(100) / no_cache_total
    } else {
        0
    };
    lines.push(metric_line(
        "Cache savings",
        format!(
            "{} ({savings_percent}% of no-cache cost)",
            format_cost_micros(savings.max(0) as u64)
        ),
    ));

    lines.push(Line::from(""));
    lines.push(section_line("Spend, last 30 days"));
    let daily_cost = trailing_daily_series(dashboard, 30, |day| day.cost_micros);
    let peak = daily_cost.iter().copied().max().unwrap_or(0);
    lines.push(Line::from(vec![
        Span::styled(" ", theme::panel()),
        Span::styled(
            sparkline(&daily_cost, inner_width.saturating_sub(2)),
            theme::body(theme::palette().border_active),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        format!(" peak {}/day", format_cost_micros(peak.max(0) as u64)),
        theme::dim(),
    )));

    lines.push(Line::from(""));
    lines.push(section_line("Most expensive sessions"));
    if dashboard.top_sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            " No priced sessions yet.",
            theme::dim(),
        )));
    }
    for session in &dashboard.top_sessions {
        let project = std::path::Path::new(&session.project_path)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| session.project_path.clone());
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    " {:>9}  ",
                    format_cost_micros(session.cost_micros.max(0) as u64)
                ),
                theme::body(theme::palette().text),
            ),
            Span::styled(
                format!("{} ({project})", session.name),
                theme::body(theme::palette().text),
            ),
            Span::styled(
                format!(
                    " · {} · {} tokens · {}",
                    session.model,
                    tokens_label(session.token_count),
                    relative_age(session.started_at_ms)
                ),
                theme::muted(),
            ),
        ]));
    }
    lines
}

fn session_lines(dashboard: &UsageDashboard) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let statuses = dashboard
        .status_counts
        .iter()
        .map(|(status, count)| format!("{count} {}", status.label()))
        .collect::<Vec<_>>()
        .join(" · ");
    let total: i64 = dashboard.status_counts.iter().map(|(_, count)| count).sum();
    lines.push(metric_line("Sessions", format!("{total} total")));
    if !statuses.is_empty() {
        lines.push(metric_line("Statuses", statuses));
    }
    lines.push(metric_line(
        "Avg duration",
        format_duration_units(dashboard.session_stats.avg_duration_ms),
    ));
    if dashboard.session_stats.longest_duration_ms > 0 {
        lines.push(metric_line(
            "Longest",
            format!(
                "{} — {}",
                format_duration_units(dashboard.session_stats.longest_duration_ms),
                dashboard.session_stats.longest_session_name
            ),
        ));
    }

    lines.push(Line::from(""));
    lines.push(section_line("Top projects"));
    if dashboard.projects.is_empty() {
        lines.push(Line::from(Span::styled(" No projects yet.", theme::dim())));
        return lines;
    }
    let name_width = dashboard
        .projects
        .iter()
        .map(|project| project.name.len())
        .chain(std::iter::once("PROJECT".len()))
        .max()
        .unwrap_or(0)
        .min(40);
    lines.push(Line::from(Span::styled(
        format!(
            " {:<name_width$}  {:>5}  {:>8}  {:>10}",
            "PROJECT", "SESS", "TOKENS", "COST"
        ),
        theme::muted().add_modifier(Modifier::BOLD),
    )));
    for project in &dashboard.projects {
        let mut name = project.name.clone();
        name.truncate(name_width);
        lines.push(Line::from(Span::styled(
            format!(
                " {:<name_width$}  {:>5}  {:>8}  {:>10}",
                name,
                project.sessions,
                tokens_label(project.tokens),
                format_cost_micros(project.cost_micros.max(0) as u64)
            ),
            theme::body(theme::palette().text),
        )));
    }
    lines
}

fn tool_lines(dashboard: &UsageDashboard) -> Vec<Line<'static>> {
    if dashboard.tools.is_empty() {
        return empty_notice("No finished tool calls yet.");
    }
    let mut lines = Vec::new();
    let name_width = dashboard
        .tools
        .iter()
        .map(|tool| tool.name.len())
        .chain(std::iter::once("TOOL".len()))
        .max()
        .unwrap_or(0)
        .min(32);
    lines.push(Line::from(Span::styled(
        format!(
            " {:<name_width$}  {:>7}  {:>5}  {:>7}  {:>7}",
            "TOOL", "CALLS", "FAIL%", "AVG", "MAX"
        ),
        theme::muted().add_modifier(Modifier::BOLD),
    )));
    for tool in &dashboard.tools {
        let fail_percent = if tool.calls > 0 {
            tool.failed.saturating_mul(100) / tool.calls
        } else {
            0
        };
        let mut name = tool.name.clone();
        name.truncate(name_width);
        let style = if fail_percent >= 20 {
            theme::body(theme::palette().error)
        } else {
            theme::body(theme::palette().text)
        };
        lines.push(Line::from(Span::styled(
            format!(
                " {:<name_width$}  {:>7}  {:>4}%  {:>7}  {:>7}",
                name,
                tool.calls,
                fail_percent,
                format_duration_units(tool.avg_duration_ms),
                format_duration_units(tool.max_duration_ms)
            ),
            style,
        )));
    }
    lines
}

fn cache_lines(dashboard: &UsageDashboard, inner_width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let read: i64 = dashboard.days.iter().map(|day| day.cache_read_tokens).sum();
    let measured: i64 = dashboard
        .days
        .iter()
        .map(|day| day.cache_measured_tokens)
        .sum();
    let hit_rate = if measured > 0 {
        format!(
            "{}% lifetime ({} read / {} measured)",
            read.saturating_mul(100) / measured,
            tokens_label(read),
            tokens_label(measured)
        )
    } else {
        "n/a — no provider reported cache usage yet".to_string()
    };
    lines.push(metric_line("Hit rate", hit_rate));
    lines.push(metric_line(
        "Savings",
        format_cost_micros(dashboard.savings_micros().max(0) as u64),
    ));

    let cold: i64 = dashboard.days.iter().map(|day| day.cold_turns).sum();
    let eligible: i64 = dashboard
        .days
        .iter()
        .map(|day| day.warm_eligible_turns)
        .sum();
    if eligible > 0 {
        lines.push(metric_line(
            "Cache breaks",
            format!(
                "{cold} cold turns of {eligible} eligible ({}%)",
                cold.saturating_mul(100) / eligible
            ),
        ));
    }

    lines.push(Line::from(""));
    lines.push(section_line("Hit rate by week (26w)"));
    let weekly = weekly_buckets(dashboard, 26);
    let weekly_rate: Vec<i64> = weekly
        .iter()
        .map(|(read, measured, _)| {
            if *measured > 0 {
                read.saturating_mul(100) / measured
            } else {
                0
            }
        })
        .collect();
    lines.push(Line::from(vec![
        Span::styled(" ", theme::panel()),
        Span::styled(
            sparkline(&weekly_rate, inner_width.saturating_sub(2)),
            theme::body(theme::palette().success),
        ),
    ]));

    lines.push(Line::from(""));
    lines.push(section_line("Savings by week"));
    let weekly_savings: Vec<i64> = weekly.iter().map(|(_, _, savings)| *savings).collect();
    let peak = weekly_savings.iter().copied().max().unwrap_or(0);
    lines.push(Line::from(vec![
        Span::styled(" ", theme::panel()),
        Span::styled(
            sparkline(&weekly_savings, inner_width.saturating_sub(2)),
            theme::body(theme::palette().border_active),
        ),
    ]));
    lines.push(Line::from(Span::styled(
        format!(" peak {}/week", format_cost_micros(peak.max(0) as u64)),
        theme::dim(),
    )));
    lines
}

/// Daily values for the trailing `window` days, zero-filled so sparklines show
/// real gaps; oldest first.
fn trailing_daily_series(
    dashboard: &UsageDashboard,
    window: i64,
    value: impl Fn(&crate::storage::DailyUsage) -> i64,
) -> Vec<i64> {
    let first = dashboard.today.julian_day - window + 1;
    let mut series = vec![0; window.max(0) as usize];
    for day in &dashboard.days {
        if day.julian_day >= first && day.julian_day <= dashboard.today.julian_day {
            series[(day.julian_day - first) as usize] = value(day);
        }
    }
    series
}

/// `(cache_read, cache_measured, savings)` per trailing week, oldest first.
fn weekly_buckets(dashboard: &UsageDashboard, weeks: usize) -> Vec<(i64, i64, i64)> {
    let mut buckets = vec![(0, 0, 0); weeks];
    for day in &dashboard.days {
        let weeks_ago = (dashboard.today.julian_day - day.julian_day).max(0) / 7;
        if (weeks_ago as usize) < weeks {
            let bucket = &mut buckets[weeks - 1 - weeks_ago as usize];
            bucket.0 += day.cache_read_tokens;
            bucket.1 += day.cache_measured_tokens;
            bucket.2 += day.savings_micros;
        }
    }
    buckets
}
