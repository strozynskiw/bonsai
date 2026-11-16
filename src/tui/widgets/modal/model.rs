use super::picker_common::*;
use super::*;

pub(super) fn render_model_picker(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    entries: &[ModelOption],
) {
    let panel = theme::frame("Models", true).style(theme::panel());
    f.render_widget(panel, area);
    let layout = model_picker_layout(area);

    // Shape the whole picker once; the three panes read from this view instead
    // of re-deriving provider rows and the filtered list per pane.
    let view = app.model_picker_view(entries);
    let provider_rows = &view.provider_rows;
    let provider_cursor = app
        .model_picker
        .provider_cursor
        .min(provider_rows.len().saturating_sub(1));
    let model_rows = &view.filtered_models;
    let model_row_count = model_rows.len() + usize::from(view.reset_label.is_some());
    let model_cursor = app
        .model_picker
        .cursor
        .min(model_row_count.saturating_sub(1));
    let selected_model = view.selected_model;
    let reasoning_choices = selected_model
        .map(AppState::model_picker_reasoning_choices)
        .unwrap_or_default();
    // Every pane takes the full body height: the price and the shortcut cue live
    // in the footer, so no pane reserves a bottom row of its own.
    let provider_capacity = pane_line_budget(layout.panes[MODEL_PICKER_PANE_PROVIDER]);
    let model_capacity = pane_line_budget(layout.panes[MODEL_PICKER_PANE_MODEL]);
    let reasoning_capacity = pane_line_budget(layout.panes[MODEL_PICKER_PANE_REASONING]);

    f.render_widget(
        Paragraph::new(model_picker_filter_line(
            &app.model_picker.filter,
            layout.filter.width as usize,
        )),
        layout.filter,
    );

    render_picker_column(
        f,
        layout.panes[MODEL_PICKER_PANE_PROVIDER],
        "Provider",
        app.model_picker.active_pane == ModelPickerPane::Provider,
        picker_viewport_rows(
            provider_rows.len(),
            provider_cursor,
            app.model_picker.provider_offset,
            provider_capacity,
        )
        .map(|idx| {
            let entry = &provider_rows[idx];
            row_line(
                idx == provider_cursor,
                entry.provider_label.clone(),
                None,
                true,
            )
        })
        .collect(),
    );

    let model_lines = if model_rows.is_empty() {
        let mut lines = view
            .reset_label
            .map(|reset_label| {
                row_line(
                    true,
                    reset_label.to_string(),
                    Some("clear this composer slot".to_string()),
                    true,
                )
            })
            .into_iter()
            .collect::<Vec<_>>();
        let empty_message = if provider_rows.is_empty() {
            "Run /authorize <provider>"
        } else {
            "No matching models"
        };
        lines.push(Line::from(Span::styled(empty_message, theme::dim())));
        lines
    } else {
        let inner_width = layout.panes[MODEL_PICKER_PANE_MODEL]
            .width
            .saturating_sub(2) as usize;
        picker_viewport_rows(
            model_row_count,
            model_cursor,
            app.model_picker.model_offset,
            model_capacity,
        )
        .map(|idx| {
            if let Some(reset_label) = view.reset_label
                && idx == 0
            {
                return row_line(
                    idx == model_cursor,
                    reset_label.to_string(),
                    Some("clear this composer slot".to_string()),
                    true,
                );
            }
            let entry_index = idx.saturating_sub(usize::from(view.reset_label.is_some()));
            model_row_line(idx == model_cursor, model_rows[entry_index], inner_width)
        })
        .collect()
    };
    render_picker_column(
        f,
        layout.panes[MODEL_PICKER_PANE_MODEL],
        "Model",
        app.model_picker.active_pane == ModelPickerPane::Model,
        model_lines,
    );

    let effort_lines = selected_model
        .map(|entry| {
            picker_viewport_rows(
                reasoning_choices.len(),
                app.model_picker.reasoning_cursor,
                app.model_picker.reasoning_offset,
                reasoning_capacity,
            )
            .map(|idx| {
                let reasoning = reasoning_choices[idx];
                let detail = reasoning_picker_detail(entry, reasoning);
                row_line(
                    idx == app.model_picker.reasoning_cursor,
                    reasoning.to_string(),
                    detail,
                    true,
                )
            })
            .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec![Line::from(Span::styled("default", theme::dim()))]);
    render_picker_column(
        f,
        layout.panes[MODEL_PICKER_PANE_REASONING],
        "Reasoning",
        app.model_picker.active_pane == ModelPickerPane::Reasoning,
        effort_lines,
    );

    let footer_width = layout.footer.width as usize;
    let summary = view
        .reset_label
        .filter(|_| view.reset_selected)
        .map(|label| format!("{label} · omit this model-chain override"))
        .or_else(|| {
            selected_model.map(|entry| {
                selected_model_summary(entry, app.model_picker_selected_reasoning(entry))
            })
        })
        .unwrap_or_else(|| {
            if provider_rows.is_empty() {
                "No authorized models — run /authorize <provider>; reset remains available"
            } else {
                "No models match the current filter"
            }
            .to_string()
        });
    // The footer band explains the picker before it reports on it: orientation, then
    // the selected model's summary and price, then the keys. No pane spends a body
    // row on text of its own.
    let price = selected_model
        .map(selected_model_price_text)
        .unwrap_or_default();
    // Only the active pane assigns shortcuts with a bare letter, so the cue belongs to
    // that pane rather than to a row the Reasoning list has to reserve.
    let mut hints = footer_hint_line(model_picker_hints(app.model_picker.active_pane));
    // The hint row never wraps, so the Reasoning-only cue is what yields when it would
    // push `Esc` past the footer's edge.
    if hints.width() > footer_width {
        hints = footer_hint_line(MODEL_PICKER_CHOOSE);
    }
    let mut footer_lines = if layout.help {
        model_picker_help_lines()
    } else {
        Vec::new()
    };
    footer_lines.push(model_picker_summary_line(&summary, &price, footer_width));
    footer_lines.push(hints_with_shortcut_usage(
        hints,
        selected_model,
        footer_width,
    ));
    f.render_widget(
        Paragraph::new(footer_lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        layout.footer,
    );
}

/// The orientation lines are a luxury: they show only while the panes still get a
/// readable body, so a short terminal keeps its rows for models.
pub(super) const MODEL_PICKER_HELP_MIN_BODY_ROWS: u16 = 8;

fn model_picker_help_rows(width: u16) -> u16 {
    wrapped_lines_height(&model_picker_help_lines(), width.max(1) as usize) as u16
}

/// Rows the footer band takes, and whether it carries the orientation lines.
/// `body_rows` is the height the panes get while the band includes them; the layout
/// and `model_picker_rows` both pass that, so the measured popup and the drawn one
/// agree.
pub(super) fn model_picker_footer_rows(body_rows: u16, width: u16) -> (u16, bool) {
    let with_help = model_picker_help_rows(width).saturating_add(2);
    if body_rows >= MODEL_PICKER_HELP_MIN_BODY_ROWS {
        (with_help, true)
    } else {
        (2, false)
    }
}

/// Orientation above the summary: what the three panes are, which keys move between
/// them, and what a bare letter does where it is pressed.
fn model_picker_help_lines() -> Vec<Line<'static>> {
    let key = |text: &'static str| Span::styled(text, theme::body(theme::palette().text));
    let dim = |text: &'static str| Span::styled(text, theme::dim());
    vec![
        Line::from(vec![
            Span::styled("Panes ", theme::muted()),
            dim("Provider → Model → Reasoning  ·  "),
            key("←/→/Tab"),
            dim(" switch  ·  "),
            key("↑/↓"),
            dim(" move  ·  "),
            key("Enter"),
            dim(" select"),
        ]),
        Line::from(vec![
            Span::styled("Keys  ", theme::muted()),
            dim("type to filter  ·  in Reasoning a letter binds "),
            key("/<key>"),
            dim(" to model + effort"),
        ]),
    ]
}

/// Provider, model and reasoning pane indices, in the order [`ModelPickerLayout`]
/// stores them.
pub(super) const MODEL_PICKER_PANE_PROVIDER: usize = 0;
pub(super) const MODEL_PICKER_PANE_MODEL: usize = 1;
pub(super) const MODEL_PICKER_PANE_REASONING: usize = 2;

/// The picker's geometry. The renderer and [`model_picker_capacities`] both read
/// it, so the panes a list is windowed against are the panes that get drawn — a
/// split change can no longer leave the offset clamps on a stale height.
#[derive(Debug, Clone, Copy)]
pub(super) struct ModelPickerLayout {
    /// The filter row above the panes.
    pub filter: Rect,
    /// Provider, model and reasoning panes, left to right, all spanning the full
    /// body height.
    pub panes: [Rect; 3],
    /// The footer band: the orientation lines, the selected-model summary and the
    /// hint row.
    pub footer: Rect,
    /// Whether the band carries the orientation lines.
    pub help: bool,
}

pub(super) fn model_picker_layout(area: Rect) -> ModelPickerLayout {
    let inner = theme::frame("Models", true).inner(area);
    let help_rows = model_picker_help_rows(inner.width);
    let body_with_help = inner
        .height
        .saturating_sub(1)
        .saturating_sub(help_rows.saturating_add(2));
    let (footer_rows, help) = model_picker_footer_rows(body_with_help, inner.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(footer_rows),
        ])
        .split(inner);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(28),
            Constraint::Percentage(48),
            Constraint::Percentage(24),
        ])
        .split(chunks[1]);
    ModelPickerLayout {
        filter: chunks[0],
        panes: [columns[0], columns[1], columns[2]],
        footer: chunks[2],
        help,
    }
}

