//! Integration coverage for the semantic implementation-stall circuit breaker.

use super::*;

use super::super::ExecutionLane;
use super::super::run_loop::{
    IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS, IMPLEMENTATION_STALL_TERMINAL_TURNS,
};

/// One exploration-only model turn: a single `read` call whose arguments are
/// unique per turn — the session-84 shape that defeats repeat-signature guards.
fn distinct_read_turn(turn: usize) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        tool_calls: vec![test_tool_call(
            &format!("read-{turn}"),
            "read",
            &format!(r#"{{"title":"inspect src/file{turn}.rs"}}"#),
        )],
        ..StreamedResponse::default()
    })
}

fn final_text_turn(content: &str) -> crate::provider::ProviderResult<StreamedResponse> {
    Ok(StreamedResponse {
        content: content.to_string(),
        tool_calls: vec![],
        ..StreamedResponse::default()
    })
}

/// Minimal bash stand-in with the real `command` schema: MockTool's schema
/// requires `title`, which would fail validation and turn the progress turn
/// into a failed (non-progress) one.
struct OkBashTool;

#[async_trait]
impl Tool for OkBashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "mock bash"
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
            rendered: "checks passed".to_string(),
            stdout: "checks passed\n".to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
            truncation: None,
        })
    }
}

struct FailedBashTool {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl Tool for FailedBashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "mock failed bash"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        OkBashTool.parameters_schema()
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(ToolOutput::Command {
            rendered: "same failure".to_string(),
            stdout: String::new(),
            stderr: "operation failed\n".to_string(),
            exit_code: Some(1),
            timed_out: false,
            truncation: None,
        })
    }
}

struct VolatileTerminalPollTool {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl Tool for VolatileTerminalPollTool {
    fn name(&self) -> &str {
        "terminal"
    }

    fn description(&self) -> &str {
        "mock terminal poll"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["title"],
            "properties": {"title": {"type": "string"}},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let call = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(ToolOutput::untrusted_context(
            "terminal:pty-1",
            &format!(
                "ID: pty-1\nSemantic version: 3\nStatus: running\nOutput: {} chars\nNormalized screen:\nworking",
                100 + call
            ),
        ))
    }
}

struct VolatileTaskPollTool {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl Tool for VolatileTaskPollTool {
    fn name(&self) -> &str {
        "tasks"
    }

    fn description(&self) -> &str {
        "mock task poll"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["title"],
            "properties": {"title": {"type": "string"}},
            "additionalProperties": false
        })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let call = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(ToolOutput::Text(format!(
            "ID     | Version | State     | Duration | Exit/timeout | Output | Command\n\
             bg-1   |       2 | running   | {call}s | -            | 10 B   | cargo test"
        )))
    }
}

fn stall_notes_in(agent: &Agent) -> Vec<String> {
    // Harness notes carry provenance and may ride a system- or user-role
    // message depending on trust level; match on serialized content.
    agent
        .messages
        .iter()
        .filter_map(|message| {
            let value = serde_json::to_value(message).ok()?;
            let content = value.get("content")?.as_str()?;
            (content.starts_with("Harness note:") && content.contains("Implementation-stall"))
                .then(|| content.to_string())
        })
        .collect()
}

