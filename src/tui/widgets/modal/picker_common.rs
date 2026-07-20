use super::*;

// ---- Axis pickers (/mode and /settings) --------------------------------
//
// Both are hierarchical option-cycling pickers: a header row per group, then
// value rows that show their full option set with the current choice bolded,
// collapsing to just the selection when a row is too narrow to fit. The layout
// helpers are identical, so they live here.

/// Width of the `> ` / `  ` focus marker.
pub(super) const AXIS_MARKER_WIDTH: usize = 2;
/// Indent that nests value rows under their group header.
pub(super) const AXIS_VALUE_INDENT: usize = 4;
/// Gap between the key column and the values column.
pub(super) const AXIS_KEY_GAP: usize = 2;
/// Separator rendered between adjacent options in the expanded layout.
pub(super) const AXIS_OPTION_GAP: &str = "  ";

/// The `marker + indent + key + gap` prefix shared by every axis value row.
pub(super) fn axis_row_prefix(
    marker: &'static str,
    key: &str,
    key_width: usize,
) -> Vec<Span<'static>> {
    vec![
        Span::styled(marker, theme::muted()),
        Span::styled(" ".repeat(AXIS_VALUE_INDENT), theme::muted()),
        Span::styled(format!("{key:<key_width$}"), theme::dim()),
        Span::styled(" ".repeat(AXIS_KEY_GAP), theme::muted()),
    ]
}

/// Rendered width of every option laid out side by side, separated by
/// [`AXIS_OPTION_GAP`] — used to decide whether the expanded layout fits.
pub(super) fn axis_options_width(values: &[&str]) -> usize {
    let text: usize = values.iter().map(|value| value.len()).sum();
    let gaps = values.len().saturating_sub(1) * AXIS_OPTION_GAP.len();
    text + gaps
}

/// Push every option as its own span, bolding the current selection and dimming
/// the rest.
pub(super) fn push_axis_options(spans: &mut Vec<Span<'static>>, values: &[&str], current: usize) {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(AXIS_OPTION_GAP, theme::muted()));
        }
        let style = if index == current {
            axis_selected_style()
        } else {
            theme::dim()
        };
        spans.push(Span::styled(value.to_string(), style));
    }
}

/// Bold accent for the currently selected option on an axis value row.
pub(super) fn axis_selected_style() -> Style {
    Style::default()
        .fg(theme::palette().border_active)
        .add_modifier(Modifier::BOLD)
}

/// Character-wrap a labelled modal input field to `width` columns. The `prefix`
/// spans (e.g. a `> ` marker and the `Label: ` text) sit on the first row; the
/// value flows from there and wraps at `width`. Returns the rendered rows plus
/// the cursor offset `(row, col)` measured from the field's first row.
///
/// Render and cursor share this one wrap, so the cursor can never drift outside
/// the modal — the previous code positioned the cursor at `x = start + len`
/// without accounting for the wrap, so a long API key sent it off the right edge
/// while ratatui's word-wrap pushed the value onto lines below.
pub(super) fn wrapped_input_field(
    prefix: Vec<Span<'static>>,
    value: &str,
    value_style: Style,
    width: u16,
) -> (Vec<Line<'static>>, (u16, u16)) {
    use unicode_segmentation::UnicodeSegmentation;
    let end = value.graphemes(true).count();
    wrapped_input_field_at(prefix, value, value_style, width, end)
}

/// Like [`wrapped_input_field`] but reports the cursor at an arbitrary grapheme
/// index into `value` (a caret *before* grapheme `cursor_index`), so a text field
/// can render a cursor mid-string instead of only at the end.
pub(super) fn wrapped_input_field_at(
    prefix: Vec<Span<'static>>,
    value: &str,
    value_style: Style,
    width: u16,
    cursor_index: usize,
) -> (Vec<Line<'static>>, (u16, u16)) {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    let width = width.max(1) as usize;
    let prefix_cols: usize = prefix
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans = prefix;
    let mut current = String::new();
    let mut row = 0usize;
    let mut col = prefix_cols;
    let mut cursor: Option<(u16, u16)> = None;

    for (index, grapheme) in value.graphemes(true).enumerate() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if col > 0 && col + grapheme_width > width {
            if !current.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current), value_style));
            }
            lines.push(Line::from(spans));
            spans = Vec::new();
            row += 1;
            col = 0;
        }
        // The caret sits before this grapheme, on its (post-wrap) row.
        if index == cursor_index {
            cursor = Some((row as u16, col as u16));
        }
        current.push_str(grapheme);
        col += grapheme_width;
    }

    if !current.is_empty() {
        spans.push(Span::styled(current, value_style));
    }
    lines.push(Line::from(spans));

    // A cursor at (or past) the end lands after the last grapheme.
    let cursor = cursor.unwrap_or((row as u16, col as u16));
    (lines, cursor)
}

