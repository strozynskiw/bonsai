//! P2.3 read-time read deduplication. A re-read of an unchanged window whose
//! earlier full copy is still live is replaced by a compact pointer, leaving the
//! earlier copy untouched so the prompt-cache prefix is preserved; anything that
//! could blind the model (an evicted prior copy, changed content) falls back to
//! full content.

use super::*;
use std::path::PathBuf;

use crate::context_view::{ContextControlState, ContextNodeId, tool_message_call_id};
use crate::tool::read_evidence::{DelegatedReadEvidence, InspectionOutcome, InspectionReason};
use crate::tool::{ReadRegionTool, ReadTool};

/// A two-line file reads back as this exact line-numbered block (the read tool
/// formats each line as `"<n>: <text>"`, 1-indexed, no truncation footer when
/// the window spans the whole file).
const FOO_RENDERED: &str = "1: fn a() {}\n2: fn b() {}\n";

struct BashReadTool {
    output: String,
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
    read_tracker: crate::tool::ReadTracker,
    path: PathBuf,
}

#[async_trait::async_trait]
impl Tool for BashReadTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "mock clean bash file read"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"command": {"type": "string"}},
            "required": ["command"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.calls.lock().await.push(args);
        self.read_tracker
            .mark_read_with_content(&self.path, self.output.as_bytes(), true)
            .await;
        Ok(ToolOutput::Command {
            rendered: self.output.clone(),
            stdout: self.output.clone(),
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
            truncation: None,
        })
    }
}

fn large_read_rendered() -> String {
    (1..=450)
        .map(|line| format!("{line}: {}\n", "x".repeat(20)))
        .collect()
}

fn read_call_response(id: &str, path: &str) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        content: String::new(),
        tool_calls: vec![test_tool_call(
            id,
            "read",
            &format!(r#"{{"path":"{path}"}}"#),
        )],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })
}

fn read_window_response(
    id: &str,
    path: &str,
    offset: usize,
    limit: usize,
) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        tool_calls: vec![test_tool_call(
            id,
            "read",
            &serde_json::json!({
                "path": path,
                "offset": offset,
                "limit": limit,
            })
            .to_string(),
        )],
        ..StreamedResponse::default()
    })
}

fn read_region_response(
    id: &str,
    path: &str,
    start_line: usize,
    end_line: usize,
) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        tool_calls: vec![test_tool_call(
            id,
            "read_region",
            &serde_json::json!({
                "path": path,
                "start_line": start_line,
                "end_line": end_line,
            })
            .to_string(),
        )],
        ..StreamedResponse::default()
    })
}

fn read_limit_response(
    id: &str,
    path: &str,
    limit: usize,
) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        tool_calls: vec![test_tool_call(
            id,
            "read",
            &serde_json::json!({
                "path": path,
                "limit": limit,
            })
            .to_string(),
        )],
        ..StreamedResponse::default()
    })
}

fn bash_read_response(id: &str, path: &str) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        tool_calls: vec![test_tool_call(
            id,
            "bash",
            &serde_json::json!({"command": format!("cat {path}")}).to_string(),
        )],
        ..StreamedResponse::default()
    })
}

fn read_batch_response(
    calls: &[(&str, &str)],
) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        content: String::new(),
        tool_calls: calls
            .iter()
            .map(|(id, path)| test_tool_call(id, "read", &format!(r#"{{"path":"{path}"}}"#)))
            .collect(),
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })
}

fn finish_response(content: &str) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        content: content.to_string(),
        tool_calls: vec![],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })
}

fn read_registry(fixture: &TestFixture) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReadTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    registry.register(Arc::new(ReadRegionTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    Arc::new(registry)
}

fn read_edit_registry(fixture: &TestFixture) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReadTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    registry.register(Arc::new(crate::tool::EditTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    Arc::new(registry)
}

fn edit_call_response(
    id: &str,
    path: &str,
    old: &str,
    new: &str,
) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        content: String::new(),
        tool_calls: vec![test_tool_call(
            id,
            "edit",
            &serde_json::json!({ "path": path, "old_string": old, "new_string": new }).to_string(),
        )],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })
}

/// Records `tool_finished` so a test can see the model-facing result the TUI
/// card ends up with — the run loop re-emits the dedup pointer here to relabel
/// the already-finished card.
#[derive(Default)]
struct RecordingSink {
    finished: StdMutex<Vec<(String, String, bool)>>,
}

impl OutputSink for RecordingSink {
    fn tool_finished(&self, id: &str, result: &str, status: crate::output::ToolExecutionStatus) {
        self.finished
            .lock()
            .expect("recording sink mutex should not be poisoned")
            .push((id.to_string(), result.to_string(), status.is_success()));
    }
}

impl RecordingSink {
    fn last_result_for(&self, id: &str) -> Option<String> {
        self.finished
            .lock()
            .expect("recording sink mutex should not be poisoned")
            .iter()
            .rev()
            .find(|(call_id, _, _)| call_id == id)
            .map(|(_, result, _)| result.clone())
    }

    fn last_success_for(&self, id: &str) -> Option<bool> {
        self.finished
            .lock()
            .expect("recording sink mutex should not be poisoned")
            .iter()
            .rev()
            .find(|(call_id, _, _)| call_id == id)
            .map(|(_, _, success)| *success)
    }
}

