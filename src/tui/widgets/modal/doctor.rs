use crate::doctor::{DoctorCheck, DoctorReport, DoctorStatus};

use super::common::*;
use super::picker_common::*;
use super::*;

/// The `/doctor` view: a read-only release-diagnostics summary. A two-line
/// header carries the environment line and the aggregate pass/warn/fail counts;
/// the body lists one selectable row per check, with warnings and failures
/// expanding to show their remediation inline so problems are self-explanatory
/// without opening anything. View-only — arrows move, Esc/Enter/q close.
pub(super) fn render_doctor(f: &mut Frame, area: Rect, report: &DoctorReport, cursor: usize) {
    let palette = theme::palette();
    let summary = &report.summary;

    let header = vec![
        Line::from(Span::styled(
            format!(
                "bonsai {} · {} · {}",
                report.bonsai_version, report.os, report.arch
            ),
            theme::dim(),
        )),
        Line::from(vec![
            count_span("✓", summary.passed, "passed", palette.success),
            Span::styled("     ", theme::dim()),
            count_span("▲", summary.warnings, "warning", palette.todo),
            Span::styled("     ", theme::dim()),
            count_span("✗", summary.failed, "failed", palette.error),
        ]),
    ];

    let footer = vec![footer_hint_line(&[("Up/Down", "move"), ("Esc", "close")])];

    render_list_picker(f, area, "Doctor", &header, &footer, |body_area| {
        if report.checks.is_empty() {
            return vec![Line::from(Span::styled("No checks were run", theme::dim()))];
        }
        let cursor = cursor.min(report.checks.len().saturating_sub(1));
        let width = body_area.width as usize;
        // One display block per check (status line plus any wrapped remediation
        // lines); windowing counts lines, not rows, so a check whose action
        // wraps can't push the selected row below the fold.
        let groups: Vec<Vec<Line<'static>>> = report
            .checks
            .iter()
            .enumerate()
            .map(|(idx, check)| check_lines(check, idx == cursor, width))
            .collect();
        let heights: Vec<usize> = groups.iter().map(Vec::len).collect();
        let visible = body_area.height.max(1) as usize;
        visible_weighted_rows(&heights, cursor, visible)
            .flat_map(|idx| groups[idx].clone())
            .collect()
    });
}

/// `✓ 11 passed`, colored by status when the count is non-zero and dimmed when
/// it is zero — so a clean run reads as green/neutral, not an alarming red 0.
fn count_span(
    glyph: &str,
    count: usize,
    noun: &str,
    color: ratatui::style::Color,
) -> Span<'static> {
    let plural = if count == 1 { "" } else { "s" };
    let text = format!("{glyph} {count} {noun}{plural}");
    let style = if count == 0 {
        theme::dim()
    } else {
        theme::label(color)
    };
    Span::styled(text, style)
}

/// The status glyph and accent color for one severity.
fn status_glyph(status: DoctorStatus) -> (&'static str, ratatui::style::Color) {
    let palette = theme::palette();
    match status {
        DoctorStatus::Pass => ("✓", palette.success),
        DoctorStatus::Warning => ("▲", palette.todo),
        DoctorStatus::Fail => ("✗", palette.error),
    }
}

/// The display block for one check: a single status line (`> ✓ Label — summary`,
/// the summary truncated to fit) followed, for warnings and failures, by the
/// wrapped remediation line so the fix is visible without extra keystrokes.
fn check_lines(check: &DoctorCheck, selected: bool, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;

    let palette = theme::palette();
    let (glyph, color) = status_glyph(check.status);
    let marker = if selected { "> " } else { "  " };

    // Fixed lead: marker + glyph + space + label + " — "; the summary takes
    // whatever cells remain and is dropped entirely on a very narrow modal.
    let separator = " — ";
    let lead_width = UnicodeWidthStr::width(marker)
        + UnicodeWidthStr::width(glyph)
        + 1
        + UnicodeWidthStr::width(check.label)
        + UnicodeWidthStr::width(separator);
    let summary_budget = width.saturating_sub(lead_width);

    let mut spans = vec![
        Span::styled(marker, theme::muted()),
        Span::styled(glyph, theme::label(color)),
        Span::styled(" ", theme::dim()),
        Span::styled(check.label, theme::body(palette.text)),
    ];
    if summary_budget >= 8 {
        spans.push(Span::styled(separator, theme::dim()));
        spans.push(Span::styled(
            truncate_ascii(&check.summary, summary_budget),
            theme::dim(),
        ));
    }
    let mut lines = vec![Line::from(spans)];

    if let Some(action) = &check.next_action {
        let action_line = Line::from(vec![
            Span::styled("    ↳ ", theme::dim()),
            Span::styled(action.clone(), theme::body(palette.muted)),
        ]);
        lines.extend(wrap_line(&action_line, width));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{DoctorReport, DoctorSummary};
    use ratatui::{Terminal, backend::TestBackend};

    fn check(
        id: &'static str,
        label: &'static str,
        status: DoctorStatus,
        summary: &str,
        next_action: Option<&str>,
    ) -> DoctorCheck {
        DoctorCheck {
            id,
            label,
            status,
            summary: summary.to_string(),
            next_action: next_action.map(str::to_string),
        }
    }

    fn report() -> DoctorReport {
        let checks = vec![
            check(
                "database",
                "Database",
                DoctorStatus::Pass,
                "Migrations, integrity check, and write probe passed.",
                None,
            ),
            check(
                "config",
                "Configuration",
                DoctorStatus::Warning,
                "1 configuration entry was skipped.",
                Some("Run `/config validate` and fix every reported field."),
            ),
            check(
                "provider_auth",
                "Provider",
                DoctorStatus::Fail,
                "OpenCode Go is selected but not authorized.",
                Some("Run `/authorize opencode` and complete the provider login."),
            ),
        ];
        DoctorReport {
            schema_version: 1,
            bonsai_version: "1.2.3",
            os: "macos",
            arch: "aarch64",
            summary: DoctorSummary {
                passed: 1,
                warnings: 1,
                failed: 1,
            },
            checks,
        }
    }

    fn render_to_text(cursor: usize) -> String {
        let area = Rect::new(0, 0, 96, 24);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        terminal
            .draw(|frame| render_doctor(frame, area, &report(), cursor))
            .expect("doctor modal should render");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn summary_header_and_every_check_render() {
        let text = render_to_text(0);
        // Environment line and colored aggregate counts.
        assert!(text.contains("bonsai 1.2.3 · macos · aarch64"), "{text}");
        assert!(text.contains("1 passed"), "{text}");
        assert!(text.contains("1 warning"), "{text}");
        assert!(text.contains("1 failed"), "{text}");
        // Each check's label and status, with remediation shown inline for the
        // problems.
        assert!(text.contains("Database"), "{text}");
        assert!(text.contains("Configuration"), "{text}");
        assert!(text.contains("Provider"), "{text}");
        assert!(text.contains("/config validate"), "{text}");
        assert!(text.contains("/authorize opencode"), "{text}");
    }
}