/// `(provider, model, reasoning)` rows each pane can show: its framed inner height,
/// measured by the same layout the renderer uses.
pub(crate) fn model_picker_capacities(area: Rect) -> (usize, usize, usize) {
    let layout = model_picker_layout(area);
    (
        pane_line_budget(layout.panes[MODEL_PICKER_PANE_PROVIDER]),
        pane_line_budget(layout.panes[MODEL_PICKER_PANE_MODEL]),
        pane_line_budget(layout.panes[MODEL_PICKER_PANE_REASONING]),
    )
}

fn picker_viewport_rows(
    len: usize,
    cursor: usize,
    offset: usize,
    capacity: usize,
) -> std::ops::Range<usize> {
    if len == 0 {
        return 0..0;
    }
    let capacity = capacity.max(1).min(len);
    let cursor = cursor.min(len.saturating_sub(1));
    let offset = offset.min(len.saturating_sub(capacity));
    let start = if cursor < offset {
        cursor
    } else if cursor >= offset.saturating_add(capacity) {
        cursor.saturating_add(1).saturating_sub(capacity)
    } else {
        offset
    };
    start..start.saturating_add(capacity)
}

fn model_picker_filter_line(filter: &str, width: usize) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;

    let prefix = "Filter models: ";
    let legend = "icons: ⚒︎ tools  ◉ vision  ∴ thinking";
    let prefix_width = UnicodeWidthStr::width(prefix);
    let legend_width = UnicodeWidthStr::width(legend);
    let minimum_filter_width = 8;

    if width >= prefix_width + minimum_filter_width + legend_width + 2 {
        let filter_width = width.saturating_sub(prefix_width + legend_width + 2);
        let filter = truncate_ascii(filter, filter_width);
        let used_width = prefix_width + UnicodeWidthStr::width(filter.as_str()) + legend_width;
        let gap = width.saturating_sub(used_width).max(2);
        return Line::from(vec![
            Span::styled(prefix, theme::muted()),
            Span::styled(filter, theme::body(theme::palette().text)),
            Span::styled(" ".repeat(gap), theme::dim()),
            Span::styled(legend, theme::dim()),
        ]);
    }

    let filter_width = width.saturating_sub(prefix_width);
    Line::from(vec![
        Span::styled(prefix, theme::muted()),
        Span::styled(
            truncate_ascii(filter, filter_width),
            theme::body(theme::palette().text),
        ),
    ])
}

