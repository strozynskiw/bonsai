//! `SessionEvent` → ACP v1 `session/update` projection.
//!
//! [`carrier`] is deliberately exhaustive over `SessionEvent`, mirroring
//! `server::contract::event_kind`: a new variant does not compile until it is
//! assigned an ACP carrier here, and the drift tests then require the same
//! assignment in the `docs/acp.md` event table and the projected samples in
//! `docs/acp-contract.json`.

use std::path::Path;

use agent_client_protocol::v1::schema::{
    ContentBlock, ContentChunk, Diff, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    SessionUpdate, TextContent, ToolCall, ToolCallContent, ToolCallId, ToolCallLocation,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};

use crate::diff::{DiffLineKind, DiffStatus, FileDiff};
use crate::output::ToolCallStart;
use crate::protocol::{
    ToolActivity, ToolExecutionStatus, ToolResult, ToolResultContent, ToolStatus, TranscriptItem,
};
use crate::server::sink::SessionEvent;

/// Largest file the projection reads back to give the client full diff text.
const MAX_DIFF_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Prefix of the authorization audit line tools stream before their output.
const AUTHORIZATION_PREFIX: &str = "[authorization] ";
/// Streamed tool output kept per call; ACP replaces content on every update.
const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;

/// How one `SessionEvent` crosses (or does not cross) the ACP boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Carrier {
    /// A `session/update` notification with this `sessionUpdate` discriminator.
    Update(&'static str),
    /// An agent → client `session/request_permission` request.
    RequestPermission,
    /// Settles the in-flight `session/prompt` response.
    PromptResponse,
    /// Triggers reconciliation from a fresh snapshot; nothing is forwarded as-is.
    Reconcile,
    /// ACP v1 has no place for it; dropped deliberately (see `docs/acp.md`).
    NotCarried,
}

impl Carrier {
    /// The label used in the `docs/acp.md` event table.
    #[cfg(test)]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Update(kind) => kind,
            Self::RequestPermission => "session/request_permission",
            Self::PromptResponse => "session/prompt response",
            Self::Reconcile => "reconcile",
            Self::NotCarried => "not carried",
        }
    }
}

/// Exhaustive ACP carrier for every `SessionEvent` variant.
pub(crate) fn carrier(event: &SessionEvent) -> Carrier {
    match event {
        SessionEvent::ResyncRequired => Carrier::Reconcile,
        // The client renders its own prompt; history replay uses
        // `user_message_chunk` from the durable transcript instead.
        SessionEvent::UserMessage(_) => Carrier::NotCarried,
        SessionEvent::MessageDelivery { .. } => Carrier::NotCarried,
        SessionEvent::AssistantDelta(_) => Carrier::Update("agent_message_chunk"),
        SessionEvent::AssistantDone => Carrier::NotCarried,
        SessionEvent::ReasoningDelta(_) => Carrier::Update("agent_thought_chunk"),
        SessionEvent::AttemptStarted => Carrier::NotCarried,
        SessionEvent::AttemptDiscarded => Carrier::NotCarried,
        SessionEvent::ToolStarted(_) => Carrier::Update("tool_call"),
        SessionEvent::ToolOutput { .. } => Carrier::Update("tool_call_update"),
        SessionEvent::ToolResult { .. } => Carrier::Update("tool_call_update"),
        SessionEvent::Usage { .. } => Carrier::NotCarried,
        SessionEvent::Plan(_) => Carrier::NotCarried,
        SessionEvent::Todos(_) => Carrier::Update("plan"),
        SessionEvent::Goal(_) => Carrier::NotCarried,
        // Only an unsuccessful run produces a line; success is the reply itself.
        SessionEvent::Completion(_) => Carrier::Update("agent_message_chunk"),
        SessionEvent::Status(_) => Carrier::NotCarried,
        // A local child of the user's own editor: show the secret-redacted
        // reason in the thread instead of ending the turn silently.
        SessionEvent::Error(_) => Carrier::Update("agent_message_chunk"),
        SessionEvent::Interaction(_) => Carrier::RequestPermission,
        SessionEvent::InteractionResolved(_) => Carrier::NotCarried,
        SessionEvent::Lifecycle { .. } => Carrier::PromptResponse,
    }
}

