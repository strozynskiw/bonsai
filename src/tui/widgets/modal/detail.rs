use super::common::*;
use super::context::*;
use super::*;

pub(super) fn render_tool_detail(f: &mut Frame, area: Rect, app: &AppState, tool_id: &str) {
    let Some(activity) = app.tool_activity(tool_id) else {
        render_modal_notice(
            f,
            area,
            "Tool Detail",
            "Tool activity is no longer available.",
        );
        return;
    };

    let title = format!("Tool: {}", activity.name);
    let body_width = area.width.saturating_sub(6) as usize;
    let body_lines = tool_detail_lines(activity, app.subagent_model_for(tool_id), body_width);
    // Advertise the diff-preview shortcut only when this tool actually carries
    // a diff (the action no-ops otherwise).
    let mut hints: Vec<(&str, &str)> = vec![("Up/Down", "scroll")];
    if activity.diff.is_some() {
        hints.push(("d", "diff"));
    }
    hints.push(("d-click", "copy word"));
    hints.push(("Esc", "close"));
    let footer_lines = vec![footer_hint_line(&hints)];
    render_scrollable_modal(
        f,
        area,
        &title,
        &[],
        &body_lines,
        &footer_lines,
        app.modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
}

pub(super) fn render_block_detail(f: &mut Frame, area: Rect, app: &AppState, item_index: usize) {
    let Some(item) = app.transcript.get(item_index) else {
        render_modal_notice(
            f,
            area,
            "Block Detail",
            "Transcript block is no longer available.",
        );
        return;
    };

    if let TranscriptItem::ToolActivity(activity) = item {
        render_tool_detail(f, area, app, &activity.id);
        return;
    }

    let body = crate::copy::body_text_for(item);
    let mut body_lines = if body.is_empty() {
        vec![Line::from(Span::styled("(empty)", theme::dim()))]
    } else {
        transcript::render_markdown(&body, area.width.saturating_sub(6) as usize)
    };
    if body_lines.is_empty() {
        body_lines.push(Line::from(Span::styled("(empty)", theme::dim())));
    }

    let footer_lines = vec![footer_hint_line(&[
        ("Up/Down", "scroll"),
        ("d-click", "copy word"),
        ("Esc", "close"),
    ])];
    render_scrollable_modal(
        f,
        area,
        &block_detail_title(item),
        &[],
        &body_lines,
        &footer_lines,
        app.modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
}

pub(super) fn render_plan_finding_detail(f: &mut Frame, area: Rect, app: &AppState, index: usize) {
    let ordered = app.plan.findings_in_severity_order();
    // Clamp to the live count so a list that shrank while the modal was open
    // shows the last finding rather than an empty "no longer available" pane.
    let index = index.min(ordered.len().saturating_sub(1));
    let Some(finding) = ordered.get(index) else {
        render_modal_notice(f, area, "Finding", "Finding is no longer available.");
        return;
    };

    let title = format!(
        "Finding {}/{} · {}",
        index + 1,
        ordered.len(),
        finding.severity.label()
    );
    let body = plan_finding_detail_lines(finding, area.width.saturating_sub(6) as usize);

    let has_evidence = finding
        .source_ids
        .iter()
        .any(|id| app.tool_activity(id).is_some());
    let mut hints: Vec<(&str, &str)> = vec![("Left/Right", "findings")];
    if has_evidence {
        hints.push(("Enter", "open evidence"));
    }
    hints.push(("d-click", "copy word"));
    hints.push(("Esc", "close"));

    render_scrollable_modal(
        f,
        area,
        &title,
        &[],
        &body,
        &[footer_hint_line(&hints)],
        app.modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
}

/// The scrollable body of the finding-detail modal: severity/location/status/
/// task header, then the issue, required-fix, acceptance, and evidence
/// sections. Pure data shaping so it can be asserted without a backend.
pub(super) fn plan_finding_detail_lines(
    finding: &crate::plan::Finding,
    body_width: usize,
) -> Vec<Line<'static>> {
    let severity_color = crate::tui::widgets::plan::severity_color(finding.severity);
    let mut body = vec![kv("severity", finding.severity.label(), severity_color)];
    if let Some(loc) = finding.location_label() {
        body.push(kv("location", &loc, theme::palette().text));
    }
    if finding.resolved {
        body.push(kv("status", "resolved", theme::palette().muted));
    }
    body.push(kv(
        "task",
        finding
            .task
            .as_deref()
            .unwrap_or(crate::plan::FINDING_UNASSIGNED),
        theme::palette().text,
    ));

    let section = |label: &str, text: &str, body: &mut Vec<Line<'static>>| {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            label.to_string(),
            theme::label(theme::palette().tool),
        )));
        body.extend(transcript::render_markdown(text, body_width));
    };
    section("issue", &finding.issue, &mut body);
    section("required fix", &finding.required_fix, &mut body);
    if !finding.acceptance_tests.is_empty() {
        body.push(Line::from(""));
        body.push(Line::from(Span::styled(
            "acceptance".to_string(),
            theme::label(theme::palette().tool),
        )));
        for test in &finding.acceptance_tests {
            body.push(Line::from(format!("• {test}")));
        }
    }
    if !finding.source_ids.is_empty() {
        body.push(Line::from(""));
        body.push(kv(
            "evidence",
            &finding.source_ids.join(", "),
            theme::palette().muted,
        ));
    }
    body
}

