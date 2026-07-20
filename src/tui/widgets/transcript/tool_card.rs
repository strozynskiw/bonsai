use super::common::*;
use super::*;

#[path = "group_semantics.rs"]
mod group_semantics;

use group_semantics::summarize_group;

pub(super) fn render_execution_group(
    group: &ExecutionGroup,
    width: usize,
    options: TranscriptRenderOptions<'_>,
) -> Vec<Line<'static>> {
    let selected_tool = options
        .active_group_tool_selection
        .filter(|selection| selection.group_id == group.id)
        .map(|selection| selection.selected_tool);
    let selected_tool = if options.serenity_mode && !options.execution_group_expanded {
        None
    } else {
        selected_tool
    };
    let mut lines = vec![Line::from("")];
    lines.extend(render_execution_group_summary_row(
        group,
        width,
        ExecutionGroupSummaryRenderOptions {
            selected: options.selected,
            focused: options.focused && selected_tool.is_none(),
            serenity_mode: options.serenity_mode,
            execution_group_expanded: options.execution_group_expanded,
            permission_command: options.permission_command,
            tick: options.tick,
        },
    ));

    for index in visible_tool_indices(
        group,
        options.serenity_mode,
        options.execution_group_expanded,
    ) {
        let Some(activity) = group.tools.get(index) else {
            continue;
        };
        let row_selected = selected_tool == Some(index);
        let row_selection = if row_selected {
            ItemSelection::Whole
        } else {
            options.selected
        };
        let row_focused = if selected_tool.is_some() {
            row_selected
        } else {
            options.focused
        };
        lines.extend(render_nested_tool_activity(
            activity,
            width,
            row_selection,
            row_focused,
            options.permission_command,
            options.tick,
        ));
    }
    lines
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ExecutionGroupSummaryRenderOptions<'a> {
    pub(super) selected: ItemSelection,
    pub(super) focused: bool,
    pub(super) serenity_mode: bool,
    pub(super) execution_group_expanded: bool,
    pub(super) permission_command: Option<&'a str>,
    pub(super) tick: u64,
}

pub(super) fn render_execution_group_summary_row(
    group: &ExecutionGroup,
    width: usize,
    options: ExecutionGroupSummaryRenderOptions<'_>,
) -> Vec<Line<'static>> {
    let hidden_tools = group.tools.len().saturating_sub(
        visible_tool_indices(
            group,
            options.serenity_mode,
            options.execution_group_expanded,
        )
        .len(),
    );
    let (summary, accent, bg) =
        execution_group_render_summary(group, options.permission_command, hidden_tools);
    let waiting_for_permission = group
        .tools
        .iter()
        .any(|activity| tool_waits_for_permission(activity, options.permission_command));
    let is_running = group
        .tools
        .iter()
        .any(|activity| matches!(activity.status, ToolStatus::Running));
    let mut spans = if options.serenity_mode {
        vec![Span::styled(
            if options.execution_group_expanded {
                "v "
            } else {
                "> "
            },
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )]
    } else {
        vec![Span::styled(
            "⟳ ".to_string(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        )]
    };
    if is_running && !options.serenity_mode {
        let frame = if waiting_for_permission {
            "?"
        } else if options.tick.is_multiple_of(2) {
            "◌"
        } else {
            "◉"
        };
        if let Some(last) = spans.last_mut() {
            *last = Span::styled(format!("{frame} "), Style::default().fg(accent));
        }
    }

    let mut text = Span::styled(summary, Style::default().fg(theme::palette().muted));
    text.style = Style::default().fg(theme::palette().text);
    spans.push(text);

    let inner_width = width.saturating_sub(4).max(8);
    let is_selected = !matches!(options.selected, ItemSelection::None);
    let (line_style, bg) = if is_selected {
        (
            theme::selection_block(accent),
            theme::palette().selection_bg,
        )
    } else {
        (theme::block(accent, bg), bg)
    };

    let content_line = Line::from(spans);
    let mut lines = Vec::new();
    for wrapped in wrap_card_line(&content_line, inner_width) {
        lines.push(card_line(
            &wrapped,
            inner_width,
            line_style,
            bg,
            accent,
            options.focused,
        ));
    }
    lines
}