/// Per-prompt projection state: in-flight tool calls and their streamed output.
#[derive(Debug)]
pub(crate) struct Projector {
    root: std::path::PathBuf,
    in_flight: Vec<(String, String)>,
    output: std::collections::HashMap<String, String>,
}

impl Projector {
    pub(crate) fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            in_flight: Vec::new(),
            output: std::collections::HashMap::new(),
        }
    }

    /// The most recently started, not yet finished tool call — the one a
    /// permission prompt belongs to.
    pub(crate) fn current_tool(&self) -> Option<&(String, String)> {
        self.in_flight.last()
    }

    /// Latest in-flight call whose tool name satisfies `matches`.
    pub(crate) fn find(&self, matches: impl Fn(&str) -> bool) -> Option<String> {
        self.in_flight
            .iter()
            .rev()
            .find(|(_, name)| matches(name))
            .map(|(id, _)| id.clone())
    }

    pub(crate) fn in_flight_ids(&self) -> Vec<String> {
        self.in_flight.iter().map(|(id, _)| id.clone()).collect()
    }

    /// Project one forwarded event. Events whose [`carrier`] is not an
    /// `Update` yield `None`; the connection layer handles those itself.
    pub(crate) fn update(&mut self, event: &SessionEvent) -> Option<SessionUpdate> {
        match event {
            SessionEvent::AssistantDelta(text) => Some(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(text_block(text)),
            )),
            SessionEvent::ReasoningDelta(text) => Some(SessionUpdate::AgentThoughtChunk(
                ContentChunk::new(text_block(text)),
            )),
            SessionEvent::ToolStarted(call) => {
                self.in_flight.push((call.id.clone(), call.name.clone()));
                Some(SessionUpdate::ToolCall(tool_call(&self.root, call)))
            }
            SessionEvent::ToolOutput { id, text } => {
                // `[authorization] …` lines are bonsai's audit trail (also in
                // the session log), not output the user asked for.
                let visible: String = text
                    .split_inclusive('\n')
                    .filter(|line| !line.starts_with(AUTHORIZATION_PREFIX))
                    .collect();
                if visible.is_empty() {
                    return None;
                }
                let buffer = self.output.entry(id.clone()).or_default();
                buffer.push_str(&visible);
                if buffer.len() > MAX_TOOL_OUTPUT_BYTES {
                    let cut = buffer.len() - MAX_TOOL_OUTPUT_BYTES;
                    let cut = (cut..buffer.len())
                        .find(|i| buffer.is_char_boundary(*i))
                        .unwrap_or(buffer.len());
                    buffer.drain(..cut);
                }
                Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    ToolCallId::new(id.as_str()),
                    ToolCallUpdateFields::new()
                        .status(ToolCallStatus::InProgress)
                        .content(vec![text_content(buffer)]),
                )))
            }
            SessionEvent::ToolResult { id, result } => {
                if !matches!(result.status, ToolExecutionStatus::Started) {
                    self.finish(id);
                }
                Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    ToolCallId::new(id.as_str()),
                    result_fields(&self.root, result),
                )))
            }
            SessionEvent::Todos(todos) => Some(SessionUpdate::Plan(plan(todos))),
            SessionEvent::Completion(report) => {
                use crate::completion_report::CompletionStatus;
                let notice = match report.status {
                    CompletionStatus::Failed => {
                        "**Run failed.** The provider or a tool returned an error; the bonsai \
                         session log has the details (`bonsai doctor` shows where it is)."
                    }
                    CompletionStatus::BudgetExhausted => {
                        "**Run stopped:** the configured run budget is exhausted."
                    }
                    CompletionStatus::Blocked => {
                        "**Run blocked:** bonsai is waiting on something ACP cannot carry; \
                         continue in the TUI with `bonsai -c`."
                    }
                    CompletionStatus::Completed
                    | CompletionStatus::Unverified
                    | CompletionStatus::Interrupted => return None,
                };
                Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    text_block(&format!("\n\n{notice}\n")),
                )))
            }
            SessionEvent::Error(message) => Some(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(text_block(&format!(
                    "\n\n**Error:** {}\n",
                    crate::redact::redact(message)
                ))),
            )),
            _ => None,
        }
    }

    /// Settle a call that finished without a forwarded result (lagged stream).
    pub(crate) fn reconcile(&mut self, activity: &ToolActivity) -> Option<SessionUpdate> {
        if matches!(activity.status, ToolStatus::Running)
            || !self.in_flight.iter().any(|(id, _)| *id == activity.id)
        {
            return None;
        }
        self.finish(&activity.id);
        Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new(activity.id.as_str()),
            activity_fields(&self.root, activity),
        )))
    }

    fn finish(&mut self, id: &str) {
        self.in_flight.retain(|(open, _)| open != id);
        self.output.remove(id);
    }
}

