use super::common::{ModalCaches, ModalLines, ModalScrollView};
use super::*;

/// The subtitle every sandbox popup carries.
const SANDBOX_SUBTITLE: &str = "OS-level confinement for this session, independent of approvals.";
/// What the file-system toggle does.
const SANDBOX_DESC: &str =
    "OS kernel blocks writes outside the paths below, independent of approvals.";
/// What the network toggle does.
const SANDBOX_NETWORK_DESC: &str = "Blocks outbound internet connections.";

pub(super) fn render_sandbox_status(f: &mut Frame, area: Rect, app: &AppState, cursor: usize) {
    let Some((header, body, footer)) = sandbox_lines(app, cursor) else {
        render_modal_notice(f, area, "Sandbox", "Sandbox state is unavailable.");
        return;
    };
    // A scrollable body, not a windowed one: the writable-root list can outgrow the
    // popup, and a list the user cannot reach is worse than a scrollbar.
    render_scrollable_modal(
        f,
        area,
        "Sandbox",
        ModalLines {
            header: &header,
            body: &body,
            footer: &footer,
        },
        ModalScrollView {
            scroll: app.modal_scroll,
            selection: app.modal_selection.map(|s| s.range()),
        },
        ModalCaches {
            body: &app.modal_body_lines,
            body_rect: &app.modal_body_rect,
        },
    );
}

/// Header, body and footer of the sandbox popup, built once so the renderer, the
/// height it asks for and the scroll clamp all resolve the same lines. `None` when
/// the session has no sandbox state to describe.
pub(super) fn sandbox_lines(
    app: &AppState,
    cursor: usize,
) -> Option<(Vec<Line<'static>>, Vec<Line<'static>>, Vec<Line<'static>>)> {
    let sandbox = app.sandbox.as_ref()?;
    let p = theme::palette();
    let backend_available = sandbox.backend().is_available();
    let is_enabled = sandbox.is_enabled();
    let deny_network = sandbox.deny_network();
    let net_unenforced = deny_network && !sandbox.backend().supports_network_deny();
    let policy = sandbox.policy();

    let backend_style = if backend_available {
        theme::body(p.success)
    } else {
        theme::dim()
    };
    let header = vec![
        modal_subtitle(SANDBOX_SUBTITLE),
        Line::from(vec![
            Span::styled("Backend  ", theme::muted()),
            Span::styled(sandbox.backend().label(), backend_style),
        ]),
    ];

    let mut body = vec![
        row_value_line(
            cursor == 0,
            "Sandbox ",
            if is_enabled { "on " } else { "off" },
            if is_enabled { p.success } else { p.muted },
            (!backend_available && is_enabled).then_some("no backend"),
        ),
        sandbox_description(SANDBOX_DESC),
        row_value_line(
            cursor == 1,
            "Network ",
            if deny_network { "blocked" } else { "allowed" },
            if deny_network { p.error } else { p.success },
            net_unenforced.then_some("unenforced"),
        ),
        sandbox_description(SANDBOX_NETWORK_DESC),
        Line::from(""),
        section_header("Writable paths"),
    ];
    for root in &policy.writable_roots {
        body.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(root.display().to_string(), theme::dim()),
        ]));
    }

    let footer = vec![footer_hint_line(SANDBOX_HINTS)];
    Some((header, body, footer))
}

/// Rows the sandbox popup asks for: the chrome, both toggles with their
/// descriptions and the writable-paths heading, plus one root. The measurement
/// uses the widest wording each row can carry, and further roots scroll — so the
/// heading and its first entry are always on screen and nothing is unreachable.
pub(super) fn sandbox_status_rows(area: Rect) -> u16 {
    let inner_width = ModalFootprint::Prompt
        .width(area)
        .saturating_sub(MODAL_FRAME_COLUMNS);
    let width = inner_width.max(1) as usize;
    let palette = theme::palette();
    let header = vec![
        modal_subtitle(SANDBOX_SUBTITLE),
        Line::from(vec![
            Span::styled("Backend  ", theme::muted()),
            Span::styled("macos seatbelt", theme::dim()),
        ]),
    ];
    let body = vec![
        row_value_line(false, "Sandbox ", "off ", palette.muted, Some("no backend")),
        sandbox_description(SANDBOX_DESC),
        row_value_line(
            false,
            "Network ",
            "blocked",
            palette.error,
            Some("unenforced"),
        ),
        sandbox_description(SANDBOX_NETWORK_DESC),
        Line::from(""),
        section_header("Writable paths"),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("/tmp/project-roots", theme::dim()),
        ]),
    ];
    shell_modal_rows(
        wrapped_lines_height(&header, width) as u16,
        wrapped_lines_height(&body, width) as u16,
        1,
    )
}

/// The dim explanation indented under a toggle.
fn sandbox_description(text: &'static str) -> Line<'static> {
    Line::from(vec![Span::raw("    "), Span::styled(text, theme::dim())])
}
