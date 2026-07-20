use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Wrap};

use crate::memory::entry::MemoryEntry;
use crate::tui::app::AppState;
use crate::tui::memory_manager::{MemoryAddWizardState, MemoryWizardField, MemoryWizardStep};
use crate::tui::theme;

use super::common::footer_hint_line;
use super::list_detail::render_detail_pane;
use super::list_detail::{ListDetailSplit, list_detail_regions};
use super::picker_common::{
    FieldCaret, FormField, form_field_lines, picker_body_height, visible_picker_rows,
};
use super::table::{
    CellRender, CellStyle, TableColumn, TableWidth, table_header, table_row, table_widths,
};

const MANAGER_TITLE: &str = "Memory";

fn wizard_title(state: &MemoryAddWizardState) -> &'static str {
    if state.editing.is_some() {
        "Edit Memory"
    } else {
        "Add Memory"
    }
}

/// Review/preview label for the file name: in edit mode the name is the fixed
/// original identity, not a slug derived from the (editable) description.
fn name_label(state: &MemoryAddWizardState) -> &'static str {
    if state.editing.is_some() {
        "name: "
    } else {
        "slug: "
    }
}

pub(super) fn render_memory_manager(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    rows: &[MemoryEntry],
    cursor: usize,
) {
    let panel = theme::frame(MANAGER_TITLE, true).style(theme::panel());
    f.render_widget(Clear, area);
    f.render_widget(panel, area);

    let (list_area, detail_area, footer_area) =
        list_detail_regions(area, ListDetailSplit::Horizontal);
    if list_area.height == 0 || detail_area.height == 0 {
        return;
    }

    let cursor = cursor.min(rows.len().saturating_sub(1));
    let header_lines = manager_header_lines(rows);
    let list_lines =
        manager_list_lines(rows, cursor, list_area.width, picker_body_height(list_area));
    let mut lines = header_lines;
    lines.extend(list_lines);
    f.render_widget(
        Paragraph::new(lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        list_area,
    );

    if let Some(row) = rows.get(cursor) {
        let detail_lines = memory_detail_lines(row);
        render_detail_pane(
            f,
            detail_area,
            "Entry",
            true,
            detail_lines,
            app.modal_scroll,
            app.modal_selection.map(|s| s.range()),
            &app.modal_body_lines,
            &app.modal_body_rect,
        );
    } else {
        super::common::render_modal_notice(f, detail_area, "Entry", "No memory entries saved.");
    }

    f.render_widget(
        Paragraph::new(footer_hint_line(&[
            ("Up/Down", "move"),
            ("Space", "enable/disable"),
            ("a", "add"),
            ("e/Enter", "edit"),
            ("d", "delete"),
            ("Esc", "close"),
        ]))
        .style(theme::panel()),
        footer_area,
    );
}

pub(super) fn render_memory_wizard(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    state: &MemoryAddWizardState,
) {
    let panel = theme::frame(wizard_title(state), true).style(theme::panel());
    f.render_widget(Clear, area);
    let inner = panel.inner(area);
    f.render_widget(panel, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(inner);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(body[0]);
    let left = columns[0];
    let right = columns[1];

    match state.step {
        MemoryWizardStep::Details => render_memory_wizard_details(f, left, state),
        MemoryWizardStep::Body => render_memory_wizard_body(f, left, state),
        MemoryWizardStep::Review => render_memory_wizard_review(f, left, state),
    }

    render_detail_pane(
        f,
        right,
        "Preview",
        true,
        wizard_preview_lines(state),
        app.modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );

    let hints: &[(&str, &str)] = match state.step {
        MemoryWizardStep::Details => &[
            ("Tab", "body"),
            ("Up/Down", "field"),
            ("Space", "cycle"),
            ("Enter", "body"),
            ("Esc", "cancel"),
        ],
        MemoryWizardStep::Body => &[
            ("Tab", "review"),
            ("Enter", "newline"),
            ("Shift+Tab", "details"),
            ("Esc", "cancel"),
        ],
        MemoryWizardStep::Review => &[("Enter", "save"), ("Shift+Tab", "body"), ("Esc", "cancel")],
    };
    f.render_widget(
        Paragraph::new(footer_hint_line(hints)).style(theme::panel()),
        body[1],
    );
}

fn manager_header_lines(rows: &[MemoryEntry]) -> Vec<Line<'static>> {
    let enabled = rows.iter().filter(|row| row.enabled).count();
    let disabled = rows.len().saturating_sub(enabled);
    vec![
        Line::from(vec![
            Span::styled(
                count_label(rows.len(), "entry"),
                theme::body(theme::palette().text),
            ),
            Span::styled("  ", theme::dim()),
            Span::styled(
                format!("{enabled} enabled"),
                theme::body(theme::palette().success),
            ),
            Span::styled("  ", theme::dim()),
            Span::styled(format!("{disabled} disabled"), theme::dim()),
        ]),
        Line::from(""),
    ]
}

fn manager_list_lines(
    rows: &[MemoryEntry],
    cursor: usize,
    width: u16,
    visible_height: usize,
) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return vec![Line::from(Span::styled(
            "No memory entries saved.",
            theme::dim(),
        ))];
    }

    let columns = memory_columns();
    let widths = table_widths(width as usize, rows, &columns);
    let range = visible_picker_rows(rows.len(), cursor, visible_height.max(1));
    let mut lines = Vec::with_capacity(1 + range.len());
    lines.push(table_header(&columns, &widths));
    for idx in range {
        lines.push(table_row(&rows[idx], &columns, &widths, idx == cursor));
    }
    lines
}

