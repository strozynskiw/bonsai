use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::agent::{
    AgentMode, AgentRunResult, QueuedUserMessage, QueuedUserMessageCommand, UserInput,
};
use crate::background::BackgroundTaskRegistry;
use crate::context;
use crate::headless::{ProviderSelection, select_provider};
use crate::interaction::InteractionService;
use crate::output::{OutputSink, SharedSink, ToolCallStart};
use crate::permissions::PermissionManager;
use crate::plan::PlanDoc;
use crate::provider::{
    DEFAULT_CONTEXT_WINDOW_TOKENS, PromptEstimator, Provider, ProviderRegistry,
    ProviderRequestDiagnostics, ReasoningSelection, StreamedResponse,
};
use crate::session::SessionStore;
use crate::storage::{SessionId, SessionStatus, Storage};
use crate::todo::TodoStore;
use crate::tool::{ProjectInfoProviderState, ReadTracker, SharedActiveSessionId};

use super::*;

const BACKGROUND_TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_EVAL_REPETITIONS: usize = 1;
const EVAL_CONTEXT_WINDOW_TOKENS_ENV: &str = "BONSAI_EVAL_CONTEXT_WINDOW_TOKENS";
const MIN_EVAL_CONTEXT_WINDOW_TOKENS: usize = 32_768;

fn parse_eval_context_window_tokens(value: &str) -> Result<usize> {
    let tokens = value.parse::<usize>().with_context(|| {
        format!("{EVAL_CONTEXT_WINDOW_TOKENS_ENV} must be an integer number of tokens")
    })?;
    if tokens < MIN_EVAL_CONTEXT_WINDOW_TOKENS {
        anyhow::bail!(
            "{EVAL_CONTEXT_WINDOW_TOKENS_ENV} must be at least {MIN_EVAL_CONTEXT_WINDOW_TOKENS}"
        );
    }
    Ok(tokens)
}

fn configured_eval_context_window_tokens() -> Result<Option<usize>> {
    let Some(value) = std::env::var_os(EVAL_CONTEXT_WINDOW_TOKENS_ENV) else {
        return Ok(None);
    };
    let value = value.to_str().ok_or_else(|| {
        anyhow::anyhow!("{EVAL_CONTEXT_WINDOW_TOKENS_ENV} must contain UTF-8 digits")
    })?;
    parse_eval_context_window_tokens(value).map(Some)
}

/// Paths and pass/fail totals returned to the CLI after a completed eval run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvalRunOutcome {
    pub report_path: PathBuf,
    pub summary_path: PathBuf,
    pub run_dir: PathBuf,
    pub passed_tasks: usize,
    pub total_tasks: usize,
    pub fail_on_task_failure: bool,
    pub baseline_regression: bool,
}

impl EvalRunOutcome {
    pub(crate) fn should_fail_process(&self) -> bool {
        (self.fail_on_task_failure && self.passed_tasks != self.total_tasks)
            || self.baseline_regression
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test]
    fn baseline_regression_fails_process_without_task_failure_flag() {
        let outcome = EvalRunOutcome {
            report_path: PathBuf::new(),
            summary_path: PathBuf::new(),
            run_dir: PathBuf::new(),
            passed_tasks: 1,
            total_tasks: 1,
            fail_on_task_failure: false,
            baseline_regression: true,
        };

        assert!(outcome.should_fail_process());
    }
}

/// Run an eval suite end-to-end: load + validate the suite, execute each task,
/// then write the JSON report and Markdown summary.
///
/// # Errors
/// Returns an error if the suite fails to load/validate, the provider cannot be
/// set up, output directories cannot be created, or report files fail to write.
pub(crate) async fn run(config: EvalCliConfig) -> Result<EvalRunOutcome> {
    let suite = EvalSuite::load(&config.suite)?;
    let selected_tasks = select_tasks(&suite, config.task.as_deref())?;
    let seed = config.seed.unwrap_or(suite.seed);
    let run_id = run_id(config.mode, seed);
    let run_dir = config.out_dir.join(&run_id);
    fs::create_dir_all(&run_dir)
        .with_context(|| format!("Failed to create eval output directory {:?}", run_dir))?;

    let storage = Storage::open_at(run_dir.join("storage").join("bonsai.db")).await?;
    let provider_setup = ProviderSetup::new(&config).await?;
    let reasoning = provider_setup.reasoning().label();
    let baseline = config
        .baseline
        .as_deref()
        .map(load_eval_baseline)
        .transpose()?;
    if let Some(baseline) = &baseline {
        baseline.require_profile(
            &suite.id,
            provider_setup.provider_id(),
            provider_setup.model(),
            &reasoning,
        )?;
    }
    let mut task_reports =
        Vec::with_capacity(selected_tasks.len().saturating_mul(suite.repetitions));
    let started = Instant::now();

    for task in selected_tasks {
        for repetition in 1..=suite.repetitions {
            let task_run_id = repeated_task_id(&task.id, repetition, suite.repetitions);
            let repetition_offset = u64::try_from(repetition.saturating_sub(1)).unwrap_or(u64::MAX);
            task_reports.push(
                run_task(
                    &suite,
                    task,
                    &task_run_id,
                    seed.saturating_add(repetition_offset),
                    &run_dir,
                    &storage,
                    &provider_setup,
                )
                .await?,
            );
        }
    }

    let usage = UsageReport::sum(task_reports.iter().map(|task| task.usage));
    let score = ScoreReport::from_tasks(&task_reports);
    let duration_ms = millis_u64(started.elapsed());
    let cache_reuse_percent = aggregate_cache_reuse_percent(&task_reports);
    let repair_turns = task_reports
        .iter()
        .map(|task| task.repair_turns)
        .sum::<u64>();
    let mut report = EvalReport {
        run_id,
        suite: SuiteReport {
            id: suite.id.clone(),
            path: suite.path.display().to_string(),
            repetitions: suite.repetitions,
        },
        mode: config.mode,
        provider: provider_setup.provider_id().to_string(),
        model: provider_setup.model().to_string(),
        reasoning,
        seed,
        score,
        usage,
        cost_micros: usage.cost_micros,
        tokens_per_dollar: tokens_per_dollar(usage),
        duration_ms,
        cache_reuse_percent,
        repair_turns,
        baseline: None,
        output_dir: run_dir.display().to_string(),
        tasks: task_reports,
    };
    report.baseline = baseline
        .as_ref()
        .map(|baseline| baseline.compare(&report))
        .transpose()?;

    let report_path = run_dir.join("report.json");
    let summary_path = run_dir.join("summary.md");
    let report_json = serde_json::to_string_pretty(&report)?;
    write_file(&report_path, &format!("{report_json}\n"), "report")?;
    write_file(&summary_path, &format_eval_summary(&report), "summary")?;

    if config.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "eval {}: {}/{} tasks passed ({:.1}%)",
            report.suite.id, report.score.passed, report.score.total, report.score.percent
        );
        println!("report: {}", report_path.display());
        println!("summary: {}", summary_path.display());
        if let Some(baseline) = &report.baseline {
            println!(
                "baseline: {} (score floor {:.1}%, delta {:+.1} points)",
                if baseline.passed { "pass" } else { "fail" },
                baseline.minimum_score_percent,
                baseline.deltas.score_percent
            );
        }
    }

    let baseline_regression = report
        .baseline
        .as_ref()
        .is_some_and(|baseline| !baseline.passed);

    Ok(EvalRunOutcome {
        report_path,
        summary_path,
        run_dir,
        passed_tasks: report.score.passed,
        total_tasks: report.score.total,
        fail_on_task_failure: config.fail_on_task_failure,
        baseline_regression,
    })
}

fn select_tasks<'a>(suite: &'a EvalSuite, task_id: Option<&str>) -> Result<Vec<&'a EvalTask>> {
    if let Some(task_id) = task_id {
        let task = suite
            .tasks
            .iter()
            .find(|task| task.id == task_id)
            .with_context(|| format!("Task '{task_id}' was not found in suite '{}'", suite.id))?;
        return Ok(vec![task]);
    }
    Ok(suite.tasks.iter().collect())
}

fn repeated_task_id(task_id: &str, repetition: usize, repetitions: usize) -> String {
    if repetitions == 1 {
        task_id.to_string()
    } else {
        format!("{task_id}-run-{repetition}")
    }
}

/// Provider backend for an eval run, resolved once before tasks execute.
#[derive(Debug)]
enum ProviderSetup {
    Mock,
    Live {
        registry: Arc<ProviderRegistry>,
        session_store: Box<SessionStore>,
        selection: ProviderSelection,
        model_catalog: Box<crate::model_catalog::ModelCatalog>,
    },
}

impl ProviderSetup {
    async fn new(config: &EvalCliConfig) -> Result<Self> {
        match config.mode {
            EvalMode::Mock => Ok(Self::Mock),
            EvalMode::Live => {
                let user_storage = Storage::open().await?;
                let model_catalog = match crate::model_catalog::load_catalog_from_home_with_refresh(
                    user_storage.home_dir(),
                )
                .await
                {
                    Ok(catalog) => catalog,
                    Err(err) => {
                        tracing::warn!(
                            home = %user_storage.home_dir().display(),
                            error = %err,
                            "failed to load user model catalog for eval; falling back to built-in catalog"
                        );
                        crate::model_catalog::ModelCatalog::load_builtin()?
                    }
                };
                let registry = Arc::new(ProviderRegistry::from_catalog(&model_catalog));
                let mut session_store = SessionStore::load_with_storage_and_catalog(
                    &user_storage,
                    Some(&model_catalog),
                )
                .await?;
                let env_provider = std::env::var("BONSAI_PROVIDER").ok();
                let provider_override = config.provider.as_deref().or(env_provider.as_deref());
                let mut selection =
                    select_provider(&registry, &mut session_store, provider_override)?;
                if let Some(model) = config.model.as_deref() {
                    selection.model = crate::headless::apply_model_override(
                        &mut session_store,
                        &selection.provider,
                        &model_catalog,
                        model,
                    );
                }
                if let Some(effort) = config.effort {
                    let metadata = registry
                        .lookup(&selection.provider)
                        .with_context(|| format!("Unknown live provider '{}'", selection.provider))?
                        .metadata();
                    let applied = apply_eval_reasoning_override(
                        &mut session_store,
                        &model_catalog,
                        metadata,
                        &selection.provider,
                        &selection.model,
                        effort,
                    );
                    if applied != effort {
                        anyhow::bail!(
                            "Model '{}' on provider '{}' does not support eval effort '{}'",
                            selection.model,
                            selection.provider,
                            effort
                        );
                    }
                }
                Ok(Self::Live {
                    registry,
                    session_store: Box::new(session_store),
                    selection,
                    model_catalog: Box::new(model_catalog),
                })
            }
        }
    }

