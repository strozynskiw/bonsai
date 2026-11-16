use super::picker_common::*;
use super::*;

/// Marker column width (`> ` / `  `), shared by every picker table.
pub(super) const TABLE_MARKER_WIDTH: usize = 2;
/// Gap rendered between adjacent table columns.
pub(super) const TABLE_GAP_WIDTH: usize = 2;

/// How a column claims horizontal space.
pub(super) enum TableWidth {
    /// Fit the widest cell (floored at the header, capped at `cap`).
    Fit { cap: usize },
    /// A fixed width.
    Exact(usize),
    /// Take whatever is left after the fixed columns, never below `min`. Exactly
    /// one column per table should be `Flex`.
    Flex { min: usize },
}

/// How a cell's text is produced.
///
/// **Prose wraps, metadata clips.** A cell whose full text matters — a title, a
/// command, a source name, a description — is [`CellRender::Wrapped`], and its
/// column is [`TableWidth::Flex`] so the text wraps inside whatever width the
/// layout gives it. A cell whose content is metadata — an id, age, price, count,
/// badge, key or header — stays a single `Fit`/`Exact` cell and is clipped by
/// `truncate_ascii`; that loss is deliberate, because such a row is a summary and
/// the full value lives in the detail pane. Choose by what the reader needs, not
/// by how long the string happens to be.
pub(super) enum CellRender<T> {
    /// A raw value the builder pads to the column width.
    Value(fn(&T) -> String),
    /// Prose that wraps onto continuation rows instead of clipping. Requires a
    /// [`TableWidth::Flex`] column: the width comes from the layout, so measuring
    /// the text itself could not size it.
    Wrapped(fn(&T) -> String),
}

/// The style a cell's text carries.
pub(super) enum CellStyle<T> {
    /// Dimmed metadata (ids, ages, counts).
    Meta,
    /// Primary body text (titles, commands).
    Text,
    /// A per-row style, e.g. a status color.
    Custom(fn(&T) -> Style),
}

pub(super) struct TableColumn<T> {
    pub header: &'static str,
    pub width: TableWidth,
    pub render: CellRender<T>,
    pub style: CellStyle<T>,
}

impl<T> TableColumn<T> {
    fn value(&self, item: &T, width: usize) -> String {
        match &self.render {
            CellRender::Value(f) => pad_ascii(&f(item), width),
            // Wrapped cells never reach this path: `table_rows` renders them
            // itself. A silent width mismatch would be worse than a loud bug.
            CellRender::Wrapped(f) => {
                debug_assert!(false, "a wrapped cell is rendered by `table_rows`");
                f(item)
            }
        }
    }
}

/// Resolve each column's rendered width for `total` cells of horizontal space.
pub(super) fn table_widths<T>(total: usize, items: &[T], columns: &[TableColumn<T>]) -> Vec<usize> {
    let gaps = columns.len().saturating_sub(1) * TABLE_GAP_WIDTH;
    let mut widths = vec![0usize; columns.len()];
    let mut flex = None;
    let mut fixed = TABLE_MARKER_WIDTH + gaps;
    for (index, column) in columns.iter().enumerate() {
        // Wrapping decides a column's width from the layout, so a wrapped column
        // measured by its own text would be circular.
        debug_assert!(
            !matches!(column.render, CellRender::Wrapped(_))
                || matches!(column.width, TableWidth::Flex { .. }),
            "table column `{}` wraps prose, so it must be TableWidth::Flex",
            column.header
        );
        match column.width {
            TableWidth::Fit { cap } => {
                // A wrapped column takes its width from the layout, so it is never
                // measured by its own text.
                let value: fn(&T) -> String = match &column.render {
                    CellRender::Value(f) => *f,
                    CellRender::Wrapped(_) => |_: &T| String::new(),
                };
                let w = column_width(items, column.header, cap, value);
                widths[index] = w;
                fixed += w;
            }
            TableWidth::Exact(w) => {
                widths[index] = w;
                fixed += w;
            }
            TableWidth::Flex { .. } => flex = Some(index),
        }
    }
    if let Some(index) = flex {
        let min = match columns[index].width {
            TableWidth::Flex { min } => min,
            _ => 0,
        };
        widths[index] = total.saturating_sub(fixed).max(min);
    }
    widths
}

/// A cell whose text wraps inside `width` instead of clipping, with continuation
/// rows indented by `indent` (the columns rendered before it). Tables that carry
/// prose columns use this so a long description can never be cut mid-word.
pub(super) fn wrapped_cell(
    text: &str,
    width: usize,
    indent: usize,
    style: Style,
) -> Vec<Line<'static>> {
    wrap_line(
        &Line::from(Span::styled(text.to_string(), style)),
        width.max(1),
    )
    .into_iter()
    .enumerate()
    .map(|(index, line)| {
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        if index > 0 {
            spans.push(Span::raw(" ".repeat(indent)));
        }
        spans.extend(line.spans);
        Line::from(spans)
    })
    .collect()
}