/// Clamp a cursor position to `inner`, so a modal cursor can never render
/// outside its own box even if the content overflows the available height.
pub(super) fn clamp_cursor(inner: Rect, x: u16, y: u16) -> Position {
    Position {
        x: x.min(inner.x + inner.width.saturating_sub(1)),
        y: y.min(inner.y + inner.height.saturating_sub(1)),
    }
}

/// Where the caret sits in a form field.
pub(super) enum FieldCaret {
    /// Non-editable value (a toggle or cycled choice); no caret, and decorated
    /// with a `‹` when the field is active.
    None,
    /// Editable text field with the caret at the end of the value.
    End,
    /// Editable text field with the caret before grapheme index `.0`.
    At(usize),
}

pub(super) struct FormField<'a> {
    pub label: &'a str,
    pub value: &'a str,
    pub active: bool,
    pub caret: FieldCaret,
    pub value_style: Style,
}

/// Render a stack of labelled input fields sharing the `> `/`  ` marker, the
/// `{label:<pad}: ` prefix, and the wrapped value. Returns the rows plus the
/// active editable field's cursor `(row, col)` measured from the first row —
/// the caller offsets it by any header rows it drew above the fields.
pub(super) fn form_field_lines(
    fields: &[FormField<'_>],
    label_pad: usize,
    width: u16,
) -> (Vec<Line<'static>>, Option<(u16, u16)>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cursor: Option<(u16, u16)> = None;
    for field in fields {
        let marker = if field.active { "> " } else { "  " };
        let mut prefix = vec![
            Span::styled(marker, theme::body(theme::palette().agent_accent)),
            Span::styled(format!("{:<label_pad$}: ", field.label), theme::muted()),
        ];
        // An active non-editable value gets the `‹` cycle affordance.
        if matches!(field.caret, FieldCaret::None) && field.active {
            prefix.push(Span::styled("‹ ", theme::dim()));
        }
        let field_start = lines.len() as u16;
        let (field_lines, (crow, ccol)) = match field.caret {
            FieldCaret::At(index) => {
                wrapped_input_field_at(prefix, field.value, field.value_style, width, index)
            }
            _ => wrapped_input_field(prefix, field.value, field.value_style, width),
        };
        if field.active && !matches!(field.caret, FieldCaret::None) {
            cursor = Some((field_start + crow, ccol));
        }
        lines.extend(field_lines);
    }
    (lines, cursor)
}

/// Width (in display cells) of a table column: the widest cell value among
/// `items`, floored at the header label's width and capped at `cap`. Uses
/// display width (not char count) so a CJK/emoji cell sizes the column to its
/// real on-screen footprint and stays aligned with `pad_ascii`.
pub(super) fn column_width<T>(
    items: &[T],
    header: &str,
    cap: usize,
    cell: impl Fn(&T) -> String,
) -> usize {
    use unicode_width::UnicodeWidthStr;
    items
        .iter()
        .map(|item| UnicodeWidthStr::width(cell(item).as_str()))
        .max()
        .unwrap_or(0)
        .max(UnicodeWidthStr::width(header))
        .min(cap)
}

pub(super) fn session_status_style(status: &crate::storage::SessionStatus) -> Style {
    let palette = theme::palette();
    let color = match status {
        crate::storage::SessionStatus::Active => palette.progress,
        crate::storage::SessionStatus::Completed => palette.success,
        crate::storage::SessionStatus::Forgotten => palette.dim,
        _ => palette.muted,
    };
    status_style(color)
}

pub(super) fn plan_status_style(status: &crate::storage::SavedPlanStatus) -> Style {
    let palette = theme::palette();
    let color = match status {
        crate::storage::SavedPlanStatus::Draft => palette.progress,
        crate::storage::SavedPlanStatus::Started => palette.success,
        _ => palette.muted,
    };
    status_style(color)
}

/// Apply the shared modal status treatment to a semantic status color.
pub(super) fn status_style(color: Color) -> Style {
    theme::body(color)
}

/// Truncate then pad `text` to exactly `width` display cells. Wide (CJK/emoji)
/// graphemes count as 2 cells and are never split, so picker columns stay
/// aligned even for non-ASCII session titles.
pub(super) fn pad_ascii(text: &str, width: usize) -> String {
    let mut value = truncate_ascii(text, width);
    let used = unicode_width::UnicodeWidthStr::width(value.as_str());
    if used < width {
        value.push_str(&" ".repeat(width - used));
    }
    value
}

pub(super) fn truncate_ascii(text: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    if width == 0 {
        return String::new();
    }
    let text = text.replace(['\n', '\r', '\t'], " ");
    if UnicodeWidthStr::width(text.as_str()) <= width {
        return text;
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    // Keep up to `width - 3` cells, then append a 3-cell ellipsis.
    let budget = width - 3;
    let mut value = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if used + w > budget {
            break;
        }
        value.push_str(grapheme);
        used += w;
    }
    value.push_str("...");
    value
}

pub(super) fn relative_age(updated_at_ms: i64) -> String {
    let elapsed_secs = crate::util::time::now_ms()
        .saturating_sub(updated_at_ms)
        .max(0)
        / 1000;
    if elapsed_secs < 60 {
        "<1m ago".to_string()
    } else if elapsed_secs < 60 * 60 {
        format!("{}m ago", elapsed_secs / 60)
    } else if elapsed_secs < 60 * 60 * 24 {
        format!("{}h ago", elapsed_secs / (60 * 60))
    } else {
        format!("{}d ago", elapsed_secs / (60 * 60 * 24))
    }
}

/// Frame + optional header + windowed body + pinned footer — the chrome every
/// single-column list picker repeats. `body` receives the body rect (so it can
/// window its rows against the real height with [`visible_picker_rows`]) and
/// returns the lines to draw; `footer` height tracks `footer.len()`.
pub(super) fn render_list_picker(
    f: &mut Frame,
    area: Rect,
    title: &str,
    header: &[Line<'static>],
    footer: &[Line<'static>],
    body: impl FnOnce(Rect) -> Vec<Line<'static>>,
) {
    let panel = theme::frame(title, true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let footer_height = (footer.len() as u16).max(1);
    let (body_area, footer_area) = if header.is_empty() {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(footer_height)])
            .split(inner);
        (chunks[0], chunks[1])
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(header.len() as u16),
                Constraint::Min(3),
                Constraint::Length(footer_height),
            ])
            .split(inner);
        f.render_widget(
            Paragraph::new(header.to_vec())
                .style(theme::panel())
                .wrap(Wrap { trim: false }),
            chunks[0],
        );
        (chunks[1], chunks[2])
    };

    f.render_widget(
        Paragraph::new(body(body_area))
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        body_area,
    );
    // Footer lines are one-row hints; don't wrap, or a long status line eats the
    // hint row below it.
    f.render_widget(
        Paragraph::new(footer.to_vec()).style(theme::panel()),
        footer_area,
    );
}

