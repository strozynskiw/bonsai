use super::picker_common::*;
use super::*;

pub(super) fn render_command_help(f: &mut Frame, area: Rect) {
    let panel = theme::frame("Commands", true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Slash commands", theme::label(theme::palette().tool)),
            Span::styled("  /keys for keyboard shortcuts", theme::dim()),
        ])),
        chunks[0],
    );

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);
    let split = crate::commands::COMMANDS.len().div_ceil(2);
    for (area, commands) in [
        (columns[0], &crate::commands::COMMANDS[..split]),
        (columns[1], &crate::commands::COMMANDS[split..]),
    ] {
        f.render_widget(
            Paragraph::new(command_help_lines(commands, area.width as usize)).style(theme::panel()),
            area,
        );
    }

    f.render_widget(
        Paragraph::new(vec![
            footer_hint_line(&[("Tab", "complete"), ("Enter", "run"), ("Esc", "close")]),
            Line::from(Span::styled(
                "Type / to filter commands inline.",
                theme::dim(),
            )),
        ]),
        chunks[2],
    );
}

pub(super) fn command_help_lines(
    commands: &[crate::commands::CommandMetadata],
    width: usize,
) -> Vec<Line<'static>> {
    let label_width = width.saturating_mul(42).saturating_div(100).clamp(10, 22);
    let gap_width = usize::from(width > label_width);
    let detail_width = width.saturating_sub(label_width + gap_width);
    let mut lines = Vec::with_capacity(commands.len() + 1);
    lines.push(Line::from(vec![
        Span::styled(pad_ascii("Command", label_width), theme::muted()),
        Span::raw(" ".repeat(gap_width)),
        Span::styled(truncate_ascii("Action", detail_width), theme::muted()),
    ]));
    for command in commands {
        lines.push(Line::from(vec![
            Span::styled(
                pad_ascii(&command_help_label(command), label_width),
                theme::body(theme::palette().text),
            ),
            Span::raw(" ".repeat(gap_width)),
            Span::styled(
                truncate_ascii(command.description, detail_width),
                theme::dim(),
            ),
        ]));
    }
    lines
}

pub(super) fn command_help_label(command: &crate::commands::CommandMetadata) -> String {
    match command.usage_hint {
        Some(hint) => format!("{} {}", command.name, hint),
        None => command.name.to_string(),
    }
}

pub(super) fn render_help(f: &mut Frame, area: Rect, app: &AppState) {
    let lines = vec![
        Line::from(Span::styled("Keys", theme::label(theme::palette().tool))),
        Line::from(""),
        Line::from(vec![
            Span::styled("Enter", theme::body(theme::palette().text)),
            Span::styled("    send / queue while running  ", theme::dim()),
            Span::styled("Ctrl+P/N", theme::body(theme::palette().text)),
            Span::styled("  history", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("Tab", theme::body(theme::palette().text)),
            Span::styled(" complete / cycle focus  ", theme::dim()),
            Span::styled("Alt+Enter", theme::body(theme::palette().text)),
            Span::styled(" newline", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("Shift+Tab", theme::body(theme::palette().text)),
            Span::styled(" switch agent  ", theme::dim()),
            Span::styled("Shift+Enter", theme::body(theme::palette().text)),
            Span::styled(" newline", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("/save", theme::body(theme::palette().text)),
            Span::styled("    save plan  ", theme::dim()),
            Span::styled("/start", theme::body(theme::palette().text)),
            Span::styled(" implement plan", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("/export", theme::body(theme::palette().text)),
            Span::styled("  export plan  ", theme::dim()),
            Span::styled("/mode", theme::body(theme::palette().text)),
            Span::styled(" runtime posture", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("Esc", theme::body(theme::palette().text)),
            Span::styled("       stop + steer foreground  ", theme::dim()),
            Span::styled("Ctrl+C", theme::body(theme::palette().text)),
            Span::styled(" cancel all / exit", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("↑/↓", theme::body(theme::palette().text)),
            Span::styled("      move or scroll where available", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("PgUp/PgDn", theme::body(theme::palette().text)),
            Span::styled(" page detailed views where available", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("Click outside", theme::body(theme::palette().text)),
            Span::styled(" dismisses using that modal's Esc action", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("/keys", theme::body(theme::palette().text)),
            Span::styled("   key reference  ", theme::dim()),
            Span::styled("/help", theme::body(theme::palette().text)),
            Span::styled(" commands", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("Ctrl+A", theme::body(theme::palette().text)),
            Span::styled("     select all  ", theme::dim()),
            Span::styled("/copy", theme::body(theme::palette().text)),
            Span::styled(" copy row", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("Shift+←/→", theme::body(theme::palette().text)),
            Span::styled(" extend  ", theme::dim()),
            Span::styled("Click/Drag", theme::body(theme::palette().text)),
            Span::styled(" select", theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("Ctrl+G", theme::body(theme::palette().text)),
            Span::styled(
                "     copy mode: native terminal selection (toggle · or /select)",
                theme::dim(),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("/help", theme::body(theme::palette().text)),
            Span::styled("    command board  ", theme::dim()),
            Span::styled("/model", theme::body(theme::palette().text)),
            Span::styled(" model picker", theme::dim()),
        ]),
    ];

    // Render through the shared scrollable-modal path so the shortcut text is
    // mouse-selectable like every other reading surface (and the scroll is
    // clamped to the content).
    render_scrollable_modal(
        f,
        area,
        "Shortcuts",
        &[],
        &lines,
        &[footer_hint_line(&[
            ("d-click", "copy word"),
            ("Esc", "close"),
        ])],
        app.modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
}
