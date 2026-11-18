use super::common::{ModalCaches, ModalLines, ModalScrollView};
use super::picker_common::*;
use super::*;

/// One row of the `/help` board: the label column and its description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HelpRow {
    pub(super) label: String,
    pub(super) description: String,
}

/// Built-in commands followed by the project's custom commands and any
/// command files that were skipped (with the reason), so a clash with a
/// built-in is visible where users look for commands.
pub(super) fn command_help_rows(
    custom: &crate::resource::command::CommandRegistry,
) -> Vec<HelpRow> {
    let mut rows: Vec<HelpRow> = crate::commands::COMMANDS
        .iter()
        .map(|command| HelpRow {
            label: command_help_label(command),
            description: command.description.to_string(),
        })
        .collect();
    rows.extend(custom.iter().map(|def| HelpRow {
        label: match &def.argument_hint {
            Some(hint) => format!("{} {hint}", def.slash_name()),
            None => def.slash_name(),
        },
        description: format!("{} (custom)", def.description),
    }));
    rows.extend(custom.diagnostics().iter().map(|diagnostic| HelpRow {
        label: format!(
            "! {}",
            diagnostic
                .path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
        ),
        description: format!("skipped: {}", diagnostic.message),
    }));
    rows
}

pub(super) fn render_command_help(f: &mut Frame, area: Rect, app: &AppState) {
    let rows = command_help_rows(&app.custom_commands.snapshot());
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
            Paragraph::new(command_help_lines(&rows, chunks[1].width as usize))
                .style(theme::panel()),
            chunks[1],
        );
    } else {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(chunks[1]);
        let split = rows.len().div_ceil(2);
        for (area, commands) in [(columns[0], &rows[..split]), (columns[1], &rows[split..])] {
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

pub(super) fn command_help_lines(commands: &[HelpRow], width: usize) -> Vec<Line<'static>> {
    // Fit the label column to the longest command usage so hints are not cut
    // while the description column has room to spare; cap it so descriptions
    // keep at least ~45% of the pane.
    let longest = commands
        .iter()
        .map(|command| command.label.chars().count())
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
            pad_ascii(&command.label, label_width),
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
            &command.description,
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