pub(super) fn render_picker_column(
    f: &mut Frame,
    area: Rect,
    title: &str,
    active: bool,
    lines: Vec<Line<'static>>,
) {
    let title = if active {
        format!("{title} *")
    } else {
        title.to_string()
    };
    let panel = Paragraph::new(lines)
        .block(theme::frame(&title, active))
        .style(theme::panel());
    f.render_widget(panel, area);
}

pub(super) fn picker_body_height(area: Rect) -> usize {
    area.height.saturating_sub(2) as usize
}

pub(super) fn visible_picker_rows(
    len: usize,
    cursor: usize,
    visible_height: usize,
) -> std::ops::Range<usize> {
    if len == 0 {
        return 0..0;
    }
    let visible_height = visible_height.max(1).min(len);
    let cursor = cursor.min(len.saturating_sub(1));
    // Centre the cursor in the viewport so it moves freely within the window
    // and the list scrolls smoothly from both directions, instead of pinning
    // the cursor to the bottom row on every press past the first page.
    let start = cursor
        .saturating_sub(visible_height / 2)
        .min(len.saturating_sub(visible_height));
    start..start + visible_height
}

/// Like [`visible_picker_rows`], but for lists whose rows occupy a varying
/// number of terminal lines (`heights`, one entry per row). Returns the range
/// of rows whose total line count fits `visible_lines` with the cursor row
/// fully visible and roughly centred: the window grows alternately above and
/// below the cursor until the line budget is spent. Counting lines instead of
/// rows matters — a window sized in rows believes everything fits, so the
/// paragraph clips the tail and the cursor walks below the fold.
pub(super) fn visible_weighted_rows(
    heights: &[usize],
    cursor: usize,
    visible_lines: usize,
) -> std::ops::Range<usize> {
    if heights.is_empty() {
        return 0..0;
    }
    if heights.iter().sum::<usize>() <= visible_lines {
        return 0..heights.len();
    }
    let cursor = cursor.min(heights.len() - 1);
    let mut lines = heights[cursor];
    let (mut lo, mut hi) = (cursor, cursor);
    loop {
        let mut grew = false;
        if lo > 0 && lines + heights[lo - 1] <= visible_lines {
            lo -= 1;
            lines += heights[lo];
            grew = true;
        }
        if hi + 1 < heights.len() && lines + heights[hi + 1] <= visible_lines {
            hi += 1;
            lines += heights[hi];
            grew = true;
        }
        if !grew {
            break;
        }
    }
    lo..hi + 1
}