pub(super) fn block_detail_title(item: &TranscriptItem) -> String {
    match item {
        TranscriptItem::UserMessage { .. } => "User Message".to_string(),
        TranscriptItem::QueuedUserMessage { .. } => "Queued Message".to_string(),
        TranscriptItem::AssistantMessage { .. } => "Assistant Message".to_string(),
        TranscriptItem::ReasoningSummary { .. } => "Thinking".to_string(),
        TranscriptItem::ToolActivity(activity) => format!("Tool: {}", activity.name),
        TranscriptItem::ExecutionGroup { .. } => "Execution Group".to_string(),
        TranscriptItem::CommandOutput { kind, .. } => match kind {
            crate::tui::event::CommandOutputKind::Status => "Status".to_string(),
            crate::tui::event::CommandOutputKind::CompactionStatus => {
                "Context Compacted".to_string()
            }
            crate::tui::event::CommandOutputKind::Error => "Command Error".to_string(),
        },
        TranscriptItem::CompletionReport(_) => "Completion Report".to_string(),
        TranscriptItem::Error { error } => format!("Error: {}", error.title),
        TranscriptItem::Edit { .. } => "Edit".to_string(),
        TranscriptItem::TodoUpdate { .. } => "Todo Update".to_string(),
        TranscriptItem::WorkLog { .. } => "Work Note".to_string(),
        TranscriptItem::PeerMessage {
            session_id,
            outgoing,
            ..
        } => {
            if *outgoing {
                format!("Peer Message → #{session_id}")
            } else {
                format!("Peer Message ← #{session_id}")
            }
        }
    }
}

pub(super) fn render_diff_preview(f: &mut Frame, area: Rect, app: &AppState, tool_id: &str) {
    let Some(activity) = app.tool_activity(tool_id) else {
        render_modal_notice(f, area, "Diff", "Tool activity is no longer available.");
        return;
    };
    let Some(diff) = activity.diff.as_ref() else {
        render_modal_notice(f, area, "Diff", "This tool has no associated diff.");
        return;
    };

    // render_scrollable_modal clamps the scroll on *wrapped* rows, so a diff
    // line wider than the modal stays reachable — the old logical-line clamp
    // here left the tail of any wrapped line below the fold.
    let title = if diff.file_count() == 1 {
        format!("Diff: {}", diff.path)
    } else {
        format!("Diff: {} files", diff.file_count())
    };
    let footer_lines = vec![footer_hint_line(&[
        ("Up/Down", "scroll"),
        ("d-click", "copy word"),
        ("Esc", "close"),
    ])];
    render_scrollable_modal(
        f,
        area,
        &title,
        &[],
        &diff_lines(diff),
        &footer_lines,
        app.modal_scroll,
        app.modal_selection.map(|s| s.range()),
        &app.modal_body_lines,
        &app.modal_body_rect,
    );
}

