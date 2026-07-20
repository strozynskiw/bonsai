use super::picker_common::*;
use super::*;
use crate::tui::event::SettingsRow;
use unicode_width::UnicodeWidthStr;

const SETTINGS_COLUMN_GAP: u16 = 3;
const SETTINGS_DESCRIPTION_GAP: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlDisplay {
    Options,
    Selected,
}

#[derive(Clone, Copy, Debug)]
struct SettingsTableLayout {
    key_width: usize,
    control_width: usize,
    control_display: ControlDisplay,
    show_descriptions: bool,
}

/// Render `/settings` as a full-screen responsive surface. Wide terminals split
/// complete sections across two columns; narrow terminals use one windowed list.
/// Both layouts keep the focused row visible while navigation skips headers.
pub(super) fn render_settings(f: &mut Frame, area: Rect, rows: &[SettingsRow], cursor: usize) {
    let panel = theme::frame("Settings", true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let cursor = cursor.min(rows.len().saturating_sub(1));

    let footer = settings_footer();
    let footer_height = (footer.len() as u16).min(inner.height);
    let regions = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(footer_height)])
        .split(inner);
    let body_area = regions[0];

    if settings_columns_fit(body_area, rows) {
        render_settings_columns(f, body_area, rows, cursor);
    } else {
        render_settings_column(f, body_area, rows, 0, cursor);
    }

    f.render_widget(Paragraph::new(footer).style(theme::panel()), regions[1]);
}

fn settings_columns_fit(area: Rect, rows: &[SettingsRow]) -> bool {
    if settings_section_boundaries(rows).len() < 2 || area.width <= SETTINGS_COLUMN_GAP {
        return false;
    }
    let split = settings_column_split(rows);
    let available = area.width.saturating_sub(SETTINGS_COLUMN_GAP);
    let left_width = available / 2;
    let right_width = available.saturating_sub(left_width);
    usize::from(left_width) >= settings_full_table_width(&rows[..split])
        && usize::from(right_width) >= settings_full_table_width(&rows[split..])
}

fn render_settings_columns(f: &mut Frame, area: Rect, rows: &[SettingsRow], cursor: usize) {
    let split = settings_column_split(rows);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(SETTINGS_COLUMN_GAP),
            Constraint::Fill(1),
        ])
        .split(area);
    render_settings_column(f, columns[0], &rows[..split], 0, cursor);
    render_settings_column(f, columns[2], &rows[split..], split, cursor);
}

/// Split at the section boundary that most closely balances rendered pane
/// heights, so a section header is never detached from its rows.
fn settings_column_split(rows: &[SettingsRow]) -> usize {
    let boundaries = settings_section_boundaries(rows);
    let total_height = settings_rendered_height(rows);
    boundaries
        .into_iter()
        .skip(1)
        .min_by_key(|boundary| {
            let left_height = settings_rendered_height(&rows[..*boundary]);
            left_height.abs_diff(total_height.saturating_sub(left_height))
        })
        .unwrap_or(rows.len())
}

fn settings_section_boundaries(rows: &[SettingsRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| matches!(row, SettingsRow::Header(_)).then_some(index))
        .collect()
}

fn settings_rendered_height(rows: &[SettingsRow]) -> usize {
    rows.iter().map(settings_row_height).sum()
}

fn settings_row_height(_row: &SettingsRow) -> usize {
    1
}

fn render_settings_column(
    f: &mut Frame,
    area: Rect,
    rows: &[SettingsRow],
    base_index: usize,
    cursor: usize,
) {
    if area.height == 0 || area.width == 0 || rows.is_empty() {
        return;
    }

    let table = settings_table_layout(rows, area.width as usize);
    let local_cursor = cursor
        .checked_sub(base_index)
        .filter(|index| *index < rows.len())
        .unwrap_or(0);
    let visible = visible_settings_rows(rows, local_cursor, area.height as usize);
    let lines = visible
        .into_iter()
        .map(|index| settings_row_line(&rows[index], base_index + index == cursor, table))
        .collect::<Vec<_>>();
    f.render_widget(Paragraph::new(lines).style(theme::panel()), area);
}

