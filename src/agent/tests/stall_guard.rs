//! Integration coverage for the implementation-stall guard (session 84): a
//! coding-persona run that only explores — distinct reads every turn, so the
//! signature-based inspection guards never trip — must receive model-only
//! nudges without being stopped, while progress, planning runs, and subagent
//! lanes stay unguarded.

use super::*;

use super::super::ExecutionLane;
use super::super::run_loop::{
    IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS, IMPLEMENTATION_STALL_REPEATED_NUDGE_INTERVAL_TURNS,
    IMPLEMENTATION_STALL_REPEATED_NUDGE_START_TURNS,
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

fn stall_notes_in(agent: &Agent) -> Vec<String> {
    // Harness notes carry provenance and may ride a system- or user-role
    // message depending on trust level; match on serialized content.
    agent
        .messages
        .iter()
        .filter_map(|message| {
            let value = serde_json::to_value(message).ok()?;
            let content = value.get("content")?.as_str()?;
            (content.starts_with("Harness note:") && content.contains("exploration"))
                .then(|| content.to_string())
        })
        .collect()
}

#[tokio::test]
async fn stall_guard_keeps_long_exploration_run_alive_with_persistent_nudges() {
    let fixture = TestFixture::new();
    let read_tool = Arc::new(MockTool::new("read", "read result"));
    let mut registry = ToolRegistry::new();
    registry.register(read_tool.clone());

    let exploration_turns = IMPLEMENTATION_STALL_REPEATED_NUDGE_START_TURNS
        + IMPLEMENTATION_STALL_REPEATED_NUDGE_INTERVAL_TURNS;
    let responses = (0..exploration_turns)
        .map(distinct_read_turn)
        .chain([final_text_turn("finished after long research")])
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
    agent.budget.max_iterations = exploration_turns + 2;
    let sink = Arc::new(CaptureSink::default());

    let result = agent
        .run(
            "implement the feature",
            CancellationToken::new(),
            sink.clone(),
        )
        .await
        .unwrap();
    assert_eq!(
        result,
        AgentRunResult::Completed("finished after long research".to_string())
    );

    // The guard never rejects reads — every scripted turn still executed.
    assert_eq!(read_tool.calls.lock().await.len(), exploration_turns);
    let notes = stall_notes_in(&agent);
    assert_eq!(
        notes.len(),
        4,
        "initial, second, and recurring nudges should have been injected: {notes:#?}"
    );
    assert!(notes[0].contains("read-only exploration"), "{}", notes[0]);
    assert!(notes[0].contains("expected baseline"), "{}", notes[0]);
    assert!(notes[1].contains("Second notice"), "{}", notes[1]);
    assert!(
        notes[2..]
            .iter()
            .all(|note| note.contains("Persistent implementation nudge")),
        "later nudges must keep steering without terminating: {notes:#?}"
    );
    assert!(
        sink.statuses().is_empty(),
        "implementation guard nudges must stay model-only"
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
            "implement the feature",
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