pub(super) fn execution_group_render_summary(
    group: &ExecutionGroup,
    permission_command: Option<&str>,
    hidden_tools: usize,
) -> (String, Color, Color) {
    let waiting_names = group
        .tools
        .iter()
        .filter(|activity| tool_waits_for_permission(activity, permission_command))
        .map(|activity| activity.name.as_str())
        .collect::<Vec<_>>();
    if waiting_names.is_empty() {
        return execution_group_summary_with_hidden(group, hidden_tools);
    }

    let mut parts = vec![format!(
        "Awaiting permission for {} tool{}",
        waiting_names.len(),
        if waiting_names.len() == 1 { "" } else { "s" }
    )];
    let mut names: Vec<&str> = Vec::new();
    for name in waiting_names {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if !names.is_empty() {
        parts.push(names.join(", "));
    }
    (
        parts.join(" · "),
        execution_group_summary_style().0,
        execution_group_summary_style().1,
    )
}

#[cfg(test)]
pub(super) fn execution_group_summary(group: &ExecutionGroup) -> (String, Color, Color) {
    execution_group_summary_with_hidden(group, 0)
}

fn execution_group_summary_with_hidden(
    group: &ExecutionGroup,
    hidden_tools: usize,
) -> (String, Color, Color) {
    let (accent, bg) = execution_group_summary_style();
    let semantics = summarize_group(group);
    (semantics.summary(hidden_tools), accent, bg)
}

pub(super) fn visible_tool_indices(
    group: &ExecutionGroup,
    serenity_mode: bool,
    execution_group_expanded: bool,
) -> Vec<usize> {
    if serenity_mode && !execution_group_expanded {
        Vec::new()
    } else {
        group.tool_indices().collect()
    }
}

fn execution_group_summary_style() -> (Color, Color) {
    (theme::palette().tool, theme::palette().tool_block)
}

/// Tool calls render as a single compact line — glyph, name, primary
/// argument, `+added -removed` for edits, and the per-call execution time.
/// Full arguments, results, failure details, and diffs live in the detail modal
/// (click the line or press Enter in transcript focus). Running Bash rows may
/// surface live output because it can require user attention.
pub(super) fn render_tool_activity(
    activity: &ToolActivity,
    width: usize,
    selected: ItemSelection,
    focused: bool,
    permission_command: Option<&str>,
    tick: u64,
) -> Vec<Line<'static>> {
    let (_, block_bg) = tool_status_style(activity.status);
    let card_accent = theme::palette().tool;
    let spans = tool_activity_summary_spans(activity, permission_command, tick);
    let inner_width = width.saturating_sub(4).max(8);
    let is_selected = !matches!(selected, ItemSelection::None);
    let (line_style, bg) = if is_selected {
        (
            theme::selection_block(card_accent),
            theme::palette().selection_bg,
        )
    } else {
        (theme::block(card_accent, block_bg), block_bg)
    };

    let content_line = Line::from(spans);
    let mut lines = vec![Line::from("")];
    for wrapped in wrap_card_line(&content_line, inner_width) {
        lines.push(card_line(
            &wrapped,
            inner_width,
            line_style,
            bg,
            card_accent,
            focused,
        ));
    }
    lines
}

pub(super) fn render_nested_tool_activity(
    activity: &ToolActivity,
    width: usize,
    selected: ItemSelection,
    focused: bool,
    permission_command: Option<&str>,
    tick: u64,
) -> Vec<Line<'static>> {
    let (_, block_bg) = tool_status_style(activity.status);
    let card_accent = theme::palette().tool;
    let mut spans = vec![Span::styled(
        "  • ".to_string(),
        Style::default().fg(theme::palette().dim),
    )];
    spans.extend(tool_activity_summary_spans(
        activity,
        permission_command,
        tick,
    ));

    let inner_width = width.saturating_sub(4).max(8);
    let is_selected = !matches!(selected, ItemSelection::None);
    let (line_style, bg) = if is_selected {
        (
            theme::selection_block(card_accent),
            theme::palette().selection_bg,
        )
    } else {
        (theme::block(card_accent, block_bg), block_bg)
    };

    let content_line = Line::from(spans);
    let mut lines = Vec::new();
    for wrapped in wrap_card_line(&content_line, inner_width) {
        lines.push(card_line(
            &wrapped,
            inner_width,
            line_style,
            bg,
            card_accent,
            focused,
        ));
    }
    lines
}