    fn provider_id(&self) -> &str {
        match self {
            Self::Mock => MOCK_PROVIDER_ID,
            Self::Live { selection, .. } => &selection.provider,
        }
    }

    fn model(&self) -> &str {
        match self {
            Self::Mock => MOCK_MODEL,
            Self::Live { selection, .. } => &selection.model,
        }
    }

    fn reasoning(&self) -> ReasoningSelection {
        match self {
            Self::Mock => ReasoningSelection::default(),
            Self::Live {
                session_store,
                selection,
                ..
            } => {
                session_store
                    .as_ref()
                    .session(&selection.provider)
                    .reasoning
            }
        }
    }

    fn project_info_provider_state(&self) -> ProjectInfoProviderState {
        match self {
            Self::Mock => ProjectInfoProviderState::new(MOCK_PROVIDER_ID, "Mock Eval", MOCK_MODEL),
            Self::Live {
                registry,
                session_store,
                ..
            } => crate::model_resolution::project_info_provider_state(
                registry,
                session_store.as_ref(),
            ),
        }
    }

    fn build_provider(
        &self,
        mock_script: Option<MockScript>,
        seed: u64,
        max_provider_attempts: Option<usize>,
    ) -> Result<(Box<dyn Provider>, Arc<EvalProviderStats>)> {
        let provider: Box<dyn Provider> = match self {
            Self::Mock => {
                let script = mock_script.context("Eval agent is missing a mock script")?;
                Box::new(MockEvalProvider::new(script, seed))
            }
            Self::Live {
                registry,
                session_store,
                selection,
                model_catalog,
            } => {
                let _ = registry
                    .get(&selection.provider)
                    .with_context(|| format!("Unknown live provider '{}'", selection.provider))?;
                crate::model_resolution::build_provider(
                    registry,
                    session_store.as_ref(),
                    Some(model_catalog.as_ref()),
                )
            }
        };
        let stats = Arc::new(EvalProviderStats::default());
        Ok((
            Box::new(CountingProvider::new(
                provider,
                stats.clone(),
                max_provider_attempts,
            )),
            stats,
        ))
    }

    fn configure_agent(
        &self,
        agent: &mut crate::agent::Agent,
        mock_context_window_tokens: Option<usize>,
    ) -> Result<()> {
        match self {
            Self::Mock => {
                agent.set_context_budget_tokens(
                    mock_context_window_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW_TOKENS as usize),
                );
                agent.set_prompt_estimator(PromptEstimator::heuristic());
            }
            Self::Live {
                registry,
                session_store,
                model_catalog,
                ..
            } => {
                let catalog_tokens =
                    crate::model_resolution::context_window_for_current_model_with_catalog(
                        registry,
                        session_store.as_ref(),
                        Some(model_catalog.as_ref()),
                    ) as usize;
                let context_budget_tokens = configured_eval_context_window_tokens()?
                    .map(|tokens| {
                        if tokens > catalog_tokens {
                            anyhow::bail!(
                                "{EVAL_CONTEXT_WINDOW_TOKENS_ENV} ({tokens}) cannot exceed the selected model's catalog window ({catalog_tokens})"
                            );
                        }
                        tracing::info!(
                            tokens,
                            catalog_tokens,
                            "live eval context-window override enabled"
                        );
                        Ok(tokens)
                    })
                    .transpose()?
                    .unwrap_or(catalog_tokens);
                agent.set_context_budget_tokens(context_budget_tokens);
                agent.set_prompt_estimator(
                    crate::model_resolution::prompt_estimator_for_current_model_with_catalog(
                        registry,
                        session_store.as_ref(),
                        Some(model_catalog.as_ref()),
                    ),
                );
            }
        }
        Ok(())
    }
}

fn apply_eval_reasoning_override(
    session_store: &mut SessionStore,
    model_catalog: &crate::model_catalog::ModelCatalog,
    metadata: &crate::provider::ProviderMetadata,
    provider_id: &str,
    model: &str,
    requested: ReasoningSelection,
) -> ReasoningSelection {
    let resolved = crate::model_resolution::resolved_model_for_provider_model(
        Some(model_catalog),
        provider_id,
        model,
    );
    let applied = resolved
        .as_ref()
        .map(|model| model.normalize_reasoning(requested))
        .unwrap_or_else(|| metadata.normalize_reasoning_for_model(model, requested));
    let session = session_store.session_mut(provider_id);
    session.reasoning = applied;
    session.model_reasoning.insert(model.to_string(), applied);
    if let Some(resolved) = resolved {
        session
            .model_reasoning
            .insert(resolved.model_id.to_string(), applied);
    }
    applied
}

struct EvalAgentBuild<'a> {
    worktree_path: &'a Path,
    storage: &'a Storage,
    provider_setup: &'a ProviderSetup,
    mock_script: Option<MockScript>,
    seed: u64,
    max_provider_attempts: Option<usize>,
    max_logical_turns: Option<usize>,
    mock_context_window_tokens: Option<usize>,
    enable_peer_context: bool,
    /// Force the episode store on even when the explicit environment kill
    /// switch is set, so episode-specific tasks stay deterministic in CI.
    enable_episodes: bool,
    /// Force the episode store off so pressure-compaction scenarios cannot be
    /// satisfied by relevance-driven episode eviction instead.
    disable_episodes: bool,
    profile: EvalAgentProfile,
    session_title: &'a str,
}

struct EvalAgentHarness {
    agent: crate::agent::Agent,
    provider_stats: Arc<EvalProviderStats>,
    background_tasks: Arc<BackgroundTaskRegistry>,
    sink: Arc<EvalSink>,
    session_id: SessionId,
}

async fn build_eval_agent(config: EvalAgentBuild<'_>) -> Result<EvalAgentHarness> {
    let read_tracker = ReadTracker::new();
    // Evals stay deterministic and isolated from the user's saved rules.
    let permissions = PermissionManager::memory_only();
    let domain_permissions = PermissionManager::memory_only_domains();
    let interaction = Arc::new(InteractionService::noninteractive());
    let todo_store = Arc::new(Mutex::new(TodoStore::new()));
    let plan_store = Arc::new(Mutex::new(PlanDoc::default()));
    let background_tasks = Arc::new(BackgroundTaskRegistry::new());
    let yolo_mode = eval_yolo_mode();
    let session_id = config
        .storage
        .start_session(
            config.worktree_path,
            config.provider_setup.provider_id(),
            config.provider_setup.model(),
            config.provider_setup.reasoning(),
        )
        .await?;
    config
        .storage
        .set_session_summary(session_id, config.session_title)
        .await?;
    config
        .storage
        .record_session_heartbeat(session_id, true)
        .await?;
    let active_session_id: SharedActiveSessionId = Arc::new(Mutex::new(Some(session_id)));
    let peer_bus = config.enable_peer_context.then(|| {
        Arc::new(crate::peer::PeerBus::new(
            config.storage.clone(),
            active_session_id.clone(),
            config.worktree_path.to_path_buf(),
        ))
    });
    let project_context = context::isolated_project_context_snapshot(config.worktree_path);
    let project_info_runtime = Arc::new(crate::tool::ProjectInfoRuntime::new(Some(
        config.provider_setup.project_info_provider_state(),
    )));
    let lsp_hub = Arc::new(crate::lsp::LspHub::new(config.worktree_path.to_path_buf()));
    let workspace_locks = if config.enable_peer_context {
        crate::tool::WorkspaceLockContext::new(
            config.worktree_path.to_path_buf(),
            config.storage.clone(),
            active_session_id.clone(),
        )
    } else {
        crate::tool::WorkspaceLockContext::disabled(config.worktree_path.to_path_buf())
    };
    // One shared store backs the recall tool and the agent ledger; enabled by
    // default or forced on for episode-asserting eval tasks.
    let episode_store = (!config.disable_episodes
        && (config.enable_episodes || crate::episode::episodes_enabled()))
    .then(crate::episode::SharedEpisodeStore::default);
    let (tool_registry, planning_registry, smol_registry, _subagent_runner) =
        crate::bootstrap::build_tool_registries(crate::bootstrap::ToolRegistryDeps {
            project_root: config.worktree_path.to_path_buf(),
            read_tracker: read_tracker.clone(),
            lsp_hub: lsp_hub.clone(),
            project_info_runtime: project_info_runtime.clone(),
            permissions,
            domain_permissions,
            interaction,
            todo_store: todo_store.clone(),
            plan_store,
            background_tasks: background_tasks.clone(),
            terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
            background_wakes: None,
            background_wakes_parkable: false,
            yolo_mode: yolo_mode.clone(),
            sandbox: crate::sandbox::CommandSandbox::disabled(),
            workspace_locks,
            session_title: crate::bootstrap::SessionTitleToolDeps {
                storage: config.storage.clone(),
                active_session_id,
            },
            skills: crate::resource::skill::shared_registry(
                crate::resource::skill::SkillRegistry::empty(),
            ),
            subagent_provider_factory: None,
            subagent_background_wake: false,
            peer_wake_when_done: false,
            subagents: Arc::new(crate::subagent::SubagentRegistry::new()),
            custom_agents: crate::resource::agent::shared_registry(
                crate::resource::agent::AgentRegistry::empty(),
            ),
            builtin_subagent_settings: crate::subagent::SharedBuiltinSubagentSettings::default(),
            peer_bus: peer_bus.clone(),
            memory: None,
            mcp_tools: Vec::new(),
            extensions: Arc::new(crate::extension::status::ExtensionRegistry::new()),
            hooks: Arc::new(crate::hooks::HookEngine::disabled()),
            authorization_ledger: crate::tool::AuthorizationLedger::disabled(),
            episode_store: episode_store.clone(),
        });

    let (provider, provider_stats) = config.provider_setup.build_provider(
        config.mock_script,
        config.seed,
        config.max_provider_attempts,
    )?;
    let mut agent_builder = crate::agent::Agent::builder(
        provider,
        tool_registry,
        planning_registry,
        read_tracker,
        config.worktree_path.to_path_buf(),
    )
    .smol_registry(smol_registry)
    .project_context_snapshot(project_context)
    .project_info_runtime(project_info_runtime)
    .lsp_hub(lsp_hub)
    .yolo_mode(yolo_mode)
    .self_review_mode(crate::self_review::SelfReviewMode::Off)
    // Scripted mock turns cannot wait on a dynamic background-task id, so
    // keep known-slow verification bash calls foreground and deterministic.
    .auto_background_verification(false);
    if let Some(max_turns) = config.max_logical_turns {
        agent_builder = agent_builder.max_iterations(max_turns);
    }
    if config.profile == EvalAgentProfile::Smol {
        agent_builder = agent_builder.smol_preference(crate::smol::SmolPreference::On);
    }
    if let Some(episode_store) = episode_store {
        agent_builder = agent_builder.episode_store(episode_store);
    }
    let mut agent = agent_builder.build()?;
    let conversation_cache_key = config.storage.conversation_cache_key(session_id).await?;
    agent.set_conversation_cache_key(&conversation_cache_key);
    agent.set_mode(AgentMode::Coding);
    agent.set_todo_store(todo_store);
    agent.set_background_tasks(background_tasks.clone());
    if let Some(peer_bus) = peer_bus {
        agent.set_peer_bus(peer_bus);
    }
    config
        .provider_setup
        .configure_agent(&mut agent, config.mock_context_window_tokens)?;
    if matches!(config.provider_setup, ProviderSetup::Mock) {
        agent.set_retry_backoff(crate::agent::RetryBackoff::Immediate);
    }

    Ok(EvalAgentHarness {
        agent,
        provider_stats,
        background_tasks,
        sink: Arc::new(EvalSink::default()),
        session_id,
    })
}

