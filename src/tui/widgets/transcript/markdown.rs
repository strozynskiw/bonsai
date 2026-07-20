use super::*;

pub(crate) fn render_markdown(body: &str, width: usize) -> Vec<Line<'static>> {
    render_markdown_with_depth(body, width, 0)
}

pub(super) fn render_markdown_with_depth(
    body: &str,
    width: usize,
    depth: usize,
) -> Vec<Line<'static>> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut current = Block::Paragraph(Vec::new());
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_lines: Vec<String> = Vec::new();
    let mut quote_depth: usize = 0;
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut item_marker: Option<String> = None;
    let mut text_style = md(theme::palette().text);
    let mut style_stack: Vec<Style> = Vec::new();
    let mut link_url: Option<String> = None;
    let mut table: Option<MarkdownTable> = None;

    let push_paragraph = |blocks: &mut Vec<Block>, current: &mut Block| {
        if let Block::Paragraph(spans) = current
            && !spans.is_empty()
        {
            blocks.push(Block::Paragraph(std::mem::take(spans)));
        }
    };

    let options = Options::all();
    let parser = Parser::new_ext(body, options);
    for event in parser {
        match event {
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                push_paragraph(&mut blocks, &mut current);
            }
            Event::Start(Tag::Heading { level, .. }) => {
                push_paragraph(&mut blocks, &mut current);
                style_stack.push(text_style);
                text_style = match level {
                    HeadingLevel::H1 => md_bold(theme::palette().border_active),
                    HeadingLevel::H2 => md_bold(theme::palette().user),
                    _ => md_bold(theme::palette().progress),
                };
            }
            Event::End(TagEnd::Heading(_)) => {
                let next = style_stack
                    .pop()
                    .unwrap_or_else(|| md(theme::palette().text));
                if let Block::Paragraph(spans) = &mut current
                    && !spans.is_empty()
                {
                    let spans = std::mem::take(spans);
                    blocks.push(Block::Heading(text_style, spans));
                }
                text_style = next;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                push_paragraph(&mut blocks, &mut current);
                quote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                push_paragraph(&mut blocks, &mut current);
                quote_depth = quote_depth.saturating_sub(1);
            }
            Event::Start(Tag::List(start)) => {
                push_paragraph(&mut blocks, &mut current);
                list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                push_paragraph(&mut blocks, &mut current);
                list_stack.pop();
                item_marker = None;
            }
            Event::Start(Tag::Item) => {
                push_paragraph(&mut blocks, &mut current);
                item_marker = Some(match list_stack.last().copied().flatten() {
                    Some(next) => format!("{:>2}. ", next),
                    None => "•   ".to_string(),
                });
            }
            Event::End(TagEnd::Item) => {
                push_paragraph(&mut blocks, &mut current);
                if let Some(Some(next)) = list_stack.last_mut() {
                    *next += 1;
                }
                item_marker = None;
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                push_paragraph(&mut blocks, &mut current);
                in_code_block = true;
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                let code = code_lines.join("\n");
                if is_markdown_language(&code_lang) && depth < 2 {
                    let body = strip_outer_fences(&code);
                    blocks.push(Block::NestedMarkdown(body));
                } else if let Some(markdown) = nested_markdown_fence_body(&code)
                    && depth < 2
                {
                    blocks.push(Block::NestedMarkdown(markdown));
                } else if !code_lines.iter().all(|line| line.is_empty()) {
                    blocks.push(Block::CodeBlock {
                        lang: code_lang.clone(),
                        body: code_lines.clone(),
                    });
                }
                in_code_block = false;
                code_lang.clear();
                code_lines.clear();
            }
            Event::Start(Tag::Table(_)) => {
                push_paragraph(&mut blocks, &mut current);
                table = Some(MarkdownTable::default());
            }
            Event::End(TagEnd::Table) => {
                if let Some(table) = table.take() {
                    blocks.push(Block::Table(table));
                }
            }
            Event::Start(Tag::TableHead) => {
                if let Some(table) = table.as_mut() {
                    table.in_header = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(table) = table.as_mut() {
                    table.finish_header();
                    table.in_header = false;
                }
            }
            Event::Start(Tag::TableRow) => {
                if let Some(table) = table.as_mut() {
                    table.start_row();
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(table) = table.as_mut() {
                    table.finish_row();
                }
            }
            Event::Start(Tag::TableCell) => {
                if let Some(table) = table.as_mut() {
                    table.start_cell();
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(table) = table.as_mut() {
                    table.finish_cell();
                }
            }
            Event::Text(text) => {
                if in_code_block {
                    code_lines.extend(
                        text.lines()
                            .map(|line| line.trim_end_matches('\r').to_string()),
                    );
                } else if let Some(table) = table.as_mut() {
                    table.push_text(&text);
                } else if let Block::Paragraph(spans) = &mut current {
                    push_block_prefix(spans, quote_depth, item_marker.as_deref());
                    spans.push(Span::styled(text.to_string(), text_style));
                }
            }
            Event::Code(code) => {
                if let Some(table) = table.as_mut() {
                    table.push_text(&code);
                } else if let Block::Paragraph(spans) = &mut current {
                    push_block_prefix(spans, quote_depth, item_marker.as_deref());
                    spans.push(Span::styled(
                        code.to_string(),
                        md_bold(theme::palette().command),
                    ));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(table) = table.as_mut() {
                    table.push_text(" ");
                } else if let Block::Paragraph(spans) = &mut current {
                    spans.push(Span::raw(" "));
                }
            }
            Event::Rule => {
                push_paragraph(&mut blocks, &mut current);
                blocks.push(Block::Rule);
            }
            Event::Start(Tag::Emphasis) => {
                style_stack.push(text_style);
                text_style = text_style.add_modifier(Modifier::ITALIC);
            }
            Event::End(TagEnd::Emphasis) => {
                text_style = style_stack
                    .pop()
                    .unwrap_or_else(|| md(theme::palette().text));
            }
            Event::Start(Tag::Strong) => {
                style_stack.push(text_style);
                text_style = text_style.add_modifier(Modifier::BOLD);
            }
            Event::End(TagEnd::Strong) => {
                text_style = style_stack
                    .pop()
                    .unwrap_or_else(|| md(theme::palette().text));
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                style_stack.push(text_style);
                text_style = md(theme::palette().user).add_modifier(Modifier::UNDERLINED);
                link_url = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => {
                text_style = style_stack
                    .pop()
                    .unwrap_or_else(|| md(theme::palette().text));
                if let (Some(url), Block::Paragraph(spans)) = (link_url.take(), &mut current) {
                    spans.push(Span::styled(
                        format!(" ({url})"),
                        md(theme::palette().muted),
                    ));
                }
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                if let Block::Paragraph(spans) = &mut current {
                    spans.push(Span::styled(html.to_string(), text_style));
                }
            }
            Event::TaskListMarker(checked) => {
                if let Block::Paragraph(spans) = &mut current {
                    push_block_prefix(spans, quote_depth, item_marker.as_deref());
                    spans.push(Span::styled(
                        if checked { "[x] " } else { "[ ] " },
                        md(theme::palette().muted),
                    ));
                }
            }
            Event::FootnoteReference(_) => {}
            Event::InlineMath(math) | Event::DisplayMath(math) => {
                if let Block::Paragraph(spans) = &mut current {
                    push_block_prefix(spans, quote_depth, item_marker.as_deref());
                    spans.push(Span::styled(
                        math.to_string(),
                        md_bold(theme::palette().muted),
                    ));
                }
            }
            _ => {}
        }
    }

    push_paragraph(&mut blocks, &mut current);

    if blocks.is_empty() {
        return vec![Line::from(vec![Span::styled(
            "(empty)",
            md(theme::palette().dim),
        )])];
    }

    // Render each block into its own group of lines, then join the groups
    // with a single blank separator line. A wrapped list item / paragraph is
    // one group, so its continuation rows stay together while distinct items,
    // paragraphs, headings, and code blocks each get breathing room. This is
    // what turns a packed wall of bullets into a readable list.
    let mut lines = Vec::new();
    let mut first_group = true;
    let mut push_group = |lines: &mut Vec<Line<'static>>, group: Vec<Line<'static>>| {
        if group.is_empty() {
            return;
        }
        if !first_group {
            lines.push(Line::from(""));
        }
        first_group = false;
        lines.extend(group);
    };
    for block in blocks {
        match block {
            Block::Paragraph(spans) => {
                if !spans.is_empty() {
                    push_group(&mut lines, vec![Line::from(spans)]);
                }
            }
            Block::Heading(style, spans) => {
                let mut heading_spans = Vec::with_capacity(spans.len() + 1);
                for span in spans {
                    let mut next = span.clone();
                    next.style = style.patch(span.style).add_modifier(Modifier::BOLD);
                    heading_spans.push(next);
                }
                push_group(&mut lines, vec![Line::from(heading_spans)]);
            }
            Block::Rule => push_group(
                &mut lines,
                vec![Line::from(vec![Span::styled(
                    "─".repeat(width.min(64)),
                    md(theme::palette().dim),
                )])],
            ),
            Block::CodeBlock { lang, body } => {
                push_group(&mut lines, render_code_block(&lang, &body, width));
            }
            Block::NestedMarkdown(md) => {
                push_group(
                    &mut lines,
                    render_markdown_with_depth(&md, width, depth + 1),
                );
            }
            Block::Table(table) => {
                push_group(&mut lines, render_table(&table, width));
            }
        }
    }

    while lines.first().is_some_and(|line| {
        line.spans.is_empty() || line.spans.iter().all(|s| s.content.is_empty())
    }) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| {
        line.spans.is_empty() || line.spans.iter().all(|s| s.content.is_empty())
    }) {
        lines.pop();
    }

    if lines.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "(empty)",
            md(theme::palette().dim),
        )]));
    }

    lines
}