pub(super) fn max_diff_preview_scroll(area: Rect, diff: &FileDiff) -> u16 {
    scrollable_modal_max_scroll(area, 0, 1, &diff_lines(diff))
}

pub(super) fn diff_lines(diff: &FileDiff) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, file) in diff.files().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(single_file_diff_lines(file));
    }
    lines
}

fn single_file_diff_lines(diff: &FileDiff) -> Vec<Line<'static>> {
    let mut lines = vec![transcript::diff_header_line(diff)];
    if let Some(old_size) = diff.old_size {
        lines.push(kv(
            "size",
            &format!("{} → {} bytes", old_size, diff.new_size),
            theme::palette().text,
        ));
    } else {
        lines.push(kv(
            "size",
            &format!("{} bytes", diff.new_size),
            theme::palette().text,
        ));
    }
    if diff.truncated {
        lines.push(Line::from(Span::styled(
            "diff preview truncated; open file or use git diff for full output.",
            theme::dim(),
        )));
    }
    lines.push(Line::from(""));
    lines.extend(transcript::diff_lines(diff, DIFF_PREVIEW_MAX_ROWS));
    lines
}

pub(super) fn tool_detail_lines(
    activity: &ToolActivity,
    subagent_model: Option<&str>,
    body_width: usize,
) -> Vec<Line<'static>> {
    // Authorization decisions stream into the same result string as tool
    // output (see merge_authorization_output); pull them out first so approval
    // context and tool output land in separate sections instead of one blob.
    let (authorization, result_body) = match activity.result.as_deref() {
        Some(result) => {
            let (authorization, body) = split_authorization_result(result);
            (authorization, Some(body))
        }
        None => (Vec::new(), None),
    };

    let mut lines = vec![
        kv(
            "status",
            activity.status.label(),
            tool_status_color(activity.status),
        ),
        kv("id", &activity.id, theme::palette().muted),
        kv(
            "duration",
            &format_duration(activity.duration()),
            theme::palette().text,
        ),
    ];

    // The model of the subagent run an `agent` call launched (its override or
    // the inherited model) — the parent turn's model would be the same for
    // every call in the turn and belongs in the usage view, not here.
    if let Some(model) = subagent_model {
        lines.push(kv("model", model, theme::palette().text));
    }

    let args = tool_argument_lines(activity);
    if !args.is_empty() {
        lines.push(Line::from(""));
        lines.push(section_header("arguments"));
        lines.extend(args);
    }

    if !authorization.is_empty() {
        lines.push(Line::from(""));
        lines.push(section_header("authorization"));
        lines.extend(authorization_section_lines(&authorization));
    }

    if let Some(diff) = &activity.diff {
        lines.push(Line::from(""));
        lines.push(section_header("changes"));
        for (index, file) in diff.files().enumerate() {
            if index > 0 {
                lines.push(Line::from(""));
            }
            lines.push(transcript::diff_header_line(file));
            lines.extend(transcript::diff_lines(file, TOOL_DETAIL_DIFF_MAX_ROWS));
        }
    }

    lines.push(Line::from(""));
    lines.extend(tool_result_section_lines(
        activity,
        result_body.as_deref(),
        body_width,
    ));
    lines
}

/// Splits `[authorization]` decision lines out of a tool result. Returns the
/// decision payloads (marker stripped) and the remaining result body.
pub(super) fn split_authorization_result(result: &str) -> (Vec<String>, String) {
    const MARKER: &str = "[authorization]";
    let mut authorization = Vec::new();
    let mut body = Vec::new();
    for line in result.lines() {
        if let Some(rest) = line.strip_prefix(MARKER) {
            authorization.push(rest.trim().to_string());
        } else {
            body.push(line);
        }
    }
    (
        authorization,
        body.join("\n").trim_matches('\n').to_string(),
    )
}

/// Renders each authorization decision as labeled rows. The wire format is
/// `decision · tier · effects · rule-source · autonomy=… · sandbox=… · reason`
/// (see ActionGovernor::record); unknown shapes fall back to a single row so
/// older or partial lines still render.
pub(super) fn authorization_section_lines(entries: &[String]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            lines.push(Line::from(""));
        }
        lines.extend(authorization_entry_lines(entry));
    }
    lines
}