/// Tool results (call_id, content) in conversation order — the model-facing view
/// the cache sees.
fn tool_messages(messages: &[ChatCompletionRequestMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .filter_map(|message| {
            let value = serde_json::to_value(message).ok()?;
            if value.get("role").and_then(|role| role.as_str()) != Some("tool") {
                return None;
            }
            let id = value.get("tool_call_id")?.as_str()?.to_string();
            let content = value.get("content")?.as_str()?.to_string();
            Some((id, content))
        })
        .collect()
}

fn read_detail(call_id: &str, arguments: &str, rendered: &str) -> ToolContextDetail {
    ToolContextDetail {
        call_id: call_id.to_string(),
        name: "read".to_string(),
        arguments: arguments.to_string(),
        read_evidence: None,
        result: ToolContextResult::Text {
            rendered: rendered.to_string(),
        },
        reuse_target_call_id: None,
    }
}

fn read_detail_with_evidence(
    call_id: &str,
    arguments: &str,
    rendered: &str,
    evidence: ReadEvidence,
) -> ToolContextDetail {
    ToolContextDetail {
        read_evidence: Some(evidence),
        ..read_detail(call_id, arguments, rendered)
    }
}

fn read_evidence(
    path: &str,
    start_line: usize,
    end_line: usize,
    total_lines: usize,
    rendered: &str,
) -> ReadEvidence {
    ReadEvidence::new(
        path,
        PathBuf::from(format!("/tmp/{path}")),
        ReadWindow {
            requested_offset: start_line,
            requested_limit: end_line.saturating_sub(start_line).saturating_add(1),
            start_line,
            end_line: Some(end_line),
            total_lines: Some(total_lines),
        },
        if start_line == 1 && end_line == total_lines {
            ReadCoverage::Full
        } else {
            ReadCoverage::Partial
        },
        rendered,
        None,
        0,
        None,
    )
}

fn reused_pointer(agent: &Agent, call_id: &str, rendered: &str) -> Option<String> {
    match agent.read_admission(call_id, rendered) {
        ReadAdmission::Reuse(reuse) => Some(reuse.pointer),
        ReadAdmission::Execute(_) | ReadAdmission::Reject(_) => None,
    }
}

#[tokio::test]
async fn delegated_full_read_observes_broad_parent_overlap_without_blocking_execution() {
    let fixture = TestFixture::new();
    let path = fixture.project_root.join("delegated.rs");
    let body = b"fn delegated() {}\n";
    tokio::fs::write(&path, body).await.unwrap();
    let canonical = tokio::fs::canonicalize(&path).await.unwrap();
    let metadata = tokio::fs::metadata(&canonical).await.unwrap();
    let evidence = ReadEvidence::new(
        "delegated.rs",
        canonical.clone(),
        ReadWindow {
            requested_offset: 1,
            requested_limit: 2000,
            start_line: 1,
            end_line: Some(1),
            total_lines: Some(1),
        },
        ReadCoverage::Full,
        "1: fn delegated() {}\n",
        metadata.modified().ok(),
        metadata.len(),
        Some(crate::tool::digest_content(body)),
    );
    let mut agent = Agent::builder(
        Box::new(MockProvider::new(vec![
            read_call_response("call-broad", "delegated.rs"),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .project_context_snapshot(crate::context::isolated_project_context_snapshot(
        &fixture.project_root,
    ))
    .build()
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);
    agent.import_delegated_read_evidence(&[
        DelegatedReadEvidence {
            subtask_id: "sub-7".to_string(),
            launch_group_id: Some("group-4".to_string()),
            source_id: "tool:child-read".to_string(),
            cited_in_result: true,
            evidence: evidence.clone(),
        },
        DelegatedReadEvidence {
            subtask_id: "sub-7".to_string(),
            launch_group_id: Some("group-4".to_string()),
            source_id: "tool:child-read-duplicate".to_string(),
            cited_in_result: true,
            evidence,
        },
    ]);
    agent.refresh_stale_read_advisory();

    assert!(!fixture.read_tracker.is_read(&canonical).await);
    let outgoing = agent.outgoing_messages_for(&agent.messages);
    let system = serde_json::to_value(&outgoing[0]).unwrap();
    let system = system["content"].as_str().unwrap();
    assert!(system.contains("### Delegated read coverage"), "{system}");
    assert!(
        system.contains("observed by group-4/sub-7; cited in result"),
        "{system}"
    );

    let narrow = test_tool_call(
        "call-narrow",
        "read",
        r#"{"path":"delegated.rs","offset":1,"limit":40}"#,
    );
    agent.observe_delegated_read_overlap(&[narrow]);
    assert_eq!(
        agent
            .usage_turns()
            .iter()
            .map(|turn| turn.delegated_parent_overlap)
            .sum::<usize>(),
        0
    );

    let result = agent
        .run(
            "read delegated.rs",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();
    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert!(fixture.read_tracker.is_read(&canonical).await);
    assert_eq!(
        tool_messages(&agent.messages)
            .iter()
            .find(|(id, _)| id == "call-broad"),
        Some(&(
            "call-broad".to_string(),
            "1: fn delegated() {}\n".to_string()
        ))
    );
    assert_eq!(agent.usage_turns()[0].delegated_parent_overlap, 1);

    let broad = test_tool_call("call-broad", "read", r#"{"path":"delegated.rs"}"#);
    agent.observe_delegated_read_overlap(&[broad]);
    assert_eq!(agent.usage_turns()[0].delegated_parent_overlap, 1);
}

#[tokio::test]
async fn stale_partial_and_unrelated_delegated_evidence_do_not_observe_overlap() {
    let fixture = TestFixture::new();
    let target = fixture.project_root.join("target.rs");
    let unrelated = fixture.project_root.join("unrelated.rs");
    tokio::fs::write(&target, "fn old() {}\n").await.unwrap();
    tokio::fs::write(&unrelated, "fn unrelated() {}\n")
        .await
        .unwrap();
    let target_canonical = tokio::fs::canonicalize(&target).await.unwrap();
    let unrelated_canonical = tokio::fs::canonicalize(&unrelated).await.unwrap();
    let stale_metadata = tokio::fs::metadata(&target_canonical).await.unwrap();
    let stale = ReadEvidence::new(
        "target.rs",
        target_canonical.clone(),
        ReadWindow {
            requested_offset: 1,
            requested_limit: 2000,
            start_line: 1,
            end_line: Some(1),
            total_lines: Some(1),
        },
        ReadCoverage::Full,
        "1: fn old() {}\n",
        stale_metadata.modified().ok(),
        stale_metadata.len(),
        Some(crate::tool::digest_content(b"fn old() {}\n")),
    );
    tokio::fs::write(&target, "fn new() {}\n").await.unwrap();
    let target_metadata = tokio::fs::metadata(&target_canonical).await.unwrap();
    let partial = ReadEvidence::new(
        "target.rs",
        target_canonical.clone(),
        ReadWindow {
            requested_offset: 1,
            requested_limit: 1,
            start_line: 1,
            end_line: Some(1),
            total_lines: Some(1),
        },
        ReadCoverage::Partial,
        "1: fn new() {}\n",
        target_metadata.modified().ok(),
        target_metadata.len(),
        Some(crate::tool::digest_content(b"fn new() {}\n")),
    );
    let unrelated_metadata = tokio::fs::metadata(&unrelated_canonical).await.unwrap();
    let unrelated = ReadEvidence::new(
        "unrelated.rs",
        unrelated_canonical,
        ReadWindow {
            requested_offset: 1,
            requested_limit: 2000,
            start_line: 1,
            end_line: Some(1),
            total_lines: Some(1),
        },
        ReadCoverage::Full,
        "1: fn unrelated() {}\n",
        unrelated_metadata.modified().ok(),
        unrelated_metadata.len(),
        Some(crate::tool::digest_content(b"fn unrelated() {}\n")),
    );
    let mut agent = review_agent(&fixture, String::new());
    agent.import_delegated_read_evidence(&[
        DelegatedReadEvidence {
            subtask_id: "stale".to_string(),
            launch_group_id: None,
            source_id: "tool:stale".to_string(),
            cited_in_result: true,
            evidence: stale,
        },
        DelegatedReadEvidence {
            subtask_id: "partial".to_string(),
            launch_group_id: None,
            source_id: "tool:partial".to_string(),
            cited_in_result: true,
            evidence: partial,
        },
        DelegatedReadEvidence {
            subtask_id: "unrelated".to_string(),
            launch_group_id: None,
            source_id: "tool:unrelated".to_string(),
            cited_in_result: true,
            evidence: unrelated,
        },
    ]);
    agent.refresh_read_evidence_freshness().await;

    agent.observe_delegated_read_overlap(&[test_tool_call(
        "call-broad",
        "read",
        r#"{"path":"target.rs"}"#,
    )]);

    assert_eq!(
        agent
            .usage_turns()
            .iter()
            .map(|turn| turn.delegated_parent_overlap)
            .sum::<usize>(),
        0
    );
    assert!(!fixture.read_tracker.is_read(&target_canonical).await);
}

/// A successful `bash` tool detail for `command` whose stdout is `output`, the
/// shape a `cat`/`head`/`tail` read produces.
fn bash_detail(call_id: &str, command: &str, output: &str) -> ToolContextDetail {
    ToolContextDetail {
        call_id: call_id.to_string(),
        name: "bash".to_string(),
        arguments: format!(r#"{{"command":"{command}"}}"#),
        read_evidence: None,
        result: ToolContextResult::Command {
            rendered: output.to_string(),
            stdout: output.to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
            truncation: None,
        },
        reuse_target_call_id: None,
    }
}

#[tokio::test]
async fn dedup_uses_effective_displayed_range_not_requested_limit() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());

    agent.messages.push(test_user_message("hi"));
    agent
        .messages
        .push(tool_result_message("call-1", FOO_RENDERED));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        read_detail_with_evidence(
            "call-1",
            r#"{"path":"foo.rs","offset":1,"limit":2}"#,
            FOO_RENDERED,
            read_evidence("foo.rs", 1, 2, 2, FOO_RENDERED),
        ),
    );
    agent.tool_context_details.insert(
        "call-2".to_string(),
        read_detail_with_evidence(
            "call-2",
            r#"{"path":"foo.rs","offset":1,"limit":2000}"#,
            FOO_RENDERED,
            read_evidence("foo.rs", 1, 2, 2, FOO_RENDERED),
        ),
    );

    assert!(
        reused_pointer(&agent, "call-2", FOO_RENDERED).is_some(),
        "same displayed full-file range should dedup even when requested limits differ"
    );
}

#[tokio::test]
async fn large_rereads_collapse_to_protected_pointers() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());
    let large = large_read_rendered();

    agent.messages.push(test_user_message("hi"));
    agent.messages.push(tool_result_message("call-1", &large));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        read_detail_with_evidence(
            "call-1",
            r#"{"path":"foo.rs"}"#,
            &large,
            read_evidence("foo.rs", 1, 450, 450, &large),
        ),
    );
    agent.tool_context_details.insert(
        "call-2".to_string(),
        read_detail_with_evidence(
            "call-2",
            r#"{"path":"foo.rs"}"#,
            &large,
            read_evidence("foo.rs", 1, 450, 450, &large),
        ),
    );

    assert!(
        reused_pointer(&agent, "call-2", &large).is_some(),
        "large duplicate reads should collapse to pointers instead of refilling context"
    );
}

#[tokio::test]
async fn covered_read_window_dedups_to_covering_prior_read() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());
    let prior = (1..=120)
        .map(|line| format!("{line}: line {line}\n"))
        .collect::<String>();
    let narrower = (40..=60)
        .map(|line| format!("{line}: line {line}\n"))
        .collect::<String>();

    agent.messages.push(test_user_message("hi"));
    agent.messages.push(tool_result_message("call-1", &prior));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        read_detail_with_evidence(
            "call-1",
            r#"{"path":"foo.rs","offset":1,"limit":120}"#,
            &prior,
            read_evidence("foo.rs", 1, 120, 200, &prior),
        ),
    );
    agent.tool_context_details.insert(
        "call-2".to_string(),
        read_detail_with_evidence(
            "call-2",
            r#"{"path":"foo.rs","offset":40,"limit":21}"#,
            &narrower,
            read_evidence("foo.rs", 40, 60, 200, &narrower),
        ),
    );

    let note = reused_pointer(&agent, "call-2", &narrower)
        .expect("a narrower read covered by a prior live read should dedup");
    assert!(
        note.contains("lines 40-60"),
        "note should describe the requested narrower range: {note}",
    );
    assert!(
        note.contains("call-1"),
        "note should reference the covering prior read: {note}",
    );
}

