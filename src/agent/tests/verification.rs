use super::*;
use crate::agent::{BackgroundVerificationCapture, capture_verification_workspace_binding};
use crate::provider::ReasoningSelection;
use crate::verification::{
    VerificationBinding, VerificationCheck, VerificationCheckStatus, VerificationKind,
    VerificationRunStatus, VerificationTerminalReason, VerifyAfterEdit,
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

#[tokio::test]
async fn manual_and_automatic_checks_populate_the_same_evidence_fields() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "Cargo.toml",
        "[workspace]\n",
        "baseline",
    );
    let call = crate::provider::ToolCall {
        id: "check".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test --locked"}"#.to_string(),
    };
    let mut manual = verification_agent(&fixture);
    manual
        .record_verification_tool_result(
            &call,
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;
    manual.revalidate_verification_for_delivery(0).await;

    let mut automatic = verification_agent(&fixture);
    automatic
        .begin_verification_run(
            VerificationKind::Test,
            &[VerificationCheck {
                name: "Rust tests".to_string(),
                command: "cargo test --locked".to_string(),
            }],
            "verify",
        )
        .await;
    automatic
        .record_verification_tool_result(
            &call,
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;
    let outcome: anyhow::Result<AgentRunResult> =
        Ok(AgentRunResult::Completed("verified".to_string()));
    automatic.finish_verification_run(&outcome).await;

    for check in [
        &manual.verification_runs()[0].checks[0],
        &automatic.verification_runs()[0].checks[0],
    ] {
        assert_eq!(check.status, VerificationCheckStatus::Passed);
        assert_eq!(check.attempt_count, 1);
        assert_eq!(check.attempt_timestamps_ms.len(), 1);
        assert!(matches!(
            check.binding.as_ref(),
            Some(VerificationBinding::Bound { .. })
        ));
        assert!(matches!(
            check.delivered_binding.as_ref(),
            Some(VerificationBinding::Bound { .. })
        ));
        assert!(check.terminal_reason_kind.is_none());
    }
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
    reasoning: ReasoningSelection,
    reasoning_escalation: Option<ReasoningSelection>,
}

impl RecoveryReasoningProvider {
    fn new(responses: Vec<crate::provider::ProviderResult<StreamedResponse>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            options: Arc::new(Mutex::new(Vec::new())),
            reasoning: ReasoningSelection::Medium,
            reasoning_escalation: Some(ReasoningSelection::High),
        }
    }

    fn with_reasoning(
        responses: Vec<crate::provider::ProviderResult<StreamedResponse>>,
        reasoning: ReasoningSelection,
    ) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            options: Arc::new(Mutex::new(Vec::new())),
            reasoning,
            reasoning_escalation: None,
        }
    }

    fn options(&self) -> Arc<Mutex<Vec<crate::provider::ProviderRequestOptions>>> {
        self.options.clone()
    }
}

#[async_trait]
impl Provider for RecoveryReasoningProvider {
    fn reasoning(&self) -> ReasoningSelection {
        self.reasoning
    }