/// The bold-muted header row.
pub(super) fn table_header<T>(columns: &[TableColumn<T>], widths: &[usize]) -> Line<'static> {
    let style = theme::muted().add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::styled("  ", style)];
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", style));
        }
        spans.push(Span::styled(pad_ascii(column.header, widths[index]), style));
    }
    Line::from(spans)
}

/// One data row, or several when a wrapped column's prose does not fit: `> `
/// marker, then each column's cell separated by two spaces. Continuation rows
/// blank-pad the columns that already finished, so a wrapped cell stays under its
/// own column instead of sliding left.
pub(super) fn table_rows<T>(
    item: &T,
    columns: &[TableColumn<T>],
    widths: &[usize],
    selected: bool,
) -> Vec<Line<'static>> {
    let meta = theme::muted();
    let mut cells: Vec<Vec<Line<'static>>> = Vec::with_capacity(columns.len());
    for (index, column) in columns.iter().enumerate() {
        // A zero-width column cannot wrap; let it show one cell rather than
        // emitting a line wider than the cell claims to be.
        let width = widths[index].max(1);
        let style = match &column.style {
            CellStyle::Meta => meta,
            CellStyle::Text => theme::body(theme::palette().text),
            CellStyle::Custom(f) => f(item),
        };
        let block = match &column.render {
            CellRender::Wrapped(text) => {
                wrap_line(&Line::from(Span::styled(text(item), style)), width.max(1))
                    .into_iter()
                    .map(|line| pad_line(line, width))
                    .collect::<Vec<_>>()
            }
            _ => vec![Line::from(Span::styled(column.value(item, width), style))],
        };
        cells.push(block);
    }
    let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    (0..height)
        .map(|row| {
            // The marker marks the row, not every line it wraps onto.
            let marker = if selected && row == 0 { "> " } else { "  " };
            let mut spans = vec![Span::styled(marker, meta)];
            for (index, _column) in columns.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled("  ", meta));
                }
                match cells[index].get(row) {
                    Some(line) => spans.extend(line.spans.iter().cloned()),
                    None => spans.push(Span::raw(" ".repeat(widths[index]))),
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// Widen a rendered cell to exactly `width` cells, so the columns after it stay
/// aligned on every wrapped row.
fn pad_line(line: Line<'static>, width: usize) -> Line<'static> {
    let used = line.width();
    if used >= width {
        return line;
    }
    let mut spans = line.spans;
    spans.push(Span::raw(" ".repeat(width - used)));
    Line::from(spans)
}

/// Lines one item's row occupies: the tallest column block. Windowing counts these
/// instead of assuming one line per row, which is what keeps a wrapped row's
/// cursor on screen.
pub(super) fn table_row_height<T>(item: &T, columns: &[TableColumn<T>], widths: &[usize]) -> usize {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| match &column.render {
            CellRender::Wrapped(text) => usize::from(wrapped_line_count(
                &Line::from(Span::raw(text(item))),
                widths[index].max(1),
            ))
            .max(1),
            CellRender::Value(_) => 1,
        })
        .max()
        .unwrap_or(1)
}

/// The table's visible rows, windowed by *line count* so a wrapped row can never
/// push the cursor below the fold. `visible_lines` is the body's own line budget,
/// header excluded.
pub(super) fn table_body_lines<T>(
    items: &[&T],
    columns: &[TableColumn<T>],
    widths: &[usize],
    cursor: usize,
    visible_lines: usize,
) -> Vec<Line<'static>> {
    if items.is_empty() {
        return Vec::new();
    }
    let heights = items
        .iter()
        .map(|item| table_row_height(*item, columns, widths))
        .collect::<Vec<_>>();
    let cursor = cursor.min(items.len().saturating_sub(1));
    let mut lines = Vec::new();
    for index in visible_weighted_rows(&heights, cursor, visible_lines.max(1)) {
        lines.extend(table_rows(items[index], columns, widths, index == cursor));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Row {
        name: &'static str,
    }

    fn cols() -> Vec<TableColumn<Row>> {
        vec![
            TableColumn {
                header: "ID",
                width: TableWidth::Fit { cap: 4 },
                render: CellRender::Value(|r| r.name.to_string()),
                style: CellStyle::Meta,
            },
            TableColumn {
                header: "Name",
                width: TableWidth::Flex { min: 6 },
                render: CellRender::Value(|r| r.name.to_string()),
                style: CellStyle::Text,
            },
            TableColumn {
                header: "X",
                width: TableWidth::Exact(3),
                render: CellRender::Value(|_| "x".to_string()),
                style: CellStyle::Meta,
            },
        ]
    }

    #[test]
    fn flex_absorbs_remainder_above_min() {
        let items = vec![Row { name: "ab" }];
        // marker 2 + id 2 + exact 3 + gaps (2*2) = 11; flex = 40 - 11 = 29.
        let w = table_widths(40, &items, &cols());
        assert_eq!(w[0], 2);
        assert_eq!(w[2], 3);
        assert_eq!(w[1], 29);
    }

    #[test]
    fn fit_respects_cap_and_header_floor() {
        let wide = table_widths(100, &[Row { name: "abcdefgh" }], &cols());
        assert_eq!(wide[0], 4, "id column capped at 4");
        let empty = table_widths(100, &Vec::<Row>::new(), &cols());
        assert_eq!(empty[0], 2, "id column floors at the 'ID' header width");
    }

    #[test]
    fn flex_never_drops_below_min() {
        let w = table_widths(10, &[Row { name: "x" }], &cols());
        assert!(w[1] >= 6, "flex column floored at its min");
    }

    #[test]
    fn header_and_row_align_under_wide_graphemes() {
        let items = vec![Row { name: "界界" }];
        let w = table_widths(40, &items, &cols());
        let header = table_header(&cols(), &w);
        let rows = table_rows(&items[0], &cols(), &w, true);
        assert_eq!(rows.len(), 1, "a single-line row stays one row");
        // Header and row render to the same display width, so columns line up.
        assert_eq!(header.width(), rows[0].width());
    }

    #[test]
    fn wrapped_cell_wraps_inside_its_column_and_indents_continuations() {
        let rows = wrapped_cell(
            "a description that needs more than a single row",
            20,
            8,
            Style::default(),
        );
        assert!(rows.len() > 1, "the text wraps instead of clipping");
        assert!(
            rows[0].width() <= 20,
            "the first row fits its column: {:?}",
            rows[0].width()
        );
        for row in rows.iter().skip(1) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.starts_with(&" ".repeat(8)),
                "continuation rows hang under the cell: {text:?}"
            );
            assert!(row.width() <= 20 + 8, "no row past the cell: {text:?}");
        }
    }

    fn prose_columns() -> Vec<TableColumn<Row>> {
        vec![
            TableColumn {
                header: "ID",
                width: TableWidth::Fit { cap: 4 },
                render: CellRender::Value(|r| r.name.to_string()),
                style: CellStyle::Meta,
            },
            TableColumn {
                header: "Body",
                width: TableWidth::Flex { min: 6 },
                render: CellRender::Wrapped(|r| r.name.to_string()),
                style: CellStyle::Text,
            },
        ]
    }

    #[test]
    fn a_wrapped_cell_spans_rows_and_stays_under_its_own_column() {
        let items = vec![Row {
            name: "a command that needs more than one row of prose",
        }];
        let columns = prose_columns();
        let widths = table_widths(30, &items, &columns);
        let rows = table_rows(&items[0], &columns, &widths, false);
        assert!(rows.len() > 1, "the prose wraps onto continuation rows");
        let body_column = TABLE_MARKER_WIDTH + widths[0] + TABLE_GAP_WIDTH;
        let text = |row: &Line<'static>| -> String {
            row.spans.iter().map(|span| span.content.as_ref()).collect()
        };
        for row in &rows {
            assert_eq!(row.width(), 30, "every row fills the table exactly");
        }
        for row in rows.iter().skip(1) {
            let line = text(row);
            assert!(
                line.chars().take(body_column).all(char::is_whitespace),
                "a continuation row blanks the columns that finished: {line:?}"
            );
            assert!(
                !line.trim().is_empty(),
                "a continuation row carries the wrapped prose: {line:?}"
            );
        }
    }

    #[test]
    fn a_wrapped_row_keeps_the_cursor_inside_the_line_budget() {
        let items = (0..8)
            .map(|_| Row {
                name: "a description long enough to wrap onto several rows",
            })
            .collect::<Vec<_>>();
        let columns = prose_columns();
        let widths = table_widths(30, &items, &columns);
        let refs = items.iter().collect::<Vec<_>>();
        let visible_lines = 6;
        let lines = table_body_lines(&refs, &columns, &widths, 7, visible_lines);
        assert!(
            lines.len() <= visible_lines,
            "the window spends a line budget, not a row count: {} lines",
            lines.len()
        );
        assert!(
            lines.iter().any(|line| line
                .spans
                .first()
                .is_some_and(|span| span.content.as_ref() == "> ")),
            "the selected row stays inside the window"
        );
    }
}