/// Durable transcript → updates for `session/load` history replay.
pub(crate) fn replay(root: &Path, items: &[TranscriptItem]) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    for item in items {
        match item {
            TranscriptItem::UserMessage { text } => updates.push(SessionUpdate::UserMessageChunk(
                ContentChunk::new(text_block(text)),
            )),
            TranscriptItem::AssistantMessage { text } => updates.push(
                SessionUpdate::AgentMessageChunk(ContentChunk::new(text_block(text))),
            ),
            TranscriptItem::ReasoningSummary { text } => updates.push(
                SessionUpdate::AgentThoughtChunk(ContentChunk::new(text_block(text))),
            ),
            TranscriptItem::ToolActivity(activity) => {
                updates.push(SessionUpdate::ToolCall(replayed_tool(root, activity)));
            }
            TranscriptItem::ExecutionGroup(group) => updates.extend(
                group
                    .tools
                    .iter()
                    .map(|activity| SessionUpdate::ToolCall(replayed_tool(root, activity))),
            ),
            _ => {}
        }
    }
    updates
}

fn replayed_tool(root: &Path, activity: &ToolActivity) -> ToolCall {
    let start = ToolCallStart {
        id: activity.id.clone(),
        name: activity.name.clone(),
        arguments: activity.arguments.clone(),
    };
    let mut call = tool_call(root, &start);
    call.update(activity_fields(root, activity));
    call
}

fn activity_fields(root: &Path, activity: &ToolActivity) -> ToolCallUpdateFields {
    if let Some(result) = &activity.tool_result {
        return result_fields(root, result);
    }
    let status = match activity.status {
        ToolStatus::Running => ToolCallStatus::InProgress,
        ToolStatus::Succeeded => ToolCallStatus::Completed,
        ToolStatus::Failed | ToolStatus::Interrupted => ToolCallStatus::Failed,
    };
    let mut content = Vec::new();
    if let Some(diff) = &activity.diff {
        content.extend(diffs(root, diff, false));
    } else if let Some(text) = activity.result.as_deref().filter(|t| !t.is_empty()) {
        content.push(text_content(text));
    }
    ToolCallUpdateFields::new().status(status).content(content)
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text))
}

fn text_content(text: &str) -> ToolCallContent {
    ToolCallContent::from(text_block(text))
}

fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" | "read_region" | "read_symbol" | "recall" | "project_info" | "definition"
        | "hover" | "diagnostics" => ToolKind::Read,
        "edit" | "write" | "apply_patch" | "rename_symbol" | "memory_write" => ToolKind::Edit,
        "bash" | "terminal" | "tasks" | "git" => ToolKind::Execute,
        "grep" | "glob" | "symbol_search" | "workspace_symbol" | "references" => ToolKind::Search,
        "webfetch" | "websearch" => ToolKind::Fetch,
        "todowrite" => ToolKind::Think,
        _ => ToolKind::Other,
    }
}

