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
pub(super) enum CellRender<T> {
    /// A raw value the builder pads to the column width.
    Value(fn(&T) -> String),
    /// A value that already fits the column width (e.g. a width-aware command
    /// truncator); rendered as-is, not padded.
    Fitted(fn(&T, usize) -> String),
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
            CellRender::Fitted(f) => f(item, width),
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
        match column.width {
            TableWidth::Fit { cap } => {
                // A Fitted column is width-aware and belongs in the flex slot;
                // measuring it here would need a width it doesn't have.
                let value: fn(&T) -> String = match &column.render {
                    CellRender::Value(f) => *f,
                    CellRender::Fitted(_) => |_: &T| String::new(),
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

/// One data row: `> ` marker, then each column's cell separated by two spaces.
pub(super) fn table_row<T>(
    item: &T,
    columns: &[TableColumn<T>],
    widths: &[usize],
    selected: bool,
) -> Line<'static> {
    let meta = theme::muted();
    let marker = if selected { "> " } else { "  " };
    let mut spans = vec![Span::styled(marker, meta)];
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", meta));
        }
        let style = match &column.style {
            CellStyle::Meta => meta,
            CellStyle::Text => theme::body(theme::palette().text),
            CellStyle::Custom(f) => f(item),
        };
        spans.push(Span::styled(column.value(item, widths[index]), style));
    }
    Line::from(spans)
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
        let row = table_row(&items[0], &cols(), &w, true);
        // Header and row render to the same display width, so columns line up.
        assert_eq!(header.width(), row.width());
    }
}