pub(super) fn picker_line(
    selected: bool,
    label: String,
    subtitle: Option<String>,
    enabled: bool,
) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let label_style = if enabled {
        theme::body(theme::palette().text)
    } else {
        theme::dim()
    };
    let mut spans = vec![
        Span::styled(marker, theme::muted()),
        Span::styled(label, label_style),
    ];
    if let Some(subtitle) = subtitle {
        spans.push(Span::styled("  ", theme::dim()));
        spans.push(Span::styled(subtitle, theme::dim()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(value: &str, width: u16) -> (Vec<Line<'static>>, (u16, u16)) {
        // 9-column prefix, matching "API key: ".
        wrapped_input_field(vec![Span::raw("API key: ")], value, Style::default(), width)
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn form_field_lines_marks_active_and_places_the_cursor() {
        let fields = vec![
            FormField {
                label: "Name",
                value: "alice",
                active: false,
                caret: FieldCaret::End,
                value_style: Style::default(),
            },
            FormField {
                label: "Model",
                value: "gpt",
                active: true,
                caret: FieldCaret::At(1),
                value_style: Style::default(),
            },
        ];
        let (lines, cursor) = form_field_lines(&fields, 6, 40);
        assert!(line_text(&lines[0]).starts_with("  Name"), "inactive field");
        assert!(line_text(&lines[1]).starts_with("> Model"), "active field");
        let (row, col) = cursor.expect("the active editable field has a cursor");
        assert_eq!(row, 1, "cursor on the second field's row");
        // Prefix is "> " + "Model : " = 2 + 8 = 10 cells; caret before grapheme 1.
        assert_eq!(col, 11);
    }

    #[test]
    fn form_field_lines_decorates_active_non_editable_and_has_no_cursor() {
        let fields = vec![FormField {
            label: "Server",
            value: "zen",
            active: true,
            caret: FieldCaret::None,
            value_style: Style::default(),
        }];
        let (lines, cursor) = form_field_lines(&fields, 6, 40);
        assert!(
            line_text(&lines[0]).contains('‹'),
            "an active non-editable field shows the cycle affordance"
        );
        assert!(cursor.is_none(), "a non-editable field has no text cursor");
    }

    #[test]
    fn short_value_stays_inline() {
        let (lines, (row, col)) = field("abc", 40);
        assert_eq!(lines.len(), 1);
        assert_eq!((row, col), (0, 12)); // 9 prefix + 3 value
    }

    #[test]
    fn long_value_wraps_and_cursor_follows() {
        // width 20 -> first row holds 9 prefix + 11 value; then 20 per row.
        let value = "a".repeat(45);
        let (lines, (row, col)) = field(&value, 20);
        // 9 + 45 = 54 total columns; 54 / 20 = row 2, 54 % 20 = col 14.
        assert_eq!((row, col), (2, 14));
        // Rows: [prefix+11], [20], [14] -> 3 rows.
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn cursor_stays_on_rendered_row_at_exact_width() {
        // 9 prefix + 11 value = 20 = width -> one rendered row, cursor at end.
        let (_, (row, col)) = field(&"a".repeat(11), 20);
        assert_eq!((row, col), (0, 20));
    }

    #[test]
    fn wide_unicode_wraps_by_display_width() {
        // Prefix is 9 cells; each CJK grapheme is 2 cells. Only five fit on
        // the first 20-cell row, leaving the cursor after the sixth on row 1.
        let (lines, (row, col)) = field("界界界界界界", 20);
        assert_eq!(lines.len(), 2);
        assert_eq!((row, col), (1, 2));
    }

    #[test]
    fn combining_grapheme_cursor_uses_display_width() {
        let (lines, (row, col)) = field("e\u{301}e\u{301}", 10);
        assert_eq!(lines.len(), 2);
        assert_eq!((row, col), (1, 1));
    }

    #[test]
    fn empty_value_sits_after_prefix() {
        let (lines, (row, col)) = field("", 40);
        assert_eq!(lines.len(), 1);
        assert_eq!((row, col), (0, 9));
    }

    #[test]
    fn clamp_keeps_cursor_inside_inner_rect() {
        let inner = Rect::new(5, 2, 10, 4);
        // Way past the right/bottom edge -> clamped to the last cell.
        let pos = clamp_cursor(inner, 99, 99);
        assert_eq!(pos.x, 5 + 10 - 1);
        assert_eq!(pos.y, 2 + 4 - 1);
        // Inside -> unchanged.
        let pos = clamp_cursor(inner, 7, 3);
        assert_eq!((pos.x, pos.y), (7, 3));
    }
}
