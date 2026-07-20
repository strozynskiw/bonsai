use super::*;
use std::sync::Arc;
use std::time::Duration;

use crate::provider::{
    DEFAULT_CONTEXT_WINDOW_TOKENS, PromptEstimator, Provider, ReasoningSelection, StreamedResponse,
    TokenUsage,
};
use crate::resource::agent::AgentRegistry;
use crate::subagent::{SubagentModelChain, SubagentRegistry, SubagentStatus};
use crate::tool::schema::closed_object;
use crate::tool::{
    ParallelPolicy, ProjectInfoRuntime, ReadTracker, Tool, ToolOutput, ToolRegistry,
};
use crate::{agent::ExecutionLaneKind, output::SharedSink};
use anyhow::{Result, anyhow};
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// A provider that answers once with fixed content and no tool calls, so a
/// nested subagent completes in a single iteration.
struct OneShotProvider {
    content: &'static str,
    usage: Option<TokenUsage>,
}

struct SpawnedCancellationProvider {
    started: Arc<tokio::sync::Notify>,
    cancelled: Arc<tokio::sync::Notify>,
}

struct CancellableProvider {
    started: Arc<tokio::sync::Notify>,
    cancelled: Arc<tokio::sync::Notify>,
}

struct ConcludeAfterBudgetProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
    saw_conclude: Arc<std::sync::atomic::AtomicBool>,
    saw_tools_disabled: Arc<std::sync::atomic::AtomicBool>,
    max_iterations: usize,
}

struct ConcludeAfterTimeoutProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

