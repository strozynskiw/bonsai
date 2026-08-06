use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

use crate::background::{BackgroundTaskRegistry, format_task_list};
use crate::background_wake::TerminalWaitRegistration;
use crate::subagent::{SubagentRegistry, SubagentSnapshot, format_subagent_list};
use crate::terminal::{TerminalRegistry, TerminalSnapshot, format_terminal_list};
use crate::tool::schema::{
    bounded_integer_property, object, parse_args, string_enum_property, string_property,
};
use crate::tool::{ParallelPolicy, Tool, ToolExecutionContext, ToolOutput};

const MAX_WAIT_SECONDS: u64 = 60;

#[derive(Debug, Deserialize)]
struct TasksArgs {
    action: TaskAction,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    wait_seconds: Option<u64>,
    #[serde(default)]
    observed_version: Option<u64>,
    #[serde(default)]
    output_threshold: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TaskAction {
    List,
    Wait,
    Tail,
    Stop,
}

#[derive(Debug)]
pub struct TasksTool {
    registry: Arc<BackgroundTaskRegistry>,
    subagents: Option<Arc<SubagentRegistry>>,
    terminals: Option<Arc<TerminalRegistry>>,
    background_wakes: Option<Arc<crate::background_wake::BackgroundWakeCoordinator>>,
    can_park: bool,
}

impl TasksTool {
    pub fn new(registry: Arc<BackgroundTaskRegistry>) -> Self {
        Self {
            registry,
            subagents: None,
            terminals: None,
            background_wakes: None,
            can_park: false,
        }
    }

    pub(crate) fn with_subagents(
        registry: Arc<BackgroundTaskRegistry>,
        subagents: Arc<SubagentRegistry>,
    ) -> Self {
        let mut tool = Self::new(registry);
        tool.subagents = Some(subagents);
        tool
    }

    pub(crate) fn with_subagents_and_terminals(
        registry: Arc<BackgroundTaskRegistry>,
        subagents: Arc<SubagentRegistry>,
        terminals: Arc<TerminalRegistry>,
        background_wakes: Option<Arc<crate::background_wake::BackgroundWakeCoordinator>>,
        can_park: bool,
    ) -> Self {
        let mut tool = Self::with_subagents(registry, subagents);
        tool.terminals = Some(terminals);
        tool.background_wakes = background_wakes;
        tool.can_park = can_park;
        tool
    }
}

#[async_trait]
impl Tool for TasksTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::LocalState
    }

    fn name(&self) -> &str {
        "tasks"
    }

    fn description(&self) -> &str {
        "List, wait for, tail, or stop process-local background Bash tasks and interactive pty-N terminals. Wait and tail also accept background subagent IDs like sub-1 from agent run_in_background:true."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [
                (
                    "action",
                    string_enum_property(
                        "Task operation to perform",
                        &["list", "wait", "tail", "stop"],
                    ),
                ),
                (
                    "task_id",
                    string_property(
                        "Background task ID (bg-N Bash task, pty-N interactive terminal, or sub-N subagent), required for tail and stop, optional for wait",
                    ),
                ),
                (
                    "wait_seconds",
                    bounded_integer_property(
                        "Optional headless wait or interactive-terminal deadline in seconds (max: 60); interactive bg-N waits park until completion instead of polling",
                        Some(0),
                        Some(MAX_WAIT_SECONDS as i64),
                    ),
                ),
                (
                    "observed_version",
                    bounded_integer_property(
                        "Optional version from a preceding list or tail; concrete interactive waits atomically use the current version when omitted",
                        Some(1),
                        None,
                    ),
                ),
                (
                    "output_threshold",
                    bounded_integer_property(
                        "Wake once total output reaches this character count; it must exceed the observed total",
                        Some(0),
                        None,
                    ),
                ),
            ],
            &["action"],
        )
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        ParallelPolicy::Serialized
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        self.execute_inner(args, None).await
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: ToolExecutionContext,
    ) -> Result<ToolOutput> {
        self.execute_inner(args, Some(context)).await
    }
}

