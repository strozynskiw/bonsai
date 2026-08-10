use super::markdown::*;
use super::selection::line_grapheme_count;
use super::tool_card::*;
use super::*;

pub(super) fn render_transcript_item(
    item: &TranscriptItem,
    width: usize,
    options: TranscriptRenderOptions<'_>,
) -> Vec<Line<'static>> {
    if item.is_suppressed() {
        return Vec::new();
    }
    if options.linear_output {
        return render_linear_transcript_item(item, width, options);
    }

    match item {
        TranscriptItem::UserMessage { text } => render_block(
            "❯ you",
            text,
            theme::palette().user,
            theme::block(theme::palette().user, theme::palette().user_block),
            theme::palette().user_block,
            width,
            options.selected,
            options.focused,
        ),
        TranscriptItem::QueuedUserMessage { text, delivery, .. } => {
            render_queued_user_message(text, *delivery, width, options.selected, options.focused)
        }
        TranscriptItem::AssistantMessage { text } => render_block(
            "● bonsai",
            text,
            theme::palette().assistant,
            theme::block(theme::palette().text, theme::palette().assistant_block),
            theme::palette().assistant_block,
            width,
            options.selected,
            options.focused,
        ),
        TranscriptItem::ReasoningSummary { text } => {
            if options.serenity_mode {
                render_serenity_reasoning_summary(text, width, options)
            } else {
                // OpenAI (gpt-5.x) reasoning summaries come as `**Heading**`
                // sections separated by empty `<!-- -->` HTML-comment
                // placeholders where the detailed reasoning is withheld.
                // Rendered through markdown verbatim those show up as literal
                // `<!-- -->` lines, so strip them and keep just the clean
                // section titles.
                let cleaned = strip_html_comments(text);
                render_block(
                    "✦ thinking",
                    &cleaned,
                    theme::palette().progress,
                    theme::block(theme::palette().dim, theme::palette().thinking_block),
                    theme::palette().thinking_block,
                    width,
                    options.selected,
                    options.focused,
                )
            }
        }
        TranscriptItem::ToolActivity(activity) => render_tool_activity(
            activity,
            width,
            options.selected,
            options.focused,
            options.permission_command,
            options.tick,
        ),
        TranscriptItem::ExecutionGroup(group) => render_execution_group(group, width, options),
        TranscriptItem::CommandOutput { kind, text } => {
            let (title, color, bg) = match kind {
                CommandOutputKind::Status => (
                    "· status",
                    theme::palette().muted,
                    theme::palette().assistant_block,
                ),
                CommandOutputKind::CompactionStatus => (
                    "· context compacted",
                    theme::palette().muted,
                    theme::palette().assistant_block,
                ),
                CommandOutputKind::Error => (
                    "✗ command error",
                    theme::palette().error,
                    theme::palette().error_block,
                ),
            };
            render_block(
                title,
                text,
                color,
                theme::block(color, bg),
                bg,
                width,
                options.selected,
                options.focused,
            )
        }
        TranscriptItem::CompletionReport(report) => {
            let (title, color, bg) = match report.status {
                crate::completion_report::CompletionStatus::Completed => (
                    "✓ completion",
                    theme::palette().success,
                    theme::palette().assistant_block,
                ),
                crate::completion_report::CompletionStatus::Failed => (
                    "✗ completion",
                    theme::palette().error,
                    theme::palette().error_block,
                ),
                crate::completion_report::CompletionStatus::Blocked => (
                    "! completion blocked",
                    theme::palette().progress,
                    theme::palette().assistant_block,
                ),
                crate::completion_report::CompletionStatus::Interrupted
                | crate::completion_report::CompletionStatus::BudgetExhausted => (
                    "! completion",
                    theme::palette().progress,
                    theme::palette().assistant_block,
                ),
            };
            render_block(
                title,
                &report.render_compact(),
                color,
                theme::block(color, bg),
                bg,
                width,
                options.selected,
                options.focused,
            )
        }
        TranscriptItem::Error { error } => render_block(
            &format!("✗ {}", error.title),
            &error.detail,
            theme::palette().error,
            theme::block(theme::palette().error, theme::palette().error_block),
            theme::palette().error_block,
            width,
            options.selected,
            options.focused,
        ),
        TranscriptItem::Edit { text } => render_block(
            "± edit",
            text,
            theme::palette().edit,
            theme::block(theme::palette().edit, theme::palette().edit_block),
            theme::palette().edit_block,
            width,
            options.selected,
            options.focused,
        ),
        TranscriptItem::TodoUpdate { text } => render_block(
            "≡ todo",
            text,
            theme::palette().todo,
            theme::block(theme::palette().todo, theme::palette().todo_block),
            theme::palette().todo_block,
            width,
            options.selected,
            options.focused,
        ),
        TranscriptItem::WorkLog { text } => render_block(
            "· note",
            text,
            theme::palette().muted,
            theme::block(theme::palette().muted, theme::palette().assistant_block),
            theme::palette().assistant_block,
            width,
            options.selected,
            options.focused,
        ),
        TranscriptItem::PeerMessage {
            session_id,
            outgoing,
            text,
            ..
        } => render_block(
            &peer_message_title(*session_id, *outgoing),
            text,
            theme::palette().peer,
            theme::block(theme::palette().peer, theme::palette().peer_block),
            theme::palette().peer_block,
            width,
            options.selected,
            options.focused,
        ),
    }
}

