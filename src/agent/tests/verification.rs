use super::*;
use crate::provider::ReasoningSelection;
use crate::verification::{
    VerificationCheck, VerificationCheckStatus, VerificationKind, VerificationRunStatus,
    VerifyAfterEdit,
};
use std::collections::VecDeque;
use std::path::PathBuf;

fn verification_agent(fixture: &TestFixture) -> Agent {
    verification_agent_with_policy(fixture, VerifyAfterEdit::Ask)
}

fn verification_agent_with_policy(fixture: &TestFixture, after_edit: VerifyAfterEdit) -> Agent {
    let mut config = crate::config::Config::empty();
    config.verification.after_edit = after_edit;
    Agent::builder(
        MockProvider::empty(),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .system_context(String::new())
    .config(Arc::new(config))
    .build()
    .unwrap()
}

fn command_result(exit_code: i32) -> ToolOutput {
    ToolOutput::Command {
        rendered: format!("exit code: {exit_code}"),
        stdout: String::new(),
        stderr: String::new(),
        exit_code: Some(exit_code),
        timed_out: false,
        truncation: None,
    }
}

fn command_failure(diagnostic: &str) -> ToolOutput {
    ToolOutput::Command {
        rendered: diagnostic.to_string(),
        stdout: String::new(),
        stderr: diagnostic.to_string(),
        exit_code: Some(1),
        timed_out: false,
        truncation: None,
    }
}

#[tokio::test]
async fn foreground_profile_checks_feed_self_review_and_verification_evidence() {
    let fixture = TestFixture::new();
    std::fs::write(
        fixture.project_root.join("Cargo.toml"),
        "[package]\nname='review-evidence'\nversion='0.1.0'\n",
    )
    .unwrap();
    let mut agent = verification_agent(&fixture);
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::On);
    agent.arm_self_review_for_coding_task("change code").await;

    agent
        .record_verification_tool_result(
            &crate::provider::ToolCall {
                id: "check-1".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"cargo test"}"#.to_string(),
            },
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;
    agent
        .record_verification_tool_result(
            &crate::provider::ToolCall {
                id: "not-a-check".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"git status --short"}"#.to_string(),
            },
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;

    assert_eq!(
        agent.self_review.checks_run(),
        &[("cargo test".to_string(), true)]
    );
    assert_eq!(agent.verification_runs().len(), 1);
    let run = &agent.verification_runs()[0];
    assert_eq!(run.status, VerificationRunStatus::Passed);
    assert_eq!(run.kind, VerificationKind::Test);
    assert_eq!(run.checks[0].tool_call_id.as_deref(), Some("check-1"));
    assert_eq!(run.observed_final_workspace, Some(true));

    agent.note_typed_verification_worthy_mutation(vec!["src/lib.rs".to_string()]);
    let run = &agent.verification_runs()[0];
    assert_eq!(run.status, VerificationRunStatus::Stale);
    assert_eq!(run.observed_final_workspace, Some(false));
    assert_eq!(
        run.workspace_changes_after_last_check,
        ["src/lib.rs".to_string()]
    );
}

fn model_tool_response(
    id: &str,
    name: &str,
    arguments: &str,
) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        tool_calls: vec![test_tool_call(id, name, arguments)],
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        ..StreamedResponse::default()
    })
}

fn model_finished_response(content: &str) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        content: content.to_string(),
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        ..StreamedResponse::default()
    })
}

struct VerificationBashTool;

#[async_trait]
impl Tool for VerificationBashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "verification bash stub"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["command"],
            "properties": {"command": {"type": "string"}},
            "additionalProperties": false
        })
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(command_result(0))
    }
}

struct ScriptedVerificationBashTool {
    results: StdMutex<VecDeque<ToolOutput>>,
}

struct RecoveryReasoningProvider {
    responses: Mutex<VecDeque<crate::provider::ProviderResult<StreamedResponse>>>,
    options: Arc<Mutex<Vec<crate::provider::ProviderRequestOptions>>>,
}

