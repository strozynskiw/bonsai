use super::markdown::*;
use super::*;

pub(crate) fn tool_result_body(text: &str) -> String {
    if let Some(body) = tool_result_preview(text) {
        body
    } else if text.trim().is_empty() {
        "(empty result)".to_string()
    } else {
        text.to_string()
    }
}

pub(super) fn tool_result_preview(text: &str) -> Option<String> {
    let mut parts = text.lines();
    let first = parts.next()?;
    if let Some(count) = first.strip_prefix("[Output truncated: ") {
        return Some(format!(
            "{}\n[truncated] {}",
            parts.collect::<Vec<_>>().join("\n"),
            count.trim_end_matches(']').trim()
        ));
    }
    None
}

/// Truncate `text` to `width` display cells and pad with spaces so the result
/// occupies exactly `width` cells. Wide (CJK/emoji) graphemes count as 2 cells
/// and are never split, so columns stay aligned.
pub(super) fn fill(text: &str, width: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    let mut value = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let w = UnicodeWidthStr::width(grapheme);
        if used + w > width {
            break;
        }
        value.push_str(grapheme);
        used += w;
    }
    if used < width {
        value.push_str(&" ".repeat(width - used));
    }
    value
}

/// The welcome screen shown when a view's transcript is empty. The coding agent
/// gets a bonsai blurb plus the few commands worth surfacing first; the Plan
/// view gets the plan-authoring commands, which only make sense there. Until a
/// provider is authorized nothing else works, so that state leads with the two
/// ways to get one instead of generic controls.
pub(super) fn empty_state(app: &AppState) -> Vec<Line<'static>> {
    match app.view {
        View::Agent if !app.has_authorized_provider => no_provider_welcome(),
        View::Agent => agent_welcome(),
        View::Plan => plan_welcome(),
    }
}

/// The first-launch state: no provider is authorized yet, so the model cannot
/// be reached. Everything on screen funnels into fixing exactly that.
fn no_provider_welcome() -> Vec<Line<'static>> {
    let mut lines = welcome_heading(
        "Welcome to bonsai",
        "No provider is authorized yet — connect one to start working.",
    );
    welcome_section(
        &mut lines,
        "Use a provider account",
        &[(
            "/authorize",
            "pick a built-in provider and add its API key or login",
        )],
    );
    welcome_section(
        &mut lines,
        "Use a local or custom server",
        &[(
            "/wizard",
            "add LM Studio, Ollama, or any OpenAI/Anthropic-compatible endpoint",
        )],
    );
    welcome_section(
        &mut lines,
        "Then",
        &[
            ("/model", "pick the model to work with"),
            ("/help", "see everything else"),
        ],
    );
    lines
}

fn agent_welcome() -> Vec<Line<'static>> {
    let mut lines = welcome_heading(
        "Welcome to bonsai",
        "A fast, token-efficient coding agent in a single Rust binary.",
    );
    welcome_section(
        &mut lines,
        "Quick start",
        &[
            ("/authorize", "choose a provider to authorize"),
            ("/model", "switch model on the current provider"),
        ],
    );
    welcome_section(
        &mut lines,
        "Controls",
        &[
            (
                "/mode",
                "set approval level: ask · conservative · balanced · auto-accept · yolo",
            ),
            ("/sandbox", "toggle OS-level command confinement"),
            ("/review", "review pending changes"),
        ],
    );
    welcome_section(
        &mut lines,
        "Session",
        &[
            ("/sessions", "list saved sessions"),
            ("/new", "start a fresh session"),
        ],
    );
    lines
}

fn plan_welcome() -> Vec<Line<'static>> {
    let mut lines = welcome_heading("Plan mode", "Draft the plan, then build.");
    welcome_section(
        &mut lines,
        "Plan commands",
        &[
            ("/start", "build"),
            ("/save", "save"),
            ("/export", "export file"),
            ("Shift+Tab", "coding agent"),
        ],
    );
    lines
}

/// A blank line, the bold accent title, and a muted one-line blurb beneath it.
fn welcome_heading(title: &str, blurb: &str) -> Vec<Line<'static>> {
    vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(title.to_string(), md_bold(theme::palette().border_active)),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(blurb.to_string(), md(theme::palette().muted)),
        ]),
    ]
}

/// Push a blank spacer, an accent section heading, and one aligned
/// `command  description` row per entry.
fn welcome_section(lines: &mut Vec<Line<'static>>, title: &str, rows: &[(&str, &str)]) {
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(title.to_string(), md_bold(theme::palette().border_active)),
    ]));
    for (cmd, what) in rows {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{cmd:<14}"), md_bold(theme::palette().user)),
            Span::styled((*what).to_string(), md(theme::palette().dim)),
        ]));
    }
}