fn authorization_entry_lines(entry: &str) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let mut segments = entry
        .split(" · ")
        .map(str::trim)
        .filter(|segment| !segment.is_empty());
    let Some(decision) = segments.next() else {
        return Vec::new();
    };

    let decision_color = if decision.starts_with("allow") || decision.starts_with("auto") {
        palette.success
    } else if decision.starts_with("deny") || decision.starts_with("block") {
        palette.error
    } else {
        palette.text
    };
    let mut lines = vec![kv("decision", decision, decision_color)];

    let mut autonomy = None;
    let mut sandbox = None;
    let mut positional = Vec::new();
    for segment in segments {
        if let Some(value) = segment.strip_prefix("autonomy=") {
            autonomy = Some(value);
        } else if let Some(value) = segment.strip_prefix("sandbox=") {
            sandbox = Some(value);
        } else {
            positional.push(segment);
        }
    }

    if positional.len() >= 3 {
        lines.push(kv("risk", positional[0], palette.text));
        lines.push(kv(
            "effects",
            &positional[1].replace(',', ", "),
            palette.text,
        ));
        lines.push(kv("rule", positional[2], palette.text));
        if positional.len() > 3 {
            lines.push(kv("note", &positional[3..].join(" · "), palette.muted));
        }
    } else if !positional.is_empty() {
        lines.push(kv("detail", &positional.join(" · "), palette.text));
    }
    if let Some(autonomy) = autonomy {
        lines.push(kv("autonomy", autonomy, palette.text));
    }
    if let Some(sandbox) = sandbox {
        lines.push(kv("sandbox", sandbox, palette.text));
    }
    lines
}

pub(super) fn max_tool_detail_scroll(
    area: Rect,
    activity: &ToolActivity,
    subagent_model: Option<&str>,
) -> u16 {
    let body_lines = tool_detail_lines(
        activity,
        subagent_model,
        area.width.saturating_sub(6) as usize,
    );
    scrollable_modal_max_scroll(area, 0, 1, &body_lines)
}

pub(super) fn tool_argument_lines(activity: &ToolActivity) -> Vec<Line<'static>> {
    match activity.name.as_str() {
        "write" => write_argument_lines(&activity.arguments)
            .unwrap_or_else(|| generic_tool_argument_lines(&activity.arguments)),
        "edit" => edit_argument_lines(&activity.arguments)
            .unwrap_or_else(|| generic_tool_argument_lines(&activity.arguments)),
        _ => generic_tool_argument_lines(&activity.arguments),
    }
}

pub(super) fn write_argument_lines(arguments: &str) -> Option<Vec<Line<'static>>> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let map = value.as_object()?;
    let path = map.get("path")?.as_str()?;
    let content = map.get("content")?.as_str()?;
    let line_count = text_line_count(content);

    Some(vec![
        argument_value_line("file", path, theme::palette().path),
        argument_value_line(
            "write",
            &format!(
                "{} bytes, {}",
                group_thousands(content.len()),
                plural_count(line_count, "line")
            ),
            theme::palette().text,
        ),
        argument_preview_line("new", theme::palette().added, content),
    ])
}

pub(super) fn edit_argument_lines(arguments: &str) -> Option<Vec<Line<'static>>> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    let map = value.as_object()?;
    let path = map.get("path")?.as_str()?;
    if let Some(edits) = map.get("edits").and_then(serde_json::Value::as_array) {
        return edit_region_argument_lines(path, edits);
    }
    let old = map.get("old_string")?.as_str()?;
    let new = map.get("new_string")?.as_str()?;
    let replace_all = map
        .get("replace_all")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let action = if old.is_empty() {
        "create"
    } else if new.is_empty() {
        "delete"
    } else if replace_all {
        "replace all"
    } else {
        "replace"
    };

    let mut lines = vec![
        argument_value_line("file", path, theme::palette().path),
        argument_value_line("action", action, theme::palette().edit),
    ];

    if old.is_empty() {
        lines.push(argument_preview_line("new", theme::palette().added, new));
    } else if new.is_empty() {
        lines.push(argument_preview_line("old", theme::palette().removed, old));
    } else {
        lines.push(argument_preview_line("old", theme::palette().removed, old));
        lines.push(argument_preview_line("new", theme::palette().added, new));
    }

    Some(lines)
}