pub(super) fn tool_activity_summary_spans(
    activity: &ToolActivity,
    permission_command: Option<&str>,
    tick: u64,
) -> Vec<Span<'static>> {
    let (accent, _) = tool_status_style(activity.status);
    let waiting_for_permission = tool_waits_for_permission(activity, permission_command);
    let glyph = match activity.status {
        ToolStatus::Running if waiting_for_permission => "?",
        ToolStatus::Running => theme::spinner_frame(tick),
        ToolStatus::Succeeded => "✓",
        ToolStatus::Failed => "✗",
    };

    let display_name = tool_activity_display_name(activity);
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            glyph.to_string(),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {display_name}"),
            Style::default()
                .fg(theme::palette().text)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    if activity.name == "question" {
        spans.extend(question_summary_spans(&activity.arguments));
    } else if let Some((key, text)) = primary_arg_value(&activity.arguments) {
        spans.push(Span::raw(" ".to_string()));
        spans.extend(primary_arg_spans(
            &key,
            &serde_json::Value::String(compact_text(&text, 64)),
        ));
    }

    // Compact reuse is model-visible work avoided, whether the retained result
    // came from a file read or an exact repository inspection.
    if activity.result.as_deref().is_some_and(|result| {
        result.starts_with(crate::agent::REUSED_READ_MARKER)
            || result.starts_with(crate::agent::REUSED_INSPECTION_MARKER)
    }) {
        spans.push(Span::styled(
            "  · reused previous inspection".to_string(),
            Style::default().fg(theme::palette().dim),
        ));
    }

    if let Some(diff) = &activity.diff {
        let stats = diff.stats();
        spans.push(Span::styled(
            format!("  +{}", stats.added),
            Style::default()
                .fg(theme::palette().added)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" -{}", stats.removed),
            Style::default()
                .fg(theme::palette().removed)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let bash_summary = (activity.status != ToolStatus::Failed)
        .then(|| bash_output_summary(activity))
        .flatten();
    let suppress_duration = bash_summary
        .as_ref()
        .map(|summary| summary.from_footer)
        .unwrap_or(false);

    if waiting_for_permission {
        spans.push(Span::styled(
            "  · awaiting permission".to_string(),
            Style::default().fg(theme::palette().dim),
        ));
    } else if !suppress_duration && let Some(duration) = meaningful_duration(activity.duration()) {
        // Sub-100ms local tools (plan edits, todowrite) finish faster than a
        // human notices; their `<1ms` stamp is pure noise, so only slow calls
        // earn a duration. The signal becomes "this one took a moment".
        spans.push(Span::styled(
            format!("  · {duration}"),
            Style::default().fg(theme::palette().dim),
        ));
    }

    if let Some(summary) = bash_summary {
        spans.push(Span::styled(
            "  → ".to_string(),
            Style::default().fg(theme::palette().dim),
        ));
        spans.push(Span::styled(
            summary.text,
            Style::default().fg(theme::palette().todo),
        ));
    }

    spans
}

pub(super) fn question_summary_spans(arguments: &str) -> Vec<Span<'static>> {
    const MAX_SHORT_QUESTION_CHARS: usize = 80;

    let Ok(serde_json::Value::Object(map)) = serde_json::from_str(arguments) else {
        return Vec::new();
    };
    let option_count = map
        .get("options")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len);
    let question = map
        .get("question")
        .or_else(|| map.get("prompt"))
        .and_then(serde_json::Value::as_str)
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty() && text.chars().count() <= MAX_SHORT_QUESTION_CHARS);

    let mut spans = Vec::new();
    if let Some(count) = option_count {
        spans.push(Span::styled(
            format!(" ({count} option{})", if count == 1 { "" } else { "s" }),
            Style::default().fg(theme::palette().dim),
        ));
    }
    if let Some(question) = question {
        spans.push(Span::styled(
            "  · ".to_string(),
            Style::default().fg(theme::palette().dim),
        ));
        spans.push(Span::styled(
            question,
            Style::default().fg(theme::palette().text),
        ));
    }
    spans
}

pub(super) fn pending_permission_command(app: &AppState) -> Option<&str> {
    match app.modal.as_ref() {
        Some(crate::tui::event::ModalKind::PermissionPrompt { command, .. }) => {
            Some(command.as_str())
        }
        // Built-in web-tool domain prompts mark pending cards the same way, keyed
        // on the URL (the tool's `url` argument).
        Some(crate::tui::event::ModalKind::WebDomainPrompt { url, .. }) => Some(url.as_str()),
        // An extension tool prompt is keyed on its args preview,
        // matching the primary-arg text a generic tool card shows.
        Some(crate::tui::event::ModalKind::ExtensionToolPrompt { args_preview, .. }) => {
            Some(args_preview.as_str())
        }
        _ => None,
    }
}

