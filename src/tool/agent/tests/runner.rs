use super::*;

#[tokio::test]
async fn explore_returns_subagent_conclusion() {
    let tool = agent_tool();
    let result = tool
        .execute(serde_json::json!({ "agent": "explore", "prompt": "where is main" }))
        .await
        .unwrap();
    let ToolOutput::TextWithUsage {
        text,
        usage,
        usage_turns,
        workspace_effect,
        ..
    } = result
    else {
        panic!("agent tool should return text with usage");
    };
    assert_eq!(text, "Found it at src/lib.rs:1.");
    assert_eq!(usage.prompt_tokens, 11);
    assert_eq!(usage.completion_tokens, 7);
    assert_eq!(usage_turns.len(), 1);
    assert_eq!(usage_turns[0].lane_kind, ExecutionLaneKind::Subagent);
    assert_eq!(usage_turns[0].lane_id, "sub-1");
    assert_eq!(usage_turns[0].lane_seq, 1);
    assert_eq!(usage_turns[0].provider_id.as_deref(), Some("test-provider"));
    assert_eq!(usage_turns[0].model.as_deref(), Some("test-model"));
    assert_eq!(
        workspace_effect,
        crate::tool::ToolWorkspaceEffect::NoMutation
    );
}

#[tokio::test]
async fn out_of_root_delegation_fails_before_starting_subagent() {
    let project = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    let runner = SubagentRunner::new(
        factory(),
        empty_sub_registry(),
        empty_sub_registry(),
        ReadTracker::new(),
        Arc::new(ProjectInfoRuntime::new(None)),
        Arc::new(SubagentRegistry::new()),
        project.path().to_path_buf(),
    );
    let tool = AgentTool::new(
        runner,
        crate::resource::agent::shared_registry(Arc::new(AgentRegistry::empty())),
    );

    let error = tool
        .execute(serde_json::json!({
            "agent": "explore",
            "prompt": format!("inspect `{}:9`", outside.path().display()),
        }))
        .await
        .expect_err("out-of-root delegation must fail before launch");

    let message = error.to_string();
    assert!(
        message.contains("outside the current project root"),
        "{message}"
    );
    assert!(
        message.contains("Launch Bonsai from that project"),
        "{message}"
    );
    assert!(tool.runner.subagents().list().is_empty());
}

#[test]
fn path_preflight_allows_existing_paths_inside_project() {
    let project = tempfile::TempDir::new().unwrap();
    let source = project.path().join("src/main.rs");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "fn main() {}\n").unwrap();

    let prompt = format!("inspect `{}:12:4`", source.display());
    assert_eq!(
        outside_project_path_in_prompt(&prompt, project.path()),
        None
    );
}

#[test]
fn root_slash_is_not_flagged() {
    let project = tempfile::TempDir::new().unwrap();
    let prompt = "look at / for the filesystem root";
    assert_eq!(outside_project_path_in_prompt(prompt, project.path()), None);
}

#[test]
fn top_level_users_is_not_flagged() {
    let project = tempfile::TempDir::new().unwrap();
    let prompt = "check /Users directory";
    assert_eq!(outside_project_path_in_prompt(prompt, project.path()), None);
}

#[test]
fn outside_path_still_rejected() {
    let project = tempfile::TempDir::new().unwrap();
    let prompt = "read /etc/passwd";
    let matched = outside_project_path_in_prompt(prompt, project.path());
    assert!(matched.is_some(), "must reject outside path /etc/passwd");
}

#[test]
fn source_location_outside_still_rejected() {
    let project = tempfile::TempDir::new().unwrap();
    let prompt = "inspect `/etc/hosts:1:5`";
    let matched = outside_project_path_in_prompt(prompt, project.path());
    assert!(matched.is_some(), "must reject outside path /etc/hosts");
}

