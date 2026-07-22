use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::tui::app::AppState;
use crate::tui::{layout, theme, widgets};

pub fn draw(f: &mut Frame, app: &AppState) {
    let area = f.area();
    f.render_widget(Paragraph::new("").style(theme::base()), area);

    let regions = layout::split(area, app.surface());
    draw_header(f, regions.header, app, regions.show_input);
    widgets::transcript::render(f, regions.main, app);
    widgets::transcript::render_jump_to_bottom(f, regions.main, app);

    if let Some(plan) = regions.plan {
        widgets::plan::render(f, plan, app);
    }

    if let Some(sidebar) = regions.sidebar {
        widgets::sidebar::render(f, sidebar, app);
    }

    if regions.show_input {
        widgets::input::render(f, regions.input, regions.input_footer, app);
        widgets::input::set_cursor(f, regions.input, app);
    }
    widgets::modal::render(f, area, app);
}

fn draw_header(f: &mut Frame, area: Rect, app: &AppState, show_input: bool) {
    if area.is_empty() {
        return;
    }
    f.render_widget(
        Paragraph::new(header_line(app, show_input, area.width as usize)).style(theme::base()),
        area,
    );
}

fn header_line(app: &AppState, show_input: bool, width: usize) -> Line<'static> {
    // The brand sits at the top-left. Status normally lives in the composer's
    // helper row, near the prompt; on short terminals where the composer is
    // hidden, the status falls back to row 0 so it stays visible.
    let mut spans = header_left_spans(app, show_input, width);

    // The context chip is the single, always-visible context readout. It stays
    // pinned to the top-right corner, so the left side can absorb the compact
    // workspace badges without losing the usage gauge.
    if let Some(report) = &app.latest_context_report {
        let title_width = spans_width(&spans);
        let label_width = context_chip_label(report, width).chars().count();
        if let Some(offset) = context_chip_x_offset(title_width, label_width, width) {
            spans.push(Span::styled(
                " ".repeat(offset.saturating_sub(title_width)),
                theme::base(),
            ));
            spans.extend(context_chip_spans(report, width));
        }
    }

    Line::from(spans)
}

/// The header context chip rendered as a compact fill bar: the leading portion
/// (proportional to context usage) carries the pressure color, the trailing
/// "empty" portion sits on the mid-tone `border` color — deliberately lighter
/// than `panel_dark` so the unfilled track reads as a bar rather than a dark
/// void. As the single context readout, the chip doubles as a usage gauge.
fn context_chip_spans(report: &crate::agent::ContextReport, width: usize) -> Vec<Span<'static>> {
    let p = theme::palette();
    let label = context_chip_label(report, width);
    let total = label.chars().count();
    let percent = crate::context_view::telemetry::context_percent(report).min(100);
    let filled = percent.saturating_mul(total) / 100;
    let fill_color = context_chip_color(report);

    let mut filled_text = String::new();
    let mut empty_text = String::new();
    for (idx, ch) in label.chars().enumerate() {
        if idx < filled {
            filled_text.push(ch);
        } else {
            empty_text.push(ch);
        }
    }

    let mut spans = Vec::with_capacity(2);
    if !filled_text.is_empty() {
        spans.push(Span::styled(
            filled_text,
            Style::default()
                .fg(p.bg)
                .bg(fill_color)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if !empty_text.is_empty() {
        spans.push(Span::styled(
            empty_text,
            Style::default()
                .fg(p.muted)
                .bg(blend_toward_dark(p.border, p.panel_dark, 25)),
        ));
    }
    spans
}

/// Blend `base` toward `dark` by `pct` percent (0 = pure base, 100 = pure dark).
fn blend_toward_dark(
    base: ratatui::style::Color,
    dark: ratatui::style::Color,
    pct: u8,
) -> ratatui::style::Color {
    match (base, dark) {
        (ratatui::style::Color::Rgb(br, bg, bb), ratatui::style::Color::Rgb(dr, dg, db)) => {
            let lerp = |b: u8, d: u8| -> u8 {
                let diff = d as i16 - b as i16;
                (b as i16 + diff * pct as i16 / 100) as u8
            };
            ratatui::style::Color::Rgb(lerp(br, dr), lerp(bg, dg), lerp(bb, db))
        }
        // Non-RGB (256-color/monochrome adapted palettes): blending isn't
        // possible, so use `dark` — a panel-anchored background — rather than
        // `base`, which is a foreground/accent that would read as a contrast hazard.
        _ => dark,
    }
}

fn header_prefix_spans(app: &AppState, show_input: bool) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled("  ", theme::base())];
    if !show_input {
        spans.extend(header_status_pill(app));
    }
    spans
}

/// Columns occupied by the header's left side (brand + session id + compact
/// workspace badges), before the right-aligned context chip.
fn header_title_width(app: &AppState, show_input: bool, width: usize) -> usize {
    let spans = header_left_spans(app, show_input, width);
    spans_width(&spans)
}

/// Left column of the context chip within a header of `width`. Pins the chip
/// flush against the right edge, but clamps it so it always keeps a 3-space gap
/// after the title; `None` when the title leaves no room for the chip at all.
/// Shared by [`header_line`] (filler pad) and [`header_context_chip_rect`]
/// (mouse hit-testing) so the drawn chip and its click target never drift.
fn context_chip_x_offset(title_width: usize, label_width: usize, width: usize) -> Option<usize> {
    let min_offset = title_width.saturating_add(3);
    if min_offset >= width {
        return None;
    }
    let right_offset = width.saturating_sub(label_width);
    Some(right_offset.max(min_offset))
}

pub(crate) fn header_context_chip_rect(
    area: Rect,
    app: &AppState,
    show_input: bool,
) -> Option<Rect> {
    if area.is_empty() {
        return None;
    }
    let report = app.latest_context_report.as_ref()?;
    let title_width = header_title_width(app, show_input, area.width as usize);
    let label = context_chip_label(report, area.width as usize);
    let label_width = label.chars().count();
    let offset = context_chip_x_offset(title_width, label_width, area.width as usize)?;
    let x = area.x.saturating_add(offset as u16);
    if x >= area.right() {
        return None;
    }
    Some(Rect {
        x,
        y: area.y,
        width: (label_width as u16).min(area.right().saturating_sub(x)),
        height: 1,
    })
}

