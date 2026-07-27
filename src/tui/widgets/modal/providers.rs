use super::picker_common::*;
use super::*;
use crate::tui::provider_manager::{ProviderDetail, ProviderManagerRow, ProviderOrigin};

pub(super) fn render_provider_manager(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    rows: &[ProviderManagerRow],
    filter: &str,
    searching: bool,
    cursor: usize,
) {
    use crate::tui::provider_manager::provider_manager_filtered;
    let filtered = provider_manager_filtered(rows, filter);
    let selected = filtered.get(cursor).copied();

    // Header: the search box, shown only once search has been engaged (`/`) or
    // a filter is applied — otherwise the manager keeps its full row budget.
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

    let detail = selected
        .map(|row| {
            if row.base_url.is_empty() {
                row.connection_id.clone()
            } else {
                format!("{} · {}", row.connection_id, row.base_url)
            }
        })
        .unwrap_or_default();
    // `d` is contextual: delete a custom provider, disable an enabled
    // built-in, re-enable a disabled one.
    let d_label = match selected {
        Some(row) if row.origin == ProviderOrigin::Custom => "remove",
        Some(row) if row.origin == ProviderOrigin::Project => "project config",
        Some(row) if row.enabled => "disable",
        _ => "enable",
    };
    // While typing, only the search-exit hint is relevant; otherwise show the
    // management shortcuts plus how to start/clear a search.
    let commands = if searching {
        footer_hint_line(&[("Enter", "detail"), ("Esc", "done"), ("Type", "filter")])
    } else if filter.is_empty() {
        footer_hint_line(&[
            ("/", "search"),
            ("a", "add"),
            ("e", "edit"),
            ("d", d_label),
            ("Esc", "close"),
        ])
    } else {
        footer_hint_line(&[
            ("/", "search"),
            ("a", "add"),
            ("e", "edit"),
            ("d", d_label),
            ("Esc", "clear"),
        ])
    };
    // Fixed three-line bottom block — a spacer, the selected provider's detail
    // line, and the command hints — so the list body keeps a constant height as
    // the cursor moves between rows. Previously an unauthorized row added an
    // auth-hint line here, growing the footer and shrinking the list mid-scroll.
    // The auth hint still surfaces in the provider detail view (Enter).
    let footer = vec![
        Line::from(""),
        Line::from(Span::styled(detail, theme::dim())),
        commands,
    ];
    render_list_picker(f, area, "Providers", &header, &footer, |body_area| {
        if filtered.is_empty() {
            let empty = if rows.is_empty() {
                "No providers"
            } else {
                "No matching providers"
            };
            return vec![Line::from(Span::styled(empty, theme::dim()))];
        }
        let cursor = cursor.min(filtered.len().saturating_sub(1));
        // Model-picker viewport semantics: the cursor moves within the visible
        // window and the list shifts only when the cursor crosses an edge,
        // instead of scrolling on every keypress past the first page.
        let capacity = picker_body_height(body_area).max(1);
        let offset = crate::tui::app::reconcile_viewport(
            app.provider_manager_offset.get(),
            cursor,
            filtered.len(),
            capacity,
        );
        app.provider_manager_offset.set(offset);
        (offset..(offset + capacity.min(filtered.len())).min(filtered.len()))
            .map(|idx| {
                let row = filtered[idx];
                let mut name = row.display_name.clone();
                if row.current {
                    name.push_str(" (current)");
                }
                let origin = match row.origin {
                    ProviderOrigin::BuiltIn => "built-in",
                    ProviderOrigin::Custom => "custom",
                    ProviderOrigin::Project => "project",
                };
                let tag = format!("({origin}) {}", row.status_label());
                picker_line(idx == cursor, name, Some(tag), row.enabled)
            })
            .collect()
    });
}

/// The body region of the provider detail view (above the footer). Shared by
/// the renderer and the scroll-clamp helper so their geometry can't drift.
fn provider_detail_body(area: Rect) -> Rect {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(area)[0]
}

/// Read-only provider inspector: a framed, scrollable detail pane plus a footer.
pub(super) fn render_provider_detail(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    detail: &ProviderDetail,
    modal_scroll: u16,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(area);
    render_detail_pane(
        f,
        chunks[0],
        &format!("Provider · {}", detail.display_name),
        true,
        provider_detail_lines(detail),
        modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
    f.render_widget(
        Paragraph::new(footer_hint_line(&[
            ("Up/Down", "scroll"),
            ("PgUp/PgDn", "page"),
            ("d-click", "copy word"),
            ("Esc", "back"),
        ]))
        .style(theme::panel()),
        chunks[1],
    );
}

/// Max detail scroll for a provider — total wrapped content height minus the
/// visible pane height. Mirrors `skill_detail_max_scroll`.
pub(super) fn provider_detail_max_scroll(area: Rect, detail: &ProviderDetail) -> u16 {
    detail_max_scroll(
        detail_pane_inner(provider_detail_body(area)),
        &provider_detail_lines(detail),
    )
}

fn provider_detail_field(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), theme::dim()),
        Span::styled(value.to_string(), theme::body(theme::palette().text)),
    ])
}