fn render_linear_transcript_item(
    item: &TranscriptItem,
    width: usize,
    options: TranscriptRenderOptions<'_>,
) -> Vec<Line<'static>> {
    match item {
        TranscriptItem::UserMessage { text } => linear_block("You", text, width, options),
        TranscriptItem::QueuedUserMessage { text, delivery, .. } => linear_block(
            match delivery {
                crate::tui::app::FollowUpDelivery::Steer => "Steering message",
                crate::tui::app::FollowUpDelivery::Queue => "Queued message",
            },
            text,
            width,
            options,
        ),
        TranscriptItem::AssistantMessage { text } => linear_block("Bonsai", text, width, options),
        TranscriptItem::ReasoningSummary { text } => linear_block(
            "Thinking summary",
            &strip_html_comments(text),
            width,
            options,
        ),
        TranscriptItem::ToolActivity(activity) => render_linear_tool(activity, width, options),
        TranscriptItem::ExecutionGroup(group) => group
            .tools
            .iter()
            .flat_map(|activity| render_linear_tool(activity, width, options))
            .collect(),
        TranscriptItem::CommandOutput { kind, text } => {
            let label = match kind {
                CommandOutputKind::Status => "Status",
                CommandOutputKind::CompactionStatus => "Context compacted",
                CommandOutputKind::Error => "Command error",
            };
            linear_block(label, text, width, options)
        }
        TranscriptItem::CompletionReport(report) => {
            let label = match report.status {
                crate::completion_report::CompletionStatus::Completed => "Completion: completed",
                crate::completion_report::CompletionStatus::Blocked => "Completion: blocked",
                crate::completion_report::CompletionStatus::Failed => "Completion: failed",
                crate::completion_report::CompletionStatus::Interrupted => {
                    "Completion: interrupted"
                }
                crate::completion_report::CompletionStatus::BudgetExhausted => {
                    "Completion: budget exhausted"
                }
            };
            linear_block(label, &report.render_compact(), width, options)
        }
        TranscriptItem::Error { error } => linear_block(
            &format!("Error: {}", error.title),
            &error.detail,
            width,
            options,
        ),
        TranscriptItem::Edit { text } => linear_block("Edit", text, width, options),
        TranscriptItem::TodoUpdate { text } => linear_block("Todo", text, width, options),
        TranscriptItem::WorkLog { text } => linear_block("Note", text, width, options),
        TranscriptItem::PeerMessage {
            session_id,
            outgoing,
            text,
            ..
        } => {
            let label = if *outgoing {
                format!("Message to agent {session_id}")
            } else {
                format!("Message from agent {session_id}")
            };
            linear_block(&label, text, width, options)
        }
    }
}

