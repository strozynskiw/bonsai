use super::picker_common::*;
use super::*;
use crate::tui::local_model_wizard::{
    LocalModelMetadataField, LocalModelSetupField, LocalModelWizardState, LocalModelWizardStep,
};

const METADATA_MODEL_WIDTH: usize = 26;
const METADATA_VALUE_WIDTH: usize = 8;
const METADATA_MARKER_WIDTH: usize = 2;
const METADATA_CONTEXT_LABEL: &str = "  ctx ";
const METADATA_OUTPUT_LABEL: &str = " out ";
const METADATA_TOOLS_LABEL: &str = " tools ";
/// Label-column pad for the Setup-step form fields.
const WIZARD_LABEL_PAD: usize = 13;

/// The header / body / footer split of the wizard, shared by the renderer and
/// the cursor placement so the two can't drift as the constraints change.
fn wizard_regions(area: Rect) -> (Rect, Rect, Rect) {
    let inner = theme::frame(String::new(), true).inner(area);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(inner);
    (chunks[0], chunks[1], chunks[2])
}

pub(super) fn render_local_model_wizard(f: &mut Frame, area: Rect, state: &LocalModelWizardState) {
    let title = format!("Local Model Wizard - {}", state.step.title());
    let panel = theme::frame(&title, true).style(theme::panel());
    f.render_widget(panel, area);

    let (header_area, body_area, footer_area) = wizard_regions(area);

    render_wizard_header(f, header_area, state);
    match state.step {
        LocalModelWizardStep::Setup => render_setup_step(f, body_area, state),
        LocalModelWizardStep::SelectModels => render_select_step(f, body_area, state),
        LocalModelWizardStep::Metadata => render_metadata_step(f, body_area, state),
        LocalModelWizardStep::Review => render_review_step(f, body_area, state),
    }
    render_wizard_footer(f, footer_area, state);
    set_wizard_cursor(f, area, state);
}