fn tool_call(root: &Path, call: &ToolCallStart) -> ToolCall {
    let input = serde_json::from_str::<serde_json::Value>(&call.arguments).ok();
    let field = |key: &str| {
        input
            .as_ref()
            .and_then(|v| v.get(key))
            .and_then(serde_json::Value::as_str)
    };
    let path = field("path").or_else(|| field("file_path"));
    let detail = field("command")
        .or(path)
        .or_else(|| field("pattern"))
        .or_else(|| field("url"))
        .or_else(|| field("query"));
    let title = match (call.name.as_str(), detail) {
        ("todowrite", _) => "Update todo list".to_string(),
        (_, Some(detail)) => format!("{}: {}", call.name, first_line(detail, 120)),
        (_, None) => call.name.clone(),
    };
    let locations = path
        .map(|p| vec![ToolCallLocation::new(root.join(p))])
        .unwrap_or_default();
    ToolCall::new(ToolCallId::new(call.id.as_str()), title)
        .kind(tool_kind(&call.name))
        .status(ToolCallStatus::InProgress)
        .locations(locations)
        .raw_input(input)
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

fn result_fields(root: &Path, result: &ToolResult) -> ToolCallUpdateFields {
    let status = match result.status {
        ToolExecutionStatus::Succeeded => ToolCallStatus::Completed,
        ToolExecutionStatus::Started => ToolCallStatus::InProgress,
        ToolExecutionStatus::Failed
        | ToolExecutionStatus::Skipped
        | ToolExecutionStatus::Interrupted => ToolCallStatus::Failed,
    };
    let content = match &result.content {
        ToolResultContent::Edit { diff, .. } => diffs(root, diff, true),
        ToolResultContent::Command {
            stdout,
            stderr,
            exit_code,
            timed_out,
            ..
        } => {
            let mut text = String::new();
            text.push_str(stdout);
            if !stderr.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(stderr);
            }
            if *timed_out {
                text.push_str("\n[timed out]");
            } else if let Some(code) = exit_code.filter(|c| *c != 0) {
                text.push_str(&format!("\n[exit {code}]"));
            }
            vec![text_content(&fence(&text))]
        }
        ToolResultContent::Content { parts } => parts
            .iter()
            .filter_map(|part| match part {
                crate::protocol::ContentPart::Text { text } => Some(text_content(text)),
                crate::protocol::ContentPart::Image { .. } => None,
            })
            .collect(),
        _ if result.preview.is_empty() => Vec::new(),
        _ => vec![text_content(&result.preview)],
    };
    ToolCallUpdateFields::new().status(status).content(content)
}

fn fence(text: &str) -> String {
    if text.trim().is_empty() {
        return String::new();
    }
    let ticks = "`".repeat(3.max(longest_backtick_run(text) + 1));
    format!("{ticks}\n{}\n{ticks}", text.trim_end_matches('\n'))
}

fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

fn plan(todos: &[crate::todo::TodoItem]) -> Plan {
    use crate::todo::TodoStatus;
    Plan::new(
        todos
            .iter()
            .filter(|todo| todo.status != TodoStatus::Cancelled)
            .map(|todo| {
                let status = match todo.status {
                    TodoStatus::Pending | TodoStatus::Cancelled => PlanEntryStatus::Pending,
                    TodoStatus::InProgress => PlanEntryStatus::InProgress,
                    TodoStatus::Completed => PlanEntryStatus::Completed,
                };
                PlanEntry::new(todo.content.clone(), PlanEntryPriority::Medium, status)
            })
            .collect(),
    )
}

/// ACP diffs carry whole old/new texts. Live results read the file back and
/// reverse-apply the hunks; anything that cannot be reconstructed exactly
/// falls back to hunk-local fragments, which clients still render as a diff.
fn diffs(root: &Path, diff: &FileDiff, live: bool) -> Vec<ToolCallContent> {
    std::iter::once(diff)
        .chain(diff.additional_files.iter())
        .map(|file| ToolCallContent::Diff(file_diff(root, file, live)))
        .collect()
}

fn file_diff(root: &Path, diff: &FileDiff, live: bool) -> Diff {
    let path = root.join(&diff.path);
    let current = (live && !diff.truncated && diff.status != DiffStatus::Deleted)
        .then(|| read_bounded(&path))
        .flatten();
    if let Some(new_text) = current {
        match diff.status {
            DiffStatus::Created => return Diff::new(path, new_text),
            DiffStatus::Modified => {
                if let Some(old_text) = reverse_apply(&new_text, diff) {
                    return Diff::new(path, new_text).old_text(old_text);
                }
            }
            DiffStatus::Deleted => {}
        }
    }
    let (old, new) = fragments(diff);
    match diff.status {
        DiffStatus::Created => Diff::new(path, new),
        DiffStatus::Modified | DiffStatus::Deleted => Diff::new(path, new).old_text(old),
    }
}

fn read_bounded(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    (metadata.is_file() && metadata.len() <= MAX_DIFF_FILE_BYTES)
        .then(|| std::fs::read_to_string(path).ok())
        .flatten()
}

