use super::common::*;
use super::picker_common::*;
use super::*;
use crate::tui::skill_manager::{SkillRow, SkillRowKind};

const PANEL_TITLE: &str = "Skills";
const DETAIL_TITLE: &str = "Instructions";

pub(super) fn render_skill_manager(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    rows: &[SkillRow],
    cursor: usize,
) {
    let hint = rows.get(cursor).map(action_hint).unwrap_or_default();
    let footer_lines = vec![
        Line::from(Span::styled(hint, theme::dim())),
        footer_hint_line(&[
            ("Up/Down", "move"),
            ("PgUp/PgDn", "page"),
            ("Space", "enable/disable"),
            ("Enter/l", "load/unload"),
            ("Esc", "close"),
        ]),
    ];
    render_list_detail_modal(
        f,
        area,
        app,
        ListDetailModal {
            title: PANEL_TITLE,
            detail_title: DETAIL_TITLE,
            split: ListDetailSplit::Horizontal,
            detail_focused: true,
            footer_lines,
            modal_scroll: app.modal_scroll,
        },
        |frame, list_area, _detail_area| {
            let list_lines: Vec<Line<'static>> = if rows.is_empty() {
                vec![Line::from(Span::styled("No skills", theme::dim()))]
            } else {
                visible_picker_rows(rows.len(), cursor, picker_body_height(list_area).max(1))
                    .map(|index| {
                        let row = &rows[index];
                        picker_line(
                            index == cursor,
                            row.name.clone(),
                            Some(list_row_tag(row)),
                            row.kind.is_advertised(),
                        )
                    })
                    .collect()
            };
            frame.render_widget(Paragraph::new(list_lines).style(theme::panel()), list_area);
            rows.get(cursor).map(skill_detail_lines).unwrap_or_default()
        },
    );
}

/// The compact tag shown after a skill's name in the list: its status word, plus
/// a `●` when its body is loaded into the conversation.
fn list_row_tag(row: &SkillRow) -> String {
    let mut tag = row.kind.label().to_string();
    if row.loaded {
        tag.push_str(" ●");
    }
    tag
}

/// The per-row footer hint: what actions apply to the selected skill.
fn action_hint(row: &SkillRow) -> String {
    if !row.disablable {
        return format!("{} — managed by its file.", row.source);
    }
    if row.disabled {
        "Disabled. Space to re-enable.".to_string()
    } else if matches!(row.kind, SkillRowKind::Builtin(state) if !state.is_active()) {
        "Inactive here. Space to disable it entirely.".to_string()
    } else {
        "Active. Space to disable.".to_string()
    }
}

fn status_color(kind: SkillRowKind) -> ratatui::style::Color {
    let palette = theme::palette();
    match kind {
        SkillRowKind::Project | SkillRowKind::Global => palette.tool,
        SkillRowKind::Builtin(state) if state.is_active() => palette.success,
        SkillRowKind::Builtin(_) => palette.dim,
    }
}

/// The detail-pane content for a skill: metadata header, then the full body.
fn skill_detail_lines(row: &SkillRow) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(Span::styled(
        row.name.clone(),
        theme::label(theme::palette().text),
    )));
    lines.push(Line::from(vec![
        Span::styled("status: ", theme::dim()),
        Span::styled(
            row.kind.label().to_string(),
            status_style(status_color(row.kind)),
        ),
        Span::styled("   source: ", theme::dim()),
        Span::styled(row.source.clone(), theme::dim()),
    ]));
    lines.push(Line::from(Span::styled(
        if row.loaded {
            "● loaded into this session"
        } else {
            "not loaded this session"
        }
        .to_string(),
        if row.loaded {
            theme::body(theme::palette().success)
        } else {
            theme::dim()
        },
    )));
    if !row.activation_hint.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("activate with: {}", row.activation_hint),
            theme::dim(),
        )));
    }
    lines.push(Line::from(""));
    if row.body.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "(no instructions body)",
            theme::dim(),
        )));
    } else {
        // Logical lines only — the Paragraph wraps at `width` on render, and
        // `wrapped_line_count` accounts for that in the scroll-max math below.
        for body_line in row.body.lines() {
            lines.push(Line::from(Span::styled(
                body_line.to_string(),
                theme::body(theme::palette().text),
            )));
        }
    }
    lines
}

/// Max detail scroll for the selected skill — total wrapped body height minus
/// the visible detail height. Shared with `common::max_modal_scroll`.
pub(super) fn skill_detail_max_scroll(area: Rect, rows: &[SkillRow], cursor: usize) -> u16 {
    let Some(row) = rows.get(cursor.min(rows.len().saturating_sub(1))) else {
        return 0;
    };
    let (_, detail_area, _) = list_detail_regions(area, ListDetailSplit::Horizontal);
    detail_max_scroll(detail_pane_inner(detail_area), &skill_detail_lines(row))
}