fn visible_settings_rows(rows: &[SettingsRow], cursor: usize, height: usize) -> Vec<usize> {
    if rows.is_empty() || height == 0 {
        return Vec::new();
    }
    let cursor = cursor.min(rows.len() - 1);
    let mut visible = visible_picker_rows(rows.len(), cursor, height).collect::<Vec<_>>();
    let header = rows[..=cursor]
        .iter()
        .rposition(|row| matches!(row, SettingsRow::Header(_)))
        .unwrap_or(cursor);
    if height >= 2 && !visible.contains(&header) {
        visible.remove(0);
        visible.insert(0, header);
    }
    visible
}

fn settings_key_width(rows: &[SettingsRow]) -> usize {
    rows.iter()
        .filter_map(|row| match row {
            SettingsRow::Choice { key, .. } | SettingsRow::Action { key, .. } => {
                Some(UnicodeWidthStr::width(*key))
            }
            SettingsRow::Header(_) => None,
        })
        .max()
        .unwrap_or(0)
}

fn settings_table_layout(rows: &[SettingsRow], body_width: usize) -> SettingsTableLayout {
    let desired_key_width = settings_key_width(rows);
    let desired_selected_width = rows.iter().map(settings_selected_width).max().unwrap_or(0);
    let choice_selected_width = rows
        .iter()
        .filter(|row| matches!(row, SettingsRow::Choice { .. }))
        .map(settings_selected_width)
        .max()
        .unwrap_or(1);
    let table_width = body_width.saturating_sub(AXIS_MARKER_WIDTH + AXIS_KEY_GAP);
    let reserved_control_width = choice_selected_width.min(table_width.saturating_sub(1));
    let key_width = desired_key_width.min(table_width.saturating_sub(reserved_control_width));
    let selected_width = desired_selected_width.min(table_width.saturating_sub(key_width));
    let prefix_width = AXIS_MARKER_WIDTH + key_width + AXIS_KEY_GAP;
    let options_width = rows.iter().map(settings_options_width).max().unwrap_or(0);
    let description_width = rows
        .iter()
        .map(settings_description_width)
        .max()
        .unwrap_or(0);
    let options_with_descriptions =
        prefix_width + options_width + SETTINGS_DESCRIPTION_GAP + description_width;
    let (control_display, control_width) = if options_with_descriptions <= body_width {
        (ControlDisplay::Options, options_width)
    } else {
        (ControlDisplay::Selected, selected_width)
    };
    let show_descriptions = description_width > 0
        && prefix_width + control_width + SETTINGS_DESCRIPTION_GAP + description_width
            <= body_width;
    SettingsTableLayout {
        key_width,
        control_width,
        control_display,
        show_descriptions,
    }
}

fn settings_full_table_width(rows: &[SettingsRow]) -> usize {
    AXIS_MARKER_WIDTH
        + settings_key_width(rows)
        + AXIS_KEY_GAP
        + rows.iter().map(settings_options_width).max().unwrap_or(0)
        + SETTINGS_DESCRIPTION_GAP
        + rows
            .iter()
            .map(settings_description_width)
            .max()
            .unwrap_or(0)
}

fn settings_selected_width(row: &SettingsRow) -> usize {
    match row {
        SettingsRow::Choice {
            values, current, ..
        } => values
            .get(*current)
            .map_or(1, |value| UnicodeWidthStr::width(*value)),
        SettingsRow::Action { value, .. } => UnicodeWidthStr::width(value.as_str()),
        SettingsRow::Header(_) => 0,
    }
}

fn settings_options_width(row: &SettingsRow) -> usize {
    match row {
        SettingsRow::Choice { values, .. } => settings_axis_options_width(values),
        _ => settings_selected_width(row),
    }
}

fn settings_description_width(row: &SettingsRow) -> usize {
    match row {
        SettingsRow::Choice { note, .. } => note.as_deref().map_or(0, UnicodeWidthStr::width),
        SettingsRow::Action { .. } => "Enter to change".len(),
        SettingsRow::Header(_) => 0,
    }
}

fn settings_axis_options_width(values: &[&str]) -> usize {
    values
        .iter()
        .map(|value| UnicodeWidthStr::width(*value))
        .sum::<usize>()
        + values.len().saturating_sub(1) * AXIS_OPTION_GAP.len()
}