#[tokio::test]
async fn typed_region_and_symbol_reads_share_the_reuse_path() {
    let fixture = TestFixture::new();

    for tool_name in ["read_region", "read_symbol"] {
        let mut agent = review_agent(&fixture, String::new());
        agent.messages.push(test_user_message("hi"));
        agent
            .messages
            .push(tool_result_message("call-1", FOO_RENDERED));

        let mut prior = read_detail_with_evidence(
            "call-1",
            r#"{"path":"foo.rs"}"#,
            FOO_RENDERED,
            read_evidence("foo.rs", 1, 2, 2, FOO_RENDERED),
        );
        prior.name = tool_name.to_string();
        agent
            .tool_context_details
            .insert("call-1".to_string(), prior);

        let mut repeated = read_detail_with_evidence(
            "call-2",
            r#"{"path":"foo.rs"}"#,
            FOO_RENDERED,
            read_evidence("foo.rs", 1, 2, 2, FOO_RENDERED),
        );
        repeated.name = tool_name.to_string();
        agent
            .tool_context_details
            .insert("call-2".to_string(), repeated);

        assert!(
            reused_pointer(&agent, "call-2", FOO_RENDERED).is_some(),
            "{tool_name} should use typed read evidence for reuse"
        );
    }
}

/// (a) A re-read of an unchanged window appends a compact pointer that references
/// the earlier read, and leaves that earlier full copy byte-for-byte intact.
#[tokio::test]
async fn unchanged_reread_appends_pointer_and_keeps_prior_full() {
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn a() {}\nfn b() {}\n");

    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_call_response("call-1", "foo.rs"),
            read_call_response("call-2", "foo.rs"),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    let sink = Arc::new(RecordingSink::default());
    let result = agent
        .run("read foo twice", CancellationToken::new(), sink.clone())
        .await
        .unwrap();
    assert_eq!(result, AgentRunResult::Completed("done".to_string()));

    let tools = tool_messages(&agent.messages);
    let first = tools
        .iter()
        .find(|(id, _)| id == "call-1")
        .expect("first read present");
    let second = tools
        .iter()
        .find(|(id, _)| id == "call-2")
        .expect("second read present");

    assert_eq!(
        first.1, FOO_RENDERED,
        "the earlier full read must be left untouched (prefix preserved)"
    );
    assert!(
        second.1.starts_with(crate::agent::REUSED_READ_MARKER),
        "the duplicate read should collapse to a pointer: {}",
        second.1
    );
    assert!(
        second.1.contains("call-1"),
        "the pointer should reference the retained read: {}",
        second.1
    );
    assert!(
        second.1.contains("lines 1-2"),
        "the pointer should name the reused window: {}",
        second.1
    );
    assert!(
        sink.last_result_for("call-2")
            .is_some_and(|result| result.starts_with(crate::agent::REUSED_READ_MARKER)),
        "the TUI card for the duplicate read should be relabelled to the pointer"
    );
}