impl TasksTool {
    async fn execute_inner(
        &self,
        args: serde_json::Value,
        context: Option<ToolExecutionContext>,
    ) -> Result<ToolOutput> {
        let args: TasksArgs = parse_args("tasks tool", args)?;

        let output = match args.action {
            TaskAction::List => ToolOutput::Text(self.list_all().await),
            TaskAction::Wait => {
                let wait_seconds = args
                    .wait_seconds
                    .map(|seconds| seconds.min(MAX_WAIT_SECONDS));
                // Treat an empty/whitespace task_id the same as an omitted one
                // (wait for any) rather than forwarding "" to the registry,
                // matching how `tail`/`stop` normalize via `required_task_id`.
                if let Some(task_id) = args
                    .task_id
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                {
                    let task_id = task_id.trim();
                    if is_subagent_id(task_id) {
                        ToolOutput::Text(
                            self.wait_for_subagent(
                                task_id,
                                Duration::from_secs(wait_seconds.unwrap_or(1)),
                            )
                            .await?,
                        )
                    } else if is_terminal_id(task_id) {
                        self.wait_for_terminal_wakeable(task_id, &args, context.as_ref())
                            .await?
                    } else {
                        self.wait_for_background_wakeable(task_id, &args, context.as_ref())
                            .await?
                    }
                } else {
                    ToolOutput::Text(
                        self.wait_for_any(Duration::from_secs(wait_seconds.unwrap_or(1)))
                            .await,
                    )
                }
            }
            TaskAction::Tail => {
                let task_id = required_task_id(args.task_id.as_deref(), "tail")?;
                if is_subagent_id(task_id) {
                    ToolOutput::Text(self.tail_subagent(task_id)?)
                } else if is_terminal_id(task_id) {
                    ToolOutput::untrusted_context(
                        format!("terminal:{task_id}"),
                        &self.tail_terminal(task_id).await?,
                    )
                } else {
                    ToolOutput::Text(
                        self.registry
                            .snapshot(task_id)
                            .await
                            .with_context(|| format!("Unknown background task: {task_id}"))?
                            .detail(),
                    )
                }
            }
            TaskAction::Stop => {
                let task_id = required_task_id(args.task_id.as_deref(), "stop")?;
                if is_subagent_id(task_id) {
                    bail!(
                        "tasks stop only supports background Bash tasks; subagent {task_id} can be inspected with tasks tail or /subagents and is cancelled when the parent run is interrupted."
                    );
                } else if is_terminal_id(task_id) {
                    let Some(terminals) = &self.terminals else {
                        bail!("Unknown interactive terminal: {task_id}");
                    };
                    let snapshot = terminals.stop(task_id).await?;
                    let (output, _) = snapshot.model_output();
                    ToolOutput::untrusted_context(format!("terminal:{task_id}"), &output)
                } else {
                    ToolOutput::Text(self.registry.stop(task_id).await?.detail())
                }
            }
        };

        Ok(output)
    }
    async fn list_all(&self) -> String {
        let bash_tasks = self.registry.list().await;
        let bash = format_task_list(&bash_tasks);
        let with_terminals = self.append_terminals_if_present(bash).await;
        self.append_subagents_if_present(with_terminals)
    }

    async fn wait_for_any(&self, wait: Duration) -> String {
        match (&self.subagents, &self.terminals) {
            (Some(subagents), Some(terminals)) => {
                let (bash_tasks, terminal_tasks, subagent_tasks) = tokio::join!(
                    self.registry.wait_for_any(wait),
                    terminals.wait_for_any(wait),
                    subagents.wait_for_any_detached(wait)
                );
                append_subagent_section(
                    append_terminal_section(format_task_list(&bash_tasks), &terminal_tasks),
                    &subagent_tasks,
                )
            }
            (Some(subagents), None) => {
                let (bash_tasks, subagent_tasks) = tokio::join!(
                    self.registry.wait_for_any(wait),
                    subagents.wait_for_any_detached(wait)
                );
                append_subagent_section(format_task_list(&bash_tasks), &subagent_tasks)
            }
            (None, Some(terminals)) => {
                let (bash_tasks, terminal_tasks) = tokio::join!(
                    self.registry.wait_for_any(wait),
                    terminals.wait_for_any(wait)
                );
                append_terminal_section(format_task_list(&bash_tasks), &terminal_tasks)
            }
            (None, None) => format_task_list(&self.registry.wait_for_any(wait).await),
        }
    }

    async fn wait_for_subagent(&self, task_id: &str, wait: Duration) -> Result<String> {
        let Some(subagents) = &self.subagents else {
            bail!(
                "Unknown background subagent: {task_id}. This runtime does not expose subagent state through the tasks tool."
            );
        };
        let snapshot = subagents
            .wait_for_subagent(task_id, wait)
            .await
            .with_context(|| format!("Unknown background subagent: {task_id}"))?;
        Ok(snapshot.detail())
    }

    fn tail_subagent(&self, task_id: &str) -> Result<String> {
        let Some(subagents) = &self.subagents else {
            bail!(
                "Unknown background subagent: {task_id}. This runtime does not expose subagent state through the tasks tool."
            );
        };
        Ok(subagents
            .snapshot(task_id)
            .with_context(|| format!("Unknown background subagent: {task_id}"))?
            .detail())
    }