pub(super) fn tool_waits_for_permission(
    activity: &ToolActivity,
    permission_command: Option<&str>,
) -> bool {
    if !matches!(activity.status, ToolStatus::Running) {
        return false;
    }
    let Some(permission_command) = permission_command else {
        return false;
    };
    // The argument carrying the prompted-on value differs by tool: bash prompts
    // on `command`, WebFetch on `url`. WebSearch authorizes its configured
    // endpoint, which is not a model argument.
    let arg_key = match activity.name.as_str() {
        "bash" => "command",
        "webfetch" => "url",
        "websearch" => return true,
        _ => return false,
    };
    let Ok(serde_json::Value::Object(map)) =
        serde_json::from_str::<serde_json::Value>(&activity.arguments)
    else {
        return false;
    };
    map.get(arg_key).and_then(serde_json::Value::as_str) == Some(permission_command)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BashOutputSummary {
    text: String,
    from_footer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedCommandSummary {
    command: String,
    exit_code: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    duration: String,
    stdout_bytes: usize,
    stderr_bytes: usize,
    saved_output: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandWorkflow {
    Format,
    Lint,
    Test,
    Build,
    Check,
    Git,
}

impl CommandWorkflow {
    fn label(self) -> &'static str {
        match self {
            Self::Format => "format",
            Self::Lint => "lint",
            Self::Test => "test",
            Self::Build => "build",
            Self::Check => "check",
            Self::Git => "git",
        }
    }
}

fn tool_activity_display_name(activity: &ToolActivity) -> String {
    if activity.name == "bash"
        && let Some(command) = bash_command_from_args(&activity.arguments)
        && let Some(workflow) = classify_command_workflow(&command)
    {
        return workflow.label().to_string();
    }
    // Surface which subagent a delegation runs, so `agent` rows read
    // `agent:explore` rather than a bare `agent`.
    if activity.name == "agent"
        && let Some(subagent) = json_string_arg(&activity.arguments, "agent")
    {
        return format!("agent:{subagent}");
    }
    activity.name.clone()
}

/// Extract a string-valued argument from a tool call's JSON arguments.
fn json_string_arg(arguments: &str, key: &str) -> Option<String> {
    let serde_json::Value::Object(map) = serde_json::from_str(arguments).ok()? else {
        return None;
    };
    map.get(key)?.as_str().map(str::to_string)
}

fn bash_output_summary(activity: &ToolActivity) -> Option<BashOutputSummary> {
    if activity.name != "bash" {
        return None;
    }
    let result = activity.result.as_ref()?;
    let body = tool_result_body(result);
    match activity.status {
        ToolStatus::Running => last_live_output_line(&body).map(|line| BashOutputSummary {
            text: compact_text(line, 80),
            from_footer: false,
        }),
        ToolStatus::Succeeded => {
            if let Some(parsed) = parse_command_summary(&body) {
                let workflow = bash_workflow_for_completed_activity(activity, &parsed.command);
                let mut text = parsed.row_summary(workflow);
                if let Some(warning) = first_warning_line(&body) {
                    text.push_str(" · ");
                    text.push_str(&compact_text(warning, 80));
                }
                Some(BashOutputSummary {
                    text,
                    from_footer: true,
                })
            } else {
                first_warning_line(&body).map(|line| BashOutputSummary {
                    text: compact_text(line, 80),
                    from_footer: false,
                })
            }
        }
        ToolStatus::Failed => None,
    }
}

impl ParsedCommandSummary {
    fn row_summary(&self, workflow: Option<CommandWorkflow>) -> String {
        let status = if self.timed_out {
            "timeout".to_string()
        } else if let Some(signal) = self.signal {
            format!("signal {signal}")
        } else if workflow.is_some() {
            match self.exit_code {
                Some(0) => "passed".to_string(),
                Some(exit_code) => format!("failed exit {exit_code}"),
                None => "failed exit none".to_string(),
            }
        } else if let Some(exit_code) = self.exit_code {
            format!("exit {exit_code}")
        } else {
            "exit none".to_string()
        };
        let mut summary = format!(
            "{status} · {} · out {} err {}",
            self.duration,
            format_compact_bytes(self.stdout_bytes),
            format_compact_bytes(self.stderr_bytes)
        );
        if self.saved_output {
            summary.push_str(" · saved");
        }
        summary
    }
}

fn parse_command_summary(text: &str) -> Option<ParsedCommandSummary> {
    let summary = command_summary_footer(text)?;
    let mut command = None;
    let mut exit_code = None;
    let mut signal = None;
    let mut timed_out = None;
    let mut duration = None;
    let mut stdout_bytes = None;
    let mut stderr_bytes = None;
    let mut saved_output = false;

    for line in summary.lines().map(str::trim) {
        if line == "last_output:" {
            break;
        }
        if let Some(value) = line.strip_prefix("command: ") {
            command = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("exit_code: ") {
            if value != "none" {
                exit_code = value.parse::<i32>().ok();
            }
        } else if let Some(value) = line.strip_prefix("signal: ") {
            signal = value.parse::<i32>().ok();
        } else if let Some(value) = line.strip_prefix("timed_out: ") {
            timed_out = Some(value == "true");
        } else if let Some(value) = line.strip_prefix("duration: ") {
            duration = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("stdout_bytes: ") {
            stdout_bytes = value.parse::<usize>().ok();
        } else if let Some(value) = line.strip_prefix("stderr_bytes: ") {
            stderr_bytes = value.parse::<usize>().ok();
        } else if let Some(value) = line.strip_prefix("saved_output: ") {
            saved_output = !value.is_empty();
        }
    }

    Some(ParsedCommandSummary {
        command: command?,
        exit_code,
        signal,
        timed_out: timed_out?,
        duration: duration?,
        stdout_bytes: stdout_bytes?,
        stderr_bytes: stderr_bytes?,
        saved_output,
    })
}

fn bash_command_from_args(arguments: &str) -> Option<String> {
    let serde_json::Value::Object(map) = serde_json::from_str(arguments).ok()? else {
        return None;
    };
    map.get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|command| !command.is_empty())
        .map(ToString::to_string)
}

fn bash_workflow_for_completed_activity(
    activity: &ToolActivity,
    footer_command: &str,
) -> Option<CommandWorkflow> {
    match bash_command_from_args(&activity.arguments) {
        Some(command) => classify_command_workflow(&command),
        None => classify_command_workflow(footer_command),
    }
}

fn classify_command_workflow(command: &str) -> Option<CommandWorkflow> {
    let words = display_shell_words(command)?;
    if words.iter().any(|word| display_word_may_execute(word)) {
        return None;
    }
    if words.iter().any(|word| is_shell_control_word(word)) {
        return None;
    }

    let mut index = skip_env_prefix(&words);
    let program = words.get(index)?.as_str();
    match program {
        "cargo" => {
            index += 1;
            if words
                .get(index)
                .is_some_and(|word| word.starts_with('+') && word.len() > 1)
            {
                index += 1;
            }
            match words.get(index).map(String::as_str) {
                Some("fmt") => Some(CommandWorkflow::Format),
                Some("clippy") => Some(CommandWorkflow::Lint),
                Some("test") => Some(CommandWorkflow::Test),
                Some("build") => Some(CommandWorkflow::Build),
                Some("check") => Some(CommandWorkflow::Check),
                _ => None,
            }
        }
        "git" => {
            index += 1;
            index = skip_git_global_options(&words, index);
            match words.get(index).map(String::as_str) {
                Some("status" | "diff") => Some(CommandWorkflow::Git),
                _ => None,
            }
        }
        _ => None,
    }
}

fn display_shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double_quote => in_single_quote = !in_single_quote,
            '"' if !in_single_quote => in_double_quote = !in_double_quote,
            '\\' if !in_single_quote => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '\n' | '\r' if !in_single_quote && !in_double_quote => {
                push_display_word(&mut words, &mut current);
                words.push(ch.to_string());
            }
            ch if ch.is_whitespace() && !in_single_quote && !in_double_quote => {
                push_display_word(&mut words, &mut current);
            }
            ch if display_control_char(ch) && !in_single_quote && !in_double_quote => {
                push_display_word(&mut words, &mut current);
                if matches!(ch, '&' | '|' | '>') && chars.peek().is_some_and(|next| *next == ch) {
                    let next = chars.next().unwrap_or(ch);
                    words.push(format!("{ch}{next}"));
                } else {
                    words.push(ch.to_string());
                }
            }
            _ => current.push(ch),
        }
    }

    if in_single_quote || in_double_quote {
        return None;
    }
    push_display_word(&mut words, &mut current);
    Some(words)
}