fn render_wizard_header(f: &mut Frame, area: Rect, state: &LocalModelWizardState) {
    let mut lines = vec![Line::from(vec![
        Span::styled("Provider ", theme::muted()),
        Span::styled(
            if state.provider_id.is_empty() {
                "<unset>".to_string()
            } else {
                state.provider_id.clone()
            },
            theme::body(theme::palette().text),
        ),
        Span::styled("  Server ", theme::muted()),
        Span::styled(state.preset.label(), theme::body(theme::palette().text)),
        Span::styled("  Models ", theme::muted()),
        Span::styled(
            state.selected_model_count().to_string(),
            theme::body(theme::palette().text),
        ),
    ])];
    if let Some(error) = &state.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            theme::body(theme::palette().error),
        )));
    } else if let Some(status) = &state.status {
        lines.push(Line::from(Span::styled(status.clone(), theme::dim())));
    } else {
        lines.push(Line::from(""));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn render_setup_step(f: &mut Frame, area: Rect, state: &LocalModelWizardState) {
    let api_key_display = if state.api_key.is_empty() {
        String::new()
    } else {
        "*".repeat(state.api_key.chars().count())
    };
    let preset = state.preset.label();
    let credential_storage = state.credential_persistence.label();

    // (label, value, field, editable). The server preset is a toggle, not a
    // text input, so it gets no cursor.
    let fields: [(&str, &str, LocalModelSetupField, bool); 6] = [
        ("Server", preset, LocalModelSetupField::Preset, false),
        (
            "Name",
            state.display_name.as_str(),
            LocalModelSetupField::DisplayName,
            true,
        ),
        (
            "Provider ID",
            state.provider_id.as_str(),
            LocalModelSetupField::ProviderId,
            true,
        ),
        (
            "Base URL",
            state.base_url.as_str(),
            LocalModelSetupField::BaseUrl,
            true,
        ),
        (
            "API key",
            api_key_display.as_str(),
            LocalModelSetupField::ApiKey,
            true,
        ),
        (
            "Store key",
            credential_storage,
            LocalModelSetupField::CredentialStorage,
            false,
        ),
    ];

    let form_fields = fields
        .iter()
        .map(|&(label, value, field, editable)| FormField {
            label,
            value,
            active: state.setup_field == field,
            caret: if editable {
                FieldCaret::End
            } else {
                FieldCaret::None
            },
            value_style: theme::body(theme::palette().text),
        })
        .collect::<Vec<_>>();
    let (lines, cursor) = form_field_lines(&form_fields, WIZARD_LABEL_PAD, area.width);

    // Pre-wrapped to `area.width`; no ratatui word-wrap so the cursor below stays
    // in step with the rendered rows.
    f.render_widget(Paragraph::new(lines).style(theme::panel()), area);

    if !state.loading
        && let Some((row, col)) = cursor
    {
        f.set_cursor_position(clamp_cursor(area, area.x + col, area.y + row));
    }
}

fn render_select_step(f: &mut Frame, area: Rect, state: &LocalModelWizardState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(area);
    let (field_lines, (crow, ccol)) = wrapped_input_field(
        vec![Span::styled("Manual model: ", theme::muted())],
        &state.manual_model_input,
        theme::body(theme::palette().text),
        chunks[0].width,
    );
    f.render_widget(Paragraph::new(field_lines).style(theme::panel()), chunks[0]);
    if !state.loading {
        f.set_cursor_position(clamp_cursor(
            chunks[0],
            chunks[0].x + ccol,
            chunks[0].y + crow,
        ));
    }

    let visible = visible_picker_rows(
        state.models.len(),
        state.model_cursor,
        picker_body_height(chunks[1]),
    );
    let lines = if state.models.is_empty() {
        vec![Line::from(Span::styled("No fetched models", theme::dim()))]
    } else {
        visible
            .map(|idx| {
                let model = &state.models[idx];
                let checkbox = if model.selected { "[x] " } else { "[ ] " };
                picker_line(
                    idx == state.model_cursor,
                    format!("{checkbox}{}", model.remote_model),
                    None,
                    true,
                )
            })
            .collect()
    };
    render_picker_column(f, chunks[1], "Fetched Models", true, lines);
}

fn render_metadata_step(f: &mut Frame, area: Rect, state: &LocalModelWizardState) {
    let indices = state.visible_model_indices();
    let visible = visible_picker_rows(indices.len(), state.model_cursor, area.height as usize);
    let lines = if indices.is_empty() {
        vec![Line::from(Span::styled("No selected models", theme::dim()))]
    } else {
        visible
            .filter_map(|row| {
                let idx = *indices.get(row)?;
                let model = state.models.get(idx)?;
                let selected = row == state.model_cursor;
                let tool = if model.tool_call { "yes" } else { "no" };
                let marker = if selected { "> " } else { "  " };
                Some(Line::from(vec![
                    Span::styled(marker, theme::body(theme::palette().agent_accent)),
                    Span::styled(
                        pad_ascii(&model.remote_model, METADATA_MODEL_WIDTH),
                        theme::body(theme::palette().text),
                    ),
                    Span::styled(METADATA_CONTEXT_LABEL, theme::muted()),
                    Span::styled(
                        pad_ascii(&model.context_window, METADATA_VALUE_WIDTH),
                        metadata_style(
                            selected,
                            state.metadata_field == LocalModelMetadataField::ContextWindow,
                        ),
                    ),
                    Span::styled(METADATA_OUTPUT_LABEL, theme::muted()),
                    Span::styled(
                        pad_ascii(
                            if model.output_limit.is_empty() {
                                "-"
                            } else {
                                &model.output_limit
                            },
                            METADATA_VALUE_WIDTH,
                        ),
                        metadata_style(
                            selected,
                            state.metadata_field == LocalModelMetadataField::OutputLimit,
                        ),
                    ),
                    Span::styled(METADATA_TOOLS_LABEL, theme::muted()),
                    Span::styled(
                        tool.to_string(),
                        metadata_style(
                            selected,
                            state.metadata_field == LocalModelMetadataField::ToolCalls,
                        ),
                    ),
                ]))
            })
            .collect()
    };
    f.render_widget(
        Paragraph::new(lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn metadata_style(selected: bool, active: bool) -> Style {
    if selected && active {
        theme::body(theme::palette().agent_accent)
    } else {
        theme::body(theme::palette().text)
    }
}

fn render_review_step(f: &mut Frame, area: Rect, state: &LocalModelWizardState) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Name: ", theme::muted()),
            Span::styled(
                state.display_name.clone(),
                theme::body(theme::palette().text),
            ),
        ]),
        Line::from(vec![
            Span::styled("Provider ID: ", theme::muted()),
            Span::styled(
                state.provider_id.clone(),
                theme::body(theme::palette().text),
            ),
        ]),
        Line::from(vec![
            Span::styled("Server: ", theme::muted()),
            Span::styled(state.preset.label(), theme::body(theme::palette().text)),
        ]),
        Line::from(vec![
            Span::styled("Base URL: ", theme::muted()),
            Span::styled(state.base_url.clone(), theme::body(theme::palette().text)),
        ]),
        Line::from(vec![
            Span::styled("Store key: ", theme::muted()),
            Span::styled(
                state.credential_persistence.label(),
                theme::body(theme::palette().text),
            ),
        ]),
        Line::from(vec![
            Span::styled("Files: ", theme::muted()),
            Span::styled(
                format!(
                    "providers/{}.toml and models/{}.toml",
                    state.provider_id, state.provider_id
                ),
                theme::body(theme::palette().text),
            ),
        ]),
        Line::from(""),
    ];
    for model in state.models.iter().filter(|model| model.selected) {
        let output = if model.output_limit.trim().is_empty() {
            "default"
        } else {
            model.output_limit.trim()
        };
        let tools = if model.tool_call { "tools" } else { "no tools" };
        let context = if model.context_window.trim().is_empty() {
            "unknown"
        } else {
            model.context_window.trim()
        };
        lines.push(Line::from(vec![
            Span::styled("- ", theme::muted()),
            Span::styled(
                model.remote_model.clone(),
                theme::body(theme::palette().text),
            ),
            Span::styled(
                format!("  ctx {context}  out {output}  {tools}"),
                theme::dim(),
            ),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_wizard_footer(f: &mut Frame, area: Rect, state: &LocalModelWizardState) {
    let submit = match state.step {
        LocalModelWizardStep::Setup => "fetch",
        LocalModelWizardStep::SelectModels => "metadata",
        LocalModelWizardStep::Metadata => "review",
        LocalModelWizardStep::Review => "write",
    };
    let mut lines = vec![footer_hint_line(&[
        ("Enter", submit),
        ("Esc", "cancel"),
        ("Left", "back"),
    ])];
    let second = match state.step {
        LocalModelWizardStep::Setup => {
            footer_hint_line(&[("Tab/Up/Down", "field"), ("←/→ or Space", "cycle option")])
        }
        LocalModelWizardStep::SelectModels => footer_hint_line(&[
            ("Up/Down", "move"),
            ("Space", "select"),
            ("Type", "add manual model"),
        ]),
        LocalModelWizardStep::Metadata => footer_hint_line(&[
            ("Up/Down", "model"),
            ("Tab", "field"),
            ("Space", "toggle tools"),
        ]),
        LocalModelWizardStep::Review => footer_hint_line(&[("Review", "only")]),
    };
    if state.loading {
        lines.push(Line::from(Span::styled("Working...", theme::dim())));
    } else {
        lines.push(second);
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn set_wizard_cursor(f: &mut Frame, area: Rect, state: &LocalModelWizardState) {
    if state.loading {
        return;
    }
    // Setup and SelectModels place their own wrap-aware cursors inside their
    // render fns; only the Metadata table cursor is positioned here.
    let LocalModelWizardStep::Metadata = state.step else {
        return;
    };
    let indices = state.visible_model_indices();
    if indices.is_empty() || state.active_model().is_none() {
        return;
    }
    let column = match state.metadata_field {
        LocalModelMetadataField::ContextWindow => metadata_context_value_column(),
        LocalModelMetadataField::OutputLimit => metadata_output_value_column(),
        LocalModelMetadataField::ToolCalls => return,
    };
    let len = match state.metadata_field {
        LocalModelMetadataField::ContextWindow => state
            .active_model()
            .map(|model| unicode_width::UnicodeWidthStr::width(model.context_window.as_str()))
            .unwrap_or(0),
        LocalModelMetadataField::OutputLimit => state
            .active_model()
            .map(|model| unicode_width::UnicodeWidthStr::width(model.output_limit.as_str()))
            .unwrap_or(0),
        LocalModelMetadataField::ToolCalls => 0,
    };
    // Derive the body rect from the same split the renderer uses, instead of
    // re-encoding the header/footer/border offsets as magic numbers.
    let (_, body_area, _) = wizard_regions(area);
    let body_height = body_area.height as usize;
    let visible = visible_picker_rows(indices.len(), state.model_cursor, body_height);
    let row = state.model_cursor.saturating_sub(visible.start);
    // Metadata values are short numbers, but clamp anyway so the cursor can
    // never leave the modal box.
    let inner = theme::frame(String::new(), true).inner(area);
    f.set_cursor_position(clamp_cursor(
        inner,
        body_area.x + column + len as u16,
        body_area.y + row as u16,
    ));
}

fn metadata_context_value_column() -> u16 {
    (METADATA_MARKER_WIDTH + METADATA_MODEL_WIDTH + METADATA_CONTEXT_LABEL.len()) as u16
}

fn metadata_output_value_column() -> u16 {
    (METADATA_MARKER_WIDTH
        + METADATA_MODEL_WIDTH
        + METADATA_CONTEXT_LABEL.len()
        + METADATA_VALUE_WIDTH
        + METADATA_OUTPUT_LABEL.len()) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::local_model_wizard::LocalModelWizardModel;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn metadata_context_cursor_lands_at_value_end() {
        let mut state = LocalModelWizardState {
            step: LocalModelWizardStep::Metadata,
            metadata_field: LocalModelMetadataField::ContextWindow,
            models: vec![LocalModelWizardModel {
                remote_model: "qwen/qwen3.6-35b-a3b".to_string(),
                display_name: None,
                selected: true,
                context_window: "30000".to_string(),
                output_limit: String::new(),
                tool_call: true,
            }],
            ..LocalModelWizardState::default()
        };
        state.model_cursor = 0;

        let mut terminal =
            Terminal::new(TestBackend::new(120, 12)).expect("test backend should initialize");
        terminal
            .draw(|f| render_local_model_wizard(f, Rect::new(0, 0, 120, 12), &state))
            .expect("wizard should render");

        let cursor = terminal
            .get_cursor_position()
            .expect("cursor should be visible");
        // Anchored to the padded body rect (Padding::horizontal(1)), matching
        // where the value text actually renders.
        assert_eq!(cursor, Position { x: 41, y: 3 });
    }

    #[test]
    fn metadata_cursor_uses_display_width() {
        let mut state = LocalModelWizardState {
            step: LocalModelWizardStep::Metadata,
            metadata_field: LocalModelMetadataField::ContextWindow,
            models: vec![LocalModelWizardModel {
                remote_model: "qwen/qwen3.6-35b-a3b".to_string(),
                display_name: None,
                selected: true,
                context_window: "界界".to_string(),
                output_limit: String::new(),
                tool_call: true,
            }],
            ..LocalModelWizardState::default()
        };
        state.model_cursor = 0;

        let mut terminal =
            Terminal::new(TestBackend::new(120, 12)).expect("test backend should initialize");
        terminal
            .draw(|f| render_local_model_wizard(f, Rect::new(0, 0, 120, 12), &state))
            .expect("wizard should render");

        let cursor = terminal
            .get_cursor_position()
            .expect("cursor should be visible");
        assert_eq!(cursor, Position { x: 40, y: 3 });
    }

    #[test]
    fn setup_and_review_steps_render_their_content() {
        let area = Rect::new(0, 0, 100, 18);
        let mut state = LocalModelWizardState {
            display_name: "Local Test".to_string(),
            provider_id: "local-test".to_string(),
            base_url: "http://localhost:1234/v1".to_string(),
            ..LocalModelWizardState::default()
        };
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        terminal
            .draw(|frame| render_local_model_wizard(frame, area, &state))
            .expect("setup step should render");
        let setup: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(setup.contains("Local Model Wizard - Setup"));
        assert!(setup.contains("http://localhost:1234/v1"));
        assert!(setup.contains("protected file"));

        state.step = LocalModelWizardStep::Review;
        terminal
            .draw(|frame| render_local_model_wizard(frame, area, &state))
            .expect("review step should render");
        let review: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(review.contains("Local Model Wizard - Review"));
        assert!(review.contains("providers/local-test.toml"));
        assert!(review.contains("protected file"));
    }
}