pub(super) fn edit_region_argument_lines(
    path: &str,
    edits: &[serde_json::Value],
) -> Option<Vec<Line<'static>>> {
    let mut lines = vec![
        argument_value_line("file", path, theme::palette().path),
        argument_value_line(
            "edits",
            &plural_count(edits.len(), "edit"),
            theme::palette().edit,
        ),
    ];

    for (index, edit) in edits.iter().enumerate() {
        let edit = edit.as_object()?;
        let old = edit.get("old_string")?.as_str()?;
        let new = edit.get("new_string")?.as_str()?;
        let replace_all = edit
            .get("replace_all")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let number = index + 1;
        let old_label = if replace_all {
            format!("old #{number} (all)")
        } else {
            format!("old #{number}")
        };
        lines.push(argument_preview_line(
            &old_label,
            theme::palette().removed,
            old,
        ));
        lines.push(argument_preview_line(
            &format!("new #{number}"),
            theme::palette().added,
            new,
        ));
    }

    Some(lines)
}

pub(super) fn argument_value_line(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(padded_label(label), theme::muted()),
        Span::styled(value.to_string(), theme::body(color)),
    ])
}

pub(super) fn argument_preview_line(label: &str, color: Color, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(padded_label(label), theme::body(color)),
        Span::styled(quoted_preview(text), theme::body(theme::palette().text)),
    ])
}

/// The shared label gutter for kv rows. Pads to a common column but always
/// keeps at least one space before the value — a 10-char label such as
/// "background" would otherwise fuse with its value ("backgroundtrue").
fn padded_label(label: &str) -> String {
    format!("{label:<10} ")
}

pub(super) fn generic_tool_argument_lines(arguments: &str) -> Vec<Line<'static>> {
    let trimmed = arguments.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Vec::new();
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return generic_json_argument_lines(&value);
    }

    vec![argument_value_line(
        "raw",
        &preview_text(trimmed),
        theme::palette().muted,
    )]
}

pub(super) fn generic_json_argument_lines(value: &serde_json::Value) -> Vec<Line<'static>> {
    match value {
        serde_json::Value::Object(map) => generic_object_argument_lines(map),
        serde_json::Value::Array(items) => {
            vec![argument_value_line(
                "items",
                &array_summary("items", items),
                theme::palette().text,
            )]
        }
        _ => vec![argument_value_line(
            "value",
            &argument_value_text("value", value),
            theme::palette().text,
        )],
    }
}

pub(super) fn generic_object_argument_lines(
    map: &serde_json::Map<String, serde_json::Value>,
) -> Vec<Line<'static>> {
    ordered_argument_keys(map)
        .into_iter()
        .filter_map(|key| {
            let value = map.get(key)?;
            let label = argument_label(key);
            let text = argument_value_text(key, value);
            (!text.is_empty())
                .then(|| argument_value_line(&label, &text, argument_value_color(key)))
        })
        .collect()
}

const MODAL_ARG_KEY_ORDER: [&str; 28] = [
    "command",
    "file_path",
    "path",
    "pattern",
    "query",
    "prompt",
    "question",
    "title",
    "heading",
    "action",
    "operation",
    "text",
    "target",
    "position",
    "task_id",
    "wait_seconds",
    "glob",
    "kind",
    "offset",
    "limit",
    "depth",
    "timeout_secs",
    "run_in_background",
    "parallel",
    "content",
    "body",
    "todos",
    "options",
];

pub(super) fn ordered_argument_keys(map: &serde_json::Map<String, serde_json::Value>) -> Vec<&str> {
    let mut keys = Vec::with_capacity(map.len());
    for key in MODAL_ARG_KEY_ORDER {
        if map.contains_key(key) {
            keys.push(key);
        }
    }
    for key in map.keys() {
        if !keys.contains(&key.as_str()) {
            keys.push(key);
        }
    }
    keys
}