fn push_display_word(words: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        words.push(std::mem::take(current));
    }
}

fn display_control_char(ch: char) -> bool {
    matches!(ch, '|' | '&' | ';' | '<' | '>')
}

fn is_shell_control_word(word: &str) -> bool {
    matches!(
        word,
        "|" | "|&" | "||" | "&" | "&&" | ";" | "\n" | "\r" | "<" | ">" | ">>"
    )
}

fn display_word_may_execute(word: &str) -> bool {
    word.contains("$(") || word.contains('`') || word.contains("<(") || word.contains(">(")
}

fn skip_env_prefix(words: &[String]) -> usize {
    let mut index = 0;
    while words
        .get(index)
        .is_some_and(|word| is_display_env_assignment(word))
    {
        index += 1;
    }
    if words.get(index).is_some_and(|word| word == "env") {
        index += 1;
        while let Some(word) = words.get(index) {
            if is_display_env_assignment(word) {
                index += 1;
            } else if matches!(word.as_str(), "-u" | "--unset") {
                index = index.saturating_add(2);
            } else if word.starts_with("-u") && word.len() > 2 {
                index += 1;
            } else {
                break;
            }
        }
    }
    index
}

fn is_display_env_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn skip_git_global_options(words: &[String], mut index: usize) -> usize {
    while let Some(word) = words.get(index) {
        match word.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => {
                index = index.saturating_add(2);
            }
            _ if word.starts_with("--git-dir=")
                || word.starts_with("--work-tree=")
                || word.starts_with("--namespace=") =>
            {
                index += 1;
            }
            _ => break,
        }
    }
    index
}