impl RecoveryReasoningProvider {
    fn new(responses: Vec<crate::provider::ProviderResult<StreamedResponse>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            options: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn options(&self) -> Arc<Mutex<Vec<crate::provider::ProviderRequestOptions>>> {
        self.options.clone()
    }
}

#[async_trait]
impl Provider for RecoveryReasoningProvider {
    fn reasoning(&self) -> ReasoningSelection {
        ReasoningSelection::Medium
    }

    fn reasoning_escalation(&self) -> Option<ReasoningSelection> {
        Some(ReasoningSelection::High)
    }

    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.chat_stream_with_options(
            messages,
            tools,
            crate::provider::ProviderRequestOptions::default(),
            cancellation_token,
            sink,
        )
        .await
    }

    async fn chat_stream_with_options(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        options: crate::provider::ProviderRequestOptions,
        _cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.options.lock().await.push(options);
        self.responses.lock().await.pop_front().ok_or_else(|| {
            crate::provider::ProviderFailure::configuration(
                "recovery provider response script exhausted",
            )
        })?
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

struct RepairingWriteTool {
    project_root: PathBuf,
    repair_count: StdMutex<u32>,
}

#[async_trait]
impl Tool for RepairingWriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "scripted recovery mutation"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let repair = {
            let mut count = self
                .repair_count
                .lock()
                .map_err(|_| anyhow::anyhow!("repair count lock poisoned"))?;
            *count = count.saturating_add(1);
            *count
        };
        let path = self.project_root.join("src.rs");
        let old = tokio::fs::read_to_string(&path).await?;
        let new = format!("repair {repair}\n");
        tokio::fs::write(path, &new).await?;
        Ok(ToolOutput::Edit {
            summary: "applied focused repair".to_string(),
            diff: crate::diff::build_file_diff("src.rs".to_string(), Some(&old), &new),
        })
    }
}

#[async_trait]
impl Tool for ScriptedVerificationBashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "scripted verification bash stub"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["command"],
            "properties": {"command": {"type": "string"}},
            "additionalProperties": false
        })
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.results
            .lock()
            .map_err(|_| anyhow::anyhow!("scripted verification result lock poisoned"))?
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("scripted verification result exhausted"))
    }
}

fn mutation_and_verification_registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(MockTool::new("write", "ok")));
    registry.register(Arc::new(VerificationBashTool));
    Arc::new(registry)
}

fn scripted_verification_registry(results: Vec<ToolOutput>) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ScriptedVerificationBashTool {
        results: StdMutex::new(results.into()),
    }));
    Arc::new(registry)
}

fn recovery_verification_registry(
    project_root: PathBuf,
    results: Vec<ToolOutput>,
) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ScriptedVerificationBashTool {
        results: StdMutex::new(results.into()),
    }));
    registry.register(Arc::new(RepairingWriteTool {
        project_root,
        repair_count: StdMutex::new(0),
    }));
    Arc::new(registry)
}

#[tokio::test]
async fn verification_records_exact_bash_result_and_final_workspace_freshness() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "src.rs",
        "fn baseline() {}\n",
        "baseline",
    );
    let mut agent = verification_agent(&fixture);
    let checks = vec![VerificationCheck {
        name: "Rust tests".to_string(),
        command: "cargo test --locked".to_string(),
    }];
    agent
        .begin_verification_run(VerificationKind::Test, &checks, "verify")
        .await;
    agent
        .record_verification_tool_result(
            &crate::provider::ToolCall {
                id: "call-test".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"cargo test --locked"}"#.to_string(),
            },
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;
    let outcome: anyhow::Result<AgentRunResult> =
        Ok(AgentRunResult::Completed("passed".to_string()));
    agent.finish_verification_run(&outcome).await;

    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Passed);
    assert_eq!(run.observed_final_workspace, Some(true));
    assert_eq!(run.checks[0].status, VerificationCheckStatus::Passed);
    assert_eq!(run.checks[0].tool_call_id.as_deref(), Some("call-test"));
}

