use super::picker_common::*;
use super::*;

pub(super) fn render_busy_command(
    f: &mut Frame,
    area: Rect,
    input: &str,
    rows: &[crate::tui::event::BusyCommandRow],
    cursor: usize,
) {
    let header = vec![
        Line::from(Span::styled("The agent is running.", theme::muted())),
        Line::from(vec![
            Span::styled("Command: ", theme::muted()),
            Span::styled(input.to_string(), theme::body(theme::palette().text)),
        ]),
    ];
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

fn busy_command_line(
    selected: bool,
    row: &crate::tui::event::BusyCommandRow,
    label_width: usize,
) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    Line::from(vec![
        Span::styled(marker, theme::muted()),
        Span::styled(
            pad_ascii(row.label, label_width),
            theme::body(theme::palette().text),
        ),
        Span::styled("  ", theme::dim()),
        Span::styled(row.description, theme::dim()),
    ])
}