fn settings_row_line(
    row: &SettingsRow,
    focused: bool,
    table: SettingsTableLayout,
) -> Line<'static> {
    let marker = if focused { "> " } else { "  " };
    match row {
        SettingsRow::Header(label) => Line::from(vec![
            Span::styled(marker, theme::muted()),
            Span::styled(*label, theme::title()),
        ]),
        SettingsRow::Choice {
            key,
            values,
            current,
            note,
            ..
        } => {
            let mut spans = settings_row_prefix(marker, key, table.key_width);
            if table.control_display == ControlDisplay::Options {
                push_axis_options(&mut spans, values, *current);
            } else {
                let value = values.get(*current).copied().unwrap_or("?");
                spans.push(Span::styled(
                    truncate_ascii(value, table.control_width),
                    axis_selected_style(),
                ));
            }
            pad_control(
                &mut spans,
                settings_control_width(row, table.control_display),
                table,
            );
            if table.show_descriptions
                && let Some(note) = note
            {
                spans.push(Span::styled(note.clone(), theme::dim()));
            }
            Line::from(spans)
        }
        SettingsRow::Action { key, value, .. } => {
            let mut spans = settings_row_prefix(marker, key, table.key_width);
            let value = truncate_ascii(value, table.control_width);
            let value_width = UnicodeWidthStr::width(value.as_str());
            spans.push(Span::styled(value, axis_selected_style()));
            pad_control(&mut spans, value_width, table);
            if table.show_descriptions {
                spans.push(Span::styled("Enter to change", theme::dim()));
            }
            Line::from(spans)
        }
    }
}

fn settings_row_prefix(marker: &'static str, key: &str, key_width: usize) -> Vec<Span<'static>> {
    let key = truncate_ascii(key, key_width);
    let key_padding = key_width.saturating_sub(UnicodeWidthStr::width(key.as_str()));
    vec![
        Span::styled(marker, theme::muted()),
        Span::styled(format!("{key}{}", " ".repeat(key_padding)), theme::dim()),
        Span::styled(" ".repeat(AXIS_KEY_GAP), theme::muted()),
    ]
}

fn settings_control_width(row: &SettingsRow, display: ControlDisplay) -> usize {
    match display {
        ControlDisplay::Options => settings_options_width(row),
        ControlDisplay::Selected => settings_selected_width(row),
    }
}

fn pad_control(spans: &mut Vec<Span<'static>>, width: usize, table: SettingsTableLayout) {
    if table.show_descriptions {
        spans.push(Span::styled(
            " ".repeat(table.control_width.saturating_sub(width) + SETTINGS_DESCRIPTION_GAP),
            theme::muted(),
        ));
    }
}

fn settings_footer() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "Changes apply live. Model/Theme open a picker with Enter.",
            theme::dim(),
        )),
        super::common::footer_hint_line(&[
            ("Up/Down", "move"),
            ("Left/Right", "cycle"),
            ("Space/Enter", "change"),
            ("Esc", "close"),
        ]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_split_balances_rendered_section_heights() {
        let rows = vec![
            SettingsRow::Header("One"),
            action("one"),
            SettingsRow::Header("Two"),
            action("two"),
            action("three"),
            SettingsRow::Header("Three"),
            action("four"),
            action("five"),
        ];

        assert_eq!(settings_column_split(&rows), 5);
    }

    #[test]
    fn constrained_window_keeps_cursor_and_section_header() {
        let rows = vec![
            SettingsRow::Header("One"),
            action("one"),
            SettingsRow::Header("Two"),
            action("two"),
            action("three"),
            action("four"),
        ];

        assert_eq!(visible_settings_rows(&rows, 5, 3), vec![2, 4, 5]);
    }

    #[test]
    fn table_layout_measures_terminal_cell_widths() {
        let rows = vec![SettingsRow::Action {
            id: crate::tui::event::SettingId::Model,
            key: "テーマ",
            value: "模型".to_string(),
        }];

        let table = settings_table_layout(&rows, 40);
        assert_eq!(table.key_width, 6);
        assert_eq!(table.control_width, 4);
    }

    fn action(key: &'static str) -> SettingsRow {
        SettingsRow::Action {
            id: crate::tui::event::SettingId::Model,
            key,
            value: "value".to_string(),
        }
    }
}