    async fn wait_for_terminal(&self, task_id: &str, wait: Duration) -> Result<String> {
        let Some(terminals) = &self.terminals else {
            bail!("Unknown interactive terminal: {task_id}");
        };
        let snapshot = terminals.wait_for_terminal(task_id, wait).await?;
        Ok(snapshot.model_output().0)
    }

    async fn wait_for_background_wakeable(
        &self,
        task_id: &str,
        args: &TasksArgs,
        context: Option<&ToolExecutionContext>,
    ) -> Result<ToolOutput> {
        let snapshot = self
            .registry
            .snapshot(task_id)
            .await
            .with_context(|| format!("Unknown background task: {task_id}"))?;
        if snapshot.status.is_finished() {
            return Ok(ToolOutput::Text(snapshot.detail()));
        }
        if !self.can_park {
            return Ok(ToolOutput::Text(
                self.registry
                    .wait_for_task(task_id, Duration::from_secs(args.wait_seconds.unwrap_or(1)))
                    .await?
                    .detail(),
            ));
        }
        let observed_version = args.observed_version.unwrap_or(snapshot.version);
        if observed_version != snapshot.version {
            bail!(
                "Background task {task_id} is at version {}; tail it before waiting",
                snapshot.version
            );
        }
        if args
            .output_threshold
            .is_some_and(|threshold| threshold <= snapshot.total_output_chars)
        {
            bail!(
                "Background task {task_id} already has {} output characters; output_threshold must be greater than the observed total",
                snapshot.total_output_chars
            );
        }
        let coordinator = context
            .and_then(ToolExecutionContext::background_wakes)
            .or(self.background_wakes.as_ref())
            .context("Wakeable waits are unavailable on this surface")?;
        let operation_key =
            context.map_or("tasks-background-wait", ToolExecutionContext::tool_call_id);
        let wait = coordinator
            .register_background_task(
                snapshot,
                operation_key,
                observed_version,
                args.output_threshold,
                // Background-process progress is already streamed to the TUI.
                // Do not wake the model on fixed intervals; completion or an
                // explicit semantic output threshold is the wake event.
                None,
            )
            .await?;
        Ok(ToolOutput::WaitStarted {
            reason: crate::agent::WaitReason::BackgroundWork(wait),
            message: format!("Waiting for {task_id} after version {observed_version}."),
        })
    }

    async fn wait_for_terminal_wakeable(
        &self,
        task_id: &str,
        args: &TasksArgs,
        context: Option<&ToolExecutionContext>,
    ) -> Result<ToolOutput> {
        self.terminals
            .as_ref()
            .context("Interactive terminals are unavailable")?;
        if !self.can_park {
            let output = self
                .wait_for_terminal(task_id, Duration::from_secs(args.wait_seconds.unwrap_or(1)))
                .await?;
            return Ok(ToolOutput::untrusted_context(
                format!("terminal:{task_id}"),
                &output,
            ));
        }
        let coordinator = context
            .and_then(ToolExecutionContext::background_wakes)
            .or(self.background_wakes.as_ref())
            .context("Wakeable waits are unavailable on this surface")?;
        let operation_key =
            context.map_or("tasks-terminal-wait", ToolExecutionContext::tool_call_id);
        match coordinator
            .register_terminal_current(
                task_id,
                operation_key,
                args.observed_version,
                args.output_threshold,
                args.wait_seconds
                    .map(|seconds| seconds.min(MAX_WAIT_SECONDS)),
            )
            .await?
        {
            TerminalWaitRegistration::Ready(snapshot) => {
                let (output, _) = snapshot.model_output();
                Ok(ToolOutput::untrusted_context(
                    format!("terminal:{task_id}"),
                    &output,
                ))
            }
            TerminalWaitRegistration::Parked(wait) => Ok(ToolOutput::WaitStarted {
                message: format!(
                    "Waiting for {task_id} from semantic version {}.",
                    wait.observed_version
                ),
                reason: crate::agent::WaitReason::BackgroundWork(wait),
            }),
        }
    }

    async fn tail_terminal(&self, task_id: &str) -> Result<String> {
        let Some(terminals) = &self.terminals else {
            bail!("Unknown interactive terminal: {task_id}");
        };
        let snapshot = terminals
            .snapshot(task_id)
            .await
            .with_context(|| format!("Unknown interactive terminal: {task_id}"))?;
        Ok(snapshot.model_output().0)
    }

    fn append_subagents_if_present(&self, bash: String) -> String {
        let Some(subagents) = &self.subagents else {
            return bash;
        };
        append_subagent_section(bash, &subagents.list_detached())
    }

