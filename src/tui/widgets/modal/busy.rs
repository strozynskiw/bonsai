use super::picker_common::*;
use super::*;

/// Header every busy-command surface draws: what the popup is for, then the
/// command it is deciding about.
fn busy_command_header(input: &str) -> Vec<Line<'static>> {
    vec![
        modal_subtitle("The agent is running. Choose what happens to this command."),
        modal_heading(format!("Command: {input}")),
    ]
}

pub(super) fn render_busy_command(
    f: &mut Frame,
    area: Rect,
    input: &str,
    rows: &[crate::tui::event::BusyCommandRow],
    cursor: usize,
) {
    let header = busy_command_header(input);
    let footer = vec![footer_hint_line(&[
        ("Up/Down", "move"),
        ("Enter", "select"),
        ("Esc", "close"),
    ])];
    render_list_picker(f, area, "Busy command", &header, &footer, |body_area| {
        if rows.is_empty() {
            return vec![Line::from(Span::styled(
                "No actions available",
                theme::dim(),
            ))];
        }
        let label_width = column_width(
            rows,
            "",
            body_area.width.saturating_sub(6) as usize,
            |row| row.label.to_string(),
        );
        let cursor = cursor.min(rows.len().saturating_sub(1));
        visible_picker_rows(rows.len(), cursor, picker_body_height(body_area))
            .map(|idx| busy_command_line(idx == cursor, &rows[idx], label_width))
            .collect()
    });
}

/// Rows the busy-command popup draws, measured from the same header builder the
/// renderer uses so a wrapped command is not cut off.
pub(super) fn busy_command_rows(
    area: Rect,
    input: &str,
    rows: &[crate::tui::event::BusyCommandRow],
) -> u16 {
    let wrap = decision_inner_width(area).max(1) as usize;
    shell_modal_rows(
        wrapped_lines_height(&busy_command_header(input), wrap) as u16,
        rows.len().max(1) as u16,
        1,
    )
}

fn busy_command_line(
    selected: bool,
    row: &crate::tui::event::BusyCommandRow,
    label_width: usize,
) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    Line::from(vec![
        Span::styled(marker, row_marker_style()),
        Span::styled(
            pad_ascii(row.label, label_width),
            theme::body(theme::palette().text),
        ),
        Span::styled("  ", theme::dim()),
        Span::styled(row.description, theme::dim()),
    ])
}
