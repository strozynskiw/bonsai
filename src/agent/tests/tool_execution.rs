use super::*;

struct FailingBashTool {
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
}

struct SuccessfulBashTool;

impl FailingBashTool {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Tool for SuccessfulBashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "mock successful bash"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["command"],
            "properties": {"command": {"type": "string"}},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::Command {
            rendered: concat!(
                "test output line 1\n",
                "test output line 2\n\n",
                "[Command summary]\n",
                "command: cargo test --locked\n",
                "exit_code: 0\n",
                "timed_out: false\n",
                "duration: 1s\n",
                "stdout_bytes: 36\n",
                "stderr_bytes: 0\n",
                "combined_output_chars: 36\n",
                "last_output:\n",
                "test output line 1\n",
                "test output line 2"
            )
            .to_string(),
            stdout: "test output line 1\ntest output line 2\n".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
            truncation: None,
        })
    }
}

#[async_trait]
impl Tool for FailingBashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "mock failing bash"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["command"],
            "properties": {"command": {"type": "string"}},
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.calls.lock().await.push(args);
        Ok(ToolOutput::Command {
            rendered: concat!(
                "error[E0609]: no field `tempdir` on type `tool::test_utils::TestFixture`\n",
                "    --> src/tool/bash.rs:2448:31\n",
                "help: a field with a similar name exists\n",
                "2448 |         let outside = fixture.temp_dir.path().join(\"outside-project\");\n"
            )
            .to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(101),
            timed_out: false,
            truncation: None,
        })
    }
}

/// Stub with a schema exercising the arg-repair rules end-to-end: an array of
/// strings, an integer, and an optional note.
struct RepairableFilesTool {
    calls: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait]
impl Tool for RepairableFilesTool {
    fn name(&self) -> &str {
        "files_tool"
    }

    fn description(&self) -> &str {
        "mock files tool"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        crate::tool::schema::closed_object(
            [
                (
                    "files",
                    crate::tool::schema::array_property(
                        "Files",
                        crate::tool::schema::string_property("File path"),
                    ),
                ),
                (
                    "count",
                    crate::tool::schema::bounded_integer_property("Count", Some(1), None),
                ),
                ("note", crate::tool::schema::string_property("Note")),
            ],
            &["files"],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.calls.lock().await.push(args);
        Ok(ToolOutput::Text("ok".to_string()))
    }
}

#[tokio::test]
async fn deepseek_shaped_arguments_are_repaired_and_tool_executes() {
    let fixture = TestFixture::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(RepairableFilesTool {
        calls: calls.clone(),
    }));

    // One tool call with the DeepSeek failure shapes (bare string for an
    // array, quoted integer, null optional), then a final text turn. A repair
    // miss would consume an extra error turn and exhaust the script instead.
    let provider = Box::new(MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call(
                "call-1",
                "files_tool",
                r#"{"files": "src/main.rs", "count": "5", "note": null}"#,
            )],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]));

    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    let result = agent
        .run("hello", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));

    let calls = calls.lock().await;
    assert_eq!(calls.len(), 1, "repaired call should reach the tool once");
    assert_eq!(
        calls[0],
        serde_json::json!({"files": ["src/main.rs"], "count": 5}),
        "arguments should arrive repaired: array wrapped, integer parsed, null dropped"
    );
}