fn fragments(diff: &FileDiff) -> (String, String) {
    let (mut old, mut new) = (Vec::new(), Vec::new());
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            if line.kind != DiffLineKind::Added {
                old.push(line.content.as_str());
            }
            if line.kind != DiffLineKind::Removed {
                new.push(line.content.as_str());
            }
        }
    }
    (old.join("\n"), new.join("\n"))
}

/// Rebuild the pre-edit text from the post-edit text and the hunks, verifying
/// every context/added line against the file. `None` on any mismatch.
fn reverse_apply(new_text: &str, diff: &FileDiff) -> Option<String> {
    let lines: Vec<&str> = new_text.split('\n').collect();
    let mut old: Vec<&str> = Vec::with_capacity(lines.len());
    let mut cursor = 0usize;
    for hunk in &diff.hunks {
        let start = (hunk.new_start as usize).saturating_sub(1);
        let added_or_context = hunk
            .lines
            .iter()
            .filter(|l| l.kind != DiffLineKind::Removed)
            .count();
        // A pure deletion hunk reports the line *before* the gap.
        let start = if added_or_context == 0 && hunk.new_start > 0 {
            start + 1
        } else {
            start
        };
        if start < cursor || start > lines.len() {
            return None;
        }
        old.extend_from_slice(&lines[cursor..start]);
        let mut at = start;
        for line in &hunk.lines {
            match line.kind {
                DiffLineKind::Removed => old.push(line.content.as_str()),
                DiffLineKind::Context | DiffLineKind::Added => {
                    if lines.get(at).map(|l| l.trim_end_matches('\r'))
                        != Some(line.content.trim_end_matches(['\r', '\n']))
                    {
                        return None;
                    }
                    if line.kind == DiffLineKind::Context {
                        old.push(lines[at]);
                    }
                    at += 1;
                }
            }
        }
        cursor = at;
    }
    old.extend_from_slice(lines.get(cursor..)?);
    Some(old.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{DiffHunk, DiffLine};
    use serde_json::{Value, json};

    fn line(kind: DiffLineKind, content: &str) -> DiffLine {
        DiffLine {
            kind,
            content: content.into(),
            old_line: None,
            new_line: None,
        }
    }

    fn modified(hunks: Vec<DiffHunk>) -> FileDiff {
        FileDiff {
            path: "a.txt".into(),
            status: DiffStatus::Modified,
            hunks,
            truncated: false,
            old_size: None,
            new_size: 0,
            added_lines: 0,
            removed_lines: 0,
            additional_files: Box::default(),
        }
    }

    #[test]
    fn reverse_apply_restores_the_original_text() {
        let diff = modified(vec![
            DiffHunk {
                old_start: 1,
                new_start: 1,
                lines: vec![
                    line(DiffLineKind::Context, "one"),
                    line(DiffLineKind::Removed, "two"),
                    line(DiffLineKind::Added, "TWO"),
                    line(DiffLineKind::Added, "two and a half"),
                ],
            },
            DiffHunk {
                old_start: 5,
                new_start: 6,
                lines: vec![
                    line(DiffLineKind::Context, "five"),
                    line(DiffLineKind::Removed, "six"),
                ],
            },
        ]);
        let new = "one\nTWO\ntwo and a half\nthree\nfour\nfive\nseven\n";
        assert_eq!(
            reverse_apply(new, &diff).as_deref(),
            Some("one\ntwo\nthree\nfour\nfive\nsix\nseven\n")
        );
    }

    #[test]
    fn reverse_apply_refuses_hunks_that_do_not_match_the_file() {
        let diff = modified(vec![DiffHunk {
            old_start: 1,
            new_start: 1,
            lines: vec![line(DiffLineKind::Added, "not in file")],
        }]);
        assert_eq!(reverse_apply("something else\n", &diff), None);
        let (old, new) = fragments(&diff);
        assert_eq!((old.as_str(), new.as_str()), ("", "not in file"));
    }

    #[test]
    fn authorization_audit_lines_never_reach_tool_cards() {
        let mut projector = Projector::new(Path::new("/w"));
        let audit = SessionEvent::ToolOutput {
            id: "call-1".into(),
            text: "[authorization] allow · low · code-execution · autonomy=ask\n".into(),
        };
        assert!(projector.update(&audit).is_none());
        let mixed = SessionEvent::ToolOutput {
            id: "call-1".into(),
            text: "[authorization] allow · low\nreal output\n".into(),
        };
        let update = serde_json::to_value(projector.update(&mixed).unwrap()).unwrap();
        assert_eq!(update["content"][0]["content"]["text"], "real output\n");
    }

    #[test]
    fn interrupted_stream_reconciles_only_finished_in_flight_tools() {
        let root = Path::new("/w");
        let mut projector = Projector::new(root);
        projector.update(&SessionEvent::ToolStarted(ToolCallStart {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"ls"}"#.into(),
        }));
        assert_eq!(
            projector.current_tool(),
            Some(&("call-1".into(), "bash".into()))
        );
        let mut activity = ToolActivity {
            id: "call-1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
            delegated_model: None,
            status: ToolStatus::Running,
            result: Some("done".into()),
            diff: None,
            tool_result: None,
            timing: Default::default(),
        };
        assert!(projector.reconcile(&activity).is_none());
        activity.status = ToolStatus::Succeeded;
        assert!(projector.reconcile(&activity).is_some());
        assert!(projector.current_tool().is_none());
        assert!(projector.reconcile(&activity).is_none());
    }

    /// Published ACP projections stay byte-for-byte what bonsai emits.
    #[test]
    fn published_acp_fixtures_match_real_projections() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../docs/acp-contract.json")).unwrap();
        assert_eq!(fixture["protocol_version"], 1);
        let root = Path::new("/workspace");
        let mut projector = Projector::new(root);
        let samples = [
            SessionEvent::AssistantDelta("Hi".into()),
            SessionEvent::ReasoningDelta("thinking".into()),
            SessionEvent::ToolStarted(ToolCallStart {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: r#"{"command":"cargo test"}"#.into(),
            }),
            SessionEvent::ToolOutput {
                id: "call-1".into(),
                text: "running 1 test".into(),
            },
            SessionEvent::ToolResult {
                id: "call-2".into(),
                result: crate::server::contract::tests::edit_result(),
            },
            SessionEvent::Todos(vec![crate::todo::TodoItem {
                content: "Ship it".into(),
                status: crate::todo::TodoStatus::InProgress,
            }]),
        ];
        let projected = samples
            .iter()
            .map(|event| {
                let update = projector.update(event).expect("sample is carried");
                assert_eq!(
                    serde_json::to_value(&update).unwrap()["sessionUpdate"],
                    json!(carrier(event).label()),
                    "projection and declared carrier must agree"
                );
                serde_json::to_value(update).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(json!(projected), fixture["updates"]);
    }

    /// Every `SessionEvent` kind the HTTP contract publishes has exactly one
    /// documented ACP carrier, and the docs table matches [`carrier`].
    #[test]
    fn every_session_event_kind_has_a_documented_acp_carrier() {
        let published: std::collections::BTreeSet<String> =
            serde_json::from_str::<Value>(include_str!("../../docs/server-contract.json")).unwrap()
                ["event_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| k.as_str().unwrap().to_string())
                .collect();
        let documented = documented_carriers();
        assert_eq!(
            documented
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            published,
            "docs/acp.md must map every published SessionEvent kind"
        );
        for event in crate::server::contract::tests::representative_events() {
            let kind = crate::server::contract::event_kind(&event);
            assert_eq!(
                documented.get(kind).map(String::as_str),
                Some(carrier(&event).label()),
                "docs/acp.md carrier for `{kind}` drifted from the code"
            );
        }
    }

    fn documented_carriers() -> std::collections::BTreeMap<String, String> {
        let docs = include_str!("../../docs/acp.md");
        let section = docs
            .split("## Event mapping")
            .nth(1)
            .expect("docs/acp.md has an event mapping section");
        section
            .split("\n## ")
            .next()
            .unwrap_or_default()
            .lines()
            .filter_map(|row| {
                let mut cells = row.strip_prefix('|')?.split('|').map(str::trim);
                let kind = cells.next()?.strip_prefix('`')?.strip_suffix('`')?;
                let carrier = cells.next()?.trim_matches('`');
                (kind != "kind").then(|| (kind.to_string(), carrier.to_string()))
            })
            .collect()
    }
}
