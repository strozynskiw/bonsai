use super::picker_common::*;
use super::*;
use crate::tui::agent_composer::{
    AgentComposerField, AgentComposerState, AgentComposerStep, VIEW_CHOICES,
};

/// Label-column pad for the Details-step form fields.
const COMPOSER_LABEL_PAD: usize = 12;

pub(super) fn render_agent_composer(f: &mut Frame, area: Rect, state: &AgentComposerState) {
    let verb = if state.is_builtin_subagent() {
        "Edit Built-in Subagent"
    } else if state.edit_target.is_editing() {
        "Edit Agent"
    } else {
        "New Agent"
    };
    let title = format!("{verb} - {}", state.step.title());
    let panel = theme::frame(&title, true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(6),
            Constraint::Length(3),
        ])
        .split(inner);

    render_header(f, chunks[0], state);
    match state.step {
        AgentComposerStep::Details => render_details(f, chunks[1], state),
        AgentComposerStep::Tools => render_tools(f, chunks[1], state),
        AgentComposerStep::Prompt => render_prompt(f, chunks[1], state),
        AgentComposerStep::Review => render_review(f, chunks[1], state),
    }
    render_footer(f, chunks[2], state);
}

fn render_header(f: &mut Frame, area: Rect, state: &AgentComposerState) {
    let slug = state.slug();
    let mode = if state.is_builtin_subagent() {
        "Settings"
    } else if state.edit_target.is_editing() {
        "Edit mode"
    } else {
        "Create mode"
    };
    let destination = if state.is_builtin_subagent() {
        "Bonsai database (built-in identity preserved)".to_string()
    } else {
        state
            .edit_target
            .source_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| match state.location {
                crate::tui::agent_composer::AgentLocation::Global => {
                    format!("~/.bonsai/agents/{slug}.md")
                }
                crate::tui::agent_composer::AgentLocation::Project => {
                    format!(".bonsai/agents/{slug}.md")
                }
            })
    };
    let spans = vec![
        Span::styled(format!("{mode}  "), theme::body(theme::palette().text)),
        Span::styled("Destination ", theme::muted()),
        Span::styled(destination, theme::dim()),
    ];
    let mut lines = vec![Line::from(spans)];
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

fn render_details(f: &mut Frame, area: Rect, state: &AgentComposerState) {
    let text_style = theme::body(theme::palette().text);
    let model = state
        .selected_model()
        .map(|model| {
            format!(
                "{model}  [effort: {}] — used when delegated",
                state.selected_effort().unwrap_or("default")
            )
        })
        .unwrap_or_else(|| {
            "Parent default  [effort: inherited] — delegated runs inherit the parent".to_string()
        });
    let fallback_model = state
        .selected_fallback_model()
        .map(|model| {
            format!(
                "{model}  [effort: {}] — used if primary fails",
                state.selected_fallback_effort().unwrap_or("default")
            )
        })
        .unwrap_or_else(|| "No backup — failover disabled".to_string());
    let view = VIEW_CHOICES
        .get(state.view_index)
        .copied()
        .unwrap_or("default");
    // Preview the persona color by rendering the value in that color.
    let color_value = state.selected_color().unwrap_or("default").to_string();
    let color_style = state
        .selected_color()
        .and_then(theme::persona_color)
        .map(theme::body)
        .unwrap_or(text_style);

    let definition_kind = state.definition_kind();
    // (group, label, value, field, editable-text, value_style, caret grapheme index)
    let fields: Vec<(&str, &str, String, AgentComposerField, bool, Style, usize)> =
        if state.is_builtin_subagent() {
            vec![
                (
                    "Identity",
                    "Built-in",
                    state.display_name().to_string(),
                    AgentComposerField::Name,
                    false,
                    text_style,
                    0,
                ),
                (
                    "Model chain",
                    "Primary model",
                    model,
                    AgentComposerField::Model,
                    false,
                    text_style,
                    0,
                ),
                (
                    "",
                    "Backup model",
                    fallback_model,
                    AgentComposerField::BackupModel,
                    false,
                    text_style,
                    0,
                ),
            ]
        } else {
            vec![
                (
                    "Identity",
                    "Name",
                    state.name.text.clone(),
                    AgentComposerField::Name,
                    true,
                    text_style,
                    state.name.cursor,
                ),
                (
                    "",
                    "Description",
                    state.description.text.clone(),
                    AgentComposerField::Description,
                    true,
                    text_style,
                    state.description.cursor,
                ),
                (
                    "Availability",
                    "Type",
                    format!("{} — {}", definition_kind.label(), definition_kind.detail()),
                    AgentComposerField::DefinitionKind,
                    false,
                    text_style,
                    0,
                ),
                (
                    "",
                    "Location",
                    state.location.label().to_string(),
                    AgentComposerField::Location,
                    false,
                    text_style,
                    0,
                ),
                (
                    "Model chain",
                    "Primary model",
                    model,
                    AgentComposerField::Model,
                    false,
                    text_style,
                    0,
                ),
                (
                    "",
                    "Backup model",
                    fallback_model,
                    AgentComposerField::BackupModel,
                    false,
                    text_style,
                    0,
                ),
                (
                    "Presentation",
                    "Color",
                    color_value,
                    AgentComposerField::Color,
                    false,
                    color_style,
                    0,
                ),
                (
                    "",
                    "View",
                    view.to_string(),
                    AgentComposerField::View,
                    false,
                    text_style,
                    0,
                ),
            ]
        };

    let mut lines = Vec::with_capacity(fields.len() + 4);
    let mut cursor = None;
    for (group, label, value, field, editable, value_style, caret) in &fields {
        if !group.is_empty() {
            lines.push(Line::from(Span::styled(*group, theme::muted())));
        }
        let row = lines.len();
        let form_field = FormField {
            label,
            value: value.as_str(),
            active: state.field == *field,
            caret: if *editable {
                FieldCaret::At(*caret)
            } else {
                FieldCaret::None
            },
            value_style: *value_style,
        };
        let (mut field_lines, field_cursor) =
            form_field_lines(&[form_field], COMPOSER_LABEL_PAD, area.width);
        lines.append(&mut field_lines);
        if let Some((cursor_row, cursor_col)) = field_cursor {
            cursor = Some((row as u16 + cursor_row, cursor_col));
        }
    }

    f.render_widget(Paragraph::new(lines).style(theme::panel()), area);
    if let Some((row, col)) = cursor {
        f.set_cursor_position(clamp_cursor(area, area.x + col, area.y + row));
    }
}