#[tokio::test]
async fn read_only_agent_can_run_in_background() {
    let tool =
        agent_tool_with_background_wake(empty_sub_registry(), Arc::new(AgentRegistry::empty()));
    let result = tool
        .execute(serde_json::json!({
            "agent": "explore",
            "prompt": "where is main",
            "run_in_background": true
        }))
        .await
        .unwrap();

    let ToolOutput::SubagentStarted {
        subtask_id,
        message,
    } = result
    else {
        panic!("expected background subagent start");
    };
    assert!(message.contains(&subtask_id));
    tool.runner
        .subagents()
        .attach_tool_call(&subtask_id, "call-1")
        .expect("background completion should be attached to its parent tool call");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if tool
                .runner
                .subagents()
                .snapshot(&subtask_id)
                .is_some_and(|snapshot| snapshot.status.is_finished())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let completed = tool.runner.subagents().drain_completed_for_agent();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].snapshot.id.as_ref(), subtask_id);
    assert_eq!(completed[0].usage_turns[0].lane_id, subtask_id);
    assert_eq!(
        completed[0].usage_turns[0].parent_tool_call_id.as_deref(),
        Some("call-1")
    );
    assert_eq!(
        completed[0].snapshot.result.as_deref(),
        Some("Found it at src/lib.rs:1.")
    );
}

#[tokio::test]
async fn billed_token_cap_rejects_background_agent_launch() {
    let tool =
        agent_tool_with_background_wake(empty_sub_registry(), Arc::new(AgentRegistry::empty()));
    let context = crate::tool::ToolExecutionContext::new(
        "call-1".to_string(),
        Arc::new(crate::output::StdoutSink),
    )
    .with_remaining_billed_tokens(Some(100));

    let error = tool
        .execute_with_context(
            serde_json::json!({
                "agent": "explore",
                "prompt": "where is main",
                "run_in_background": true
            }),
            context,
        )
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("cumulative billed-token hard limit"));
    assert!(tool.runner.subagents().list().is_empty());
}

#[tokio::test]
async fn background_without_wake_loop_runs_synchronously() {
    let tool = agent_tool();
    let result = tool
        .execute(serde_json::json!({
            "agent": "explore",
            "prompt": "where is main",
            "run_in_background": true
        }))
        .await
        .unwrap();

    let ToolOutput::TextWithUsage {
        text,
        usage,
        usage_turns,
        ..
    } = result
    else {
        panic!("agent tool should fall back to synchronous execution");
    };
    assert_eq!(text, "Found it at src/lib.rs:1.");
    assert_eq!(usage.prompt_tokens, 11);
    assert_eq!(usage.completion_tokens, 7);
    assert_eq!(usage_turns.len(), 1);

    let runs = tool.runner.subagents().list();
    assert_eq!(runs.len(), 1);
    assert!(!runs[0].detached);
    assert!(
        tool.runner
            .subagents()
            .drain_completed_for_agent()
            .is_empty()
    );
}

#[tokio::test]
async fn mutating_agent_cannot_run_in_background() {
    let custom = custom_registry(&[(
        "fixer",
        "---\nname: fixer\ndescription: d\ntools: [bash]\n---\nprompt",
    )]);
    let runner = test_runner_with_full_registry(
        sub_registry_with(&["read"]),
        sub_registry_with(&["read", "bash"]),
    )
    .with_background_wake();
    let tool = AgentTool::new(runner, crate::resource::agent::shared_registry(custom));

    let err = tool
        .execute(serde_json::json!({
            "agent": "fixer",
            "prompt": "change it",
            "run_in_background": true
        }))
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("run_in_background is only supported for read-only subagents"));
}

