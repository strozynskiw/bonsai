use super::picker_common::*;
use super::*;

/// Label-column pad for the endpoint-auth form fields.
const AUTH_LABEL_PAD: usize = 8;

pub(super) fn render_api_key_prompt(f: &mut Frame, area: Rect, app: &AppState, provider_id: &str) {
    if app.provider_uses_endpoint_auth_form(provider_id) {
        render_openai_compatible_prompt(f, area, app, provider_id);
        return;
    }

    let block = theme::frame("Authorize Provider", true);
    let inner = block.inner(area);

    let (field_lines, (cursor_row, cursor_col)) = wrapped_input_field(
        vec![Span::styled("API key: ", theme::muted())],
        &"*".repeat(app.provider_auth_form.api_key_input.chars().count()),
        theme::body(theme::palette().text),
        inner.width,
    );

    let mut lines = vec![
        Line::from(Span::styled(
            format!("Provider: {provider_id}"),
            theme::body(theme::palette().text),
        )),
        Line::from(""),
    ];
    let field_start = lines.len() as u16;
    lines.extend(field_lines);
    lines.push(Line::from(""));
    lines.push(kv(
        "Store",
        app.provider_auth_form.credential_persistence.label(),
        theme::palette().text,
    ));
    lines.push(Line::from(""));
    lines.push(footer_hint_line(&[
        ("Ctrl+P", "storage"),
        ("Enter", "submit"),
        ("Esc", "cancel"),
    ]));

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        area,
    );
    f.set_cursor_position(clamp_cursor(
        inner,
        inner.x + cursor_col,
        inner.y + field_start + cursor_row,
    ));
}

