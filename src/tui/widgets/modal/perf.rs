use super::common::*;
use super::*;

pub(super) fn render_perf_report(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    title: &str,
    lines: &[String],
) {
    let body_lines = perf_report_lines(lines);
    let footer_lines = vec![footer_hint_line(&[("Up/Down", "scroll"), ("Esc", "close")])];
    render_scrollable_modal(
        f,
        area,
        title,
        &[],
        &body_lines,
        &footer_lines,
        app.modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
}

pub(super) fn perf_report_max_scroll(area: Rect, lines: &[String]) -> u16 {
    let body_lines = perf_report_lines(lines);
    scrollable_modal_max_scroll(area, 0, 1, &body_lines)
}

fn perf_report_lines(lines: &[String]) -> Vec<Line<'static>> {
    if lines.is_empty() {
        return vec![Line::from(Span::styled(
            "No performance data is available yet.",
            theme::dim(),
        ))];
    }

    lines.iter().map(|line| perf_report_line(line)).collect()
}

fn perf_report_line(line: &str) -> Line<'static> {
    if line.is_empty() {
        return Line::from("");
    }

    if line == "Performance: last model call"
        || line == "Usage: session"
        || line == "Usage: recent turns"
    {
        return section_header(line);
    }

    if let Some((label, value)) = split_metric_line(line) {
        return Line::from(vec![
            Span::styled(format!("{label:<16}"), theme::muted()),
            Span::styled(value.to_string(), theme::panel()),
        ]);
    }

    Line::from(Span::styled(line.to_string(), theme::panel()))
}

fn split_metric_line(line: &str) -> Option<(&str, &str)> {
    let split = line.find("  ")?;
    let label = line[..split].trim_end();
    let value = line[split..].trim_start();
    if label.is_empty() || value.is_empty() {
        return None;
    }
    Some((label, value))
}