#[expect(
    clippy::enum_variant_names,
    reason = "variant names like CodeBlock mirror the Markdown block vocabulary they model; renaming to satisfy the lint would obscure the domain terms"
)]
pub(super) enum Block {
    Paragraph(Vec<Span<'static>>),
    Heading(Style, Vec<Span<'static>>),
    Rule,
    CodeBlock { lang: String, body: Vec<String> },
    NestedMarkdown(String),
    Table(MarkdownTable),
}

pub(super) fn push_block_prefix(
    spans: &mut Vec<Span<'static>>,
    quote_depth: usize,
    marker: Option<&str>,
) {
    if !spans.is_empty() {
        return;
    }
    if quote_depth > 0 {
        spans.push(Span::styled(
            format!("│ {}", "  ".repeat(quote_depth - 1)),
            md(theme::palette().muted),
        ));
    }
    if let Some(marker) = marker {
        spans.push(Span::styled(marker.to_string(), md(theme::palette().muted)));
    }
}

#[derive(Default)]
pub(super) struct MarkdownTable {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    in_header: bool,
}

impl MarkdownTable {
    fn start_row(&mut self) {
        self.current_row.clear();
    }

    fn finish_row(&mut self) {
        if self.in_header && self.header.is_empty() {
            self.header = std::mem::take(&mut self.current_row);
        } else if !self.current_row.is_empty() {
            self.rows.push(std::mem::take(&mut self.current_row));
        }
    }

    fn finish_header(&mut self) {
        if self.header.is_empty() && !self.current_row.is_empty() {
            self.header = std::mem::take(&mut self.current_row);
        }
    }

    fn start_cell(&mut self) {
        self.current_cell.clear();
    }

    fn finish_cell(&mut self) {
        self.current_row
            .push(self.current_cell.trim().replace('\n', " "));
        self.current_cell.clear();
    }

    fn push_text(&mut self, text: &str) {
        self.current_cell.push_str(text);
    }
}

pub(super) fn is_markdown_language(lang: &str) -> bool {
    matches!(lang.trim().to_ascii_lowercase().as_str(), "md" | "markdown")
}

pub(super) fn strip_outer_fences(code: &str) -> String {
    let trimmed = code.trim();
    let mut lines: Vec<&str> = trimmed.split('\n').collect();
    if lines
        .first()
        .is_some_and(|line| line.trim().starts_with("```"))
    {
        lines.remove(0);
    }
    if lines
        .last()
        .is_some_and(|line| line.trim().starts_with("```"))
    {
        lines.pop();
    }
    lines.join("\n")
}

pub(super) fn nested_markdown_fence_body(code: &str) -> Option<String> {
    let trimmed = code.trim_start_matches('\n').trim_start();
    let lines: Vec<&str> = trimmed.split('\n').collect();
    if lines.is_empty() {
        return None;
    }

    let first = lines[0].trim();
    let inner_lang = first.strip_prefix("```")?.trim();
    if !is_markdown_language(inner_lang) {
        return None;
    }

    let mut end = lines.len();
    if end > 1 && lines[end - 1].trim().starts_with("```") {
        end -= 1;
    }

    Some(lines[1..end].join("\n").trim_end_matches('\n').to_string())
}

pub(super) fn render_code_block(
    lang: &str,
    code_lines: &[String],
    width: usize,
) -> Vec<Line<'static>> {
    let width = width.max(8);
    let label = code_block_label(lang);
    let total_bar = width.min(80);
    let prefix_len = 3 + label.chars().count() + 1; // "┌─ " + label + " "
    let remaining = total_bar.saturating_sub(prefix_len + 1);
    let bottom = format!("└{}┘", "─".repeat(total_bar.saturating_sub(2)));

    let mut lines = vec![Line::from(vec![
        Span::styled("┌─ ".to_string(), md(theme::palette().muted)),
        Span::styled(
            label.clone(),
            Style::default()
                .fg(theme::palette().tool)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}┐", "─".repeat(remaining)),
            md(theme::palette().muted),
        ),
    ])];

