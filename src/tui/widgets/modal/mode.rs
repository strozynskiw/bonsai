use super::picker_common::*;
use super::*;

/// Render the `/mode` posture picker. Each axis has a non-selectable header
/// row followed by one or more value rows; navigation only ever rests on a
/// value row, which carries the `>` marker. Every value row shows its full
/// option set with the current choice highlighted; when a row is too wide for
/// the panel it collapses to just the selected option. The bottom-pinned
/// footer carries the navigation hints.
pub(super) fn render_mode_picker(f: &mut Frame, area: Rect, rows: &[ModeRow], cursor: usize) {
    render_list_picker(f, area, "Mode", &[], &mode_picker_footer(), |body_area| {
        if rows.is_empty() {
            return vec![Line::from(Span::styled("No posture axes", theme::dim()))];
        }
        let cursor = cursor.min(rows.len().saturating_sub(1));
        // Align every value column under a common key width so options line up.
        let key_width = rows
            .iter()
            .filter_map(|row| match row {
                ModeRow::Value { key, .. } => Some(key.len()),
                ModeRow::Header(_) => None,
            })
            .max()
            .unwrap_or(0);
        let body_width = body_area.width as usize;
        mode_picker_visible_rows(rows, cursor, body_area.height as usize)
            .map(|index| (index, &rows[index]))
            .map(|(index, row)| mode_picker_row_line(row, index == cursor, key_width, body_width))
            .collect()
    });
}

fn mode_picker_visible_rows(
    rows: &[ModeRow],
    cursor: usize,
    visible_height: usize,
) -> std::ops::Range<usize> {
    if rows.is_empty() {
        return 0..0;
    }
    let visible_height = visible_height.max(1).min(rows.len());
    let cursor = cursor.min(rows.len().saturating_sub(1));
    let trailing_start = cursor
        .saturating_add(1)
        .saturating_sub(visible_height)
        .min(rows.len().saturating_sub(visible_height));
    let section_start = rows[..=cursor]
        .iter()
        .rposition(|row| matches!(row, ModeRow::Header(_)))
        .unwrap_or(cursor);
    let focused_section_fits = cursor.saturating_sub(section_start) < visible_height;
    let start = if section_start < trailing_start && focused_section_fits {
        section_start
    } else {
        trailing_start
    };
    let end = start.saturating_add(visible_height).min(rows.len());
    start..end
}

fn mode_picker_row_line(
    row: &ModeRow,
    focused: bool,
    key_width: usize,
    body_width: usize,
) -> Line<'static> {
    let marker = if focused { "> " } else { "  " };
    match row {
        ModeRow::Header(label) => Line::from(vec![
            Span::styled(marker, theme::muted()),
            Span::styled(*label, theme::title()),
        ]),
        ModeRow::Value {
            key,
            values,
            current,
            note,
            ..
        } => {
            let mut spans = axis_row_prefix(marker, key, key_width);

            // Show every option when the whole row fits; otherwise fall back to
            // just the selected one so a narrow panel never wraps mid-row.
            let prefix_width = AXIS_MARKER_WIDTH + AXIS_VALUE_INDENT + key_width + AXIS_KEY_GAP;
            let note_width = note.map_or(0, |note| note.len() + 4);
            if prefix_width + axis_options_width(values) + note_width <= body_width {
                push_axis_options(&mut spans, values, *current);
            } else {
                let value = values.get(*current).copied().unwrap_or("?");
                spans.push(Span::styled(value.to_string(), axis_selected_style()));
            }

            if let Some(note) = note {
                spans.push(Span::styled(format!("  ({note})"), theme::dim()));
            }
            Line::from(spans)
        }
    }
}

fn mode_picker_footer() -> Vec<Line<'static>> {
    vec![
        Line::from(Span::styled(
            "Cycle posture axes. Confinement=off makes network a no-op.",
            theme::dim(),
        )),
        super::common::footer_hint_line(&[
            ("Up/Down", "move"),
            ("Left/Right", "cycle"),
            ("Enter/Esc", "close"),
        ]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::tests::support::rendered_lines_text;
    use crate::tui::event::ModeAxisId;

    fn rows() -> Vec<ModeRow> {
        vec![
            ModeRow::Header("Autonomy"),
            ModeRow::Value {
                axis: ModeAxisId::Autonomy,
                key: "level",
                values: &["ask", "balanced"],
                current: 0,
                note: None,
            },
            ModeRow::Header("Self-review"),
            ModeRow::Value {
                axis: ModeAxisId::SelfReview,
                key: "mode",
                values: &["auto", "off"],
                current: 0,
                note: None,
            },
            ModeRow::Header("Sandbox"),
            ModeRow::Value {
                axis: ModeAxisId::SandboxConfinement,
                key: "confinement",
                values: &["off", "on"],
                current: 0,
                note: None,
            },
            ModeRow::Value {
                axis: ModeAxisId::SandboxNetwork,
                key: "network",
                values: &["allow", "deny"],
                current: 0,
                note: None,
            },
        ]
    }

    #[test]
    fn mode_picker_footer_describes_live_cycle_and_close_keys() {
        let footer = rendered_lines_text(&mode_picker_footer());
        assert!(footer.contains("Left/Right cycle"));
        assert!(footer.contains("Enter/Esc close"));
    }

    #[test]
    fn constrained_mode_rows_keep_focused_sandbox_section_visible() {
        let rows = rows();
        let visible = mode_picker_visible_rows(&rows, 6, 4);
        assert!(visible.contains(&4), "Sandbox header should remain visible");
        assert!(
            visible.contains(&6),
            "focused Sandbox row should remain visible"
        );
    }
}