    fn reasoning_escalation(&self) -> Option<ReasoningSelection> {
        self.reasoning_escalation
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

struct ReviewableWriteTool {
    project_root: PathBuf,
    writes: StdMutex<u32>,
}

#[async_trait]
impl Tool for ReviewableWriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "scripted reviewable mutation"
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
        let write = {
            let mut writes = self
                .writes
                .lock()
                .map_err(|_| anyhow::anyhow!("reviewable write count lock poisoned"))?;
            *writes = writes.saturating_add(1);
            *writes
        };
        let path = self.project_root.join("src.rs");
        let old = tokio::fs::read_to_string(&path).await?;
        let mut new = (0..20)
            .map(|index| format!("fn changed_{index}() {{}}\n"))
            .collect::<String>();
        if write > 1 {
            new.push_str("fn repaired_after_review() {}\n");
        }
        tokio::fs::write(&path, &new).await?;
        Ok(ToolOutput::Edit {
            summary: "wrote reviewable change".to_string(),
            diff: crate::diff::build_file_diff("src.rs".to_string(), Some(&old), &new),
        })
    }
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

fn review_and_verification_registry(project_root: PathBuf) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(ReviewableWriteTool {
        project_root,
        writes: StdMutex::new(0),
    }));
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
async fn active_mutation_reopens_earlier_passes_for_the_final_gate() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    let mut agent = verification_agent(&fixture);
    let checks = vec![
        VerificationCheck {
            name: "Compile".to_string(),
            command: "cargo check".to_string(),
        },
        VerificationCheck {
            name: "Tests".to_string(),
            command: "cargo test".to_string(),
        },
    ];
    agent
        .begin_verification_run(VerificationKind::Test, &checks, "verify")
        .await;
    let call = |id: &str, command: &str| crate::provider::ToolCall {
        id: id.to_string(),
        name: "bash".to_string(),
        arguments: serde_json::json!({"command": command}).to_string(),
    };

    agent
        .record_verification_tool_result(
            &call("compile-before-repair", "cargo check"),
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;
    std::fs::write(fixture.project_root.join("src.rs"), "repair\n").unwrap();
    agent.note_typed_verification_worthy_mutation(vec!["src.rs".to_string()]);
    assert_eq!(
        agent.verification_runs().last().unwrap().checks[0].status,
        VerificationCheckStatus::Pending
    );

    for (id, command) in [
        ("tests-after-repair", "cargo test"),
        ("compile-final", "cargo check"),
    ] {
        agent
            .record_verification_tool_result(
                &call(id, command),
                &command_result(0),
                crate::output::ToolExecutionStatus::Succeeded,
            )
            .await;
    }
    let outcome: anyhow::Result<AgentRunResult> =
        Ok(AgentRunResult::Completed("verified".to_string()));
    agent.finish_verification_run(&outcome).await;

    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Passed);
    assert_eq!(run.checks[0].attempt_count, 2);
    assert_eq!(run.checks[1].attempt_count, 1);
    assert_eq!(run.observed_final_workspace, Some(true));
    assert!(!agent.verification.after_edit_verification_pending);
}

#[tokio::test]
async fn delivery_revalidation_ignores_unrelated_untracked_notes_but_not_config() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "Cargo.toml",
        "[workspace]\n",
        "baseline",
    );
    let mut agent = verification_agent(&fixture);
    let call = crate::provider::ToolCall {
        id: "call-test".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test --locked"}"#.to_string(),
    };
    agent
        .record_verification_tool_result(
            &call,
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;

    std::fs::write(fixture.project_root.join("notes.md"), "unrelated").unwrap();
    agent.revalidate_verification_for_delivery(0).await;
    assert_eq!(
        agent.verification_runs()[0].status,
        VerificationRunStatus::Passed
    );

    std::fs::create_dir(fixture.project_root.join(".cargo")).unwrap();
    std::fs::write(
        fixture.project_root.join(".cargo/config.toml"),
        "[build]\nrustflags = []\n",
    )
    .unwrap();
    agent.revalidate_verification_for_delivery(0).await;
    assert_eq!(
        agent.verification_runs()[0].status,
        VerificationRunStatus::Stale
    );
}

