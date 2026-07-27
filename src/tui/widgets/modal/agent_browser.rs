use super::picker_common::*;
use super::*;
use crate::tui::agent_composer::AgentBrowserRow;

pub(super) fn render_agent_browser(
    f: &mut Frame,
    area: Rect,
    rows: &[AgentBrowserRow],
    cursor: usize,
) {
    let description = rows
        .get(cursor)
        .map(|row| row.description.clone())
        .unwrap_or_default();
    let footer = vec![
        Line::from(Span::styled(description, theme::dim())),
        footer_hint_line(&[
            ("Up/Down", "move"),
            ("Enter/e", "edit"),
            ("Space/t", "enable/disable"),
            ("a", "add"),
            ("d/Delete", "delete"),
            ("Esc", "close"),
        ]),
    ];
    render_list_picker(f, area, "Agents & Subagents", &[], &footer, |body_area| {
        if rows.is_empty() {
            return vec![Line::from(Span::styled("No definitions", theme::dim()))];
        }
        visible_picker_rows(rows.len(), cursor, picker_body_height(body_area).max(1))
            .map(|idx| {
                let row = &rows[idx];
                let mut tag = format!("({} · {}", row.origin.label(), row.definition_kind.label());
                if let Some(model) = &row.model {
                    tag.push_str(" · ");
                    tag.push_str(crate::tui::pickers::short_model_label(model));
                }
                tag.push(')');
                if !row.enabled {
                    tag.push_str(" disabled");
                }
                picker_line(idx == cursor, row.name.clone(), Some(tag), row.enabled)
            })
            .collect()
    });
}

pub(super) fn render_agent_delete_confirm(
    f: &mut Frame,
    area: Rect,
    name: &str,
    path: &std::path::Path,
) {
    let panel = theme::frame("Delete Agent", true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);

    let lines = vec![
        Line::from(vec![
            Span::styled("Delete ", theme::body(theme::palette().text)),
            Span::styled(name.to_string(), theme::body(theme::palette().error)),
            Span::styled("?", theme::body(theme::palette().text)),
        ]),
        Line::from(Span::styled(path.display().to_string(), theme::dim())),
        Line::from(""),
        footer_hint_line(&[("Enter/Y", "confirm"), ("Esc/N", "cancel")]),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn browser_renders_selected_agent_and_actions() {
        let area = Rect::new(0, 0, 80, 12);
        let rows = vec![AgentBrowserRow {
            name: "reviewer".to_string(),
            description: "Reviews the current diff".to_string(),
            origin: crate::tui::agent_composer::AgentOrigin::Project,
            definition_kind: crate::tui::agent_composer::AgentDefinitionKind::Subagent,
            builtin_id: None,
            enabled: true,
            model: None,
            source_path: None,
        }];
        let mut terminal = Terminal::new(TestBackend::new(area.width, area.height))
            .expect("test backend should initialize");
        terminal
            .draw(|frame| render_agent_browser(frame, area, &rows, 0))
            .expect("agent browser should render");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("> reviewer"));
        assert!(text.contains("project · subagent"));
        assert!(text.contains("Reviews the current diff"));
        assert!(text.contains("Space enable/disable"));
    }
}