#[tokio::test]
async fn adjacent_live_windows_jointly_satisfy_a_read_before_execution() {
    let fixture = TestFixture::new();
    let body = (1..=160)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    fixture.create_file("foo.rs", &body);
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_region_response("call-1", "foo.rs", 1, 20),
            read_region_response("call-2", "foo.rs", 21, 40),
            read_region_response("call-3", "foo.rs", 10, 30),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "read overlapping windows",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    let third = tool_messages(&agent.messages)
        .into_iter()
        .find(|(id, _)| id == "call-3")
        .map(|(_, content)| content)
        .unwrap();
    assert!(
        third.starts_with(crate::agent::REUSED_READ_MARKER),
        "{third}"
    );
    assert!(third.contains("source_calls: call-1, call-2"), "{third}");
    let event = &agent.read_evidence.inspection_events["call-3"];
    assert_eq!(event.outcome, InspectionOutcome::Reused);
    assert!(event.avoided_chars > 0);
}

#[tokio::test]
async fn partially_covered_read_executes_only_the_uncovered_delta() {
    let fixture = TestFixture::new();
    let body = (1..=160)
        .map(|line| format!("line {line}\n"))
        .collect::<String>();
    fixture.create_file("foo.rs", &body);
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_region_response("call-1", "foo.rs", 1, 20),
            read_region_response("call-2", "foo.rs", 10, 30),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "extend the first window",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    let second = tool_messages(&agent.messages)
        .into_iter()
        .find(|(id, _)| id == "call-2")
        .map(|(_, content)| content)
        .unwrap();
    assert!(
        second.starts_with(crate::agent::PARTIAL_READ_REUSE_MARKER),
        "{second}"
    );
    assert!(second.contains("kept lines 10-20"), "{second}");
    assert!(second.contains("21: line 21"), "{second}");
    assert!(second.contains("30: line 30"), "{second}");
    assert!(!second.contains("10: line 10"), "{second}");
    let evidence = agent.tool_context_details["call-2"]
        .read_evidence
        .as_ref()
        .unwrap();
    assert_eq!(evidence.window().start_line, 21);
    assert_eq!(evidence.window().end_line, Some(30));
    let event = &agent.read_evidence.inspection_events["call-2"];
    assert_eq!(event.outcome, InspectionOutcome::Reused);
    assert!(event.avoided_chars > 0);
}

#[tokio::test]
async fn deduped_reread_is_compact_and_keeps_target_fresh() {
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn a() {}\nfn b() {}\n");

    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_call_response("call-1", "foo.rs"),
            read_call_response("call-2", "foo.rs"),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "read foo twice",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    let first = agent.tool_context_details.get("call-1").unwrap();
    assert!(
        first
            .read_evidence
            .as_ref()
            .is_some_and(|evidence| !evidence.freshness().requires_marker()),
        "the retained target must keep Fresh read evidence",
    );
    let second = agent.tool_context_details.get("call-2").unwrap();
    assert!(second.read_evidence.is_none());
    assert_eq!(second.reuse_target_call_id.as_deref(), Some("call-1"));

    let (_, call_2) = tool_messages(&agent.messages)
        .into_iter()
        .find(|(id, _)| id == "call-2")
        .expect("call-2 tool message present");
    assert!(
        call_2.starts_with(crate::agent::REUSED_READ_MARKER),
        "the re-read should carry the reuse note: {call_2}",
    );
    assert!(
        !call_2.contains("fn a() {}"),
        "fresh duplicate bytes should be omitted from the compact result: {call_2}",
    );
    let admission = agent.read_evidence.inspection_events.get("call-2").unwrap();
    assert_eq!(admission.outcome, InspectionOutcome::Reused);
    assert_eq!(admission.reason, InspectionReason::FreshVisibleCoverage);
    assert_eq!(admission.requested_chars, FOO_RENDERED.chars().count());
    assert_eq!(
        admission.avoided_chars,
        admission
            .requested_chars
            .saturating_sub(admission.returned_chars)
    );
    assert_eq!(agent.usage_turns()[0].inspection_executed, 1);
    assert_eq!(agent.usage_turns()[1].inspection_reused, 1);
    assert_eq!(
        agent.usage_turns()[1].inspection_avoided_chars,
        admission.avoided_chars
    );

    agent.refresh_read_evidence_freshness().await;
    agent.refresh_stale_read_advisory();
    let outgoing = agent.outgoing_messages_for(&agent.messages);
    let system = serde_json::to_value(&outgoing[0]).unwrap();
    assert!(
        !system["content"]
            .as_str()
            .unwrap()
            .contains("Files changed since you read them"),
    );
}

#[tokio::test]
async fn repeat_after_compact_reuse_returns_real_bytes_and_keeps_sibling_read() {
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn foo() {}\n");
    fixture.create_file("bar.rs", "fn bar() {}\n");
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_call_response("call-1", "foo.rs"),
            read_call_response("call-2", "foo.rs"),
            read_batch_response(&[("call-3", "foo.rs"), ("call-4", "bar.rs")]),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "reuse foo, then inspect bar",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    let tools = tool_messages(&agent.messages);
    let redundant = tools.iter().find(|(id, _)| id == "call-3").unwrap();
    let useful = tools.iter().find(|(id, _)| id == "call-4").unwrap();
    assert!(redundant.1.contains("fn foo() {}"), "{}", redundant.1);
    assert!(useful.1.contains("fn bar() {}"), "{}", useful.1);
    assert_eq!(
        agent.read_evidence.inspection_events["call-3"].outcome,
        InspectionOutcome::Executed
    );
    assert_eq!(
        agent.read_evidence.inspection_events["call-3"].reason,
        InspectionReason::RepeatedFreshReuse
    );
}

#[tokio::test]
async fn range_shifting_after_compact_reuse_returns_real_bytes() {
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn a() {}\nfn b() {}\n");
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_call_response("call-1", "foo.rs"),
            read_window_response("call-2", "foo.rs", 1, 1),
            read_window_response("call-3", "foo.rs", 2, 1),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "read shifting windows",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    let tools = tool_messages(&agent.messages);
    assert!(
        tools
            .iter()
            .find(|(id, _)| id == "call-2")
            .is_some_and(|(_, content)| content.starts_with(crate::agent::REUSED_READ_MARKER))
    );
    assert!(
        tools
            .iter()
            .find(|(id, _)| id == "call-3")
            .is_some_and(|(_, content)| content.contains("2: fn b() {}"))
    );
    assert_eq!(
        agent.read_evidence.inspection_events["call-3"].outcome,
        InspectionOutcome::Executed
    );
    assert_eq!(
        agent.read_evidence.inspection_events["call-3"].reason,
        InspectionReason::RepeatedFreshReuse
    );
}