#[tokio::test]
async fn dropping_subagent_run_cancels_provider_token() {
    let started = Arc::new(tokio::sync::Notify::new());
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let factory: SubagentProviderFactory = {
        let started = started.clone();
        let cancelled = cancelled.clone();
        Arc::new(move |_agent: String, _chain: SubagentModelChain| {
            let started = started.clone();
            let cancelled = cancelled.clone();
            Box::pin(async move {
                SubagentProviderConfig::new(
                    Box::new(SpawnedCancellationProvider { started, cancelled }),
                    DEFAULT_CONTEXT_WINDOW_TOKENS as usize,
                    PromptEstimator::heuristic(),
                    "test-model".to_string(),
                )
            })
        })
    };
    let runner = SubagentRunner::new(
        factory,
        empty_sub_registry(),
        empty_sub_registry(),
        ReadTracker::new(),
        Arc::new(ProjectInfoRuntime::new(None)),
        Arc::new(SubagentRegistry::new()),
        std::env::temp_dir(),
    );
    let handle = tokio::spawn(async move {
        runner
            .run_new(
                test_run_spec(
                    "explore",
                    "read only",
                    "wait",
                    empty_sub_registry(),
                    BuiltinSubagentBudget::Explore.limits(),
                ),
                CancellationToken::new(),
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("provider should start");
    handle.abort();
    let _ = handle.await;
    tokio::time::timeout(Duration::from_secs(1), cancelled.notified())
        .await
        .expect("dropping the subagent run must cancel its provider token");
}

#[tokio::test]
async fn cancel_all_running_cancels_provider_and_subtask_record() {
    let started = Arc::new(tokio::sync::Notify::new());
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let factory: SubagentProviderFactory = {
        let started = started.clone();
        let cancelled = cancelled.clone();
        Arc::new(move |_agent: String, _chain: SubagentModelChain| {
            let started = started.clone();
            let cancelled = cancelled.clone();
            Box::pin(async move {
                SubagentProviderConfig::new(
                    Box::new(CancellableProvider { started, cancelled }),
                    DEFAULT_CONTEXT_WINDOW_TOKENS as usize,
                    PromptEstimator::heuristic(),
                    "test-model".to_string(),
                )
            })
        })
    };
    let subagents = Arc::new(SubagentRegistry::new());
    let runner = SubagentRunner::new(
        factory,
        empty_sub_registry(),
        empty_sub_registry(),
        ReadTracker::new(),
        Arc::new(ProjectInfoRuntime::new(None)),
        subagents.clone(),
        std::env::temp_dir(),
    );
    let runner_for_task = runner.clone();
    let handle = tokio::spawn(async move {
        runner_for_task
            .run_new(
                test_run_spec(
                    "explore",
                    "read only",
                    "wait",
                    empty_sub_registry(),
                    BuiltinSubagentBudget::Explore.limits(),
                ),
                CancellationToken::new(),
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("provider should start");
    let snap = subagents
        .list()
        .into_iter()
        .next()
        .expect("subtask should be registered");
    assert_eq!(snap.status, SubagentStatus::Running);

    assert_eq!(runner.cancel_all_running(), 1);
    tokio::time::timeout(Duration::from_secs(1), cancelled.notified())
        .await
        .expect("runner cancellation must reach provider token");

    let (result, _usage, _turns, _evidence) = tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect("cancelled subagent should finish promptly")
        .expect("subagent task should not panic");
    let err = result.expect_err("cancelled subagent should return an error");
    assert!(err.to_string().contains("cancelled"), "{err:#}");
    assert_eq!(
        subagents.snapshot(&snap.id).unwrap().status,
        SubagentStatus::Cancelled
    );
    assert_eq!(runner.cancel_all_running(), 0);
}

#[tokio::test]
async fn cancel_all_running_reaches_background_subagents() {
    // A `run_in_background` subagent is detached, but a user cancel must still
    // stop it — regression for background runs surviving cancellation because
    // they were never tracked in `active_runs`.
    let started = Arc::new(tokio::sync::Notify::new());
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let factory: SubagentProviderFactory = {
        let started = started.clone();
        let cancelled = cancelled.clone();
        Arc::new(move |_agent: String, _chain: SubagentModelChain| {
            let started = started.clone();
            let cancelled = cancelled.clone();
            Box::pin(async move {
                SubagentProviderConfig::new(
                    Box::new(CancellableProvider { started, cancelled }),
                    DEFAULT_CONTEXT_WINDOW_TOKENS as usize,
                    PromptEstimator::heuristic(),
                    "test-model".to_string(),
                )
            })
        })
    };
    let subagents = Arc::new(SubagentRegistry::new());
    let runner = SubagentRunner::new(
        factory,
        empty_sub_registry(),
        empty_sub_registry(),
        ReadTracker::new(),
        Arc::new(ProjectInfoRuntime::new(None)),
        subagents.clone(),
        std::env::temp_dir(),
    );

    let subtask_id = runner.run_in_background(
        "explore",
        "read only",
        "wait",
        empty_sub_registry(),
        SubagentModelChain::default(),
        BuiltinSubagentBudget::Explore.limits(),
    );

    // Cancellation must see the run immediately, even before the spawned
    // future gets its first poll and starts the provider.
    assert_eq!(runner.cancel_all_running(), 1);
    assert_eq!(
        subagents.snapshot(&subtask_id).unwrap().status,
        SubagentStatus::Cancelled
    );
    tokio::task::yield_now().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), started.notified())
            .await
            .is_err(),
        "a run cancelled before its first poll must not start the provider"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), cancelled.notified())
            .await
            .is_err(),
        "an unstarted provider has no cancellation callback"
    );
}

#[tokio::test]
async fn explore_iteration_budget_gets_one_tool_disabled_conclude_turn() {
    use std::sync::atomic::Ordering;

    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let saw_conclude = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_tools_disabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let factory: SubagentProviderFactory = {
        let calls = calls.clone();
        let saw_conclude = saw_conclude.clone();
        let saw_tools_disabled = saw_tools_disabled.clone();
        Arc::new(move |_agent, _override| {
            let provider = ConcludeAfterBudgetProvider {
                calls: calls.clone(),
                saw_conclude: saw_conclude.clone(),
                saw_tools_disabled: saw_tools_disabled.clone(),
                max_iterations: EXPLORE_MAX_ITERATIONS,
            };
            Box::pin(async move {
                SubagentProviderConfig::new(
                    Box::new(provider),
                    DEFAULT_CONTEXT_WINDOW_TOKENS as usize,
                    PromptEstimator::heuristic(),
                    "test-model".to_string(),
                )
            })
        })
    };
    let runner = SubagentRunner::new(
        factory,
        sub_registry_with(&["read"]),
        sub_registry_with(&["read"]),
        ReadTracker::new(),
        Arc::new(ProjectInfoRuntime::new(None)),
        Arc::new(SubagentRegistry::new()),
        std::env::temp_dir(),
    );

    let (result, _, _, _) = runner
        .run_new(
            test_run_spec(
                "explore",
                "explore",
                "task",
                sub_registry_with(&["read"]),
                BuiltinSubagentBudget::Explore.limits(),
            ),
            CancellationToken::new(),
        )
        .await;

    let conclusion = result.expect("conclude turn should return a review conclusion");
    assert_eq!(conclusion, "Major: src/lib.rs:1 is incorrect.");
    assert_eq!(calls.load(Ordering::SeqCst), EXPLORE_MAX_ITERATIONS + 1);
    assert!(saw_conclude.load(Ordering::SeqCst));
    assert!(saw_tools_disabled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn self_review_timeout_gets_one_bounded_conclude_turn() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory: SubagentProviderFactory = {
        let calls = calls.clone();
        Arc::new(move |_agent, _override| {
            let provider = ConcludeAfterTimeoutProvider {
                calls: calls.clone(),
            };
            Box::pin(async move {
                SubagentProviderConfig::new(
                    Box::new(provider),
                    DEFAULT_CONTEXT_WINDOW_TOKENS as usize,
                    PromptEstimator::heuristic(),
                    "test-model".to_string(),
                )
            })
        })
    };
    let runner = SubagentRunner::new(
        factory,
        empty_sub_registry(),
        empty_sub_registry(),
        ReadTracker::new(),
        Arc::new(ProjectInfoRuntime::new(None)),
        Arc::new(SubagentRegistry::new()),
        std::env::temp_dir(),
    );
    let result = runner
        .run_new(
            SubagentRunSpec {
                label: "self-review-timeout-test".to_string(),
                instructions: "review".to_string(),
                task: "task".to_string(),
                registry: empty_sub_registry(),
                model_chain: SubagentModelChain::default(),
                lane_kind: ExecutionLaneKind::SelfReview,
                limits: SubagentRunLimits {
                    max_iterations: 8,
                    timeout: Duration::from_millis(10),
                    conclude_timeout: Duration::from_secs(1),
                },
            },
            CancellationToken::new(),
        )
        .await
        .0;

    assert_eq!(result.unwrap_or_default(), "The changes look correct.");
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "one timed-out attempt plus one conclude turn"
    );
}

#[tokio::test]
async fn conclude_timeout_returns_bounded_partial_evidence() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory: SubagentProviderFactory = {
        let calls = calls.clone();
        Arc::new(move |_agent, _override| {
            let provider = NeverConcludeProvider {
                calls: calls.clone(),
            };
            Box::pin(async move {
                SubagentProviderConfig::new(
                    Box::new(provider),
                    DEFAULT_CONTEXT_WINDOW_TOKENS as usize,
                    PromptEstimator::heuristic(),
                    "test-model".to_string(),
                )
            })
        })
    };
    let registry = Arc::new(SubagentRegistry::new());
    let runner = SubagentRunner::new(
        factory,
        empty_sub_registry(),
        empty_sub_registry(),
        ReadTracker::new(),
        Arc::new(ProjectInfoRuntime::new(None)),
        registry.clone(),
        std::env::temp_dir(),
    );
    let result = runner
        .run_new(
            SubagentRunSpec {
                label: "partial-timeout-test".to_string(),
                instructions: "explore".to_string(),
                task: "task".to_string(),
                registry: empty_sub_registry(),
                model_chain: SubagentModelChain::default(),
                lane_kind: ExecutionLaneKind::Subagent,
                limits: SubagentRunLimits {
                    max_iterations: 8,
                    timeout: Duration::from_millis(250),
                    conclude_timeout: Duration::from_millis(250),
                },
            },
            CancellationToken::new(),
        )
        .await
        .0;

    let error = result.expect_err("both provider calls should time out");
    let message = format!("{error:#}");
    assert!(message.contains("Partial subagent evidence"), "{message}");
    assert!(message.contains("src/lib.rs:9"), "{message}");
    assert!(
        message.contains("<<<untrusted-content source=\"subagent:partial\">>>"),
        "{message}"
    );
    let snapshot = registry.list().into_iter().next().unwrap();
    assert_eq!(snapshot.status, SubagentStatus::TimedOut);
    assert!(
        snapshot
            .result
            .as_deref()
            .is_some_and(|result| result.contains("src/lib.rs:9"))
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn scoped_project_info_runtime_is_per_subagent_run() {
    let custom = custom_registry(&[
        (
            "reader",
            "---\nname: reader\ndescription: d\ntools: [project_info, read]\n---\nprompt",
        ),
        (
            "searcher",
            "---\nname: searcher\ndescription: d\ntools: [project_info, grep]\n---\nprompt",
        ),
    ]);
    let project_root = tempfile::TempDir::new().unwrap();
    let base_runtime = Arc::new(ProjectInfoRuntime::new(None));
    let mut sub_registry = ToolRegistry::new();
    sub_registry.register(Arc::new(crate::tool::ProjectInfoTool::new(
        project_root.path().to_path_buf(),
        base_runtime,
    )));
    sub_registry.register(Arc::new(DummyTool("read")));
    sub_registry.register(Arc::new(DummyTool("grep")));
    let tool = agent_tool_with(Arc::new(sub_registry), custom);

    let reader = tool.resolve("reader").unwrap();
    let searcher = tool.resolve("searcher").unwrap();
    let (reader_registry, reader_runtime) = tool
        .runner
        .registry_for_run(&reader.registry, ReadTracker::new())
        .unwrap();
    let (searcher_registry, searcher_runtime) = tool
        .runner
        .registry_for_run(&searcher.registry, ReadTracker::new())
        .unwrap();

    reader_runtime.set_active_tools(
        "coding",
        reader_registry.names().map(ToString::to_string).collect(),
    );
    searcher_runtime.set_active_tools(
        "coding",
        searcher_registry.names().map(ToString::to_string).collect(),
    );

    assert_eq!(
        project_info_tools(&reader_registry).await,
        vec!["project_info".to_string(), "read".to_string()]
    );
    assert_eq!(
        project_info_tools(&searcher_registry).await,
        vec!["project_info".to_string(), "grep".to_string()]
    );
}

#[tokio::test]
async fn subagent_run_registries_do_not_share_read_evidence() {
    let project_root = tempfile::TempDir::new().unwrap();
    let file_path = project_root.path().join("data.txt");
    tokio::fs::write(&file_path, "one\n").await.unwrap();
    let project_root_path = project_root.path().to_path_buf();
    let registry_factory: SubagentToolRegistryFactory =
        Arc::new(move |source, runtime, read_tracker| {
            let mut registry = ToolRegistry::new();
            for name in source.names() {
                let tool: Arc<dyn Tool> = match name {
                    "project_info" => Arc::new(crate::tool::ProjectInfoTool::new(
                        project_root_path.clone(),
                        runtime.clone(),
                    )),
                    "read" => Arc::new(crate::tool::ReadTool::new(
                        project_root_path.clone(),
                        read_tracker.clone(),
                    )),
                    "write" => Arc::new(crate::tool::WriteTool::with_yolo_mode(
                        project_root_path.clone(),
                        read_tracker.clone(),
                        crate::yolo::YoloMode::new(),
                    )),
                    _ => source.get(name).ok_or_else(|| {
                        anyhow!(
                            "subagent registry entry '{name}' disappeared while preparing a run"
                        )
                    })?,
                };
                registry.register(tool);
            }
            Ok(Arc::new(registry))
        });
    let mut source = ToolRegistry::new();
    source.register(Arc::new(DummyTool("read")));
    source.register(Arc::new(DummyTool("write")));
    let runner = SubagentRunner::new_with_registry_factory(
        factory(),
        Arc::new(source),
        empty_sub_registry(),
        registry_factory,
        Arc::new(ProjectInfoRuntime::new(None)),
        Arc::new(SubagentRegistry::new()),
        project_root.path().to_path_buf(),
    );

    let (first_registry, _) = runner
        .registry_for_run(runner.read_only_registry(), ReadTracker::new())
        .unwrap();
    let read = first_registry.get("read").unwrap();
    read.execute(serde_json::json!({ "path": "data.txt" }))
        .await
        .unwrap();
    let write = first_registry.get("write").unwrap();
    write
        .execute(serde_json::json!({ "path": "data.txt", "content": "two\n" }))
        .await
        .unwrap();

    let (second_registry, _) = runner
        .registry_for_run(runner.read_only_registry(), ReadTracker::new())
        .unwrap();
    let write = second_registry.get("write").unwrap();
    let err = write
        .execute(serde_json::json!({ "path": "data.txt", "content": "three\n" }))
        .await
        .unwrap_err()
        .to_string();

    assert!(err.contains("must be read before overwriting"), "{err}");
}