#[tokio::test]
async fn delivery_revalidation_binds_failed_checks_without_turning_them_into_passes() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "Cargo.toml",
        "[workspace]\n",
        "baseline",
    );
    let mut agent = verification_agent(&fixture);
    agent
        .record_verification_tool_result(
            &crate::provider::ToolCall {
                id: "failed-check".to_string(),
                name: "bash".to_string(),
                arguments: r#"{"command":"cargo test --locked"}"#.to_string(),
            },
            &command_failure("error: broken test"),
            crate::output::ToolExecutionStatus::Failed,
        )
        .await;

    agent.revalidate_verification_for_delivery(0).await;
    let run = &agent.verification_runs()[0];
    assert_eq!(run.status, VerificationRunStatus::Failed);
    assert_eq!(run.observed_final_workspace, Some(true));
    assert!(run.checks[0].delivered_binding.is_some());

    std::fs::write(
        fixture.project_root.join("Cargo.toml"),
        "[workspace]\nmembers = []\n",
    )
    .unwrap();
    agent.revalidate_verification_for_delivery(0).await;
    let run = &agent.verification_runs()[0];
    assert_eq!(run.status, VerificationRunStatus::Failed);
    assert_eq!(run.observed_final_workspace, Some(false));
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
async fn manual_retries_share_history_and_confirmed_failure_is_suppressed() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "Cargo.toml",
        "[workspace]\n",
        "baseline",
    );
    let mut agent = verification_agent(&fixture);
    let call = |id: &str| crate::provider::ToolCall {
        id: id.to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test --locked"}"#.to_string(),
    };

    for id in ["attempt-1", "attempt-2"] {
        agent
            .record_verification_tool_result(
                &call(id),
                &command_failure("error: stable root cause"),
                crate::output::ToolExecutionStatus::Failed,
            )
            .await;
    }

    let run = &agent.verification_runs()[0];
    assert_eq!(run.checks[0].attempt_count, 2);
    assert_eq!(run.checks[0].attempt_timestamps_ms.len(), 2);
    assert_eq!(run.checks[0].failure_signatures.len(), 2);
    assert_eq!(run.status, VerificationRunStatus::Blocked);
    assert_eq!(
        run.terminal_reason_kind,
        Some(VerificationTerminalReason::RepeatedDeterministicFailure)
    );

    let suppressed = call("attempt-3");
    let rejections = agent
        .capture_pending_verification_bindings(std::slice::from_ref(&suppressed))
        .await;
    assert!(rejections.contains_key("attempt-3"));
    agent
        .record_verification_tool_result(
            &suppressed,
            &ToolOutput::Text("guarded".to_string()),
            crate::output::ToolExecutionStatus::Skipped,
        )
        .await;

    assert_eq!(
        agent.verification_runs()[0].checks[0].attempt_count,
        2,
        "a rejected request is not an executed attempt"
    );
    let rejected = agent.verification_runs().last().unwrap();
    assert_eq!(rejected.status, VerificationRunStatus::Blocked);
    assert_eq!(rejected.checks[0].attempt_count, 0);
}

#[tokio::test]
async fn manual_checks_in_separate_user_runs_keep_separate_delivery_evidence() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "Cargo.toml",
        "[workspace]\n",
        "baseline",
    );
    let mut agent = verification_agent(&fixture);
    let call = |id: &str| crate::provider::ToolCall {
        id: id.to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test --locked"}"#.to_string(),
    };

    agent
        .record_verification_tool_result(
            &call("first-run"),
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;
    agent.begin_verification_observation_window();
    agent
        .record_verification_tool_result(
            &call("second-run"),
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;

    assert_eq!(agent.verification_runs().len(), 2);
    assert_eq!(agent.verification_runs()[0].checks[0].attempt_count, 1);
    assert_eq!(agent.verification_runs()[1].checks[0].attempt_count, 1);
    assert_eq!(
        agent.verification_runs()[1].checks[0]
            .tool_call_id
            .as_deref(),
        Some("second-run")
    );
}

#[tokio::test]
async fn pre_execution_binding_uses_the_persistent_bash_working_directory() {
    let fixture = TestFixture::new();
    let subdir = fixture.project_root.join("subdir");
    tokio::fs::create_dir(&subdir).await.unwrap();
    let expected = subdir.canonicalize().unwrap();
    let bash = Arc::new(crate::tool::BashTool::new(
        fixture.project_root.clone(),
        fixture.permissions.clone(),
        fixture.read_tracker.clone(),
        fixture.interaction.clone(),
    ));
    bash.execute(serde_json::json!({"command": "cd subdir"}))
        .await
        .unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(bash);
    let mut config = crate::config::Config::empty();
    config.verification.test = Some(vec!["cargo test".to_string()]);
    let mut agent = Agent::builder(
        MockProvider::empty(),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .system_context(String::new())
    .config(Arc::new(config))
    .build()
    .unwrap();
    let call = crate::provider::ToolCall {
        id: "persistent-cwd".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
    };

    agent
        .capture_pending_verification_bindings(std::slice::from_ref(&call))
        .await;

    let binding = agent
        .verification
        .pending_verification_bindings
        .get(&call.id)
        .unwrap();
    let VerificationBinding::Bound { identity, .. } = binding else {
        panic!("persistent cwd should produce a bound identity");
    };
    assert_eq!(identity.command_cwd, expected.to_string_lossy());
}

#[tokio::test]
async fn denied_verification_is_user_skipped_not_a_deterministic_failure() {
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
    let call = crate::provider::ToolCall {
        id: "denied".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
    };

    agent
        .record_verification_tool_result(
            &call,
            &ToolOutput::Text(
                "Error: Permission denied by user for command: cargo test".to_string(),
            ),
            crate::output::ToolExecutionStatus::Failed,
        )
        .await;

    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Incomplete);
    assert_eq!(
        run.terminal_reason_kind,
        Some(VerificationTerminalReason::UserSkipped)
    );
    assert_eq!(run.checks[0].attempt_count, 0);
}

#[tokio::test]
async fn interactive_verification_is_typed_as_delegated() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    let mut agent = verification_agent(&fixture);
    agent
        .begin_verification_run(
            VerificationKind::Test,
            &[VerificationCheck {
                name: "Interactive tests".to_string(),
                command: "cargo test".to_string(),
            }],
            "verify",
        )
        .await;
    let call = crate::provider::ToolCall {
        id: "interactive".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test","interactive":true}"#.to_string(),
    };

    agent
        .record_verification_tool_result(
            &call,
            &ToolOutput::BackgroundTaskStarted {
                task_id: "terminal-1".to_string(),
                message: "interactive terminal started".to_string(),
            },
            crate::output::ToolExecutionStatus::Started,
        )
        .await;

    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Incomplete);
    assert_eq!(
        run.terminal_reason_kind,
        Some(VerificationTerminalReason::Delegated)
    );
    assert_eq!(run.checks[0].attempt_count, 0);
}