fn command_summary_footer(text: &str) -> Option<&str> {
    const MARKER: &str = "[Command summary]";

    let mut search_end = text.len();
    while let Some(index) = text[..search_end].rfind(MARKER) {
        let summary = text[index + MARKER.len()..].trim_start_matches(['\r', '\n']);
        if has_command_summary_fields(summary) {
            return Some(summary);
        }
        search_end = index;
    }
    None
}

fn has_command_summary_fields(summary: &str) -> bool {
    let mut lines = summary.lines().map(str::trim);
    let Some(command) = lines.next() else {
        return false;
    };
    let Some(exit_code) = lines.next() else {
        return false;
    };
    let mut next = lines.next();

    if next.is_some_and(|line| line.starts_with("signal: ")) {
        next = lines.next();
    }
    let Some(timed_out) = next else {
        return false;
    };
    next = lines.next();
    if next.is_some_and(|line| line.starts_with("timeout_seconds: ")) {
        next = lines.next();
    }
    let Some(duration) = next else {
        return false;
    };
    let Some(stdout_bytes) = lines.next() else {
        return false;
    };
    let Some(stderr_bytes) = lines.next() else {
        return false;
    };
    let Some(combined_output_chars) = lines.next() else {
        return false;
    };
    next = lines.next();
    if next.is_some_and(|line| line.starts_with("saved_output: ")) {
        next = lines.next();
    }
    matches!(
        (
            command.starts_with("command: "),
            exit_code.starts_with("exit_code: "),
            timed_out.starts_with("timed_out: "),
            duration.starts_with("duration: "),
            stdout_bytes.starts_with("stdout_bytes: "),
            stderr_bytes.starts_with("stderr_bytes: "),
            combined_output_chars.starts_with("combined_output_chars: "),
            next,
        ),
        (
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            Some("last_output:")
        )
    )
}

fn format_compact_bytes(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * KB;
    const GB: usize = 1024 * MB;

    if bytes < KB {
        format!("{bytes}B")
    } else if bytes < MB {
        format_scaled_bytes(bytes, KB, "KB")
    } else if bytes < GB {
        format_scaled_bytes(bytes, MB, "MB")
    } else {
        format_scaled_bytes(bytes, GB, "GB")
    }
}

fn format_scaled_bytes(bytes: usize, unit: usize, suffix: &str) -> String {
    let value = bytes as f64 / unit as f64;
    if value < 10.0 && !bytes.is_multiple_of(unit) {
        format!("{value:.1}{suffix}")
    } else {
        format!("{value:.0}{suffix}")
    }
}

fn first_warning_line(text: &str) -> Option<&str> {
    text.lines()
        .map(str::trim)
        .find(|line| is_warning_line(line))
}

fn is_warning_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("warning") else {
        return false;
    };
    rest.is_empty() || matches!(rest.chars().next(), Some(':') | Some('[') | Some(' '))
}

fn last_live_output_line(text: &str) -> Option<&str> {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty() && !line.starts_with("[Live output truncated:"))
}

