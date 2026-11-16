use super::common::{ModalCaches, ModalLines, ModalScrollView};
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

    // Two command columns need room for a legible description beside each label;
    // below this width the board reads better as one wider column.
    const TWO_COLUMN_MIN_WIDTH: u16 = 110;
    if chunks[1].width < TWO_COLUMN_MIN_WIDTH {
        f.render_widget(
            Paragraph::new(command_help_lines(
                crate::commands::COMMANDS,
                chunks[1].width as usize,
            ))
            .style(theme::panel()),
            chunks[1],
        );
    } else {
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
                Paragraph::new(command_help_lines(commands, area.width as usize))
                    .style(theme::panel()),
                area,
            );
        }
    }

    f.render_widget(
        Paragraph::new(vec![
            footer_hint_line(COMMAND_HELP_HINTS),
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
    // Fit the label column to the longest command usage so hints are not cut
    // while the description column has room to spare; cap it so descriptions
    // keep at least ~45% of the pane.
    let longest = commands
        .iter()
        .map(|command| command_help_label(command).chars().count())
        .max()
        .unwrap_or(10);
    let label_cap = width.saturating_mul(55).saturating_div(100).max(10);
    let label_width = (longest + 1).clamp(10, label_cap.min(40));
    let gap_width = usize::from(width > label_width);
    let detail_width = width.saturating_sub(label_width + gap_width);
    let mut lines = Vec::with_capacity(commands.len() + 1);
    lines.push(Line::from(vec![
        Span::styled(pad_ascii("Command", label_width), theme::muted()),
        Span::raw(" ".repeat(gap_width)),
        // Header label (metadata): clipping it is intentional — the column's own
        // cells carry the detail.
        Span::styled(truncate_ascii("Action", detail_width), theme::muted()),
    ]));
    for command in commands {
        let label = Span::styled(
            pad_ascii(&command_help_label(command), label_width),
            theme::body(theme::palette().text),
        );
        // A zero-width detail column (very narrow pane) keeps the single-line row:
        // wrapping into zero columns would emit one grapheme per row and bleed into
        // the neighbouring command column.
        if detail_width == 0 {
            lines.push(Line::from(label));
            continue;
        }
        let mut wrapped = wrapped_cell(
            command.description,
            detail_width,
            label_width + gap_width,
            theme::dim(),
        )
        .into_iter();
        let mut first = vec![label, Span::raw(" ".repeat(gap_width))];
        if let Some(row) = wrapped.next() {
            first.extend(row.spans);
        }
        lines.push(Line::from(first));
        lines.extend(wrapped);
    }
    lines
}

pub(super) fn command_help_label(command: &crate::commands::CommandMetadata) -> String {
    match command.usage_hint {
        Some(hint) => format!("{} {}", command.name, hint),
        None => command.name.to_string(),
    }
}

/// Keyboard reference, grouped and aligned as one key/action table.
const KEY_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Run",
        &[
            ("Enter", "send; queue while a run is active"),
            ("Esc", "steer the queued message"),
            ("Ctrl+C", "cancel run; again to exit"),
            ("Shift+Tab", "switch agent"),
            ("Tab", "complete / cycle focus"),
        ],
    ),
    (
        "Editing",
        &[
            ("Alt+Enter", "newline (also Shift+Enter)"),
            ("Ctrl+P / Ctrl+N", "input history"),
            ("Ctrl+B / Ctrl+F", "move cursor"),
            ("Ctrl+E", "end of line"),
            ("Ctrl+D", "delete character"),
            ("Ctrl+A", "select all"),
            ("Shift+←/→", "extend selection"),
        ],
    ),
    (
        "Navigation",
        &[
            ("↑ / ↓", "move or scroll where available"),
            ("PgUp / PgDn", "page detailed views"),
            ("Click outside", "dismiss with that modal's Esc action"),
            ("Click / Drag", "select; double-click copies a word"),
            (
                "Ctrl+G",
                "copy mode: native terminal selection (or /select)",
            ),
        ],
    ),
    (
        "Commands",
        &[
            ("/help", "command board"),
            ("/keys", "this reference"),
            ("/model", "model picker"),
            ("/mode", "runtime posture (autonomy, sandbox)"),
            ("/copy", "copy the selected row"),
            ("/save  /export", "save or export the plan"),
            ("/start", "implement the plan"),
        ],
    ),
];

pub(super) fn render_help(f: &mut Frame, area: Rect, app: &AppState) {
    let key_width = KEY_GROUPS
        .iter()
        .flat_map(|(_, rows)| rows.iter())
        .map(|(key, _)| key.chars().count())
        .max()
        .unwrap_or(10)
        + 2;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, (group, rows)) in KEY_GROUPS.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            (*group).to_string(),
            theme::label(theme::palette().tool),
        )));
        for (key, action) in *rows {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {}", pad_ascii(key, key_width)),
                    theme::body(theme::palette().text),
                ),
                Span::styled((*action).to_string(), theme::dim()),
            ]));
        }
    }

    // Render through the shared scrollable-modal path so the shortcut text is
    // mouse-selectable like every other reading surface (and the scroll is
    // clamped to the content).
    render_scrollable_modal(
        f,
        area,
        "Shortcuts",
        ModalLines {
            header: &[],
            body: &lines,
            footer: &[footer_hint_line(HELP_KEYS_HINTS)],
        },
        ModalScrollView {
            scroll: app.modal_ui.modal_scroll,
            selection: app.modal_ui.modal_selection.map(|s| s.range()),
        },
        ModalCaches {
            body: &app.modal_ui.modal_body_lines,
            body_rect: &app.modal_ui.modal_body_rect,
        },
    );
}
