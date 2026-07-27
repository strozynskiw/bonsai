use super::common::*;
use super::picker_common::*;
use super::*;

use crate::context_view::EpisodeReport;

pub(super) fn render_episodes(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    report: &ContextReport,
    cursor: usize,
) {
    let footer_lines = vec![
        Line::from(Span::styled(episode_summary(report), theme::dim())),
        footer_hint_line(&[("Up/Down", "move"), ("PgUp/PgDn", "page"), ("Esc", "close")]),
    ];
    render_list_detail_modal(
        f,
        area,
        app,
        ListDetailModal {
            title: "Episodes",
            detail_title: "Selected Episode",
            split: ListDetailSplit::Vertical,
            detail_focused: false,
            footer_lines,
            modal_scroll: app.modal_scroll,
        },
        |frame, table_area, _detail_area| {
            let episodes = &report.episodes;
            let columns = episode_columns();
            let widths = table_widths(table_area.width as usize, episodes, &columns);
            let mut lines = vec![table_header(&columns, &widths)];
            if episodes.is_empty() {
                lines.push(Line::from(Span::styled(empty_state(), theme::dim())));
            } else {
                let cursor = cursor.min(episodes.len().saturating_sub(1));
                let visible_count = table_area.height.saturating_sub(1).max(1) as usize;
                lines.extend(
                    visible_picker_rows(episodes.len(), cursor, visible_count).map(|index| {
                        table_row(&episodes[index], &columns, &widths, index == cursor)
                    }),
                );
            }
            frame.render_widget(
                Paragraph::new(lines)
                    .style(theme::panel())
                    .wrap(Wrap { trim: false }),
                table_area,
            );
            episodes
                .get(cursor.min(episodes.len().saturating_sub(1)))
                .map(episode_detail_lines)
                .unwrap_or_else(|| vec![Line::from(Span::styled(empty_state(), theme::dim()))])
        },
    );
}

fn empty_state() -> &'static str {
    if crate::episode::episodes_enabled() {
        "No episodes tracked yet — one opens on the next user turn."
    } else {
        "Episodes are disabled. Set BONSAI_EPISODES=1 to enable them."
    }
}

fn episode_summary(report: &ContextReport) -> String {
    if report.episodes.is_empty() {
        return empty_state().to_string();
    }
    let active = report
        .episodes
        .iter()
        .filter(|episode| episode.status_label == "active")
        .count();
    format!(
        "{} episode{} · {active} active",
        report.episodes.len(),
        if report.episodes.len() == 1 { "" } else { "s" },
    )
}

fn episode_columns() -> Vec<TableColumn<EpisodeReport>> {
    vec![
        TableColumn {
            header: "#",
            width: TableWidth::Fit { cap: 5 },
            render: CellRender::Value(|episode| episode.seq.to_string()),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Status",
            width: TableWidth::Fit { cap: 9 },
            render: CellRender::Value(|episode| episode.status_label.clone()),
            style: CellStyle::Custom(|episode| {
                status_style(match episode.status_label.as_str() {
                    "active" => theme::palette().progress,
                    "evicted" => theme::palette().todo,
                    "restored" => theme::palette().success,
                    _ => theme::palette().muted,
                })
            }),
        },
        TableColumn {
            header: "Span / archive",
            width: TableWidth::Fit { cap: 16 },
            render: CellRender::Value(episode_span_label),
            style: CellStyle::Meta,
        },
        TableColumn {
            header: "Title / goal",
            width: TableWidth::Flex { min: 0 },
            render: CellRender::Fitted(|episode, width| {
                truncate_ascii(&episode_title_or_goal(episode), width)
            }),
            style: CellStyle::Text,
        },
    ]
}

fn episode_title_or_goal(episode: &EpisodeReport) -> String {
    if episode.title.is_empty() {
        episode.goal.clone()
    } else {
        episode.title.clone()
    }
}

fn episode_span_label(episode: &EpisodeReport) -> String {
    if let Some(messages) = episode.live_span_messages {
        format!("{messages} live")
    } else if episode.archived_messages > 0 {
        format!("{} archived", episode.archived_messages)
    } else {
        "not in context".to_string()
    }
}

pub(super) fn episode_detail_lines(episode: &EpisodeReport) -> Vec<Line<'static>> {
    let fields = [
        ("Sequence", format!("#{}", episode.seq)),
        ("Status", episode.status_label.clone()),
        (
            "Title",
            if episode.title.is_empty() {
                "(untitled)".to_string()
            } else {
                episode.title.clone()
            },
        ),
        ("Goal", episode.goal.clone()),
        (
            "Live span",
            episode
                .live_span_messages
                .map(|messages| format!("{messages} messages"))
                .unwrap_or_else(|| "not in live context".to_string()),
        ),
        (
            "Close reason",
            episode
                .close_reason_label
                .clone()
                .unwrap_or_else(|| "open".to_string()),
        ),
        (
            "Evicted tokens",
            episode
                .evicted_tokens
                .map(|tokens| tokens.to_string())
                .unwrap_or_else(|| "none".to_string()),
        ),
        ("Archived messages", episode.archived_messages.to_string()),
        ("Recall count", episode.recall_count.to_string()),
        (
            "Flags",
            match (episode.completable, episode.repaired) {
                (true, true) => "todos complete · repaired".to_string(),
                (true, false) => "todos complete".to_string(),
                (false, true) => "repaired after resume skew".to_string(),
                (false, false) => "none".to_string(),
            },
        ),
    ];
    fields
        .into_iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(format!("{label:<18}"), theme::muted()),
                Span::styled(value, theme::body(theme::palette().text)),
            ])
        })
        .collect()
}

pub(super) fn max_episode_detail_scroll(
    area: Rect,
    episodes: &[EpisodeReport],
    cursor: usize,
) -> u16 {
    let Some(episode) = episodes.get(cursor.min(episodes.len().saturating_sub(1))) else {
        return 0;
    };
    let (_, detail_area, _) = list_detail_regions(area, ListDetailSplit::Vertical);
    detail_max_scroll(
        detail_pane_inner(detail_area),
        &episode_detail_lines(episode),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn populated_detail_carries_lifecycle_metadata() {
        let episode = EpisodeReport {
            seq: 2,
            title: "Implement modal".to_string(),
            status_label: "evicted".to_string(),
            close_reason_label: Some("title_change".to_string()),
            goal: "Open a modal".to_string(),
            live_span_messages: None,
            evicted_tokens: Some(12_345),
            recall_count: 3,
            archived_messages: 5,
            completable: true,
            repaired: true,
        };
        let detail = episode_detail_lines(&episode)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(detail.contains("title_change"));
        assert!(detail.contains("12345"));
        assert!(detail.contains("Archived messages 5"));
        assert!(detail.contains("Recall count      3"));
        assert!(detail.contains("todos complete · repaired"));
    }
}
