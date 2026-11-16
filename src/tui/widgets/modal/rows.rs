//! The modal row vocabulary: one selectable row, one label-plus-description block,
//! one keyed prefix, and the decision-popup builders every choice popup shares.
//!
//! Every popup draws its rows through these primitives so markers, aligned
//! description columns and wrapped continuations cannot drift between modals.
//! [`row_block`] shares [`row_geometry`] with the row it draws, so a measured row
//! always agrees with the drawn one.

use super::picker_common::pad_ascii;
use super::*;

/// Width of the `> ` / `  ` selection marker every row starts with.
pub(super) const ROW_MARKER_WIDTH: usize = 2;
/// Width of the `[x] ` checkbox prefix a multi-select option carries.
pub(super) const ROW_CHECKBOX_WIDTH: usize = 4;
/// Gap between the label column and the description column.
pub(super) const ROW_COLUMN_GAP: usize = 2;
/// Narrowest description column worth aligning beside a label. Below this the
/// block stacks its description under the label instead of squeezing it into a
/// sliver that clips mid-word.
pub(super) const ROW_DESCRIPTION_MIN_WIDTH: usize = 28;
/// Label column ceiling as a percentage of the row's content width, so one long
/// label cannot starve its description.
pub(super) const ROW_LABEL_COLUMN_PERCENT: usize = 40;
/// Indent of a stacked description under its own label.
pub(super) const ROW_STACKED_INDENT: usize = 2;
/// Indent of an axis value row under its section header.
const ROW_AXIS_INDENT: usize = 4;
/// Everything [`row_key_prefix`] spends before the key: marker, indent and gap.
/// Exported so a fits-or-collapses decision sizes the prefix the vocabulary draws.
pub(super) const ROW_AXIS_PREFIX_WIDTH: usize = ROW_MARKER_WIDTH + ROW_AXIS_INDENT + ROW_COLUMN_GAP;

/// One row's content, before geometry turns it into lines.
#[derive(Debug, Clone, Copy)]
pub(super) struct RowBlock<'a> {
    pub selected: bool,
    /// `Some(checked)` draws a `[x] ` / `[ ] ` prefix of constant width, so a
    /// measurement taken with an empty selection stays valid.
    pub checkbox: Option<bool>,
    pub label: &'a str,
    /// Wrapped in the description column, or stacked under the label when the
    /// row is too narrow to hold a column.
    pub description: Option<&'a str>,
}

/// How one row lays out at a given width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RowGeometry {
    /// Columns the marker and the optional checkbox occupy.
    pub prefix_width: usize,
    /// Width of the label column; `0` in the stacked layout.
    pub label_column: usize,
    /// Indent every description continuation row starts at.
    pub description_indent: usize,
    /// Wrap width of the description.
    pub description_width: usize,
    /// True when the description sits under the label instead of beside it.
    pub stacked: bool,
}

/// Resolve [`RowGeometry`] for a label of `label_width` cells inside `width`.
///
/// The description gets a column only when the label fits the column cap *and*
/// the remaining width is still worth a column; otherwise the block stacks.
pub(super) fn row_geometry(label_width: usize, checkbox: bool, width: u16) -> RowGeometry {
    let prefix_width = ROW_MARKER_WIDTH + if checkbox { ROW_CHECKBOX_WIDTH } else { 0 };
    let content = usize::from(width).saturating_sub(prefix_width).max(1);
    let label_cap = (content * ROW_LABEL_COLUMN_PERCENT / 100).max(1);
    let aligned_width = content
        .saturating_sub(label_width)
        .saturating_sub(ROW_COLUMN_GAP);
    if label_width <= label_cap && aligned_width >= ROW_DESCRIPTION_MIN_WIDTH {
        RowGeometry {
            prefix_width,
            label_column: label_width,
            description_indent: prefix_width + label_width + ROW_COLUMN_GAP,
            description_width: aligned_width,
            stacked: false,
        }
    } else {
        RowGeometry {
            prefix_width,
            label_column: 0,
            description_indent: prefix_width + ROW_STACKED_INDENT,
            description_width: content.saturating_sub(ROW_STACKED_INDENT).max(1),
            stacked: true,
        }
    }
}

/// The accent `> ` marker. Selection is the arrow alone — never reverse video.
pub(super) fn row_marker_style() -> Style {
    theme::body(theme::palette().tool)
}

/// Dim style for trailing metadata (`· current`, `(project · subagent)`).
pub(super) fn row_trailing_style() -> Style {
    theme::dim()
}

/// One selectable row: marker, label, and an optional trailing subtitle on the
/// same line. Callers that want an aligned trailing column pad it with
/// [`pad_ascii`] before handing it over.
pub(super) fn row_line(
    selected: bool,
    label: String,
    trailing: Option<String>,
    enabled: bool,
) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let label_style = if enabled {
        theme::body(theme::palette().text)
    } else {
        theme::dim()
    };
    let mut spans = vec![
        Span::styled(marker, row_marker_style()),
        Span::styled(label, label_style),
    ];
    if let Some(trailing) = trailing {
        spans.push(Span::styled("  ", theme::dim()));
        spans.push(Span::styled(trailing, row_trailing_style()));
    }
    Line::from(spans)
}