fn render_linear_tool(
    activity: &ToolActivity,
    width: usize,
    options: TranscriptRenderOptions<'_>,
) -> Vec<Line<'static>> {
    let status = if tool_waits_for_permission(activity, options.permission_command) {
        "awaiting permission"
    } else {
        activity.status.label()
    };
    let mut label = format!("Tool {}", tool_activity_display_name(activity));
    if activity.name == "agent"
        && let Some(model) = activity.delegated_model_name()
    {
        label.push_str(&format!("  {model}"));
    }
    label.push_str(&format!(" ({status})"));
    if let Some((key, value)) = primary_arg_value(&activity.arguments) {
        label.push_str(&format!(": {key}={}", compact_text(&value, 96)));
    }

    let mut body = activity
        .result
        .as_deref()
        .map(tool_result_body)
        .unwrap_or_default();
    if let Some(diff) = &activity.diff {
        let stats = diff.stats();
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!(
            "File changes: {} added lines, {} removed lines",
            stats.added, stats.removed
        ));
    }
    linear_block(&label, &body, width, options)
}

fn linear_block(
    label: &str,
    body: &str,
    width: usize,
    options: TranscriptRenderOptions<'_>,
) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let label_style = if options.focused {
        theme::body(palette.border_active).add_modifier(Modifier::BOLD)
    } else {
        theme::body(palette.text).add_modifier(Modifier::BOLD)
    };
    let whole_style = matches!(options.selected, ItemSelection::Whole)
        .then(|| theme::selection_block(palette.text));
    let mut lines = vec![Line::from("")];
    let label_line = Line::from(Span::styled(format!("{label}:"), label_style));
    lines.extend(wrap_card_line(&label_line, width).into_iter().map(|line| {
        if let Some(style) = whole_style {
            line.style(style)
        } else {
            line
        }
    }));

    let mut selection_cursor = 0usize;
    for (line_index, line) in render_markdown(body, width).into_iter().enumerate() {
        if line_index > 0 {
            selection_cursor += 1;
        }
        for wrapped in wrap_card_line(&line, width) {
            let line = apply_range_selection(&wrapped, options.selected, selection_cursor);
            selection_cursor += line_grapheme_count(&wrapped);
            lines.push(if let Some(style) = whole_style {
                line.style(style)
            } else {
                line
            });
        }
    }
    lines
}

/// Reasoning volume below this reads as "Quick thought".
const SERENITY_THOUGHT_SOME_MIN_CHARS: usize = 250;
/// Reasoning volume where this reads as "Chewed on it".
const SERENITY_THOUGHT_QUITE_A_BIT_MIN_CHARS: usize = 900;
/// Reasoning volume where this reads as "Went deep".
const SERENITY_THOUGHT_HEAVY_LIFTING_MIN_CHARS: usize = 1800;
/// Reasoning volume at or above this reads as "Took the scenic route".
const SERENITY_THOUGHT_A_LOT_MIN_CHARS: usize = 3200;

/// Serenity's finished-thinking label, bucketed by how much reasoning the
/// model actually produced:
///
/// - below 250 chars: "Quick thought"
/// - 250-899 chars: "Chewed on it"
/// - 900-1799 chars: "Went deep"
/// - 1800-3199 chars: "Ran the numbers"
/// - 3200+ chars: "Took the scenic route"
///
/// Counted on the raw cell text (provider comment placeholders included) — the
/// bucket reflects volume, not readability.
fn serenity_thought_label(reasoning_chars: usize) -> &'static str {
    if reasoning_chars >= SERENITY_THOUGHT_A_LOT_MIN_CHARS {
        "Took the scenic route"
    } else if reasoning_chars >= SERENITY_THOUGHT_HEAVY_LIFTING_MIN_CHARS {
        "Ran the numbers"
    } else if reasoning_chars >= SERENITY_THOUGHT_QUITE_A_BIT_MIN_CHARS {
        "Went deep"
    } else if reasoning_chars >= SERENITY_THOUGHT_SOME_MIN_CHARS {
        "Chewed on it"
    } else {
        "Quick thought"
    }
}

