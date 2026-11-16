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
    let footer = vec![footer_hint_line(BUSY_HINTS)];
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
        let lines = rows
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                row_line(
                    idx == cursor,
                    pad_ascii(row.label, label_width),
                    Some(row.description.to_string()),
                    true,
                )
            })
            .collect::<Vec<_>>();
        let heights = lines
            .iter()
            .map(|line| row_line_height(line, body_area.width, 0))
            .collect::<Vec<_>>();
        visible_rows(body_area, &heights, cursor)
            .map(|idx| lines[idx].clone())
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