struct PeerEvalOutcome {
    session_id: SessionId,
    changed_files: Vec<String>,
}

async fn run_shared_workspace_peer(
    peer: &SharedWorkspacePeer,
    task_id: &str,
    worktree_path: &Path,
    storage: &Storage,
    provider_setup: &ProviderSetup,
    seed: u64,
    max_duration_ms: Option<u64>,
) -> Result<PeerEvalOutcome> {
    if !matches!(provider_setup, ProviderSetup::Mock) {
        anyhow::bail!("Task '{task_id}' shared-workspace peer is supported only in mock mode");
    }
    let session_title = format!("eval peer: {task_id}");
    let mut harness = build_eval_agent(EvalAgentBuild {
        worktree_path,
        storage,
        provider_setup,
        mock_script: Some(peer.mock.clone()),
        seed: seed.saturating_add(1),
        max_provider_attempts: Some(12),
        max_logical_turns: Some(12),
        mock_context_window_tokens: None,
        enable_peer_context: true,
        enable_episodes: false,
        disable_episodes: false,
        profile: EvalAgentProfile::Full,
        session_title: &session_title,
    })
    .await?;
    let sink: SharedSink = harness.sink.clone();
    let run = harness
        .agent
        .run(&peer.prompt, CancellationToken::new(), sink);
    let result = if let Some(max_duration_ms) = max_duration_ms {
        tokio::time::timeout(Duration::from_millis(max_duration_ms), run)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Shared-workspace peer exceeded the {max_duration_ms} ms duration budget"
                )
            })?
    } else {
        run.await
    }?;
    let _ = harness
        .background_tasks
        .stop_all_running(BACKGROUND_TASK_SHUTDOWN_TIMEOUT)
        .await;
    if !matches!(result, AgentRunResult::Completed(_)) {
        anyhow::bail!("Shared-workspace peer ended in non-completed state: {result:?}");
    }
    let changed_files = harness.sink.changed_paths();
    storage
        .record_session_file_changes(harness.session_id, &changed_files)
        .await?;
    storage
        .record_session_heartbeat(harness.session_id, false)
        .await?;
    Ok(PeerEvalOutcome {
        session_id: harness.session_id,
        changed_files,
    })
}

async fn run_task(
    suite: &EvalSuite,
    task: &EvalTask,
    task_run_id: &str,
    seed: u64,
    run_dir: &Path,
    storage: &Storage,
    provider_setup: &ProviderSetup,
) -> Result<TaskReport> {
    let started = Instant::now();
    let budgets = suite.budgets.overlay(task.budgets);
    let worktree_path = run_dir.join("worktrees").join(task_run_id);
    let fixture_path = SafeRelativePath::parse(&task.fixture, "fixture")?.join(&suite.base_dir);
    copy_dir_all(&fixture_path, &worktree_path)?;

    let session_title = format!("eval primary: {task_run_id}");
    let mut harness = build_eval_agent(EvalAgentBuild {
        worktree_path: &worktree_path,
        storage,
        provider_setup,
        mock_script: task.mock.clone(),
        seed,
        max_provider_attempts: budgets.max_provider_attempts,
        max_logical_turns: budgets.max_logical_turns,
        mock_context_window_tokens: task.mock_context_window_tokens,
        enable_peer_context: task.shared_workspace_peer.is_some(),
        enable_episodes: task.min_episode_evictions.is_some(),
        disable_episodes: task.disable_episodes,
        profile: task.profile,
        session_title: &session_title,
    })
    .await?;
    let session_id = harness.session_id;
    let provider_stats = harness.provider_stats.clone();
    let background_tasks = harness.background_tasks.clone();
    let eval_sink = harness.sink.clone();
    let agent = &mut harness.agent;
    let sink: SharedSink = eval_sink.clone();
    let cancellation_token = CancellationToken::new();
    let cancellation_task = if let Some(attempts) = task.cancel_after_provider_attempts {
        let token = cancellation_token.clone();
        let provider_stats = provider_stats.clone();
        Some(tokio::spawn(async move {
            provider_stats.wait_for_attempt(attempts).await;
            token.cancel();
        }))
    } else {
        task.cancel_after_ms.map(|delay_ms| {
            let token = cancellation_token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                token.cancel();
            })
        })
    };
    let run = async {
        if task.queued_messages.is_empty() {
            return agent
                .run(&task.prompt, cancellation_token, sink.clone())
                .await;
        }

        let (sender, receiver) = mpsc::unbounded_channel();
        for (index, text) in task.queued_messages.iter().enumerate() {
            let id = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            sender
                .send(QueuedUserMessageCommand::Send(QueuedUserMessage {
                    id,
                    display_text: text.clone(),
                    transcript_text: text.clone(),
                    input: UserInput::from_text(text),
                }))
                .map_err(|_| anyhow::anyhow!("Eval queued-message channel closed"))?;
        }
        drop(sender);
        agent
            .run_with_queue_control(
                UserInput::from_text(&task.prompt),
                cancellation_token.into(),
                sink.clone(),
                receiver,
            )
            .await
    };
    let mut run_result = if let Some(max_duration_ms) = budgets.max_duration_ms {
        match tokio::time::timeout(Duration::from_millis(max_duration_ms), run).await {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!(
                "Eval task exceeded the {} ms duration budget",
                max_duration_ms
            )),
        }
    } else {
        run.await
    };
    if let Some(task) = cancellation_task {
        task.abort();
    }
    let mut peer_outcome = None;
    if matches!(&run_result, Ok(AgentRunResult::Interrupted(_)))
        && let Some(peer) = &task.shared_workspace_peer
    {
        peer_outcome = Some(
            run_shared_workspace_peer(
                peer,
                task_run_id,
                &worktree_path,
                storage,
                provider_setup,
                seed,
                budgets.max_duration_ms,
            )
            .await?,
        );
    }
    if task.shared_workspace_peer.is_some() && peer_outcome.is_none() {
        anyhow::bail!(
            "Task '{}' did not reach its shared-workspace interleaving point",
            task.id
        );
    }
    if task.resume_after_interruption && matches!(&run_result, Ok(AgentRunResult::Interrupted(_))) {
        let resume = async {
            if let Some(prompt) = task.resume_prompt.as_deref() {
                agent.run(prompt, CancellationToken::new(), sink).await
            } else {
                agent
                    .run_current_context(CancellationToken::new(), sink)
                    .await
            }
        };
        run_result = if let Some(max_duration_ms) = budgets.max_duration_ms {
            match tokio::time::timeout(Duration::from_millis(max_duration_ms), resume).await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!(
                    "Resumed eval task exceeded the {} ms duration budget",
                    max_duration_ms
                )),
            }
        } else {
            resume.await
        };
    }
    let _ = background_tasks
        .stop_all_running(BACKGROUND_TASK_SHUTDOWN_TIMEOUT)
        .await;

    let (status, output, run_error) = match run_result {
        Ok(AgentRunResult::Completed(output)) => (TaskStatus::Completed, output, None),
        Ok(AgentRunResult::Incomplete { output, failure }) => (
            match failure.outcome {
                crate::agent::CompletionFailureOutcome::Blocked => TaskStatus::Blocked,
                crate::agent::CompletionFailureOutcome::Failed => TaskStatus::Failed,
                crate::agent::CompletionFailureOutcome::Cancelled => TaskStatus::Interrupted,
            },
            output,
            Some(failure.detail),
        ),
        Ok(AgentRunResult::Interrupted(output)) => (TaskStatus::Interrupted, output, None),
        Ok(AgentRunResult::Waiting(reason)) => (
            TaskStatus::Error,
            String::new(),
            Some(format!("agent entered nonterminal wait state: {reason:?}")),
        ),
        Err(err) => (TaskStatus::Error, String::new(), Some(err.to_string())),
    };
    let usage_totals = agent.usage_totals();
    let completion_guard = agent.completion_guard_trace();
    let execution_policy = agent.execution_policy_snapshot().map(str::to_owned);
    let usage = UsageReport::from_totals(usage_totals);
    let usage_turns = agent.context_report().usage_turns;
    let repair_turns = agent
        .verification_runs()
        .iter()
        .map(|run| u64::from(run.repair_attempts))
        .sum();
    let budget_metrics = EvalMetricsReport::from_run(
        &usage_turns,
        provider_stats.logical_turns.load(Ordering::Relaxed),
        provider_stats.attempts.load(Ordering::Relaxed),
        started.elapsed(),
        eval_sink.max_reads_per_unchanged_path(),
        eval_sink.read_path_reports(),
        budgets.cache_warmup_turns.unwrap_or(1),
    );
    let budget = EvalBudgetReport::evaluate(budgets, budget_metrics, usage);
    let task_effects = eval_sink.task_effects();
    let primary_changed_files = task_effects.changed_files.clone();
    let grader_results = grade_task(
        &task.graders,
        &worktree_path,
        &suite.base_dir,
        &output,
        &task_effects,
    )
    .await;
    let compactions = agent.compaction_events().len();
    // Episodes that left live context as a card marker. A later `/ctx` restore
    // flips Evicted to Restored without undoing the fact of the eviction, so
    // both statuses count.
    let episode_evictions = agent
        .episode_reports()
        .iter()
        .filter(|episode| matches!(episode.status_label.as_str(), "evicted" | "restored"))
        .count();
    storage
        .record_session_file_changes(session_id, &primary_changed_files)
        .await?;
    storage.record_session_heartbeat(session_id, false).await?;
    if let Some(outcome) = &peer_outcome {
        storage
            .mark_session_status(outcome.session_id, SessionStatus::Completed)
            .await?;
    }
    let shared_workspace = task
        .shared_workspace_peer
        .as_ref()
        .zip(peer_outcome.as_ref())
        .map(|(peer, outcome)| {
            let expected_primary_changed_files =
                normalized_paths(&peer.expected_primary_changed_files);
            let expected_peer_changed_files = normalized_paths(&peer.expected_peer_changed_files);
            let passed = primary_changed_files == expected_primary_changed_files
                && outcome.changed_files == expected_peer_changed_files;
            SharedWorkspaceReport {
                primary_session_id: session_id.as_i64(),
                peer_session_id: outcome.session_id.as_i64(),
                primary_changed_files: primary_changed_files.clone(),
                peer_changed_files: outcome.changed_files.clone(),
                expected_primary_changed_files,
                expected_peer_changed_files,
                passed,
            }
        });
    let status_matches = status == task.expected_status;
    let error_matches = task.expected_error_contains.iter().all(|needle| {
        run_error
            .as_deref()
            .is_some_and(|error| error.contains(needle))
    });
    let passed = status_matches
        && error_matches
        && task
            .min_compactions
            .is_none_or(|minimum| compactions >= minimum)
        && task
            .max_compactions
            .is_none_or(|maximum| compactions <= maximum)
        && task
            .min_episode_evictions
            .is_none_or(|minimum| episode_evictions >= minimum)
        && shared_workspace.as_ref().is_none_or(|report| report.passed)
        && grader_results.iter().all(|grader| grader.passed)
        && budget.passed();
    let score = ScoreReport::from_grader_results(&grader_results);
    let duration_ms = millis_u64(started.elapsed());

    if let Err(err) = storage
        .sync_usage_turns_snapshot(session_id, agent.usage_turns())
        .await
    {
        tracing::warn!(session_id = %session_id, error = %err, "failed to persist eval usage turns");
    }
    if let Err(err) = storage
        .update_session_usage_totals(session_id, &usage_totals)
        .await
    {
        tracing::warn!(session_id = %session_id, error = %err, "failed to persist eval usage");
    }
    let storage_status = if passed {
        SessionStatus::Completed
    } else {
        SessionStatus::Failed
    };
    if let Err(err) = storage
        .mark_session_status(session_id, storage_status)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %err,
            "failed to persist eval session status"
        );
    }

    Ok(TaskReport {
        id: task_run_id.to_string(),
        prompt: task.prompt.clone(),
        resume_prompt: task.resume_prompt.clone(),
        queued_messages: task.queued_messages.clone(),
        profile: task.profile,
        status,
        expected_status: task.expected_status,
        passed,
        score,
        output,
        run_error,
        completion_guard,
        execution_policy,
        usage,
        usage_turns,
        budget,
        cost_micros: usage.cost_micros,
        tokens_per_dollar: tokens_per_dollar(usage),
        duration_ms,
        repair_turns,
        compactions,
        min_compactions: task.min_compactions,
        max_compactions: task.max_compactions,
        episode_evictions,
        min_episode_evictions: task.min_episode_evictions,
        shared_workspace,
        changed_files: primary_changed_files,
        tool_effects: task_effects.tool_effects,
        attempted_tool_effects: task_effects.attempted_tool_effects,
        worktree_path: worktree_path.display().to_string(),
        graders: grader_results,
    })
}