struct NeverConcludeProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl Provider for OneShotProvider {
    async fn chat_stream(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        _cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        Ok(StreamedResponse {
            content: self.content.to_string(),
            usage: self.usage,
            ..StreamedResponse::default()
        })
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl Provider for SpawnedCancellationProvider {
    async fn chat_stream(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        let cancelled = self.cancelled.clone();
        tokio::spawn(async move {
            cancellation_token.cancelled().await;
            cancelled.notify_one();
        });
        self.started.notify_one();
        std::future::pending::<crate::provider::ProviderResult<StreamedResponse>>().await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl Provider for CancellableProvider {
    async fn chat_stream(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.started.notify_one();
        cancellation_token.cancelled().await;
        self.cancelled.notify_one();
        Ok(StreamedResponse::interrupted())
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl Provider for ConcludeAfterBudgetProvider {
    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        _cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        use std::sync::atomic::Ordering;

        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.max_iterations {
            return Ok(StreamedResponse {
                tool_calls: vec![crate::provider::ToolCall {
                    id: format!("missing-{call}"),
                    name: format!("missing_read_tool_{call}"),
                    arguments: "{}".to_string(),
                }],
                ..StreamedResponse::default()
            });
        }
        self.saw_conclude.store(
            format!("{messages:?}").contains("Your subagent budget is exhausted"),
            Ordering::SeqCst,
        );
        self.saw_tools_disabled
            .store(tools.is_empty(), Ordering::SeqCst);
        Ok(StreamedResponse {
            content: "Major: src/lib.rs:1 is incorrect.".to_string(),
            ..StreamedResponse::default()
        })
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl Provider for ConcludeAfterTimeoutProvider {
    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        _cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        use std::sync::atomic::Ordering;

        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return std::future::pending().await;
        }
        assert!(format!("{messages:?}").contains("Your subagent budget is exhausted"));
        Ok(StreamedResponse {
            content: "The changes look correct.".to_string(),
            ..StreamedResponse::default()
        })
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl Provider for NeverConcludeProvider {
    async fn chat_stream(
        &self,
        _messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        _cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if call == 0 {
            return Ok(StreamedResponse {
                tool_calls: vec![crate::provider::ToolCall {
                    id: "partial-evidence-read".to_string(),
                    name: "missing_read_tool".to_string(),
                    arguments: r#"{"path":"src/lib.rs:9"}"#.to_string(),
                }],
                ..StreamedResponse::default()
            });
        }
        std::future::pending().await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

fn factory() -> SubagentProviderFactory {
    Arc::new(|_agent: String, _chain: SubagentModelChain| {
        Box::pin(async {
            SubagentProviderConfig::new(
                Box::new(OneShotProvider {
                    content: "Found it at src/lib.rs:1.",
                    usage: Some(TokenUsage {
                        prompt_tokens: 11,
                        completion_tokens: 7,
                        input_cache: None,
                    }),
                }),
                DEFAULT_CONTEXT_WINDOW_TOKENS as usize,
                PromptEstimator::heuristic(),
                "test-model".to_string(),
            )
            .with_active_model_identity("test-provider", "test-model")
        })
    })
}

fn empty_sub_registry() -> Arc<ToolRegistry> {
    Arc::new(ToolRegistry::new())
}

fn test_run_spec(
    label: &str,
    instructions: &str,
    task: &str,
    registry: Arc<ToolRegistry>,
    limits: SubagentRunLimits,
) -> SubagentRunSpec {
    SubagentRunSpec {
        label: label.to_string(),
        instructions: instructions.to_string(),
        task: task.to_string(),
        registry,
        model_chain: SubagentModelChain::default(),
        lane_kind: ExecutionLaneKind::Subagent,
        limits,
    }
}

/// A no-op tool so a scoped read-only `sub_registry` has named entries to
/// filter in tests.
#[derive(Debug)]
struct DummyTool(&'static str);

#[async_trait]
impl Tool for DummyTool {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "dummy read-only tool"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        closed_object([], &[])
    }
    async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
        Ok(ToolOutput::Text("ok".into()))
    }
}

fn sub_registry_with(names: &[&'static str]) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for name in names {
        registry.register(Arc::new(DummyTool(name)));
    }
    Arc::new(registry)
}

/// Build an `AgentRegistry` from in-memory `.bonsai/agents/<name>.md` files.
/// Bodies are read into memory during load, so the tempdir can drop after.
fn custom_registry(files: &[(&str, &str)]) -> Arc<AgentRegistry> {
    let dir = tempfile::TempDir::new().unwrap();
    for (name, contents) in files {
        let path = dir.path().join(format!(".bonsai/agents/{name}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    Arc::new(AgentRegistry::load_from(
        dir.path(),
        &dir.path().join("home"),
    ))
}

fn agent_tool() -> AgentTool {
    agent_tool_with(empty_sub_registry(), Arc::new(AgentRegistry::empty()))
}

fn test_runner(sub_registry: Arc<ToolRegistry>) -> SubagentRunner {
    // Tests scope a custom agent's `tools:` from the full registry, so it
    // must carry the same named tools the read-only set advertises.
    SubagentRunner::new(
        factory(),
        sub_registry.clone(),
        sub_registry,
        ReadTracker::new(),
        Arc::new(ProjectInfoRuntime::new(None)),
        Arc::new(SubagentRegistry::new()),
        std::env::temp_dir(),
    )
}

fn test_runner_with_full_registry(
    sub_registry: Arc<ToolRegistry>,
    full_registry: Arc<ToolRegistry>,
) -> SubagentRunner {
    SubagentRunner::new(
        factory(),
        sub_registry,
        full_registry,
        ReadTracker::new(),
        Arc::new(ProjectInfoRuntime::new(None)),
        Arc::new(SubagentRegistry::new()),
        std::env::temp_dir(),
    )
}

fn agent_tool_with(sub_registry: Arc<ToolRegistry>, custom: Arc<AgentRegistry>) -> AgentTool {
    AgentTool::new(
        test_runner(sub_registry),
        crate::resource::agent::shared_registry(custom),
    )
}

fn agent_tool_with_settings(
    sub_registry: Arc<ToolRegistry>,
    custom: Arc<AgentRegistry>,
    settings: crate::subagent::BuiltinSubagentSettingsRegistry,
) -> AgentTool {
    AgentTool::new_with_settings(
        test_runner(sub_registry),
        crate::resource::agent::shared_registry(custom),
        crate::subagent::SharedBuiltinSubagentSettings::new(settings),
    )
}

fn agent_tool_with_background_wake(
    sub_registry: Arc<ToolRegistry>,
    custom: Arc<AgentRegistry>,
) -> AgentTool {
    AgentTool::new(
        test_runner(sub_registry).with_background_wake(),
        crate::resource::agent::shared_registry(custom),
    )
}

async fn project_info_tools(registry: &ToolRegistry) -> Vec<String> {
    let project_info = registry.get("project_info").expect("project_info tool");
    let result = project_info.execute(serde_json::json!({})).await.unwrap();
    let ToolOutput::Text(text) = result else {
        panic!("project_info should return text");
    };
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|tool| tool.as_str().expect("tool name").to_string())
        .collect()
}

mod catalog;
mod runner;
mod tool;