pub(super) fn render_openai_compatible_prompt(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    provider_id: &str,
) {
    let block = theme::frame("Authorize Provider", true);
    let inner = block.inner(area);

    let mut lines = vec![
        Line::from(Span::styled(
            format!("Provider: {provider_id}"),
            theme::body(theme::palette().text),
        )),
        Line::from(""),
    ];

    let masked_api_key = "*".repeat(app.provider_auth_form.api_key_input.chars().count());
    let fields = [
        (
            ProviderAuthField::BaseUrl,
            "Base URL",
            &app.provider_auth_form.provider_base_url_input,
        ),
        (ProviderAuthField::ApiKey, "API key", &masked_api_key),
        (
            ProviderAuthField::Model,
            "Model",
            &app.provider_auth_form.provider_model_input,
        ),
        (
            ProviderAuthField::ContextWindow,
            "Context",
            &app.provider_auth_form.context_window_input,
        ),
    ];

    let form_fields = fields
        .iter()
        .map(|&(field, label, value)| FormField {
            label,
            value: value.as_str(),
            active: app.provider_auth_form.provider_auth_field == field,
            caret: FieldCaret::End,
            value_style: theme::body(theme::palette().text),
        })
        .collect::<Vec<_>>();
    let header_offset = lines.len() as u16;
    let (field_lines, cursor) = form_field_lines(&form_fields, AUTH_LABEL_PAD, inner.width);
    lines.extend(field_lines);

    lines.push(kv(
        "Store",
        app.provider_auth_form.credential_persistence.label(),
        theme::palette().text,
    ));

    lines.push(Line::from(""));
    lines.push(footer_hint_line(&[
        ("Tab", "field"),
        ("Ctrl+P", "storage"),
        ("Enter", "submit"),
        ("Esc", "cancel"),
    ]));

    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        area,
    );
    if let Some((row, col)) = cursor {
        f.set_cursor_position(clamp_cursor(
            inner,
            inner.x + col,
            inner.y + header_offset + row,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn app() -> AppState {
        AppState::new("codex", "gpt-5".to_string(), ".".to_string(), None)
    }

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn api_key_form_renders_provider_key_and_actions() {
        let area = Rect::new(0, 0, 70, 12);
        let mut app = app();
        app.provider_auth_form.api_key_input = "secret".to_string();
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        terminal
            .draw(|frame| render_api_key_prompt(frame, area, &app, "codex"))
            .expect("API-key form should render");
        let text = rendered_text(&terminal);
        assert!(text.contains("Provider: codex"));
        assert!(text.contains("API key: ******"));
        assert!(!text.contains("secret"));
        assert!(text.contains("protected file"));
        assert!(text.contains("Enter submit"));
    }

    #[test]
    fn compatible_form_renders_all_fields() {
        let area = Rect::new(0, 0, 80, 16);
        let mut app = app();
        app.provider_auth_form.provider_base_url_input = "http://localhost/v1".to_string();
        app.provider_auth_form.api_key_input = "secret".to_string();
        app.provider_auth_form.provider_model_input = "local-model".to_string();
        app.provider_auth_form.context_window_input = "32768".to_string();
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        terminal
            .draw(|frame| render_openai_compatible_prompt(frame, area, &app, "local"))
            .expect("compatible form should render");
        let text = rendered_text(&terminal);
        assert!(text.contains("Base URL"));
        assert!(text.contains("local-model"));
        assert!(text.contains("Context"));
        assert!(text.contains("******"));
        assert!(!text.contains("secret"));
        assert!(text.contains("protected file"));
        assert!(text.contains("Tab field"));
    }

    #[test]
    fn unauthorize_picker_renders_title_rows_and_footer() {
        let area = Rect::new(0, 0, 62, 12);
        let app = app();
        let providers = vec![ProviderOption {
            provider_id: "opencode".to_string(),
            provider_label: "OpenCode Go".to_string(),
            authorized: true,
            current: true,
            uses_endpoint_auth_form: false,
        }];
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        terminal
            .draw(|frame| render_unauthorize_provider_picker(frame, area, &app, &providers, "", 0))
            .expect("unauthorize picker should render");
        let text = rendered_text(&terminal);
        assert!(text.contains("Unauthorize Provider"));
        assert!(text.contains("OpenCode Go"));
        assert!(text.contains("Enter unauthorize"));
    }

    #[test]
    fn unauthorize_picker_shows_empty_state_without_authorized_providers() {
        let area = Rect::new(0, 0, 62, 10);
        let app = app();
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        terminal
            .draw(|frame| render_unauthorize_provider_picker(frame, area, &app, &[], "", 0))
            .expect("unauthorize picker should render");
        let text = rendered_text(&terminal);
        assert!(text.contains("No authorized providers"));
    }

    #[test]
    fn unauthorize_confirm_renders_provider_name_and_hints() {
        let area = Rect::new(0, 0, 66, 8);
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        terminal
            .draw(|frame| render_unauthorize_confirm(frame, area, "OpenCode Go"))
            .expect("unauthorize confirm should render");
        let text = rendered_text(&terminal);
        assert!(text.contains("Clear Bonsai auth for OpenCode Go?"));
        assert!(text.contains("Enter/Y unauthorize"));
        assert!(text.contains("Esc/N cancel"));
    }
}

pub(super) fn render_authorize_provider_picker(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    providers: &[ProviderOption],
    query: &str,
    cursor: usize,
) {
    let header = vec![Line::from(vec![
        Span::styled("Search: ", theme::muted()),
        Span::styled(query.to_string(), theme::body(theme::palette().text)),
    ])];
    let footer = vec![
        Line::from(vec![
            Span::styled("Legend: ", theme::dim()),
            Span::styled("authorized", theme::body(theme::palette().success)),
            Span::styled(" / ", theme::dim()),
            Span::styled("not authorized", theme::body(theme::palette().text)),
        ]),
        footer_hint_line(&[
            ("Type", "search"),
            ("Up/Down", "move"),
            ("Enter", "authorize"),
            ("Esc", "cancel"),
        ]),
    ];
    let filtered = crate::tui::pickers::filter_authorize_providers(providers, query);
    render_list_picker(
        f,
        area,
        "Authorize Provider",
        &header,
        &footer,
        |body_area| {
            if filtered.is_empty() {
                let empty = if providers.is_empty() {
                    "No supported providers"
                } else {
                    "No matching providers"
                };
                return vec![Line::from(Span::styled(empty, theme::dim()))];
            }
            let cursor = cursor.min(filtered.len().saturating_sub(1));
            let columns = authorize_provider_columns_filtered(body_area.width as usize, &filtered);
            let capacity = picker_body_height(body_area).max(1);
            let offset = crate::tui::app::reconcile_viewport(
                app.authorize_provider_offset.get(),
                cursor,
                filtered.len(),
                capacity,
            );
            app.authorize_provider_offset.set(offset);
            (offset..(offset + capacity.min(filtered.len())).min(filtered.len()))
                .map(|idx| authorize_provider_line(filtered[idx], &columns, idx == cursor))
                .collect()
        },
    );
}

pub(super) fn render_unauthorize_provider_picker(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    providers: &[ProviderOption],
    query: &str,
    cursor: usize,
) {
    let header = vec![Line::from(vec![
        Span::styled("Search: ", theme::muted()),
        Span::styled(query.to_string(), theme::body(theme::palette().text)),
    ])];
    let footer = vec![footer_hint_line(&[
        ("Type", "search"),
        ("Up/Down", "move"),
        ("Enter", "unauthorize"),
        ("Esc", "cancel"),
    ])];
    let filtered = crate::tui::pickers::filter_authorize_providers(providers, query);
    render_list_picker(
        f,
        area,
        "Unauthorize Provider",
        &header,
        &footer,
        |body_area| {
            if filtered.is_empty() {
                let empty = if providers.is_empty() {
                    "No authorized providers"
                } else {
                    "No matching providers"
                };
                return vec![Line::from(Span::styled(empty, theme::dim()))];
            }
            let cursor = cursor.min(filtered.len().saturating_sub(1));
            let columns = authorize_provider_columns_filtered(body_area.width as usize, &filtered);
            let capacity = picker_body_height(body_area).max(1);
            // The two provider pickers are mutually exclusive, so they share
            // the authorize picker's viewport offset cell.
            let offset = crate::tui::app::reconcile_viewport(
                app.authorize_provider_offset.get(),
                cursor,
                filtered.len(),
                capacity,
            );
            app.authorize_provider_offset.set(offset);
            (offset..(offset + capacity.min(filtered.len())).min(filtered.len()))
                .map(|idx| authorize_provider_line(filtered[idx], &columns, idx == cursor))
                .collect()
        },
    );
}

pub(super) fn render_unauthorize_confirm(f: &mut Frame, area: Rect, display_name: &str) {
    let panel = theme::frame("Unauthorize Provider", true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(inner);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Clear Bonsai auth for {display_name}?"),
                theme::body(theme::palette().text),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Stored credentials for this provider will be removed.",
                theme::dim(),
            )),
        ])
        .style(theme::panel())
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    f.render_widget(
        Paragraph::new(footer_hint_line(&[
            ("Enter/Y", "unauthorize"),
            ("Esc/N", "cancel"),
        ])),
        chunks[1],
    );
}

