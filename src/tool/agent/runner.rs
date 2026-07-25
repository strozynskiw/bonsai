use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Result, anyhow};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::catalog::SubagentRunLimits;
use super::lifecycle::{ActiveSubagentRun, CancelSubagentOnDrop};
use crate::agent::{
    ActiveModelIdentity, Agent, AgentRunResult, ExecutionLane, ExecutionLaneKind, UsageTotals,
    UsageTurn,
};
use crate::model_catalog::ConnectionId;
use crate::output::SharedSink;
use crate::provider::{PromptEstimator, Provider};
use crate::self_review::SelfReviewMode;
use crate::subagent::{
    RecordingSink, SubagentModelChain, SubagentRegistry, SubagentRunGuard, SubagentStatus,
};
use crate::tool::read_evidence::DelegatedReadEvidence;
use crate::tool::{ProjectInfoRuntime, ReadTracker, ToolRegistry};

/// Most subagents allowed to run at once across a turn.
const MAX_CONCURRENT_SUBAGENTS: usize = 5;
const SELF_REVIEW_MAX_ITERATIONS: usize = 12;
const SELF_REVIEW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(360);
// Self-review runs the same slow reasoning models through the same "budget
// exhausted, conclude now" path as the delegation lanes, so a 45s conclude window
// cut them off mid-synthesis and discarded the review. Match the generous
// delegation conclude budget (catalog.rs SUBAGENT_CONCLUDE_TIMEOUT), scaled to
// self-review's tighter 270s wall-clock. Don't shrink this back.
const SELF_REVIEW_CONCLUDE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Runtime provider and model-budget settings for a nested subagent.
pub(crate) struct SubagentProviderConfig {
    pub(crate) provider: Box<dyn Provider>,
    pub(crate) context_budget_tokens: usize,
    pub(crate) prompt_estimator: PromptEstimator,
    /// The model this config targets (the override model, or the parent's current
    /// model), recorded on the run so `/subagents` can show what it used.
    pub(crate) model: String,
    /// Exact provider/model attribution for persisted per-turn usage. Test-only
    /// factories may omit it; production resolution always supplies it.
    pub(crate) active_model_identity: Option<ActiveModelIdentity>,
    /// Optional one-shot backup resolved alongside the primary assignment.
    pub(crate) backup: Option<Box<SubagentProviderConfig>>,
}

impl SubagentProviderConfig {
    pub(crate) fn new(
        provider: Box<dyn Provider>,
        context_budget_tokens: usize,
        prompt_estimator: PromptEstimator,
        model: String,
    ) -> Self {
        Self {
            provider,
            context_budget_tokens,
            prompt_estimator,
            model,
            active_model_identity: None,
            backup: None,
        }
    }

    #[must_use]
    pub(crate) fn with_active_model_identity(mut self, provider_id: &str, model: &str) -> Self {
        self.active_model_identity = Some(ActiveModelIdentity {
            provider_id: provider_id.parse().unwrap_or_else(|_| ConnectionId::fallback("unknown")),
            model: model.to_string(),
        });
        self
    }

    #[must_use]
    pub(crate) fn with_backup(mut self, backup: Option<Self>) -> Self {
        self.backup = backup.map(Box::new);
        self
    }
}

type SubagentProviderFuture = Pin<Box<dyn Future<Output = SubagentProviderConfig> + Send>>;

/// Mints a fresh provider and model-budget settings for a nested subagent. Takes
/// the agent name (so a live `/subagents` override can be looked up) and the
/// agent's declared frontmatter override; precedence is **live > frontmatter >
/// parent's current model**.
pub(crate) type SubagentProviderFactory =
    Arc<dyn Fn(String, SubagentModelChain) -> SubagentProviderFuture + Send + Sync>;

/// Rebuilds a scoped subagent registry for one run.
///
/// The source registry carries the granted tool names. The factory swaps in
/// run-local instances for tools that hold mutable safety state, such as the
/// read-before-write tracker.
pub(crate) type SubagentToolRegistryFactory = Arc<
    dyn Fn(&ToolRegistry, Arc<ProjectInfoRuntime>, ReadTracker) -> Result<Arc<ToolRegistry>>
        + Send
        + Sync,
