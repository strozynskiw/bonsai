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
async fn paused_and_resumed_goal_rejects_old_provider_response() {
    struct PauseResumeProvider {
        goal: crate::goal::GoalService,
        id: String,
    }

    #[async_trait]
    impl Provider for PauseResumeProvider {
        async fn chat_stream(
            &self,
            _messages: &[ChatCompletionRequestMessage],
            _tools: &[ChatCompletionTool],
            _cancellation_token: CancellationToken,
            _sink: SharedSink,
        ) -> crate::provider::ProviderResult<StreamedResponse> {
            self.goal
                .transition(&self.id, GoalState::Paused, None)
                .await
                .unwrap();
            self.goal
                .transition(&self.id, GoalState::Active, None)
                .await
                .unwrap();
            Ok(StreamedResponse {
                tool_calls: vec![test_tool_call("stale", "read", r#"{"path":"file.txt"}"#)],
                ..StreamedResponse::default()
            })
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    let fixture = TestFixture::new();
    std::fs::write(fixture.project_root.join("file.txt"), "content").unwrap();
    let goal = crate::goal::GoalService::default();
    let record = goal.set("Review file.txt").await.unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(crate::tool::ReadTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    let registry = Arc::new(registry);
    let mut agent = Agent::new(
        Box::new(PauseResumeProvider {
            goal: goal.clone(),
            id: record.id.clone(),
        }),
        registry.clone(),
        registry,
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.goal = goal.clone();
    let sink = Arc::new(CaptureSink::default());
    let result = agent
        .run("Review file.txt", CancellationToken::new(), sink.clone())
        .await
        .unwrap();
    assert!(matches!(result, AgentRunResult::Interrupted(_)));
    assert!(
        sink.tool_starts().is_empty(),
        "stale tool call must not execute"
    );
    let current = goal.snapshot().await.unwrap();
    assert_eq!(current.id, record.id);
    assert_eq!(current.state, GoalState::Active);
}

#[tokio::test]
async fn progress_reply_cannot_complete_a_goal_without_an_explicit_claim() {
    let fixture = TestFixture::new();
    let responses = (0..3)
        .map(|_| StreamedResponse {
            content: "Here is the progress so far.".into(),
            ..StreamedResponse::default()
        })
        .collect();
    let mut agent = goal_agent(&fixture, responses);
    agent.goal.set("Explain the project").await.unwrap();
    let result = agent
        .run(
            "Explain the project",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();
    assert!(matches!(result, AgentRunResult::Incomplete { .. }));
    assert_eq!(
        agent.goal.snapshot().await.unwrap().state,
        GoalState::Paused
    );
    let trace = agent.completion_guard_trace().unwrap();
    assert!(
        trace
            .attempts
            .last()
            .unwrap()
            .gaps
            .contains(&crate::agent::CompletionGap::GoalCompletionUnconfirmed)
    );
}

#[tokio::test]
async fn completion_claim_must_be_standalone_and_follow_the_work() {
    let fixture = TestFixture::new();
    std::fs::write(fixture.project_root.join("file.txt"), "content\n").unwrap();
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(crate::tool::ReadTool::new(
        fixture.project_root.clone(),
        fixture.read_tracker.clone(),
    )));
    let registry = Arc::new(registry);
    let claim = || test_tool_call("claim", "goal_complete", r#"{"evidence":"File reviewed"}"#);
    let responses = vec![
        StreamedResponse {
            tool_calls: vec![
                claim(),
                test_tool_call("read", "read", r#"{"path":"file.txt"}"#),
            ],
            ..StreamedResponse::default()
        },
        StreamedResponse {
            content: "Reviewed the file.".into(),
            ..StreamedResponse::default()
        },
        StreamedResponse {
            tool_calls: vec![claim()],
            ..StreamedResponse::default()
        },
        StreamedResponse {
            content: "Reviewed the file.".into(),
            ..StreamedResponse::default()
        },
    ];
    let provider = MockProvider::new(responses.into_iter().map(Ok).collect());
    let tool_names = provider.tool_names();
    let mut agent = Agent::new(
        Box::new(provider),
        registry.clone(),
        registry,
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();
    agent.goal.set("Review file.txt").await.unwrap();
    let result = agent
        .run(
            "Review file.txt",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();
    assert!(matches!(result, AgentRunResult::Completed(_)));
    let trace = agent.completion_guard_trace().unwrap();
    assert!(
        trace.attempts[0]
            .gaps
            .contains(&crate::agent::CompletionGap::GoalCompletionUnconfirmed)
    );
    assert_eq!(
        agent.goal.snapshot().await.unwrap().state,
        GoalState::Complete
    );
    assert!(
        tool_names
            .lock()
            .await
            .iter()
            .all(|names| names.iter().any(|name| name == "goal_complete"))
    );
    assert!(
        !agent
            .active_tool_schema()
            .names()
            .iter()
            .any(|name| name == "goal_complete")
    );
}

#[tokio::test]
async fn completion_claim_alone_does_not_count_as_requested_work() {
    let fixture = TestFixture::new();
    let responses = vec![
        StreamedResponse {
            tool_calls: vec![test_tool_call(
                "claim",
                "goal_complete",
                r#"{"evidence":"Done"}"#,
            )],
            ..StreamedResponse::default()
        },
        StreamedResponse {
            content: "Done.".into(),
            ..StreamedResponse::default()
        },
        StreamedResponse {
            content: "Done.".into(),
            ..StreamedResponse::default()
        },
    ];
    let mut agent = goal_agent(&fixture, responses);
    agent.goal.set("Review the files").await.unwrap();
    let result = agent
        .run(
            "Review the files",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();
    assert!(matches!(result, AgentRunResult::Incomplete { .. }));
    let trace = agent.completion_guard_trace().unwrap();
    assert!(
        trace
            .attempts
            .last()
            .unwrap()
            .gaps
            .contains(&crate::agent::CompletionGap::RequestedActionMissing)
    );
}

#[tokio::test]
async fn goal_requires_a_current_claim_and_new_steering_revokes_it() {
    use crate::agent::completion::CompletionGuardVerdict;

    let fixture = TestFixture::new();
    let mut agent = goal_agent(&fixture, vec![]);
    let goal = agent.goal.set("Review all four files").await.unwrap();
    let lease = agent.goal.active_run_lease().await.unwrap();
    agent.goal_run_id = Some(goal.id);
    agent.goal_run_lease = Some(lease.clone());
    agent.begin_inferred_completion_task("Review all four files");
    agent.note_completion_action();
    assert!(matches!(
        agent.completion_guard_verdict("Reviewed the files.").await,
        CompletionGuardVerdict::Continue { .. }
    ));
    agent.goal.claim_completion(&lease).await.unwrap();
    assert!(matches!(
        agent
            .completion_guard_verdict("Reviewed all four files.")
            .await,
        CompletionGuardVerdict::Accept
    ));
    agent.note_completion_workspace_mutation();
    assert!(!agent.goal.completion_claimed(&lease).await);
    agent.goal.claim_completion(&lease).await.unwrap();
    agent.merge_completion_steering("Also check the tests.");
    assert!(!agent.goal.completion_claimed(&lease).await);
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
async fn goal_recovery_continues_with_progress_but_bounds_repeated_status() {
    use crate::agent::completion::CompletionGuardVerdict;
    use crate::completion_report::CompletionStatus;

    let fixture = TestFixture::new();
    let mut agent = goal_agent(&fixture, vec![]);
    let goal = agent.goal.set("Review all four files").await.unwrap();
    let lease = agent.goal.active_run_lease().await.unwrap();
    agent.goal_run_id = Some(goal.id);
    agent.begin_inferred_completion_task("Review all four files");
    agent.note_completion_action();
    let partial = "The remaining files are unreviewed.";

    for _ in 0..4 {
        agent.note_goal_progress();
        assert!(matches!(
            agent.completion_guard_verdict(partial).await,
            CompletionGuardVerdict::Continue { .. }
        ));
    }
    for _ in 0..2 {
        assert!(matches!(
            agent.completion_guard_verdict(partial).await,
            CompletionGuardVerdict::Continue { .. }
        ));
    }
    let CompletionGuardVerdict::Reject(failure) = agent.completion_guard_verdict(partial).await
    else {
        panic!("stagnant goal must stop after the bounded recovery");
    };
    let trace = agent.completion_guard_trace().unwrap();
    assert_eq!(
        crate::goal::terminal_from_completion_guarded(CompletionStatus::Failed, None, Some(&trace)),
        (GoalState::Paused, Some(crate::goal::GOAL_STAGNATED.into()))
    );
    assert_eq!(
        crate::goal::terminal_from_completion_guarded(
            CompletionStatus::Blocked,
            None,
            Some(&trace)
        )
        .0,
        GoalState::Blocked
    );
    assert_eq!(
        crate::goal::terminal_from_completion_guarded(
            CompletionStatus::BudgetExhausted,
            Some(crate::run_budget::RunBudgetExhaustion::MaxTurns { limit: 1 }),
            Some(&trace),
        )
        .0,
        GoalState::BudgetLimited
    );
    let mut mixed = trace;
    mixed
        .attempts
        .last_mut()
        .unwrap()
        .gaps
        .push(crate::agent::CompletionGap::VerificationBlocked(
            "failed".into(),
        ));
    assert_eq!(
        crate::goal::terminal_from_completion_guarded(CompletionStatus::Failed, None, Some(&mixed))
            .0,
        GoalState::Blocked
    );
    agent
        .finish_goal_run(
            &lease,
            &Ok(AgentRunResult::Incomplete {
                output: failure.detail.clone(),
                failure,
            }),
        )
        .await
        .unwrap();
    let paused = agent.goal.snapshot().await.unwrap();
    assert_eq!(paused.state, GoalState::Paused);
    assert_eq!(paused.reason.as_deref(), Some(crate::goal::GOAL_STAGNATED));
    agent
        .goal
        .transition(&paused.id, GoalState::Active, None)
        .await
        .unwrap();
    assert_eq!(
        agent.goal.snapshot().await.unwrap().state,
        GoalState::Active
    );
}

#[tokio::test]
async fn deferred_goal_finalization_leaves_the_surface_to_classify_the_result() {
    let fixture = TestFixture::new();
    let responses = (0..3)
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
    let responses = (0..3)
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
    agent.goal_run_lease = agent.goal.active_run_lease().await;
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
