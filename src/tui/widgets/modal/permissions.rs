use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::common::wrap_line;
use super::picker_common::*;
use super::*;
use crate::permissions::{Permission, RuleSource};
use crate::tui::permissions_manager::{PermissionRuleRow, RuleLane, permission_manager_filtered};

/// The row marker: `> ` on the selected row, blank elsewhere. Two cells wide, so
/// it also fixes the base column the decision word starts at.
const MARKER_WIDTH: usize = 2;
/// The separator between the decision and the pattern.
const SEP: &str = " · ";

/// The `/permissions` manager: a single searchable list of every editable rule
/// (bash command + web-domain, session + persisted), each removable with `d`.
/// Mirrors the provider manager's search chrome; `cursor` indexes the filtered
/// view carried in the modal state.
pub(super) fn render_permissions_manager(
    f: &mut Frame,
    area: Rect,
    rows: &[PermissionRuleRow],
    filter: &str,
    searching: bool,
    cursor: usize,
) {
    let filtered = permission_manager_filtered(rows, filter);

    // Search box header, shown only once search is engaged (`/`) or a filter is
    // applied — otherwise the list keeps its full row budget.
    let header = if searching || !filter.is_empty() {
        let mut spans = vec![
            Span::styled("Search: ", theme::muted()),
            Span::styled(filter.to_string(), theme::body(theme::palette().text)),
        ];
        if searching {
            spans.push(Span::styled("▏", theme::muted()));
        }
        vec![Line::from(spans)]
    } else {
        Vec::new()
    };

    let counts = rule_counts_line(rows);
    let commands = if searching {
        footer_hint_line(&[("Esc/Enter", "done"), ("Type", "filter")])
    } else if filter.is_empty() {
        footer_hint_line(&[("/", "search"), ("d", "delete"), ("Esc", "close")])
    } else {
        footer_hint_line(&[("/", "search"), ("d", "delete"), ("Esc", "clear")])
    };
    // Fixed two-line footer so the list body keeps a constant height as the
    // cursor moves: a summary count line and the command hints.
    let footer = vec![counts, commands];

    render_list_picker(f, area, "Permissions", &header, &footer, |body_area| {
        if filtered.is_empty() {
            let empty = if rows.is_empty() {
                "No editable rules. Approve a command with \"always for project\", or deny with \"never\", to add one."
            } else {
                "No matching rules"
            };
            return vec![Line::from(Span::styled(empty, theme::dim()))];
        }
        let cursor = cursor.min(filtered.len().saturating_sub(1));
        let capacity = picker_body_height(body_area).max(1);
        let range = visible_picker_rows(filtered.len(), cursor, capacity);
        let width = body_area.width.max(1) as usize;
        range
            .flat_map(|idx| render_rule_lines(filtered[idx], idx == cursor, width))
            .collect()
    });
}