fn render_serenity_reasoning_summary(
    text: &str,
    width: usize,
    options: TranscriptRenderOptions<'_>,
) -> Vec<Line<'static>> {
    if options.reasoning_active {
        // Leading blank line so the streaming "Thinking" indicator is separated
        // from the preceding card, matching the gap every titled block gets.
        // `render_title: true` would add an empty title row too, so the spacer
        // is prepended here and the block itself stays title-less.
        let mut lines = vec![Line::from("")];
        lines.extend(render_block_lines(
            "",
            vec![serenity_thinking_line(options.tick)],
            theme::palette().progress,
            theme::block(theme::palette().muted, theme::palette().thinking_block),
            theme::palette().thinking_block,
            width,
            options.selected,
            false,
            options.focused,
        ));
        return lines;
    }

    let label = serenity_thought_label(text.chars().count());
    render_block(
        "· serenity",
        label,
        theme::palette().muted,
        theme::block(theme::palette().muted, theme::palette().thinking_block),
        theme::palette().thinking_block,
        width,
        options.selected,
        options.focused,
    )
}

const SERENITY_THINKING_LABEL: &str = "Thinking";

/// Produces a moving highlight that sweeps over the thinking label. The
/// sparkle has its own gentle pulse so the animation still feels alive while
/// the highlight is just outside either edge of the word.
fn serenity_thinking_line(tick: u64) -> Line<'static> {
    let label_len = SERENITY_THINKING_LABEL.chars().count();
    let shimmer_center = (tick / 2 % (label_len as u64 + 4)) as isize - 2;

    let spans: Vec<Span<'static>> = SERENITY_THINKING_LABEL
        .chars()
        .enumerate()
        .map(|(index, letter)| {
            let distance = (index as isize - shimmer_center).unsigned_abs();
            Span::styled(letter.to_string(), serenity_shimmer_style(distance))
        })
        .collect();
    Line::from(spans)
}

fn serenity_shimmer_style(distance: usize) -> Style {
    let palette = theme::palette();
    match distance {
        0 => theme::body(palette.text).add_modifier(Modifier::BOLD),
        1 => theme::body(palette.progress).add_modifier(Modifier::BOLD),
        2 => theme::body(palette.progress),
        _ => theme::body(palette.muted).add_modifier(Modifier::DIM),
    }
}

/// Remove `<!-- ... -->` HTML comments from reasoning-summary text and collapse
/// the blank lines they leave behind, so a provider's comment placeholders don't
/// render as literal `<!-- -->` lines in the thinking block. An unterminated
/// `<!--` (never closed before end of text) drops the remainder — a truncated
/// comment tail is noise, not content.
fn strip_html_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    collapse_blank_runs(out.trim())
}

/// Collapse runs of 3+ newlines (left behind after removing a comment on its own
/// line) down to a single blank line.
fn collapse_blank_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut newlines = 0usize;
    for ch in text.chars() {
        if ch == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push(ch);
            }
        } else {
            newlines = 0;
            out.push(ch);
        }
    }
    out
}

