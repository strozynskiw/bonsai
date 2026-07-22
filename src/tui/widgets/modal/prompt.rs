use super::common::*;
use super::*;
use unicode_width::UnicodeWidthStr;

/// Which approval scopes an [`ConfirmPrompt`] offers. Every prompt allows
/// once/session/deny; `project` adds a persistent per-project rule (the
/// sandbox-escalation prompt is the one that never offers it — bypassing the
/// enforcement floor is never made sticky).
pub(super) struct ApprovalScopes {
    pub project: bool,
}

/// A one-shot approval dialog: a heading, optional dim subtitle lines, a body,
/// and a scoped once/session/(project/)deny footer. The five approval prompts
/// are data now, not five copies of the same header/body/footer plumbing — a
/// new one (the M5.x pattern keeps adding them) is a spec builder plus a
/// `ModalKind` arm.
pub(super) struct ConfirmPrompt {
    pub title: &'static str,
    pub heading: Line<'static>,
    pub subtitle: Vec<Line<'static>>,
    pub body: Vec<Line<'static>>,
    pub scopes: ApprovalScopes,
}

pub(super) fn approval_footer(scopes: &ApprovalScopes) -> Vec<Line<'static>> {
    let mut hints: Vec<(&str, &str)> = vec![("A", "once"), ("S", "session")];
    if scopes.project {
        hints.push(("P", "project"));
    }
    hints.push(("D/Esc", "deny"));
    if scopes.project {
        // "Never": the persistent-deny counterpart to "project". Placed after
        // "deny" so the once/session/project allow tiers stay grouped.
        hints.push(("N", "never"));
    }
    vec![footer_hint_line(&hints)]
}

pub(super) fn render_confirm_prompt(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    prompt: &ConfirmPrompt,
    modal_scroll: u16,
) {
    let mut header_lines = Vec::with_capacity(prompt.subtitle.len() + 2);
    header_lines.push(prompt.heading.clone());
    header_lines.extend(prompt.subtitle.iter().cloned());
    header_lines.push(Line::from(""));
    render_scrollable_modal(
        f,
        area,
        prompt.title,
        &header_lines,
        &prompt.body,
        &approval_footer(&prompt.scopes),
        modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
}

pub(super) fn confirm_prompt_max_scroll(area: Rect, prompt: &ConfirmPrompt) -> u16 {
    let mut header_lines = Vec::with_capacity(prompt.subtitle.len() + 2);
    header_lines.push(prompt.heading.clone());
    header_lines.extend(prompt.subtitle.iter().cloned());
    header_lines.push(Line::from(""));
    let footer_lines = approval_footer(&prompt.scopes);
    let body = split_scrollable_modal_lines(area, &header_lines, &footer_lines).body;
    detail_max_scroll(body, &prompt.body)
}

fn origin_subtitle(origin: Option<&str>) -> Vec<Line<'static>> {
    origin
        .map(|origin| Line::from(Span::styled(format!("from {origin}"), theme::dim())))
        .into_iter()
        .collect()
}

pub(super) fn permission_prompt(command: &str, origin: Option<&str>) -> ConfirmPrompt {
    ConfirmPrompt {
        title: "Permission",
        heading: Line::from(Span::styled(
            "Allow this command to run?",
            theme::label(theme::palette().tool),
        )),
        subtitle: origin_subtitle(origin),
        body: vec![Line::from(Span::styled(
            command.to_string(),
            theme::body(theme::palette().text),
        ))],
        scopes: ApprovalScopes { project: true },
    }
}

pub(super) fn sandbox_escalation_prompt(command: &str, origin: Option<&str>) -> ConfirmPrompt {
    let mut subtitle = origin_subtitle(origin);
    subtitle.push(Line::from(Span::styled(
        "Bypasses filesystem/network confinement for this one run.",
        theme::dim(),
    )));
    ConfirmPrompt {
        title: "Sandbox escape",
        heading: Line::from(Span::styled(
            "⚠ Run this command OUTSIDE the sandbox?",
            theme::label(theme::palette().error),
        )),
        subtitle,
        body: vec![Line::from(Span::styled(
            command.to_string(),
            theme::body(theme::palette().text),
        ))],
        // Escaping the sandbox is the enforcement floor — never made sticky.
        scopes: ApprovalScopes { project: false },
    }
}

pub(super) fn web_domain_prompt(
    host: &str,
    url: &str,
    redirected_from: Option<&str>,
    origin: Option<&str>,
) -> ConfirmPrompt {
    let mut subtitle = origin_subtitle(origin);
    if let Some(from) = redirected_from {
        // A cross-domain redirect: make it explicit the user is approving a NEW
        // domain reached via redirect, not the one they originally allowed.
        subtitle.push(Line::from(Span::styled(
            format!("⚠ Redirected from {from} — approving adds a rule for {host}."),
            theme::body(theme::palette().error),
        )));
    }
    ConfirmPrompt {
        title: "Web access",
        heading: Line::from(Span::styled(
            "Allow web access to this domain?",
            theme::label(theme::palette().tool),
        )),
        subtitle,
        body: vec![
            Line::from(Span::styled(
                host.to_string(),
                theme::label(theme::palette().text),
            )),
            Line::from(Span::styled(url.to_string(), theme::dim())),
        ],
        scopes: ApprovalScopes { project: true },
    }
}