/// One row block: the label row, then the description wrapped in its column
/// with a continuation indent equal to that column offset. The caller adds the
/// blank line between blocks.
pub(super) fn row_block(row: &RowBlock<'_>, width: u16) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;

    let label_width = UnicodeWidthStr::width(row.label);
    let geometry = row_geometry(label_width, row.checkbox.is_some(), width);

    let mut prefix = vec![Span::styled(
        if row.selected { "> " } else { "  " },
        row_marker_style(),
    )];
    if let Some(checked) = row.checkbox {
        prefix.push(Span::styled(
            if checked { "[x] " } else { "[ ] " },
            if checked {
                theme::label(theme::palette().tool)
            } else {
                theme::dim()
            },
        ));
    }
    let label = Span::styled(row.label.to_string(), theme::body(theme::palette().text));

    let Some(description) = row
        .description
        .map(str::trim)
        .filter(|description| !description.is_empty())
    else {
        let mut spans = prefix;
        spans.push(label);
        return vec![Line::from(spans)];
    };

    if geometry.stacked {
        let mut first = prefix;
        first.push(label);
        let mut lines = vec![Line::from(first)];
        lines.extend(indented_description(description, &geometry));
        return lines;
    }

    let description_lines = wrap_line(
        &Line::from(Span::styled(description.to_string(), theme::dim())),
        geometry.description_width,
    );
    let mut first = prefix;
    first.push(label);
    first.push(Span::styled(" ".repeat(ROW_COLUMN_GAP), theme::dim()));
    if let Some(chunk) = description_lines.first() {
        first.extend(chunk.spans.clone());
    }
    let mut lines = vec![Line::from(first)];
    for chunk in description_lines.iter().skip(1) {
        let mut spans = vec![Span::raw(" ".repeat(geometry.description_indent))];
        spans.extend(chunk.spans.clone());
        lines.push(Line::from(spans));
    }
    lines
}

fn indented_description(description: &str, geometry: &RowGeometry) -> Vec<Line<'static>> {
    wrap_line(
        &Line::from(Span::styled(description.to_string(), theme::dim())),
        geometry.description_width,
    )
    .into_iter()
    .map(|line| {
        let mut spans = vec![Span::raw(" ".repeat(geometry.description_indent))];
        spans.extend(line.spans);
        Line::from(spans)
    })
    .collect()
}

/// A row whose value carries its own colour: marker, label, a `·` separator, the
/// value, and an optional parenthesised note. The sandbox toggles colour their
/// state rather than their label, which `row_line` cannot express.
pub(super) fn row_value_line(
    selected: bool,
    label: &str,
    value: &str,
    value_color: Color,
    note: Option<&str>,
) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let mut spans = vec![
        Span::styled(marker, row_marker_style()),
        Span::styled(label.to_string(), theme::body(theme::palette().text)),
        Span::styled("  ·  ", theme::dim()),
        Span::styled(value.to_string(), theme::body(value_color)),
    ];
    if let Some(note) = note {
        spans.push(Span::styled(format!("  ({note})"), theme::dim()));
    }
    Line::from(spans)
}

/// The keyed prefix a mode or settings row starts with: marker, indent, the key
/// padded to its column, then the gap before the value column.
pub(super) fn row_key_prefix(selected: bool, key: &str, key_width: usize) -> Vec<Span<'static>> {
    vec![
        Span::styled(if selected { "> " } else { "  " }, row_marker_style()),
        Span::styled(" ".repeat(ROW_AXIS_INDENT), theme::muted()),
        Span::styled(format!("{key:<key_width$}"), theme::dim()),
        Span::styled(" ".repeat(ROW_COLUMN_GAP), theme::muted()),
    ]
}

/// One `row_block` per decision option, with a blank row between blocks. Labels
/// are padded to a shared column so the descriptions line up down the popup.
pub(super) fn decision_body(
    options: &[(String, String)],
    cursor: usize,
    inner_width: u16,
) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;

    let label_width = options
        .iter()
        .map(|(label, _)| UnicodeWidthStr::width(label.as_str()))
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    for (index, (label, description)) in options.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        let padded = pad_ascii(label, label_width);
        lines.extend(row_block(
            &RowBlock {
                selected: index == cursor,
                checkbox: None,
                label: &padded,
                description: Some(description.as_str()),
            },
            inner_width,
        ));
    }
    lines
}

/// The width a decision popup's content wraps at.
pub(super) fn decision_inner_width(area: Rect) -> u16 {
    ModalFootprint::Prompt
        .width(area)
        .saturating_sub(MODAL_FRAME_COLUMNS)
}

