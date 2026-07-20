//! Test doubles shared across the agent test suite: a scripted [`Provider`], an
//! output-capturing sink, several [`Tool`] stubs, and the registry/agent
//! builders that assemble them.

use super::*;

#[derive(Clone)]
pub(super) struct MockProvider {
    pub(super) responses: Arc<Mutex<Vec<crate::provider::ProviderResult<StreamedResponse>>>>,
    pub(super) requests: Arc<Mutex<Vec<Vec<ChatCompletionRequestMessage>>>>,
    conversation_cache_keys: Arc<StdMutex<Vec<String>>>,
    project_state_cache_strategy: crate::provider::ProjectStateCacheStrategy,
}

impl MockProvider {
    pub(super) fn new(responses: Vec<crate::provider::ProviderResult<StreamedResponse>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses)),
            requests: Arc::new(Mutex::new(Vec::new())),
            conversation_cache_keys: Arc::new(StdMutex::new(Vec::new())),
            project_state_cache_strategy:
                crate::provider::ProjectStateCacheStrategy::MutableSystemTail,
        }
    }

    pub(super) fn with_append_only_project_state(mut self) -> Self {
        self.project_state_cache_strategy =
            crate::provider::ProjectStateCacheStrategy::AppendOnlyHistory;
        self
    }

    pub(super) fn empty() -> Box<Self> {
        Box::new(Self::new(vec![]))
    }

    pub(super) fn empty_append_only() -> Box<Self> {
        Box::new(Self::new(vec![]).with_append_only_project_state())
    }

    pub(super) fn requests(&self) -> Arc<Mutex<Vec<Vec<ChatCompletionRequestMessage>>>> {
        self.requests.clone()
    }

    pub(super) fn conversation_cache_keys(&self) -> Arc<StdMutex<Vec<String>>> {
        self.conversation_cache_keys.clone()
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn project_state_cache_strategy(&self) -> crate::provider::ProjectStateCacheStrategy {
        self.project_state_cache_strategy
    }

    fn set_conversation_cache_key(&mut self, key: &str) {
        self.conversation_cache_keys
            .lock()
            .expect("mock cache-key mutex should not be poisoned")
            .push(key.to_owned());
    }

    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        _tools: &[ChatCompletionTool],
        _cancellation_token: CancellationToken,
        _sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.requests.lock().await.push(messages.to_vec());
        let mut responses = self.responses.lock().await;
        responses.remove(0)
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
}

#[derive(Default)]
pub(super) struct CaptureSink {
    assistant_deltas: StdMutex<Vec<String>>,
    assistant_done_count: StdMutex<usize>,
    reasoning_deltas: StdMutex<Vec<String>>,
    attempt_discard_count: StdMutex<usize>,
    pub(super) reports: StdMutex<Vec<ContextReport>>,
    pub(super) statuses: StdMutex<Vec<String>>,
    tool_starts: StdMutex<Vec<(String, String, String)>>,
    tool_finishes: StdMutex<Vec<(String, String, bool)>>,
    workspace_changes: StdMutex<Vec<(Vec<String>, String)>>,
}

impl CaptureSink {
    pub(super) fn reports(&self) -> Vec<ContextReport> {
        self.reports
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .clone()
    }

    pub(super) fn statuses(&self) -> Vec<String> {
        self.statuses
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .clone()
    }

    pub(super) fn tool_starts(&self) -> Vec<(String, String, String)> {
        self.tool_starts
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .clone()
    }

    pub(super) fn tool_finishes(&self) -> Vec<(String, String, bool)> {
        self.tool_finishes
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .clone()
    }

    pub(super) fn workspace_changes(&self) -> Vec<(Vec<String>, String)> {
        self.workspace_changes
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .clone()
    }

    pub(super) fn assistant_text(&self) -> String {
        self.assistant_deltas
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .join("")
    }

    pub(super) fn assistant_done_count(&self) -> usize {
        *self
            .assistant_done_count
            .lock()
            .expect("capture sink mutex should not be poisoned")
    }

    pub(super) fn reasoning_text(&self) -> String {
        self.reasoning_deltas
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .join("")
    }

    pub(super) fn attempt_discard_count(&self) -> usize {
        *self
            .attempt_discard_count
            .lock()
            .expect("capture sink mutex should not be poisoned")
    }
}

impl OutputSink for CaptureSink {
    fn assistant_delta(&self, text: &str) {
        self.assistant_deltas
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .push(text.to_string());
    }

    fn assistant_done(&self) {
        *self
            .assistant_done_count
            .lock()
            .expect("capture sink mutex should not be poisoned") += 1;
    }

    fn reasoning_delta(&self, text: &str) {
        self.reasoning_deltas
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .push(text.to_string());
    }

    fn attempt_discarded(&self) {
        *self
            .attempt_discard_count
            .lock()
            .expect("capture sink mutex should not be poisoned") += 1;
    }

    fn context_updated(&self, report: ContextReport) {
        self.reports
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .push(report);
    }

    fn status(&self, text: &str) {
        self.statuses
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .push(text.to_string());
    }

    fn tool_started(&self, id: &str, name: &str, arguments: &str) {
        self.tool_starts
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .push((id.to_string(), name.to_string(), arguments.to_string()));
    }

    fn tool_finished(&self, id: &str, result: &str, status: crate::output::ToolExecutionStatus) {
        self.tool_finishes
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .push((id.to_string(), result.to_string(), status.is_success()));
    }

    fn workspace_changed(&self, paths: &[String], intent: &str) {
        self.workspace_changes
            .lock()
            .expect("capture sink mutex should not be poisoned")
            .push((paths.to_vec(), intent.to_string()));
    }
}