>;

/// Zero usage that reads as *priced* (`cost_micros: Some(0)`), returned alongside
/// failures that happen before the nested model receives a request.
fn zero_priced_usage() -> UsageTotals {
    UsageTotals {
        cost_micros: Some(0),
        ..UsageTotals::default()
    }
}

/// The shared core that runs a nested subagent to completion: builds a
/// fresh [`Agent`], runs it on a [`RecordingSink`] (so `/subagents` shows it live),
/// and returns only its conclusion + token usage. Used by both the `agent` tool
/// and the self-review pass. `Clone` (all fields are `Arc`/cheap), so one runner
/// can be shared between the tool and the parent agent.
#[derive(Clone)]
pub(crate) struct SubagentRunner {
    provider_factory: SubagentProviderFactory,
    /// Read-only registry built-in subagents run with. Excludes write/mutating
    /// tools and the `agent` tool itself (no recursion).
    sub_registry: Arc<ToolRegistry>,
    /// The full grantable registry (read-only + write/edit/bash/rename/…), the
    /// filter source for a *custom* subagent's declared `tools:`. Mutating tools
    /// prompt under the current policy. Built-ins never use this.
    full_registry: Arc<ToolRegistry>,
    /// Rebuilds the granted registry for a single run, binding tools that carry
    /// safety state to that run's fresh read tracker.
    registry_factory: SubagentToolRegistryFactory,
    /// Base project_info runtime; cloned per run so concurrent subagents don't
    /// clobber each other's advertised tool list.
    project_info_runtime: Arc<ProjectInfoRuntime>,
    /// Live registry of subagent runs, surfaced by `/subagents`.
    subagents: Arc<SubagentRegistry>,
    pub(super) project_root: PathBuf,
    semaphore: Arc<Semaphore>,
    active_runs: Arc<StdMutex<BTreeMap<String, CancellationToken>>>,
    background_wake_enabled: bool,
}

impl std::fmt::Debug for SubagentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentRunner")
            .field("project_root", &self.project_root)
            .finish_non_exhaustive()
    }
}

pub(super) struct SubagentRunSpec {
    pub(super) label: String,
    pub(super) instructions: String,
    pub(super) task: String,
    pub(super) registry: Arc<ToolRegistry>,
    pub(super) model_chain: SubagentModelChain,
    pub(super) lane_kind: ExecutionLaneKind,
    pub(super) limits: SubagentRunLimits,
}

impl SubagentRunner {
    #[cfg(test)]
    pub(crate) fn new(
        provider_factory: SubagentProviderFactory,
        sub_registry: Arc<ToolRegistry>,
        full_registry: Arc<ToolRegistry>,
        _read_tracker: ReadTracker,
        project_info_runtime: Arc<ProjectInfoRuntime>,
        subagents: Arc<SubagentRegistry>,
        project_root: PathBuf,
    ) -> Self {
        let registry_factory = default_subagent_registry_factory(project_root.clone());
        Self::new_with_registry_factory(
            provider_factory,
            sub_registry,
            full_registry,
            registry_factory,
            project_info_runtime,
            subagents,
            project_root,
        )
    }