fn eval_yolo_mode() -> crate::yolo::YoloMode {
    crate::yolo::YoloMode::with_level(crate::tool::ApprovalLevel::Balanced)
}

/// A loaded, validated eval suite plus the directory its fixtures resolve from.
#[derive(Debug, Clone)]
struct EvalSuite {
    path: PathBuf,
    base_dir: PathBuf,
    id: String,
    seed: u64,
    repetitions: usize,
    budgets: EvalBudgets,
    tasks: Vec<EvalTask>,
}

impl EvalSuite {
    fn load(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read eval suite {:?}", path))?;
        let raw: SuiteFile = toml::from_str(&content)
            .with_context(|| format!("Failed to parse eval suite {:?}", path))?;
        let base_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let suite = Self {
            path: path.to_path_buf(),
            base_dir,
            id: raw.id,
            seed: raw.seed,
            repetitions: raw.repetitions,
            budgets: raw.budgets,
            tasks: raw.tasks,
        };
        suite.validate()?;
        Ok(suite)
    }

    fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            anyhow::bail!("Eval suite id is required");
        }
        if self.tasks.is_empty() {
            anyhow::bail!("Eval suite '{}' has no tasks", self.id);
        }
        if self.repetitions == 0 {
            anyhow::bail!(
                "Eval suite '{}' repetitions must be greater than zero",
                self.id
            );
        }

        let mut ids = HashSet::new();
        for task in &self.tasks {
            task.validate(&self.base_dir)?;
            if !ids.insert(task.id.clone()) {
                anyhow::bail!(
                    "Eval suite '{}' has duplicate task id '{}'",
                    self.id,
                    task.id
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteFile {
    id: String,
    seed: u64,
    #[serde(default = "default_eval_repetitions")]
    repetitions: usize,
    #[serde(default)]
    budgets: EvalBudgets,
    tasks: Vec<EvalTask>,
}

const fn default_eval_repetitions() -> usize {
    DEFAULT_EVAL_REPETITIONS
}

/// Prompt/tool profile used for an eval task.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EvalAgentProfile {
    #[default]
    Full,
    Smol,
}

/// A single eval task: a fixture to copy, a prompt to run, and its graders.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalTask {
    id: String,
    fixture: String,
    prompt: String,
    #[serde(default)]
    profile: EvalAgentProfile,
    #[serde(default)]
    mock: Option<MockScript>,
    #[serde(default)]
    graders: Vec<GraderSpec>,
    #[serde(default)]
    budgets: EvalBudgets,
    #[serde(default)]
    expected_status: TaskStatus,
    #[serde(default)]
    expected_error_contains: Vec<String>,
    #[serde(default)]
    cancel_after_ms: Option<u64>,
    #[serde(default)]
    cancel_after_provider_attempts: Option<usize>,
    #[serde(default)]
    resume_after_interruption: bool,
    /// Optional human follow-up added when resuming an interrupted run. When
    /// absent, the existing context resumes without another user message.
    #[serde(default)]
    resume_prompt: Option<String>,
    /// Human follow-ups delivered through the in-flight composer queue. The
    /// runner enqueues them before the first provider turn so the agent merges
    /// them at the first normal tool/response boundary deterministically.
    #[serde(default)]
    queued_messages: Vec<String>,
    #[serde(default)]
    mock_context_window_tokens: Option<usize>,
    #[serde(default)]
    min_compactions: Option<usize>,
    /// Minimum episode evictions the run must record. Setting this wires the
    /// episode store for the task's agent, so the dark feature is exercised
    /// deterministically regardless of the BONSAI_EPISODES environment.
    #[serde(default)]
    min_episode_evictions: Option<usize>,
    /// Disable task episodes so a scenario specifically measures pressure
    /// compaction rather than episode eviction.
    #[serde(default)]
    disable_episodes: bool,
    /// Ceiling on pressure compactions — `0` asserts that relevance-driven
    /// episode eviction alone kept the prompt healthy.
    #[serde(default)]
    max_compactions: Option<usize>,
    #[serde(default)]
    shared_workspace_peer: Option<SharedWorkspacePeer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SharedWorkspacePeer {
    prompt: String,
    mock: MockScript,
    #[serde(default)]
    expected_primary_changed_files: Vec<String>,
    #[serde(default)]
    expected_peer_changed_files: Vec<String>,
}

impl EvalTask {
    fn validate(&self, suite_base_dir: &Path) -> Result<()> {
        validate_id(&self.id)?;
        if self.prompt.trim().is_empty() {
            anyhow::bail!("Task '{}' prompt is required", self.id);
        }
        let fixture = SafeRelativePath::parse(&self.fixture, "fixture")?.join(suite_base_dir);
        if !fixture.is_dir() {
            anyhow::bail!(
                "Task '{}' fixture directory does not exist: {}",
                self.id,
                fixture.display()
            );
        }
        if let Some(mock) = &self.mock {
            mock.validate(&self.id, &fixture)?;
        }
        if self.expected_status == TaskStatus::Completed && self.graders.is_empty() {
            anyhow::bail!("Task '{}' must define at least one grader", self.id);
        }
        if !self.expected_error_contains.is_empty() && self.expected_status != TaskStatus::Error {
            anyhow::bail!(
                "Task '{}' declares expected_error_contains but does not expect error status",
                self.id
            );
        }
        if self.cancel_after_ms == Some(0) {
            anyhow::bail!(
                "Task '{}' cancel_after_ms must be greater than zero",
                self.id
            );
        }
        if self.cancel_after_provider_attempts == Some(0) {
            anyhow::bail!(
                "Task '{}' cancel_after_provider_attempts must be greater than zero",
                self.id
            );
        }
        if self.cancel_after_ms.is_some() && self.cancel_after_provider_attempts.is_some() {
            anyhow::bail!(
                "Task '{}' cannot combine delay-based and provider-attempt cancellation",
                self.id
            );
        }
        if self.resume_after_interruption
            && self.cancel_after_ms.is_none()
            && self.cancel_after_provider_attempts.is_none()
        {
            anyhow::bail!(
                "Task '{}' resume_after_interruption requires a cancellation trigger",
                self.id
            );
        }
        if let Some(prompt) = self.resume_prompt.as_deref() {
            if !self.resume_after_interruption {
                anyhow::bail!(
                    "Task '{}' resume_prompt requires resume_after_interruption",
                    self.id
                );
            }
            if prompt.trim().is_empty() {
                anyhow::bail!("Task '{}' resume_prompt must not be blank", self.id);
            }
        }
        if self
            .queued_messages
            .iter()
            .any(|message| message.trim().is_empty())
        {
            anyhow::bail!("Task '{}' queued_messages must not contain blanks", self.id);
        }
        if self.mock_context_window_tokens == Some(0) {
            anyhow::bail!(
                "Task '{}' mock_context_window_tokens must be greater than zero",
                self.id
            );
        }
        if self.min_compactions == Some(0) {
            anyhow::bail!(
                "Task '{}' min_compactions must be greater than zero",
                self.id
            );
        }
        if self.min_episode_evictions == Some(0) {
            anyhow::bail!(
                "Task '{}' min_episode_evictions must be greater than zero",
                self.id
            );
        }
        if self.disable_episodes && self.min_episode_evictions.is_some() {
            anyhow::bail!(
                "Task '{}' cannot disable episodes and require episode evictions",
                self.id
            );
        }
        if let (Some(min), Some(max)) = (self.min_compactions, self.max_compactions)
            && min > max
        {
            anyhow::bail!("Task '{}' min_compactions exceeds max_compactions", self.id);
        }
        if let Some(peer) = &self.shared_workspace_peer {
            if !self.resume_after_interruption {
                anyhow::bail!(
                    "Task '{}' shared_workspace_peer requires resume_after_interruption",
                    self.id
                );
            }
            if self.cancel_after_provider_attempts.is_none() || self.cancel_after_ms.is_some() {
                anyhow::bail!(
                    "Task '{}' shared_workspace_peer requires provider-attempt cancellation",
                    self.id
                );
            }
            if self
                .mock
                .as_ref()
                .and_then(|mock| mock.wait_for_cancellation_after_tool_turns)
                .is_none()
            {
                anyhow::bail!(
                    "Task '{}' shared_workspace_peer requires a scripted interleaving point",
                    self.id
                );
            }
            if peer.prompt.trim().is_empty() {
                anyhow::bail!(
                    "Task '{}' shared workspace peer prompt is required",
                    self.id
                );
            }
            peer.mock.validate(&format!("{}-peer", self.id), &fixture)?;
            for path in peer
                .expected_primary_changed_files
                .iter()
                .chain(&peer.expected_peer_changed_files)
            {
                SafeRelativePath::parse(path, "shared workspace changed file")
                    .with_context(|| format!("Invalid changed file in task '{}'", self.id))?;
            }
        }
        for grader in &self.graders {
            grader.validate(suite_base_dir, &self.id)?;
        }
        Ok(())
    }
}

/// A scripted sequence of reads, writes, and a final response for a mock task.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MockScript {
    #[serde(default)]
    pub(crate) read: Vec<String>,
    #[serde(default)]
    pub(crate) write: Vec<MockWrite>,
    #[serde(default = "default_final_response")]
    pub(crate) final_response: String,
    /// Number of leading provider calls that fail with a transient, retryable
    /// stream error before the scripted reads/writes/response begin — exercises
    /// the agent's retry path (`chat_stream_with_retry`) end to end. Must be
    /// `<= MAX_PROVIDER_RETRIES` for the task to recover.
    #[serde(default)]
    pub(crate) transient_errors: usize,
    /// Number of leading successful calls that end with an empty length-
    /// truncated response before the normal script begins.
    #[serde(default)]
    pub(crate) truncated_responses: usize,
    /// Optional invariant checked on every provider call after prompt shaping.
    #[serde(default)]
    pub(crate) expected_user_message_count: Option<usize>,
    /// Text that must never appear in a provider-shaped request.
    #[serde(default)]
    pub(crate) forbidden_request_substrings: Vec<String>,
    /// Text that must appear in the final ordinary provider request.
    #[serde(default)]
    pub(crate) final_request_contains: Vec<String>,
    /// Text that must NOT appear in the final ordinary provider request — the
    /// complement of `final_request_contains`. Unlike
    /// `forbidden_request_substrings` (every request), this permits the text
    /// before the final request (e.g. bulk that an episode eviction removes).
    #[serde(default)]
    pub(crate) final_request_forbidden_substrings: Vec<String>,
    /// Raw tool-call batches for failure and batching scenarios. Mutually
    /// exclusive with the legacy `read`/`write` shorthand.
    #[serde(default)]
    pub(crate) tool_turns: Vec<MockToolTurn>,
    /// Synthetic assistant text added to every scripted tool turn to create
    /// deterministic context pressure without a large fixture artifact.
    #[serde(default)]
    pub(crate) tool_turn_content_chars: usize,
    /// Report cache reads from the byte-stable prefix shared with the previous
    /// ordinary request.
    #[serde(default)]
    pub(crate) simulate_prompt_cache: bool,
    /// Hold the provider call until the task cancellation token fires.
    #[serde(default)]
    pub(crate) wait_for_cancellation: bool,
    /// Pause once before emitting tool turn N, allowing the runner to interleave
    /// a peer session before resuming this exact agent context.
    #[serde(default)]
    pub(crate) wait_for_cancellation_after_tool_turns: Option<usize>,
}

impl MockScript {
    fn validate(&self, task_id: &str, fixture: &Path) -> Result<()> {
        if !self.tool_turns.is_empty() && (!self.read.is_empty() || !self.write.is_empty()) {
            anyhow::bail!(
                "Task '{task_id}' mock script cannot mix tool_turns with read/write shorthand"
            );
        }
        if self.wait_for_cancellation
            && (!self.read.is_empty() || !self.write.is_empty() || !self.tool_turns.is_empty())
        {
            anyhow::bail!(
                "Task '{task_id}' cancellation script cannot also emit scripted tool calls"
            );
        }
        if self.wait_for_cancellation && self.wait_for_cancellation_after_tool_turns.is_some() {
            anyhow::bail!("Task '{task_id}' mock script cannot combine both cancellation waits");
        }
        if self
            .wait_for_cancellation_after_tool_turns
            .is_some_and(|step| step > self.tool_turns.len())
        {
            anyhow::bail!("Task '{task_id}' cancellation wait is beyond its scripted tool turns");
        }
        for path in &self.read {
            let read_path = SafeRelativePath::parse(path, "mock read path")
                .with_context(|| format!("Invalid mock read path in task '{task_id}'"))?
                .join(fixture);
            if !read_path.exists() {
                anyhow::bail!(
                    "Task '{}' mock read path does not exist in fixture: {}",
                    task_id,
                    path
                );
            }
        }
        for write in &self.write {
            SafeRelativePath::parse(&write.path, "mock write path")
                .with_context(|| format!("Invalid mock write path in task '{task_id}'"))?;
        }
        for (turn_index, turn) in self.tool_turns.iter().enumerate() {
            if turn.calls.is_empty() {
                anyhow::bail!("Task '{task_id}' mock tool turn {turn_index} has no calls");
            }
            let mut ids = HashSet::new();
            for (call_index, call) in turn.calls.iter().enumerate() {
                if call.name.trim().is_empty() {
                    anyhow::bail!(
                        "Task '{task_id}' mock tool turn {turn_index} call {call_index} has no name"
                    );
                }
                if let Some(id) = call.id.as_deref()
                    && (!ids.insert(id) || id.trim().is_empty())
                {
                    anyhow::bail!(
                        "Task '{task_id}' mock tool turn {turn_index} has an empty or duplicate call id '{id}'"
                    );
                }
            }
        }
        if self
            .forbidden_request_substrings
            .iter()
            .any(|text| text.is_empty())
        {
            anyhow::bail!("Task '{task_id}' mock forbidden request text cannot be empty");
        }
        if self
            .final_request_contains
            .iter()
            .any(|text| text.is_empty())
        {
            anyhow::bail!("Task '{task_id}' mock required final request text cannot be empty");
        }
        if self
            .final_request_forbidden_substrings
            .iter()
            .any(|text| text.is_empty())
        {
            anyhow::bail!("Task '{task_id}' mock forbidden final request text cannot be empty");
        }
        if self.read.is_empty()
            && self.write.is_empty()
            && self.tool_turns.is_empty()
            && self.final_response.trim().is_empty()
            && !self.wait_for_cancellation
        {
            anyhow::bail!("Task '{task_id}' mock script has no actions or final response");
        }
        Ok(())
    }
}

struct CountingProvider {
    inner: Box<dyn Provider>,
    stats: Arc<EvalProviderStats>,
    max_attempts: Option<usize>,
}

impl CountingProvider {
    fn new(
        inner: Box<dyn Provider>,
        stats: Arc<EvalProviderStats>,
        max_attempts: Option<usize>,
    ) -> Self {
        Self {
            inner,
            stats,
            max_attempts,
        }
    }

    fn record_attempt(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
    ) -> Result<()> {
        let attempts = self.stats.attempts.fetch_add(1, Ordering::Relaxed) + 1;
        self.stats.attempt_notify.notify_waiters();
        if self
            .max_attempts
            .is_some_and(|max_attempts| attempts > max_attempts)
        {
            anyhow::bail!(
                "Eval provider-attempt budget exceeded: attempt {attempts} is above the maximum {}",
                self.max_attempts.unwrap_or_default()
            );
        }
        let signature = serde_json::to_string(&(messages, tools)).unwrap_or_default();
        let mut previous = self
            .stats
            .previous_request
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if previous.as_deref() != Some(signature.as_str()) {
            self.stats.logical_turns.fetch_add(1, Ordering::Relaxed);
            *previous = Some(signature);
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct EvalProviderStats {
    attempts: AtomicUsize,
    logical_turns: AtomicUsize,
    previous_request: StdMutex<Option<String>>,
    attempt_notify: Notify,
}

impl EvalProviderStats {
    async fn wait_for_attempt(&self, target: usize) {
        loop {
            let notified = self.attempt_notify.notified();
            if self.attempts.load(Ordering::Acquire) >= target {
                return;
            }
            notified.await;
        }
    }
}

#[async_trait]
impl Provider for CountingProvider {
    fn reasoning(&self) -> ReasoningSelection {
        self.inner.reasoning()
    }

    fn reasoning_escalation(&self) -> Option<ReasoningSelection> {
        self.inner.reasoning_escalation()
    }

    fn take_last_request_diagnostics(&self) -> Option<ProviderRequestDiagnostics> {
        self.inner.take_last_request_diagnostics()
    }

    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.record_attempt(messages, tools)
            .map_err(|error| crate::provider::ProviderFailure::configuration(error.to_string()))?;
        self.inner
            .chat_stream(messages, tools, cancellation_token, sink)
            .await
    }

    async fn chat_stream_with_options(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: crate::provider::ProviderRequestOptions,
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        self.record_attempt(messages, tools)
            .map_err(|error| crate::provider::ProviderFailure::configuration(error.to_string()))?;
        self.inner
            .chat_stream_with_options(messages, tools, options, cancellation_token, sink)
            .await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        self.inner.list_models().await
    }
}

/// Silent eval sink that records tool-call shapes for efficiency budgets.
#[derive(Debug, Default)]
struct EvalSink {
    state: StdMutex<EvalSinkState>,
}

#[derive(Debug, Default)]
struct EvalSinkState {
    pending: HashMap<String, EvalToolAction>,
    pending_effects: HashMap<String, EvalToolEffect>,
    observed_tool_effects: HashSet<EvalToolEffect>,
    attempted_tool_effects: HashSet<EvalToolEffect>,
    read_calls: HashMap<String, EvalReadCall>,
    path_generations: HashMap<String, usize>,
    global_generation: usize,
}

#[derive(Debug)]
enum EvalToolAction {
    Mutate(String),
    GlobalMutate,
}

#[derive(Debug)]
struct EvalReadCall {
    path: String,
    arguments: String,
    path_generation: usize,
    global_generation: usize,
    outcome: EvalReadOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvalReadOutcome {
    Pending,
    Executed,
    Reused,
    Rejected,
    Failed,
}

impl EvalSink {
    fn task_effects(&self) -> EvalTaskEffects {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut changed_files = state.path_generations.keys().cloned().collect::<Vec<_>>();
        changed_files.sort();
        let mut tool_effects = state
            .observed_tool_effects
            .iter()
            .copied()
            .collect::<Vec<_>>();
        tool_effects.sort();
        let mut attempted_tool_effects = state
            .attempted_tool_effects
            .iter()
            .copied()
            .collect::<Vec<_>>();
        attempted_tool_effects.sort();
        EvalTaskEffects {
            changed_files,
            tool_effects,
            attempted_tool_effects,
        }
    }

    fn changed_paths(&self) -> Vec<String> {
        self.task_effects().changed_files
    }

    fn max_reads_per_unchanged_path(&self) -> usize {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut counts = HashMap::<(&str, usize, usize), usize>::new();
        for call in state
            .read_calls
            .values()
            .filter(|call| call.outcome == EvalReadOutcome::Executed)
        {
            *counts
                .entry((
                    call.path.as_str(),
                    call.path_generation,
                    call.global_generation,
                ))
                .or_default() += 1;
        }
        counts.values().copied().max().unwrap_or(0)
    }

    fn read_path_reports(&self) -> Vec<EvalReadPathReport> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut reports = HashMap::<&str, EvalReadPathReport>::new();
        for call in state.read_calls.values() {
            let report = reports
                .entry(call.path.as_str())
                .or_insert_with(|| EvalReadPathReport {
                    path: call.path.clone(),
                    attempts: 0,
                    executed: 0,
                    reused: 0,
                    rejected: 0,
                    failed: 0,
                    arguments: Vec::new(),
                });
            report.attempts += 1;
            report.arguments.push(call.arguments.clone());
            match call.outcome {
                EvalReadOutcome::Pending | EvalReadOutcome::Failed => report.failed += 1,
                EvalReadOutcome::Executed => report.executed += 1,
                EvalReadOutcome::Reused => report.reused += 1,
                EvalReadOutcome::Rejected => report.rejected += 1,
            }
        }
        let mut reports = reports.into_values().collect::<Vec<_>>();
        reports.sort_by(|left, right| left.path.cmp(&right.path));
        reports
    }

    fn finish_tool(
        &self,
        id: &str,
        result: &str,
        status: crate::output::ToolExecutionStatus,
        diff_paths: &[String],
    ) {
        let success = status.is_success();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(effect) = state.pending_effects.remove(id)
            && success
        {
            state.observed_tool_effects.insert(effect);
        }
        if let Some(call) = state.read_calls.get_mut(id) {
            call.outcome = if result.starts_with(crate::agent::REUSED_READ_MARKER)
                || result.starts_with(crate::agent::REUSED_INSPECTION_MARKER)
            {
                EvalReadOutcome::Reused
            } else if result.starts_with("Error: this exact read was already answered")
                || result.starts_with("Error: repeated unchanged inspection blocked")
            {
                EvalReadOutcome::Rejected
            } else if success {
                EvalReadOutcome::Executed
            } else {
                EvalReadOutcome::Failed
            };
            return;
        }
        let Some(action) = state.pending.remove(id) else {
            if success {
                record_changed_paths(&mut state, diff_paths);
            }
            return;
        };
        if !success {
            return;
        }
        match action {
            EvalToolAction::Mutate(path) if diff_paths.is_empty() => {
                record_changed_paths(&mut state, &[path]);
            }
            EvalToolAction::Mutate(_) => record_changed_paths(&mut state, diff_paths),
            EvalToolAction::GlobalMutate => {
                state.global_generation = state.global_generation.saturating_add(1);
                record_changed_paths(&mut state, diff_paths);
            }
        }
    }
}

fn record_changed_paths(state: &mut EvalSinkState, paths: &[String]) {
    for path in paths {
        let generation = state.path_generations.entry(path.clone()).or_default();
        *generation = generation.saturating_add(1);
    }
}

impl OutputSink for EvalSink {
    fn tool_calls_started(&self, calls: &[ToolCallStart]) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for call in calls {
            let effect = EvalToolEffect::for_tool_name(&call.name);
            state.attempted_tool_effects.insert(effect);
            state.pending_effects.insert(call.id.clone(), effect);
            let arguments = serde_json::from_str::<serde_json::Value>(&call.arguments).ok();
            let path = arguments
                .as_ref()
                .and_then(|value| value.get("path"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            match call.name.as_str() {
                "read" | "read_region" | "read_symbol" => {
                    if let Some(path) = path {
                        let path_generation = state
                            .path_generations
                            .get(&path)
                            .copied()
                            .unwrap_or_default();
                        let global_generation = state.global_generation;
                        state.read_calls.insert(
                            call.id.clone(),
                            EvalReadCall {
                                path,
                                arguments: call.arguments.clone(),
                                path_generation,
                                global_generation,
                                outcome: EvalReadOutcome::Pending,
                            },
                        );
                    }
                }
                "write" | "edit" => {
                    if let Some(path) = path {
                        state
                            .pending
                            .insert(call.id.clone(), EvalToolAction::Mutate(path));
                    }
                }
                "bash" => {
                    state
                        .pending
                        .insert(call.id.clone(), EvalToolAction::GlobalMutate);
                }
                _ if effect == EvalToolEffect::WorkspaceMutation => {
                    state
                        .pending
                        .insert(call.id.clone(), EvalToolAction::GlobalMutate);
                }
                _ => {}
            }
        }
    }

    fn tool_finished(&self, id: &str, result: &str, status: crate::output::ToolExecutionStatus) {
        self.finish_tool(id, result, status, &[]);
    }

    fn tool_finished_with_diff(
        &self,
        id: &str,
        result: &str,
        status: crate::output::ToolExecutionStatus,
        diff: crate::diff::FileDiff,
    ) {
        let paths = diff
            .files()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        self.finish_tool(id, result, status, &paths);
    }

    fn workspace_changed(&self, paths: &[String], _intent: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !paths.is_empty() {
            state
                .observed_tool_effects
                .insert(EvalToolEffect::WorkspaceMutation);
        }
        record_changed_paths(&mut state, paths);
    }
}

fn run_id(mode: EvalMode, seed: u64) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    format!("{millis}-{mode}-{seed}")
}

fn normalized_paths(paths: &[String]) -> Vec<String> {
    let mut normalized = paths.to_vec();
    normalized.sort();
    normalized.dedup();
    normalized
}

/// Convert a [`Duration`] to whole milliseconds, saturating at [`u64::MAX`].
pub(crate) fn millis_u64(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_context_window_override_is_guarded() {
        assert_eq!(parse_eval_context_window_tokens("32768").unwrap(), 32_768);
        assert!(parse_eval_context_window_tokens("32767").is_err());
        assert!(parse_eval_context_window_tokens("large").is_err());
    }

    #[test]
    fn evals_auto_approve_routine_development_commands_without_dropping_guardrails() {
        let mode = eval_yolo_mode();

        assert_eq!(mode.level(), crate::tool::ApprovalLevel::Balanced);
        assert!(!mode.is_enabled());
        assert!(mode.level().is_confined());
        assert!(mode.level().requires_read_before_write());
        assert!(mode.level().auto_approves(crate::tool::RiskTier::Medium));
        assert!(mode.level().enforces_floor());
    }

    #[test]
    fn eval_reasoning_override_uses_resolved_model_capabilities() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let metadata = crate::provider::metadata_for("opencode").unwrap();
        let mut session = SessionStore::default();
        session.ensure_provider("opencode");

        assert_eq!(
            apply_eval_reasoning_override(
                &mut session,
                &catalog,
                metadata,
                "opencode",
                "opencode/glm-5.2",
                ReasoningSelection::Max,
            ),
            ReasoningSelection::Max
        );
        assert_eq!(
            session.session("opencode").reasoning,
            ReasoningSelection::Max
        );
        assert_eq!(
            apply_eval_reasoning_override(
                &mut session,
                &catalog,
                metadata,
                "opencode",
                "opencode/qwen3.7-max",
                ReasoningSelection::Max,
            ),
            ReasoningSelection::Default
        );
    }

    fn tool_start(id: &str, name: &str, path: &str) -> ToolCallStart {
        ToolCallStart::new(id, name, serde_json::json!({ "path": path }).to_string())
    }

    #[test]
    fn eval_sink_counts_real_reads_not_reuse_or_rejections() {
        let sink = EvalSink::default();

        sink.tool_calls_started(&[tool_start("read-1", "read", "src/lib.rs")]);
        sink.tool_finished(
            "read-1",
            "real file body",
            crate::output::ToolExecutionStatus::Succeeded,
        );
        sink.tool_finished(
            "read-1",
            &format!("{} call-0", crate::agent::REUSED_READ_MARKER),
            crate::output::ToolExecutionStatus::Succeeded,
        );
        sink.tool_calls_started(&[tool_start("read-2", "read", "src/lib.rs")]);
        sink.tool_finished(
            "read-2",
            "real file body",
            crate::output::ToolExecutionStatus::Succeeded,
        );
        sink.tool_calls_started(&[tool_start("read-3", "read", "src/lib.rs")]);
        sink.tool_finished(
            "read-3",
            "real file body",
            crate::output::ToolExecutionStatus::Succeeded,
        );
        sink.tool_finished(
            "read-3",
            "Error: repeated unchanged inspection blocked",
            crate::output::ToolExecutionStatus::Failed,
        );

        assert_eq!(sink.max_reads_per_unchanged_path(), 1);
        let report = sink.read_path_reports().pop().unwrap();
        assert_eq!((report.attempts, report.executed), (3, 1));
        assert_eq!((report.reused, report.rejected), (1, 1));

        sink.tool_calls_started(&[tool_start("write-1", "write", "src/lib.rs")]);
        sink.tool_finished(
            "write-1",
            "write failed",
            crate::output::ToolExecutionStatus::Failed,
        );
        sink.tool_calls_started(&[tool_start("read-4", "read", "src/lib.rs")]);
        sink.tool_finished(
            "read-4",
            "real file body",
            crate::output::ToolExecutionStatus::Succeeded,
        );
        assert_eq!(sink.max_reads_per_unchanged_path(), 2);

        sink.tool_calls_started(&[tool_start("write-2", "write", "src/lib.rs")]);
        sink.tool_finished(
            "write-2",
            "written",
            crate::output::ToolExecutionStatus::Succeeded,
        );
        assert_eq!(sink.changed_paths(), vec!["src/lib.rs"]);
        assert_eq!(
            sink.task_effects().tool_effects,
            vec![
                EvalToolEffect::Inspection,
                EvalToolEffect::WorkspaceMutation
            ]
        );
        sink.tool_calls_started(&[tool_start("read-5", "read", "src/lib.rs")]);
        sink.tool_finished(
            "read-5",
            "new file body",
            crate::output::ToolExecutionStatus::Succeeded,
        );
        assert_eq!(sink.max_reads_per_unchanged_path(), 2);
    }

    #[test]
    fn eval_sink_records_multi_file_diffs_and_shell_workspace_changes() {
        let sink = EvalSink::default();
        sink.tool_calls_started(&[ToolCallStart::new(
            "patch-1",
            "apply_patch",
            serde_json::json!({ "patch": "synthetic" }).to_string(),
        )]);
        let primary =
            crate::diff::build_file_diff("src/lib.rs".to_string(), Some("old\n"), "new\n");
        let secondary =
            crate::diff::build_file_diff("tests/total.rs".to_string(), Some("old\n"), "new\n");
        sink.tool_finished_with_diff(
            "patch-1",
            "patched",
            crate::output::ToolExecutionStatus::Succeeded,
            primary.with_additional_files(vec![secondary]),
        );

        sink.tool_calls_started(&[ToolCallStart::new(
            "bash-1",
            "bash",
            serde_json::json!({ "command": "touch generated.txt" }).to_string(),
        )]);
        sink.tool_finished("bash-1", "", crate::output::ToolExecutionStatus::Succeeded);
        sink.workspace_changed(&["generated.txt".to_string()], "synthetic shell mutation");

        assert_eq!(
            sink.changed_paths(),
            vec!["generated.txt", "src/lib.rs", "tests/total.rs"]
        );
        assert_eq!(
            sink.task_effects().tool_effects,
            vec![
                EvalToolEffect::WorkspaceMutation,
                EvalToolEffect::CommandExecution
            ]
        );
    }

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn duplicate_task_ids_are_rejected() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("fixture")).unwrap();
        let suite = EvalSuite {
            path: temp.path().join("suite.toml"),
            base_dir: temp.path().to_path_buf(),
            id: "test".to_string(),
            seed: 1,
            repetitions: 1,
            budgets: EvalBudgets::default(),
            tasks: vec![
                EvalTask {
                    id: "same".to_string(),
                    fixture: "fixture".to_string(),
                    prompt: "do it".to_string(),
                    profile: EvalAgentProfile::Full,
                    mock: Some(MockScript {
                        read: Vec::new(),
                        write: Vec::new(),
                        final_response: "Done".to_string(),
                        transient_errors: 0,
                        truncated_responses: 0,
                        expected_user_message_count: None,
                        forbidden_request_substrings: Vec::new(),
                        final_request_contains: Vec::new(),
                        final_request_forbidden_substrings: Vec::new(),
                        tool_turns: Vec::new(),
                        tool_turn_content_chars: 0,
                        simulate_prompt_cache: false,
                        wait_for_cancellation: false,
                        wait_for_cancellation_after_tool_turns: None,
                    }),
                    graders: vec![GraderSpec::Assertion {
                        contains: vec!["Done".to_string()],
                        not_contains: Vec::new(),
                    }],
                    budgets: EvalBudgets::default(),
                    expected_status: TaskStatus::Completed,
                    expected_error_contains: Vec::new(),
                    cancel_after_ms: None,
                    cancel_after_provider_attempts: None,
                    resume_after_interruption: false,
                    resume_prompt: None,
                    queued_messages: Vec::new(),
                    mock_context_window_tokens: None,
                    min_compactions: None,
                    min_episode_evictions: None,
                    disable_episodes: false,
                    max_compactions: None,
                    shared_workspace_peer: None,
                },
                EvalTask {
                    id: "same".to_string(),
                    fixture: "fixture".to_string(),
                    prompt: "do it again".to_string(),
                    profile: EvalAgentProfile::Full,
                    mock: Some(MockScript {
                        read: Vec::new(),
                        write: Vec::new(),
                        final_response: "Done".to_string(),
                        transient_errors: 0,
                        truncated_responses: 0,
                        expected_user_message_count: None,
                        forbidden_request_substrings: Vec::new(),
                        final_request_contains: Vec::new(),
                        final_request_forbidden_substrings: Vec::new(),
                        tool_turns: Vec::new(),
                        tool_turn_content_chars: 0,
                        simulate_prompt_cache: false,
                        wait_for_cancellation: false,
                        wait_for_cancellation_after_tool_turns: None,
                    }),
                    graders: vec![GraderSpec::Assertion {
                        contains: vec!["Done".to_string()],
                        not_contains: Vec::new(),
                    }],
                    budgets: EvalBudgets::default(),
                    expected_status: TaskStatus::Completed,
                    expected_error_contains: Vec::new(),
                    cancel_after_ms: None,
                    cancel_after_provider_attempts: None,
                    resume_after_interruption: false,
                    resume_prompt: None,
                    queued_messages: Vec::new(),
                    mock_context_window_tokens: None,
                    min_compactions: None,
                    min_episode_evictions: None,
                    disable_episodes: false,
                    max_compactions: None,
                    shared_workspace_peer: None,
                },
            ],
        };

        let err = suite.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate task id"));
    }

    #[test]
    fn tasks_without_graders_are_rejected() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("fixture")).unwrap();
        let suite = EvalSuite {
            path: temp.path().join("suite.toml"),
            base_dir: temp.path().to_path_buf(),
            id: "test".to_string(),
            seed: 1,
            repetitions: 1,
            budgets: EvalBudgets::default(),
            tasks: vec![EvalTask {
                id: "no-graders".to_string(),
                fixture: "fixture".to_string(),
                prompt: "do it".to_string(),
                profile: EvalAgentProfile::Full,
                mock: Some(MockScript {
                    read: Vec::new(),
                    write: Vec::new(),
                    final_response: "Done".to_string(),
                    transient_errors: 0,
                    truncated_responses: 0,
                    expected_user_message_count: None,
                    forbidden_request_substrings: Vec::new(),
                    final_request_contains: Vec::new(),
                    final_request_forbidden_substrings: Vec::new(),
                    tool_turns: Vec::new(),
                    tool_turn_content_chars: 0,
                    simulate_prompt_cache: false,
                    wait_for_cancellation: false,
                    wait_for_cancellation_after_tool_turns: None,
                }),
                graders: Vec::new(),
                budgets: EvalBudgets::default(),
                expected_status: TaskStatus::Completed,
                expected_error_contains: Vec::new(),
                cancel_after_ms: None,
                cancel_after_provider_attempts: None,
                resume_after_interruption: false,
                resume_prompt: None,
                queued_messages: Vec::new(),
                mock_context_window_tokens: None,
                min_compactions: None,
                min_episode_evictions: None,
                disable_episodes: false,
                max_compactions: None,
                shared_workspace_peer: None,
            }],
        };

        let err = suite.validate().unwrap_err().to_string();
        assert!(err.contains("at least one grader"));
    }

    #[test]
    fn missing_mock_read_paths_are_rejected() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("fixture")).unwrap();
        let suite = EvalSuite {
            path: temp.path().join("suite.toml"),
            base_dir: temp.path().to_path_buf(),
            id: "test".to_string(),
            seed: 1,
            repetitions: 1,
            budgets: EvalBudgets::default(),
            tasks: vec![EvalTask {
                id: "missing-read".to_string(),
                fixture: "fixture".to_string(),
                prompt: "read missing file".to_string(),
                profile: EvalAgentProfile::Full,
                mock: Some(MockScript {
                    read: vec!["missing.txt".to_string()],
                    write: Vec::new(),
                    final_response: "Done".to_string(),
                    transient_errors: 0,
                    truncated_responses: 0,
                    expected_user_message_count: None,
                    forbidden_request_substrings: Vec::new(),
                    final_request_contains: Vec::new(),
                    final_request_forbidden_substrings: Vec::new(),
                    tool_turns: Vec::new(),
                    tool_turn_content_chars: 0,
                    simulate_prompt_cache: false,
                    wait_for_cancellation: false,
                    wait_for_cancellation_after_tool_turns: None,
                }),
                graders: vec![GraderSpec::Assertion {
                    contains: vec!["Done".to_string()],
                    not_contains: Vec::new(),
                }],
                budgets: EvalBudgets::default(),
                expected_status: TaskStatus::Completed,
                expected_error_contains: Vec::new(),
                cancel_after_ms: None,
                cancel_after_provider_attempts: None,
                resume_after_interruption: false,
                resume_prompt: None,
                queued_messages: Vec::new(),
                mock_context_window_tokens: None,
                min_compactions: None,
                min_episode_evictions: None,
                disable_episodes: false,
                max_compactions: None,
                shared_workspace_peer: None,
            }],
        };

        let err = suite.validate().unwrap_err().to_string();
        assert!(err.contains("mock read path does not exist in fixture"));
    }

    #[tokio::test]
    async fn mock_provider_modifies_fixture_through_agent_loop() {
        let temp = tempfile::TempDir::new().unwrap();
        let fixture = temp.path().join("fixture");
        fs::create_dir_all(&fixture).unwrap();
        write_file(&fixture.join("README.md"), "old\n");

        let suite = EvalSuite {
            path: temp.path().join("suite.toml"),
            base_dir: temp.path().to_path_buf(),
            id: "test".to_string(),
            seed: 7,
            repetitions: 1,
            budgets: EvalBudgets::default(),
            tasks: vec![EvalTask {
                id: "mock-task".to_string(),
                fixture: "fixture".to_string(),
                prompt: "update readme".to_string(),
                profile: EvalAgentProfile::Full,
                mock: Some(MockScript {
                    read: vec!["README.md".to_string()],
                    write: vec![MockWrite {
                        path: "README.md".to_string(),
                        content: "new\n".to_string(),
                    }],
                    final_response: "Done.".to_string(),
                    transient_errors: 0,
                    truncated_responses: 0,
                    expected_user_message_count: None,
                    forbidden_request_substrings: Vec::new(),
                    final_request_contains: Vec::new(),
                    final_request_forbidden_substrings: Vec::new(),
                    tool_turns: Vec::new(),
                    tool_turn_content_chars: 0,
                    simulate_prompt_cache: false,
                    wait_for_cancellation: false,
                    wait_for_cancellation_after_tool_turns: None,
                }),
                graders: vec![GraderSpec::FileState {
                    path: "README.md".to_string(),
                    exists: Some(true),
                    contains: vec!["new".to_string()],
                    not_contains: vec!["old".to_string()],
                    exact_file: None,
                }],
                budgets: EvalBudgets::default(),
                expected_status: TaskStatus::Completed,
                expected_error_contains: Vec::new(),
                cancel_after_ms: None,
                cancel_after_provider_attempts: None,
                resume_after_interruption: false,
                resume_prompt: None,
                queued_messages: Vec::new(),
                mock_context_window_tokens: None,
                min_compactions: None,
                min_episode_evictions: None,
                disable_episodes: false,
                max_compactions: None,
                shared_workspace_peer: None,
            }],
        };
        let run_dir = temp.path().join("out");
        fs::create_dir_all(&run_dir).unwrap();
        let storage = Storage::open_at(run_dir.join("bonsai.db")).await.unwrap();
        let report = run_task(
            &suite,
            &suite.tasks[0],
            "mock-task",
            suite.seed,
            &run_dir,
            &storage,
            &ProviderSetup::Mock,
        )
        .await
        .unwrap();

        assert!(report.passed);
        assert_eq!(
            fs::read_to_string(run_dir.join("worktrees/mock-task/README.md")).unwrap(),
            "new\n"
        );
        assert!(report.usage.total_tokens > 0);
    }

    #[tokio::test]
    async fn release_gating_suite_runs_every_mock_scenario() {
        let temp = tempfile::TempDir::new().unwrap();
        let suite =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("eval/suites/release_gating.toml");
        let baseline =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("eval/baselines/release-v1.toml");
        let outcome = run(EvalCliConfig {
            suite,
            out_dir: temp.path().to_path_buf(),
            baseline: Some(baseline),
            fail_on_task_failure: true,
            ..EvalCliConfig::default()
        })
        .await
        .unwrap();

        assert_eq!((outcome.passed_tasks, outcome.total_tasks), (10, 10));
        assert!(!outcome.should_fail_process());
    }

    #[test]
    fn language_acceptance_suite_loads_all_release_languages() {
        let suite =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("eval/suites/language_acceptance.toml");
        let suite = EvalSuite::load(&suite).unwrap();

        assert_eq!(
            suite
                .tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            [
                "rust_inspect_edit_verify",
                "typescript_inspect_edit_verify",
                "python_inspect_edit_verify",
                "go_inspect_edit_verify",
            ]
        );
    }

    #[test]
    fn intent_authority_suite_covers_the_release_intent_matrix() {
        let suite =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("eval/suites/intent_authority.toml");
        let suite = EvalSuite::load(&suite).unwrap();

        assert_eq!(suite.repetitions, 3);
        assert!(suite.tasks.iter().all(|task| task.mock.is_none()));
        assert_eq!(
            suite
                .tasks
                .iter()
                .map(|task| (task.id.as_str(), task.profile))
                .collect::<Vec<_>>(),
            [
                ("full_explain_without_mutation", EvalAgentProfile::Full),
                ("smol_explain_without_mutation", EvalAgentProfile::Smol),
                (
                    "full_review_findings_without_mutation",
                    EvalAgentProfile::Full
                ),
                ("smol_verify_without_fixing", EvalAgentProfile::Smol),
                ("full_monitor_until_unchanged", EvalAgentProfile::Full),
                ("full_diagnose_then_fix", EvalAgentProfile::Full),
                ("smol_diagnose_then_fix", EvalAgentProfile::Smol),
                ("full_extend_established_parser", EvalAgentProfile::Full),
            ]
        );
        assert_eq!(repeated_task_id("diagnose", 2, 3), "diagnose-run-2");
        assert_eq!(repeated_task_id("diagnose", 1, 1), "diagnose");
    }

    #[tokio::test]
    async fn intent_continuity_suite_runs_every_mock_scenario() {
        let temp = tempfile::TempDir::new().unwrap();
        let suite =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("eval/suites/intent_continuity.toml");
        let outcome = run(EvalCliConfig {
            suite,
            out_dir: temp.path().to_path_buf(),
            fail_on_task_failure: true,
            ..EvalCliConfig::default()
        })
        .await
        .unwrap();

        assert_eq!((outcome.passed_tasks, outcome.total_tasks), (5, 5));
        assert!(!outcome.should_fail_process());
    }

    #[tokio::test]
    async fn runner_reports_failing_grader_without_aborting_suite() {
        let temp = tempfile::TempDir::new().unwrap();
        let fixture = temp.path().join("fixture");
        fs::create_dir_all(&fixture).unwrap();
        write_file(&fixture.join("file.txt"), "old\n");

        let suite_path = temp.path().join("suite.toml");
        write_file(
            &suite_path,
            r#"
id = "tiny"
seed = 3

[[tasks]]
id = "ok"
fixture = "fixture"
prompt = "write"

[tasks.mock]
read = ["file.txt"]
final_response = "Done."
[[tasks.mock.write]]
path = "file.txt"
content = "new\n"

[[tasks.graders]]
type = "file-state"
path = "file.txt"
contains = ["new"]

[[tasks]]
id = "bad"
fixture = "fixture"
prompt = "write"

[tasks.mock]
read = ["file.txt"]
final_response = "Done."
[[tasks.mock.write]]
path = "file.txt"
content = "new\n"

[[tasks.graders]]
type = "file-state"
path = "file.txt"
contains = ["not present"]
"#,
        );

        let config = EvalCliConfig {
            suite: suite_path,
            out_dir: temp.path().join("out"),
            json: false,
            ..EvalCliConfig::default()
        };
        let outcome = run(config).await.unwrap();

        assert_eq!(outcome.total_tasks, 2);
        assert_eq!(outcome.passed_tasks, 1);
        assert!(outcome.report_path.exists());
        assert!(outcome.summary_path.exists());
    }

    #[tokio::test]
    async fn runner_repeats_tasks_in_isolated_worktrees() {
        let temp = tempfile::TempDir::new().unwrap();
        let fixture = temp.path().join("fixture");
        fs::create_dir_all(&fixture).unwrap();
        write_file(&fixture.join("README.md"), "fixture\n");
        let suite_path = temp.path().join("suite.toml");
        write_file(
            &suite_path,
            r#"
id = "repeated"
seed = 9
repetitions = 2

[[tasks]]
id = "observe"
fixture = "fixture"
prompt = "Explain the fixture."

[tasks.mock]
final_response = "Done."

[[tasks.graders]]
type = "assertion"
contains = ["Done"]
"#,
        );
        let outcome = run(EvalCliConfig {
            suite: suite_path,
            out_dir: temp.path().join("out"),
            fail_on_task_failure: true,
            ..EvalCliConfig::default()
        })
        .await
        .unwrap();

        assert_eq!((outcome.passed_tasks, outcome.total_tasks), (2, 2));
        for repetition in 1..=2 {
            assert!(
                outcome
                    .run_dir
                    .join(format!("worktrees/observe-run-{repetition}/README.md"))
                    .is_file()
            );
        }
        let report = fs::read_to_string(&outcome.report_path).unwrap();
        assert!(report.contains("\"id\": \"observe-run-1\""));
        assert!(report.contains("\"id\": \"observe-run-2\""));
    }
}