/// The title line of a blue inter-agent chat message: `⇄ agent #45 → you` for
/// an incoming message, `⇄ you → agent #45` for one this session sent.
pub(crate) fn peer_message_title(session_id: i64, outgoing: bool) -> String {
    if outgoing {
        format!("⇄ you → agent #{session_id}")
    } else {
        format!("⇄ agent #{session_id} → you")
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_block(
    title: &str,
    body: &str,
    accent: Color,
    body_style: Style,
    block_bg: Color,
    width: usize,
    selected: ItemSelection,
    focused: bool,
) -> Vec<Line<'static>> {
    let inner_width = width.saturating_sub(4).max(8);
    let body_lines = render_markdown(body, inner_width);
    render_block_lines(
        title, body_lines, accent, body_style, block_bg, width, selected, true, focused,
    )
}

pub(super) fn render_queued_user_message(
    body: &str,
    delivery: crate::tui::app::FollowUpDelivery,
    width: usize,
    selected: ItemSelection,
    focused: bool,
) -> Vec<Line<'static>> {
    let inner_width = width.saturating_sub(4).max(8);
    let accent = theme::palette().dim;
    let block_bg = theme::palette().user_block;
    let is_selected = !matches!(selected, ItemSelection::None);
    let (title_style, body_bg) = if is_selected {
        (
            theme::selection_block(accent).add_modifier(Modifier::BOLD),
            theme::palette().selection_bg,
        )
    } else if focused {
        (
            theme::block(theme::palette().border_active, block_bg).add_modifier(Modifier::BOLD),
            block_bg,
        )
    } else {
        (
            theme::block(accent, block_bg)
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::DIM),
            block_bg,
        )
    };
    let body_style = if is_selected {
        theme::selection_block(theme::palette().text)
    } else {
        theme::block(accent, block_bg).add_modifier(Modifier::DIM)
    };

    let title = queued_title_line(delivery, inner_width);
    let mut lines = vec![
        Line::from(""),
        card_line(&title, inner_width, title_style, body_bg, accent, focused),
    ];

    let body_lines = render_markdown(body, inner_width);
    lines.extend(render_block_lines(
        "", body_lines, accent, body_style, block_bg, width, selected, false, focused,
    ));

    lines
}

pub(super) fn queued_title_line(
    delivery: crate::tui::app::FollowUpDelivery,
    width: usize,
) -> Line<'static> {
    let label = match delivery {
        crate::tui::app::FollowUpDelivery::Steer => "❯ steer",
        crate::tui::app::FollowUpDelivery::Queue => "❯ queued",
    };
    let label_width = label.width();
    let cancel_width = QUEUED_CANCEL_LABEL.width();
    let gap = width.saturating_sub(label_width + cancel_width);
    Line::from(vec![
        Span::styled(label.to_string(), Style::default()),
        Span::styled(" ".repeat(gap), Style::default()),
        Span::styled(
            QUEUED_CANCEL_LABEL.to_string(),
            Style::default().fg(theme::palette().error),
        ),
    ])
}

/// Renders a card from pre-styled body lines. Spans that carry their own
/// background (diff rows) keep it; everything else gets the card background.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_block_lines(
    title: &str,
    body_lines: Vec<Line<'static>>,
    accent: Color,
    body_style: Style,
    block_bg: Color,
    width: usize,
    selected: ItemSelection,
    render_title: bool,
    focused: bool,
) -> Vec<Line<'static>> {
    let inner_width = width.saturating_sub(4).max(8);
    let mut lines = if render_title {
        vec![Line::from("")]
    } else {
        Vec::new()
    };
    let card_width = inner_width;

    let is_whole_selected = matches!(selected, ItemSelection::Whole);
    let (title_style, body_bg) = if is_whole_selected {
        (
            theme::selection_block(accent).add_modifier(Modifier::BOLD),
            theme::palette().selection_bg,
        )
    } else if focused {
        (
            theme::block(theme::palette().border_active, block_bg).add_modifier(Modifier::BOLD),
            block_bg,
        )
    } else {
        (
            theme::block(accent, block_bg).add_modifier(Modifier::BOLD),
            block_bg,
        )
    };

    if render_title {
        lines.push(card_line(
            &Line::from(title.to_string()),
            card_width,
            title_style,
            body_bg,
            accent,
            focused,
        ));
    }

    let effective_body_style = if is_whole_selected {
        theme::selection_block(theme::palette().text)
    } else {
        body_style
    };

    let mut selection_cursor = 0usize;
    for (line_index, line) in body_lines.into_iter().enumerate() {
        if line_index > 0 {
            // Match the newline retained by `selection_text_for` between
            // distinct rendered Markdown block lines.
            selection_cursor += 1;
        }
        for wrapped in wrap_card_line(&line, card_width) {
            let styled_wrapped = apply_range_selection(&wrapped, selected, selection_cursor);
            selection_cursor += line_grapheme_count(&wrapped);
            lines.push(card_line(
                &styled_wrapped,
                card_width,
                effective_body_style,
                body_bg,
                accent,
                focused,
            ));
        }
    }
    lines
}