fn render_tools(f: &mut Frame, area: Rect, state: &AgentComposerState) {
    let mut lines = vec![Line::from(Span::styled(
        "Space toggles. None selected = read-only default. write/edit/bash prompt for approval.",
        theme::dim(),
    ))];
    for (idx, tool) in state.tools.iter().enumerate() {
        let checkbox = if tool.selected { "[x] " } else { "[ ] " };
        lines.push(picker_line(
            idx == state.tools_cursor,
            format!("{checkbox}{}", tool.name),
            None,
            true,
        ));
    }
    // A `view: canvas` persona also gets the plan-canvas tools, granted by the
    // view (not scoped here), so the agent can build/edit the plan it renders.
    if state.selected_view() == Some("canvas") {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "+ canvas plan tools (plan_*) — granted automatically with view: canvas",
            theme::body(theme::palette().plan_accent),
        )));
    }
    f.render_widget(Paragraph::new(lines).style(theme::panel()), area);
}

fn render_prompt(f: &mut Frame, area: Rect, state: &AgentComposerState) {
    use crate::tui::widgets::input::{composer_cursor_row_col, composer_visual_rows};
    use unicode_segmentation::UnicodeSegmentation;

    let text = &state.prompt.text;
    if text.is_empty() {
        let hint = Line::from(Span::styled(
            "Type or paste a persona prompt, or press Ctrl+G to extend the description.",
            theme::dim(),
        ));
        f.render_widget(Paragraph::new(vec![hint]).style(theme::panel()), area);
        if !state.generating {
            f.set_cursor_position(clamp_cursor(area, area.x, area.y));
        }
        return;
    }

    let width = area.width.max(1) as usize;
    let body_height = area.height.max(1) as usize;
    let caret = state.prompt.cursor;
    let rows = composer_visual_rows(text, caret, width);
    let (cursor_row, cursor_col) = composer_cursor_row_col(text, caret, width);

    // Scroll so the caret's visual row stays on screen.
    let start = (cursor_row + 1)
        .saturating_sub(body_height)
        .min(rows.len().saturating_sub(body_height));
    let end = (start + body_height).min(rows.len());

    let graphemes: Vec<&str> = text.graphemes(true).collect();
    let text_style = theme::body(theme::palette().text);
    let lines: Vec<Line<'static>> = rows[start..end]
        .iter()
        .map(|row| {
            Line::from(Span::styled(
                graphemes[row.start..row.end].concat(),
                text_style,
            ))
        })
        .collect();

    f.render_widget(Paragraph::new(lines).style(theme::panel()), area);

    if !state.generating {
        let row = cursor_row.saturating_sub(start) as u16;
        f.set_cursor_position(clamp_cursor(area, area.x + cursor_col as u16, area.y + row));
    }
}

