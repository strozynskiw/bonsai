use super::*;
use crate::tool::read_evidence::{InspectionOutcome, InspectionReason};

struct GitInspectionTool {
    response: String,
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait]
impl Tool for GitInspectionTool {
    fn name(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "test git inspection"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["op"],
            "properties": {
                "op": {"type": "string", "enum": ["diff", "show"]}
            },
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.calls.lock().await.push(args);
        Ok(ToolOutput::Text(self.response.clone()))
    }
}

fn git_call_response(id: &str, op: &str) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        content: String::new(),
        tool_calls: vec![test_tool_call(id, "git", &format!(r#"{{"op":"{op}"}}"#))],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    })
}

fn tool_messages(messages: &[ChatCompletionRequestMessage]) -> Vec<(String, String)> {
    messages
        .iter()
        .filter_map(|message| {
            let value = serde_json::to_value(message).ok()?;
            (value.get("role").and_then(serde_json::Value::as_str) == Some("tool")).then(|| {
                (
                    value
                        .get("tool_call_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    value
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                )
            })
        })
        .collect()
}

fn git_detail(call_id: &str, op: &str, rendered: &str) -> ToolContextDetail {
    ToolContextDetail {
        call_id: call_id.to_string(),
        name: "git".to_string(),
        arguments: format!(r#"{{"op":"{op}"}}"#),
        read_evidence: None,
        result: ToolContextResult::Text {
            rendered: rendered.to_string(),
        },
        reuse_target_call_id: None,
    }
}

struct MissingReuseTool;

#[async_trait]
impl Tool for MissingReuseTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "test cached missing path"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {"path": {"type": "string"}},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::MissingPathReuse {
            text: "[reused missing-path evidence]\npath: missing.md".to_string(),
        })
    }
}

#[tokio::test]
async fn missing_path_reuse_records_reused_inspection_telemetry() {
    let fixture = TestFixture::new();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(MissingReuseTool));
    let responses = vec![
        Ok(StreamedResponse {
            tool_calls: vec![test_tool_call("call-1", "read", r#"{"path":"missing.md"}"#)],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            ..StreamedResponse::default()
        }),
    ];
    let mut agent = Agent::new(
        Box::new(MockProvider::new(responses)),
        Arc::new(registry),
        Arc::new(ToolRegistry::new()),
        fixture.read_tracker,
        String::new(),
        fixture.project_root,
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    agent
        .run(
            "read missing path",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    let event = &agent.read_evidence.inspection_events["call-1"];
    assert_eq!(event.outcome, InspectionOutcome::Reused);
    assert_eq!(event.reason, InspectionReason::MissingPathEvidence);
}

#[tokio::test]
async fn repeated_unchanged_git_diff_recovers_after_one_rejection() {
    let fixture = TestFixture::new();
    let diff = (1..=200)
        .map(|line| format!("+changed line {line}\n"))
        .collect::<String>();
    let executed = Arc::new(Mutex::new(Vec::new()));
    let git_tool = Arc::new(GitInspectionTool {
        response: diff.clone(),
        calls: executed.clone(),
    });
    let mut registry = ToolRegistry::new();
    registry.register(git_tool);
    let mut responses = (1..=4)
        .map(|index| git_call_response(&format!("call-{index}"), "diff"))
        .collect::<Vec<_>>();
    responses.push(Ok(StreamedResponse {
        content: "done".to_string(),
        tool_calls: vec![],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    }));
    let mut agent = Agent::new(
        Box::new(MockProvider::new(responses)),
        Arc::new(registry),
        Arc::new(ToolRegistry::new()),
        fixture.read_tracker,
        String::new(),
        fixture.project_root,
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    let result = agent
        .run(
            "inspect the same diff repeatedly",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();
    assert_eq!(result, AgentRunResult::Completed("done".to_string()));

    let tools = tool_messages(&agent.messages);
    assert_eq!(tools.len(), 4);
    assert_eq!(tools[0].1, diff);
    assert!(
        tools[1]
            .1
            .starts_with(crate::agent::REUSED_INSPECTION_MARKER),
        "{}",
        tools[1].1
    );
    assert!(
        tools[2]
            .1
            .starts_with("Error: repeated inspection loop detected"),
        "{}: {}",
        tools[2].0,
        tools[2].1
    );
    assert!(
        tools[3].1.contains("repeated unchanged inspection blocked"),
        "the admitted retry may still be deduplicated after execution: {}",
        tools[3].1
    );
    assert_eq!(executed.lock().await.len(), 3);
    assert_eq!(
        agent.read_evidence.inspection_events["call-1"].outcome,
        InspectionOutcome::Executed
    );
    assert_eq!(
        agent.read_evidence.inspection_events["call-2"].outcome,
        InspectionOutcome::Reused
    );
    assert_eq!(
        agent.read_evidence.inspection_events["call-3"].reason,
        InspectionReason::ToolFailed
    );
    assert!(agent.read_evidence.inspection_events["call-2"].avoided_chars > 0);
}

#[tokio::test]
async fn git_show_reuses_only_an_exact_unchanged_result() {
    let fixture = TestFixture::new();
    let mut agent = Agent::new(
        MockProvider::empty(),
        Arc::new(ToolRegistry::new()),
        Arc::new(ToolRegistry::new()),
        fixture.read_tracker,
        String::new(),
        fixture.project_root,
    )
    .unwrap();
    agent
        .messages
        .push(tool_result_message("call-1", "commit body\n"));
    agent.tool_context_details.insert(
        "call-1".to_string(),
        git_detail("call-1", "show", "commit body\n"),
    );
    agent.tool_context_details.insert(
        "call-2".to_string(),
        git_detail("call-2", "show", "commit body\n"),
    );

    assert!(matches!(
        agent.read_admission("call-2", "commit body\n"),
        ReadAdmission::Reuse(_)
    ));

    agent.tool_context_details.insert(
        "call-3".to_string(),
        git_detail("call-3", "show", "different commit body\n"),
    );
    assert!(matches!(
        agent.read_admission("call-3", "different commit body\n"),
        ReadAdmission::Execute(InspectionReason::NoFreshVisibleCoverage)
    ));
}