pub(super) fn apply_range_selection(
    line: &Line<'static>,
    selected: ItemSelection,
    line_start: usize,
) -> Line<'static> {
    let ItemSelection::Range { start, end } = selected else {
        return line.clone();
    };
    let line_len = line_grapheme_count(line);
    let line_end = line_start + line_len;
    if line_len == 0 || end <= line_start || start >= line_end {
        return line.clone();
    }

    let local_start = start.saturating_sub(line_start).min(line_len);
    let local_end = end.saturating_sub(line_start).min(line_len);
    if local_start >= local_end {
        return line.clone();
    }

    let mut out = Vec::new();
    let mut cursor = 0usize;
    for span in &line.spans {
        use unicode_segmentation::UnicodeSegmentation;
        let graphemes = span.content.as_ref().graphemes(true).collect::<Vec<_>>();
        let span_len = graphemes.len();
        let span_start = cursor;
        let span_end = cursor + span_len;
        push_span_segment(
            &mut out,
            &graphemes,
            0,
            local_start.saturating_sub(span_start),
            span.style,
        );

        let selected_start = local_start.saturating_sub(span_start).min(span_len);
        let selected_end = local_end.saturating_sub(span_start).min(span_len);
        let selected_style = span
            .style
            .patch(theme::selection_block(theme::palette().text));
        push_span_segment(
            &mut out,
            &graphemes,
            selected_start,
            selected_end,
            selected_style,
        );

        let unselected_start = local_end.saturating_sub(span_start).min(span_len);
        push_span_segment(&mut out, &graphemes, unselected_start, span_len, span.style);
        cursor = span_end;
    }

    Line::from(out)
}

pub(super) fn push_span_segment(
    out: &mut Vec<Span<'static>>,
    graphemes: &[&str],
    start: usize,
    end: usize,
    style: Style,
) {
    let start = start.min(graphemes.len());
    let end = end.min(graphemes.len());
    if start >= end {
        return;
    }
    let text = graphemes[start..end].concat();
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(&text);
        return;
    }
    out.push(Span::styled(text, style));
}

/// Wraps a single line to fit within `width` display cells. CJK and emoji
/// characters count as 2 cells (matching their terminal width), and a wide
/// character is never split across lines. Style runs are preserved so the
/// resulting wrapped lines keep their original styling.
pub(super) fn wrap_card_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if line.spans.is_empty() {
        return vec![line.clone()];
    }
    let total = rendered_line_width(line);
    if total <= width {
        return vec![line.clone()];
    }

    // Flatten the line's content into (text, style) runs so we can re-slice
    // it across wrap boundaries while keeping style attributes.
    struct Run {
        text: String,
        style: Style,
    }
    let mut runs: Vec<Run> = Vec::new();
    for span in &line.spans {
        if span.content.is_empty() {
            continue;
        }
        if let Some(last) = runs.last_mut()
            && last.style == span.style
        {
            last.text.push_str(span.content.as_ref());
        } else {
            runs.push(Run {
                text: span.content.to_string(),
                style: span.style,
            });
        }
    }

    // For each run, build a (char_index, run_index, cell_width) tuple so
    // we can map a cell range back to runs.
    struct Cell {
        run: usize,
        ch: char,
        width: usize,
    }
    let cells: Vec<Cell> = runs
        .iter()
        .enumerate()
        .flat_map(|(run_index, run)| {
            run.text.chars().map(move |ch| Cell {
                run: run_index,
                width: ch.width().unwrap_or(0),
                ch,
            })
        })
        .collect();

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cursor = 0usize;
    while cursor < cells.len() {
        let mut line_end = cursor;
        let mut line_width = 0usize;
        let mut last_break: Option<usize> = None;
        while line_end < cells.len() {
            let cell = &cells[line_end];
            if cell.width > width {
                // A single cell is wider than the line. Emit what we have so
                // far (if any) then break right before the wide cell.
                break;
            }
            if line_width + cell.width > width {
                break;
            }
            line_width += cell.width;
            if ch_is_breakable_whitespace(cell.ch) {
                // +1 reserves room for the space itself when we slice it off.
                last_break = Some(line_end + 1);
            }
            line_end += 1;
        }

        if line_end == cursor {
            // The cell at `cursor` does not fit; place it alone and advance.
            line_end = cursor + 1;
            last_break = None;
        }

        // Break on whitespace only when the segment actually overflowed;
        // a remainder that fits in full must not be split at its last space.
        let overflowed = line_end < cells.len();
        let slice_end = match last_break {
            Some(break_at) if overflowed && break_at < line_end => break_at,
            _ => line_end,
        };

        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut current_run = usize::MAX;
        let mut current_text = String::new();
        for cell in &cells[cursor..slice_end] {
            if cell.run != current_run {
                if !current_text.is_empty() {
                    let style = runs[current_run].style;
                    spans.push(Span::styled(current_text, style));
                }
                current_run = cell.run;
                current_text = String::new();
            }
            current_text.push(cell.ch);
        }
        if !current_text.is_empty() {
            let style = runs[current_run].style;
            spans.push(Span::styled(current_text, style));
        }
        out.push(Line::from(spans));

        // Skip the leading whitespace on the next line if we broke on a space.
        cursor = slice_end;
        if cursor < cells.len() && cells[cursor - 1].ch.is_whitespace() {
            // already consumed it
        }
    }
    if out.is_empty() {
        out.push(line.clone());
    }
    out
}