    // The frame is narrower than the transcript (capped at 80 cols), so code
    // must wrap at the frame's interior width — the outer layout only wraps
    // at pane width, which would let long lines poke out past the border.
    let interior = total_bar.saturating_sub(2);
    for code_line in code_lines {
        let highlighted = Line::from(crate::tui::widgets::syntax::highlight_line(code_line, lang));
        for wrapped in super::message::wrap_card_line(&highlighted, interior) {
            let mut spans = vec![Span::styled("│ ", code_style(theme::palette().dim))];
            spans.extend(wrapped.spans);
            lines.push(Line::from(spans));
        }
    }

    lines.push(Line::from(vec![Span::styled(
        bottom,
        md(theme::palette().muted),
    )]));

    lines
}

pub(super) fn render_table(table: &MarkdownTable, width: usize) -> Vec<Line<'static>> {
    let columns = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    if columns == 0 {
        return Vec::new();
    }

    let widths = table_column_widths(table, columns, width);
    let mut lines = Vec::new();
    let top = render_table_border(&widths, TableBorder::Top);
    let middle = render_table_border(&widths, TableBorder::Middle);
    let bottom = render_table_border(&widths, TableBorder::Bottom);

    if !table.header.is_empty() {
        lines.push(top);
        lines.push(render_table_row(&table.header, &widths, true));
        lines.push(middle);
    } else {
        lines.push(top);
    }
    for row in &table.rows {
        lines.push(render_table_row(row, &widths, false));
    }
    lines.push(bottom);
    lines
}