#[tokio::test]
async fn verification_marks_a_passed_check_stale_after_a_later_workspace_change() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "src.rs",
        "fn baseline() {}\n",
        "baseline",
    );
    let mut agent = verification_agent(&fixture);
    let checks = vec![VerificationCheck {
        name: "Rust build".to_string(),
        command: "cargo build --locked".to_string(),
    }];
    agent
        .begin_verification_run(VerificationKind::Build, &checks, "verify")
        .await;
    agent
        .record_verification_tool_result(
            &crate::provider::ToolCall {
                id: "call-build".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"cargo build --locked"}"#.to_string(),
            },
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;
    std::fs::write(fixture.project_root.join("src.rs"), "fn changed() {}\n").unwrap();
    let outcome: anyhow::Result<AgentRunResult> =
        Ok(AgentRunResult::Completed("passed".to_string()));
    agent.finish_verification_run(&outcome).await;

    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Stale);
    assert_eq!(run.observed_final_workspace, Some(false));
    assert_eq!(run.workspace_changes_after_last_check, ["src.rs"]);
}

#[tokio::test]
async fn verification_failure_wins_over_an_incomplete_agent_summary() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    let mut agent = verification_agent(&fixture);
    let checks = vec![VerificationCheck {
        name: "Go tests".to_string(),
        command: "go test ./...".to_string(),
    }];
    agent
        .begin_verification_run(VerificationKind::Test, &checks, "verify")
        .await;
    agent
        .record_verification_tool_result(
            &crate::provider::ToolCall {
                id: "call-go".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"go test ./..."}"#.to_string(),
            },
            &command_result(1),
            crate::output::ToolExecutionStatus::Failed,
        )
        .await;
    let outcome: anyhow::Result<AgentRunResult> = Ok(AgentRunResult::Completed("done".to_string()));
    agent.finish_verification_run(&outcome).await;

    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Failed);
    assert_eq!(run.checks[0].status, VerificationCheckStatus::Failed);
    assert_eq!(run.checks[0].exit_code, Some(1));
}

#[tokio::test]
async fn repeated_deterministic_verification_failure_stops_the_run() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    let provider = MockProvider::new(vec![
        model_tool_response("call-test-1", "bash", r#"{"command":"cargo test"}"#),
        model_tool_response("call-test-2", "bash", r#"{"command":"cargo test"}"#),
        model_finished_response("should not be reached"),
    ]);
    let requests = provider.requests();
    let registry = scripted_verification_registry(vec![
        command_failure("error: expected alpha"),
        command_failure("error: expected alpha"),
    ]);
    let mut agent = Agent::builder(
        Box::new(provider),
        registry,
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .system_context(String::new())
    .build()
    .unwrap();
    let checks = vec![VerificationCheck {
        name: "Rust tests".to_string(),
        command: "cargo test".to_string(),
    }];
    agent
        .begin_verification_run(VerificationKind::Test, &checks, "verify")
        .await;

    let error = agent
        .run_current_context(CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("same deterministic failure"));
    assert_eq!(requests.lock().await.len(), 2);
    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Blocked);
    assert_eq!(run.repair_attempts, 1);
    assert_eq!(run.checks[0].attempt_count, 2);
    assert!(
        run.terminal_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("same deterministic failure"))
    );
}

#[tokio::test]
async fn changed_no_workspace_failure_gets_one_flaky_rerun() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    let mut agent = verification_agent(&fixture);
    let checks = vec![VerificationCheck {
        name: "Rust tests".to_string(),
        command: "cargo test".to_string(),
    }];
    agent
        .begin_verification_run(VerificationKind::Test, &checks, "verify")
        .await;

    for (id, result, status) in [
        (
            "call-1",
            command_failure("error: worker alpha crashed"),
            crate::output::ToolExecutionStatus::Failed,
        ),
        (
            "call-2",
            command_failure("error: worker beta crashed"),
            crate::output::ToolExecutionStatus::Failed,
        ),
        (
            "call-3",
            command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        ),
    ] {
        agent
            .record_verification_tool_result(
                &crate::provider::ToolCall {
                    id: id.to_string(),
                    name: "bash".to_string(),
                    arguments: r#"{"command":"cargo test"}"#.to_string(),
                },
                &result,
                status,
            )
            .await;
    }
    let outcome: anyhow::Result<AgentRunResult> = Ok(AgentRunResult::Completed("done".to_string()));
    agent.finish_verification_run(&outcome).await;

    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Unstable);
    assert_eq!(run.repair_attempts, 1);
    assert_eq!(run.checks[0].attempt_count, 3);
    let context = agent
        .messages
        .iter()
        .map(message_content)
        .collect::<String>();
    assert!(context.contains("recoverable_failure"));
    assert!(context.contains("suspected_flaky"));
    assert!(context.contains("unstable_pass"));
}