fn memory_columns() -> Vec<TableColumn<MemoryEntry>> {
    vec![
        TableColumn {
            header: "Name",
            width: TableWidth::Flex { min: 12 },
            render: CellRender::Value(|entry| entry.name.clone()),
            style: CellStyle::Custom(memory_name_style),
        },
        TableColumn {
            header: "Tier",
            width: TableWidth::Exact(8),
            render: CellRender::Value(|entry| entry.tier.label().to_string()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Type",
            width: TableWidth::Exact(12),
            render: CellRender::Value(|entry| entry.entry_type.label().to_string()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "State",
            width: TableWidth::Exact(10),
            render: CellRender::Value(|entry| memory_state_label(entry.enabled).to_string()),
            style: CellStyle::Custom(memory_state_style),
        },
        TableColumn {
            header: "Updated",
            width: TableWidth::Exact(10),
            render: CellRender::Value(|entry| entry.updated.clone()),
            style: CellStyle::Meta,
        },
    ]
}

fn memory_name_style(entry: &MemoryEntry) -> Style {
    if entry.enabled {
        theme::body(theme::palette().text)
    } else {
        theme::dim()
    }
}

fn memory_state_style(entry: &MemoryEntry) -> Style {
    if entry.enabled {
        theme::body(theme::palette().success).add_modifier(Modifier::BOLD)
    } else {
        theme::dim()
    }
}

fn memory_state_label(enabled: bool) -> &'static str {
    if enabled { "enabled" } else { "disabled" }
}

pub(super) fn memory_detail_lines(entry: &MemoryEntry) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            entry.name.clone(),
            theme::label(theme::palette().text),
        )),
        Line::from(vec![
            Span::styled("tier: ", theme::dim()),
            Span::styled(
                entry.tier.label().to_string(),
                theme::body(theme::palette().text),
            ),
            Span::styled("   type: ", theme::dim()),
            Span::styled(
                entry.entry_type.label().to_string(),
                theme::body(theme::palette().text),
            ),
            Span::styled("   state: ", theme::dim()),
            Span::styled(
                memory_state_label(entry.enabled).to_string(),
                if entry.enabled {
                    theme::body(theme::palette().success)
                } else {
                    theme::dim()
                },
            ),
        ]),
        Line::from(vec![
            Span::styled("path: ", theme::dim()),
            Span::styled(entry.path.display().to_string(), theme::dim()),
        ]),
        Line::from(vec![
            Span::styled("created: ", theme::dim()),
            Span::styled(entry.created.clone(), theme::dim()),
            Span::styled("   updated: ", theme::dim()),
            Span::styled(entry.updated.clone(), theme::dim()),
        ]),
        Line::from(""),
        Line::from(Span::styled("description", theme::muted())),
        Line::from(Span::styled(
            entry.description.clone(),
            theme::body(theme::palette().text),
        )),
        Line::from(""),
        Line::from(Span::styled("body", theme::muted())),
    ];
    if entry.body.trim().is_empty() {
        lines.push(Line::from(Span::styled("(empty body)", theme::dim())));
    } else {
        for line in entry.body.lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                theme::body(theme::palette().text),
            )));
        }
    }
    lines
}