pub(super) fn argument_label(key: &str) -> String {
    match key {
        "file_path" | "path" => "file".to_string(),
        "old_string" => "old".to_string(),
        "new_string" => "new".to_string(),
        "old_text" => "old text".to_string(),
        "new_text" => "new text".to_string(),
        "run_in_background" => "background".to_string(),
        "timeout_secs" => "timeout".to_string(),
        "wait_seconds" => "wait".to_string(),
        _ => key.replace('_', " "),
    }
}

pub(super) fn argument_value_color(key: &str) -> Color {
    match key {
        "command" => theme::palette().command,
        "file_path" | "path" => theme::palette().path,
        "pattern" | "query" | "prompt" | "question" => theme::palette().user,
        "todos" => theme::palette().todo,
        "old_string" | "old_text" => theme::palette().removed,
        "new_string" | "new_text" => theme::palette().added,
        "action" | "operation" => theme::palette().edit,
        _ => theme::palette().text,
    }
}

pub(super) fn argument_value_text(key: &str, value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) if argument_string_is_unquoted(key) => preview_text(text),
        serde_json::Value::String(text) => quoted_preview(text),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::Array(items) => array_summary(key, items),
        serde_json::Value::Object(map) => object_summary(map),
        serde_json::Value::Null => "null".to_string(),
    }
}

pub(super) fn argument_string_is_unquoted(key: &str) -> bool {
    matches!(key, "command" | "file_path" | "path")
}

pub(super) fn array_summary(key: &str, items: &[serde_json::Value]) -> String {
    if items.is_empty() {
        return "0 items".to_string();
    }
    if key == "todos"
        && let Some(summary) = todo_array_summary(items)
    {
        return summary;
    }
    if key == "options"
        && let Some(summary) = option_array_summary(items)
    {
        return summary;
    }

    let count = plural_count(items.len(), "item");
    let Some(first) = items.first() else {
        return count;
    };
    let preview = match first {
        serde_json::Value::Object(map) => object_keys_summary(map),
        serde_json::Value::Array(values) => plural_count(values.len(), "nested item"),
        _ => argument_value_text(key, first),
    };
    if preview.is_empty() {
        count
    } else {
        format!("{count}: {preview}")
    }
}

pub(super) fn todo_array_summary(items: &[serde_json::Value]) -> Option<String> {
    let first = items.first()?.as_object()?;
    let content = first.get("content")?.as_str()?;
    let status = first.get("status").and_then(serde_json::Value::as_str);
    let mut summary = format!(
        "{}: {}",
        plural_count(items.len(), "todo"),
        preview_text(content)
    );
    if let Some(status) = status.filter(|status| !status.is_empty()) {
        summary.push_str(&format!(" ({status})"));
    }
    Some(summary)
}

pub(super) fn option_array_summary(items: &[serde_json::Value]) -> Option<String> {
    let labels = items
        .iter()
        .filter_map(|item| item.get("label").and_then(serde_json::Value::as_str))
        .take(3)
        .map(preview_text)
        .collect::<Vec<_>>();
    if labels.is_empty() {
        return None;
    }
    let mut summary = format!(
        "{}: {}",
        plural_count(items.len(), "option"),
        labels.join(", ")
    );
    if items.len() > labels.len() {
        summary.push_str(", ...");
    }
    Some(summary)
}

pub(super) fn object_summary(map: &serde_json::Map<String, serde_json::Value>) -> String {
    let field_count = plural_count(map.len(), "field");
    let keys = object_keys_summary(map);
    if keys.is_empty() {
        field_count
    } else {
        format!("{field_count}: {keys}")
    }
}

pub(super) fn object_keys_summary(map: &serde_json::Map<String, serde_json::Value>) -> String {
    let keys = map.keys().take(3).map(String::as_str).collect::<Vec<_>>();
    if keys.is_empty() {
        return String::new();
    }
    let mut summary = keys.join(", ");
    if map.len() > keys.len() {
        summary.push_str(", ...");
    }
    summary
}