/// One compact row per model: name first, then context + capability icons.
fn model_row_line(selected: bool, entry: &ModelOption, width: usize) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;

    let marker = if selected { "> " } else { "  " };
    let label_style = theme::body(theme::palette().text);
    let shortcut_badges = shortcut_badges_for_model(entry);
    let shortcut_width = shortcut_badges
        .as_ref()
        .map(|value| UnicodeWidthStr::width(value.as_str()))
        .unwrap_or(0);
    let shortcut_prefix_width = if shortcut_width == 0 { 0 } else { 2 };
    // Width budget: the model name comes first; metadata (context window +
    // capability icons) is shown only when the row is wide enough, keeping the
    // name at least MODEL_NAME_RESERVED_WIDTH cells and dropping metadata that
    // would shrink below a legible minimum.
    const MODEL_ROW_MIN_WIDTH_FOR_METADATA: usize = 24;
    const MODEL_NAME_RESERVED_WIDTH: usize = 16;
    const MODEL_METADATA_MIN_WIDTH: usize = 4;
    let metadata = model_picker_metadata(entry);
    let metadata_full_width = UnicodeWidthStr::width(metadata.as_str());
    let marker_width = UnicodeWidthStr::width(marker);
    let available_width = width.saturating_sub(marker_width);
    let metadata_budget =
        if metadata.is_empty() || available_width < MODEL_ROW_MIN_WIDTH_FOR_METADATA {
            0
        } else {
            metadata_full_width.min(
                available_width
                    .saturating_sub(shortcut_prefix_width + shortcut_width)
                    .saturating_sub(MODEL_NAME_RESERVED_WIDTH),
            )
        };
    let metadata = if metadata_budget >= MODEL_METADATA_MIN_WIDTH {
        // Model metadata (metadata): context window and capability icons are a
        // summary; the footer summary spells the selected model out in full.
        truncate_ascii(&metadata, metadata_budget)
    } else {
        String::new()
    };
    let metadata_width = UnicodeWidthStr::width(metadata.as_str());
    let metadata_prefix_width = if metadata_width == 0 { 0 } else { 2 };
    let suffix_width =
        metadata_prefix_width + metadata_width + shortcut_prefix_width + shortcut_width;
    let name_width = available_width.saturating_sub(suffix_width);

    let mut spans = vec![
        Span::styled(marker, theme::muted()),
        // Model label (identifier): the row's metadata and badges keep their
        // cells, and the picker column wraps the whole row if it still overflows.
        Span::styled(
            truncate_ascii(entry.picker_model_label(), name_width),
            label_style,
        ),
    ];
    if !metadata.is_empty() {
        spans.push(Span::styled("  ", theme::dim()));
        spans.push(Span::styled(metadata, theme::dim()));
    }
    if let Some(shortcut_badges) = shortcut_badges {
        spans.push(Span::styled("  ", theme::dim()));
        spans.push(Span::styled(
            shortcut_badges,
            theme::label(theme::palette().tool),
        ));
    }

    Line::from(spans)
}