fn render_memory_wizard_details(f: &mut Frame, area: Rect, state: &MemoryAddWizardState) {
    let fields = [
        FormField {
            label: "Tier",
            value: state.tier.label(),
            active: matches!(state.field, MemoryWizardField::Tier),
            caret: FieldCaret::None,
            value_style: theme::body(theme::palette().text),
        },
        FormField {
            label: "Type",
            value: state.entry_type.label(),
            active: matches!(state.field, MemoryWizardField::Type),
            caret: FieldCaret::None,
            value_style: theme::body(theme::palette().text),
        },
        FormField {
            label: "Enabled",
            value: if state.enabled { "true" } else { "false" },
            active: matches!(state.field, MemoryWizardField::Enabled),
            caret: FieldCaret::None,
            value_style: if state.enabled {
                theme::body(theme::palette().success)
            } else {
                theme::dim()
            },
        },
        FormField {
            label: "Description",
            value: &state.description,
            active: matches!(state.field, MemoryWizardField::Description),
            caret: FieldCaret::End,
            value_style: theme::body(theme::palette().text),
        },
    ];
    let (lines, _cursor) = form_field_lines(&fields, 12, area.width);
    f.render_widget(
        Paragraph::new(lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_memory_wizard_body(f: &mut Frame, area: Rect, state: &MemoryAddWizardState) {
    let body_lines = wizard_body_lines(state);
    f.render_widget(
        Paragraph::new(body_lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_memory_wizard_review(f: &mut Frame, area: Rect, state: &MemoryAddWizardState) {
    let lines = wizard_review_lines(state);
    f.render_widget(
        Paragraph::new(lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn wizard_body_lines(state: &MemoryAddWizardState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        "Body",
        theme::label(theme::palette().text),
    )));
    lines.push(Line::from(Span::styled(
        "Enter to insert a newline. Tab moves to review.",
        theme::dim(),
    )));
    lines.push(Line::from(""));
    if state.body.text.trim().is_empty() {
        lines.push(Line::from(Span::styled("(empty body)", theme::dim())));
    } else {
        for line in state.body.text.lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                theme::body(theme::palette().text),
            )));
        }
    }
    lines
}

pub(super) fn wizard_review_lines(state: &MemoryAddWizardState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Review", theme::label(theme::palette().text))),
        Line::from(vec![
            Span::styled("tier: ", theme::dim()),
            Span::styled(
                state.tier.label().to_string(),
                theme::body(theme::palette().text),
            ),
            Span::styled("   type: ", theme::dim()),
            Span::styled(
                state.entry_type.label().to_string(),
                theme::body(theme::palette().text),
            ),
            Span::styled("   state: ", theme::dim()),
            Span::styled(
                if state.enabled { "enabled" } else { "disabled" }.to_string(),
                if state.enabled {
                    theme::body(theme::palette().success)
                } else {
                    theme::dim()
                },
            ),
        ]),
        Line::from(vec![
            Span::styled(name_label(state), theme::dim()),
            Span::styled(
                state.effective_name(),
                theme::body(theme::palette().border_active),
            ),
        ]),
        Line::from(vec![
            Span::styled("path: ", theme::dim()),
            Span::styled(format!("{}.md", state.effective_name()), theme::dim()),
        ]),
        Line::from(""),
        Line::from(Span::styled("Description", theme::muted())),
        Line::from(Span::styled(
            normalize_blank(&state.description),
            theme::body(theme::palette().text),
        )),
        Line::from(""),
        Line::from(Span::styled("Body preview", theme::muted())),
    ];
    if state.body.text.trim().is_empty() {
        lines.push(Line::from(Span::styled("(empty body)", theme::dim())));
    } else {
        for line in state.body.text.lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                theme::body(theme::palette().text),
            )));
        }
    }
    lines
}

pub(super) fn wizard_preview_lines(state: &MemoryAddWizardState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled("Preview", theme::label(theme::palette().text))),
        Line::from(vec![
            Span::styled(name_label(state), theme::dim()),
            Span::styled(
                state.effective_name(),
                theme::body(theme::palette().border_active),
            ),
        ]),
        Line::from(vec![
            Span::styled("state: ", theme::dim()),
            Span::styled(
                if state.enabled { "enabled" } else { "disabled" }.to_string(),
                if state.enabled {
                    theme::body(theme::palette().success)
                } else {
                    theme::dim()
                },
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled("body", theme::muted())),
    ];
    if state.body.text.trim().is_empty() {
        lines.push(Line::from(Span::styled("(empty body)", theme::dim())));
    } else {
        for line in state.body.text.lines() {
            lines.push(Line::from(Span::styled(
                line.to_string(),
                theme::body(theme::palette().text),
            )));
        }
    }
    lines
}

fn count_label(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn normalize_blank(text: &str) -> String {
    text.trim().to_string()
}
