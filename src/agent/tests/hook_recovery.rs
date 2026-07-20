//! Agent-loop recovery gate (M5.3, plan section 4.6): a turn whose
//! `PreToolUse` hook fails (nonzero exit, not a `Block`) must not crash the
//! run loop. With the default `on_failure: warn`, the tool still resolves,
//! the turn completes, a warning reaches the sink, and the *next* turn runs
//! normally — proving the failure doesn't leave the engine in a broken state.

use super::*;

use crate::config::{
    Config, ConfigSource, FailureBehavior, HookAction, HookDef, HookEvent, HookMatcher,
};
use crate::hooks::HookEngine;
use crate::interaction::InteractionService;
use crate::permissions::PermissionManager;

fn failing_pre_tool_use_hook(tool_name: &str, on_failure: FailureBehavior) -> Config {
    Config {
        hooks: vec![(
            HookDef {
                name: "flaky".to_string(),
                event: HookEvent::PreToolUse,
                matcher: HookMatcher {
                    tool: Some(tool_name.to_string()),
                    path: None,
                },
                action: HookAction::Shell {
                    command: "exit 3".to_string(),
                },
                timeout_secs: 5,
                blocking: true,
                on_failure,
                capabilities: Vec::new(),
                enabled: true,
            },
            ConfigSource::Global,
        )],
        ..Config::default()
    }
}

fn post_tool_context_hook(tool_name: &str, context: &str) -> Config {
    Config {
        hooks: vec![(
            HookDef {
                name: "add-context".to_string(),
                event: HookEvent::PostToolUse,
                matcher: HookMatcher {
                    tool: Some(tool_name.to_string()),
                    path: None,
                },
                action: HookAction::Shell {
                    command: format!(r#"echo '{{"context":"{context}"}}'"#),
                },
                timeout_secs: 5,
                blocking: true,
                on_failure: FailureBehavior::Warn,
                capabilities: Vec::new(),
                enabled: true,
            },
            ConfigSource::Global,
        )],
        ..Config::default()
    }
}

#[tokio::test]
async fn failing_hook_warns_but_the_turn_and_the_next_one_still_complete() {
    let fixture = TestFixture::new();
    let config = failing_pre_tool_use_hook("mock_tool", FailureBehavior::Warn);
    let hooks = Arc::new(
        HookEngine::build(
            &config,
            fixture.project_root.clone(),
            PermissionManager::memory_only_hooks(),
            Arc::new(InteractionService::noninteractive()),
            Arc::new(crate::extension::status::ExtensionRegistry::new()),
            None,
        )
        .await,
    );

    let tool = Arc::new(MockTool::new("mock_tool", "tool ran"));
    let calls = tool.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(tool);

    let provider = MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call("call-1", "mock_tool", r#"{"title":"one"}"#)],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "first done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call("call-2", "mock_tool", r#"{"title":"two"}"#)],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "second done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]);

    let mut agent = Agent::builder(
        Box::new(provider),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .hooks(hooks)
    .build()
    .unwrap();

    let sink = Arc::new(CaptureSink::default());

    let first = agent
        .run("first", CancellationToken::new(), sink.clone())
        .await
        .unwrap();
    assert_eq!(first, AgentRunResult::Completed("first done".to_string()));
    assert_eq!(
        calls.lock().await.len(),
        1,
        "the hook failure must not block the tool call (on_failure: warn is fail-open)"
    );
    assert!(
        sink.statuses()
            .iter()
            .any(|status| status.contains("[hooks]") && status.contains("failed")),
        "{:?}",
        sink.statuses()
    );

    let second = agent
        .run("second", CancellationToken::new(), sink.clone())
        .await
        .unwrap();
    assert_eq!(second, AgentRunResult::Completed("second done".to_string()));
    assert_eq!(
        calls.lock().await.len(),
        2,
        "the next turn must still run its tool call normally"
    );
}

#[tokio::test]
async fn failing_hook_blocks_the_call_when_fail_closed() {
    let fixture = TestFixture::new();
    let config = failing_pre_tool_use_hook("mock_tool", FailureBehavior::Block);
    let hooks = Arc::new(
        HookEngine::build(
            &config,
            fixture.project_root.clone(),
            PermissionManager::memory_only_hooks(),
            Arc::new(InteractionService::noninteractive()),
            Arc::new(crate::extension::status::ExtensionRegistry::new()),
            None,
        )
        .await,
    );

    let tool = Arc::new(MockTool::new("mock_tool", "tool ran"));
    let calls = tool.calls.clone();
    let mut registry = ToolRegistry::new();
    registry.register(tool);

    let provider = MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call("call-1", "mock_tool", r#"{"title":"one"}"#)],
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

    let mut agent = Agent::builder(
        Box::new(provider),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .hooks(hooks)
    .build()
    .unwrap();

    let result = agent
        .run("go", CancellationToken::new(), Arc::new(StdoutSink))
        .await
        .unwrap();
    assert_eq!(result, AgentRunResult::Completed("done".to_string()));
    assert!(
        calls.lock().await.is_empty(),
        "on_failure: block must veto the call, not just warn"
    );

    let requests = requests.lock().await;
    let tool_result = requests[1]
        .iter()
        .find_map(|message| match message {
            ChatCompletionRequestMessage::Tool(_) => Some(message_content(message)),
            _ => None,
        })
        .expect("the blocked call still resolves with a tool message");
    assert!(tool_result.contains("flaky"), "{tool_result}");
}

#[tokio::test]
async fn post_tool_context_is_redacted_before_sink_and_model() {
    let fixture = TestFixture::new();
    let secret = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
    let config = post_tool_context_hook("mock_tool", &format!("hook token: {secret}"));
    let hooks = Arc::new(
        HookEngine::build(
            &config,
            fixture.project_root.clone(),
            PermissionManager::memory_only_hooks(),
            Arc::new(InteractionService::noninteractive()),
            Arc::new(crate::extension::status::ExtensionRegistry::new()),
            None,
        )
        .await,
    );

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(MockTool::new("mock_tool", "tool ran")));

    let provider = MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call("call-1", "mock_tool", r#"{"title":"one"}"#)],
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
    let mut agent = Agent::builder(
        Box::new(provider),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        fixture.project_root.clone(),
    )
    .hooks(hooks)
    .build()
    .unwrap();
    let sink = Arc::new(CaptureSink::default());

    let result = agent
        .run("go", CancellationToken::new(), sink.clone())
        .await
        .unwrap();
    assert_eq!(result, AgentRunResult::Completed("done".to_string()));

    let requests = requests.lock().await;
    let tool_result = requests[1]
        .iter()
        .find_map(|message| match message {
            ChatCompletionRequestMessage::Tool(_) => Some(message_content(message)),
            _ => None,
        })
        .expect("the next provider request carries the tool result");
    assert!(!tool_result.contains(&secret), "{tool_result}");
    assert!(
        tool_result.contains("[REDACTED:GitHub token]"),
        "{tool_result}"
    );

    let finished = sink.tool_finishes();
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert!(!finished[0].1.contains(&secret), "{:?}", finished[0]);
}