fn model_picker_metadata(entry: &ModelOption) -> String {
    let mut parts = vec![entry.context_window_label()];
    let icons = entry.feature_icons();
    if !icons.is_empty() {
        parts.push(icons);
    }
    if entry.unverified {
        parts.push("unverified".to_string());
    }
    parts.join("  ")
}

fn selected_model_summary(
    entry: &ModelOption,
    reasoning: crate::provider::ReasoningSelection,
) -> String {
    let mut parts = vec![
        entry.provider_label.clone(),
        entry.picker_model_label().to_string(),
        entry.context_window_label(),
    ];
    let icons = entry.feature_icons();
    if !icons.is_empty() {
        parts.push(icons);
    }
    parts.push(format!("reasoning: {reasoning}"));
    if !entry.parameter_preview.is_empty() {
        parts.push(entry.parameter_preview.clone());
    }
    parts.join(" · ")
}

/// The selected model's price and provenance, as one line of metadata. It joins
/// the summary in the picker footer instead of sitting under the model list, so
/// no pane spends a body row on text.
fn selected_model_price_text(entry: &ModelOption) -> String {
    let mut parts = vec![format!("price: {}", entry.pricing_label())];
    if !entry.catalog_drift.is_empty() {
        parts.push("⚠ catalog drift".to_string());
    }
    if entry.unverified {
        parts.push("⚠ unverified live model".to_string());
    }
    // Where the model's metadata came from is diagnostic noise when everything agrees;
    // the footer keeps that line about the model, not about the catalog.
    let sources = entry.metadata_sources.compact_label();
    if (!entry.catalog_drift.is_empty() || entry.unverified) && !sources.is_empty() {
        parts.push(format!("sources: {sources}"));
    }
    parts.join(" · ")
}