#[tokio::test]
async fn bare_continuation_rejects_title_churn_and_executes_real_work() {
    let fixture = TestFixture::new();
    let title_tool = Arc::new(MockTool::new("set_session_title", "title changed"));
    let title_calls = title_tool.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(title_tool);
    registry.register(Arc::new(SuccessfulBashTool));
    let provider = Box::new(MockProvider::new(vec![
        Ok(StreamedResponse {
            tool_calls: vec![test_tool_call(
                "title-1",
                "set_session_title",
                r#"{"title":"Isolate TUI verification"}"#,
            )],
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            tool_calls: vec![test_tool_call(
                "bash-1",
                "bash",
                r#"{"command":"cargo test --locked"}"#,
            )],
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "No change was needed; the tests pass.".to_string(),
            ..StreamedResponse::default()
        }),
    ]));
    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.push_user_message_raw("Fix the parser and run tests");

    let result = agent
        .run("continue", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();

    assert_eq!(
        result,
        AgentRunResult::Completed("No change was needed; the tests pass.".to_string())
    );
    assert!(
        title_calls.lock().await.is_empty(),
        "continuation title call must be rejected before tool execution"
    );
    assert!(
        tool_message_text(&agent, "title-1").contains("unavailable on continuation"),
        "the model should receive precise recovery guidance"
    );
}

#[tokio::test]
async fn malformed_tool_arguments_return_precise_error_to_model() {
    let fixture = TestFixture::new();
    let mock_tool = Arc::new(MockTool::new("mock_tool", "tool result"));
    let tool_name = mock_tool.name().to_string();
    let calls = mock_tool.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(mock_tool);

    // First turn: model sends garbage that isn't valid JSON at all, and a
    // second turn where it sends the right shape so the agent can finish.
    let provider = Box::new(MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![crate::provider::ToolCall {
                id: "call-1".to_string(),
                name: tool_name.to_string(),
                arguments: r#"{"invoke name=\"plan_set_section\""}"#.to_string(),
            }],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![crate::provider::ToolCall {
                id: "call-2".to_string(),
                name: tool_name.to_string(),
                arguments: r#"{"title": "Feature X"}"#.to_string(),
            }],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]));

    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    let result = agent
        .run("hello", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));

    // The tool was only invoked once (with the valid arguments); the
    // first turn surfaced a precise parse error to the model instead of
    // calling the tool with a bogus `{}` and double-failing.
    let calls = calls.lock().await;
    assert_eq!(calls.len(), 1, "tool should not be called with empty args");
    assert_eq!(calls[0], serde_json::json!({"title": "Feature X"}));

    // The model's tool-result message for the failed call should name
    // the tool, list the required fields, echo the bad payload, and
    // include the parse error so the model can self-correct next turn.
    let tool_message = tool_message_text(&agent, "call-1");
    assert!(
        tool_message.contains("mock_tool"),
        "error should name the tool, got: {tool_message}"
    );
    assert!(
        tool_message.contains("title"),
        "error should list the required field, got: {tool_message}"
    );
    assert!(
        tool_message.contains("invoke name"),
        "error should echo the bad payload, got: {tool_message}"
    );
    assert!(
        tool_message.contains("Parse error") || tool_message.contains("parse error"),
        "error should include the parser detail, got: {tool_message}"
    );
}