pub(super) struct MockTool {
    pub(super) name: &'static str,
    pub(super) response: String,
    pub(super) calls: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl MockTool {
    pub(super) fn new(name: &'static str, response: &str) -> Self {
        Self {
            name,
            response: response.to_string(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Tool for MockTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "mock tool"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["title"],
            "properties": {"title": {"type": "string"}},
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.calls.lock().await.push(args);
        Ok(ToolOutput::Text(self.response.clone()))
    }
}

pub(super) struct BarrierTool {
    pub(super) name: &'static str,
    pub(super) barrier: Arc<Barrier>,
    pub(super) started: Arc<AtomicUsize>,
}

impl BarrierTool {
    pub(super) fn new(
        name: &'static str,
        barrier: Arc<Barrier>,
        started: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            name,
            barrier,
            started,
        }
    }
}

#[async_trait]
impl Tool for BarrierTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "barrier tool"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.started.fetch_add(1, Ordering::SeqCst);
        self.barrier.wait().await;
        Ok(ToolOutput::Text(format!("{} done", self.name)))
    }
}

pub(super) struct QueueingTool {
    pub(super) sender: mpsc::UnboundedSender<QueuedUserMessageCommand>,
}

pub(super) struct SerializedQueueingTool {
    pub(super) sender: mpsc::UnboundedSender<QueuedUserMessageCommand>,
}

pub(super) struct SerializedRecordingTool {
    pub(super) calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for QueueingTool {
    fn name(&self) -> &str {
        "queueing_tool"
    }

    fn description(&self) -> &str {
        "queues a user message while tools run"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let _ = self
            .sender
            .send(QueuedUserMessageCommand::Send(QueuedUserMessage {
                id: 7,
                display_text: "please also check tests".to_string(),
                input: crate::agent::UserInput::from_text("please also check tests"),
            }));
        let _ = self
            .sender
            .send(QueuedUserMessageCommand::Send(QueuedUserMessage {
                id: 8,
                display_text: "and update docs".to_string(),
                input: crate::agent::UserInput::from_text("and update docs"),
            }));
        Ok(ToolOutput::Text("queued".to_string()))
    }
}

#[async_trait]
impl Tool for SerializedQueueingTool {
    fn name(&self) -> &str {
        "serialized_queue"
    }

    fn description(&self) -> &str {
        "queues higher-priority steering"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        let _ = self
            .sender
            .send(QueuedUserMessageCommand::Send(QueuedUserMessage {
                id: 9,
                display_text: "stop before the next action".to_string(),
                input: crate::agent::UserInput::from_text("stop before the next action"),
            }));
        Ok(ToolOutput::Text("queued steering".to_string()))
    }
}

#[async_trait]
impl Tool for SerializedRecordingTool {
    fn name(&self) -> &str {
        "serialized_action"
    }

    fn description(&self) -> &str {
        "records whether the serialized action ran"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::Text("action ran".to_string()))
    }
}

pub(super) struct PolicyStub {
    pub(super) name: &'static str,
    pub(super) policy: crate::tool::ParallelPolicy,
    /// For an `agent`-tool stub: the agent-arg names that resolve to a read-only
    /// subagent. `None` leaves `delegation_is_read_only` at its default (`None`),
    /// modeling a non-delegation tool or an unresolvable target.
    pub(super) read_only_agents: Option<&'static [&'static str]>,
}

#[async_trait]
impl Tool for PolicyStub {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "policy stub"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        self.policy
    }
    fn delegation_is_read_only(&self, args: &serde_json::Value) -> Option<bool> {
        let read_only = self.read_only_agents?;
        let name = args.get("agent").and_then(|value| value.as_str())?;
        Some(read_only.contains(&name))
    }
    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::Text(String::new()))
    }
}

pub(super) fn empty_registry() -> Arc<ToolRegistry> {
    Arc::new(ToolRegistry::new())
}

pub(super) fn mock_registry(names: &[&'static str]) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for name in names {
        registry.register(Arc::new(MockTool::new(name, "ok")));
    }
    Arc::new(registry)
}

/// Build a registry with one `PolicyStub` per (name, policy) pair.
/// Used by batching tests that need the agent to see a real
/// `parallel_policy` for a name without dragging in the full
/// `BashTool` (which needs a `ReadTracker`, `PermissionService`, and
/// `InteractionService`).
pub(super) fn registry_with_policies(
    tools: &[(&'static str, crate::tool::ParallelPolicy)],
) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    for (name, policy) in tools {
        registry.register(Arc::new(PolicyStub {
            name,
            policy: *policy,
            read_only_agents: None,
        }));
    }
    Arc::new(registry)
}

/// A registry whose `agent` tool classifies `read_only` targets as read-only
/// delegations (parallel-safe) and every other target as write-capable
/// (serialized) — mirroring the real `AgentTool::delegation_is_read_only`.
pub(super) fn registry_with_agent_delegation(
    read_only: &'static [&'static str],
) -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(PolicyStub {
        name: "agent",
        policy: crate::tool::ParallelPolicy::Serialized,
        read_only_agents: Some(read_only),
    }));
    Arc::new(registry)
}

pub(super) fn test_tool_call(id: &str, name: &str, arguments: &str) -> crate::provider::ToolCall {
    crate::provider::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: arguments.to_string(),
    }
}

pub(super) fn review_agent(fixture: &TestFixture, system_context: String) -> Agent {
    Agent::new(
        MockProvider::empty(),
        empty_registry(),
        empty_registry(),
        fixture.read_tracker.clone(),
        system_context,
        fixture.project_root.clone(),
    )
    .unwrap()
}

pub(super) fn batch_root() -> &'static Path {
    Path::new(".")
}