/// Rows a decision popup draws, so `modal_area` sizes it from the same builders
/// the renderer uses.
pub(super) fn decision_rows(
    header: &[Line<'static>],
    body: &[Line<'static>],
    footer: &[Line<'static>],
    inner_width: u16,
) -> u16 {
    let wrap = inner_width.max(1) as usize;
    let body_rows = body.iter().fold(0u16, |rows, line| {
        rows.saturating_add(wrapped_line_count(line, wrap))
    });
    shell_modal_rows(
        wrapped_lines_height(header, wrap) as u16,
        body_rows,
        wrapped_lines_height(footer, wrap) as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(label: &'a str, description: Option<&'a str>) -> RowBlock<'a> {
        RowBlock {
            selected: false,
            checkbox: None,
            label,
            description,
        }
    }

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn row_block_aligns_the_description_column() {
        let width = 80;
        let block = row_block(
            &row("Alpha", Some("a description that stays on one line")),
            width,
        );
        assert_eq!(block.len(), 1, "a short description fits the label row");
        let geometry = row_geometry(5, false, width);
        assert!(!geometry.stacked, "80 columns is wide enough for a column");
        assert_eq!(geometry.label_column, 5);
        assert_eq!(geometry.description_indent, 2 + 5 + ROW_COLUMN_GAP);
        let line = text(&block[0]);
        assert!(line.starts_with("  Alpha  a description"), "got {line:?}");
        assert_eq!(
            line.find("a description"),
            Some(geometry.label_column + geometry.prefix_width + ROW_COLUMN_GAP),
        );
    }

    #[test]
    fn row_block_continuation_rows_share_the_column_indent() {
        let width = 60;
        let description = "wrapping ".repeat(20);
        let block = row_block(&row("Alpha", Some(&description)), width);
        assert!(block.len() > 1, "a long description wraps onto more rows");
        let geometry = row_geometry(5, false, width);
        for line in block.iter().skip(1) {
            assert!(
                text(line).starts_with(&" ".repeat(geometry.description_indent)),
                "continuation lines start at the description column",
            );
            assert!(
                line.width() <= usize::from(width),
                "no continuation row may exceed the row width",
            );
        }
    }

    #[test]
    fn row_block_never_exceeds_the_row_width() {
        for width in [40u16, 60, 80, 120] {
            let description = "a long description ".repeat(12);
            let block = row_block(
                &row("A label that is quite long indeed", Some(&description)),
                width,
            );
            for line in &block {
                assert!(
                    line.width() <= usize::from(width),
                    "line {:?} is wider than {width}",
                    text(line),
                );
            }
        }
    }

    #[test]
    fn row_block_stacks_below_the_column_threshold() {
        // 30 columns leave 21 cells beside a 5-cell label — less than the 28 a
        // description column needs — so the block stacks instead of squeezing.
        let width = 30;
        let geometry = row_geometry(5, false, width);
        assert!(
            geometry.stacked,
            "21 cells cannot hold a description column"
        );
        let block = row_block(&row("Alpha", Some("stacked description text")), width);
        assert_eq!(block.len(), 2, "label row, then its description");
        assert!(
            text(&block[0]).starts_with("  Alpha"),
            "got {:?}",
            text(&block[0])
        );
        assert_eq!(
            text(&block[1]),
            format!(
                "{}{}",
                " ".repeat(geometry.description_indent),
                "stacked description text"
            ),
        );
    }

    #[test]
    fn row_block_without_a_description_is_one_row() {
        assert_eq!(row_block(&row("Alpha", None), 80).len(), 1);
        assert_eq!(row_block(&row("Alpha", Some("   ")), 80).len(), 1);
    }

    #[test]
    fn row_block_checkbox_prefix_keeps_a_constant_width() {
        let checked = row_block(
            &RowBlock {
                selected: true,
                checkbox: Some(true),
                label: "Use the cache",
                description: Some("keeps the prefix warm"),
            },
            80,
        );
        let unchecked = row_block(
            &RowBlock {
                selected: false,
                checkbox: Some(false),
                label: "Use the cache",
                description: Some("keeps the prefix warm"),
            },
            80,
        );
        assert!(text(&checked[0]).starts_with("> [x] Use the cache"));
        assert!(text(&unchecked[0]).starts_with("  [ ] Use the cache"));
        // Measurement with an empty selection stays valid only if the checkbox
        // width does not depend on its state.
        assert_eq!(checked[0].width(), unchecked[0].width());
    }

    #[test]
    fn row_line_is_arrow_only_selection() {
        let selected = row_line(true, "row".to_string(), Some("tag".to_string()), true);
        let unselected = row_line(false, "row".to_string(), Some("tag".to_string()), true);
        assert_eq!(selected.spans[0].content.as_ref(), "> ");
        assert_eq!(unselected.spans[0].content.as_ref(), "  ");
        assert_eq!(selected.spans[1].style, unselected.spans[1].style);
        assert!(
            !selected.spans[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn row_line_dims_a_disabled_label() {
        let enabled = row_line(false, "row".to_string(), None, true);
        let disabled = row_line(false, "row".to_string(), None, false);
        assert_ne!(enabled.spans[1].style, disabled.spans[1].style);
    }
}