#[tokio::test]
async fn malformed_tool_arguments_are_sanitized_in_followup_history() {
    let fixture = TestFixture::new();
    let mock_tool = Arc::new(MockTool::new("mock_tool", "tool result"));
    let calls = mock_tool.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(mock_tool);

    let provider = MockProvider::new(vec![
        Ok(StreamedResponse {
            content: "creating plan".to_string(),
            tool_calls: vec![test_tool_call("call-1", "mock_tool", "")],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]);
    let requests = provider.requests();

    let mut agent = Agent::new(
        Box::new(provider),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    let result = agent
        .run("hello", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert!(
        calls.lock().await.is_empty(),
        "tool should not execute with empty arguments"
    );

    let requests = requests.lock().await;
    let second = requests
        .get(1)
        .expect("follow-up request should include failed tool-call history");
    let arguments = second
        .iter()
        .find_map(|message| {
            let value = serde_json::to_value(message).ok()?;
            value
                .pointer("/tool_calls/0/function/arguments")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        })
        .expect("assistant tool-call arguments should be present");
    assert_eq!(
        arguments, "{}",
        "history sent to the next provider request must stay valid JSON"
    );

    let tool_message = tool_message_text(&agent, "call-1");
    assert!(
        tool_message.contains("Got: <empty>"),
        "tool error should still describe the original malformed payload: {tool_message}"
    );
}

#[tokio::test]
async fn invalid_tool_argument_shape_reports_missing_rejected_and_truncated_payload() {
    let fixture = TestFixture::new();
    let mock_tool = Arc::new(MockTool::new("mock_tool", "tool result"));
    let tool_name = mock_tool.name().to_string();
    let calls = mock_tool.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(mock_tool);

    let long_payload = (0..200)
        .map(|index| format!("line{index:03};"))
        .collect::<String>();
    let bad_args = serde_json::json!({
        "name": "wrong",
        "payload": long_payload,
    })
    .to_string();
    let provider = Box::new(MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![crate::provider::ToolCall {
                id: "call-1".to_string(),
                name: tool_name.to_string(),
                arguments: bad_args,
            }],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![crate::provider::ToolCall {
                id: "call-2".to_string(),
                name: tool_name.to_string(),
                arguments: r#"{"title": "Feature X"}"#.to_string(),
            }],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]));

    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    let result = agent
        .run("hello", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    let calls = calls.lock().await;
    assert_eq!(calls.len(), 1, "tool should only receive valid arguments");
    assert_eq!(calls[0], serde_json::json!({"title": "Feature X"}));

    let tool_message = tool_message_text(&agent, "call-1");
    assert!(tool_message.contains("mock_tool"), "{tool_message}");
    assert!(
        tool_message.contains("Required fields: title"),
        "{tool_message}"
    );
    assert!(
        tool_message.contains("Missing fields: title"),
        "{tool_message}"
    );
    assert!(
        tool_message.contains("Rejected fields: name, payload"),
        "{tool_message}"
    );
    assert!(
        tool_message.contains("Schema: object fields"),
        "{tool_message}"
    );
    assert!(
        tool_message.contains("title (required, string)"),
        "{tool_message}"
    );
    assert!(tool_message.contains("line000"), "{tool_message}");
    assert!(
        !tool_message.contains("line199"),
        "long bad payload should be truncated: {tool_message}"
    );
}

#[tokio::test]
async fn independent_tool_calls_run_concurrently_and_are_awaited() {
    let fixture = TestFixture::new();
    let barrier = Arc::new(Barrier::new(2));
    let started = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(BarrierTool::new(
        "slow_a",
        barrier.clone(),
        started.clone(),
    )));
    registry.register(Arc::new(BarrierTool::new(
        "slow_b",
        barrier,
        started.clone(),
    )));

    let provider = Box::new(MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![
                test_tool_call("call-1", "slow_a", "{}"),
                test_tool_call("call-2", "slow_b", "{}"),
            ],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]));

    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        agent.run("hello", CancellationToken::new(), Arc::new(StdoutSink)),
    )
    .await
    .expect("independent tool calls should run concurrently")
    .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert_eq!(started.load(Ordering::SeqCst), 2);
}

#[derive(Default)]
struct ToolStartBatchSink {
    batches: StdMutex<Vec<Vec<String>>>,
}

impl ToolStartBatchSink {
    fn batches(&self) -> Vec<Vec<String>> {
        self.batches
            .lock()
            .expect("tool start batch sink mutex should not be poisoned")
            .clone()
    }
}

impl OutputSink for ToolStartBatchSink {
    fn tool_calls_started(&self, calls: &[ToolCallStart]) {
        self.batches
            .lock()
            .expect("tool start batch sink mutex should not be poisoned")
            .push(calls.iter().map(|call| call.id.clone()).collect());
    }
}