fn context_chip_label(report: &crate::agent::ContextReport, width: usize) -> String {
    crate::context_view::telemetry::CostTelemetry::from_report(report).header_label(report, width)
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

fn header_left_spans(app: &AppState, show_input: bool, width: usize) -> Vec<Span<'static>> {
    let mut spans = header_prefix_spans(app, show_input);
    spans.push(Span::styled("✦ bonsai", theme::title()));
    spans.extend(header_session_spans(app));
    let available_width = app.latest_context_report.as_ref().map_or(width, |report| {
        width.saturating_sub(context_chip_label(report, width).chars().count() + 3)
    });
    append_header_workspace_spans(&mut spans, app, available_width);
    spans
}

fn header_session_spans(app: &AppState) -> Vec<Span<'static>> {
    let Some(label) = header_session_label(app) else {
        return Vec::new();
    };
    vec![
        Span::styled("   ", theme::base()),
        Span::styled(truncate_header_session(&label, 56), theme::dim()),
    ]
}

fn header_session_label(app: &AppState) -> Option<String> {
    let session_id = app.current_session_id?;
    let status = app.current_session_status.label();
    Some(match app.view {
        crate::tui::event::View::Plan => format!("#{session_id}"),
        crate::tui::event::View::Agent => format!("#{session_id} {status}"),
    })
}

fn append_header_workspace_spans(spans: &mut Vec<Span<'static>>, app: &AppState, width: usize) {
    let mut remaining_width = width.saturating_sub(spans_width(spans));
    let cwd = header_cwd_label(&app.cwd);
    let branch = header_branch_label(app.branch.as_deref());
    let elapsed = app.elapsed_label();
    let elapsed_width = elapsed.chars().count().min(12);
    let elapsed_badge_width = header_badge_width(elapsed_width);

    match (&cwd, &branch) {
        (Some(cwd), Some(branch)) => {
            let available_width =
                remaining_width.saturating_sub(header_badge_width(0) * 2 + elapsed_badge_width);
            let (cwd_width, branch_width) = header_workspace_label_widths(
                cwd.chars().count(),
                branch.chars().count(),
                available_width,
            );
            if cwd_width > 0 {
                append_header_badge(spans, "⌂", cwd, cwd_width);
            }
            if branch_width > 0 {
                append_header_badge(spans, "⎇", branch, branch_width);
            }
        }
        (Some(cwd), None) => {
            let label_width = remaining_width
                .saturating_sub(header_badge_width(0) + elapsed_badge_width)
                .min(cwd.chars().count());
            if label_width > 0 {
                append_header_badge(spans, "⌂", cwd, label_width);
            }
        }
        (None, Some(branch)) => {
            let label_width = remaining_width
                .saturating_sub(header_badge_width(0) + elapsed_badge_width)
                .min(branch.chars().count());
            if label_width > 0 {
                append_header_badge(spans, "⎇", branch, label_width);
            }
        }
        (None, None) => {}
    }
    remaining_width = width.saturating_sub(spans_width(spans));

    if remaining_width >= elapsed_badge_width {
        append_header_badge(spans, "⏱", &elapsed, elapsed_width);
    }
}

fn header_workspace_label_widths(
    cwd_width: usize,
    branch_width: usize,
    available_width: usize,
) -> (usize, usize) {
    if cwd_width.saturating_add(branch_width) <= available_width {
        return (cwd_width, branch_width);
    }

    let cwd_allocation = cwd_width.min(available_width / 2);
    let branch_allocation = branch_width.min(available_width.saturating_sub(cwd_allocation));
    let remaining_width = available_width.saturating_sub(cwd_allocation + branch_allocation);
    (
        cwd_allocation + remaining_width.min(cwd_width.saturating_sub(cwd_allocation)),
        branch_allocation,
    )
}

fn header_badge_width(label_width: usize) -> usize {
    5 + label_width
}

fn append_header_badge(
    spans: &mut Vec<Span<'static>>,
    icon: &'static str,
    label: &str,
    label_width: usize,
) {
    spans.extend([
        Span::styled("   ", theme::base()),
        Span::styled(icon, theme::muted()),
        Span::styled(" ", theme::dim()),
        Span::styled(truncate_header_workspace(label, label_width), theme::dim()),
    ]);
}

fn header_cwd_label(cwd: &str) -> Option<String> {
    let path = std::path::Path::new(cwd);
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .or_else(|| (!cwd.trim().is_empty()).then(|| cwd.to_string()))
}

fn header_branch_label(branch: Option<&str>) -> Option<String> {
    branch.and_then(|branch| {
        let branch = branch.trim();
        (!branch.is_empty()).then(|| branch.to_string())
    })
}

fn truncate_header_workspace(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        label.to_string()
    } else {
        let truncated: String = label.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn truncate_header_session(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        label.to_string()
    } else {
        let truncated: String = label.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn context_chip_color(report: &crate::agent::ContextReport) -> ratatui::style::Color {
    let percent = crate::context_view::telemetry::context_percent(report);
    let p = theme::palette();
    if percent >= 95 {
        p.error
    } else if percent >= 85 {
        p.edit
    } else if percent >= 70 {
        p.todo
    } else {
        p.success
    }
}

/// Bare status indicator for the composer's meta line: a single glyph (spinner
/// frame when busy, filled dot when idle) carrying the state color directly,
/// with no surrounding pill or label. The composer's meta line assembles the
/// rest of the left side (agent name, model, autonomy, sandbox) around it.
pub(crate) fn status_dot_spans(app: &AppState) -> Vec<Span<'static>> {
    let p = theme::palette();
    if app.task_state.is_busy() {
        vec![Span::styled(
            theme::spinner_frame(app.animation_tick()),
            theme::body(p.todo),
        )]
    } else {
        vec![Span::styled("●", theme::body(p.success))]
    }
}

/// Status color for the approval level: red for `yolo` (no guardrails), amber
/// for every other shown level (the levels above the quiet `ask` default).
fn approval_color(level: crate::tool::ApprovalLevel) -> ratatui::style::Color {
    match level {
        crate::tool::ApprovalLevel::Yolo => theme::palette().error,
        _ => theme::palette().todo,
    }
}

/// Composer meta-line marker for the current approval level. Only rendered above
/// `ask` (callers gate on `!is_ask()`), so the default stays quiet while higher
/// autonomy levels are always visible.
pub(crate) fn approval_marker_span(level: crate::tool::ApprovalLevel) -> Span<'static> {
    Span::styled(
        level.label(),
        theme::body(approval_color(level)).add_modifier(ratatui::style::Modifier::BOLD),
    )
}

pub(crate) fn smol_marker_span() -> Span<'static> {
    Span::styled(
        "smol",
        theme::body(theme::palette().edit).add_modifier(ratatui::style::Modifier::BOLD),
    )
}

