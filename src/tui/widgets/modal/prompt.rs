use super::common::*;
use super::picker_common::pad_ascii;
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
    vec![footer_hint_line(approval_hints(scopes.project))]
}

/// The fixed header above a confirm prompt: the decision, then its subtitles.
/// Render and scroll metrics share this builder, and the shell owns the blank
/// row that separates the header from the body.
pub(super) fn confirm_prompt_header_lines(prompt: &ConfirmPrompt) -> Vec<Line<'static>> {
    let mut header_lines = Vec::with_capacity(prompt.subtitle.len() + 1);
    header_lines.push(prompt.heading.clone());
    header_lines.extend(prompt.subtitle.iter().cloned());
    header_lines
}

/// Rows an approval prompt needs, measured with the same header, body and footer
/// builders the renderer draws through the shell.
pub(super) fn confirm_prompt_rows(area: Rect, prompt: &ConfirmPrompt) -> u16 {
    super::rows::decision_rows(
        &confirm_prompt_header_lines(prompt),
        &prompt.body,
        &approval_footer(&prompt.scopes),
        super::rows::decision_inner_width(area),
    )
}

pub(super) fn render_confirm_prompt(
    f: &mut Frame,
    area: Rect,
    app: &AppState,
    prompt: &ConfirmPrompt,
    modal_scroll: u16,
) {
    let header_lines = confirm_prompt_header_lines(prompt);
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
    let header_lines = confirm_prompt_header_lines(prompt);
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

/// The dim line every approval prompt carries, so the popup explains itself even
/// when no origin is recorded.
fn approval_subtitle(origin: Option<&str>, explanation: &str) -> Vec<Line<'static>> {
    let mut subtitle = origin_subtitle(origin);
    subtitle.push(Line::from(Span::styled(
        explanation.to_string(),
        theme::dim(),
    )));
    subtitle
}

pub(super) fn permission_prompt(command: &str, origin: Option<&str>) -> ConfirmPrompt {
    ConfirmPrompt {
        title: "Permission",
        heading: Line::from(Span::styled(
            "Allow this command to run?",
            theme::label(theme::palette().tool),
        )),
        subtitle: approval_subtitle(
            origin,
            "Runs exactly this command, then returns to the conversation.",
        ),
        body: vec![Line::from(Span::styled(
            command.to_string(),
            theme::body(theme::palette().text),
        ))],
        scopes: ApprovalScopes { project: true },
    }
}

pub(super) fn sandbox_escalation_prompt(
    command: &str,
    origin: Option<&str>,
    kind: crate::interaction::SandboxEscalationKind,
) -> ConfirmPrompt {
    let mut subtitle = approval_subtitle(
        origin,
        "Bypasses filesystem/network confinement for this one run.",
    );
    if kind == crate::interaction::SandboxEscalationKind::CommandAndSandbox {
        subtitle.push(Line::from(Span::styled(
            "This single decision approves both the command and its sandbox escape.",
            theme::body(theme::palette().todo),
        )));
    }
    ConfirmPrompt {
        title: "Sandbox escape",
        heading: Line::from(Span::styled(
            if kind == crate::interaction::SandboxEscalationKind::CommandAndSandbox {
                "⚠ Allow this command to run OUTSIDE the sandbox?"
            } else {
                "⚠ Run this command OUTSIDE the sandbox?"
            },
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
    let mut subtitle = approval_subtitle(origin, "Approving stores a rule for this host.");
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
            format!("from {server} · results come back as untrusted data"),
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
            format!("fires on {event} · {action_kind} · runs with your user permissions"),
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

    let body_lines = question_prompt_body(
        options,
        multiple,
        cursor,
        selected,
        theme::frame(String::new(), true).inner(area).width,
    )
    .lines;

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
    vec![footer_hint_line(question_hints(multiple))]
}

/// Builds the fixed header above a question's scrollable options: the one dim
/// line explaining what the popup is for, then the question itself as the accent
/// heading. Rendering and scroll metrics share this builder, and the shell owns
/// the blank row below it.
pub(super) fn question_prompt_header_lines(
    prompt: &str,
    origin: Option<&str>,
) -> Vec<Line<'static>> {
    let mut header_lines = if origin.is_some() {
        origin_subtitle(origin)
    } else {
        vec![modal_subtitle("The turn waits for this answer.")]
    };
    header_lines.push(modal_heading(prompt.to_string()));
    header_lines
}

/// Wrapped rows the option body occupies at `inner_width` — measured through
/// [`question_prompt_body`], the same builder the renderer draws.
pub(super) fn question_prompt_body_height(
    options: &[crate::interaction::QuestionOption],
    multiple: bool,
    inner_width: u16,
) -> u16 {
    let wrap = inner_width.max(1) as usize;
    question_prompt_body(options, multiple, 0, &[], inner_width)
        .lines
        .iter()
        .fold(0u16, |rows, line| {
            rows.saturating_add(wrapped_line_count(line, wrap))
        })
}

/// Total rows the question prompt draws at `inner_width`: the wrapped header,
/// the option body and the hint footer, plus the shell chrome. Sizing and
/// drawing agree because both walk these builders.
pub(super) fn question_prompt_rows(
    prompt: &str,
    origin: Option<&str>,
    options: &[crate::interaction::QuestionOption],
    multiple: bool,
    inner_width: u16,
) -> u16 {
    let wrap = inner_width.max(1) as usize;
    let header = wrapped_lines_height(&question_prompt_header_lines(prompt, origin), wrap) as u16;
    let body = question_prompt_body_height(options, multiple, inner_width);
    let footer = wrapped_lines_height(&question_prompt_footer_lines(multiple), wrap) as u16;
    shell_modal_rows(header, body, footer)
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
    width: u16,
) -> QuestionPromptBody {
    // One label column for every option, so the descriptions line up down the
    // list; `pad_ascii` pads by display width, so CJK labels align too.
    let label_width = options
        .iter()
        .map(|option| UnicodeWidthStr::width(option.label.as_str()))
        .max()
        .unwrap_or(0);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut option_offsets: Vec<usize> = Vec::with_capacity(options.len() + 1);
    for (idx, opt) in options.iter().enumerate() {
        if idx > 0 {
            // One blank row between blocks: this is a decision to read, not a
            // wall of labels.
            lines.push(Line::from(""));
        }
        option_offsets.push(lines.len());
        let label = pad_ascii(&opt.label, label_width);
        let description = (!opt.description.trim().is_empty()).then_some(opt.description.as_str());
        lines.extend(row_block(
            &RowBlock {
                selected: idx == cursor,
                checkbox: multiple.then(|| selected.get(idx).copied().unwrap_or(false)),
                label: &label,
                description,
            },
            width,
        ));
    }
    // The trailing sentinel makes `option_offsets[i+1]` the end of option i.
    option_offsets.push(lines.len());
    QuestionPromptBody {
        lines,
        option_offsets,
    }
}