pub(super) fn text_line_count(text: &str) -> usize {
    text.lines().count()
}

pub(super) fn plural_count(count: usize, singular: &str) -> String {
    let suffix = if count == 1 { "" } else { "s" };
    format!("{} {singular}{suffix}", group_thousands(count))
}

pub(super) fn quoted_preview(text: &str) -> String {
    format!("\"{}\"", preview_text(text))
}

pub(super) fn preview_text(text: &str) -> String {
    let escaped = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('"', "\\\"");
    if escaped.chars().count() <= ARG_PREVIEW_MAX_CHARS {
        escaped
    } else {
        format!(
            "{}...",
            escaped
                .chars()
                .take(ARG_PREVIEW_MAX_CHARS)
                .collect::<String>()
        )
    }
}

pub(super) fn tool_result_lines(result: &str) -> Vec<Line<'static>> {
    let body = transcript::tool_result_body(result);
    if body.trim().is_empty() {
        return vec![Line::from(Span::styled("(empty result)", theme::dim()))];
    }

    body.lines()
        .map(|line| {
            if let Some((number, text)) = read_result_line(line) {
                Line::from(vec![
                    Span::styled(format!("{number:>4}"), theme::muted()),
                    Span::styled(": ", theme::dim()),
                    Span::styled(text.to_string(), theme::body(theme::palette().text)),
                ])
            } else if line.starts_with('[') && line.ends_with(']') {
                Line::from(Span::styled(line.to_string(), theme::dim()))
            } else {
                Line::from(Span::styled(
                    line.to_string(),
                    theme::body(theme::palette().text),
                ))
            }
        })
        .collect()
}

/// The result portion of the tool-detail modal, section header included. Agent
/// tools carry a structured subagent report that gets its own sections; every
/// other tool renders a plain `result` section.
fn tool_result_section_lines(
    activity: &ToolActivity,
    result_body: Option<&str>,
    body_width: usize,
) -> Vec<Line<'static>> {
    let is_agent = activity.name == "agent";
    let header = if is_agent {
        "subagent response"
    } else {
        "result"
    };
    let running = matches!(activity.status, ToolStatus::Running);

    let body = result_body.map(str::trim).unwrap_or("");
    if body.is_empty() {
        let placeholder = match (running, is_agent) {
            (true, true) => "subagent still running",
            (true, false) => "still running",
            (false, _) => "(empty result)",
        };
        return vec![
            section_header(header),
            Line::from(Span::styled(placeholder, theme::dim())),
        ];
    }

    if is_agent {
        if let Some(report) = parse_subagent_report(body) {
            return subagent_report_lines(&report, body_width);
        }
        let mut lines = vec![section_header(header)];
        let markdown = transcript::render_markdown(&transcript::tool_result_body(body), body_width);
        if markdown.is_empty() {
            lines.push(Line::from(Span::styled("(empty result)", theme::dim())));
        } else {
            lines.extend(markdown);
        }
        return lines;
    }

    let mut lines = vec![section_header(header)];
    lines.extend(tool_result_lines(body));
    lines
}