pub(crate) fn serenity_marker_span() -> Span<'static> {
    Span::styled(
        "serenity",
        theme::body(theme::palette().muted).add_modifier(ratatui::style::Modifier::BOLD),
    )
}

/// Meta-line marker shown while mouse capture is released ("copy mode") — a cue
/// that in-app scroll/click are off and the terminal owns click-drag selection.
pub(crate) fn select_mode_marker_span() -> Span<'static> {
    Span::styled(
        "⊙ select",
        theme::body(theme::palette().todo).add_modifier(ratatui::style::Modifier::BOLD),
    )
}

fn smol_status_pill() -> Span<'static> {
    Span::styled(" smol ", theme::pill(theme::palette().edit))
}

fn serenity_status_pill() -> Span<'static> {
    Span::styled(" serenity ", theme::pill(theme::palette().muted))
}

/// Header-fallback pill for a non-`ask` approval level — the deviation worth
/// flagging on a compact header.
fn approval_status_pill(level: crate::tool::ApprovalLevel) -> Span<'static> {
    Span::styled(
        format!(" {} ", level.label()),
        theme::pill(approval_color(level)),
    )
}

/// Compact label for the OS sandbox posture.
fn sandbox_label(sandbox: &crate::sandbox::CommandSandbox) -> &'static str {
    if sandbox.is_active() && sandbox.deny_network() {
        "sandbox no-net"
    } else if sandbox.is_active() {
        "sandbox on"
    } else if !sandbox.backend().is_available() {
        "sandbox none"
    } else {
        "sandbox off"
    }
}

fn sandbox_marker_style(sandbox: &crate::sandbox::CommandSandbox) -> Style {
    if sandbox.is_active() {
        theme::body(theme::palette().success).add_modifier(Modifier::BOLD)
    } else if !sandbox.backend().is_available() {
        theme::dim()
    } else {
        theme::composer_meta()
    }
}

/// Composer-meta marker for the current OS sandbox posture.
pub(crate) fn sandbox_marker_span(sandbox: &crate::sandbox::CommandSandbox) -> Span<'static> {
    Span::styled(sandbox_label(sandbox), sandbox_marker_style(sandbox))
}

/// Header-fallback pill for the OS sandbox posture (short terminals).
fn sandbox_status_pill(sandbox: &crate::sandbox::CommandSandbox) -> Span<'static> {
    let color = if sandbox.is_active() {
        theme::palette().success
    } else {
        theme::palette().border
    };
    Span::styled(format!(" {} ", sandbox_label(sandbox)), theme::pill(color))
}

/// Full status pill used as a fallback in the top header on short terminals
/// where the composer is hidden. Keeps the labeled pill (spinner + word, or
/// `● ready`) so the header stays self-explanatory without the meta line.
pub(crate) fn header_status_pill(app: &AppState) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if let Some(notice) = &app.shutdown_notice {
        spans.push(Span::styled(
            format!(" {notice} "),
            theme::pill(theme::palette().edit),
        ));
    } else if let Some(hint) = &app.session_hint {
        // Surface the actionable interrupted-session hint even on short terminals
        // where the composer (and its meta line) is hidden, until it expires.
        spans.push(Span::styled(
            format!(" {} ", hint.text),
            theme::pill(theme::palette().todo),
        ));
    } else if let Some(toast) = &app.session_toast {
        // Resume confirmations are transient UI, not transcript rows. When the
        // composer is hidden, surface them in the header so they remain visible.
        spans.push(Span::styled(
            format!(" {} ", toast.text),
            theme::pill(theme::palette().muted),
        ));
    } else if app.task_state.is_busy() {
        spans.push(Span::styled(
            format!(
                " {} {} ",
                theme::spinner_frame(app.animation_tick()),
                app.task_state.label()
            ),
            theme::pill(theme::palette().todo),
        ));
    } else {
        spans.push(Span::styled(
            " ● ready ",
            theme::pill(theme::palette().success),
        ));
    }
    if !app.approval_level.is_ask() {
        spans.push(Span::styled("  ", theme::base()));
        spans.push(approval_status_pill(app.approval_level));
    }
    if app.smol_mode {
        spans.push(Span::styled("  ", theme::base()));
        spans.push(smol_status_pill());
    }
    if app.serenity_mode {
        spans.push(Span::styled("  ", theme::base()));
        spans.push(serenity_status_pill());
    }
    if let Some(sandbox) = app.sandbox.as_ref() {
        spans.push(Span::styled("  ", theme::base()));
        spans.push(sandbox_status_pill(sandbox));
    }
    // The phase label only applies while busy, and is suppressed once a
    // shutdown notice is armed so the header doesn't read both the exit
    // pill and a stale "Working: ..." line.
    if app.task_state.is_busy()
        && app.shutdown_notice.is_none()
        && let Some(phase) = &app.current_phase
    {
        spans.push(Span::styled("  ", theme::base()));
        spans.push(Span::styled(truncate_phase(phase, 48), theme::dim()));
    }
    spans
}

pub(crate) fn truncate_phase(phase: &str, max_chars: usize) -> String {
    if phase.chars().count() <= max_chars {
        phase.to_string()
    } else {
        let truncated: String = phase.chars().take(max_chars).collect();
        format!("{truncated}…")
    }
}

