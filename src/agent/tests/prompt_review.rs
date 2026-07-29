use super::*;

#[tokio::test]
async fn errors_when_max_iterations_are_exhausted() {
    let fixture = TestFixture::new();
    let responses = (0..2)
        .map(|_| {
            Ok(StreamedResponse {
                content: String::new(),
                tool_calls: vec![crate::provider::ToolCall {
                    id: "call-1".to_string(),
                    name: "unknown".to_string(),
                    arguments: "{}".to_string(),
                }],
                terminal: crate::provider::StreamTerminal::Completed(
                    crate::provider::FinishReason::Stop,
                ),
                usage: None,
                ..StreamedResponse::default()
            })
        })
        .collect();
    let provider = Box::new(MockProvider::new(responses));
    let mut agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.budget.max_iterations = 2;
    let result = agent
        .run("hello", CancellationToken::new(), Arc::new(StdoutSink))
        .await;

    let err = result.unwrap_err();
    assert_eq!(
        err.downcast_ref::<crate::run_budget::RunBudgetExhaustion>(),
        Some(&crate::run_budget::RunBudgetExhaustion::MaxTurns { limit: 2 })
    );
    let err = err.to_string();
    assert!(
        err.contains("Agent stopped after 2 model/tool iterations"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn restored_session_turn_budget_stops_before_another_provider_call() {
    let fixture = TestFixture::new();
    let provider = Box::new(MockProvider::new(vec![Ok(StreamedResponse {
        content: "done".to_string(),
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        ..StreamedResponse::default()
    })]));
    let mut first_agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    first_agent
        .run("first", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();
    let persisted_turns = first_agent.usage_turns().to_vec();

    let resumed_provider = MockProvider::new(Vec::new());
    let resumed_requests = resumed_provider.requests();
    let mut resumed_agent = Agent::new(
        Box::new(resumed_provider),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    resumed_agent.restore_usage_turns(persisted_turns);
    resumed_agent.set_run_budget(crate::run_budget::RunBudget {
        max_session_turns: Some(1),
        ..crate::run_budget::RunBudget::default()
    });

    let error = resumed_agent
        .run("continue", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap_err();

    assert_eq!(
        error.downcast_ref::<crate::run_budget::RunBudgetExhaustion>(),
        Some(&crate::run_budget::RunBudgetExhaustion::SessionTurns { limit: 1, used: 1 })
    );
    assert!(resumed_requests.lock().await.is_empty());
}

#[tokio::test]
async fn restored_session_output_budget_stops_before_another_provider_call() {
    let fixture = TestFixture::new();
    let provider = Box::new(MockProvider::new(vec![Ok(StreamedResponse {
        content: "done".to_string(),
        terminal: crate::provider::StreamTerminal::Completed(crate::provider::FinishReason::Stop),
        ..StreamedResponse::default()
    })]));
    let mut first_agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    first_agent
        .run("first", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();
    let mut persisted_turns = first_agent.usage_turns().to_vec();
    persisted_turns[0].provider_attempts[0].assistant_chars = 10;

    let resumed_provider = MockProvider::new(Vec::new());
    let resumed_requests = resumed_provider.requests();
    let mut resumed_agent = Agent::new(
        Box::new(resumed_provider),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    resumed_agent.restore_usage_turns(persisted_turns);
    resumed_agent.set_run_budget(crate::run_budget::RunBudget {
        max_session_output_chars: Some(10),
        ..crate::run_budget::RunBudget::default()
    });

    let error = resumed_agent
        .run("continue", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap_err();

    assert_eq!(
        error.downcast_ref::<crate::run_budget::RunBudgetExhaustion>(),
        Some(&crate::run_budget::RunBudgetExhaustion::SessionOutput {
            limit_chars: 10,
            used_chars: 10,
        })
    );
    assert!(resumed_requests.lock().await.is_empty());
}

fn system_content(mode: AgentMode, context: &str) -> String {
    let message = system_message(mode, context);
    let value = serde_json::to_value(&message).unwrap();
    value
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or_default()
        .to_string()
}

#[test]
fn system_context_is_appended_to_prompt() {
    let content = system_content(AgentMode::Coding, "MARKER-CTX");
    assert!(content.contains("coding agent"));
    assert!(content.contains("# Project context"));
    assert!(content.contains("MARKER-CTX"));
}

#[test]
fn empty_context_omits_project_section() {
    let content = system_content(AgentMode::Coding, "   ");
    assert!(content.contains("coding agent"));
    assert!(!content.contains("# Project context"));
}

#[test]
fn coding_prompt_says_bash_starts_in_project_cwd() {
    let content = system_content(AgentMode::Coding, "");
    assert!(content.contains("Bash commands start in the project cwd"));
    assert!(content.contains("Do not prefix commands with `cd <repo> &&`"));
    assert!(content.contains("workdir"));
}

#[test]
fn coding_prompt_requires_professional_implementation_loop() {
    let content = system_content(AgentMode::Coding, "");
    assert!(content.contains("Operating principles"));
    assert!(content.contains("Instruction priority"));
    assert!(content.contains("ask only when blocked"));
    assert!(content.contains("smallest coherent change"));
    assert!(content.contains("For non-trivial implementation work"));
    assert!(content.contains("for each /start phase"));
    assert!(content.contains("make a short plan"));
    assert!(content.contains("Run targeted verification such as cargo check, clippy, formatters, or equivalent project tools."));
    assert!(content.contains("mini self-review"));
    assert!(content.contains("same standard as /review"));
    assert!(content.contains("Run full verification"));
}

#[test]
fn coding_prompt_keeps_progress_in_todowrite() {
    let content = system_content(AgentMode::Coding, "");
    assert!(content.contains(
        "Track anything larger than a single trivial answer or one tiny edit with todowrite"
    ));
    assert!(content.contains("Do not use plan-canvas tools to mark implementation progress"));
    assert!(content.contains("review findings in the prompt as implementation work"));
    assert!(!content.contains("plan_resolve_finding"));
}

#[test]
fn coding_prompt_requires_worktree_protection() {
    let content = system_content(AgentMode::Coding, "");
    assert!(content.contains("changes you did not make"));
    assert!(content.contains("do not remove them"));
    assert!(content.contains("reset git state"));
    assert!(content.contains("Stay on the current branch"));
}

#[test]
fn coding_prompt_forbids_telegraphic_narration_fragments() {
    let content = system_content(AgentMode::Coding, "");
    // The anti-telegraphic guardrail must be present: visible narration is
    // complete sentences, fragments/notes-to-self are banned and routed to
    // thinking. Exact phrasing may evolve; each assertion pins one of those
    // three commitments rather than a full sentence.
    assert!(content.contains("complete sentence"));
    assert!(
        content.contains("No fragments or notes-to-self"),
        "the fragment ban must stay in the coding prompt"
    );
    assert!(content.contains("belongs in thinking"));
    // ...and the permissive phrasing that caused the leak must be gone.
    assert!(
        !content.contains("narrate as you go"),
        "the regressed permissive narration phrasing must not return"
    );
}

#[test]
fn planning_prompt_forbids_telegraphic_notes() {
    let content = system_content(AgentMode::Planning, "");
    assert!(content.contains("never telegraphic notes-to-self"));
}

#[test]
fn planning_prompt_requires_structured_task_tools() {
    let content = system_content(AgentMode::Planning, "");
    assert!(content.contains("one plan_replace_draft call"));
    assert!(content.contains("tasks or phases arrays"));
    assert!(content.contains("/start hands to the coding agent as todos"));
}

#[test]
fn planning_prompt_keeps_todos_short_and_context_in_sections() {
    let content = system_content(AgentMode::Planning, "");
    assert!(content.contains("keep each task a short action label"));
    assert!(content.contains("anchor it to its file or function"));
    assert!(content.contains("Rationale, edge cases, and test strategy live in the sections"));
}

#[test]
fn planning_prompt_plans_behavior_not_code() {
    // The user's bar (aligned with Codex plan mode): decision complete — the
    // executor inherits no open choices — but the plan describes behavior,
    // not a code walkthrough: grouped behavior-level changes, minimal file
    // naming, and only the detail needed for implementation safety.
    let content = system_content(AgentMode::Planning, "");
    assert!(content.contains("decision-complete spec"));
    assert!(content.contains("DECIDED approach"));
    assert!(content.contains("by subsystem or behavior rather than file-by-file inventories"));
    assert!(content.contains("behavior-level descriptions over symbol-by-symbol edit lists"));
    assert!(content.contains("minimum detail needed for implementation safety"));
    assert!(
        content.contains(
            "Do not invent detailed schema, validation, precedence, or wire-format policy"
        )
    );
}

#[test]
fn planning_prompt_distinguishes_discoverable_facts_from_preferences() {
    // Codex-style unknown handling: repo truth is explored, never asked;
    // preferences/tradeoffs are asked early with a recommended default that
    // becomes a recorded assumption if unanswered.
    let content = system_content(AgentMode::Planning, "");
    assert!(content.contains("Discoverable facts"));
    assert!(content.contains("Preferences and tradeoffs cannot be discovered"));
    assert!(content.contains("recommended default"));
    assert!(content.contains("record it as an assumption"));
    assert!(content.contains("Settle intent before mechanism"));
}

#[test]
fn planning_prompt_leads_with_a_short_human_overview() {
    // Complement to the execution-detail bar: the canvas top is a skimmable
    // human overlay — approve without reading further — while depth stays in
    // the sections. Neither audience's layer may be sacrificed for the other.
    let content = system_content(AgentMode::Planning, "");
    assert!(content.contains("two readers at different depths"));
    assert!(content.contains("short human overview"));
    assert!(content.contains("approve the plan without reading further"));
    assert!(content.contains("never thin the sections to make the overview look complete"));
}

#[test]
fn planning_prompt_requires_evidence_based_precise_plans() {
    let content = system_content(AgentMode::Planning, "");
    assert!(content.contains("Planning principles"));
    assert!(content.contains("Canvas mechanics"));
    assert!(content.contains("Prefer current code and command output over stale docs"));
    assert!(content.contains("Plans never contain open questions"));
    assert!(!content.contains("questions array"));
    assert!(content.contains("Make plans clean and precise"));
    assert!(content.contains("Concise, not sparse"));
    assert!(content.contains("nothing consequential is left to guess"));
    assert!(content.contains("Context, Goals, Non-goals, Phases, and Validation"));
    assert!(content.contains("Do not invent line counts, test counts, performance claims"));
    assert!(content.contains("start with a verb"));
}

#[test]
fn planning_prompt_uses_a_lightweight_spec_interview() {
    let content = system_content(AgentMode::Planning, "");

    assert!(content.contains("extracting the requirements, constraints, and decisions"));
    assert!(content.contains("never force an interview or repeat a question"));
    assert!(content.contains("facts the code can answer"));
    assert!(content.contains("consequential product or implementation choices"));
    assert!(content.contains("prefer one focused decision at a time"));
    assert!(content.contains("include a recommended option"));
    assert!(content.contains("ask the user with the question tool before drafting"));
}

#[test]
fn planning_prompt_synthesizes_detail_into_a_decision_complete_draft() {
    let content = system_content(AgentMode::Planning, "");

    assert!(content.contains("ordinary user messages as valid free-form answers"));
    assert!(content.contains("Synthesize user answers and repository evidence"));
    assert!(content.contains("close consequential gaps before declaring the plan complete"));
    assert!(content.contains("decision-complete spec"));
    for requirement in [
        "behavior",
        "scope",
        "key decisions",
        "edge cases",
        "test strategy",
        "assumptions",
    ] {
        assert!(content.contains(requirement), "missing {requirement}");
    }
    assert!(content.contains("Later user details refine the existing draft"));
    assert!(content.contains("granular patch/insert/remove/move tools"));
}

#[test]
fn coding_prompt_names_memory_tiers_and_capture_triggers() {
    let content = system_content(AgentMode::Coding, "");
    assert!(content.contains("save it with memory_write"));
    assert!(content.contains("tier user is global memory that follows the user across projects"));
    assert!(content.contains("tier project is stored in this repo's .bonsai/memory"));
    // Capture corner cases: explicit remember requests, hard-won discoveries,
    // the shared project tier, and one-off vs standing instructions.
    assert!(content.contains("explicitly asks you to remember"));
    assert!(content.contains("non-obvious gotcha the hard way"));
    assert!(content.contains("personal or sensitive facts never go there"));
    assert!(content.contains("save the non-obvious part instead"));
    assert!(content.contains("Standing means it outlives this task"));
}

#[test]
fn planning_prompt_carves_out_memory_write() {
    let content = system_content(AgentMode::Planning, "");
    // Planning stays read-only for the codebase but owns one write surface.
    assert!(content.contains("You never modify project files"));
    assert!(content.contains("your one write surface is memory_write"));
    assert!(content.contains("tier user is global memory that follows the user across projects"));
    assert!(content.contains("the repo, steering files, or the plan itself already record"));
    assert!(content.contains("nothing personal or sensitive there"));
    assert!(content.contains("explicitly asks you to remember"));
}

#[test]
fn review_prompt_requires_read_only_evidence_and_severity_definitions() {
    let content = system_content(AgentMode::Review, "");
    assert!(content.contains("You are a code reviewer"));
    assert!(content.contains("Do not modify files, run mutating commands, or update todos"));
    assert!(
        content.contains("plan_add_finding"),
        "review prompt should instruct structured finding capture"
    );
    assert!(content.contains("Base findings on evidence"));
    assert!(content.contains("Blocker (must fix before merge)"));
    assert!(content.contains("Major (likely bug/regression)"));
    assert!(content.contains("Minor (edge case/maintainability)"));
    assert!(content.contains("Nit (small polish)"));
    assert!(content.contains("If there are no substantive findings, say so"));
}

#[test]
fn planning_prompt_mentions_reorder_and_question_tools() {
    let content = system_content(AgentMode::Planning, "");
    assert!(content.contains("plan_move_section"));
    assert!(!content.contains("plan_add_question"));
    assert!(content.contains("plan_remove_question"));
    assert!(content.contains("repeats the section's own heading"));
}

#[tokio::test]
async fn implement_plan_from_seeds_todos_and_resets_messages() {
    use crate::plan::PlanDoc;
    use crate::todo::TodoStatus;

    let fixture = TestFixture::new();
    let provider = Box::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::from("ctx-line"),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_mode(AgentMode::Planning);
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::On);

    // Pre-seed an old conversation + a stale todo so we can verify
    // both are reset.
    agent
        .messages
        .push(test_user_message("ignore me: old turn"));
    let mut todo_store = crate::todo::TodoStore::new();
    todo_store.set_todos(vec![crate::todo::TodoItem {
        content: "stale item".to_string(),
        status: TodoStatus::InProgress,
    }]);
    agent.set_todo_store(Arc::new(tokio::sync::Mutex::new(todo_store)));

    let mut plan = PlanDoc::default();
    plan.edit().set_title("Demo");
    plan.edit().add_task("A");
    plan.edit().add_task("B");
    plan.edit().add_task("C");

    let has_plan = agent.implement_plan_from(&plan, None).await;
    assert!(has_plan, "plan with tasks should be implementable");
    assert_eq!(agent.mode, AgentMode::Coding);
    assert!(
        agent.self_review.is_armed(),
        "plan handoff should arm self-review"
    );

    // Conversation: exactly two messages — system + user. The system
    // message must contain the project context (so the model gets
    // fresh cwd info), the user message must contain the plan
    // markdown.
    assert_eq!(agent.messages.len(), 2);
    let system_text = system_content(agent.mode, "ctx-line");
    let user_text = user_message_content(&agent.messages[1]);
    assert!(
        system_text.contains("ctx-line"),
        "system message should be rebuilt with project context, got: {system_text:?}"
    );
    assert!(
        user_text.contains("The plan on the canvas is ready"),
        "user message should carry the implement prompt, got: {user_text:?}"
    );
    assert!(
        user_text.contains("Use todowrite explicitly"),
        "user message should tell the model to use todowrite, got: {user_text:?}"
    );
    assert!(
        user_text.contains("[in_progress] A")
            && user_text.contains("[pending] B")
            && user_text.contains("[pending] C"),
        "user message should include the extracted todo list with statuses, got: {user_text:?}"
    );
    assert!(
        user_text.contains("Task A") || user_text.contains("A"),
        "user message should embed the plan body so the model has a canonical view, got: {user_text:?}"
    );

    // Todo store: tasks seeded, first pending is in_progress.
    let store = agent.todo_store.as_ref().expect("todo store was attached");
    let snapshot = store.lock().await.todos().to_vec();
    assert_eq!(snapshot.len(), 3);
    assert_eq!(snapshot[0].content, "A");
    assert_eq!(snapshot[0].status, TodoStatus::InProgress);
    assert_eq!(snapshot[1].content, "B");
    assert_eq!(snapshot[1].status, TodoStatus::Pending);
    assert_eq!(snapshot[2].content, "C");
    assert_eq!(snapshot[2].status, TodoStatus::Pending);
}

#[tokio::test]
async fn implement_plan_from_can_keep_planning_context() {
    use crate::plan::PlanDoc;

    let fixture = TestFixture::new();
    let mut agent = Agent::new(
        Box::new(MockProvider::new(vec![])),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_mode(AgentMode::Planning);
    agent
        .messages
        .push(test_user_message("keep this planning detail"));
    let mut plan = PlanDoc::default();
    plan.edit().set_title("Demo");
    plan.edit().add_task("Implement it");

    let has_plan = agent
        .implement_plan_from_with_context(&plan, None, crate::agent::PlanContextMode::Keep)
        .await;

    assert!(has_plan);
    assert_eq!(agent.mode, AgentMode::Coding);
    assert!(
        agent
            .messages
            .iter()
            .any(|message| message_content(message).contains("keep this planning detail"))
    );
    assert!(
        agent
            .messages
            .last()
            .is_some_and(|message| message_content(message).contains("Please implement it"))
    );
}

#[tokio::test]
async fn implement_plan_from_phase_seeds_only_that_phase() {
    use crate::plan::PlanDoc;
    use crate::todo::TodoStatus;

    let fixture = TestFixture::new();
    let provider = Box::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::from("ctx-line"),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_mode(AgentMode::Planning);
    agent.set_todo_store(Arc::new(tokio::sync::Mutex::new(
        crate::todo::TodoStore::new(),
    )));

    let mut plan = PlanDoc::default();
    plan.edit().set_title("Phased demo");
    plan.edit().add_phase("Phase 1: storage");
    plan.edit().add_phase("Phase 2: wiring");
    plan.edit()
        .add_task_to_phase("Phase 1: storage", "add table")
        .unwrap();
    plan.edit()
        .add_task_to_phase("Phase 2: wiring", "wire it up")
        .unwrap();
    plan.edit()
        .add_task_to_phase("Phase 2: wiring", "render it")
        .unwrap();

    // Implement the second phase only.
    let has_plan = agent.implement_plan_from(&plan, Some(1)).await;
    assert!(has_plan);
    assert_eq!(agent.mode, AgentMode::Coding);

    let user_text = user_message_content(&agent.messages[1]);
    assert!(
        user_text.contains("Implement only this phase — Phase 2: wiring"),
        "message should scope the agent to the phase, got: {user_text:?}"
    );
    // Only the targeted phase's tasks are in the todo block; phase 1's are not.
    assert!(user_text.contains("[in_progress] wire it up"));
    assert!(user_text.contains("[pending] render it"));
    assert!(
        !user_text.contains("[in_progress] add table")
            && !user_text.contains("[pending] add table"),
        "phase 1's tasks must not be seeded for a phase-2 run, got: {user_text:?}"
    );
    // Full plan markdown is still appended for context (all phases visible).
    assert!(user_text.contains("## Phase 1: storage"));

    let store = agent.todo_store.as_ref().expect("todo store was attached");
    let snapshot = store.lock().await.todos().to_vec();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(snapshot[0].content, "wire it up");
    assert_eq!(snapshot[0].status, TodoStatus::InProgress);
    assert_eq!(snapshot[1].content, "render it");
    assert_eq!(snapshot[1].status, TodoStatus::Pending);
}

#[tokio::test]
async fn implement_plan_from_section_only_plan_starts_and_clears_stale_todos() {
    use crate::plan::PlanDoc;
    use crate::todo::TodoStatus;

    let fixture = TestFixture::new();
    let provider = Box::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    let todo_store = Arc::new(tokio::sync::Mutex::new(crate::todo::TodoStore::new()));
    todo_store
        .lock()
        .await
        .set_todos(vec![crate::todo::TodoItem {
            content: "stale item".to_string(),
            status: TodoStatus::InProgress,
        }]);
    agent.set_todo_store(todo_store.clone());

    let mut plan = PlanDoc::default();
    plan.edit().set_title("Section-only plan");
    plan.edit()
        .set_section("Approach", "Use the saved plan details.");

    let has_plan = agent.implement_plan_from(&plan, None).await;

    assert!(has_plan, "section-only plans should still be implemented");
    assert_eq!(agent.messages.len(), 2);
    let user_text = user_message_content(&agent.messages[1]);
    assert!(user_text.contains("The plan on the canvas is ready"));
    assert!(user_text.contains("Use todowrite explicitly"));
    assert!(user_text.contains("No explicit todo list was extracted"));
    assert!(user_text.contains("Section-only plan"));
    assert!(user_text.contains("Use the saved plan details."));
    assert!(todo_store.lock().await.todos().is_empty());
}

#[tokio::test]
async fn implement_plan_from_handles_empty_plan() {
    use crate::plan::PlanDoc;

    let fixture = TestFixture::new();
    let provider = Box::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    let plan = PlanDoc::default();
    let has_plan = agent.implement_plan_from(&plan, None).await;
    assert!(!has_plan);

    // With no plan content we leave the user prompt empty: just the
    // refreshed system message.
    assert_eq!(agent.messages.len(), 1);
}

#[tokio::test]
async fn implement_plan_from_includes_structured_findings_block() {
    use crate::plan::{Finding, PlanDoc, Severity};

    let fixture = TestFixture::new();
    let provider = Box::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    let mut plan = PlanDoc::default();
    plan.edit().set_title("Demo");
    plan.edit().add_task("Fix it");
    plan.edit().add_finding(Finding {
        severity: Severity::Blocker,
        file: Some("src/foo.rs".to_string()),
        line: Some(10),
        issue: "data loss on close".to_string(),
        required_fix: "flush before close".to_string(),
        acceptance_tests: vec!["no loss under test".to_string()],
        source_ids: vec!["call-9".to_string()],
        task: Some("Fix it".to_string()),
        resolved: false,
    });

    agent.implement_plan_from(&plan, None).await;

    // The findings ride as their own structured section, built from
    // plan.findings (which survives the message reset) — not the prose.
    let user_text = user_message_content(&agent.messages[1]);
    assert!(
        user_text.contains("## Review findings (must address)"),
        "handoff should carry the findings section, got: {user_text:?}"
    );
    assert!(
        user_text.contains("[BLOCKER] src/foo.rs:10 — data loss on close"),
        "got: {user_text:?}"
    );
    assert!(user_text.contains("Required fix: flush before close"));
    assert!(user_text.contains("Task: Fix it"));
    assert!(user_text.contains("do not mutate the plan canvas to mark them done"));
}

#[tokio::test]
async fn begin_focused_coding_run_arms_self_review() {
    let fixture = TestFixture::new();
    let provider = Box::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.set_self_review_mode(crate::self_review::SelfReviewMode::On);

    agent.begin_focused_coding_run("focused prompt").await;

    assert_eq!(agent.mode, AgentMode::Coding);
    assert!(
        agent.self_review.is_armed(),
        "focused coding workflows should arm self-review"
    );
    assert_eq!(agent.messages.len(), 2);
    assert!(user_message_content(&agent.messages[1]).contains("focused prompt"));
}

#[tokio::test]
async fn clear_resets_session_context_and_usage() {
    let fixture = TestFixture::new();
    let provider = Box::new(MockProvider::new(vec![]));
    let mut agent = Agent::new(
        provider,
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::from("ctx-line"),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.messages.push(test_user_message("old turn"));
    agent.usage.last_usage = Some(crate::provider::TokenUsage {
        prompt_tokens: 10,
        completion_tokens: 5,
        input_cache: Some(InputCacheUsage::new(2, 1, 10)),
    });
    agent.usage.prompt_tokens = 10;
    agent.usage.completion_tokens = 5;
    agent.usage.cache_read_input_tokens = 2;
    agent.usage.cache_creation_input_tokens = 1;
    agent.usage.cache_measured_input_tokens = 10;

    agent.clear().await;

    assert_eq!(agent.messages.len(), 1);
    let system_text = message_content(&agent.messages[0]);
    assert!(system_text.contains("ctx-line"));
    assert_eq!(agent.usage.last_usage, None);
    assert_eq!(agent.usage.prompt_tokens, 0);
    assert_eq!(agent.usage.completion_tokens, 0);
    assert_eq!(agent.context_report().session_input_cache, None);
}

#[tokio::test]
async fn review_pending_changes_seeds_diff_context() {
    let fixture = TestFixture::new();
    let root = &fixture.project_root;
    init_repo(root);
    commit_file(root, "lib.rs", "fn old() {}\n", "baseline");
    std::fs::write(root.join("lib.rs"), "fn new() {}\n").unwrap();

    let mut agent = review_agent(&fixture, String::from("ctx-line"));
    // Pre-seed an old conversation so we can verify it's reset.
    agent
        .messages
        .push(test_user_message("ignore me: old turn"));

    let has_changes = agent.review_pending_changes(ReviewScope::Uncommitted).await;
    assert!(has_changes, "working tree has changes to review");
    assert_eq!(agent.mode, AgentMode::Review);

    // Conversation: exactly two messages — system + user.
    assert_eq!(agent.messages.len(), 2);
    let system_text = system_content(agent.mode, "ctx-line");
    assert!(
        system_text.contains("ctx-line"),
        "system message should be rebuilt with project context, got: {system_text:?}"
    );
    let user_text = user_message_content(&agent.messages[1]);
    assert!(
        user_text.contains("Review the pending changes (Uncommitted changes: git diff HEAD)"),
        "user message should carry the review prompt and command, got: {user_text:?}"
    );
    assert!(
        user_text.contains("Changed files:\nlib.rs |"),
        "user message should include the stat file list, got: {user_text:?}"
    );
    assert!(
        user_text.contains("Full diff (git diff HEAD):"),
        "user message should label the full diff with the command, got: {user_text:?}"
    );
    assert!(
        user_text.contains("fn new"),
        "user message should embed the git diff, got: {user_text:?}"
    );
    assert!(
        user_text.contains("For each changed file, read the file"),
        "user message should instruct per-file reads, got: {user_text:?}"
    );
    assert!(
        user_text.contains("do not run commands"),
        "user message should keep review truncation handling read-only, got: {user_text:?}"
    );
    assert!(
        user_text.contains("Base findings on evidence"),
        "user message should require evidence-based findings, got: {user_text:?}"
    );
    assert!(
        user_text.contains("Report findings ordered by severity: Blocker (must fix before merge)"),
        "user message should define review severities, got: {user_text:?}"
    );
    assert!(
        user_text.contains("Do not modify files"),
        "user message should instruct the agent not to modify, got: {user_text:?}"
    );
}

#[tokio::test]
async fn security_review_pending_changes_seeds_curated_read_only_context() {
    let fixture = TestFixture::new();
    let root = &fixture.project_root;
    init_repo(root);
    commit_file(root, "auth.rs", "fn authorize() {}\n", "baseline");
    std::fs::write(root.join("auth.rs"), "fn authorize_effect() {}\n").unwrap();

    let mut agent = review_agent(&fixture, String::from("ctx-line"));
    let has_changes = agent
        .security_review_pending_changes(ReviewScope::Uncommitted)
        .await;

    assert!(has_changes);
    assert_eq!(agent.mode, AgentMode::Review);
    assert_eq!(agent.messages.len(), 2);
    let prompt = user_message_content(&agent.messages[1]);
    assert!(prompt.contains("read-only security reviewer"), "{prompt}");
    assert!(
        prompt.contains("shared pre-effect authorization verdict"),
        "{prompt}"
    );
    assert!(prompt.contains("credentials, tokens, logs"), "{prompt}");
    assert!(prompt.contains("language-specific"), "{prompt}");
    assert!(prompt.contains("fn authorize_effect"), "{prompt}");
    assert!(prompt.contains("do not modify files"), "{prompt}");
}

#[tokio::test]
async fn review_pending_changes_uses_read_only_review_registry() {
    let fixture = TestFixture::new();
    let root = &fixture.project_root;
    init_repo(root);
    commit_file(root, "lib.rs", "fn old() {}\n", "baseline");
    std::fs::write(root.join("lib.rs"), "fn new() {}\n").unwrap();

    let mut agent = Agent::new(
        MockProvider::empty(),
        mock_registry(&["read", "write", "edit", "bash", "todowrite", "tasks"]),
        mock_registry(&[
            "project_info",
            "read",
            "glob",
            "grep",
            "symbol_search",
            "skill",
            "question",
            "plan_set_title",
        ]),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    let has_changes = agent.review_pending_changes(ReviewScope::Uncommitted).await;

    assert!(has_changes, "working tree has changes to review");
    assert_eq!(agent.mode, AgentMode::Review);
    for tool in [
        "project_info",
        "read",
        "glob",
        "grep",
        "symbol_search",
        "skill",
        "question",
    ] {
        assert!(
            agent.tool_registry.get(tool).is_some(),
            "review registry should include {tool}"
        );
    }
    for tool in [
        "write",
        "edit",
        "bash",
        "todowrite",
        "tasks",
        "plan_set_title",
    ] {
        assert!(
            agent.tool_registry.get(tool).is_none(),
            "review registry must not expose {tool}"
        );
    }
}

#[tokio::test]
async fn versus_master_uses_main_when_master_is_absent() {
    let fixture = TestFixture::new();
    let root = &fixture.project_root;
    init_repo(root);
    run_git(root, &["checkout", "-b", "main"]);
    commit_file(root, "lib.rs", "fn base() {}\n", "baseline");
    run_git(root, &["checkout", "-b", "feature"]);
    std::fs::write(root.join("lib.rs"), "fn feature() {}\n").unwrap();
    run_git(root, &["add", "lib.rs"]);
    run_git(root, &["commit", "--quiet", "-m", "feature"]);

    let mut agent = review_agent(&fixture, String::new());
    let has_changes = agent
        .review_pending_changes(ReviewScope::VersusMaster)
        .await;

    assert!(has_changes, "main-only repo should produce a diff");
    let user_text = user_message_content(&agent.messages[1]);
    assert!(
        user_text.contains("Diff vs main branch: git diff main...HEAD"),
        "prompt should name the resolved main command, got: {user_text:?}"
    );
    assert!(
        user_text.contains("fn feature"),
        "prompt should include the branch diff, got: {user_text:?}"
    );
}

#[tokio::test]
async fn last_commit_falls_back_to_root_for_single_commit_repo() {
    let fixture = TestFixture::new();
    let root = &fixture.project_root;
    init_repo(root);
    commit_file(root, "lib.rs", "fn initial() {}\n", "initial");

    let mut agent = review_agent(&fixture, String::new());
    let has_changes = agent.review_pending_changes(ReviewScope::LastCommit).await;

    assert!(has_changes, "single-commit repo should diff against root");
    let user_text = user_message_content(&agent.messages[1]);
    assert!(
        user_text.contains("Last commit: git show --root --format= --patch HEAD"),
        "prompt should name the single-commit fallback, got: {user_text:?}"
    );
    assert!(
        user_text.contains("Changed files:\nlib.rs |"),
        "prompt should include stat output for the root commit, got: {user_text:?}"
    );
    assert!(
        user_text.contains("Full diff (git show --root --format= --patch HEAD):"),
        "prompt should label the root commit patch command, got: {user_text:?}"
    );
    assert!(
        user_text.contains("fn initial"),
        "prompt should include initial commit content, got: {user_text:?}"
    );
    assert_eq!(
        user_text.matches("diff --git a/lib.rs b/lib.rs").count(),
        1,
        "stat capture should not duplicate the patch body, got: {user_text:?}"
    );
}

#[tokio::test]
async fn untracked_file_appears_in_review_prompt() {
    let fixture = TestFixture::new();
    let root = &fixture.project_root;
    init_repo(root);
    commit_file(root, "lib.rs", "fn base() {}\n", "baseline");
    std::fs::write(root.join("new_file.rs"), "fn untracked() {}\n").unwrap();

    let mut agent = review_agent(&fixture, String::new());
    let has_changes = agent.review_pending_changes(ReviewScope::Uncommitted).await;

    assert!(
        has_changes,
        "untracked file should count as reviewable changes"
    );
    let user_text = user_message_content(&agent.messages[1]);
    assert!(
        user_text.contains("New files created this session (untracked, so absent from the diff above — they ARE part of the changes under review; read each in full):\nnew_file.rs"),
        "prompt should list untracked files, got: {user_text:?}"
    );
    assert!(
        user_text.contains("For untracked files listed below the diff, read them in full"),
        "prompt should instruct reading untracked files, got: {user_text:?}"
    );
}

#[tokio::test]
async fn unborn_repo_review_includes_staged_and_untracked_files() {
    let fixture = TestFixture::new();
    let root = &fixture.project_root;
    init_repo(root);
    std::fs::write(root.join("staged.rs"), "fn staged_index() {}\n").unwrap();
    std::fs::write(root.join("untracked.rs"), "fn untracked() {}\n").unwrap();
    run_git(root, &["add", "staged.rs"]);
    std::fs::write(root.join("staged.rs"), "fn staged_worktree() {}\n").unwrap();

    let mut agent = review_agent(&fixture, String::new());
    let has_changes = agent.review_pending_changes(ReviewScope::Uncommitted).await;

    assert!(has_changes, "unborn repo changes should be reviewable");
    let user_text = user_message_content(&agent.messages[1]);
    assert!(
        user_text.contains("git diff --cached"),
        "prompt should use an unborn-HEAD-safe diff command, got: {user_text:?}"
    );
    assert!(
        user_text.contains("staged.rs"),
        "prompt should include staged file diff/stat output, got: {user_text:?}"
    );
    assert!(
        user_text.contains("fn staged_index"),
        "prompt should include staged file contents in diff, got: {user_text:?}"
    );
    assert!(
        user_text.contains("fn staged_worktree"),
        "prompt should include unstaged worktree edits in diff, got: {user_text:?}"
    );
    assert!(
        user_text.contains("New files created this session (untracked, so absent from the diff above — they ARE part of the changes under review; read each in full):\nuntracked.rs"),
        "prompt should include untracked files, got: {user_text:?}"
    );
}

#[tokio::test]
async fn large_diff_keeps_stat_when_body_is_truncated() {
    let fixture = TestFixture::new();
    let root = &fixture.project_root;
    init_repo(root);
    commit_file(root, "first.rs", "fn first() {}\n", "baseline");
    let large = (0..6000)
        .map(|line| format!("fn changed_{line}() {{}}\n"))
        .collect::<String>();
    std::fs::write(root.join("first.rs"), large).unwrap();
    std::fs::write(root.join("later.rs"), "fn later() {}\n").unwrap();

    let mut agent = review_agent(&fixture, String::new());
    let has_changes = agent.review_pending_changes(ReviewScope::Uncommitted).await;

    assert!(has_changes, "large working tree should have changes");
    let user_text = user_message_content(&agent.messages[1]);
    assert!(
        user_text.contains("first.rs |"),
        "stat should include the large changed file, got: {user_text:?}"
    );
    assert!(
        user_text.contains("New files created this session (untracked, so absent from the diff above — they ARE part of the changes under review; read each in full):\nlater.rs"),
        "untracked list should survive diff truncation, got: {user_text:?}"
    );
    assert!(
        user_text.contains("…(truncated)"),
        "large diff body should be truncated, got: {user_text:?}"
    );
}

#[tokio::test]
async fn review_pending_changes_reports_no_changes() {
    let fixture = TestFixture::new();
    let mut agent = review_agent(&fixture, String::new());
    agent
        .messages
        .push(test_user_message("keep this active conversation"));
    let prior_messages = agent.messages.clone();

    // No git repo in the temp dir: no changes to review.
    let has_changes = agent.review_pending_changes(ReviewScope::Uncommitted).await;
    assert!(!has_changes, "bare dir has no changes to review");
    assert_eq!(agent.mode, AgentMode::Coding);
    assert_eq!(
        agent.messages, prior_messages,
        "empty review scope must not reset the active conversation"
    );
}