/// Render one rule as its (possibly wrapped) visual lines. The first line is
/// `<marker><decision> · <pattern…> <tag>`; wrapped continuations hang under the
/// *pattern* (indented past the coloured decision + separator) so a multi-line
/// command reads as one block. The decision and scope are coloured so they stay
/// scannable against the neutral pattern text.
fn render_rule_lines(row: &PermissionRuleRow, selected: bool, width: usize) -> Vec<Line<'static>> {
    let marker = if selected { "> " } else { "  " };
    let decision = row.permission.as_db_str();
    // Column where the pattern begins — the hang for continuation lines.
    let head_width = MARKER_WIDTH + decision.width() + SEP.width();

    let head = vec![
        Span::styled(marker.to_string(), theme::muted()),
        Span::styled(decision.to_string(), decision_style(row.permission)),
        Span::styled(SEP.to_string(), theme::dim()),
    ];

    // The wrappable remainder: the pattern, then the dim/coloured metadata tag.
    let mut body_spans = vec![Span::styled(
        row.pattern.clone(),
        theme::body(theme::palette().text),
    )];
    body_spans.push(Span::styled("  ", theme::dim()));
    body_spans.extend(tag_spans(row));

    let content_width = width.saturating_sub(head_width).max(1);
    wrap_line(&Line::from(body_spans), content_width)
        .into_iter()
        .enumerate()
        .map(|(idx, line)| {
            let mut spans = Vec::with_capacity(line.spans.len() + head.len());
            if idx == 0 {
                spans.extend(head.clone());
            } else {
                spans.push(Span::raw(" ".repeat(head_width)));
            }
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// Colour the decision so allow/deny/ask read at a glance: green allow, red
/// deny, amber ask, all bold.
fn decision_style(permission: Permission) -> Style {
    let color = match permission {
        Permission::Allow => theme::palette().success,
        Permission::Deny => theme::palette().error,
        Permission::Ask => theme::palette().todo,
    };
    theme::body(color).add_modifier(Modifier::BOLD)
}

/// Colour the rule's scope so its lifetime stands out from the dim lane tag:
/// amber for the ephemeral session, blue for project, teal for global.
fn scope_style(source: RuleSource) -> Style {
    let color = match source {
        RuleSource::Session => theme::palette().plan_accent,
        RuleSource::Project => theme::palette().peer,
        RuleSource::Global => theme::palette().tool,
    };
    theme::body(color)
}

/// The metadata tag spans: dim `(lane)`, the coloured scope, and a dim `#id`
/// (persisted rules only). Kept as separate spans so the scope keeps its colour.
fn tag_spans(row: &PermissionRuleRow) -> Vec<Span<'static>> {
    let mut spans = vec![
        Span::styled(format!("({}) ", row.lane.label()), theme::dim()),
        Span::styled(row.source.label().to_string(), scope_style(row.source)),
    ];
    if let Some(id) = row.id {
        spans.push(Span::styled(format!(" #{id}"), theme::dim()));
    }
    spans
}

fn rule_counts_line(rows: &[PermissionRuleRow]) -> Line<'static> {
    let bash = rows.iter().filter(|r| r.lane == RuleLane::Bash).count();
    let domain = rows.len().saturating_sub(bash);
    let persisted = rows.iter().filter(|r| r.id.is_some()).count();
    let session = rows.len().saturating_sub(persisted);
    Line::from(Span::styled(
        format!(
            "{} command · {} domain · {} persisted · {} session",
            bash, domain, persisted, session
        ),
        theme::dim(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn rule(pattern: &str) -> PermissionRuleRow {
        PermissionRuleRow {
            lane: RuleLane::Bash,
            source: RuleSource::Project,
            pattern: pattern.to_string(),
            permission: Permission::Allow,
            id: Some(3),
        }
    }

    #[test]
    fn wrapped_rule_lines_hang_under_the_pattern() {
        let row = rule("git commit -m a very long message that has to wrap several times");
        let lines = render_rule_lines(&row, true, 30);

        assert!(lines.len() > 1, "long pattern should wrap at width 30");
        // First line: selection marker, then the coloured decision, then pattern.
        assert!(line_text(&lines[0]).starts_with("> allow · git"));
        // The pattern begins at `marker + "allow" + " · "` = 10 columns; every
        // continuation hangs to that same column so the command reads as a block.
        let hang = MARKER_WIDTH + "allow".width() + SEP.width();
        for line in &lines[1..] {
            let text = line_text(line);
            // The injected hang prefix is exactly `hang` spaces (a wrapped chunk
            // may add more if it breaks on a space, so check the prefix, not the
            // total leading run).
            assert!(
                text.chars().take(hang).all(|c| c == ' '),
                "continuation should hang under the pattern, got {text:?}"
            );
            assert!(line.width() <= 30, "line overflows width: {text:?}");
        }
        // An unselected row shows a blank marker, not the arrow.
        let unselected = render_rule_lines(&row, false, 30);
        assert!(!line_text(&unselected[0]).starts_with('>'));
    }

    #[test]
    fn decision_and_scope_are_coloured_distinctly() {
        let allow = decision_style(Permission::Allow);
        let deny = decision_style(Permission::Deny);
        assert_ne!(allow, deny, "allow and deny must be visually distinct");
        assert!(allow.add_modifier.contains(Modifier::BOLD));
        // Scope colours differ by lifetime so session stands out from project.
        assert_ne!(
            scope_style(RuleSource::Session),
            scope_style(RuleSource::Project)
        );
    }
}