pub(super) enum TableBorder {
    Top,
    Middle,
    Bottom,
}

pub(super) fn render_table_border(widths: &[usize], border: TableBorder) -> Line<'static> {
    let (left, mid, right) = match border {
        TableBorder::Top => ('┌', '┬', '┐'),
        TableBorder::Middle => ('├', '┼', '┤'),
        TableBorder::Bottom => ('└', '┴', '┘'),
    };
    let mut spans = vec![Span::styled(left.to_string(), md(theme::palette().muted))];
    for (index, width) in widths.iter().enumerate() {
        spans.push(Span::styled(
            "─".repeat(*width + 2),
            md(theme::palette().muted),
        ));
        let join = if index + 1 == widths.len() {
            right
        } else {
            mid
        };
        spans.push(Span::styled(join.to_string(), md(theme::palette().muted)));
    }
    Line::from(spans)
}

pub(super) fn table_column_widths(
    table: &MarkdownTable,
    columns: usize,
    width: usize,
) -> Vec<usize> {
    let overhead = columns * 4 + 1;
    let available = width.saturating_sub(overhead).max(columns);
    let max_column_width = (available / columns).max(3);
    (0..columns)
        .map(|column| {
            std::iter::once(table.header.get(column))
                .chain(table.rows.iter().map(|row| row.get(column)))
                .flatten()
                .map(|cell| unicode_width::UnicodeWidthStr::width(cell.as_str()))
                .max()
                .unwrap_or(3)
                .min(max_column_width)
                .max(3)
        })
        .collect()
}

pub(super) fn render_table_row(row: &[String], widths: &[usize], header: bool) -> Line<'static> {
    let cell_style = if header {
        table_style(theme::palette().user).add_modifier(Modifier::BOLD)
    } else {
        table_style(theme::palette().text)
    };
    let mut spans = Vec::new();

    for (index, width) in widths.iter().enumerate() {
        let separator = if index == 0 { "│ " } else { " │ " };
        spans.push(Span::styled(
            separator.to_string(),
            md(theme::palette().muted),
        ));
        let cell = row.get(index).map(String::as_str).unwrap_or("");
        spans.push(Span::styled(fill(cell, *width), cell_style));
    }
    spans.push(Span::styled(" │".to_string(), md(theme::palette().muted)));

    Line::from(spans)
}

pub(super) fn code_block_label(lang: &str) -> String {
    let lang = lang.trim();
    if lang.is_empty() { "code" } else { lang }.to_string()
}

/// Markdown spans must not carry a background: the enclosing card paints it
/// via `card_line`, which honors span backgrounds only when set explicitly
/// (diff rows). A span-level `bg` here would punch a hole in the card.
pub(crate) fn md(fg: Color) -> Style {
    Style::default().fg(fg)
}

pub(crate) fn md_bold(fg: Color) -> Style {
    Style::default().fg(fg).add_modifier(Modifier::BOLD)
}

pub(super) fn code_style(fg: Color) -> Style {
    Style::default().fg(fg)
}

pub(super) fn table_style(fg: Color) -> Style {
    Style::default().fg(fg)
}

pub(super) fn rendered_line_width(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.as_ref().width())
        .sum()
}