    async fn append_terminals_if_present(&self, bash: String) -> String {
        let Some(terminals) = &self.terminals else {
            return bash;
        };
        append_terminal_section(bash, &terminals.list().await)
    }
}

fn required_task_id<'a>(task_id: Option<&'a str>, action: &str) -> Result<&'a str> {
    let Some(task_id) = task_id.map(str::trim).filter(|value| !value.is_empty()) else {
        bail!("task_id is required for tasks action '{action}'");
    };
    Ok(task_id)
}

fn append_subagent_section(mut bash: String, subagents: &[SubagentSnapshot]) -> String {
    if !subagents.is_empty() {
        bash.push_str("\n\n");
        bash.push_str(&format_subagent_list(subagents));
    }
    bash
}

fn is_subagent_id(task_id: &str) -> bool {
    task_id.trim_start().starts_with("sub-")
}

fn is_terminal_id(task_id: &str) -> bool {
    task_id.trim_start().starts_with("pty-")
}

fn append_terminal_section(mut output: String, terminals: &[TerminalSnapshot]) -> String {
    if !terminals.is_empty() {
        output.push_str("\n\n");
        output.push_str(&format_terminal_list(terminals));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    use crate::storage::test_utils::TestStorage;

    fn shell() -> String {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }

    #[tokio::test]
    async fn tasks_tool_lists_and_tails_background_task() {
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let temp_dir = tempfile::TempDir::new().unwrap();
        let task = registry
            .start(&shell(), "printf task-output", temp_dir.path(), 5)
            .await
            .unwrap();
        registry
            .wait_for_task(&task.id, Duration::from_secs(2))
            .await
            .unwrap();

        let tool = TasksTool::new(registry);
        let listed = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        let tailed = tool
            .execute(serde_json::json!({"action": "tail", "task_id": task.id}))
            .await
            .unwrap();

        assert!(matches!(listed, ToolOutput::Text(text) if text.contains("succeeded")));
        assert!(matches!(tailed, ToolOutput::Text(text) if text.contains("task-output")));
    }

    #[tokio::test]
    async fn tasks_tool_waits_for_specific_task() {
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let temp_dir = tempfile::TempDir::new().unwrap();
        let task = registry
            .start(&shell(), "sleep 0.1; printf done", temp_dir.path(), 5)
            .await
            .unwrap();

        let tool = TasksTool::new(registry);
        let waited = tool
            .execute(serde_json::json!({
                "action": "wait",
                "task_id": task.id,
                "wait_seconds": 2
            }))
            .await
            .unwrap();

        assert!(matches!(waited, ToolOutput::Text(text) if text.contains("done")));
    }

    #[tokio::test]
    async fn interactive_background_wait_parks_from_current_version_and_wakes_once() {
        let storage = TestStorage::new().await;
        let session_id = storage.start_session().await;
        let active_session_id = Arc::new(Mutex::new(None));
        crate::storage::activate_session_heartbeat(
            &storage.storage,
            &active_session_id,
            session_id,
        )
        .await
        .expect("test session should become live");
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let terminals = Arc::new(TerminalRegistry::new());
        let coordinator = Arc::new(crate::background_wake::BackgroundWakeCoordinator::new(
            storage.storage.clone(),
            active_session_id,
            registry.clone(),
            terminals.clone(),
        ));
        let tool = TasksTool::with_subagents_and_terminals(
            registry.clone(),
            Arc::new(SubagentRegistry::new()),
            terminals,
            Some(coordinator.clone()),
            true,
        );
        let task = registry
            .start(
                &shell(),
                "sleep 0.05; printf progress; sleep 0.3",
                storage.project_path(),
                60,
            )
            .await
            .expect("background fixture should start");
        let baseline = registry
            .snapshot(&task.id)
            .await
            .expect("background fixture should be registered");

        let parked = tool
            .execute(serde_json::json!({
                "action": "wait",
                "task_id": task.id,
                "wait_seconds": 1
            }))
            .await
            .expect("interactive wait should park");
        let ToolOutput::WaitStarted {
            reason: crate::agent::WaitReason::BackgroundWork(wait),
            ..
        } = parked
        else {
            panic!("interactive wait should return a durable wait");
        };
        assert_eq!(wait.observed_version, baseline.version);

        let mut wakes = coordinator.subscribe();
        assert!(
            tokio::time::timeout(Duration::from_millis(150), wakes.recv())
                .await
                .is_err(),
            "ordinary background progress must update the TUI without waking the model"
        );
        let wake = tokio::time::timeout(Duration::from_secs(2), wakes.recv())
            .await
            .expect("completion should wake promptly")
            .expect("wake channel should remain open");
        assert_eq!(wake.wait.target_id, task.id);
        assert_eq!(wake.reason, "succeeded");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), wakes.recv())
                .await
                .is_err(),
            "a parked background wait must wake the model only once"
        );
    }

    #[tokio::test]
    async fn tasks_tool_stops_running_task() {
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let temp_dir = tempfile::TempDir::new().unwrap();
        let task = registry
            .start(&shell(), "sleep 5", temp_dir.path(), 30)
            .await
            .unwrap();

        let tool = TasksTool::new(registry);
        let stopped = tool
            .execute(serde_json::json!({"action": "stop", "task_id": task.id}))
            .await
            .unwrap();

        assert!(matches!(stopped, ToolOutput::Text(text) if text.contains("stopped")));
    }

    #[tokio::test]
    async fn tasks_tool_requires_task_id_for_tail() {
        let tool = TasksTool::new(Arc::new(BackgroundTaskRegistry::new()));
        let err = tool
            .execute(serde_json::json!({"action": "tail"}))
            .await
            .expect_err("tail requires task_id");

        assert!(err.to_string().contains("task_id is required"));
    }

    #[tokio::test]
    async fn tasks_tool_waits_for_subagent_task_id() {
        let subagents = Arc::new(SubagentRegistry::new());
        let subtask_id = subagents.register("research", "scan", true);
        let tool =
            TasksTool::with_subagents(Arc::new(BackgroundTaskRegistry::new()), subagents.clone());
        let finisher = subagents.clone();
        let finish_id = subtask_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            finisher.finish(
                &finish_id,
                crate::subagent::SubagentStatus::Succeeded,
                Some("found it".into()),
            );
        });

        let waited = tool
            .execute(serde_json::json!({
                "action": "wait",
                "task_id": subtask_id,
                "wait_seconds": 1
            }))
            .await
            .unwrap();

        assert!(
            matches!(waited, ToolOutput::Text(text) if text.contains("Status: succeeded") && text.contains("found it"))
        );
    }

    #[tokio::test]
    async fn tasks_tool_lists_and_tails_subagents() {
        let subagents = Arc::new(SubagentRegistry::new());
        let subtask_id = subagents.register("research", "scan", true);
        let tool =
            TasksTool::with_subagents(Arc::new(BackgroundTaskRegistry::new()), subagents.clone());

        let listed = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        let tailed = tool
            .execute(serde_json::json!({"action": "tail", "task_id": subtask_id}))
            .await
            .unwrap();

        assert!(
            matches!(listed, ToolOutput::Text(text) if text.contains("background subagent") && text.contains("sub-1"))
        );
        assert!(matches!(tailed, ToolOutput::Text(text) if text.contains("Mode: background")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tasks_tool_lists_tails_and_stops_interactive_terminals() {
        let terminals = Arc::new(TerminalRegistry::new());
        let terminal = terminals
            .start(
                "/bin/sh",
                "printf terminal-output; sleep 30",
                std::path::Path::new("/"),
                60,
                None,
            )
            .await
            .expect("PTY fixture should start");
        let tool = TasksTool::with_subagents_and_terminals(
            Arc::new(BackgroundTaskRegistry::new()),
            Arc::new(SubagentRegistry::new()),
            terminals,
            None,
            false,
        );

        tokio::time::sleep(Duration::from_millis(20)).await;
        let listed = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .expect("list should succeed");
        let tailed = tool
            .execute(serde_json::json!({"action": "tail", "task_id": terminal.id}))
            .await
            .expect("tail should succeed");
        let waited = tool
            .execute(serde_json::json!({
                "action": "wait",
                "task_id": terminal.id,
                "wait_seconds": 0
            }))
            .await
            .expect("wait should succeed");
        let stopped = tool
            .execute(serde_json::json!({"action": "stop", "task_id": terminal.id}))
            .await
            .expect("stop should succeed");

        assert!(matches!(listed, ToolOutput::Text(text) if text.contains("pty-1")));
        assert!(matches!(
            tailed,
            ToolOutput::UntrustedContext { content, .. }
                if content.contains("terminal-output")
                    && content.contains("Normalized screen:")
                    && !content.contains("Output tail:")
        ));
        assert!(matches!(
            waited,
            ToolOutput::UntrustedContext { content, .. }
                if content.contains("Semantic version:") && !content.contains("Output tail:")
        ));
        assert!(matches!(
            stopped,
            ToolOutput::UntrustedContext { content, .. }
                if content.contains("Status: stopped") && !content.contains("Output tail:")
        ));
    }
}