/// The footer's first row: the selected model's summary, then its price when the
/// row can hold both. When it cannot, the price drops rather than pushing the
/// footer onto a third row. Both halves are clipped on purpose: this is
/// single-row chrome, not a table cell.
fn model_picker_summary_line(summary: &str, price: &str, width: usize) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;

    // Cells the summary keeps when the price shares the row.
    const MIN_SUMMARY_WIDTH: usize = 12;
    const SEPARATOR: &str = " · ";

    let price = truncate_ascii(
        price,
        width.saturating_sub(MIN_SUMMARY_WIDTH + SEPARATOR.len()),
    );
    let price_width = UnicodeWidthStr::width(price.as_str());
    if price.is_empty() || price_width + MIN_SUMMARY_WIDTH + SEPARATOR.len() > width {
        return Line::from(Span::styled(truncate_ascii(summary, width), theme::dim()));
    }
    Line::from(vec![
        Span::styled(
            truncate_ascii(summary, width - price_width - SEPARATOR.len()),
            theme::dim(),
        ),
        Span::styled(SEPARATOR, theme::muted()),
        Span::styled(price, theme::muted()),
    ])
}

/// Append the shortcut *invocation* hint to the button row, right-aligned so
/// it reads as a trailing legend rather than another key hint. Pairs with the
/// Reasoning pane's "press letter → shortcut" assignment cue: shows the
/// selected model's own bindings (`/g /h`) when it has them, otherwise the
/// `/[letter]` template so the mechanism is discoverable before anything is
/// assigned. Dropped entirely when the row is too narrow to hold both.
fn hints_with_shortcut_usage(
    mut hints: Line<'static>,
    entry: Option<&ModelOption>,
    width: usize,
) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;

    let label = "shortcut usage: ";
    let usage = entry
        .map(|entry| {
            let mut keys = Vec::new();
            for (key, _reasoning) in &entry.shortcut_bindings {
                let key = key.as_char();
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
            if keys.is_empty() {
                "/[letter]".to_string()
            } else {
                keys.iter()
                    .map(|key| format!("/{key}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        })
        .unwrap_or_else(|| "/[letter]".to_string());

    let hints_width: usize = hints
        .spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let usage_width = label.len() + UnicodeWidthStr::width(usage.as_str());
    // Only attach the legend when it fits with at least two cells of breathing
    // room after the key hints; otherwise the button row stands alone.
    if width > hints_width + usage_width + 2 {
        let gap = width - hints_width - usage_width;
        hints
            .spans
            .push(Span::styled(" ".repeat(gap), theme::dim()));
        hints.spans.push(Span::styled(label, theme::muted()));
        hints.spans.push(Span::styled(usage, theme::dim()));
    }
    hints
}

fn shortcut_badges_for_model(entry: &ModelOption) -> Option<String> {
    shortcut_badges(
        entry
            .shortcut_bindings
            .iter()
            .map(|(key, _reasoning)| key.as_char()),
    )
}

fn shortcut_badges_for_reasoning(
    entry: &ModelOption,
    reasoning: crate::provider::ReasoningSelection,
) -> Option<String> {
    shortcut_badges(
        entry
            .shortcut_bindings
            .iter()
            .filter(move |(_key, bound_reasoning)| *bound_reasoning == reasoning)
            .map(|(key, _reasoning)| key.as_char()),
    )
}

fn reasoning_picker_detail(
    entry: &ModelOption,
    reasoning: crate::provider::ReasoningSelection,
) -> Option<String> {
    let mut details = Vec::new();
    if entry.recommended_reasoning == Some(reasoning) {
        details.push("recommended".to_string());
    }
    if entry.discouraged_reasoning.contains(&reasoning) {
        details.push("long reasoning risk".to_string());
    }
    if let Some(shortcuts) = shortcut_badges_for_reasoning(entry, reasoning) {
        details.push(shortcuts);
    }
    (!details.is_empty()).then(|| details.join(" · "))
}

fn shortcut_badges(keys: impl Iterator<Item = char>) -> Option<String> {
    let keys = keys.map(|key| key.to_string()).collect::<Vec<_>>();
    (!keys.is_empty()).then_some(keys.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ReasoningSelection;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn reasoning_detail_labels_recommendation_and_risk() {
        let mut entry = crate::tui::test_utils::model_option("opencode", "OpenCode Go", "glm-5.2");
        entry.recommended_reasoning = Some(ReasoningSelection::Default);
        entry.discouraged_reasoning = vec![ReasoningSelection::High, ReasoningSelection::Max];

        assert_eq!(
            reasoning_picker_detail(&entry, ReasoningSelection::Default).as_deref(),
            Some("recommended")
        );
        assert_eq!(
            reasoning_picker_detail(&entry, ReasoningSelection::Max).as_deref(),
            Some("long reasoning risk")
        );
        assert_eq!(
            reasoning_picker_detail(&entry, ReasoningSelection::Medium),
            None
        );
    }

    #[test]
    fn composer_reset_and_authorization_guidance_render_without_models() {
        let area = Rect::new(0, 0, 90, 18);
        let mut app = AppState::new("codex", "model".to_string(), ".".to_string(), None);
        app.model_picker.target = crate::tui::app::ModelPickerTarget::ComposerPrimary;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");

        terminal
            .draw(|frame| render_model_picker(frame, area, &app, &[]))
            .expect("picker should render");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("> Parent default"));
        assert!(text.contains("/authorize <provider>"));
    }

    fn row_text(buffer: &ratatui::buffer::Buffer, width: u16, row: u16) -> String {
        buffer
            .content()
            .iter()
            .skip(row as usize * width as usize)
            .take(width as usize)
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn panes_share_one_border_row_and_the_footer_holds_price_and_hints() {
        let area = Rect::new(0, 0, 100, 30);
        let layout = model_picker_layout(area);
        let pane_bottom = layout.panes[MODEL_PICKER_PANE_PROVIDER].bottom();
        for pane in layout.panes {
            assert_eq!(pane.y, layout.panes[0].y, "every pane starts on one row");
            assert_eq!(
                pane.height, layout.panes[0].height,
                "every pane is one height"
            );
            assert_eq!(pane.bottom(), pane_bottom, "every pane ends on one row");
        }
        // The band carries the orientation lines here, because 30 rows leave the panes
        // plenty: two help rows, the summary and the hint row.
        assert!(layout.help, "a roomy terminal shows the orientation lines");
        assert_eq!(layout.footer.height, 4, "help, summary and hints");
        assert!(
            layout.footer.y >= pane_bottom,
            "the footer starts below the panes"
        );

        let cramped = model_picker_layout(Rect::new(0, 0, 60, 12));
        assert!(
            !cramped.help,
            "a short terminal gives its rows to the panes instead"
        );

        let entries = vec![
            crate::tui::test_utils::model_option("opencode", "OpenCode Go", "glm-5.2"),
            crate::tui::test_utils::model_option("opencode", "OpenCode Go", "kimi-k2"),
        ];
        let app = AppState::new("codex", "model".to_string(), ".".to_string(), None);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        terminal
            .draw(|frame| render_model_picker(frame, area, &app, &entries))
            .expect("picker should render");
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("reasoning"), "the summary renders: {text}");
        assert!(
            text.contains("price:"),
            "the price joins the summary: {text}"
        );
        assert!(text.contains("select"), "the key hints render: {text}");

        // The regression this layout fixes: the price used to be drawn on the
        // panes' bottom border row.
        let border_row = row_text(buffer, area.width, pane_bottom - 1);
        assert!(
            !border_row.contains("price:") && !border_row.contains("reasoning"),
            "the shared border row stays a border: {border_row:?}"
        );
        // The band reads top-down: orientation, then the selected model, then the keys.
        let help_row = row_text(buffer, area.width, layout.footer.y);
        assert!(
            help_row.contains("Provider → Model → Reasoning") && help_row.contains("switch"),
            "the orientation lines name the panes and their keys: {help_row:?}"
        );
        let keys_row = row_text(buffer, area.width, layout.footer.y + 1);
        assert!(
            keys_row.contains("binds") && keys_row.contains("/<key>"),
            "the second line explains the letter shortcut: {keys_row:?}"
        );
        let summary_row = row_text(buffer, area.width, layout.footer.y + 2);
        assert!(
            summary_row.contains("price:"),
            "the price draws on the summary row: {summary_row:?}"
        );
    }
}
