use crate::peer::PeerOverview;

use super::common::*;
use super::picker_common::*;
use super::*;

/// The `/peers` view (peers P5): a live, read-only list of the other bonsai
/// sessions in this project root — session id, title, advisory claims, and
/// recently changed files. One row per peer; the selected row expands its
/// claims and changed files below.
pub(super) fn render_peer_list(f: &mut Frame, area: Rect, peers: &[PeerOverview], cursor: usize) {
    let footer = vec![
        Line::from(Span::styled(
            "Read-only view. Message peers with the peers tool.",
            theme::dim(),
        )),
        footer_hint_line(&[("Up/Down", "move"), ("Esc", "close")]),
    ];
    render_list_picker(
        f,
        area,
        "Bonsai Sessions in This Project",
        &[],
        &footer,
        |list_area| {
            if peers.is_empty() {
                return vec![Line::from(Span::styled(
                    "No other live bonsai sessions in this project",
                    theme::dim(),
                ))];
            }
            let cursor = cursor.min(peers.len().saturating_sub(1));
            let id_width = column_width(peers, "id", 8, |peer| format!("#{}", peer.id));
            let visible = list_area.height.max(1) as usize;
            visible_picker_rows(peers.len(), cursor, visible)
                .map(|idx| peer_row(&peers[idx], id_width, idx == cursor))
                .collect()
        },
    );
}

fn peer_row(peer: &PeerOverview, id_width: usize, selected: bool) -> Line<'static> {
    let marker = if selected { "> " } else { "  " };
    let title = if peer.title.trim().is_empty() {
        "(no title)".to_string()
    } else {
        peer.title.trim().to_string()
    };
    let (state, state_color) = if peer.working {
        ("working", theme::palette().progress)
    } else {
        ("idle", theme::palette().success)
    };
    let mut spans = vec![
        Span::styled(marker, theme::muted()),
        Span::styled(
            pad_ascii(&format!("#{}", peer.id), id_width),
            theme::body(theme::palette().peer),
        ),
        Span::styled("  ", theme::dim()),
        Span::styled(pad_ascii(state, 8), theme::body(state_color)),
        Span::styled(title, theme::body(theme::palette().text)),
    ];
    if !peer.claims.is_empty() {
        spans.push(Span::styled(
            format!("  · claims: {}", peer.claims.join(", ")),
            theme::body(theme::palette().todo),
        ));
    }
    if peer.waiting_on_peer {
        spans.push(Span::styled(
            "  · you are waiting",
            theme::body(theme::palette().progress),
        ));
    }
    if peer.peer_waiting_on_you {
        spans.push(Span::styled(
            "  · waiting for you",
            theme::body(theme::palette().todo),
        ));
    }
    if !peer.changed_files.is_empty() {
        let shown = peer
            .changed_files
            .iter()
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let extra = peer.changed_files.len().saturating_sub(3);
        let suffix = if extra > 0 {
            format!(" (+{extra})")
        } else {
            String::new()
        };
        spans.push(Span::styled(
            format!("  · changed: {shown}{suffix}"),
            theme::dim(),
        ));
    }
    Line::from(spans)
}
