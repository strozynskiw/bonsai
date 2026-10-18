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
    agent
        .goal
        .transition(&record.id, GoalState::Paused, Some("Paused by user."))
        .await
        .unwrap();
    agent
        .finish_goal_run(&record.id, &Ok(AgentRunResult::Completed("done".into())))
        .await
        .unwrap();
    assert_eq!(
        agent.goal.snapshot().await.unwrap().state,
        GoalState::Paused
    );
}