#[tokio::test]
async fn changed_workspace_allows_two_focused_repairs_before_a_pass() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(&fixture.project_root, "src.rs", "zero\n", "baseline");
    let mut agent = verification_agent(&fixture);
    let checks = vec![VerificationCheck {
        name: "Rust tests".to_string(),
        command: "cargo test".to_string(),
    }];
    agent
        .begin_verification_run(VerificationKind::Test, &checks, "verify")
        .await;
    let call = |id: &str| crate::provider::ToolCall {
        id: id.to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
    };

    agent
        .record_verification_tool_result(
            &call("call-1"),
            &command_failure("error: first root cause"),
            crate::output::ToolExecutionStatus::Failed,
        )
        .await;
    std::fs::write(fixture.project_root.join("src.rs"), "one\n").unwrap();
    agent
        .record_verification_tool_result(
            &call("call-2"),
            &command_failure("error: second root cause"),
            crate::output::ToolExecutionStatus::Failed,
        )
        .await;
    std::fs::write(fixture.project_root.join("src.rs"), "two\n").unwrap();
    agent
        .record_verification_tool_result(
            &call("call-3"),
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;
    let outcome: anyhow::Result<AgentRunResult> = Ok(AgentRunResult::Completed("done".to_string()));
    agent.finish_verification_run(&outcome).await;

    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Passed);
    assert_eq!(run.repair_attempts, 2);
    assert_eq!(run.checks[0].attempt_count, 3);
}

#[tokio::test]
async fn failed_repair_escalates_reasoning_then_verifies_the_final_workspace() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(&fixture.project_root, "src.rs", "zero\n", "baseline");
    let provider = RecoveryReasoningProvider::new(vec![
        model_tool_response("call-test-1", "bash", r#"{"command":"cargo test"}"#),
        model_tool_response("call-repair-1", "write", "{}"),
        model_tool_response("call-test-2", "bash", r#"{"command":"cargo test"}"#),
        model_tool_response("call-repair-2", "write", "{}"),
        model_tool_response("call-test-3", "bash", r#"{"command":"cargo test"}"#),
        model_finished_response("verified after bounded recovery"),
    ]);
    let options = provider.options();
    let registry = recovery_verification_registry(
        fixture.project_root.clone(),
        vec![
            command_failure("error: first root cause"),
            command_failure("error: second root cause"),
            command_result(0),
        ],
    );
    let mut agent = Agent::builder(
        Box::new(provider),
        registry,
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .system_context(String::new())
    .build()
    .unwrap();
    let checks = vec![VerificationCheck {
        name: "Rust tests".to_string(),
        command: "cargo test".to_string(),
    }];
    agent
        .begin_verification_run(VerificationKind::Test, &checks, "verify")
        .await;

    let result = agent
        .run_current_context(CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();

    assert_eq!(
        result,
        AgentRunResult::Completed("verified after bounded recovery".to_string())
    );
    assert_eq!(
        tokio::fs::read_to_string(fixture.project_root.join("src.rs"))
            .await
            .unwrap(),
        "repair 2\n"
    );
    let options = options.lock().await;
    assert_eq!(options.len(), 6);
    assert!(
        options[..3]
            .iter()
            .all(|options| options.reasoning.is_none())
    );
    assert!(
        options[3..]
            .iter()
            .all(|options| { options.reasoning == Some(ReasoningSelection::High) })
    );
    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Passed);
    assert_eq!(run.repair_attempts, 2);
    assert_eq!(run.observed_final_workspace, Some(true));
    assert_eq!(run.reasoning_escalations.len(), 1);
    assert_eq!(
        run.reasoning_escalations[0].from,
        ReasoningSelection::Medium
    );
    assert_eq!(run.reasoning_escalations[0].to, ReasoningSelection::High);
    assert_eq!(run.reasoning_escalations[0].repair_attempt, 2);
}

#[tokio::test]
async fn third_failed_verification_after_workspace_repairs_exhausts_budget() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(&fixture.project_root, "src.rs", "zero\n", "baseline");
    let mut agent = verification_agent(&fixture);
    let checks = vec![VerificationCheck {
        name: "Rust tests".to_string(),
        command: "cargo test".to_string(),
    }];
    agent
        .begin_verification_run(VerificationKind::Test, &checks, "verify")
        .await;

    for (index, diagnostic) in ["first root", "second root", "third root"]
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            std::fs::write(
                fixture.project_root.join("src.rs"),
                format!("repair {index}\n"),
            )
            .unwrap();
        }
        agent
            .record_verification_tool_result(
                &crate::provider::ToolCall {
                    id: format!("call-{index}"),
                    name: "bash".to_string(),
                    arguments: r#"{"command":"cargo test"}"#.to_string(),
                },
                &command_failure(diagnostic),
                crate::output::ToolExecutionStatus::Failed,
            )
            .await;
    }
    let outcome: anyhow::Result<AgentRunResult> = Err(anyhow::anyhow!("verification blocked"));
    agent.finish_verification_run(&outcome).await;

    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Blocked);
    assert_eq!(run.repair_attempts, 2);
    assert_eq!(run.checks[0].attempt_count, 3);
    assert!(
        run.terminal_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("2 focused repair attempts"))
    );
}