pub(super) fn extension_tool_prompt(
    id: &str,
    server: &str,
    capabilities: &[String],
    args_preview: &str,
) -> ConfirmPrompt {
    let caps_line = if capabilities.is_empty() {
        "capabilities: undeclared".to_string()
    } else {
        format!("capabilities: {}", capabilities.join(", "))
    };
    let mut body = vec![
        Line::from(Span::styled(
            id.to_string(),
            theme::label(theme::palette().text),
        )),
        Line::from(Span::styled(caps_line, theme::dim())),
    ];
    if !args_preview.is_empty() {
        body.push(Line::from(Span::styled(
            args_preview.to_string(),
            theme::dim(),
        )));
    }
    ConfirmPrompt {
        title: "Extension tool",
        heading: Line::from(Span::styled(
            "Allow this extension tool call?",
            theme::label(theme::palette().tool),
        )),
        subtitle: vec![Line::from(Span::styled(
            format!("from {server}"),
            theme::dim(),
        ))],
        body,
        scopes: ApprovalScopes { project: true },
    }
}

pub(super) fn hook_trust_prompt(
    name: &str,
    event: &str,
    action_kind: &str,
    action_preview: &str,
) -> ConfirmPrompt {
    ConfirmPrompt {
        title: "Hook trust",
        heading: Line::from(Span::styled(
            "Trust this project-config hook?",
            theme::label(theme::palette().tool),
        )),
        subtitle: vec![Line::from(Span::styled(
            format!("fires on {event} · {action_kind}"),
            theme::dim(),
        ))],
        body: vec![
            Line::from(Span::styled(
                name.to_string(),
                theme::label(theme::palette().text),
            )),
            Line::from(Span::styled(action_preview.to_string(), theme::dim())),
        ],
        scopes: ApprovalScopes { project: true },
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_question_prompt(
    f: &mut Frame,
    area: Rect,
    prompt: &str,
    header: Option<&str>,
    origin: Option<&str>,
    options: &[crate::interaction::QuestionOption],
    multiple: bool,
    cursor: usize,
    selected: &[bool],
    modal_scroll: u16,
) {
    let header_lines = question_prompt_header_lines(prompt, origin);

    let body_lines = question_prompt_body(options, multiple, cursor, selected).lines;

    let footer_lines = question_prompt_footer_lines(multiple);

    render_scrollable_modal(
        f,
        area,
        header.unwrap_or("Question"),
        &header_lines,
        &body_lines,
        &footer_lines,
        modal_scroll,
        None,
        &std::cell::RefCell::new(Vec::new()),
        &std::cell::Cell::new(None),
    );
}

pub(super) fn question_prompt_footer_lines(multiple: bool) -> Vec<Line<'static>> {
    let mut hints: Vec<(&str, &str)> = vec![("Up/Down", "move")];
    if multiple {
        hints.push(("Space", "toggle"));
    }
    hints.push(("Enter", "select"));
    hints.push(("Esc", "cancel"));
    vec![footer_hint_line(&hints)]
}

/// Builds the fixed header above a question's scrollable options.
///
/// Rendering and scroll metrics share this builder so header geometry cannot
/// drift when a prompt or origin is added.
pub(super) fn question_prompt_header_lines(
    prompt: &str,
    origin: Option<&str>,
) -> Vec<Line<'static>> {
    let mut header_lines = Vec::new();
    header_lines.extend(origin_subtitle(origin));
    header_lines.push(Line::from(Span::styled(
        prompt.to_string(),
        theme::body(theme::palette().text),
    )));
    header_lines.push(Line::from(""));

    header_lines
}

/// The scrollable option body of the question prompt, plus the logical line
/// index where each option's block begins (with a trailing end sentinel).
/// Render draws `lines`; `question_prompt_metrics` walks the
/// same list to clamp the scroll and place the auto-follow anchor, so the two
/// can never disagree about option geometry.
pub(super) struct QuestionPromptBody {
    pub lines: Vec<Line<'static>>,
    pub option_offsets: Vec<usize>,
}

pub(super) fn question_prompt_body(
    options: &[crate::interaction::QuestionOption],
    multiple: bool,
    cursor: usize,
    selected: &[bool],
) -> QuestionPromptBody {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut option_offsets: Vec<usize> = Vec::with_capacity(options.len() + 1);
    let label_width = options
        .iter()
        .map(|option| UnicodeWidthStr::width(option.label.as_str()))
        .max()
        .unwrap_or(0)
        .max(16);
    for (idx, opt) in options.iter().enumerate() {
        option_offsets.push(lines.len());
        let marker = if idx == cursor { "> " } else { "  " };
        let checkbox = if multiple {
            if selected.get(idx).copied().unwrap_or(false) {
                "[x] "
            } else {
                "[ ] "
            }
        } else {
            ""
        };
        let prefix = format!("{marker}{checkbox}");
        let label_padding =
            " ".repeat(label_width.saturating_sub(UnicodeWidthStr::width(opt.label.as_str())));
        let mut row = vec![
            Span::styled(prefix, theme::muted()),
            Span::styled(opt.label.clone(), theme::body(theme::palette().text)),
            Span::raw(label_padding),
        ];
        if !opt.description.is_empty() {
            row.push(Span::raw("  "));
            row.push(Span::styled(opt.description.clone(), theme::dim()));
        }
        lines.push(Line::from(row));
    }
    // The trailing sentinel makes `option_offsets[i+1]` the end of option i.
    option_offsets.push(lines.len());
    QuestionPromptBody {
        lines,
        option_offsets,
    }
}