/// The structured completion report an agent tool result carries — see
/// SubagentSnapshot::detail for the writer. Parsed back into sections here so
/// the modal can render run metadata, prompt, activity trace, and the final
/// response as separate sections instead of one markdown blob (markdown
/// rendering soft-wraps consecutive `Key: value` lines into a single
/// unreadable paragraph).
struct SubagentReport {
    meta: Vec<(&'static str, String)>,
    prompt: Option<String>,
    activity: Vec<String>,
    response: Option<String>,
}

fn parse_subagent_report(body: &str) -> Option<SubagentReport> {
    // Label order mirrors SubagentSnapshot::detail. "Model" must be attempted
    // even though "Mode" is its prefix — matching requires the ": " separator,
    // so "Model: x" falls through "Mode" and lands on "Model".
    const META_KEYS: [(&str, &str); 7] = [
        ("ID", "id"),
        ("Agent", "agent"),
        ("Mode", "mode"),
        ("Model", "model"),
        ("Launch group", "group"),
        ("Status", "status"),
        ("Elapsed", "elapsed"),
    ];

    let lines = body.lines().collect::<Vec<_>>();
    let marker_after = |marker: &str, from: usize| {
        lines[from..]
            .iter()
            .position(|line| line.trim() == marker)
            .map(|offset| from + offset)
    };
    let prompt_start = marker_after("Prompt:", 0);
    let activity_start = marker_after("Activity:", prompt_start.map_or(0, |index| index + 1));
    let response_start = marker_after(
        "Result:",
        activity_start.or(prompt_start).map_or(0, |index| index + 1),
    );

    let meta_end = prompt_start
        .or(activity_start)
        .or(response_start)
        .unwrap_or(lines.len());
    let mut meta = Vec::new();
    for line in &lines[..meta_end] {
        for (key, label) in META_KEYS {
            if let Some(value) = line
                .strip_prefix(key)
                .and_then(|rest| rest.strip_prefix(": "))
            {
                meta.push((label, value.trim().to_string()));
                break;
            }
        }
    }

    if meta.is_empty() && prompt_start.is_none() && activity_start.is_none() {
        return None;
    }

    let section_text = |start: Option<usize>, end: usize| {
        start.map(|index| lines[index + 1..end].join("\n").trim().to_string())
    };
    let prompt = section_text(
        prompt_start,
        activity_start.or(response_start).unwrap_or(lines.len()),
    );
    let activity = activity_start
        .map(|index| {
            lines[index + 1..response_start.unwrap_or(lines.len())]
                .iter()
                .map(|line| line.trim_end())
                .filter(|line| !line.is_empty() && *line != "(no activity yet)")
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let response = section_text(response_start, lines.len()).filter(|text| !text.is_empty());

    Some(SubagentReport {
        meta,
        prompt: prompt.filter(|text| !text.is_empty()),
        activity,
        response,
    })
}

fn subagent_report_lines(report: &SubagentReport, body_width: usize) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let mut lines = Vec::new();

    if !report.meta.is_empty() {
        lines.push(section_header("subagent run"));
        for (label, value) in &report.meta {
            let color = match *label {
                "status" => subagent_status_color(value),
                "id" | "group" => palette.muted,
                _ => palette.text,
            };
            lines.push(kv(label, value, color));
        }
        lines.push(Line::from(""));
    }

    if let Some(prompt) = &report.prompt {
        lines.push(section_header("prompt"));
        lines.extend(transcript::render_markdown(prompt, body_width));
        lines.push(Line::from(""));
    }

    if !report.activity.is_empty() {
        lines.push(section_header("activity"));
        for entry in &report.activity {
            lines.push(Line::from(Span::styled(
                entry.clone(),
                theme::body(palette.text),
            )));
        }
        lines.push(Line::from(""));
    }

    lines.push(section_header("subagent response"));
    match &report.response {
        Some(response) => lines.extend(transcript::render_markdown(response, body_width)),
        None => lines.push(Line::from(Span::styled(
            "subagent still running",
            theme::dim(),
        ))),
    }
    lines
}

fn subagent_status_color(status: &str) -> ratatui::style::Color {
    let palette = theme::palette();
    match status {
        "succeeded" => palette.success,
        "failed" | "timed out" => palette.error,
        "cancelled" => palette.muted,
        "running" => palette.todo,
        _ => palette.text,
    }
}

pub(super) fn read_result_line(line: &str) -> Option<(&str, &str)> {
    let (number, text) = line.split_once(':')?;
    if number.is_empty() || !number.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some((number, text.strip_prefix(' ').unwrap_or(text)))
}

pub(super) fn kv(label: &str, value: &str, color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(padded_label(label), theme::muted()),
        Span::styled(value.to_string(), theme::body(color)),
    ])
}

pub(super) fn tool_status_color(status: ToolStatus) -> ratatui::style::Color {
    match status {
        ToolStatus::Running => theme::palette().todo,
        ToolStatus::Succeeded => theme::palette().success,
        ToolStatus::Failed | ToolStatus::Interrupted => theme::palette().error,
    }
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