#[tokio::test]
async fn explicit_default_offset_after_truncated_reuse_returns_real_bytes() {
    let fixture = TestFixture::new();
    let body = (1..=1_400)
        .map(|line| {
            format!(
                "fn line_{line}() {{ let value = \"{}\"; }}\n",
                "x".repeat(40)
            )
        })
        .collect::<String>();
    fixture.create_file("foo.rs", &body);
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_limit_response("call-1", "foo.rs", 1_000),
            read_limit_response("call-2", "foo.rs", 1_000),
            read_window_response("call-3", "foo.rs", 1, 1_000),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "retry a truncated read with an explicit default offset",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    let tools = tool_messages(&agent.messages);
    let second = tools
        .iter()
        .find(|(id, _)| id == "call-2")
        .map(|(_, content)| content)
        .expect("call-2 result");
    assert!(
        second.starts_with(crate::agent::REUSED_READ_MARKER)
            && second.contains("continue with offset=")
            && second.contains("do not restart at offset=1"),
        "{second}"
    );
    assert!(
        tools
            .iter()
            .find(|(id, _)| id == "call-3")
            .is_some_and(|(_, content)| {
                content.contains("1: fn line_1()")
                    && !content.starts_with(crate::agent::REUSED_READ_MARKER)
            })
    );
    assert_eq!(
        agent.read_evidence.inspection_events["call-3"].reason,
        InspectionReason::RepeatedFreshReuse
    );
}

#[tokio::test]
async fn repeated_read_region_after_compact_pointer_returns_real_bytes() {
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_call_response("call-1", "foo.rs"),
            read_region_response("call-2", "foo.rs", 2, 3),
            read_region_response("call-3", "foo.rs", 2, 3),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "read the same region after a compact pointer",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    let tools = tool_messages(&agent.messages);
    assert!(
        tools
            .iter()
            .find(|(id, _)| id == "call-2")
            .is_some_and(|(_, content)| content.starts_with(crate::agent::REUSED_READ_MARKER))
    );
    assert!(
        tools
            .iter()
            .find(|(id, _)| id == "call-3")
            .is_some_and(|(_, content)| {
                content.contains("2: fn b() {}") && content.contains("3: fn c() {}")
            })
    );
    assert_eq!(
        agent.read_evidence.inspection_events["call-3"].outcome,
        InspectionOutcome::Executed
    );
    assert_eq!(
        agent.read_evidence.inspection_events["call-3"].reason,
        InspectionReason::RepeatedFreshReuse
    );
}

#[tokio::test]
async fn repeated_read_region_rejection_does_not_stop_the_run() {
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_call_response("call-1", "foo.rs"),
            read_region_response("call-2", "foo.rs", 2, 3),
            read_region_response("call-3", "foo.rs", 2, 3),
            read_region_response("call-4", "foo.rs", 2, 3),
            read_region_response("call-5", "foo.rs", 2, 3),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    let result = agent
        .run(
            "keep reading the same region",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    let tools = tool_messages(&agent.messages);
    assert!(
        tools
            .iter()
            .find(|(id, _)| id == "call-3")
            .is_some_and(|(_, content)| content.contains("2: fn b() {}"))
    );
    assert!(
        tools
            .iter()
            .find(|(id, _)| id == "call-4")
            .is_some_and(|(_, content)| content.contains("repeated read storm detected"))
    );
    assert!(
        tools.iter().any(|(id, _)| id == "call-5"),
        "one explicit retry after the rejection must remain available"
    );
}

#[tokio::test]
async fn repeated_bash_file_read_recovers_after_one_rejection() {
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn from_bash() {}\n");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let bash_tool = Arc::new(BashReadTool {
        output: "fn from_bash() {}\n".to_string(),
        calls: calls.clone(),
        read_tracker: fixture.read_tracker.clone(),
        path: fixture
            .project_root
            .join("foo.rs")
            .canonicalize()
            .expect("fixture file should canonicalize"),
    });
    let mut registry = ToolRegistry::new();
    registry.register(bash_tool);
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            bash_read_response("call-1", "foo.rs"),
            bash_read_response("call-2", "foo.rs"),
            bash_read_response("call-3", "foo.rs"),
            bash_read_response("call-4", "foo.rs"),
            bash_read_response("call-5", "foo.rs"),
            finish_response("done"),
        ])),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    let result = agent
        .run(
            "keep reading the same file through bash",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert_eq!(
        calls.lock().await.len(),
        4,
        "the rejected call stays blocked, then one explicit retry executes"
    );
    let tools = tool_messages(&agent.messages);
    assert_eq!(tools[0].1, "fn from_bash() {}\n");
    assert!(
        tools[1].1.starts_with(crate::agent::REUSED_READ_MARKER),
        "{}",
        tools[1].1
    );
    assert_eq!(
        tools[2].1, "fn from_bash() {}\n",
        "the first retry after a compact pointer must return real bytes"
    );
    assert!(tools[3].1.contains("repeated read storm detected"));
    assert_eq!(tools[4].1, "fn from_bash() {}\n");
    assert_eq!(
        agent.read_evidence.inspection_events["call-3"].reason,
        InspectionReason::RepeatedFreshReuse
    );
}

#[tokio::test]
async fn stale_flagged_reread_returns_full_content_not_a_dedup_stub() {
    // Sessions 245 & 251: the stale-read advisory told the model "this file
    // changed, re-read it", but the re-read collapsed to a `[reused previous
    // read] … identical content omitted` stub — handing back no bytes to act
    // on. The model looped re-reading and the loop guard killed the run. When a
    // file is currently flagged stale, the re-read must return REAL content, so
    // the model gets what the advisory promised and the fresh read supersedes
    // the stale row.
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn a() {}\nfn b() {}\n");
    let canonical = std::fs::canonicalize(fixture.project_root.join("foo.rs")).unwrap();

    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_call_response("call-2", "foo.rs"),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    // Seed a prior full read whose evidence is stale against the real file
    // (len 0, no hash/mtime → `refresh_freshness` judges it Stale), so the
    // advisory is actively nagging the model to re-read foo.rs.
    agent.messages.push(test_user_message("hi"));
    agent
        .messages
        .push(tool_result_message("call-1", FOO_RENDERED));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        read_detail_with_evidence(
            "call-1",
            r#"{"path":"foo.rs","offset":1,"limit":2}"#,
            FOO_RENDERED,
            ReadEvidence::new(
                "foo.rs",
                canonical.clone(),
                ReadWindow {
                    requested_offset: 1,
                    requested_limit: 2,
                    start_line: 1,
                    end_line: Some(2),
                    total_lines: Some(2),
                },
                ReadCoverage::Full,
                FOO_RENDERED,
                None,
                0,
                None,
            ),
        ),
    );

    agent
        .run(
            "re-read foo",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    // The re-read returns the real file bytes (annotated with a reuse note),
    // NEVER a content-less stub — so a model told to re-read a changed file
    // actually gets the content it needs instead of looping (sessions 245, 251).
    let call_2 = agent.tool_context_details.get("call-2").unwrap();
    assert!(
        call_2.reuse_target_call_id.is_none(),
        "the re-read must stay a real read row, not a content-less pointer",
    );
    let (_, call_2_content) = tool_messages(&agent.messages)
        .into_iter()
        .find(|(id, _)| id == "call-2")
        .expect("call-2 tool message present");
    assert!(
        call_2_content.contains("fn a() {}") && call_2_content.contains("fn b() {}"),
        "the re-read must return the real file bytes, not just a `{}` note: {call_2_content}",
        crate::agent::REUSED_READ_MARKER,
    );

    // The fresh full read supersedes the stale row, so the advisory clears.
    agent.refresh_read_evidence_freshness().await;
    agent.refresh_stale_read_advisory();
    let outgoing = agent.outgoing_messages_for(&agent.messages);
    let system = serde_json::to_value(&outgoing[0]).unwrap();
    assert!(
        !system["content"]
            .as_str()
            .unwrap()
            .contains("Files changed since you read them"),
        "re-reading the flagged file must clear the advisory",
    );
}