    pub(crate) fn new_with_registry_factory(
        provider_factory: SubagentProviderFactory,
        sub_registry: Arc<ToolRegistry>,
        full_registry: Arc<ToolRegistry>,
        registry_factory: SubagentToolRegistryFactory,
        project_info_runtime: Arc<ProjectInfoRuntime>,
        subagents: Arc<SubagentRegistry>,
        project_root: PathBuf,
    ) -> Self {
        Self {
            provider_factory,
            sub_registry,
            full_registry,
            registry_factory,
            project_info_runtime,
            subagents,
            project_root,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_SUBAGENTS)),
            active_runs: Arc::new(StdMutex::new(BTreeMap::new())),
            background_wake_enabled: false,
        }
    }

    #[must_use]
    pub(crate) fn with_background_wake(mut self) -> Self {
        self.background_wake_enabled = true;
        self
    }

    pub(crate) fn has_background_wake(&self) -> bool {
        self.background_wake_enabled
    }

    /// The default read-only registry, used by built-in agents and as the default
    /// (`tools:` unset) for a custom agent.
    pub(crate) fn read_only_registry(&self) -> &Arc<ToolRegistry> {
        &self.sub_registry
    }

    /// The full grantable registry a *custom* subagent's `tools:` scopes from.
    pub(crate) fn full_registry(&self) -> &Arc<ToolRegistry> {
        &self.full_registry
    }

    /// The read-only registry with `git` removed, for the self-review reviewer.
    /// The reviewer is handed a curated, baseline-scoped diff in its task and
    /// must judge *that*; letting it run its own `git diff` would pull in
    /// unrelated working-tree changes the baseline deliberately excluded.
    /// Enforcing the exclusion in the registry — not just the prose instruction
    /// the model can ignore — is the real guardrail.
    pub(crate) fn review_registry(&self) -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        for name in self.sub_registry.names() {
            if name == "git" {
                continue;
            }
            if let Some(tool) = self.sub_registry.get(name) {
                registry.register(tool);
            }
        }
        Arc::new(registry)
    }

    /// Cancel every currently running subagent backed by this runner. This is
    /// used by parent-run cancellation so the `/subagents` state and nested
    /// provider calls are stopped together.
    pub(crate) fn cancel_all_running(&self) -> usize {
        let active = {
            let active_runs = self.active_runs.lock().unwrap_or_else(|p| p.into_inner());
            active_runs
                .iter()
                .map(|(id, token)| (id.clone(), token.clone()))
                .collect::<Vec<_>>()
        };
        for (_id, token) in &active {
            token.cancel();
        }
        for (id, _token) in &active {
            self.subagents.finish(id, SubagentStatus::Cancelled, None);
        }
        active.len()
    }

    pub(crate) fn subagents(&self) -> &Arc<SubagentRegistry> {
        &self.subagents
    }

    pub(crate) async fn run_self_review(
        &self,
        instructions: &str,
        task: &str,
        registry: Arc<ToolRegistry>,
        cancellation: CancellationToken,
        model_override: Option<crate::subagent::SubagentModelOverride>,
    ) -> (
        Result<String>,
        UsageTotals,
        Vec<UsageTurn>,
        Vec<DelegatedReadEvidence>,
    ) {
        self.run_new(
            SubagentRunSpec {
                label: "self-review".to_string(),
                instructions: instructions.to_string(),
                task: task.to_string(),
                registry,
                model_chain: SubagentModelChain::single(model_override),
                lane_kind: ExecutionLaneKind::SelfReview,
                limits: SubagentRunLimits {
                    max_iterations: SELF_REVIEW_MAX_ITERATIONS,
                    timeout: SELF_REVIEW_TIMEOUT,
                    conclude_timeout: SELF_REVIEW_CONCLUDE_TIMEOUT,
                },
            },
            cancellation,
        )
        .await
    }

    pub(super) async fn run_new(
        &self,
        spec: SubagentRunSpec,
        cancellation: CancellationToken,
    ) -> (
        Result<String>,
        UsageTotals,
        Vec<UsageTurn>,
        Vec<DelegatedReadEvidence>,
    ) {
        let subtask_id = self.subagents.register(&spec.label, &spec.task, false);
        // Link the run to the `agent` tool call driving it (foreground runs
        // never reach the detached attach path in the run loop), so the TUI
        // can surface run details — e.g. the actual model — on that call.
        // Internal lanes (self-review) run outside a tool dispatch and get no id.
        if let Some(tool_call_id) = crate::tool::current_tool_call_id() {
            let _ = self.subagents.attach_tool_call(&subtask_id, tool_call_id);
        }
        self.run_registered(spec, subtask_id, cancellation, true)
            .await
    }

    /// Start a read-only detached subagent and return immediately. The spawned
    /// run records completion in [`SubagentRegistry`]; the parent agent drains it
    /// later through the background-context wake path.
    pub(super) fn run_in_background(
        &self,
        label: &str,
        instructions: &str,
        task: &str,
        registry: Arc<ToolRegistry>,
        model_chain: SubagentModelChain,
        limits: SubagentRunLimits,
    ) -> String {
        let subtask_id = self.subagents.register(label, task, true);
        let runner = self.clone();
        let spec = SubagentRunSpec {
            label: label.to_string(),
            instructions: instructions.to_string(),
            task: task.to_string(),
            registry,
            model_chain,
            lane_kind: ExecutionLaneKind::Subagent,
            limits,
        };
        let token = CancellationToken::new();
        // Track the token before spawning. If Ctrl+C lands after this method
        // returns but before the spawned future is first polled, cancellation
        // must still reach the detached run.
        let active_run =
            ActiveSubagentRun::insert(self.active_runs.clone(), subtask_id.clone(), token.clone());
        tokio::spawn({
            let subtask_id = subtask_id.clone();
            async move {
                let _active_run = active_run;
                let _ = runner.run_registered(spec, subtask_id, token, false).await;
            }
        });
        subtask_id
    }

    async fn run_registered(
        &self,
        spec: SubagentRunSpec,
        subtask_id: String,
        cancellation: CancellationToken,
        track_active: bool,
    ) -> (
        Result<String>,
        UsageTotals,
        Vec<UsageTurn>,
        Vec<DelegatedReadEvidence>,
    ) {
        // A child token lets this nested run be cancelled from the parent token,
        // while the drop guard covers hard parent aborts where the parent token
        // might otherwise never be flipped before the future is dropped.
        let run_cancellation = cancellation.child_token();
        let _cancellation_guard = CancelSubagentOnDrop::new(run_cancellation.clone());
        let guard = SubagentRunGuard::new(self.subagents.clone(), subtask_id.clone());
        let _active_run = track_active.then(|| {
            ActiveSubagentRun::insert(
                self.active_runs.clone(),
                subtask_id.clone(),
                run_cancellation.clone(),
            )
        });

        // These set-up failures happen before any model request, so no usage was
        // incurred; report zero-but-priced usage so absorbing it stays a no-op.
        let run_read_tracker = ReadTracker::new();
        let (run_registry, project_info_runtime) =
            match self.registry_for_run(&spec.registry, run_read_tracker.clone()) {
                Ok(value) => value,
                Err(err) => {
                    guard.finish_with_usage(
                        SubagentStatus::Failed,
                        Some(format!("error: {err}")),
                        Some(zero_priced_usage()),
                        Vec::new(),
                    );
                    return (Err(err), zero_priced_usage(), Vec::new(), Vec::new());
                }
            };

        // Bound how many subagents run at once.
        let _permit = match tokio::select! {
            biased;
            _ = run_cancellation.cancelled() => {
                guard.finish_with_usage(
                    SubagentStatus::Cancelled,
                    None,
                    Some(zero_priced_usage()),
                    Vec::new(),
                );
                return (
                    Err(anyhow!("'{}' subagent cancelled", spec.label)),
                    zero_priced_usage(),
                    Vec::new(),
                    Vec::new(),
                );
            }
            permit = self.semaphore.acquire() => permit,
        } {
            Ok(permit) => permit,
            Err(_) => {
                guard.finish_with_usage(
                    SubagentStatus::Failed,
                    Some("error: subagent scheduler unavailable".to_string()),
                    Some(zero_priced_usage()),
                    Vec::new(),
                );
                return (
                    Err(anyhow!("subagent scheduler unavailable")),
                    zero_priced_usage(),
                    Vec::new(),
                    Vec::new(),
                );
            }
        };

        if run_cancellation.is_cancelled() {
            guard.finish_with_usage(
                SubagentStatus::Cancelled,
                None,
                Some(zero_priced_usage()),
                Vec::new(),
            );
            return (
                Err(anyhow!("'{}' subagent cancelled", spec.label)),
                zero_priced_usage(),
                Vec::new(),
                Vec::new(),
            );
        }

        let provider_config = (self.provider_factory)(spec.label.clone(), spec.model_chain);
        let run_config = tokio::select! {
            biased;
            _ = run_cancellation.cancelled() => {
                guard.finish_with_usage(
                    SubagentStatus::Cancelled,
                    None,
                    Some(zero_priced_usage()),
                    Vec::new(),
                );
                return (
                    Err(anyhow!("'{}' subagent cancelled", spec.label)),
                    zero_priced_usage(),
                    Vec::new(),
                    Vec::new(),
                );
            }
            config = provider_config => config,
        };
        let SubagentProviderConfig {
            provider,
            context_budget_tokens,
            prompt_estimator,
            model,
            active_model_identity,
            backup,
        } = run_config;
        // Record which model this run actually used (the override or the base
        // model), so `/subagents` can show when an override took effect.
        self.subagents.set_model(&subtask_id, model);
        let max_iterations = spec.limits.max_iterations;
        let builder = Agent::builder(
            provider,
            run_registry.clone(),
            run_registry,
            run_read_tracker,
            self.project_root.clone(),
        )
        .max_iterations(max_iterations)
        .context_budget_tokens(context_budget_tokens)
        .execution_lane(match spec.lane_kind {
            ExecutionLaneKind::SelfReview => ExecutionLane::self_review(subtask_id.clone()),
            _ => ExecutionLane::subagent(subtask_id.clone()),
        })
        .project_info_runtime(project_info_runtime)
        .tool_origin(format!("subagent {} ({subtask_id})", spec.label))
        // A scoped delegation never self-reviews.
        .self_review_mode(SelfReviewMode::Off);
        let builder = match active_model_identity {
            Some(identity) => builder.active_model_identity(identity),
            None => builder,
        };
        let mut subagent = match builder.build() {
            Ok(subagent) => subagent,
            Err(err) => {
                guard.finish_with_usage(
                    SubagentStatus::Failed,
                    Some(format!("error: {err}")),
                    Some(zero_priced_usage()),
                    Vec::new(),
                );
                return (Err(err), zero_priced_usage(), Vec::new(), Vec::new());
            }
        };
        subagent.set_prompt_estimator(prompt_estimator);
        if let Some(backup) = backup {
            subagent.set_provider_fallback(*backup);
        }

        let framed = format!("{}\n\n# Task\n{}", spec.instructions, spec.task);
        // Record the subagent's live activity so `/subagents` can show it while it
        // runs; only the final conclusion crosses back to the caller.
        let sink: SharedSink = Arc::new(RecordingSink::new(
            self.subagents.clone(),
            subtask_id.clone(),
        ));
        let timeout = spec.limits.timeout;
        let initial_cancellation = run_cancellation.child_token();
        let run = subagent.run(&framed, initial_cancellation.clone(), sink.clone());
        let mut outcome = tokio::time::timeout(timeout, run).await;
        if outcome.is_err() {
            initial_cancellation.cancel();
        }
        let limit_expired = match &outcome {
            Err(_) => true,
            Ok(Err(err)) => iteration_limit_expired(err, spec.limits.max_iterations),
            _ => false,
        };
        if limit_expired {
            subagent.restrict_to_conclusion_turn();
            let conclude = subagent.run(
                "Your subagent budget is exhausted. Conclude with your concrete findings now. Tools are disabled; return only the final conclusion.",
                run_cancellation.child_token(),
                sink,
            );
            outcome = tokio::time::timeout(spec.limits.conclude_timeout, conclude).await;
        }

        // `run` borrows `&mut subagent`, so once the future resolves (success,
        // error, or timeout-drop) the subagent is readable again — capture its
        // usage here so every branch below can return it.
        let usage = subagent.usage_totals();
        let usage_turns = subagent.usage_turns().to_vec();
        let partial_result = self
            .subagents
            .snapshot(&subtask_id)
            .and_then(|snapshot| snapshot.partial_result());

        let (result, delegated_read_evidence) = match outcome {
            Ok(Ok(run_result)) => {
                // A run cancelled via the token returns `Interrupted` with partial
                // output; treat it as cancelled rather than a real conclusion.
                if run_cancellation.is_cancelled() {
                    guard.finish_with_usage(
                        SubagentStatus::Cancelled,
                        None,
                        Some(usage),
                        usage_turns.clone(),
                    );
                    (
                        Err(anyhow!("'{}' subagent cancelled", spec.label)),
                        Vec::new(),
                    )
                } else {
                    match run_result {
                        AgentRunResult::Completed(conclusion)
                        | AgentRunResult::Interrupted(conclusion) => {
                            let evidence =
                                subagent.delegated_read_evidence_manifest(&subtask_id, &conclusion);
                            self.subagents
                                .set_delegated_read_evidence(&subtask_id, evidence.clone());
                            guard.finish_with_usage(
                                SubagentStatus::Succeeded,
                                Some(conclusion.clone()),
                                Some(usage),
                                usage_turns.clone(),
                            );
                            (Ok(conclusion), evidence)
                        }
                        AgentRunResult::Waiting(reason) => {
                            guard.finish_with_usage(
                                SubagentStatus::Failed,
                                Some(format!("nested wait state: {reason:?}")),
                                Some(usage),
                                usage_turns.clone(),
                            );
                            (
                                Err(anyhow!("nested agent entered wait state: {reason:?}")),
                                Vec::new(),
                            )
                        }
                    }
                }
            }
            Ok(Err(err)) => {
                let error_text = format!("{err:#}");
                let report = failure_with_partial(&error_text, partial_result.as_deref());
                let evidence = subagent.delegated_read_evidence_manifest(
                    &subtask_id,
                    partial_result.as_deref().unwrap_or(""),
                );
                self.subagents
                    .set_delegated_read_evidence(&subtask_id, evidence.clone());
                guard.finish_with_usage(
                    SubagentStatus::Failed,
                    Some(report.clone()),
                    Some(usage),
                    usage_turns.clone(),
                );
                (Err(anyhow!(report)), evidence)
            }
            Err(_) => {
                run_cancellation.cancel();
                let timeout_message = format!(
                    "'{}' subagent timed out after {}s",
                    spec.label,
                    if limit_expired {
                        spec.limits.conclude_timeout.as_secs()
                    } else {
                        timeout.as_secs()
                    }
                );
                let report = failure_with_partial(&timeout_message, partial_result.as_deref());
                let evidence = subagent.delegated_read_evidence_manifest(
                    &subtask_id,
                    partial_result.as_deref().unwrap_or(""),
                );
                self.subagents
                    .set_delegated_read_evidence(&subtask_id, evidence.clone());
                guard.finish_with_usage(
                    SubagentStatus::TimedOut,
                    Some(report.clone()),
                    Some(usage),
                    usage_turns.clone(),
                );
                (Err(anyhow!(report)), evidence)
            }
        };
        (result, usage, usage_turns, delegated_read_evidence)
    }

    /// Clone a scoped registry for a single nested run, replacing run-local
    /// tools so concurrent scoped agents cannot share mutable safety state.
    pub(super) fn registry_for_run(
        &self,
        registry: &ToolRegistry,
        read_tracker: ReadTracker,
    ) -> Result<(Arc<ToolRegistry>, Arc<ProjectInfoRuntime>)> {
        let runtime = Arc::new(ProjectInfoRuntime::new(
            self.project_info_runtime.provider_state(),
        ));
        let isolated = (self.registry_factory)(registry, runtime.clone(), read_tracker)?;
        Ok((isolated, runtime))
    }
}

fn failure_with_partial(error: &str, partial: Option<&str>) -> String {
    match partial {
        Some(partial) => format!("{error}\n\n{partial}"),
        None => error.to_string(),
    }
}

fn iteration_limit_expired(error: &anyhow::Error, max_iterations: usize) -> bool {
    matches!(
        error.downcast_ref::<crate::run_budget::RunBudgetExhaustion>(),
        Some(crate::run_budget::RunBudgetExhaustion::MaxTurns { limit })
            if *limit == max_iterations
    )
}

#[cfg(test)]
fn default_subagent_registry_factory(project_root: PathBuf) -> SubagentToolRegistryFactory {
    Arc::new(move |registry, runtime, _read_tracker| {
        let mut isolated = ToolRegistry::new();
        for name in registry.names() {
            if name == "project_info" {
                isolated.register(Arc::new(crate::tool::ProjectInfoTool::new(
                    project_root.clone(),
                    runtime.clone(),
                )));
                continue;
            }
            let Some(tool) = registry.get(name) else {
                return Err(anyhow!(
                    "subagent registry entry '{name}' disappeared while preparing a run"
                ));
            };
            isolated.register(tool);
        }
        Ok(Arc::new(isolated))
    })
}
