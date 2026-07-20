use super::picker_common::*;
use super::*;

pub(super) fn render_model_picker(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    entries: &[ModelOption],
) {
    let panel = theme::frame("Models", true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(2),
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
    // The Model column reserves its bottom row for the selected model's price,
    // mirroring the Reasoning column's shortcut-assignment hint below.
    let model_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(columns[1]);
    let provider_capacity = picker_body_height(columns[0]);
    let model_capacity = picker_body_height(model_area[0]);

    f.render_widget(
        Paragraph::new(model_picker_filter_line(
            &app.model_picker.filter,
            chunks[0].width as usize,
        )),
        chunks[0],
    );

    render_picker_column(
        f,
        columns[0],
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
            picker_line(
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
                picker_line(
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
        let inner_width = model_area[0].width.saturating_sub(2) as usize;
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
                return picker_line(
                    idx == model_cursor,
                    reset_label.to_string(),
                    Some("clear this composer slot".to_string()),
                    true,
                );
            }
            let entry_index = idx.saturating_sub(usize::from(view.reset_label.is_some()));
            model_picker_line(idx == model_cursor, model_rows[entry_index], inner_width)
        })
        .collect()
    };
    render_picker_column(
        f,
        model_area[0],
        "Model",
        app.model_picker.active_pane == ModelPickerPane::Model,
        model_lines,
    );
    f.render_widget(
        Paragraph::new(selected_model_price_line(
            selected_model,
            model_area[1].width as usize,
        )),
        model_area[1],
    );

    // Reserve the bottom row of the reasoning column for the shortcut-assignment
    // hint, leaving the rest for the reasoning list.
    let reasoning_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(columns[2]);
    let effort_lines = selected_model
        .map(|entry| {
            picker_viewport_rows(
                reasoning_choices.len(),
                app.model_picker.reasoning_cursor,
                app.model_picker.reasoning_offset,
                picker_body_height(reasoning_area[0]),
            )
            .map(|idx| {
                let reasoning = reasoning_choices[idx];
                let detail = reasoning_picker_detail(entry, reasoning);
                picker_line(
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
        reasoning_area[0],
        "Reasoning",
        app.model_picker.active_pane == ModelPickerPane::Reasoning,
        effort_lines,
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("press ", theme::dim()),
            Span::styled("letter", theme::body(theme::palette().text)),
            Span::styled(" → shortcut", theme::dim()),
        ])),
        reasoning_area[1],
    );

    let footer_width = chunks[2].width as usize;
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
    let hints = footer_hint_line(&[
        ("Left/Right/Tab", "pane"),
        ("Up/Down", "move"),
        ("Enter", "select"),
        ("Esc", "cancel"),
    ]);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                truncate_ascii(&summary, footer_width),
                theme::dim(),
            )),
            hints_with_shortcut_usage(hints, selected_model, footer_width),
        ]),
        chunks[2],
    );
}

pub(crate) fn model_picker_capacities(area: Rect) -> (usize, usize, usize) {
    let inner = theme::frame("Models", true).inner(area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(2),
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
    let model_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(columns[1]);
    let reasoning_area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(columns[2]);
    (
        picker_body_height(columns[0]),
        picker_body_height(model_area[0]),
        picker_body_height(reasoning_area[0]),
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
fn model_picker_line(selected: bool, entry: &ModelOption, width: usize) -> Line<'static> {
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

fn selected_model_price_line(entry: Option<&ModelOption>, width: usize) -> Line<'static> {
    let label = "price: ";
    let price = entry
        .map(ModelOption::pricing_label)
        .unwrap_or_else(|| "n/a".to_string());
    Line::from(vec![
        Span::styled(label, theme::muted()),
        Span::styled(
            truncate_ascii(&price, width.saturating_sub(label.len())),
            theme::dim(),
        ),
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
}