/// The (key, value) pair promoted to the tool one-liner: the first
/// `PRIMARY_ARG_KEYS` hit, falling back to the first argument present.
pub(super) fn primary_arg_value(arguments: &str) -> Option<(String, String)> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str(arguments) else {
        let compact = compact_text(arguments, 64);
        if compact.is_empty() || compact == "{}" {
            return None;
        }
        return Some(("raw".to_string(), compact));
    };
    let key = PRIMARY_ARG_KEYS
        .iter()
        .find(|key| map.contains_key(**key))
        .map(|key| key.to_string())
        .or_else(|| map.keys().next().cloned())?;
    let value = map.get(&key)?;
    if key == "todos" {
        return Some((key, todo_summary_text(value)));
    }
    let text = json_value_text(value);
    if text.is_empty() {
        return None;
    }
    Some((key, text))
}

/// Keys promoted to the prominent first position of a tool card, in order.
const PRIMARY_ARG_KEYS: [&str; 7] = [
    "command",
    "op",
    "file_path",
    "path",
    "pattern",
    "query",
    "prompt",
];

/// Renders tool arguments as a styled summary line instead of raw JSON:
/// the primary value (command, path, pattern) gets a semantic color and the
/// remaining keys trail behind it dimmed.
#[cfg(test)]
pub(crate) fn arg_summary_lines(arguments: &str) -> Vec<Line<'static>> {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str(arguments) else {
        let compact = compact_text(arguments, 160);
        if compact.is_empty() || compact == "{}" {
            return Vec::new();
        }
        return vec![Line::from(Span::styled(
            compact,
            Style::default().fg(theme::palette().muted),
        ))];
    };
    if map.is_empty() {
        return Vec::new();
    }

    let primary_key = PRIMARY_ARG_KEYS
        .iter()
        .find(|key| map.contains_key(**key))
        .map(|key| key.to_string())
        .or_else(|| map.keys().next().cloned());

    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(key) = &primary_key
        && let Some(value) = map.get(key)
    {
        spans.extend(primary_arg_spans(key, value));
    }

    let rest: Vec<String> = map
        .iter()
        .filter(|(key, _)| Some(*key) != primary_key.as_ref())
        .map(|(key, value)| format!("{key} {}", json_value_text(value)))
        .collect();
    if !rest.is_empty() {
        if !spans.is_empty() {
            spans.push(Span::styled(
                "  ·  ".to_string(),
                Style::default().fg(theme::palette().dim),
            ));
        }
        spans.push(Span::styled(
            rest.join(" · "),
            Style::default().fg(theme::palette().dim),
        ));
    }
    if spans.is_empty() {
        Vec::new()
    } else {
        vec![Line::from(spans)]
    }
}

pub(super) fn primary_arg_spans(key: &str, value: &serde_json::Value) -> Vec<Span<'static>> {
    let text = json_value_text(value);
    match key {
        "command" => vec![
            Span::styled(
                "$ ".to_string(),
                Style::default()
                    .fg(theme::palette().command)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(text, Style::default().fg(theme::palette().command)),
        ],
        "file_path" | "path" => vec![Span::styled(
            text,
            Style::default().fg(theme::palette().path),
        )],
        "pattern" | "query" => vec![Span::styled(
            text,
            Style::default().fg(theme::palette().user),
        )],
        "todos" => vec![Span::styled(
            text,
            Style::default().fg(theme::palette().todo),
        )],
        _ => vec![Span::styled(
            text,
            Style::default().fg(theme::palette().text),
        )],
    }
}

/// One-line summary for the todowrite tool's `todos` array: the in-progress
/// item's content (the most informative single line), or a count when none is
/// in progress. Falls back to a count for malformed arrays.
pub(super) fn todo_summary_text(value: &serde_json::Value) -> String {
    let Some(items) = value.as_array() else {
        return "updating todos".to_string();
    };
    if items.is_empty() {
        return "updating todos".to_string();
    }
    let in_progress = items.iter().find_map(|item| {
        let obj = item.as_object()?;
        let status = obj.get("status")?.as_str()?;
        if status == "in_progress" {
            obj.get("content")?.as_str().map(|s| s.to_string())
        } else {
            None
        }
    });
    if let Some(content) = in_progress {
        return content;
    }
    format!(
        "{} todo{}",
        items.len(),
        if items.len() == 1 { "" } else { "s" }
    )
}

pub(super) fn json_value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