#[tokio::test]
async fn edit_advances_freshness_without_relabeling_old_displayed_bytes() {
    // The model's own edit must not leave its earlier read flagged stale (Fix 2):
    // after editing foo.rs the read of foo.rs is re-baselined to the new content,
    // so no stale-read advisory nags the model to re-verify what it just wrote.
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn a() {}\n");

    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_call_response("call-1", "foo.rs"),
            edit_call_response("call-2", "foo.rs", "fn a() {}", "fn b() {}"),
            read_call_response("call-3", "foo.rs"),
            finish_response("done"),
        ])),
        read_edit_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "edit foo",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    // The read of foo.rs is Fresh against the post-edit content, even though the
    // edit changed the file — the model's own edit no longer flags its read.
    let read = agent.tool_context_details.get("call-1").unwrap();
    let evidence = read
        .read_evidence
        .as_ref()
        .expect("first read should retain evidence");
    assert!(
        !evidence.freshness().requires_marker(),
        "a successful edit must re-baseline the read of the same file to Fresh"
    );
    assert_ne!(
        evidence.file_digest_at_read(),
        evidence.freshness_baseline().current_file_digest(),
        "the old observation must keep its pre-edit digest"
    );
    let tools = tool_messages(&agent.messages);
    let post_edit_read = tools
        .iter()
        .find(|(id, _)| id == "call-3")
        .expect("post-edit read should be present");
    assert!(post_edit_read.1.contains("fn b() {}"));
    assert!(
        !post_edit_read
            .1
            .starts_with(crate::agent::REUSED_READ_MARKER),
        "post-edit bytes must not point at the pre-edit observation"
    );
    agent.refresh_read_evidence_freshness().await;
    agent.refresh_stale_read_advisory();
    let outgoing = agent.outgoing_messages_for(&agent.messages);
    let system = serde_json::to_value(&outgoing[0]).unwrap();
    assert!(
        !system["content"]
            .as_str()
            .unwrap()
            .contains("Files changed since you read them"),
        "the model's own edit must not leave a stale-read advisory"
    );
}

#[tokio::test]
async fn failed_reread_keeps_failure_status_and_full_error() {
    let fixture = TestFixture::new();

    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![
            read_call_response("call-1", "missing.rs"),
            read_call_response("call-2", "missing.rs"),
            finish_response("done"),
        ])),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    let sink = Arc::new(RecordingSink::default());
    agent
        .run(
            "read missing file twice",
            CancellationToken::new(),
            sink.clone(),
        )
        .await
        .unwrap();

    let tools = tool_messages(&agent.messages);
    let second = tools
        .iter()
        .find(|(id, _)| id == "call-2")
        .expect("second failed read present");

    assert!(
        !second.1.starts_with(crate::agent::REUSED_READ_MARKER),
        "failed reads must not collapse to a reused-read pointer: {}",
        second.1
    );
    assert!(
        second.1.starts_with("Error:") && second.1.contains("missing.rs"),
        "the model should receive the actual failure: {}",
        second.1
    );
    assert_eq!(
        sink.last_success_for("call-2"),
        Some(false),
        "the TUI card must remain failed, not be relabelled as a successful reuse"
    );
}

struct MutateBetweenReadsProvider {
    root: PathBuf,
    path: &'static str,
    content: &'static str,
    inner: MockProvider,
    calls: AtomicUsize,
    mutate_on_call: usize,
}

#[async_trait::async_trait]
impl Provider for MutateBetweenReadsProvider {
    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        // Rewrite the file just before the second read is requested, so the
        // re-read sees fresh content and must not dedup.
        if self.calls.fetch_add(1, Ordering::SeqCst) == self.mutate_on_call {
            std::fs::write(self.root.join(self.path), self.content).map_err(|error| {
                crate::provider::ProviderFailure::configuration(error.to_string())
            })?;
        }
        self.inner
            .chat_stream(messages, tools, cancellation_token, sink)
            .await
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        self.inner.list_models().await
    }
}