pub(super) fn render_review_scope_picker(f: &mut Frame, area: Rect, cursor: usize) {
    let footer = vec![
        Line::from(vec![Span::styled(
            "Choose which changes to review.",
            theme::dim(),
        )]),
        footer_hint_line(&[("Up/Down", "move"), ("Enter", "review"), ("Esc", "cancel")]),
    ];
    render_list_picker(f, area, "Review", &[], &footer, |body_area| {
        let scopes = crate::agent::ReviewScope::all();
        let cursor = cursor.min(scopes.len().saturating_sub(1));
        visible_picker_rows(scopes.len(), cursor, picker_body_height(body_area))
            .map(|idx| {
                let scope = scopes[idx];
                picker_line(
                    idx == cursor,
                    scope.label().to_string(),
                    Some(scope.description().to_string()),
                    true,
                )
            })
            .collect()
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AuthorizeProviderColumns {
    pub(super) provider: usize,
    pub(super) details: usize,
}

/// Column layout for the filtered rows actually on screen. Column widths key
/// off the widest *visible* label, so filtering can reclaim horizontal space.
pub(super) fn authorize_provider_columns_filtered(
    width: usize,
    providers: &[&ProviderOption],
) -> AuthorizeProviderColumns {
    authorize_provider_columns_for(
        width,
        providers
            .iter()
            .map(|provider| provider.provider_label.chars().count())
            .max()
            .unwrap_or(0),
    )
}

fn authorize_provider_columns_for(width: usize, widest_label: usize) -> AuthorizeProviderColumns {
    const MARKER_WIDTH: usize = 2;
    const GAP_WIDTH: usize = 2;
    const GAP_COUNT: usize = 1;
    const MIN_PROVIDER_WIDTH: usize = 10;
    const MIN_DETAILS_WIDTH: usize = 12;

    let available = width.saturating_sub(MARKER_WIDTH + (GAP_WIDTH * GAP_COUNT));
    let max_provider = available.saturating_sub(MIN_DETAILS_WIDTH);
    let preferred_provider = widest_label.max(MIN_PROVIDER_WIDTH);
    let provider = preferred_provider.min(max_provider).max(MIN_PROVIDER_WIDTH);
    let details = available.saturating_sub(provider);

    AuthorizeProviderColumns { provider, details }
}

pub(super) fn authorize_provider_line(
    provider: &ProviderOption,
    columns: &AuthorizeProviderColumns,
    selected: bool,
) -> Line<'static> {
    let marker_style = theme::muted();
    let label_color = if provider.authorized {
        theme::palette().success
    } else {
        theme::palette().text
    };
    let label_style = theme::body(label_color);
    let meta_style = theme::muted();
    let marker = if selected { "> " } else { "  " };
    let mut spans = vec![
        Span::styled(marker, marker_style),
        Span::styled(
            pad_ascii(&provider.provider_label, columns.provider),
            label_style,
        ),
        Span::styled("  ", meta_style),
    ];
    spans.extend(authorize_provider_detail_spans(
        provider,
        columns.details,
        meta_style,
    ));
    Line::from(spans)
}

pub(super) fn authorize_provider_detail_spans(
    provider: &ProviderOption,
    width: usize,
    meta_style: Style,
) -> Vec<Span<'static>> {
    if provider.provider_id.chars().count() > width {
        return vec![Span::styled(
            truncate_ascii(&provider.provider_id, width),
            meta_style,
        )];
    }

    vec![Span::styled(provider.provider_id.clone(), meta_style)]
}
