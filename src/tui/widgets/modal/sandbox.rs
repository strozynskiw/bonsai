use super::*;

pub(super) fn render_sandbox_status(f: &mut Frame, area: Rect, app: &AppState, cursor: usize) {
    let Some(sandbox) = &app.sandbox else {
        render_modal_notice(f, area, "Sandbox", "Sandbox state is unavailable.");
        return;
    };

    let p = theme::palette();
    let backend_available = sandbox.backend().is_available();
    let is_enabled = sandbox.is_enabled();
    let deny_network = sandbox.deny_network();
    let net_unenforced = deny_network && !sandbox.backend().supports_network_deny();
    let policy = sandbox.policy();

    // ── header: backend info ──────────────────────────────────────────────
    let backend_style = if backend_available {
        theme::body(p.success)
    } else {
        theme::dim()
    };
    let header = vec![
        modal_subtitle("OS-level confinement for this session, independent of approvals."),
        Line::from(vec![
            Span::styled("Backend  ", theme::muted()),
            Span::styled(sandbox.backend().label(), backend_style),
        ]),
    ];

    // ── interactive rows ──────────────────────────────────────────────────
    let desc =
        |text: &'static str| Line::from(vec![Span::raw("    "), Span::styled(text, theme::dim())]);
    let option_rows = vec![
        sandbox_option_line(
            cursor == 0,
            "Sandbox ",
            if is_enabled { "on " } else { "off" },
            if is_enabled { p.success } else { p.muted },
            if !backend_available && is_enabled {
                Some("no backend")
            } else {
                None
            },
        ),
        desc("OS kernel blocks writes outside the paths below, independent of approvals."),
        sandbox_option_line(
            cursor == 1,
            "Network ",
            if deny_network { "blocked" } else { "allowed" },
            if deny_network { p.error } else { p.success },
            if net_unenforced {
                Some("unenforced")
            } else {
                None
            },
        ),
        desc("Blocks outbound internet connections."),
    ];

    // ── writable roots ────────────────────────────────────────────────────
    let mut info_lines: Vec<Line<'static>> = vec![Line::from(""), section_header("Writable paths")];
    for root in &policy.writable_roots {
        info_lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(root.display().to_string(), theme::dim()),
        ]));
    }

    // ── body and footer through the shared shell ──────────────────────────
    let footer_lines = vec![footer_hint_line(&[
        ("Up/Down", "move"),
        ("Left/Right", "toggle"),
        ("Esc", "close"),
    ])];
    let mut body_lines: Vec<Line<'static>> = option_rows;
    body_lines.extend(info_lines);

    render_form_shell(
        f,
        area,
        "Sandbox",
        &header,
        &body_lines,
        &footer_lines,
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
}

fn sandbox_option_line(
    selected: bool,
    label: &'static str,
    value: &'static str,
    value_color: Color,
    note: Option<&'static str>,
) -> Line<'static> {
    let p = theme::palette();
    let marker = if selected { "> " } else { "  " };
    // Marker-only selection (house style): the `> ` marker signals the row, the
    // label keeps body text rather than recoloring.
    let mut spans = vec![
        Span::styled(marker, theme::muted()),
        Span::styled(label, theme::body(p.text)),
        Span::styled("  ·  ", theme::dim()),
        Span::styled(value, theme::body(value_color)),
    ];
    if let Some(note) = note {
        spans.push(Span::styled(format!("  ({note})"), theme::dim()));
    }
    Line::from(spans)
}