/// (b) A re-read after the file changed returns full fresh content (no pointer),
/// so the model still gets evidence after an edit.
#[tokio::test]
async fn reread_after_change_returns_full_content() {
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn a() {}\n");

    let provider = MutateBetweenReadsProvider {
        root: fixture.project_root.clone(),
        path: "foo.rs",
        content: "fn changed() {}\n",
        inner: MockProvider::new(vec![
            read_call_response("call-1", "foo.rs"),
            read_call_response("call-2", "foo.rs"),
            finish_response("done"),
        ]),
        calls: AtomicUsize::new(0),
        mutate_on_call: 1,
    };
    let mut agent = Agent::new(
        Box::new(provider),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "read, edit, read",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    let tools = tool_messages(&agent.messages);
    let second = tools
        .iter()
        .find(|(id, _)| id == "call-2")
        .expect("second read present");
    assert!(
        !second.1.starts_with(crate::agent::REUSED_READ_MARKER),
        "a changed file must not dedup: {}",
        second.1
    );
    assert!(
        second.1.contains("fn changed()"),
        "the re-read should carry the new content: {}",
        second.1
    );
}

#[tokio::test]
async fn changed_file_after_compact_reuse_executes_the_next_read() {
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn a() {}\n");
    let provider = MutateBetweenReadsProvider {
        root: fixture.project_root.clone(),
        path: "foo.rs",
        content: "fn changed() {}\n",
        inner: MockProvider::new(vec![
            read_call_response("call-1", "foo.rs"),
            read_call_response("call-2", "foo.rs"),
            read_call_response("call-3", "foo.rs"),
            finish_response("done"),
        ]),
        calls: AtomicUsize::new(0),
        mutate_on_call: 2,
    };
    let mut agent = Agent::new(
        Box::new(provider),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "read through an external change",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    let tools = tool_messages(&agent.messages);
    let second = tools.iter().find(|(id, _)| id == "call-2").unwrap();
    let third = tools.iter().find(|(id, _)| id == "call-3").unwrap();
    assert!(second.1.starts_with(crate::agent::REUSED_READ_MARKER));
    assert!(third.1.contains("fn changed() {}"), "{}", third.1);
    assert!(!third.1.starts_with("Error:"), "{}", third.1);
}

struct MutateBeforeEachRereadProvider {
    root: PathBuf,
    path: &'static str,
    inner: MockProvider,
    calls: AtomicUsize,
    read_calls: usize,
}

#[async_trait::async_trait]
impl Provider for MutateBeforeEachRereadProvider {
    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call > 0 && call < self.read_calls {
            std::fs::write(
                self.root.join(self.path),
                format!("fn changed_v{call}() {{}}\n"),
            )
            .map_err(|error| crate::provider::ProviderFailure::configuration(error.to_string()))?;
        }
        self.inner
            .chat_stream(messages, tools, cancellation_token, sink)
            .await
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        self.inner.list_models().await
    }
}

#[tokio::test]
async fn identical_read_arguments_are_allowed_when_file_changes_each_turn() {
    let fixture = TestFixture::new();
    fixture.create_file("foo.rs", "fn initial() {}\n");
    let read_calls = 5;
    let provider = MutateBeforeEachRereadProvider {
        root: fixture.project_root.clone(),
        path: "foo.rs",
        inner: MockProvider::new(vec![
            read_call_response("call-1", "foo.rs"),
            read_call_response("call-2", "foo.rs"),
            read_call_response("call-3", "foo.rs"),
            read_call_response("call-4", "foo.rs"),
            read_call_response("call-5", "foo.rs"),
            finish_response("done"),
        ]),
        calls: AtomicUsize::new(0),
        read_calls,
    };
    let mut agent = Agent::new(
        Box::new(provider),
        read_registry(&fixture),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "keep reading this file as it changes",
            CancellationToken::new(),
            Arc::new(RecordingSink::default()),
        )
        .await
        .unwrap();

    let tools = tool_messages(&agent.messages);
    assert_eq!(tools.len(), read_calls);
    assert!(
        tools
            .iter()
            .all(|(_, content)| !content.starts_with("Error:")
                && !content.starts_with(crate::agent::REUSED_READ_MARKER)),
        "every changed version should execute as a fresh read: {tools:?}"
    );
}

/// (c) When the earlier copy has already been evicted (stubbed), the re-read
/// returns full content rather than a pointer to content the model no longer has.
#[tokio::test]
async fn dedup_skips_when_prior_copy_is_stubbed() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());

    agent.messages.push(test_user_message("hi"));
    agent
        .messages
        .push(tool_result_message("call-1", FOO_RENDERED));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        read_detail_with_evidence(
            "call-1",
            r#"{"path":"foo.rs"}"#,
            FOO_RENDERED,
            read_evidence("foo.rs", 1, 2, 2, FOO_RENDERED),
        ),
    );
    agent.tool_context_details.insert(
        "call-2".to_string(),
        read_detail_with_evidence(
            "call-2",
            r#"{"path":"foo.rs"}"#,
            FOO_RENDERED,
            read_evidence("foo.rs", 1, 2, 2, FOO_RENDERED),
        ),
    );

    // Live prior copy ⇒ dedups.
    assert!(
        reused_pointer(&agent, "call-2", FOO_RENDERED).is_some(),
        "a live identical prior read should dedup"
    );

    // Stub the prior copy ⇒ must fall back to full content (no blinding).
    agent.context_controls.insert(
        ContextNodeId::tool("call-1").into_string(),
        ContextControlState {
            stubbed: true,
            ..Default::default()
        },
    );
    assert!(
        reused_pointer(&agent, "call-2", FOO_RENDERED).is_none(),
        "a stubbed prior read must not be reused"
    );
}

/// (d) A pointer never supersedes the full read it references, so the retained
/// copy is never stubbed and the cached prefix stays byte-identical.
#[tokio::test]
async fn pointer_does_not_supersede_or_stub_its_target() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());
    let pointer = format!(
        "{} foo.rs lines 1-2 — unchanged since tool call-1",
        crate::agent::REUSED_READ_MARKER
    );

    agent.messages.push(test_user_message("hi"));
    agent.messages.push(assistant_tool_call_message(
        "call-1",
        "read",
        r#"{"path":"foo.rs"}"#,
    ));
    agent
        .messages
        .push(tool_result_message("call-1", FOO_RENDERED));
    agent.messages.push(assistant_tool_call_message(
        "call-2",
        "read",
        r#"{"path":"foo.rs"}"#,
    ));
    agent.messages.push(tool_result_message("call-2", &pointer));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        read_detail("call-1", r#"{"path":"foo.rs"}"#, FOO_RENDERED),
    );
    let mut call2_detail = read_detail("call-2", r#"{"path":"foo.rs"}"#, &pointer);
    call2_detail.reuse_target_call_id = Some("call-1".to_string());
    agent
        .tool_context_details
        .insert("call-2".to_string(), call2_detail);

    assert!(
        agent.superseded_read_indices().is_empty(),
        "the pointer must not mark the retained full read as superseded"
    );
    assert_eq!(
        agent.apply_context_gc(true),
        0,
        "GC must not stub anything, leaving the cached prefix intact"
    );
}

/// (e) A dedup pointer keeps its referenced full read live through both
/// continuous context GC and automatic compaction planning. Otherwise the next
/// provider request could contain a pointer to content that was stubbed or
/// summarized before the model saw it.
#[tokio::test]
async fn pointer_target_is_protected_from_gc_and_compaction() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());
    let large = large_read_rendered();
    let pointer = format!(
        "{} foo.rs lines 1-450 — unchanged since tool call-1; identical content omitted",
        crate::agent::REUSED_READ_MARKER
    );

    agent.messages.push(test_user_message("start"));
    agent.messages.push(assistant_tool_call_message(
        "call-1",
        "read",
        r#"{"path":"foo.rs"}"#,
    ));
    agent.messages.push(tool_result_message("call-1", &large));
    agent.messages.push(assistant_tool_call_message(
        "call-2",
        "read",
        r#"{"path":"foo.rs"}"#,
    ));
    agent.messages.push(tool_result_message("call-2", &pointer));
    agent
        .messages
        .push(test_user_message("newer user message one"));
    agent
        .messages
        .push(test_user_message("newer user message two"));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        read_detail("call-1", r#"{"path":"foo.rs"}"#, &large),
    );
    let mut call2_detail = read_detail("call-2", r#"{"path":"foo.rs"}"#, &pointer);
    call2_detail.reuse_target_call_id = Some("call-1".to_string());
    agent
        .tool_context_details
        .insert("call-2".to_string(), call2_detail);

    let target_index = agent
        .messages
        .iter()
        .position(|message| tool_message_call_id(message).as_deref() == Some("call-1"))
        .expect("target read message should be present");

    assert_eq!(
        agent.apply_context_gc(true),
        0,
        "GC must not stub a full read while a pointer references it"
    );
    assert!(
        !agent
            .context_controls
            .get(&ContextNodeId::tool("call-1").into_string())
            .is_some_and(|state| state.stubbed),
        "the pointer target must stay verbatim"
    );

    let draft = agent.build_compaction_draft(1);
    assert!(
        !draft
            .tool_outputs_to_stub
            .iter()
            .any(|stub| stub.old_index == target_index),
        "automatic compaction must not stub a pointer target"
    );
    let target_message = agent.messages[target_index].clone();
    assert!(
        !draft
            .omitted
            .iter()
            .flat_map(|omitted| omitted.originals.iter())
            .any(|message| message == &target_message),
        "automatic compaction must not summarize away a pointer target"
    );
}

