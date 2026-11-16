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
        footer_hint_line(AGENT_BROWSER_HINTS),
    ];
    render_list_picker(f, area, "Agents & Subagents", &[], &footer, |body_area| {
        if rows.is_empty() {
            return modal_empty_body("No agent or subagent definitions are installed.");
        }
        let lines = rows
            .iter()
            .enumerate()
            .map(|(idx, row)| {
                let mut tag = format!("({} · {}", row.origin.label(), row.definition_kind.label());
                if let Some(model) = &row.model {
                    tag.push_str(" · ");
                    tag.push_str(crate::tui::pickers::short_model_label(model));
                }
                tag.push(')');
                if !row.enabled {
                    tag.push_str(" disabled");
                }
                row_line(idx == cursor, row.name.clone(), Some(tag), row.enabled)
            })
            .collect::<Vec<_>>();
        let heights = lines
            .iter()
            .map(|line| row_line_height(line, body_area.width, 0))
            .collect::<Vec<_>>();
        visible_rows(body_area, &heights, cursor)
            .map(|idx| lines[idx].clone())
            .collect()
    });
}

/// The agent-delete confirm's header, body and footer.
fn agent_delete_layout(
    name: &str,
    path: &std::path::Path,
) -> (Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>) {
    let body = vec![
        Line::from(vec![
            Span::styled("Delete ", theme::body(theme::palette().text)),
            Span::styled(name.to_string(), theme::body(theme::palette().error)),
            Span::styled("?", theme::body(theme::palette().text)),
        ]),
        Line::from(Span::styled(path.display().to_string(), theme::dim())),
    ];
    (Vec::new(), body, vec![confirm_footer_hint()])
}

pub(super) fn agent_delete_confirm_rows(area: Rect, name: &str, path: &std::path::Path) -> u16 {
    let (header, body, footer) = agent_delete_layout(name, path);
    decision_rows(&header, &body, &footer, decision_inner_width(area))
}

pub(super) fn render_agent_delete_confirm(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    name: &str,
    path: &std::path::Path,
) {
    render_decision_popup(
        f,
        area,
        app,
        "Delete Agent",
        agent_delete_layout(name, path),
    );
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
        assert!(text.contains("Space/t enable/disable"));
    }
}