/// Origin-specific guidance, shown inside the detail view. This is where the
/// built-in "cannot be edited" hint now lives instead of being pushed to the
/// transcript on every `e` keypress.
fn provider_detail_origin_note(detail: &ProviderDetail) -> Option<String> {
    match detail.origin {
        ProviderOrigin::BuiltIn => Some(format!(
            "Built-in provider — cannot be edited. Run /authorize {} for credentials, or press `d` in the list to disable it.",
            detail.connection_id
        )),
        ProviderOrigin::Project => Some(
            "Project provider — managed by the trusted `.bonsai/providers` and `.bonsai/models` files.".to_string(),
        ),
        ProviderOrigin::Custom => None,
    }
}

fn provider_detail_lines(detail: &ProviderDetail) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let mut lines = Vec::new();

    let mut name = detail.display_name.clone();
    if detail.current {
        name.push_str(" (current)");
    }
    lines.push(Line::from(Span::styled(name, theme::label(palette.text))));
    let origin = match detail.origin {
        ProviderOrigin::BuiltIn => "built-in",
        ProviderOrigin::Custom => "custom",
        ProviderOrigin::Project => "project",
    };
    lines.push(Line::from(Span::styled(
        format!("{origin} · {}", detail.status_label),
        theme::dim(),
    )));
    if let Some(hint) = detail.auth_hint.as_deref() {
        lines.push(Line::from(Span::styled(
            hint.to_string(),
            theme::body(palette.error),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Connection",
        theme::label(palette.text),
    )));
    lines.push(provider_detail_field("id", &detail.connection_id));
    if !detail.base_url.is_empty() {
        lines.push(provider_detail_field("base URL", &detail.base_url));
    }
    if !detail.endpoint_path.is_empty() {
        lines.push(provider_detail_field("endpoint", &detail.endpoint_path));
    }
    if !detail.transport.is_empty() {
        lines.push(provider_detail_field("transport", &detail.transport));
    }
    if !detail.default_model.is_empty() {
        lines.push(provider_detail_field(
            "default model",
            &detail.default_model,
        ));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Environment",
        theme::label(palette.text),
    )));
    if detail.env_vars.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no environment variables)",
            theme::dim(),
        )));
    } else {
        for env in &detail.env_vars {
            let (state, color) = if env.is_set {
                ("set", palette.success)
            } else {
                ("not set", palette.dim)
            };
            lines.push(Line::from(vec![
                Span::styled(env.name.clone(), theme::body(palette.text)),
                Span::styled(format!("  ({})  ", env.role), theme::dim()),
                Span::styled(state.to_string(), theme::body(color)),
            ]));
        }
    }

    if let Some(note) = provider_detail_origin_note(detail) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(note, theme::dim())));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!("Models ({})", detail.models.len()),
        theme::label(palette.text),
    )));
    if detail.models.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no models available)",
            theme::dim(),
        )));
    } else {
        for model in &detail.models {
            lines.push(Line::from(Span::styled(
                model.clone(),
                theme::body(palette.text),
            )));
        }
    }

    lines
}

pub(super) fn render_provider_remove_confirm(
    f: &mut Frame,
    area: Rect,
    display_name: &str,
    disable_builtin: bool,
) {
    let (title, verb, note) = if disable_builtin {
        (
            "Disable Provider",
            "Disable ",
            "Credentials are kept; re-enable it from /providers.",
        )
    } else {
        (
            "Remove Provider",
            "Remove ",
            "Deletes its catalog files and stored credentials.",
        )
    };
    let panel = theme::frame(title, true).style(theme::panel());
    let inner = panel.inner(area);
    f.render_widget(panel, area);

    let lines = vec![
        Line::from(vec![
            Span::styled(verb, theme::body(theme::palette().text)),
            Span::styled(
                display_name.to_string(),
                theme::body(theme::palette().error),
            ),
            Span::styled("?", theme::body(theme::palette().text)),
        ]),
        Line::from(Span::styled(note, theme::dim())),
        Line::from(""),
        footer_hint_line(&[("Enter/Y", "confirm"), ("Esc/N", "cancel")]),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
