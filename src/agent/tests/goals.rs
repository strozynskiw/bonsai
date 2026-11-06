use super::*;
use crate::goal::GoalState;

fn goal_agent(fixture: &TestFixture, responses: Vec<StreamedResponse>) -> Agent {
    Agent::new(
        Box::new(MockProvider::new(responses.into_iter().map(Ok).collect())),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap()
}

#[tokio::test]
async fn explicit_unfinished_goal_work_triggers_recovery_before_completion() {
    use crate::agent::completion::{CompletionGap, CompletionGuardVerdict};

    let fixture = TestFixture::new();
    let mut agent = goal_agent(&fixture, vec![]);
    let goal = agent.goal.set("Review all four files").await.unwrap();
    agent.goal_run_id = Some(goal.id);
    agent.begin_inferred_completion_task("Review all four files");
    agent.note_completion_action();

    let verdict = agent
        .completion_guard_verdict("I reviewed README.md. The other three files remain unreviewed.")
        .await;
    assert!(matches!(verdict, CompletionGuardVerdict::Continue { .. }));
    let trace = agent.completion_guard_trace().unwrap();
    assert!(
        trace.attempts[0]
            .gaps
            .contains(&CompletionGap::GoalWorkRemaining)
    );
    assert!(matches!(
        agent
            .completion_guard_verdict("All four files were reviewed.")
            .await,
        CompletionGuardVerdict::Accept
    ));

    agent.goal_run_id = None;
    agent.begin_inferred_completion_task("Report the review progress");
    agent.note_completion_action();
    assert!(matches!(
        agent
            .completion_guard_verdict("Three files remain unreviewed.")
            .await,
        CompletionGuardVerdict::Accept
    ));

    let reporting_goal = agent.goal.set("List the unreviewed files").await.unwrap();
    agent.goal_run_id = Some(reporting_goal.id);
    agent.begin_inferred_completion_task("List the unreviewed files");
    agent.note_completion_action();
    assert!(matches!(
        agent
            .completion_guard_verdict("Three files remain unreviewed.")
            .await,
        CompletionGuardVerdict::Accept
    ));
}

#[tokio::test]
async fn deferred_goal_finalization_leaves_the_surface_to_classify_the_result() {
    let fixture = TestFixture::new();
    let responses = (0..2)
        .map(|_| StreamedResponse {
            content: "I will implement the feature next.".into(),
            ..StreamedResponse::default()
        })
        .collect();
    let mut agent = goal_agent(&fixture, responses);
    agent
        .goal
        .set("Implement a tested authentication feature")
        .await
        .unwrap();
    agent.defer_goal_finalization = true;
    let result = agent
        .run("Start", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();
    assert!(matches!(result, AgentRunResult::Incomplete { .. }));
    assert_eq!(
        agent.goal.snapshot().await.unwrap().state,
        GoalState::Active
    );
}

#[tokio::test]
async fn goal_rejects_progress_only_completion_and_stops_after_bounded_retry() {
    let fixture = TestFixture::new();
    let responses = (0..2)
        .map(|_| StreamedResponse {
            content: "I will implement the feature next.".into(),
            ..StreamedResponse::default()
        })
        .collect();
    let mut agent = goal_agent(&fixture, responses);
    agent
        .goal
        .set("Implement a tested authentication feature")
        .await
        .unwrap();
    let result = agent
        .run("Start", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();
    assert!(matches!(result, AgentRunResult::Incomplete { .. }));
    assert_eq!(
        agent.goal.snapshot().await.unwrap().state,
        GoalState::Blocked
    );
}

#[tokio::test]
async fn goal_budget_exhaustion_is_not_completion() {
    let fixture = TestFixture::new();
    let mut agent = goal_agent(
        &fixture,
        vec![StreamedResponse {
            content: "I will implement the feature next.".into(),
            ..StreamedResponse::default()
        }],
    );
    agent.goal.set("Implement a tested feature").await.unwrap();
    agent.set_run_max_turns(1);
    assert!(
        agent
            .run("Start", CancellationToken::new(), Arc::new(StdoutSink))
            .await
            .is_err()
    );
    assert_eq!(
        agent.goal.snapshot().await.unwrap().state,
        GoalState::BudgetLimited
    );
}

#[tokio::test]
async fn goal_note_is_reconstructed_after_compaction_and_provider_change() {
    let fixture = TestFixture::new();
    let mut agent = goal_agent(&fixture, vec![]);
    let objective = "Preserve every requirement, including the original acceptance criteria.";
    let record = agent.goal.set(objective).await.unwrap();
    agent.goal_run_id = Some(record.id.clone());
    agent.refresh_goal_context().await.unwrap();
    for index in 0..40 {
        agent.push_user_message_raw(&format!(
            "Old observation {index}: {}",
            "evidence ".repeat(100)
        ));
    }
    agent
        .compact_context(
            CompactionRequest::manual(false, CancellationToken::new())
                .with_summary_policy(crate::agent::CompactionSummaryPolicy::DeterministicOnly)
                .with_target_tokens(1),
        )
        .await
        .unwrap();
    agent.set_provider(
        MockProvider::empty(),
        128_000,
        PromptEstimator::default(),
        crate::agent::ActiveModelIdentity {
            provider_id: "openai-compatible".parse().unwrap(),
            model: "other-model".into(),
        },
    );
    assert!(agent.refresh_goal_context().await.unwrap());
    let text = agent
        .context_messages()
        .iter()
        .map(super::super::output::message_content_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains(objective));
    assert_eq!(agent.goal.snapshot().await.unwrap().id, record.id);
}

#[tokio::test]
async fn explicit_user_pause_wins_over_a_late_completion() {
    let fixture = TestFixture::new();
    let agent = goal_agent(&fixture, vec![]);
    let record = agent.goal.set("Ship it").await.unwrap();
    let lease = agent.goal.active_run_lease().await.unwrap();
    agent
        .goal
        .transition(&record.id, GoalState::Paused, Some("Paused by user."))
        .await
        .unwrap();
    agent
        .finish_goal_run(&lease, &Ok(AgentRunResult::Completed("done".into())))
        .await
        .unwrap();
    assert_eq!(
        agent.goal.snapshot().await.unwrap().state,
        GoalState::Paused
    );
}

#[tokio::test]
async fn late_wait_and_completion_cannot_override_pause_or_resume() {
    let fixture = TestFixture::new();
    let agent = goal_agent(&fixture, vec![]);
    let record = agent.goal.set("Ship it").await.unwrap();
    let old_run = agent.goal.active_run_lease().await.unwrap();
    agent
        .goal
        .transition(&record.id, GoalState::Paused, Some("Paused by user."))
        .await
        .unwrap();
    agent
        .finish_goal_run(
            &old_run,
            &Ok(AgentRunResult::Waiting(
                crate::agent::WaitReason::Subagents(vec!["sub-1".into()]),
            )),
        )
        .await
        .unwrap();
    assert_eq!(
        agent.goal.snapshot().await.unwrap().state,
        GoalState::Paused
    );
    agent
        .goal
        .transition(&record.id, GoalState::Active, None)
        .await
        .unwrap();
    agent
        .finish_goal_run(&old_run, &Ok(AgentRunResult::Completed("stale".into())))
        .await
        .unwrap();
    assert_eq!(
        agent.goal.snapshot().await.unwrap().state,
        GoalState::Active
    );
}