fn render_review(f: &mut Frame, area: Rect, state: &AgentComposerState) {
    let model = state.selected_model().unwrap_or("(parent default)");
    let effort = state.selected_effort().unwrap_or("default");
    let fallback_model = state.selected_fallback_model().unwrap_or("none");
    let fallback_effort = state.selected_fallback_effort().unwrap_or("default");
    let model_chain =
        format!("{model} [effort: {effort}] → {fallback_model} [effort: {fallback_effort}]");
    if state.is_builtin_subagent() {
        let lines = vec![
            review_line("Built-in subagent", state.display_name()),
            review_line("Storage", "Bonsai database"),
            review_line("Model chain", &model_chain),
            Line::from(""),
            Line::from(Span::styled(
                "Compiled prompt, tools, and run limits stay unchanged.",
                theme::dim(),
            )),
        ];
        f.render_widget(
            Paragraph::new(lines)
                .style(theme::panel())
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let color = state.selected_color().unwrap_or("default");
    let view = state.selected_view().unwrap_or("default");
    let definition_kind = state.definition_kind();
    let tools = match state.selected_tools() {
        Some(tools) => tools.join(", "),
        None => "read-only default".to_string(),
    };
    let mut lines = vec![
        review_line("Name", state.display_name()),
        review_line("File", &format!("{}.md", state.slug())),
        review_line("Description", state.description.text.trim()),
        review_line(
            "Type",
            &format!("{} — {}", definition_kind.label(), definition_kind.detail()),
        ),
        review_line("Location", state.location.label()),
        review_line("Model chain", &model_chain),
        review_line("Color", color),
        review_line("View", view),
        review_line("Tools", &tools),
        Line::from(""),
        Line::from(Span::styled("Prompt", theme::muted())),
    ];
    // Preview only the first few prompt lines on the review screen.
    const COMPOSER_PROMPT_PREVIEW_LINES: usize = 8;
    let prompt_lines = state.prompt.text.trim().split('\n').collect::<Vec<_>>();
    for line in prompt_lines.iter().take(COMPOSER_PROMPT_PREVIEW_LINES) {
        lines.push(Line::from(Span::styled(
            (*line).to_string(),
            theme::body(theme::palette().text),
        )));
    }
    if prompt_lines.len() > COMPOSER_PROMPT_PREVIEW_LINES {
        lines.push(Line::from(Span::styled(
            format!(
                "… prompt preview truncated ({} more lines)",
                prompt_lines.len() - COMPOSER_PROMPT_PREVIEW_LINES
            ),
            theme::dim(),
        )));
    }
    f.render_widget(
        Paragraph::new(lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn review_line(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), theme::muted()),
        Span::styled(value.to_string(), theme::body(theme::palette().text)),
    ])
}

fn render_footer(f: &mut Frame, area: Rect, state: &AgentComposerState) {
    let progress = if state.is_builtin_subagent() {
        match state.step {
            AgentComposerStep::Details => "Step 1/2 · Model chain — durable built-in assignment",
            AgentComposerStep::Review => "Step 2/2 · Review — compiled identity stays built-in",
            AgentComposerStep::Tools | AgentComposerStep::Prompt => "Built-in subagent settings",
        }
    } else {
        match state.step {
            AgentComposerStep::Details => {
                "Step 1/4 · Details — identity, invocation type, model chain, and presentation"
            }
            AgentComposerStep::Tools => "Step 2/4 · Tools — capabilities this agent may invoke",
            AgentComposerStep::Prompt => "Step 3/4 · Prompt — instructions sent to the agent",
            AgentComposerStep::Review => {
                "Step 4/4 · Review — confirm defaults and fallback order before saving"
            }
        }
    };
    let controls = if state.is_builtin_subagent() {
        match state.step {
            AgentComposerStep::Details => {
                footer_hint_line(&[("Enter", "choose"), ("d", "delete"), ("Tab", "review")])
            }
            AgentComposerStep::Review => {
                footer_hint_line(&[("Enter", "save"), ("Shift+Tab", "back"), ("Esc", "cancel")])
            }
            AgentComposerStep::Tools | AgentComposerStep::Prompt => Line::default(),
        }
    } else {
        match state.step {
            AgentComposerStep::Details => match state.field {
                AgentComposerField::Name | AgentComposerField::Description => {
                    footer_hint_line(&[("Left/Right", "edit"), ("Tab", "tools")])
                }
                AgentComposerField::Model | AgentComposerField::BackupModel => {
                    footer_hint_line(&[("Enter", "choose"), ("d", "delete"), ("Tab", "tools")])
                }
                AgentComposerField::Location
                | AgentComposerField::DefinitionKind
                | AgentComposerField::Color
                | AgentComposerField::View => {
                    footer_hint_line(&[("Left/Right", "cycle"), ("Tab", "tools")])
                }
            },
            AgentComposerStep::Tools => {
                footer_hint_line(&[("Up/Down", "move"), ("Space", "toggle"), ("Tab", "prompt")])
            }
            AgentComposerStep::Prompt => footer_hint_line(&[
                ("Enter", "newline"),
                ("Ctrl+G", "extend description"),
                ("Tab", "review"),
            ]),
            AgentComposerStep::Review => {
                footer_hint_line(&[("Enter", "save"), ("Shift+Tab", "back"), ("Esc", "cancel")])
            }
        }
    };
    let help = if state.is_builtin_subagent() {
        match state.field {
            AgentComposerField::Model => {
                "Primary inherits the parent by default; choosing a model persists the assignment."
            }
            AgentComposerField::BackupModel => {
                "Backup is optional and runs only when the primary model fails."
            }
            _ => "Built-in name, prompt, tools, and limits are compiled and immutable.",
        }
    } else {
        match state.step {
            AgentComposerStep::Details => focused_field_help(state.field),
            AgentComposerStep::Tools => {
                "No selected tools means the read-only default; write-capable tools remain approval-gated."
            }
            AgentComposerStep::Prompt => {
                "Describe the role, constraints, workflow, and expected reporting format."
            }
            AgentComposerStep::Review => {
                "Defaults are omitted from frontmatter; the displayed model chain is tried left to right."
            }
        }
    };
    let mut lines = vec![
        Line::from(Span::styled(progress, theme::muted())),
        Line::from(Span::styled(help, theme::dim())),
    ];
    if state.generating {
        lines.push(Line::from(Span::styled("Generating…", theme::dim())));
    } else {
        lines.push(controls);
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn focused_field_help(field: AgentComposerField) -> &'static str {
    match field {
        AgentComposerField::Name => "Name becomes the agent label and its slugified filename.",
        AgentComposerField::Description => {
            "Description explains when this definition should be selected or delegated work."
        }
        AgentComposerField::DefinitionKind => {
            "Agent is user-facing; Subagent is delegated; Both supports either entry point."
        }
        AgentComposerField::Location => {
            "Global agents are available everywhere; project agents stay in this repository."
        }
        AgentComposerField::Model => {
            "This assignment applies when delegated; Shift+Tab agents use the active session model."
        }
        AgentComposerField::BackupModel => {
            "Delegated-run backup is optional and runs only when its primary model fails."
        }
        AgentComposerField::Color => {
            "Color applies on the user-facing agent surface; subagent-only definitions ignore it."
        }
        AgentComposerField::View => {
            "View applies on the user-facing agent surface; subagent-only definitions ignore it."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn composer_details_renders_fields_and_footer() {
        let area = Rect::new(0, 0, 110, 24);
        let mut state = AgentComposerState::new();
        state.name.text = "reviewer".to_string();
        state.description.text = "Reviews diffs".to_string();
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        terminal
            .draw(|frame| render_agent_composer(frame, area, &state))
            .expect("composer should render");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("New Agent - Details"));
        assert!(text.contains("reviewer"));
        assert!(text.contains("Reviews diffs"));
        assert!(text.contains("Identity"));
        assert!(text.contains("Availability"));
        assert!(text.contains("subagent"));
        assert!(text.contains("delegated by an agent"));
        assert!(text.contains("Model chain"));
        assert!(text.contains("Presentation"));
        assert!(text.contains("Create mode"));
        assert!(text.contains("Step 1/4"));
        assert!(!text.contains("Enter choose"));
    }

    #[test]
    fn composer_model_field_renders_contextual_help_and_effort() {
        let area = Rect::new(0, 0, 110, 24);
        let mut state = AgentComposerState::new();
        state.field = AgentComposerField::Model;
        state.model.set_text("codex:gpt-5".to_string());
        state.effort_index = 4;
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");

        terminal
            .draw(|frame| render_agent_composer(frame, area, &state))
            .expect("composer should render");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("[effort: high]"));
        assert!(text.contains("This assignment applies when delegated"));
        assert!(text.contains("Enter choose"));
    }

    #[test]
    fn composer_review_marks_truncated_prompt_preview() {
        let area = Rect::new(0, 0, 110, 28);
        let mut state = AgentComposerState::new();
        state.step = AgentComposerStep::Review;
        state.prompt.set_text(
            (1..=10)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");

        terminal
            .draw(|frame| render_agent_composer(frame, area, &state))
            .expect("composer should render");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(text.contains("prompt preview truncated (2 more lines)"));
        assert!(text.contains("Step 4/4"));
    }
}
