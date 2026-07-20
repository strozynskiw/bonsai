use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Result, bail};
use tokio::sync::Mutex;

use crate::agent::{Agent, AgentMode, ExecutionLane};
use crate::background::BackgroundTaskRegistry;
use crate::interaction::InteractionService;
use crate::lsp::LspHub;
use crate::model_catalog::ModelCatalog;
use crate::permissions::PermissionManager;
use crate::plan::{PlanDoc, SharedPlanStore};
use crate::provider::ProviderRegistry;
use crate::run_budget::RunBudget;
use crate::session::SessionStore;
use crate::storage::{SessionId, Storage};
use crate::todo::{SharedTodoStore, TodoStore};
use crate::tool::{ProjectInfoRuntime, ReadTracker, SharedActiveSessionId};

#[cfg(test)]
mod acceptance;

/// Product surface whose shared runtime is being assembled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurfaceKind {
    Tui,
    Headless,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InteractionMode {
    Interactive,
    NonInteractive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepoMapMode {
    Deferred,
    Eager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubagentWakeMode {
    Background,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerWaitMode {
    Parkable,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolatileContextMode {
    RefreshEachTurn,
    Frozen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SandboxPreferenceMode {
    PersistedInteractive,
    ConfigOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionBindingMode {
    Deferred,
    Required,
}

/// Frozen capability contract for one product surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeCapabilities {
    interaction: InteractionMode,
    repo_map: RepoMapMode,
    subagent_wake: SubagentWakeMode,
    peer_wait: PeerWaitMode,
    volatile_context: VolatileContextMode,
    sandbox_preferences: SandboxPreferenceMode,
    session_binding: SessionBindingMode,
}

impl SurfaceKind {
    const fn capabilities(self) -> RuntimeCapabilities {
        match self {
            Self::Tui => RuntimeCapabilities {
                interaction: InteractionMode::Interactive,
                repo_map: RepoMapMode::Deferred,
                subagent_wake: SubagentWakeMode::Background,
                peer_wait: PeerWaitMode::Parkable,
                volatile_context: VolatileContextMode::RefreshEachTurn,
                sandbox_preferences: SandboxPreferenceMode::PersistedInteractive,
                session_binding: SessionBindingMode::Deferred,
            },
            Self::Headless => RuntimeCapabilities {
                interaction: InteractionMode::NonInteractive,
                repo_map: RepoMapMode::Eager,
                subagent_wake: SubagentWakeMode::Disabled,
                peer_wait: PeerWaitMode::Terminal,
                volatile_context: VolatileContextMode::Frozen,
                sandbox_preferences: SandboxPreferenceMode::ConfigOnly,
                session_binding: SessionBindingMode::Required,
            },
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Tui => "TUI",
            Self::Headless => "headless",
        }
    }
}

/// Inputs whose construction or selection remains owned by the surface adapter.
pub(crate) struct RuntimeBuildRequest {
    pub(crate) surface: SurfaceKind,
    pub(crate) storage: Storage,
    pub(crate) project_root: PathBuf,
    pub(crate) session_project_root: PathBuf,
    pub(crate) model_catalog: Arc<ModelCatalog>,
    pub(crate) registry: Arc<ProviderRegistry>,
    pub(crate) session_store: Arc<Mutex<SessionStore>>,
    pub(crate) run_budget: RunBudget,
    pub(crate) interaction: Arc<InteractionService>,
    pub(crate) active_session_id: SharedActiveSessionId,
    pub(crate) yolo_mode: crate::yolo::YoloMode,
    pub(crate) current_session_id: Option<SessionId>,
    pub(crate) system_prompt_suffix: Option<String>,
    pub(crate) workspace_trust: crate::workspace_trust::WorkspaceTrustGate,
}

/// Shared services and configured Agent consumed by a thin surface adapter.
pub(crate) struct RuntimeCore {
    pub(crate) agent: Agent,
    pub(crate) storage: Storage,
    pub(crate) session_store: Arc<Mutex<SessionStore>>,
    pub(crate) registry: Arc<ProviderRegistry>,
    pub(crate) model_catalog: Arc<ModelCatalog>,
    pub(crate) todo_store: SharedTodoStore,
    pub(crate) plan_store: SharedPlanStore,
    pub(crate) interaction: Arc<InteractionService>,
    pub(crate) active_session_id: SharedActiveSessionId,
    pub(crate) background_tasks: Arc<BackgroundTaskRegistry>,
    pub(crate) terminals: Arc<crate::terminal::TerminalRegistry>,
    pub(crate) subagents: Arc<crate::subagent::SubagentRegistry>,
    pub(crate) subagent_model_overrides: crate::subagent::SubagentModelOverrides,
    pub(crate) permissions: PermissionManager,
    pub(crate) domain_permissions: PermissionManager,
    pub(crate) peer_bus: Arc<crate::peer::PeerBus>,
    pub(crate) hooks: Arc<crate::hooks::HookEngine>,
    pub(crate) run_budget: RunBudget,
}

/// Builds the surface-neutral runtime services and applies the explicit surface
/// capability contract at the few points where TUI and headless differ.
pub(crate) struct RuntimeBuilder {
    request: RuntimeBuildRequest,
}

impl RuntimeBuilder {
    pub(crate) fn new(request: RuntimeBuildRequest) -> Self {
        Self { request }
    }

    /// Assemble the shared runtime.
    ///
    /// # Errors
    ///
    /// Returns an error when persistent policy state cannot be loaded, shared
    /// extension services cannot initialize, the selected provider is invalid,
    /// or the requested surface/session combination is inconsistent.
    pub(crate) async fn build(self) -> Result<RuntimeCore> {
        let RuntimeBuildRequest {
            surface,
            storage,
            project_root,
            session_project_root,
            model_catalog,
            registry,
            session_store,
            run_budget,
            interaction,
            active_session_id,
            yolo_mode,
            current_session_id,
            system_prompt_suffix,
            workspace_trust,
        } = self.request;
        let capabilities = surface.capabilities();
        validate_session_binding(surface, capabilities.session_binding, current_session_id)?;
        let smol_preference = storage.smol_preference().await?;

        let read_tracker = ReadTracker::new();
        let lsp_hub = Arc::new(LspHub::new(project_root.clone()));
        let project_id = storage.ensure_project(&session_project_root).await?;
        let permissions = PermissionManager::load(storage.clone(), project_id).await?;
        let domain_permissions =
            PermissionManager::load_domains(storage.clone(), project_id).await?;
        let workspace_trusted = matches!(
            workspace_trust.state(),
            crate::workspace_trust::WorkspaceTrust::Trusted
        );
        if !workspace_trusted {
            tracing::warn!(
                surface = surface.label(),
                root = %session_project_root.display(),
                "workspace is restricted; project configuration and instructions are inert"
            );
        }

        let config = Arc::new(if workspace_trusted {
            crate::config::load(&project_root, storage.home_dir())
        } else {
            crate::config::load_without_project_config(&project_root, storage.home_dir())
        });
        for diagnostic in &config.diagnostics {
            tracing::warn!(%diagnostic, "config diagnostic");
        }

        let todo_store = Arc::new(Mutex::new(TodoStore::new()));
        let plan_store = Arc::new(Mutex::new(PlanDoc::default()));
        let sandbox = crate::sandbox::CommandSandbox::from_config(
            crate::sandbox::detect_backend(),
            &project_root,
            &config.sandbox,
        );
        if capabilities.sandbox_preferences == SandboxPreferenceMode::PersistedInteractive {
            restore_interactive_sandbox_preferences(&storage, &config, &sandbox).await;
        }
        let background_tasks = Arc::new(BackgroundTaskRegistry::with_sandbox(sandbox.clone()));
        let terminals = Arc::new(crate::terminal::TerminalRegistry::with_sandbox(
            sandbox.clone(),
        ));
        let subagents = Arc::new(crate::subagent::SubagentRegistry::new());
        let subagent_model_overrides = Arc::new(StdMutex::new(HashMap::new()));
        let builtin_subagent_settings = crate::subagent::SharedBuiltinSubagentSettings::new(
            storage.load_builtin_subagent_settings().await?,
        );
        let authorization_ledger = crate::tool::AuthorizationLedger::new(
            storage.clone(),
            active_session_id.clone(),
            sandbox.clone(),
            yolo_mode.clone(),
        );

        let skills = load_skills(&project_root, workspace_trusted);
        let custom_agents = load_custom_agents(&project_root, workspace_trusted);
        let memory = Arc::new(crate::memory::MemoryService::load(
            storage.home_dir(),
            &project_root,
            storage.clone(),
            project_id,
        ));
        let skills_snapshot = crate::resource::skill::snapshot(&skills);
        let mut project_context = crate::context::project_context_snapshot(&project_root)
            .with_skills_index(skills_snapshot.index_section())
            .with_smol_skills_index(skills_snapshot.user_index_section())
            .with_agents_index(crate::tool::agents_index_section_with_settings(
                &crate::resource::agent::snapshot(&custom_agents),
                &builtin_subagent_settings.snapshot(),
            ))
            .with_memory_index(memory.index_section());
        if capabilities.repo_map == RepoMapMode::Eager {
            project_context =
                project_context.with_repo_map(crate::tool::build_repo_map(&project_root));
        }
        if !workspace_trusted {
            project_context = project_context.restrict_untrusted_workspace();
        }

        let project_info_runtime = {
            let session_store = session_store.lock().await;
            Arc::new(ProjectInfoRuntime::new(Some(
                crate::model_resolution::project_info_provider_state(&registry, &session_store),
            )))
        };
        let peer_bus = Arc::new(crate::peer::PeerBus::new(
            storage.clone(),
            active_session_id.clone(),
            session_project_root.clone(),
        ));

        let mcp_permissions = PermissionManager::load_mcp(storage.clone(), project_id).await?;
        let extensions = Arc::new(crate::extension::status::ExtensionRegistry::new());
        let extension_gate = Arc::new(crate::extension::gate::ExtensionGate::new(
            mcp_permissions,
            interaction.clone(),
            yolo_mode.clone(),
            authorization_ledger.clone(),
        ));
        let (mcp_hub, mcp_tools) = crate::mcp::McpHub::connect_and_discover(
            &config,
            extension_gate,
            extensions.clone(),
            sandbox.clone(),
            project_root.clone(),
            crate::session::CredentialStore::with_home(storage.home_dir()),
            storage.credential_persistence().await?.unwrap_or_default(),
        )
        .await;

        let hook_permissions = PermissionManager::load_hooks(storage.clone(), project_id).await?;
        let hook_llm_provider: Arc<dyn crate::provider::Provider> = {
            let session_store = session_store.lock().await;
            Arc::from(crate::model_resolution::build_provider_for_optional_model(
                &registry,
                &session_store,
                Some(&model_catalog),
                Some("fast"),
            ))
        };
        let hook_execution_policy = crate::hooks::HookExecutionPolicy::new(
            sandbox.clone(),
            authorization_ledger.clone(),
            yolo_mode.clone(),
        );
        let hook_runtime = crate::hooks::HookRuntimeDeps::new(
            project_root.clone(),
            hook_permissions,
            interaction.clone(),
            extensions.clone(),
            hook_execution_policy,
        )
        .with_llm(hook_llm_provider);
        let hooks =
            Arc::new(crate::hooks::HookEngine::build_with_runtime(&config, hook_runtime).await);

        let episode_store =
            crate::episode::episodes_enabled().then(crate::episode::SharedEpisodeStore::default);
        let deps = crate::bootstrap::ToolRegistryDeps {
            project_root: project_root.clone(),
            read_tracker: read_tracker.clone(),
            lsp_hub: lsp_hub.clone(),
            project_info_runtime: project_info_runtime.clone(),
            permissions: permissions.clone(),
            domain_permissions: domain_permissions.clone(),
            interaction: interaction.clone(),
            todo_store: todo_store.clone(),
            plan_store: plan_store.clone(),
            background_tasks: background_tasks.clone(),
            terminals: terminals.clone(),
            yolo_mode: yolo_mode.clone(),
            sandbox: sandbox.clone(),
            workspace_locks: crate::tool::WorkspaceLockContext::new(
                project_root.clone(),
                storage.clone(),
                active_session_id.clone(),
            ),
            session_title: crate::bootstrap::SessionTitleToolDeps {
                storage: storage.clone(),
                active_session_id: active_session_id.clone(),
            },
            skills: skills.clone(),
            subagent_provider_factory: Some(crate::model_resolution::subagent_provider_factory(
                registry.clone(),
                session_store.clone(),
                Some(model_catalog.clone()),
                subagent_model_overrides.clone(),
            )),
            subagent_background_wake: capabilities.subagent_wake == SubagentWakeMode::Background,
            subagents: subagents.clone(),
            custom_agents: custom_agents.clone(),
            builtin_subagent_settings: builtin_subagent_settings.clone(),
            peer_bus: Some(peer_bus.clone()),
            peer_wake_when_done: capabilities.peer_wait == PeerWaitMode::Parkable,
            memory: Some(memory.clone()),
            mcp_tools,
            extensions: extensions.clone(),
            hooks: hooks.clone(),
            authorization_ledger,
            episode_store: episode_store.clone(),
        };
        let (tool_registry, planning_registry, smol_registry, subagent_runner) =
            crate::bootstrap::build_tool_registries(deps);

        let (provider, context_budget_tokens, prompt_estimator, model_identity) = {
            let session_store = session_store.lock().await;
            (
                crate::model_resolution::build_provider(
                    &registry,
                    &session_store,
                    Some(&model_catalog),
                ),
                crate::model_resolution::context_window_for_current_model_with_catalog(
                    &registry,
                    &session_store,
                    Some(&model_catalog),
                ) as usize,
                crate::model_resolution::prompt_estimator_for_current_model_with_catalog(
                    &registry,
                    &session_store,
                    Some(&model_catalog),
                ),
                crate::model_resolution::active_model_identity(&session_store),
            )
        };

        let mut agent_builder = Agent::builder(
            provider,
            tool_registry,
            planning_registry,
            read_tracker,
            project_root,
        )
        .smol_registry(smol_registry)
        .project_context_snapshot(project_context)
        .context_budget_tokens(context_budget_tokens)
        .smol_preference(smol_preference)
        .active_model_identity(model_identity)
        .project_info_runtime(project_info_runtime)
        .lsp_hub(lsp_hub)
        .run_budget(run_budget)
        .yolo_mode(yolo_mode)
        .sandbox(sandbox)
        .self_review_mode(crate::self_review::SelfReviewMode::from_env().unwrap_or_default())
        .skills(skills)
        .memory(memory)
        .custom_agents(custom_agents)
        .builtin_subagent_settings(builtin_subagent_settings)
        .subagent_runner(subagent_runner)
        .config(config)
        .mcp_hub(mcp_hub)
        .extensions(extensions)
        .hooks(hooks.clone());
        if capabilities.interaction == InteractionMode::Interactive {
            agent_builder = agent_builder.interaction(interaction.clone());
        }
        if let Some(session_id) = current_session_id {
            agent_builder =
                agent_builder.execution_lane(ExecutionLane::parent(session_id.to_string()));
        }
        if let Some(suffix) = system_prompt_suffix.as_deref() {
            agent_builder = agent_builder.system_prompt_suffix(suffix);
        }
        if let Some(episode_store) = episode_store {
            agent_builder = agent_builder.episode_store(episode_store);
        }
        let mut agent = agent_builder.build()?;
        if surface == SurfaceKind::Headless {
            agent.set_mode(AgentMode::Coding);
        }
        agent.set_prompt_estimator(prompt_estimator);
        agent.set_todo_store(todo_store.clone());
        agent.set_background_tasks(background_tasks.clone());
        agent.set_terminals(terminals.clone());
        agent.set_peer_bus(peer_bus.clone());
        agent.set_refresh_volatile(
            capabilities.volatile_context == VolatileContextMode::RefreshEachTurn,
        );

        fire_session_start_hooks(&mut agent, &hooks).await;

        Ok(RuntimeCore {
            agent,
            storage,
            session_store,
            registry,
            model_catalog,
            todo_store,
            plan_store,
            interaction,
            active_session_id,
            background_tasks,
            terminals,
            subagents,
            subagent_model_overrides,
            permissions,
            domain_permissions,
            peer_bus,
            hooks,
            run_budget,
        })
    }
}

fn validate_session_binding(
    surface: SurfaceKind,
    mode: SessionBindingMode,
    current_session_id: Option<SessionId>,
) -> Result<()> {
    match (mode, current_session_id) {
        (SessionBindingMode::Deferred, None) | (SessionBindingMode::Required, Some(_)) => Ok(()),
        (SessionBindingMode::Deferred, Some(_)) => {
            bail!(
                "{} runtime assembly must defer session binding",
                surface.label()
            )
        }
        (SessionBindingMode::Required, None) => {
            bail!(
                "{} runtime assembly requires an active session id",
                surface.label()
            )
        }
    }
}

async fn restore_interactive_sandbox_preferences(
    storage: &Storage,
    config: &crate::config::Config,
    sandbox: &crate::sandbox::CommandSandbox,
) {
    if std::env::var_os("BONSAI_SANDBOX").is_none()
        && let Ok(Some(enabled)) = storage.sandbox_enabled().await
    {
        sandbox.set_enabled(enabled);
    }
    if std::env::var_os("BONSAI_SANDBOX_NETWORK").is_none()
        && config.sandbox.deny_network.is_none()
        && let Ok(Some(deny_network)) = storage.sandbox_deny_network().await
    {
        sandbox.set_deny_network(deny_network);
    }
}

fn load_skills(
    project_root: &std::path::Path,
    workspace_trusted: bool,
) -> crate::resource::skill::SharedSkillRegistry {
    if workspace_trusted {
        crate::resource::skill::shared_registry(crate::resource::skill::SkillRegistry::load(
            project_root,
        ))
    } else {
        crate::resource::skill::shared_registry_untrusted(
            crate::resource::skill::SkillRegistry::load_untrusted(project_root),
        )
    }
}

fn load_custom_agents(
    project_root: &std::path::Path,
    workspace_trusted: bool,
) -> crate::resource::agent::SharedAgentRegistry {
    if workspace_trusted {
        crate::resource::agent::shared_registry(crate::resource::agent::AgentRegistry::load(
            project_root,
        ))
    } else {
        crate::resource::agent::shared_registry_untrusted(
            crate::resource::agent::AgentRegistry::load_untrusted(project_root),
        )
    }
}

async fn fire_session_start_hooks(agent: &mut Agent, hooks: &Arc<crate::hooks::HookEngine>) {
    let session_start = hooks
        .fire(
            crate::hooks::HookEvent::SessionStart,
            crate::hooks::HookContext::default(),
        )
        .await;
    for warning in &session_start.warnings {
        tracing::warn!(%warning, "SessionStart hook warning");
    }
    if let crate::hooks::HookDecision::AddContext { text } = session_start.decision {
        agent.push_user_context_note(&crate::tool::wrap_untrusted_content(
            "hook:SessionStart",
            &text,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_capability_matrix_is_explicit_and_complete() {
        assert_eq!(
            SurfaceKind::Tui.capabilities(),
            RuntimeCapabilities {
                interaction: InteractionMode::Interactive,
                repo_map: RepoMapMode::Deferred,
                subagent_wake: SubagentWakeMode::Background,
                peer_wait: PeerWaitMode::Parkable,
                volatile_context: VolatileContextMode::RefreshEachTurn,
                sandbox_preferences: SandboxPreferenceMode::PersistedInteractive,
                session_binding: SessionBindingMode::Deferred,
            }
        );
        assert_eq!(
            SurfaceKind::Headless.capabilities(),
            RuntimeCapabilities {
                interaction: InteractionMode::NonInteractive,
                repo_map: RepoMapMode::Eager,
                subagent_wake: SubagentWakeMode::Disabled,
                peer_wait: PeerWaitMode::Terminal,
                volatile_context: VolatileContextMode::Frozen,
                sandbox_preferences: SandboxPreferenceMode::ConfigOnly,
                session_binding: SessionBindingMode::Required,
            }
        );
    }

    #[test]
    fn session_binding_matches_surface_lifecycle() {
        let session_id = SessionId::from_raw(42);

        assert!(
            validate_session_binding(SurfaceKind::Tui, SessionBindingMode::Deferred, None,).is_ok()
        );
        assert!(
            validate_session_binding(
                SurfaceKind::Headless,
                SessionBindingMode::Required,
                Some(session_id),
            )
            .is_ok()
        );
        assert!(
            validate_session_binding(
                SurfaceKind::Tui,
                SessionBindingMode::Deferred,
                Some(session_id),
            )
            .is_err()
        );
        assert!(
            validate_session_binding(SurfaceKind::Headless, SessionBindingMode::Required, None,)
                .is_err()
        );
    }
}