#[tokio::test]
async fn completed_background_check_produces_observed_verification_evidence() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "Cargo.toml",
        "[workspace]\n",
        "baseline",
    );
    let mut agent = verification_agent(&fixture);
    let binding = capture_verification_workspace_binding(
        &fixture.project_root,
        &fixture.project_root,
        "cargo test --locked",
    )
    .await;
    assert!(matches!(binding, VerificationBinding::Bound { .. }));
    agent.verification.background_verification_bindings.insert(
        "bg-1".to_string(),
        BackgroundVerificationCapture {
            binding,
            record_index: None,
            command: "cargo test --locked".to_string(),
        },
    );
    let now = std::time::SystemTime::now();
    let task = crate::background::BackgroundTaskSnapshot {
        id: "bg-1".to_string(),
        incarnation: "incarnation-1".to_string(),
        command: "cargo test --locked".to_string(),
        cwd: fixture.project_root.clone(),
        status: crate::background::BackgroundTaskStatus::Succeeded,
        started_at: now,
        finished_at: Some(now),
        exit_code: Some(0),
        timeout_secs: 30,
        timed_out: false,
        tail: "tests passed".to_string(),
        tail_truncated: false,
        total_output_chars: 12,
        version: 2,
        tool_call_id: Some("background-call".to_string()),
    };

    agent.record_background_verification_results(&[task]).await;

    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Passed);
    assert_eq!(run.checks[0].attempt_count, 1);
    assert_eq!(
        run.checks[0].tool_call_id.as_deref(),
        Some("background-call")
    );
}