/// Compute the terminal tab/window title string.
///
/// Emitted via OSC each frame so the VS Code (and any other) terminal tab
/// always reflects the current session and agent state:
///   `[!] bonsai · <title>` — modal needs user attention
///   `⠙ bonsai · <title>`  — agent running (animated spinner)
///   `bonsai · <title>`    — idle with a session title
///   `bonsai`              — idle, no session title
pub fn terminal_title(app: &AppState) -> String {
    use crate::tui::event::ModalKind;

    let needs_attention = app.modal.as_ref().is_some_and(|m| {
        matches!(
            m,
            ModalKind::PermissionPrompt { .. }
                | ModalKind::SandboxEscalationPrompt { .. }
                | ModalKind::WebDomainPrompt { .. }
                | ModalKind::ExtensionToolPrompt { .. }
                | ModalKind::HookTrustPrompt { .. }
                | ModalKind::ApiKeyPrompt { .. }
                | ModalKind::QuestionPrompt { .. }
                | ModalKind::AuthorizeProviderPicker { .. }
                | ModalKind::UnauthorizeProviderPicker { .. }
                | ModalKind::UnauthorizeConfirm { .. }
        )
    });

    let prefix = if needs_attention {
        "[!] ".to_string()
    } else if app.task_state.is_busy() {
        format!("{} ", theme::spinner_frame(app.animation_tick() / 2))
    } else {
        String::new()
    };

    match app.current_session_title() {
        Some(title) => format!("{}bonsai · {}", prefix, title),
        None => format!("{}bonsai", prefix),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::event::{AppAction, UiEvent};

    #[test]
    fn terminal_title_tracks_idle_and_consecutive_busy_frames() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "bonsai".to_string(),
            None,
        );
        app.current_session_summary = "Polish terminal title".to_string();

        assert_eq!(terminal_title(&app), "bonsai · Polish terminal title");

        app.task_state = crate::tui::event::TaskState::Running;
        app.tick = 0;
        assert_eq!(terminal_title(&app), "⠋ bonsai · Polish terminal title");

        app.tick = 2;
        assert_eq!(terminal_title(&app), "⠙ bonsai · Polish terminal title");
    }

    #[test]
    fn reduced_motion_freezes_busy_terminal_title() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "bonsai".to_string(),
            None,
        );
        app.current_session_summary = "Accessible run".to_string();
        app.task_state = crate::tui::event::TaskState::Running;
        app.reduced_motion = true;

        app.tick = 0;
        let first = terminal_title(&app);
        app.tick = 42;
        assert_eq!(terminal_title(&app), first);
        assert_eq!(first, "⠋ bonsai · Accessible run");
    }

    #[test]
    fn terminal_title_prioritizes_attention_over_busy_spinner() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "bonsai".to_string(),
            None,
        );
        app.current_session_summary = "Approve command".to_string();
        app.task_state = crate::tui::event::TaskState::Running;
        app.modal = Some(crate::tui::event::ModalKind::PermissionPrompt {
            request_id: 1,
            command: "cargo test".to_string(),
            origin: None,
        });

        assert_eq!(terminal_title(&app), "[!] bonsai · Approve command");
    }

    #[test]
    fn blend_toward_dark_falls_back_to_dark_for_non_rgb() {
        use ratatui::style::Color;
        // RGB blend interpolates as usual.
        assert_eq!(
            blend_toward_dark(Color::Rgb(100, 100, 100), Color::Rgb(0, 0, 0), 50),
            Color::Rgb(50, 50, 50)
        );
        // Non-RGB (256-color-adapted) inputs can't blend — anchor on `dark`, not
        // `base`, so a foreground accent never lands as a background.
        assert_eq!(
            blend_toward_dark(Color::Indexed(200), Color::Indexed(233), 50),
            Color::Indexed(233)
        );
    }

    fn context_report(tokens: usize, budget: usize) -> crate::agent::ContextReport {
        use crate::agent::{ContextEntry, ContextRole};
        crate::agent::ContextReport {
            budget_tokens: budget,
            entries: vec![
                ContextEntry {
                    role: ContextRole::System,
                    tokens: tokens / 4,
                    text: "system".to_string(),
                },
                ContextEntry {
                    role: ContextRole::User,
                    tokens: tokens / 4,
                    text: "user".to_string(),
                },
                ContextEntry {
                    role: ContextRole::Assistant,
                    tokens: tokens / 4,
                    text: "assistant".to_string(),
                },
                ContextEntry {
                    role: ContextRole::Tool,
                    tokens: tokens.saturating_sub((tokens / 4) * 3),
                    text: "tool".to_string(),
                },
            ],
            last_prompt_tokens: Some(7_900),
            last_completion_tokens: Some(640),
            session_prompt_tokens: 12_500,
            session_completion_tokens: 1_200,
            ..Default::default()
        }
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    /// Fills `app` with one of every transcript component kind, plus sidebar
    /// todos and a plan canvas, so rendering tests exercise populated state.
    fn populate_sample(app: &mut AppState) {
        use crate::diff::{DiffHunk, DiffLine, DiffLineKind, DiffStatus, FileDiff};
        use crate::todo::{TodoItem, TodoStatus};
        use crate::tui::app::{ToolActivity, ToolStatus, TranscriptItem};
        use crate::tui::event::{CommandOutputKind, UiError};
        use std::time::{Duration, Instant};

        let now = Instant::now();
        let started = now.checked_sub(Duration::from_millis(12)).unwrap_or(now);
        app.transcript.extend([
            TranscriptItem::CommandOutput {
                kind: CommandOutputKind::Status,
                text: "Sample data — nothing was sent to a model.".to_string(),
            },
            TranscriptItem::UserMessage {
                text: "Show me everything bonsai can render.".to_string(),
            },
            TranscriptItem::ReasoningSummary {
                text: "I will read a file, apply an edit, and summarize.".to_string(),
            },
            TranscriptItem::TodoUpdate {
                text: "Render the markdown showcase".to_string(),
            },
            TranscriptItem::ToolActivity(ToolActivity {
                id: "sample-read".to_string(),
                name: "read".to_string(),
                arguments: r#"{"file_path":"src/main.rs"}"#.to_string(),
                status: ToolStatus::Succeeded,
                result: Some("120 lines read".to_string()),
                diff: None,
                started_at: started,
                finished_at: Some(now),
            }),
            TranscriptItem::ToolActivity(ToolActivity {
                id: "sample-edit".to_string(),
                name: "edit".to_string(),
                arguments: r#"{"file_path":"src/lib.rs"}"#.to_string(),
                status: ToolStatus::Succeeded,
                result: Some("Edited src/lib.rs (+2 / -1)".to_string()),
                diff: Some(FileDiff {
                    path: "src/lib.rs".to_string(),
                    status: DiffStatus::Modified,
                    hunks: vec![DiffHunk {
                        old_start: 4,
                        new_start: 4,
                        lines: vec![
                            DiffLine {
                                kind: DiffLineKind::Context,
                                content: "fn main() {".to_string(),
                                old_line: Some(4),
                                new_line: Some(4),
                            },
                            DiffLine {
                                kind: DiffLineKind::Removed,
                                content: "    println!(\"hi\");".to_string(),
                                old_line: Some(5),
                                new_line: None,
                            },
                            DiffLine {
                                kind: DiffLineKind::Added,
                                content: "    println!(\"hello\");".to_string(),
                                old_line: None,
                                new_line: Some(5),
                            },
                        ],
                    }],
                    truncated: false,
                    old_size: Some(40),
                    new_size: 41,
                    added_lines: 2,
                    removed_lines: 1,
                    additional_files: Box::default(),
                }),
                started_at: started,
                finished_at: Some(now),
            }),
            TranscriptItem::ToolActivity(ToolActivity {
                id: "sample-bash".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"cargo test"}"#.to_string(),
                status: ToolStatus::Running,
                result: None,
                diff: None,
                started_at: now,
                finished_at: None,
            }),
            TranscriptItem::Edit {
                text: "src/lib.rs — tweaked greeting (+2 / -1)".to_string(),
            },
            TranscriptItem::AssistantMessage {
                text: "# Sample\n\nPlain, **bold**, `code`, and a [link](https://ratatui.rs).\n\n\
                       - bullet one\n- bullet two\n\n| Col | Val |\n| --- | --- |\n| a | 1 |"
                    .to_string(),
            },
            TranscriptItem::Error {
                error: UiError::new(
                    "Provider error (sample)",
                    "429 Too Many Requests — retrying.",
                ),
            },
            TranscriptItem::CommandOutput {
                kind: CommandOutputKind::Error,
                text: "Unknown command.".to_string(),
            },
        ]);
        app.todo = vec![
            TodoItem {
                content: "Render the markdown showcase".to_string(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "Animate the running bash card".to_string(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                content: "Review the diff preview modal".to_string(),
                status: TodoStatus::Pending,
            },
        ];
        app.plan.edit().set_title("Sample plan");
        app.plan
            .edit()
            .set_section("Goal", "Exercise every plan renderer.");
        app.plan.edit().set_section(
            "Approach",
            "| Step | Area |\n| --- | --- |\n| 1 | view |\n| 2 | tests |",
        );
        app.plan.edit().add_task("Define the approach");
        app.plan.edit().add_task("Write the tests");
        app.plan.edit().check_task("Define the approach");
        app.active_tools = vec![("bash".to_string(), Instant::now())];
        app.scroll_to_bottom_current();
    }

    #[test]
    fn renders_empty_tui() {
        let backend = TestBackend::new(100, 28);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );

        terminal
            .draw(|frame| draw(frame, &app))
            .expect("empty TUI should render");
    }

    #[test]
    fn header_renders_compact_session_and_chat_title() {
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            Some("master".to_string()),
        );
        app.set_session_identity(
            crate::storage::SessionId::from_raw(42),
            "workspace",
            "Polish resume picker",
        );

        terminal
            .draw(|frame| draw(frame, &app))
            .expect("TUI should render");

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("✦ bonsai"));
        assert!(text.contains("#42 active"));
        assert!(text.contains("⌂"));
        assert!(text.contains("⎇ master"));
        assert!(text.contains("⏱"));
        assert!(
            text.contains("Chat · Polish resume picker"),
            "chat frame should carry the session title"
        );
    }

    #[test]
    fn header_workspace_badges_use_remaining_width() {
        let workspace = "workspace-with-a-descriptive-name";
        let branch = "feat/keep-the-whole-branch-name";
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            workspace.to_string(),
            Some(branch.to_string()),
        );
        app.set_session_identity(crate::storage::SessionId::from_raw(27), "", "");

        let text = header_line(&app, true, 120)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains(workspace), "{text}");
        assert!(text.contains(branch), "{text}");
    }

    #[test]
    fn header_renders_workspace_badges_in_plan_view() {
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            Some("main".to_string()),
        );
        app.set_session_identity(crate::storage::SessionId::from_raw(42), "", "");
        app.reduce(AppAction::SetView(crate::tui::event::View::Plan));

        let line = header_line(&app, true, 100);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("✦ bonsai"));
        assert!(text.contains("#42"));
        assert!(text.contains("⌂ workspace"));
        assert!(text.contains("⎇ main"));
        assert!(text.contains("⏱"));

        let narrow_line = header_line(&app, true, 22);
        let narrow_text = narrow_line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(narrow_text.contains("✦ bonsai"), "{narrow_text}");
        assert!(narrow_text.contains("#42"), "{narrow_text}");
        assert!(!narrow_text.contains("⌂") && !narrow_text.contains("⎇"));
        assert!(spans_width(&narrow_line.spans) <= 22, "{narrow_text}");
    }

    #[test]
    fn transcript_frame_keeps_plain_chat_title_in_plan_view() {
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.reduce(AppAction::SetView(crate::tui::event::View::Plan));

        terminal
            .draw(|frame| draw(frame, &app))
            .expect("plan view should render");

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Chat"));
        assert!(
            !text.contains("Chat ·"),
            "plan view chat pane should not carry the session title"
        );
    }

    #[test]
    fn fresh_tui_focuses_input_with_cursor_on_the_prompt() {
        let backend = TestBackend::new(100, 28);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );

        terminal
            .draw(|frame| draw(frame, &app))
            .expect("empty TUI should render");

        let area = terminal.backend().buffer().area;
        let regions = crate::tui::layout::split(area, crate::agent::PersonaView::Todo);
        assert!(regions.show_input, "input must be visible on a fresh start");

        let buffer = terminal.backend().buffer();
        let mut first_prompt: Option<(u16, u16)> = None;
        for y in regions.input.y..regions.input.y + regions.input.height {
            for x in regions.input.x..regions.input.x + regions.input.width {
                if buffer[(x, y)].symbol() == ">" {
                    first_prompt = Some((x, y));
                    break;
                }
            }
            if first_prompt.is_some() {
                break;
            }
        }
        let (prompt_x, prompt_y) = first_prompt.expect("input should render a '>' prompt");

        let cursor = terminal.get_cursor_position().unwrap();
        assert_eq!(
            cursor.x,
            prompt_x + 2,
            "terminal cursor should sit just past the prompt on start (cursor={cursor:?}, prompt=({prompt_x},{prompt_y}))"
        );
        assert_eq!(
            cursor.y, prompt_y,
            "terminal cursor should be on the input's first line (cursor={cursor:?}, prompt=({prompt_x},{prompt_y}))"
        );
    }

    #[test]
    fn typing_lands_in_the_visible_input() {
        let backend = TestBackend::new(100, 28);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );

        app.reduce(AppAction::InputChar('h'));
        app.reduce(AppAction::InputChar('i'));

        terminal
            .draw(|frame| draw(frame, &app))
            .expect("typed TUI should render");

        let area = terminal.backend().buffer().area;
        let regions = crate::tui::layout::split(area, crate::agent::PersonaView::Todo);
        let buffer = terminal.backend().buffer();
        let body_row = regions.input.y + 1;
        let mut rendered = String::new();
        for x in regions.input.x..regions.input.x + regions.input.width {
            rendered.push_str(buffer[(x, body_row)].symbol());
        }
        assert!(
            rendered.contains("> hi"),
            "typed text should appear in the input body, got {rendered:?}"
        );
    }

    #[test]
    fn exact_wrap_boundary_keeps_cursor_inside_input_body() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        let regions = crate::tui::layout::split(
            terminal.backend().buffer().area,
            crate::agent::PersonaView::Todo,
        );
        let content_width = crate::tui::widgets::input::body_content_width(regions.input) as usize;
        for _ in 0..content_width {
            app.reduce(AppAction::InputChar('a'));
        }

        terminal
            .draw(|frame| draw(frame, &app))
            .expect("wrapped input should render");

        let cursor = terminal.get_cursor_position().unwrap();
        let input_body_y = regions.input.y + 1;
        let continuation_y = input_body_y + 1;
        assert_eq!(
            cursor.y, continuation_y,
            "cursor at an exact wrap boundary must stay in the body, not the helper row"
        );

        let buffer = terminal.backend().buffer();
        let mut body_row = String::new();
        for x in regions.input.x..regions.input.x + regions.input.width {
            body_row.push_str(buffer[(x, continuation_y)].symbol());
        }
        assert!(
            body_row.contains("  "),
            "body should render the empty continuation prompt, got {body_row:?}"
        );
    }

    #[test]
    fn renders_populated_tui() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.reduce(AppAction::SubmitInput("hello".to_string()));
        app.reduce(AppAction::Agent(UiEvent::Thinking("planning".to_string())));
        app.reduce(AppAction::Agent(UiEvent::AssistantDelta("hi".to_string())));

        terminal
            .draw(|frame| draw(frame, &app))
            .expect("populated TUI should render");
    }

    #[test]
    fn renders_command_completion_popover() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.focus = crate::tui::event::Focus::Input;
        app.reduce(AppAction::InputChar('/'));
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("composer with completion popover should render");

        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in buffer.area.y..buffer.area.y + buffer.area.height {
            for x in buffer.area.x..buffer.area.x + buffer.area.width {
                rendered.push_str(buffer[(x, y)].symbol());
            }
            rendered.push('\n');
        }

        assert!(
            rendered.contains("Tab to accept"),
            "completion popover frame should be visible, got {rendered:?}"
        );
        assert!(
            rendered.contains("/authorize") && rendered.contains("/bonsai"),
            "bare slash should show possible commands, got {rendered:?}"
        );
        let visible_commands = ["/agents", "/authorize", "/autonomy", "/bonsai", "/bug"]
            .into_iter()
            .filter(|command| rendered.contains(command))
            .count();
        assert_eq!(
            visible_commands, 5,
            "completion popover should show five dense command rows without dynamic shortcuts, got {rendered:?}"
        );
    }

    #[test]
    fn renders_context_modal() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        use crate::agent::{
            ContextEntry, ContextInclusion, ContextNode, ContextNodeKind, ContextRole,
            ContextUsageReconciliation,
        };
        use crate::provider::{EstimateConfidence, TokenCounterKind};
        let report = crate::agent::ContextReport {
            budget_tokens: 120_000,
            entries: vec![
                ContextEntry {
                    role: ContextRole::System,
                    tokens: 2_000,
                    text: "You are a coding agent.\n\n# Project context\n- cwd: /repo".to_string(),
                },
                ContextEntry {
                    role: ContextRole::User,
                    tokens: 500,
                    text: "fix the bug in main.rs".to_string(),
                },
                ContextEntry {
                    role: ContextRole::Tool,
                    tokens: 5_000,
                    text: "file contents…".to_string(),
                },
            ],
            ledger: vec![
                ContextNode {
                    id: "tool-output-row".into(),
                    kind: ContextNodeKind::ToolsRoot,
                    inclusion: ContextInclusion::Included,
                    role: Some(ContextRole::Tool),
                    label: "Tool output row".to_string(),
                    tokens: 5_000,
                    chars: 4_000,
                    bytes: 4_000,
                    source: TokenCounterKind::Tiktoken,
                    confidence: EstimateConfidence::High,
                    preview: String::new(),
                    sources: Vec::new(),
                    children: Vec::new(),
                },
                ContextNode {
                    id: "tiktoken-row".into(),
                    kind: ContextNodeKind::ChatMessage,
                    inclusion: ContextInclusion::Included,
                    role: Some(ContextRole::User),
                    label: "Tiktoken row".to_string(),
                    tokens: 123,
                    chars: 456,
                    bytes: 789,
                    source: TokenCounterKind::Tiktoken,
                    confidence: EstimateConfidence::High,
                    preview: String::new(),
                    sources: Vec::new(),
                    children: Vec::new(),
                },
            ],
            estimate_source: TokenCounterKind::Tiktoken,
            estimate_confidence: EstimateConfidence::High,
            reconciliation: Some(ContextUsageReconciliation {
                estimated_prompt_tokens: 7_800,
                actual_prompt_tokens: 7_900,
                actual_completion_tokens: 640,
                provider_delta_tokens: 100,
                estimate_source: TokenCounterKind::Tiktoken,
                estimate_confidence: EstimateConfidence::High,
            }),
            last_prompt_tokens: Some(7_900),
            last_completion_tokens: Some(640),
            last_input_cache: Some(crate::provider::InputCacheUsage::new(1_975, 0, 7_900)),
            session_prompt_tokens: 12_500,
            session_completion_tokens: 1_200,
            session_input_cache: Some(crate::provider::InputCacheUsage::new(3_000, 500, 12_500)),
            ..Default::default()
        };
        app.reduce(AppAction::OpenModal(crate::tui::event::ModalKind::Context(
            Box::new(report),
        )));
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("context modal should render");

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("cache 25%"),
            "/ctx should render last-turn input cache hit rate"
        );
        assert!(
            !text.contains("session total"),
            "session totals moved to the Turns view; the header stays slim"
        );
        assert!(
            text.contains("drivers "),
            "/ctx should show the top context drivers on one line"
        );
        assert!(
            text.contains("tool outputs 5k"),
            "/ctx should attribute tool-output context separately, got {text:?}"
        );
        assert!(text.contains("123 tok"), "/ctx should keep token counts");
        assert!(
            text.contains("est tiktoken/high"),
            "/ctx should surface the estimate source and confidence"
        );
        assert!(!text.contains("chars"), "/ctx should hide character counts");
        assert!(!text.contains("bytes"), "/ctx should hide byte counts");
    }

    #[test]
    fn renders_context_modal_cache_unavailable() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        let report = crate::agent::ContextReport {
            budget_tokens: 120_000,
            last_prompt_tokens: Some(100),
            last_completion_tokens: Some(20),
            session_prompt_tokens: 100,
            session_completion_tokens: 20,
            ..Default::default()
        };
        app.reduce(AppAction::OpenModal(crate::tui::event::ModalKind::Context(
            Box::new(report),
        )));
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("context modal should render");

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("cache n/a"),
            "/ctx should render unavailable input cache metrics"
        );
    }

    #[test]
    fn header_chip_is_the_only_context_readout_when_sidebar_is_visible() {
        // The dedicated CONTEXT sidebar card is gone: context now lives solely
        // in the always-visible header chip, in both views, so the readout no
        // longer jumps location when the sidebar appears or you switch views.
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new("codex", "gpt-5.5".to_string(), "bonsai".to_string(), None);
        app.latest_context_report = Some(context_report(102_000, 120_000));

        terminal
            .draw(|frame| draw(frame, &app))
            .expect("context header chip should render");

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            !text.contains("CONTEXT"),
            "the standalone CONTEXT sidebar card should no longer render"
        );
        assert!(
            text.contains("ctx 85%"),
            "header chip should carry the context percentage even with the sidebar visible"
        );
        assert!(
            text.contains("TODO"),
            "todo card should now own the full sidebar"
        );
    }

    #[test]
    fn renders_header_context_chip_when_sidebar_is_hidden() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new("codex", "gpt-5.5".to_string(), "bonsai".to_string(), None);
        app.latest_context_report = Some(context_report(84_000, 120_000));

        terminal
            .draw(|frame| draw(frame, &app))
            .expect("header context chip should render");

        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("ctx 70%"),
            "header chip should render, got {text:?}"
        );
        assert!(!text.contains("CONTEXT"), "sidebar card should be hidden");
    }

    #[test]
    fn wide_context_chip_label_includes_unknown_cost() {
        let report = context_report(84_000, 120_000);

        let label = context_chip_label(&report, 130);

        assert!(
            label.contains("cost n/a"),
            "wide context chip should render unknown pricing as unavailable"
        );
    }

    #[test]
    fn renders_populated_transcript() {
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        populate_sample(&mut app);
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("populated content should render");
    }

    #[test]
    fn jump_to_latest_pill_appears_only_when_scrolled_up() {
        use crate::tui::app::TranscriptItem;

        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        for i in 0..60 {
            app.transcript.push(TranscriptItem::AssistantMessage {
                text: format!("reply number {i}"),
            });
        }

        // Caught up at the bottom (the default): no pill.
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        assert!(
            !buffer_text(terminal.backend().buffer()).contains("Ctrl+End"),
            "the jump-to-latest pill must stay hidden while auto-following the bottom"
        );

        // Scroll up: the pill appears with a count of the unseen replies.
        app.transcript_autoscroll = false;
        app.transcript_scroll = 0;
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(
            text.contains("new messages") && text.contains("Ctrl+End"),
            "scrolling up should surface the jump-to-latest pill, got:\n{text}"
        );
    }

    #[test]
    fn renders_plan_view_with_sample_canvas() {
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        populate_sample(&mut app);
        app.reduce(AppAction::SetView(crate::tui::event::View::Plan));
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("plan view should render");

        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(text.contains("Chat"), "chat pane should render its frame");
        assert!(text.contains("Plan ·"), "canvas frame should render");
        assert!(
            text.contains("Define the approach") && text.contains("Write the tests"),
            "plan view should surface sample plan content"
        );
    }

    #[test]
    fn visual_smoke_dump() {
        // Renders the sample showcase and dumps it for manual inspection:
        //   cargo test visual_smoke_dump -- --nocapture
        let backend = TestBackend::new(130, 90);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.3-codex".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        populate_sample(&mut app);
        app.task_state = crate::tui::event::TaskState::Running;
        app.tick = 3;
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("sample TUI should render");
        let buffer = terminal.backend().buffer().clone();
        for y in 0..buffer.area.height {
            let mut line = String::new();
            for x in 0..buffer.area.width {
                line.push_str(buffer[(x, y)].symbol());
            }
            eprintln!("|{line}|");
        }
    }

    #[test]
    fn chrome_rows_have_no_stray_background_chips() {
        let _theme_guard = theme::TEST_LOCK.blocking_lock();
        // Row 0 is the whole header now. It sits on the canvas background;
        // only the status pill may chip with panel_dark. Anything else is a
        // leaked span background (the old "black boxes" bug). There is no tab
        // row anymore — the agent switch moved to the composer footer.
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        populate_sample(&mut app);
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let palette = theme::palette();
        for x in 0..buffer.area.width {
            let bg = buffer[(x, 0)].bg;
            assert!(
                bg == palette.bg || bg == palette.panel_dark,
                "row 0 cell {x} leaked background {bg:?}"
            );
        }
    }

    #[test]
    fn header_drops_status_pill_when_composer_is_visible() {
        let _theme_guard = theme::TEST_LOCK.blocking_lock();
        // On a terminal tall enough to show the input, the status pill lives in
        // the composer's helper row — the header should stay on the base
        // canvas background with no panel_dark chip in row 0.
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.task_state = crate::tui::event::TaskState::Running;
        app.current_phase = Some("Running read".to_string());
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let palette = theme::palette();
        for x in 0..buffer.area.width {
            let bg = buffer[(x, 0)].bg;
            assert_ne!(
                bg, palette.panel_dark,
                "row 0 must not carry the status pill background when composer is visible"
            );
        }
    }

    #[test]
    fn composer_meta_line_uses_dot_agent_model_without_provider_or_phase() {
        // The status row above the input should read: <dot> · Agent · <model>,
        // with no provider, labeled pill, spinner word, or busy phase text.
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "opencode-zen",
            "opencode-zen/big-pickle".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.task_state = crate::tui::event::TaskState::Running;
        app.current_phase = Some("Running read".to_string());
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        // The composer lives in the lower half; scan every row for the new
        // sequence. The previous text (" ● ready " / " running ") must not
        // appear anywhere in the rendered frame.
        let mut combined = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                combined.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            combined.push('\n');
        }
        assert!(
            !combined.contains("ready"),
            "meta line should no longer carry the `ready` pill label"
        );
        assert!(
            !combined.contains(" running "),
            "meta line should no longer carry the labeled `running` pill"
        );
        // The expected order: dot · Coding · model (the default persona name).
        // `·` may be styled with a different fg color, so we look for each
        // substring independently rather than the joined sequence.
        for needle in ["Coding", "big-pickle"] {
            assert!(
                combined.contains(needle),
                "meta line should contain `{needle}` (combined buffer did not)"
            );
        }
        assert!(
            !combined.contains("opencode-zen"),
            "composer footer should not show the provider name"
        );
        assert!(
            !combined.contains("opencode-zen/big-pickle"),
            "composer footer should use the short model name"
        );
        assert!(
            !combined.contains("Running read"),
            "composer footer should not show the current busy phase"
        );
    }

    #[test]
    fn composer_meta_line_shows_smol_marker() {
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.smol_mode = true;

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        assert!(buffer_text(&buffer).contains("smol"));
    }

    #[test]
    fn composer_meta_line_shows_select_marker_only_when_capture_is_off() {
        let render = |mouse_capture: bool| {
            let backend = TestBackend::new(130, 40);
            let mut terminal = Terminal::new(backend).expect("test backend should initialize");
            let mut app = AppState::new(
                "codex",
                "gpt-5.5".to_string(),
                "bonsai".to_string(),
                Some("master".to_string()),
            );
            app.mouse_capture = mouse_capture;
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            buffer_text(&terminal.backend().buffer().clone())
        };

        assert!(
            !render(true).contains("select"),
            "no select marker while capture is on"
        );
        assert!(
            render(false).contains("select"),
            "select marker appears when capture is released"
        );
    }

    #[test]
    fn composer_meta_line_shows_serenity_marker() {
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.serenity_mode = true;

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        assert!(buffer_text(&buffer).contains("serenity"));
    }

    #[test]
    fn composer_meta_line_shows_shutdown_notice() {
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.shutdown_notice = Some("Ctrl+C again to exit".to_string());

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        assert!(buffer_text(&buffer).contains("Ctrl+C again to exit"));
    }

    #[test]
    fn composer_meta_line_shows_update_notice_instead_of_version() {
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.update_notice = Some("updated to v9.9.9 — restart to apply".to_string());

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let text = buffer_text(&buffer);
        assert!(text.contains("updated to v9.9.9"));
        assert!(!text.contains(concat!("v", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn queued_message_renders_in_transcript_and_composer_footer() {
        let backend = TestBackend::new(130, 40);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new("codex", "gpt-5.5".to_string(), "bonsai".to_string(), None);
        app.task_state = crate::tui::event::TaskState::Running;
        app.reduce(AppAction::QueueInput {
            id: 1,
            text: "follow-up instruction".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: crate::agent::AgentMode::Coding,
        });

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut combined = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                combined.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            combined.push('\n');
        }

        assert!(
            combined.contains("queued"),
            "pending transcript block should be labeled queued"
        );
        assert!(
            combined.contains("queued: follow-up instruction"),
            "composer footer should show queued summary"
        );
    }

    #[test]
    fn header_keeps_status_pill_when_composer_is_hidden() {
        let _theme_guard = theme::TEST_LOCK.blocking_lock();
        // On a short terminal where the input is hidden, the status pill falls
        // back to the header so it stays visible.
        let backend = TestBackend::new(130, 14);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.task_state = crate::tui::event::TaskState::Running;
        app.current_phase = Some("Running read".to_string());
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();

        let palette = theme::palette();
        let header_has_pill =
            (0..buffer.area.width).any(|x| buffer[(x, 0)].bg == palette.panel_dark);
        assert!(
            header_has_pill,
            "row 0 should carry the status pill background when composer is hidden"
        );
    }

    #[test]
    fn header_status_pill_includes_auto_accept_when_composer_is_hidden() {
        let _theme_guard = theme::TEST_LOCK.blocking_lock();
        let backend = TestBackend::new(130, 14);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.approval_level = crate::tool::ApprovalLevel::AutoAccept;

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let header = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        let policy_cell_is_amber = (0..buffer.area.width)
            .any(|x| buffer[(x, 0)].symbol() == "a" && buffer[(x, 0)].fg == theme::palette().todo);

        assert!(header.contains("auto-accept"));
        assert!(
            policy_cell_is_amber,
            "auto-accept header pill should use the caution (todo) color"
        );
    }

    #[test]
    fn header_status_pill_includes_serenity_when_composer_is_hidden() {
        let _theme_guard = theme::TEST_LOCK.blocking_lock();
        let backend = TestBackend::new(130, 14);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.serenity_mode = true;

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let header = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();

        assert!(header.contains("serenity"));
    }

    #[test]
    fn header_status_pill_shows_shutdown_notice_when_composer_is_hidden() {
        let backend = TestBackend::new(130, 14);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.shutdown_notice = Some("Ctrl+C again to exit".to_string());

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let header = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();

        assert!(header.contains("Ctrl+C again to exit"));
    }

    #[test]
    fn header_status_pill_shows_session_toast_when_composer_is_hidden() {
        let backend = TestBackend::new(130, 14);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "gpt-5.5".to_string(),
            "bonsai".to_string(),
            Some("master".to_string()),
        );
        app.set_session_toast("Resumed session #42.");

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let header = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();

        assert!(header.contains("Resumed session #42."));
    }

    #[test]
    fn frames_carry_view_identity_colors() {
        let _theme_guard = theme::TEST_LOCK.blocking_lock();
        use ratatui::layout::Rect;

        let area = Rect::new(0, 0, 130, 40);
        let backend = TestBackend::new(area.width, area.height);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        populate_sample(&mut app);

        // Agent view: blue transcript frame.
        let regions = crate::tui::layout::split(area, crate::agent::PersonaView::Todo);
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let corner = terminal.backend().buffer()[(regions.main.x, regions.main.y)].clone();
        assert!(
            corner.fg == theme::palette().agent_border
                || corner.fg == theme::palette().agent_accent,
            "agent view frame should be blue, got {:?}",
            corner.fg
        );

        // Plan view: yellow chat and canvas frames.
        app.reduce(AppAction::SetView(crate::tui::event::View::Plan));
        let regions = crate::tui::layout::split(area, crate::agent::PersonaView::Canvas);
        let plan = regions.plan.expect("plan view should have a canvas");
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        for (x, y, label) in [
            (regions.main.x, regions.main.y, "chat"),
            (plan.x, plan.y, "canvas"),
        ] {
            let cell = buffer[(x, y)].clone();
            assert!(
                cell.fg == theme::palette().plan_border || cell.fg == theme::palette().plan_accent,
                "plan view {label} frame should be yellow, got {:?}",
                cell.fg
            );
        }
    }

    #[test]
    fn renders_completion_popover() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("test backend should initialize");
        let mut app = AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        );
        app.focus = crate::tui::event::Focus::Input;
        app.reduce(AppAction::InputChar('/'));
        app.reduce(AppAction::InputChar('h'));
        app.reduce(AppAction::InputChar('e'));
        terminal
            .draw(|frame| draw(frame, &app))
            .expect("completion popover should render");
    }
}