pub(super) fn ch_is_breakable_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t')
}

pub(super) fn card_line(
    line: &Line<'static>,
    width: usize,
    style: Style,
    block_bg: Color,
    accent: Color,
    focused: bool,
) -> Line<'static> {
    let rail = if focused { "▌ " } else { "┃ " };
    let rail_color = if focused {
        theme::palette().border_active
    } else {
        accent
    };
    let mut spans = vec![Span::styled(rail, Style::default().fg(rail_color))];
    let mut content_width = 0;

    for span in &line.spans {
        let content = span.content.to_string();
        let cell_width = content.as_str().width();
        content_width += cell_width;
        let merged = style.patch(span.style);
        let merged = if span.style.bg.is_some() {
            merged
        } else {
            merged.bg(block_bg)
        };
        spans.push(Span::styled(content, merged));
    }

    if content_width < width {
        spans.push(Span::styled(
            " ".repeat(width - content_width),
            style.bg(block_bg),
        ));
    }
    spans.push(Span::styled("  ", Style::default().bg(block_bg)));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_comments_removes_reasoning_placeholders() {
        // gpt-5.x reasoning summary shape: headings separated by empty comments.
        let raw = "**A**\n\n<!-- -->\n\n**B**\n\n<!-- -->\n\n**C**\n\n<!-- -->";
        assert_eq!(strip_html_comments(raw), "**A**\n\n**B**\n\n**C**");
    }

    #[test]
    fn strip_html_comments_handles_inline_no_comment_and_unterminated() {
        assert_eq!(strip_html_comments("a <!-- x --> b"), "a  b");
        assert_eq!(strip_html_comments("plain text"), "plain text");
        // Unterminated comment drops the truncated tail.
        assert_eq!(strip_html_comments("keep <!-- dangling"), "keep");
    }

    #[test]
    fn serenity_thinking_line_keeps_the_label_readable() {
        let line = serenity_thinking_line(4);
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, "Thinking");
    }

    #[test]
    fn serenity_thinking_line_moves_the_brightest_letter_between_ticks() {
        let palette = theme::palette();
        let bright_letter = |line: &Line<'static>| {
            line.spans
                .iter()
                .position(|span| span.style.fg == Some(palette.text))
        };

        let first = bright_letter(&serenity_thinking_line(4));
        let later = bright_letter(&serenity_thinking_line(12));

        assert_eq!((first, later), (Some(0), Some(4)));
    }
}
