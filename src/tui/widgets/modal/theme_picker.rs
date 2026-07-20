use super::picker_common::*;
use super::*;
use crate::tui::theme::ThemeSource;

/// Provenance tag appended to a theme's picker subtitle. Built-ins carry none;
/// custom themes name the tier they were loaded from so shadowing is visible.
fn theme_source_suffix(source: ThemeSource) -> &'static str {
    match source {
        ThemeSource::Builtin => "",
        ThemeSource::Project => " · project theme",
        ThemeSource::Global => " · global theme",
    }
}

pub(super) fn render_theme_picker(f: &mut Frame, area: Rect, cursor: usize) {
    let footer = vec![
        Line::from(Span::styled("Preview updates while moving.", theme::dim())),
        footer_hint_line(&[("Up/Down", "move"), ("Enter", "save"), ("Esc", "cancel")]),
    ];
    render_list_picker(f, area, "Themes", &[], &footer, |body_area| {
        let themes = theme::theme_overview();
        let cursor = cursor.min(themes.len().saturating_sub(1));
        let current = theme::current_theme_index();
        visible_picker_rows(themes.len(), cursor, picker_body_height(body_area))
            .map(|idx| {
                let option = themes[idx];
                let mut detail = option.blurb.to_string();
                detail.push_str(theme_source_suffix(option.source));
                if idx == current {
                    detail.push_str(" · current");
                }
                picker_line(idx == cursor, option.name.to_string(), Some(detail), true)
            })
            .collect()
    });
}