#[tokio::test]
async fn older_background_check_does_not_consume_a_new_active_workflow() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "Cargo.toml",
        "[workspace]\n",
        "baseline",
    );
    let mut agent = verification_agent(&fixture);
    let binding = capture_verification_workspace_binding(
        &fixture.project_root,
        &fixture.project_root,
        "cargo test --locked",
    )
    .await;
    agent.verification.background_verification_bindings.insert(
        "older-bg".to_string(),
        BackgroundVerificationCapture {
            binding,
            record_index: None,
            command: "cargo test --locked".to_string(),
        },
    );
    agent
        .begin_verification_run(
            VerificationKind::Test,
            &[VerificationCheck {
                name: "Current tests".to_string(),
                command: "cargo test --locked".to_string(),
            }],
            "verify current work",
        )
        .await;
    let now = std::time::SystemTime::now();
    let task = crate::background::BackgroundTaskSnapshot {
        id: "older-bg".to_string(),
        incarnation: "older-incarnation".to_string(),
        command: "cargo test --locked".to_string(),
        cwd: fixture.project_root.clone(),
        status: crate::background::BackgroundTaskStatus::Succeeded,
        started_at: now,
        finished_at: Some(now),
        exit_code: Some(0),
        timeout_secs: 30,
        timed_out: false,
        tail: "older tests passed".to_string(),
        tail_truncated: false,
        total_output_chars: 18,
        version: 2,
        tool_call_id: Some("older-call".to_string()),
    };

    agent.record_background_verification_results(&[task]).await;

    assert_eq!(agent.verification_runs().len(), 2);
    assert_eq!(
        agent.verification_runs()[0].status,
        VerificationRunStatus::Running
    );
    assert_eq!(
        agent.verification_runs()[0].checks[0].status,
        VerificationCheckStatus::Pending
    );
    assert_eq!(
        agent.verification_runs()[1].status,
        VerificationRunStatus::Passed
    );
    assert_eq!(
        agent.verification_runs()[1].checks[0]
            .tool_call_id
            .as_deref(),
        Some("older-call")
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
        options[3..5]
            .iter()
            .all(|options| { options.reasoning == Some(ReasoningSelection::High) })
    );
    assert_eq!(options[5].reasoning, None);
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
async fn mechanical_verification_turns_use_one_lower_request_local_effort() {
    let fixture = TestFixture::new();
    let provider = RecoveryReasoningProvider::with_reasoning(
        vec![
            model_tool_response("call-test", "bash", r#"{"command":"cargo test"}"#),
            model_finished_response("verified"),
        ],
        ReasoningSelection::High,
    );
    let options = provider.options();
    let mut agent = Agent::builder(
        Box::new(provider),
        mutation_and_verification_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .system_context(String::new())
    .build()
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::Off);
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

    assert_eq!(result, AgentRunResult::Completed("verified".to_string()));
    let options = options.lock().await;
    assert_eq!(options.len(), 2);
    assert!(
        options
            .iter()
            .all(|options| { options.reasoning == Some(ReasoningSelection::Medium) })
    );
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
    let run = agent.verification_runs().last().expect("typed skipped run");
    assert_eq!(run.status, VerificationRunStatus::Blocked);
    assert_eq!(
        run.terminal_reason_kind,
        Some(crate::verification::VerificationTerminalReason::EnvironmentBlocked)
    );
}

#[tokio::test]
async fn resume_replaces_verification_state_and_interrupts_in_flight_run() {
    let fixture = TestFixture::new();
    let mut agent = verification_agent_with_policy(&fixture, VerifyAfterEdit::On);
    agent
        .begin_verification_run(
            VerificationKind::Test,
            &[crate::verification::VerificationCheck {
                name: "Old check".to_string(),
                command: "cargo test --locked".to_string(),
            }],
            "Run the old check.",
        )
        .await;
    agent.verification.after_edit_verification_pending = true;
    agent.verification.after_edit_verification_injected = true;
    let restored = crate::verification::VerificationRunRecord::running(
        VerificationKind::Build,
        &[crate::verification::VerificationCheck {
            name: "Resumed build".to_string(),
            command: "cargo build --locked".to_string(),
        }],
    );

    agent.restore_verification_runs(vec![restored]);

    assert_eq!(agent.verification_runs().len(), 1);
    assert_eq!(agent.verification_runs()[0].kind, VerificationKind::Build);
    assert_eq!(
        agent.verification_runs()[0].status,
        VerificationRunStatus::Interrupted
    );
    assert_eq!(
        agent.verification_runs()[0].checks[0].terminal_reason_kind,
        Some(VerificationTerminalReason::Interrupted)
    );
    assert!(agent.verification.active_verification.is_none());
    assert!(!agent.verification.after_edit_verification_pending);
    assert!(!agent.verification.after_edit_verification_injected);
}

#[tokio::test]
async fn stale_verification_rearms_at_the_next_edit_quiet_point_even_when_policy_is_off() {
    let fixture = TestFixture::new();
    std::fs::write(
        fixture.project_root.join("Cargo.toml"),
        "[package]\nname='verify'\nversion='0.1.0'\n",
    )
    .unwrap();
    let mut agent = verification_agent_with_policy(&fixture, VerifyAfterEdit::Off);
    let check = crate::provider::ToolCall {
        id: "call-initial-check".to_string(),
        name: "bash".to_string(),
        arguments: r#"{"command":"cargo test"}"#.to_string(),
    };
    agent
        .record_verification_tool_result(
            &check,
            &command_result(0),
            crate::output::ToolExecutionStatus::Succeeded,
        )
        .await;
    assert_eq!(
        agent.verification_runs().last().unwrap().status,
        VerificationRunStatus::Passed
    );

    agent.note_typed_verification_worthy_mutation(vec!["src.rs".to_string()]);
    assert_eq!(
        agent.verification_runs().last().unwrap().status,
        VerificationRunStatus::Stale
    );

    let sink: SharedSink = Arc::new(StdoutSink);
    assert!(
        agent
            .maybe_verify_after_edit(&sink, CancellationToken::new())
            .await,
        "a stale run is a pending obligation independent of the default post-edit policy"
    );
    assert_eq!(agent.verification_runs().len(), 2);
    assert_eq!(
        agent.verification_runs().last().unwrap().status,
        VerificationRunStatus::Running
    );
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
    let (_queue_sender, queue_receiver) = tokio::sync::mpsc::unbounded_channel();

    let result = agent
        .run_with_queue(
            crate::agent::UserInput::from_text("make a change"),
            CancellationToken::new(),
            Arc::new(StdoutSink),
            queue_receiver,
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("verified".to_string()));
    assert_eq!(requests.lock().await.len(), 4);
    let run = agent.verification_runs().last().unwrap();
    assert_eq!(run.status, VerificationRunStatus::Passed);
    assert_eq!(run.checks[0].tool_call_id.as_deref(), Some("call-test"));
}

#[tokio::test]
async fn coding_run_finishes_review_repairs_before_starting_one_final_gate() {
    let fixture = TestFixture::new();
    init_repo(&fixture.project_root);
    commit_file(
        &fixture.project_root,
        "Cargo.toml",
        "[package]\nname='verify'\nversion='0.1.0'\n",
        "baseline manifest",
    );
    commit_file(
        &fixture.project_root,
        "src.rs",
        "fn baseline() {}\n",
        "baseline source",
    );
    let provider = MockProvider::new(vec![
        model_tool_response("call-write", "write", r#"{"path":"src.rs"}"#),
        model_finished_response("implementation complete"),
        model_tool_response("call-repair", "write", r#"{"path":"src.rs"}"#),
        model_tool_response(
            "call-duplicate-review",
            "agent",
            r#"{"agent":"review","prompt":"review the repaired implementation"}"#,
        ),
        model_finished_response("Fixed the invariant violation from review."),
        model_tool_response("call-test", "bash", r#"{"command":"cargo test"}"#),
        model_finished_response("verified"),
    ]);
    let requests = provider.requests();
    let mut config = crate::config::Config::empty();
    config.verification.after_edit = VerifyAfterEdit::On;
    let mut agent = Agent::builder(
        Box::new(provider),
        review_and_verification_registry(fixture.project_root.clone()),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .system_context(String::new())
    .config(Arc::new(config))
    .build()
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::On);
    let (_queue_sender, queue_receiver) = tokio::sync::mpsc::unbounded_channel();

    let result = agent
        .run_with_queue(
            crate::agent::UserInput::from_text("make a reviewable change"),
            CancellationToken::new(),
            Arc::new(StdoutSink),
            queue_receiver,
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("verified".to_string()));
    let requests = requests.lock().await;
    assert_eq!(requests.len(), 7);
    let duplicate_review_result = requests[4]
        .iter()
        .find(|message| {
            matches!(message, ChatCompletionRequestMessage::Tool(_))
                && message_content(message).contains("automatic review pass is already complete")
        })
        .map(message_content);
    assert!(
        duplicate_review_result.is_some(),
        "the rejected duplicate review result must be returned before repairs complete: {:?}",
        requests[4]
    );
    assert_eq!(agent.self_review_runs().len(), 1);
    assert_eq!(
        agent.self_review_runs()[0].disposition,
        Some(crate::self_review::SelfReviewDisposition::Fixed)
    );
    assert_eq!(agent.verification_runs().len(), 1);
    assert_eq!(
        agent.verification_runs()[0].status,
        VerificationRunStatus::Passed
    );
}