#[tokio::test]
async fn stall_guard_terminates_unchanged_reads_with_typed_failure() {
    let fixture = TestFixture::new();
    let read_tool = Arc::new(MockTool::new("read", "read result"));
    let mut registry = ToolRegistry::new();
    registry.register(read_tool.clone());

    // The first successful read contributes one novel evidence frame. The
    // provider-independent terminal bound then counts unchanged frames.
    let exploration_turns = IMPLEMENTATION_STALL_TERMINAL_TURNS + 1;
    let responses = (0..exploration_turns).map(distinct_read_turn).collect();
    let mut agent = Agent::new(
        Box::new(MockProvider::new(responses)),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.budget.max_iterations = exploration_turns + 1;
    let sink = Arc::new(CaptureSink::default());

    let result = agent
        .run(
            "research the feature",
            CancellationToken::new(),
            sink.clone(),
        )
        .await
        .unwrap();
    let AgentRunResult::Incomplete { output, failure } = result else {
        panic!("stall must be a typed incomplete result");
    };
    assert_eq!(
        failure.outcome,
        crate::agent::CompletionFailureOutcome::Failed
    );
    assert_eq!(
        failure.gaps,
        vec![crate::agent::CompletionGap::ImplementationStall(
            IMPLEMENTATION_STALL_TERMINAL_TURNS
        )]
    );
    assert!(output.contains("Partial workspace and conversation state were preserved"));

    assert_eq!(read_tool.calls.lock().await.len(), exploration_turns);
    let notes = stall_notes_in(&agent);
    assert_eq!(
        notes.len(),
        3,
        "nudge, recovery, and terminal transitions must be persisted: {notes:#?}"
    );
    assert!(notes[0].contains("guard:"), "{}", notes[0]);
    assert!(notes[1].contains("recovery:"), "{}", notes[1]);
    assert!(notes[2].contains("terminal:"), "{}", notes[2]);
    assert!(
        notes[2].contains("equivalent file evidence"),
        "{}",
        notes[2]
    );
    assert!(
        sink.statuses().is_empty(),
        "implementation guard nudges must stay model-only"
    );
}

#[tokio::test]
async fn stall_guard_terminates_syntactically_varied_failed_calls() {
    let fixture = TestFixture::new();
    let bash = Arc::new(FailedBashTool {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut registry = ToolRegistry::new();
    registry.register(bash.clone());
    let responses = (0..IMPLEMENTATION_STALL_TERMINAL_TURNS)
        .map(|turn| {
            Ok(StreamedResponse {
                tool_calls: vec![test_tool_call(
                    &format!("failed-{turn}"),
                    "bash",
                    &format!(r#"{{"command":"false # spelling {turn}"}}"#),
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
    agent.budget.max_iterations = IMPLEMENTATION_STALL_TERMINAL_TURNS + 1;

    let result = agent
        .run(
            "repair the failure",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert!(matches!(result, AgentRunResult::Incomplete { .. }));
    assert_eq!(
        bash.calls.load(std::sync::atomic::Ordering::Relaxed),
        IMPLEMENTATION_STALL_TERMINAL_TURNS
    );
}

#[tokio::test]
async fn stall_guard_terminates_equivalent_terminal_and_status_polls() {
    let fixture = TestFixture::new();
    let terminal = Arc::new(VolatileTerminalPollTool {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let tasks = Arc::new(VolatileTaskPollTool {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut registry = ToolRegistry::new();
    registry.register(terminal.clone());
    registry.register(tasks.clone());
    let poll_turns = IMPLEMENTATION_STALL_TERMINAL_TURNS + 1;
    let responses = (0..poll_turns)
        .map(|turn| {
            Ok(StreamedResponse {
                tool_calls: vec![
                    test_tool_call(
                        &format!("terminal-{turn}"),
                        "terminal",
                        &format!(r#"{{"title":"terminal poll {turn}"}}"#),
                    ),
                    test_tool_call(
                        &format!("tasks-{turn}"),
                        "tasks",
                        &format!(r#"{{"title":"task poll {turn}"}}"#),
                    ),
                ],
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
    agent.budget.max_iterations = poll_turns + 1;

    let result = agent
        .run(
            "wait for the same state forever",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert!(matches!(result, AgentRunResult::Incomplete { .. }));
    assert_eq!(
        terminal.calls.load(std::sync::atomic::Ordering::Relaxed),
        poll_turns
    );
    assert_eq!(
        tasks.calls.load(std::sync::atomic::Ordering::Relaxed),
        poll_turns
    );
}

#[tokio::test]
async fn stall_guard_resets_on_progress_making_bash_turn() {
    let fixture = TestFixture::new();
    let read_tool = Arc::new(MockTool::new("read", "read result"));
    let mut registry = ToolRegistry::new();
    registry.register(read_tool.clone());
    registry.register(Arc::new(OkBashTool));

    // Five reads, a progress-making bash turn (non-file-read command), five
    // more reads: neither read streak reaches the nudge threshold.
    let before = (0..IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS - 1).map(distinct_read_turn);
    let progress = [Ok(StreamedResponse {
        tool_calls: vec![test_tool_call(
            "bash-1",
            "bash",
            r#"{"command":"cargo check"}"#,
        )],
        ..StreamedResponse::default()
    })];
    let after = (100..100 + IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS - 1).map(distinct_read_turn);
    let responses = before
        .chain(progress)
        .chain(after)
        .chain([final_text_turn("done")])
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
    agent.budget.max_iterations = IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS * 2;

    let result = agent
        .run(
            "research the feature",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert!(
        stall_notes_in(&agent).is_empty(),
        "a run that keeps making progress must never be nudged"
    );
}

#[tokio::test]
async fn stall_guard_inactive_for_planning_persona() {
    let fixture = TestFixture::new();
    let read_tool = Arc::new(MockTool::new("read", "read result"));
    let mut registry = ToolRegistry::new();
    registry.register(read_tool.clone());

    // Enough read-only turns to trip the coding stall nudge, but still under
    // the planning research budget: planning research is governed by
    // PLANNING_RESEARCH_TURN_LIMIT, not this guard.
    let research_turns = IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS + 1;
    assert!(research_turns < PLANNING_RESEARCH_TURN_LIMIT);
    let responses = (0..research_turns)
        .map(distinct_read_turn)
        .chain([final_text_turn("plan drafted")])
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
    agent.budget.max_iterations = research_turns + 2;

    let result = agent
        .run(
            "draft a plan",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert_eq!(
        result,
        AgentRunResult::Completed("plan drafted".to_string())
    );
    assert!(stall_notes_in(&agent).is_empty());
}

#[tokio::test]
async fn stall_guard_inactive_for_subagent_lane() {
    let fixture = TestFixture::new();
    let read_tool = Arc::new(MockTool::new("read", "read result"));
    let mut registry = ToolRegistry::new();
    registry.register(read_tool.clone());

    let research_turns = IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS + 1;
    let responses = (0..research_turns)
        .map(distinct_read_turn)
        .chain([final_text_turn("explored")])
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
    agent.set_execution_lane(ExecutionLane::subagent("sub-1"));
    agent.budget.max_iterations = research_turns + 2;

    let result = agent
        .run(
            "explore the module",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();

    assert_eq!(result, AgentRunResult::Completed("explored".to_string()));
    assert!(
        stall_notes_in(&agent).is_empty(),
        "read-only subagent missions (explore, review) must not be stall-guarded"
    );
}