pub(super) fn tool_status_style(status: ToolStatus) -> (Color, Color) {
    let accent = match status {
        ToolStatus::Running => theme::palette().todo,
        ToolStatus::Succeeded => theme::palette().success,
        ToolStatus::Failed => theme::palette().error,
    };
    (accent, theme::palette().tool_block)
}

pub(super) fn format_duration(duration: std::time::Duration) -> String {
    let millis = duration.as_millis();
    if duration > std::time::Duration::ZERO && millis == 0 {
        "<1ms".to_string()
    } else if millis < 1_000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", millis as f64 / 1_000.0)
    }
}

/// Duration worth showing on a tool row: `None` below 100ms (instant local
/// tools whose `<1ms`/`12ms` stamps only add noise), the formatted string
/// otherwise. Slow calls keep their duration so a long-running tool still reads.
pub(super) fn meaningful_duration(duration: std::time::Duration) -> Option<String> {
    (duration >= std::time::Duration::from_millis(100)).then(|| format_duration(duration))
}

pub(super) fn compact_text(text: &str, max_chars: usize) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= max_chars {
        flattened
    } else {
        format!(
            "{}...",
            flattened.chars().take(max_chars).collect::<String>()
        )
    }
}

#[cfg(test)]
mod reused_read_label_tests {
    use super::*;
    use std::time::Instant;

    fn read_activity(result: Option<&str>) -> ToolActivity {
        let mut activity = ToolActivity::new(
            "call-1".to_string(),
            "read".to_string(),
            r#"{"path":"src/main.rs"}"#.to_string(),
            Instant::now(),
        );
        activity.status = ToolStatus::Succeeded;
        activity.result = result.map(str::to_string);
        activity
    }

    fn summary_text(activity: &ToolActivity) -> String {
        tool_activity_summary_spans(activity, None, 0)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn reuse_pointer_result_shows_reused_label() {
        let result = format!(
            "{} src/main.rs lines 1-10 — unchanged since tool call-0",
            crate::agent::REUSED_READ_MARKER
        );
        assert!(summary_text(&read_activity(Some(&result))).contains("reused previous inspection"));
    }

    #[test]
    fn normal_read_result_has_no_reused_label() {
        assert!(
            !summary_text(&read_activity(Some("1: fn main() {}")))
                .contains("reused previous inspection")
        );
    }
}

#[cfg(test)]
mod command_workflow_tests {
    use super::*;

    #[test]
    fn classifies_cargo_workflows() {
        assert_eq!(
            classify_command_workflow("cargo fmt --all -- --check"),
            Some(CommandWorkflow::Format)
        );
        assert_eq!(
            classify_command_workflow("cargo clippy --all-targets -- -D warnings"),
            Some(CommandWorkflow::Lint)
        );
        assert_eq!(
            classify_command_workflow("cargo test --locked"),
            Some(CommandWorkflow::Test)
        );
        assert_eq!(
            classify_command_workflow("cargo build --release --locked"),
            Some(CommandWorkflow::Build)
        );
        assert_eq!(
            classify_command_workflow("cargo check"),
            Some(CommandWorkflow::Check)
        );
    }

    #[test]
    fn classifies_env_prefixed_and_toolchain_cargo_commands() {
        assert_eq!(
            classify_command_workflow("RUSTFLAGS='-D warnings' cargo test --locked"),
            Some(CommandWorkflow::Test)
        );
        assert_eq!(
            classify_command_workflow("env CARGO_TERM_COLOR=never cargo +nightly clippy"),
            Some(CommandWorkflow::Lint)
        );
    }

    #[test]
    fn leaves_unknown_or_complex_shell_commands_generic() {
        assert_eq!(classify_command_workflow("cargo metadata"), None);
        assert_eq!(classify_command_workflow("cargo test && cargo fmt"), None);
        assert_eq!(classify_command_workflow("cargo test\ncargo fmt"), None);
        assert_eq!(classify_command_workflow("cargo test\rcargo fmt"), None);
        assert_eq!(
            classify_command_workflow("FOO=$(touch pwned) cargo test"),
            None
        );
    }

    #[test]
    fn classifies_read_only_git_shell_fallbacks() {
        assert_eq!(
            classify_command_workflow("git status --short"),
            Some(CommandWorkflow::Git)
        );
        assert_eq!(
            classify_command_workflow("git -C crate diff --stat"),
            Some(CommandWorkflow::Git)
        );
        assert_eq!(classify_command_workflow("git push origin main"), None);
    }
}
