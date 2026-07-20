use super::*;

pub(crate) fn diff_header_line(diff: &crate::diff::FileDiff) -> Line<'static> {
    let (word, color) = match diff.status {
        crate::diff::DiffStatus::Created => ("created", theme::palette().added),
        crate::diff::DiffStatus::Modified => ("modified", theme::palette().edit),
        crate::diff::DiffStatus::Deleted => ("deleted", theme::palette().removed),
    };
    let stats = diff.own_stats();
    Line::from(vec![
        Span::styled(
            format!("{word} "),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            diff.path.clone(),
            Style::default()
                .fg(theme::palette().path)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  +{}", stats.added),
            Style::default()
                .fg(theme::palette().added)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" -{}", stats.removed),
            Style::default()
                .fg(theme::palette().removed)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

pub(crate) fn diff_lines(diff: &crate::diff::FileDiff, max_rows: usize) -> Vec<Line<'static>> {
    let total: usize = diff.hunks.iter().map(|hunk| hunk.lines.len()).sum();
    let mut out = Vec::new();
    let mut shown = 0usize;
    'hunks: for (index, hunk) in diff.hunks.iter().enumerate() {
        if index > 0 {
            out.push(Line::from(Span::styled(
                "   ⋯".to_string(),
                Style::default().fg(theme::palette().dim),
            )));
        }
        for line in &hunk.lines {
            if shown >= max_rows {
                break 'hunks;
            }
            out.push(diff_line(line));
            shown += 1;
        }
    }
    if total > shown || diff.truncated {
        let hidden = total.saturating_sub(shown);
        out.push(Line::from(Span::styled(
            format!("⋯ {hidden} more — click the card (or Enter in Tools/Edits) for the full diff"),
            Style::default().fg(theme::palette().dim),
        )));
    }
    out
}

pub(super) fn diff_line(line: &crate::diff::DiffLine) -> Line<'static> {
    let (marker, fg, bg, number) = match line.kind {
        crate::diff::DiffLineKind::Added => (
            "+",
            theme::palette().added,
            Some(theme::palette().added_bg),
            line.new_line,
        ),
        crate::diff::DiffLineKind::Removed => (
            "-",
            theme::palette().removed,
            Some(theme::palette().removed_bg),
            line.old_line,
        ),
        crate::diff::DiffLineKind::Context => (
            " ",
            theme::palette().muted,
            None,
            line.old_line.or(line.new_line),
        ),
    };
    let number = number
        .map(|n| format!("{n:>4} "))
        .unwrap_or_else(|| "     ".to_string());
    let mut content_style = Style::default().fg(fg);
    if let Some(bg) = bg {
        content_style = content_style.bg(bg);
    }
    Line::from(vec![
        Span::styled(number, Style::default().fg(theme::palette().lineno)),
        Span::styled(format!("{marker} {}", line.content), content_style),
    ])
}