#[tokio::test]
async fn multi_tool_response_starts_and_records_calls_as_one_batch() {
    let fixture = TestFixture::new();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(MockTool::new("tool_a", "a done")));
    registry.register(Arc::new(MockTool::new("tool_b", "b done")));

    let provider = MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![
                test_tool_call("call-1", "tool_a", r#"{"title":"A"}"#),
                test_tool_call("call-2", "tool_b", r#"{"title":"B"}"#),
            ],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]);
    let requests = provider.requests();
    let sink = Arc::new(ToolStartBatchSink::default());
    let mut agent = Agent::new(
        Box::new(provider),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    let result = agent
        .run("hello", CancellationToken::new(), sink.clone())
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert_eq!(sink.batches(), vec![vec!["call-1", "call-2"]]);

    let requests = requests.lock().await;
    let second = requests
        .get(1)
        .expect("second request should include tool-call context");
    let assistant_tool_messages = second
        .iter()
        .filter_map(|message| match message {
            ChatCompletionRequestMessage::Assistant(assistant)
                if assistant
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty()) =>
            {
                Some(assistant)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        assistant_tool_messages.len(),
        1,
        "the assistant turn should stay one chat message"
    );
    let tool_calls = assistant_tool_messages[0]
        .tool_calls
        .as_ref()
        .expect("assistant tool message should contain calls");
    assert_eq!(tool_calls.len(), 2);
}

#[tokio::test]
async fn planning_research_budget_rejects_repeated_read_only_calls() {
    let fixture = TestFixture::new();
    let read_tool = Arc::new(MockTool::new("read", "read result"));
    let mut registry = ToolRegistry::new();
    registry.register(read_tool.clone());

    let mut responses = Vec::new();
    for turn in 0..=PLANNING_RESEARCH_TURN_LIMIT {
        responses.push(Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call(
                &format!("read-{turn}"),
                "read",
                r#"{"title":"inspect"}"#,
            )],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }));
    }
    responses.push(Ok(StreamedResponse {
        content: "done".to_string(),
        tool_calls: vec![],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    }));

    let mut agent = Agent::new(
        Box::new(MockProvider::new(responses)),
        empty_registry(),
        Arc::new(registry),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_mode(AgentMode::Planning);
    agent.budget.max_iterations = PLANNING_RESEARCH_TURN_LIMIT + 3;

    let result = agent
        .run(
            "draft a plan",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert_eq!(
        read_tool.calls.lock().await.len(),
        PLANNING_RESEARCH_TURN_LIMIT,
        "the over-budget read-only planning call should not execute"
    );
    let rejected = tool_message_text(&agent, &format!("read-{PLANNING_RESEARCH_TURN_LIMIT}"));
    assert!(rejected.contains("planning research budget reached"));
    assert!(rejected.contains("plan_replace_draft"));
}

#[tokio::test]
async fn planning_research_budget_stops_when_model_ignores_rejection() {
    let fixture = TestFixture::new();
    let read_tool = Arc::new(MockTool::new("read", "read result"));
    let mut registry = ToolRegistry::new();
    registry.register(read_tool.clone());

    let responses = (0..PLANNING_RESEARCH_TURN_LIMIT + 2)
        .map(|turn| {
            Ok(StreamedResponse {
                tool_calls: vec![test_tool_call(
                    &format!("read-{turn}"),
                    "read",
                    r#"{"title":"inspect"}"#,
                )],
                ..StreamedResponse::default()
            })
        })
        .collect();
    let mut agent = Agent::new(
        Box::new(MockProvider::new(responses)),
        empty_registry(),
        Arc::new(registry),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_mode(AgentMode::Planning);
    agent.budget.max_iterations = PLANNING_RESEARCH_TURN_LIMIT + 2;

    let error = agent
        .run(
            "draft a plan",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("stopped after the planning research limit"),
        "{error:#}"
    );
    assert_eq!(
        read_tool.calls.lock().await.len(),
        PLANNING_RESEARCH_TURN_LIMIT,
        "only in-budget research calls should execute"
    );
    let rejected = tool_message_text(&agent, &format!("read-{PLANNING_RESEARCH_TURN_LIMIT}"));
    assert!(rejected.contains("planning research budget reached"));
}

#[tokio::test]
async fn identical_failed_tool_call_retries_once_then_rejects_and_stops() {
    let fixture = TestFixture::new();
    let bash_tool = Arc::new(FailingBashTool::new());
    let mut registry = ToolRegistry::new();
    registry.register(bash_tool.clone());
    let responses = (0..4)
        .map(|turn| {
            Ok(StreamedResponse {
                tool_calls: vec![test_tool_call(
                    &format!("bash-{turn}"),
                    "bash",
                    r#"{"command":"rustc broken.rs"}"#,
                )],
                ..StreamedResponse::default()
            })
        })
        .collect();
    let mut agent = Agent::new(
        Box::new(MockProvider::new(responses)),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);
    agent.budget.max_iterations = 4;

    let error = agent
        .run(
            "keep running the same broken command",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap_err();

    assert!(
        format!("{error:#}").contains("identical `bash` tool-call failure loop"),
        "{error:#}"
    );
    assert_eq!(
        bash_tool.calls.lock().await.len(),
        2,
        "the third identical failure should be rejected before execution"
    );
    let rejected = tool_message_text(&agent, "bash-2");
    assert!(rejected.contains("already failed 2 times"), "{rejected}");
}

#[tokio::test]
async fn planning_research_budget_warns_before_rejecting() {
    let fixture = TestFixture::new();
    let read_tool = Arc::new(MockTool::new("read", "read result"));
    let plan_tool = Arc::new(MockTool::new("plan_replace_draft", "plan updated"));
    let mut registry = ToolRegistry::new();
    registry.register(read_tool.clone());
    registry.register(plan_tool);

    let mut responses = Vec::new();
    for turn in 0..PLANNING_RESEARCH_TURN_LIMIT {
        responses.push(Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call(
                &format!("read-{turn}"),
                "read",
                r#"{"title":"inspect"}"#,
            )],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }));
    }
    responses.push(Ok(StreamedResponse {
        content: String::new(),
        tool_calls: vec![test_tool_call(
            "plan-title",
            "plan_replace_draft",
            r#"{"title":"Draft plan"}"#,
        )],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    }));
    responses.push(Ok(StreamedResponse {
        content: "done".to_string(),
        tool_calls: vec![],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        usage: None,
        ..StreamedResponse::default()
    }));

    let provider = MockProvider::new(responses);
    let requests = provider.requests();
    let project_context = crate::context::isolated_project_context_snapshot(&fixture.project_root);
    let mut agent = Agent::builder(
        Box::new(provider),
        empty_registry(),
        Arc::new(registry),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .project_context_snapshot(project_context)
    .build()
    .unwrap();
    agent.set_mode(AgentMode::Planning);
    agent.budget.max_iterations = PLANNING_RESEARCH_TURN_LIMIT + 3;

    let result = agent
        .run(
            "draft a plan",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert_eq!(
        read_tool.calls.lock().await.len(),
        PLANNING_RESEARCH_TURN_LIMIT,
        "all in-budget research turns should still execute"
    );

    let requests = requests.lock().await;
    let advisory_request = requests
        .get(PLANNING_RESEARCH_TURN_LIMIT)
        .expect("request after hitting planning budget should be captured");
    let advisory_system = message_content(&advisory_request[0]);
    assert!(
        advisory_system.contains("### Planning decision required"),
        "{advisory_system}"
    );
    assert!(
        advisory_system.contains("ask the user now with the question tool"),
        "{advisory_system}"
    );

    let final_request = requests
        .get(PLANNING_RESEARCH_TURN_LIMIT + 1)
        .expect("request after plan progress should be captured");
    let final_system = message_content(&final_request[0]);
    assert!(
        !final_system.contains("### Planning decision required"),
        "{final_system}"
    );
}

#[tokio::test]
async fn planning_question_at_research_limit_executes_and_refreshes_budget() {
    let fixture = TestFixture::new();
    let read_tool = Arc::new(MockTool::new("read", "read result"));
    let question_tool = Arc::new(MockTool::new("question", "user answered"));
    let mut registry = ToolRegistry::new();
    registry.register(read_tool.clone());
    registry.register(question_tool.clone());

    let mut responses = Vec::new();
    for turn in 0..PLANNING_RESEARCH_TURN_LIMIT {
        responses.push(Ok(StreamedResponse {
            tool_calls: vec![test_tool_call(
                &format!("read-{turn}"),
                "read",
                r#"{"title":"inspect"}"#,
            )],
            ..StreamedResponse::default()
        }));
    }
    responses.push(Ok(StreamedResponse {
        tool_calls: vec![test_tool_call(
            "question-at-limit",
            "question",
            r#"{"title":"Choose behavior"}"#,
        )],
        ..StreamedResponse::default()
    }));
    responses.push(Ok(StreamedResponse {
        tool_calls: vec![test_tool_call(
            "read-after-question",
            "read",
            r#"{"title":"verify answer"}"#,
        )],
        ..StreamedResponse::default()
    }));
    responses.push(Ok(StreamedResponse {
        content: "done".to_string(),
        ..StreamedResponse::default()
    }));

    let mut agent = Agent::new(
        Box::new(MockProvider::new(responses)),
        empty_registry(),
        Arc::new(registry),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_mode(AgentMode::Planning);
    agent.budget.max_iterations = PLANNING_RESEARCH_TURN_LIMIT + 4;

    let result = agent
        .run(
            "draft a plan",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert_eq!(question_tool.calls.lock().await.len(), 1);
    assert_eq!(
        read_tool.calls.lock().await.len(),
        PLANNING_RESEARCH_TURN_LIMIT + 1,
        "question progress should refresh the read-only research budget"
    );
    assert_eq!(
        tool_message_text(&agent, "question-at-limit"),
        "user answered"
    );
    assert_eq!(
        tool_message_text(&agent, "read-after-question"),
        "read result"
    );
}

#[tokio::test]
async fn repeated_inspection_loop_rejects_noop_coding_batch() {
    let fixture = TestFixture::new();
    let project_info_tool = Arc::new(MockTool::new("project_info", "project state"));
    let git_tool = Arc::new(MockTool::new("git", "no diff"));
    let project_info_calls = project_info_tool.calls.clone();
    let git_calls = git_tool.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(project_info_tool);
    registry.register(git_tool);

    let repeated_batch = |turn: usize| {
        vec![
            test_tool_call(
                &format!("project-info-{turn}"),
                "project_info",
                r#"{"title":"state"}"#,
            ),
            test_tool_call(&format!("git-{turn}"), "git", r#"{"title":"diff"}"#),
        ]
    };
    let provider = Box::new(MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: repeated_batch(0),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: repeated_batch(1),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: repeated_batch(2),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]));

    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    let result = agent
        .run(
            "make the change",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert_eq!(
        project_info_calls.lock().await.len(),
        2,
        "the third repeated project_info call should be rejected before execution"
    );
    assert_eq!(
        git_calls.lock().await.len(),
        2,
        "the third repeated git call should be rejected before execution"
    );
    let rejected = tool_message_text(&agent, "git-2");
    assert!(rejected.contains("repeated inspection loop detected"));
    assert!(rejected.contains("take a concrete next step"));
}

#[tokio::test]
async fn repair_advisory_blocks_read_only_refresh_after_failed_command() {
    let fixture = TestFixture::new();
    let bash_tool = Arc::new(FailingBashTool::new());
    let read_tool = Arc::new(MockTool::new("read", "read result"));
    let read_calls = read_tool.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(bash_tool);
    registry.register(read_tool);

    let provider = MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call(
                "bash-1",
                "bash",
                r#"{"command":"cargo test --locked out_of_tree_file_write_targets_prompt_at_balanced"}"#,
            )],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        // First inspection after the failure: legitimate recovery (reading what
        // the failed build points at) — must execute.
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call(
                "read-1",
                "read",
                r#"{"title":"refresh edited test"}"#,
            )],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        // The *same* inspection again while the repair advisory is still
        // active: a loop — rejected without execution.
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call(
                "read-2",
                "read",
                r#"{"title":"refresh edited test"}"#,
            )],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "The compile error remains unresolved.".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]);
    let requests = provider.requests();

    let mut agent = Agent::builder(
        Box::new(provider),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .project_context_snapshot(crate::context::isolated_project_context_snapshot(
        &fixture.project_root,
    ))
    .build()
    .unwrap();

    let result = agent
        .run(
            "fix the compile error",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    let AgentRunResult::Incomplete { failure, .. } = result else {
        panic!("a promised compile-error fix must not succeed without a mutation");
    };
    assert!(
        failure
            .gaps
            .iter()
            .any(|gap| matches!(gap, crate::agent::CompletionGap::WorkspaceMutationMissing))
    );
    assert_eq!(
        read_calls.lock().await.len(),
        1,
        "the first recovery inspection must execute; only the repeat is blocked"
    );
    let rejected = tool_message_text(&agent, "read-2");
    assert!(
        rejected.contains("this exact inspection already ran"),
        "{rejected}"
    );

    let requests = requests.lock().await;
    let second_request_system = message_content(&requests[1][0]);
    assert!(
        second_request_system.contains("### Current repair target"),
        "{second_request_system}"
    );
    assert!(
        second_request_system.contains("fixture.temp_dir.path()"),
        "{second_request_system}"
    );
    assert!(
        second_request_system.contains("Do not refresh unchanged reads first"),
        "{second_request_system}"
    );
}

#[tokio::test]
async fn successful_cargo_test_output_is_kept_for_model_context() {
    let fixture = TestFixture::new();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(SuccessfulBashTool));

    let provider = MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call(
                "bash-1",
                "bash",
                r#"{"command":"cargo test --locked"}"#,
            )],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: Vec::new(),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]);
    let mut agent = Agent::new(
        Box::new(provider),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    let result = agent
        .run("run tests", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    let model_text = tool_message_text(&agent, "bash-1");
    assert!(model_text.starts_with("test output line 1"), "{model_text}");
    assert!(model_text.contains("test output line 2"), "{model_text}");
    assert!(
        !model_text.contains("output compacted for model context"),
        "{model_text}"
    );

    let detail = agent
        .tool_context_details
        .get("bash-1")
        .expect("structured command detail should be stored");
    match &detail.result {
        ToolContextResult::Command { rendered, .. } => {
            assert!(rendered.starts_with("test output line 1"), "{rendered}");
        }
        other => panic!("expected command context result, got {other:?}"),
    }
}

#[tokio::test]
async fn failed_bash_output_stays_detailed_for_model_context() {
    let fixture = TestFixture::new();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FailingBashTool::new()));

    let provider = MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call(
                "bash-1",
                "bash",
                r#"{"command":"cargo test --locked failing_test"}"#,
            )],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: Vec::new(),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "The failing test remains unresolved.".to_string(),
            tool_calls: Vec::new(),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]);
    let mut agent = Agent::new(
        Box::new(provider),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    let result = agent
        .run("run tests", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();

    assert!(matches!(
        result,
        AgentRunResult::Incomplete {
            failure: crate::agent::CompletionGuardFailure {
                outcome: crate::agent::CompletionFailureOutcome::Blocked,
                ..
            },
            ..
        }
    ));
    let model_text = tool_message_text(&agent, "bash-1");
    assert!(
        model_text.starts_with("error[E0609]"),
        "failed command detail should stay visible: {model_text}"
    );
    assert!(!model_text.contains("output compacted"), "{model_text}");
}

#[tokio::test]
async fn repeated_inspection_rejection_allows_an_explicit_retry() {
    let fixture = TestFixture::new();
    let git_tool = Arc::new(MockTool::new("git", "no diff"));
    let git_calls = git_tool.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(git_tool);

    let repeated_call = |turn: usize| {
        vec![test_tool_call(
            &format!("git-{turn}"),
            "git",
            r#"{"title":"diff"}"#,
        )]
    };
    let provider = Box::new(MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: repeated_call(0),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: repeated_call(1),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: repeated_call(2),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: repeated_call(3),
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]));

    let mut agent = Agent::new(
        provider,
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    let result = agent
        .run(
            "make the change",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert_eq!(
        git_calls.lock().await.len(),
        3,
        "the rejected call stays blocked, then one explicit retry executes"
    );
    let rejected = tool_message_text(&agent, "git-2");
    assert!(rejected.contains("repeated inspection loop detected"));
    assert_eq!(tool_message_text(&agent, "git-3"), "no diff");
}

fn tool_message_text(agent: &Agent, call_id: &str) -> String {
    agent
        .messages
        .iter()
        .find_map(|msg| match msg {
            async_openai::types::chat::ChatCompletionRequestMessage::Tool(t)
                if t.tool_call_id == call_id =>
            {
                match &t.content {
                    async_openai::types::chat::ChatCompletionRequestToolMessageContent::Text(
                        text,
                    ) => Some(text.clone()),
                    _ => None,
                }
            }
            _ => None,
        })
        .expect("failed tool call should produce a tool message for the model")
}