#[tokio::test]
async fn post_edit_on_injects_one_test_workflow_and_does_not_recurse() {
    let fixture = TestFixture::new();
    std::fs::write(
        fixture.project_root.join("Cargo.toml"),
        "[package]\nname='verify'\nversion='0.1.0'\n",
    )
    .unwrap();
    let mut agent = verification_agent_with_policy(&fixture, VerifyAfterEdit::On);
    agent.reset_after_edit_verification();
    agent.note_typed_verification_worthy_mutation(vec!["src.rs".to_string()]);
    let sink: SharedSink = Arc::new(StdoutSink);

    assert!(
        agent
            .maybe_verify_after_edit(&sink, CancellationToken::new())
            .await
    );
    assert_eq!(agent.verification_runs().len(), 1);
    assert_eq!(agent.verification_runs()[0].kind, VerificationKind::Test);
    assert_eq!(agent.verification_runs()[0].checks[0].command, "cargo test");
    assert!(
        !agent
            .maybe_verify_after_edit(&sink, CancellationToken::new())
            .await
    );
}

#[tokio::test]
async fn post_edit_ask_skips_without_an_interactive_surface() {
    let fixture = TestFixture::new();
    std::fs::write(
        fixture.project_root.join("Cargo.toml"),
        "[package]\nname='verify'\nversion='0.1.0'\n",
    )
    .unwrap();
    let mut agent = verification_agent_with_policy(&fixture, VerifyAfterEdit::Ask);
    agent.reset_after_edit_verification();
    agent.note_typed_verification_worthy_mutation(vec!["src.rs".to_string()]);
    let sink: SharedSink = Arc::new(StdoutSink);

    assert!(
        !agent
            .maybe_verify_after_edit(&sink, CancellationToken::new())
            .await
    );
    assert!(agent.verification_runs().is_empty());
}

#[tokio::test]
async fn coding_run_executes_post_edit_verification_before_completing() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "Cargo.toml",
        "[package]\nname='verify'\nversion='0.1.0'\n",
        "baseline",
    );
    let provider = MockProvider::new(vec![
        model_tool_response("call-write", "write", r#"{"title":"record a mutation"}"#),
        model_finished_response("coding done"),
        model_tool_response("call-test", "bash", r#"{"command":"cargo test"}"#),
        model_finished_response("verified"),
    ]);
    let requests = provider.requests();
    let mut config = crate::config::Config::empty();
    config.verification.after_edit = VerifyAfterEdit::On;
    let mut agent = Agent::builder(
        Box::new(provider),
        mutation_and_verification_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .system_context(String::new())
    .config(Arc::new(config))
    .build()
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);

    let result = agent
        .run(
            "make a change",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("verified".to_string()));
    assert_eq!(requests.lock().await.len(), 4);
    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Passed);
    assert_eq!(run.checks[0].tool_call_id.as_deref(), Some("call-test"));
}