/// A pointer target that somehow carries an automatic stub (only reachable from
/// legacy/persisted state — `dedup_read_pointer` refuses to target a stubbed
/// copy) must be restored by the every-turn GC pass, and that restore is a
/// wire-byte mutation, so it is tagged.
#[tokio::test]
async fn context_gc_restores_and_tags_stubbed_pointer_target() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());
    let large = large_read_rendered();
    let pointer = format!(
        "{} foo.rs lines 1-450 — unchanged since tool call-1; identical content omitted",
        crate::agent::REUSED_READ_MARKER
    );

    agent.messages.push(test_user_message("start"));
    agent.messages.push(assistant_tool_call_message(
        "call-1",
        "read",
        r#"{"path":"foo.rs"}"#,
    ));
    agent.messages.push(tool_result_message("call-1", &large));
    agent.messages.push(assistant_tool_call_message(
        "call-2",
        "read",
        r#"{"path":"foo.rs"}"#,
    ));
    agent.messages.push(tool_result_message("call-2", &pointer));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        read_detail("call-1", r#"{"path":"foo.rs"}"#, &large),
    );
    let mut call2_detail = read_detail("call-2", r#"{"path":"foo.rs"}"#, &pointer);
    call2_detail.reuse_target_call_id = Some("call-1".to_string());
    agent
        .tool_context_details
        .insert("call-2".to_string(), call2_detail);

    let target_id = ContextNodeId::tool("call-1").into_string();
    agent.context_controls.insert(
        target_id.clone(),
        ContextControlState {
            stubbed: true,
            stub_reason: Some(ContextStubReason::SupersededRead),
            ..Default::default()
        },
    );
    agent.pending_context_rewrite = Default::default();

    agent.apply_context_gc(false);

    assert!(
        !agent
            .context_controls
            .get(&target_id)
            .is_some_and(|state| state.stubbed),
        "a stubbed pointer target must be restored so the pointer's copy stays live"
    );
    assert_eq!(
        agent.pending_context_rewrite.kind,
        ContextRewriteKind::Gc,
        "restoring a pointer target is a wire-byte mutation and must be tagged"
    );
}

/// (f) A read of a different window is not a duplicate even with equal bytes.
#[tokio::test]
async fn dedup_skips_a_different_window() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());

    agent.messages.push(test_user_message("hi"));
    agent
        .messages
        .push(tool_result_message("call-1", FOO_RENDERED));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        read_detail("call-1", r#"{"path":"foo.rs","offset":1}"#, FOO_RENDERED),
    );
    agent.tool_context_details.insert(
        "call-2".to_string(),
        read_detail("call-2", r#"{"path":"foo.rs","offset":50}"#, FOO_RENDERED),
    );

    assert!(
        reused_pointer(&agent, "call-2", FOO_RENDERED).is_none(),
        "a different offset is a different window, not a duplicate"
    );
}

/// Raw `cat` output: file bytes with no read-tool line numbers.
const CAT_OUTPUT: &str = "fn a() {}\nfn b() {}\n";

/// (g) A repeated identical `cat` of the same file dedups to a pointer, just
/// like the `read` tool.
#[tokio::test]
async fn bash_cat_reread_dedups_to_pointer() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());

    agent.messages.push(test_user_message("hi"));
    agent
        .messages
        .push(tool_result_message("call-1", CAT_OUTPUT));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        bash_detail("call-1", "cat foo.rs", CAT_OUTPUT),
    );
    agent.tool_context_details.insert(
        "call-2".to_string(),
        bash_detail("call-2", "cat foo.rs", CAT_OUTPUT),
    );

    let note = reused_pointer(&agent, "call-2", CAT_OUTPUT)
        .expect("an identical repeated cat should dedup");
    assert!(note.starts_with(crate::agent::REUSED_READ_MARKER));
    assert!(note.contains("foo.rs"));
    assert!(note.contains("call-1"));
}

/// (h) A `cat` whose bytes changed since the prior read falls back to full
/// content — the byte-identity gate prevents blinding the model.
#[tokio::test]
async fn bash_cat_reread_after_change_returns_full() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());

    agent
        .messages
        .push(tool_result_message("call-1", "fn a() {}\n"));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        bash_detail("call-1", "cat foo.rs", "fn a() {}\n"),
    );
    agent.tool_context_details.insert(
        "call-2".to_string(),
        bash_detail("call-2", "cat foo.rs", "fn a() {}\nfn b() {}\n"),
    );

    assert!(
        reused_pointer(&agent, "call-2", "fn a() {}\nfn b() {}\n").is_none(),
        "changed cat output must not be reused"
    );
}

/// (i) A `bash` read never points at a `read`-tool copy of the same file, even
/// when the bytes coincide — their signatures (and rendered text) differ.
#[tokio::test]
async fn bash_and_read_tool_do_not_cross_dedup() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());

    agent.messages.push(test_user_message("hi"));
    agent
        .messages
        .push(tool_result_message("call-1", CAT_OUTPUT));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        read_detail("call-1", r#"{"path":"foo.rs"}"#, CAT_OUTPUT),
    );
    agent.tool_context_details.insert(
        "call-2".to_string(),
        bash_detail("call-2", "cat foo.rs", CAT_OUTPUT),
    );

    assert!(
        reused_pointer(&agent, "call-2", CAT_OUTPUT).is_none(),
        "a bash read must not dedup against a read-tool copy"
    );
}

/// (j) Bash commands that are not single-file reads (a pipeline, multiple
/// operands, or a non-read program) never dedup.
#[tokio::test]
async fn bash_dedup_skips_non_single_file_reads() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());

    agent
        .messages
        .push(tool_result_message("call-1", CAT_OUTPUT));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        bash_detail("call-1", "cat foo.rs", CAT_OUTPUT),
    );

    for (call_id, command) in [
        ("call-echo", "echo hi"),
        ("call-multi", "cat foo.rs bar.rs"),
        ("call-pipe", "cat foo.rs | head"),
    ] {
        agent.tool_context_details.insert(
            call_id.to_string(),
            bash_detail(call_id, command, CAT_OUTPUT),
        );
        assert!(
            reused_pointer(&agent, call_id, CAT_OUTPUT).is_none(),
            "`{command}` is not a single-file read and must not dedup"
        );
    }
}
