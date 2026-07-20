use super::*;

pub(super) fn render_sandbox_status(f: &mut Frame, area: Rect, app: &AppState, cursor: usize) {
    let frame = theme::frame("Sandbox", true);
    let inner = frame.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(Vec::<Line<'static>>::new())
            .block(frame)
            .style(theme::panel()),
        area,
    );

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let Some(sandbox) = &app.sandbox else {
        f.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                "Sandbox state unavailable.",
                theme::dim(),
            ))])
            .style(theme::panel()),
            inner,
        );
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
        Line::from(vec![
            Span::styled("Backend  ", theme::muted()),
            Span::styled(sandbox.backend().label(), backend_style),
        ]),
        Line::from(""),
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
    let mut info_lines: Vec<Line<'static>> = vec![
        Line::from(""),
        Line::from(Span::styled("Writable paths:", theme::muted())),
    ];
    for root in &policy.writable_roots {
        info_lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(root.display().to_string(), theme::dim()),
        ]));
    }

    // ── footer ────────────────────────────────────────────────────────────
    let footer = super::common::footer_hint_line(&[
        ("Up/Down", "move"),
        ("Left/Right", "toggle"),
        ("Esc", "close"),
    ]);

    // ── layout: body above, footer pinned to bottom ───────────────────────
    let footer_height: u16 = 2; // blank separator + footer line
    if inner.height <= footer_height {
        f.render_widget(Paragraph::new(vec![footer]).style(theme::panel()), inner);
        return;
    }

    let body_rect = Rect {
        height: inner.height - footer_height,
        ..inner
    };
    let footer_rect = Rect {
        y: inner.y + inner.height - footer_height,
        height: footer_height,
        ..inner
    };

    let mut body_lines: Vec<Line<'static>> = header;
    body_lines.extend(option_rows);
    body_lines.extend(info_lines);

    f.render_widget(
        Paragraph::new(body_lines)
            .style(theme::panel())
            .wrap(Wrap { trim: false }),
        body_rect,
    );
    f.render_widget(
        Paragraph::new(vec![Line::from(""), footer]).style(theme::panel()),
        footer_rect,
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
