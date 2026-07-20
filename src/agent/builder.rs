//! The `AgentBuilder` used to assemble an [`Agent`]. Fields are `pub(super)` so
//! [`Agent::build`] (in the module root) can consume them.

use super::*;

const REVIEW_TOOL_NAMES: [&str; 14] = [
    "project_info",
    "read",
    "glob",
    "grep",
    "symbol_search",
    "definition",
    "references",
    "hover",
    "workspace_symbol",
    "git",
    "skill",
    "question",
    // Findings tools: review is read-only w.r.t. *files*, but recording a
    // structured finding on the plan canvas (like the planner writes the canvas)
    // is allowed, so review output survives as data, not just transcript prose.
    "plan_add_finding",
    "plan_associate_finding",
];

/// Read-only conversational tools a custom switcher persona may scope from: no
/// write/edit/bash, no plan-canvas tools.
const READ_ONLY_TOOL_NAMES: [&str; 13] = [
    "project_info",
    "read",
    "glob",
    "grep",
    "symbol_search",
    "definition",
    "references",
    "hover",
    "workspace_symbol",
    "git",
    "skill",
    "question",
    "agent",
];

/// Fallback SMOL subset projected from the coding registry when no assembled
/// SMOL registry is injected. Keep the names and order in sync with the
/// `ToolProfile::Smol` column of the tool descriptor table.
const SMOL_TOOL_NAMES: [&str; 7] = [
    "read",
    "write",
    "edit",
    "bash",
    "terminal",
    "todowrite",
    "set_session_title",
];
/// SMOL with user-provided skills: the `skill` tool joins the minimal set so
/// the skills advertised by the SMOL prompt's user-skills index stay loadable.
/// Built-in skills are not advertised in SMOL, so without user skills the tool
/// (and its schema cost) is omitted entirely.
const SMOL_TOOL_NAMES_WITH_SKILLS: [&str; 8] = [
    "read",
    "write",
    "edit",
    "bash",
    "terminal",
    "todowrite",
    "set_session_title",
    "skill",
];

impl Agent {
    pub(crate) fn builder(
        provider: Box<dyn Provider>,
        coding_registry: Arc<ToolRegistry>,
        planning_registry: Arc<ToolRegistry>,
        read_tracker: ReadTracker,
        project_root: PathBuf,
    ) -> AgentBuilder {
        AgentBuilder::new(
            provider,
            coding_registry,
            planning_registry,
            read_tracker,
            project_root,
        )
    }

    /// Project a named subset of `source` into a fresh registry (skipping names
    /// the source doesn't have). The shared mechanism behind the read-mostly
    /// personas.
    fn registry_subset(source: &Arc<ToolRegistry>, names: &[&str]) -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        for name in names {
            if let Some(tool) = source.get(name) {
                registry.register(tool);
            }
        }
        Arc::new(registry)
    }

    fn review_registry_from(
        planning_registry: &Arc<ToolRegistry>,
        subagent_runner: Option<&SubagentRunner>,
        custom_agents: &crate::resource::agent::SharedAgentRegistry,
        builtin_subagent_settings: &crate::subagent::SharedBuiltinSubagentSettings,
    ) -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        for name in REVIEW_TOOL_NAMES {
            if let Some(tool) = planning_registry.get(name) {
                registry.register(tool);
            }
        }
        if let Some(runner) = subagent_runner {
            // Review delegation is intentionally built-in-only. The general
            // agent tool can resolve custom agents with mutating frontmatter,
            // which would violate the review persona's read-only boundary.
            registry.register(Arc::new(
                crate::tool::AgentTool::new_builtin_only_with_settings(
                    "agent",
                    runner.clone(),
                    custom_agents.clone(),
                    builtin_subagent_settings.clone(),
                ),
            ));
        }
        Arc::new(registry)
    }

    fn read_only_registry_from(coding_registry: &Arc<ToolRegistry>) -> Arc<ToolRegistry> {
        Self::registry_subset(coding_registry, &READ_ONLY_TOOL_NAMES)
    }

    fn smol_registry_from(
        coding_registry: &Arc<ToolRegistry>,
        has_user_skills: bool,
    ) -> Arc<ToolRegistry> {
        let names: &[&str] = if has_user_skills {
            &SMOL_TOOL_NAMES_WITH_SKILLS
        } else {
            &SMOL_TOOL_NAMES
        };
        Self::registry_subset(coding_registry, names)
    }

    #[cfg(test)]
    pub fn new(
        provider: Box<dyn Provider>,
        coding_registry: Arc<ToolRegistry>,
        planning_registry: Arc<ToolRegistry>,
        read_tracker: ReadTracker,
        system_context: String,
        project_root: PathBuf,
    ) -> Result<Self> {
        Self::builder(
            provider,
            coding_registry,
            planning_registry,
            read_tracker,
            project_root,
        )
        .system_context(system_context)
        .build()
    }

    fn build(mut builder: AgentBuilder) -> Result<Self> {
        let mode = AgentMode::Coding;
        let smol_preference = builder.smol_preference;
        let system_message = system_message_with_suffix(
            mode,
            &builder.system_context,
            builder.system_prompt_suffix.as_deref(),
        );
        let review_registry = Self::review_registry_from(
            &builder.planning_registry,
            builder.subagent_runner.as_ref(),
            &builder.custom_agents,
            &builder.builtin_subagent_settings,
        );
        let read_only_registry = Self::read_only_registry_from(&builder.coding_registry);
        let smol_registry = builder.smol_registry.unwrap_or_else(|| {
            Self::smol_registry_from(
                &builder.coding_registry,
                crate::resource::skill::snapshot(&builder.skills).has_user_skills(),
            )
        });

        let conversation_cache_key = crate::provider::new_conversation_cache_key();
        builder
            .provider
            .set_conversation_cache_key(&conversation_cache_key);
        let mut agent = Self {
            provider: builder.provider,
            provider_fallback: None,
            conversation_cache_key,
            tool_registry: builder.coding_registry.clone(),
            registries: ToolRegistrySet {
                coding: builder.coding_registry,
                planning: builder.planning_registry,
                smol: smol_registry,
                review: review_registry,
                read_only: read_only_registry,
            },
            background_tasks: Arc::new(BackgroundTaskRegistry::new()),
            terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
            peer_bus: None,
            pending_peer_delivery_receipts: BTreeMap::new(),
            mode,
            smol_preference: crate::smol::SmolPreference::Off,
            smol_mode: false,
            active_persona: ActivePersona::Builtin(mode),
            read_tracker: builder.read_tracker,
            lsp_hub: builder.lsp_hub,
            todo_store: None,
            messages: vec![system_message],
            max_iterations: builder.max_iterations,
            cached_models: Vec::new(),
            transcript_logger: TranscriptLogger::from_env().map(Arc::new),
            system_context: builder.system_context,
            system_prompt_suffix: builder.system_prompt_suffix,
            project_context: builder.project_context,
            repair_advisory: String::new(),
            read_coverage_advisory: String::new(),
            planning_advisory: String::new(),
            subagent_status_advisory: String::new(),
            last_volatile_context_message: None,
            project_root: builder.project_root,
            refresh_volatile: false,
            run_start_dirty_paths: None,
            usage: SessionUsage::default(),
            active_model_identity: builder.active_model_identity,
            execution_lane: builder.execution_lane,
            retry_backoff: RetryBackoff::Exponential,
            message_ids: vec![format_context_message_id(0)],
            next_message_id: 1,
            prompt_estimator: PromptEstimator::default(),
            tool_schema_cache: StdMutex::new(HashMap::new()),
            last_prompt_estimate: None,
            last_sent_prompt_estimate: None,
            last_perf_report: None,
            previous_request_body: None,
            cache_warning_lanes: HashSet::new(),
            context_budget_tokens: builder.context_budget_tokens.max(1),
            max_generation_duration: builder.max_generation_duration,
            max_streamed_chars: builder.max_streamed_chars,
            max_tool_duration: builder.max_tool_duration,
            max_session_turns: builder.max_session_turns,
            max_session_output_chars: builder.max_session_output_chars,
            max_session_active_seconds: builder.max_session_active_seconds,
            max_session_cost_micros: builder.max_session_cost_micros,
            last_background_status_report: None,
            last_terminal_status_report: None,
            tool_context_details: HashMap::new(),
            inspection_events: HashMap::new(),
            mention_read_evidence: HashMap::new(),
            delegated_read_evidence: Vec::new(),
            delegated_overlap_advised: HashSet::new(),
            next_subagent_launch_group_id: 1,
            context_controls: HashMap::new(),
            summary_sources: HashMap::new(),
            compaction_events: Vec::new(),
            pending_context_rewrite: PendingContextRewrite::default(),
            yolo_mode: builder.yolo_mode,
            sandbox: builder.sandbox,
            project_info_runtime: builder.project_info_runtime,
            self_review: SelfReviewState::with_mode(builder.self_review),
            self_review_runs: Vec::new(),
            interaction: builder.interaction,
            skills: builder.skills,
            loaded_skills: std::collections::BTreeSet::new(),
            memory: builder.memory,
            custom_agents: builder.custom_agents,
            builtin_subagent_settings: builder.builtin_subagent_settings,
            subagent_runner: builder.subagent_runner,
            tool_origin: builder.tool_origin,
            config: builder.config,
            mcp_hub: builder.mcp_hub,
            extensions: builder.extensions,
            hooks: builder.hooks,
            verification_runs: Vec::new(),
            active_verification: None,
            after_edit_verification_pending: false,
            after_edit_verification_injected: false,
            last_retryable_turn: false,
            episode_store: builder.episode_store,
        };
        agent.set_smol_preference(smol_preference);
        agent.refresh_project_info_runtime();
        Ok(agent)
    }
}

pub(crate) struct AgentBuilder {
    pub(super) provider: Box<dyn Provider>,
    pub(super) coding_registry: Arc<ToolRegistry>,
    pub(super) planning_registry: Arc<ToolRegistry>,
    pub(super) smol_registry: Option<Arc<ToolRegistry>>,
    pub(super) read_tracker: ReadTracker,
    pub(super) lsp_hub: Option<Arc<LspHub>>,
    pub(super) project_root: PathBuf,
    pub(super) system_context: String,
    pub(super) system_prompt_suffix: Option<String>,
    pub(super) project_context: Option<ProjectContextSnapshot>,
    /// `None` in tests/evals without a run selection; usage turns then carry no
    /// per-turn model attribution and analytics fall back to the session model.
    pub(super) active_model_identity: Option<ActiveModelIdentity>,
    pub(super) execution_lane: ExecutionLane,
    pub(super) context_budget_tokens: usize,
    pub(super) smol_preference: crate::smol::SmolPreference,
    pub(super) max_generation_duration: Option<Duration>,
    pub(super) max_streamed_chars: Option<usize>,
    pub(super) max_tool_duration: Option<Duration>,
    pub(super) max_session_turns: Option<usize>,
    pub(super) max_session_output_chars: Option<usize>,
    pub(super) max_session_active_seconds: Option<u64>,
    pub(super) max_session_cost_micros: Option<u64>,
    pub(super) yolo_mode: YoloMode,
    pub(super) sandbox: CommandSandbox,
    pub(super) project_info_runtime: Option<Arc<ProjectInfoRuntime>>,
    pub(super) self_review: SelfReviewMode,
    pub(super) interaction: Option<Arc<InteractionService>>,
    pub(super) skills: SharedSkillRegistry,
    pub(super) memory: Option<Arc<crate::memory::MemoryService>>,
    pub(super) custom_agents: crate::resource::agent::SharedAgentRegistry,
    pub(super) builtin_subagent_settings: crate::subagent::SharedBuiltinSubagentSettings,
    pub(super) subagent_runner: Option<SubagentRunner>,
    pub(super) tool_origin: Option<String>,
    pub(super) max_iterations: usize,
    pub(super) config: Arc<crate::config::Config>,
    /// `None` in evals/tests, which never connect to real MCP servers.
    pub(super) mcp_hub: Option<Arc<crate::mcp::McpHub>>,
    pub(super) extensions: Arc<crate::extension::status::ExtensionRegistry>,
    pub(super) hooks: Arc<crate::hooks::HookEngine>,
    /// Episode ledger. `None` keeps every episode hook inert (subagents and
    /// isolated unit tests); production callers wire it by default unless the
    /// explicit `BONSAI_EPISODES=0` kill switch is set.
    pub(super) episode_store: Option<crate::episode::SharedEpisodeStore>,
}

impl AgentBuilder {
    pub(super) fn new(
        provider: Box<dyn Provider>,
        coding_registry: Arc<ToolRegistry>,
        planning_registry: Arc<ToolRegistry>,
        read_tracker: ReadTracker,
        project_root: PathBuf,
    ) -> Self {
        Self {
            provider,
            coding_registry,
            planning_registry,
            smol_registry: None,
            read_tracker,
            lsp_hub: None,
            project_root,
            system_context: String::new(),
            system_prompt_suffix: None,
            project_context: None,
            active_model_identity: None,
            execution_lane: ExecutionLane::default(),
            context_budget_tokens: DEFAULT_CONTEXT_WINDOW_TOKENS as usize,
            smol_preference: crate::smol::SmolPreference::Off,
            max_generation_duration: None,
            max_streamed_chars: None,
            max_tool_duration: None,
            max_session_turns: None,
            max_session_output_chars: None,
            max_session_active_seconds: None,
            max_session_cost_micros: None,
            yolo_mode: YoloMode::new(),
            sandbox: CommandSandbox::disabled(),
            project_info_runtime: None,
            self_review: SelfReviewMode::default(),
            interaction: None,
            skills: crate::resource::skill::shared_registry(SkillRegistry::empty()),
            memory: None,
            custom_agents: crate::resource::agent::shared_registry(AgentRegistry::empty()),
            builtin_subagent_settings: crate::subagent::SharedBuiltinSubagentSettings::default(),
            subagent_runner: None,
            tool_origin: None,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            config: Arc::new(crate::config::Config::empty()),
            mcp_hub: None,
            extensions: Arc::new(crate::extension::status::ExtensionRegistry::new()),
            hooks: Arc::new(crate::hooks::HookEngine::disabled()),
            episode_store: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn system_context(mut self, system_context: impl Into<String>) -> Self {
        self.system_context = system_context.into();
        self.project_context = None;
        self
    }

    pub(crate) fn project_context_snapshot(
        mut self,
        project_context: ProjectContextSnapshot,
    ) -> Self {
        self.system_context = match self.provider.project_state_cache_strategy() {
            crate::provider::ProjectStateCacheStrategy::MutableSystemTail => {
                // IMPORTANT COMPATIBILITY: providers without an explicit
                // rolling-history policy keep their verified system-tail
                // representation. Do not apply append-only state globally.
                project_context.render()
            }
            crate::provider::ProjectStateCacheStrategy::AppendOnlyHistory => {
                // IMPORTANT CACHE INVARIANT: only the byte-stable half belongs
                // in message zero; changing state is appended later.
                project_context.cacheable_prefix()
            }
        };
        self.project_context = Some(project_context);
        self
    }

    pub(crate) fn system_prompt_suffix(mut self, suffix: impl Into<String>) -> Self {
        let suffix = suffix.into();
        self.system_prompt_suffix = (!suffix.trim().is_empty()).then_some(suffix);
        self
    }

    pub(crate) fn context_budget_tokens(mut self, context_budget_tokens: usize) -> Self {
        self.context_budget_tokens = context_budget_tokens;
        self
    }

    pub(crate) fn smol_preference(mut self, smol_preference: crate::smol::SmolPreference) -> Self {
        self.smol_preference = smol_preference;
        self
    }

    pub(crate) fn run_budget(mut self, budget: crate::run_budget::RunBudget) -> Self {
        self.max_iterations = budget.max_turns.unwrap_or(DEFAULT_MAX_ITERATIONS);
        self.max_generation_duration = budget.max_generation_duration();
        self.max_streamed_chars = budget.max_output_chars;
        self.max_tool_duration = budget.max_tool_duration();
        self.max_session_turns = budget.max_session_turns;
        self.max_session_output_chars = budget.max_session_output_chars;
        self.max_session_active_seconds = budget.max_session_active_seconds;
        self.max_session_cost_micros = budget.max_session_cost_micros;
        self
    }

    pub(crate) fn active_model_identity(mut self, identity: ActiveModelIdentity) -> Self {
        self.active_model_identity = Some(identity);
        self
    }

    pub(crate) fn execution_lane(mut self, lane: ExecutionLane) -> Self {
        self.execution_lane = lane;
        self
    }

    pub(crate) fn smol_registry(mut self, smol_registry: Arc<ToolRegistry>) -> Self {
        self.smol_registry = Some(smol_registry);
        self
    }

    pub(crate) fn yolo_mode(mut self, yolo_mode: YoloMode) -> Self {
        self.yolo_mode = yolo_mode;
        self
    }

    pub(crate) fn lsp_hub(mut self, lsp_hub: Arc<LspHub>) -> Self {
        self.lsp_hub = Some(lsp_hub);
        self
    }

    pub(crate) fn sandbox(mut self, sandbox: CommandSandbox) -> Self {
        self.sandbox = sandbox;
        self
    }

    pub(crate) fn project_info_runtime(mut self, runtime: Arc<ProjectInfoRuntime>) -> Self {
        self.project_info_runtime = Some(runtime);
        self
    }

    pub(crate) fn self_review_mode(mut self, mode: SelfReviewMode) -> Self {
        self.self_review = mode;
        self
    }

    pub(crate) fn interaction(mut self, interaction: Arc<InteractionService>) -> Self {
        self.interaction = Some(interaction);
        self
    }

    pub(crate) fn skills(mut self, skills: SharedSkillRegistry) -> Self {
        self.skills = skills;
        self
    }

    pub(crate) fn memory(mut self, memory: Arc<crate::memory::MemoryService>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub(crate) fn custom_agents(
        mut self,
        custom_agents: crate::resource::agent::SharedAgentRegistry,
    ) -> Self {
        self.custom_agents = custom_agents;
        self
    }

    pub(crate) fn builtin_subagent_settings(
        mut self,
        settings: crate::subagent::SharedBuiltinSubagentSettings,
    ) -> Self {
        self.builtin_subagent_settings = settings;
        self
    }

    pub(crate) fn subagent_runner(mut self, runner: Option<SubagentRunner>) -> Self {
        self.subagent_runner = runner;
        self
    }

    pub(crate) fn tool_origin(mut self, origin: impl Into<String>) -> Self {
        self.tool_origin = Some(origin.into());
        self
    }

    /// The layered `.bonsai/config.toml` view, for `/config` and
    /// (later) the extension/MCP/hooks runtimes. Defaults to
    /// [`crate::config::Config::empty`] so evals and unit tests that never
    /// call this still build.
    pub(crate) fn config(mut self, config: Arc<crate::config::Config>) -> Self {
        self.config = config;
        self
    }

    /// The connected MCP hub, for `/mcp enable|disable|reload`. `None`
    /// outside interactive/headless runs (evals stay deterministic; no MCP
    /// surface exists there).
    pub(crate) fn mcp_hub(mut self, mcp_hub: Arc<crate::mcp::McpHub>) -> Self {
        self.mcp_hub = Some(mcp_hub);
        self
    }

    /// Shared extension status store, for `/mcp`'s per-server view.
    pub(crate) fn extensions(
        mut self,
        extensions: Arc<crate::extension::status::ExtensionRegistry>,
    ) -> Self {
        self.extensions = extensions;
        self
    }

    /// The hooks engine. Defaults to [`crate::hooks::HookEngine::disabled`]
    /// so evals and unit tests that never call this still build.
    pub(crate) fn hooks(mut self, hooks: Arc<crate::hooks::HookEngine>) -> Self {
        self.hooks = hooks;
        self
    }

    /// Wire the episode ledger (task-scoped context lifecycle). Callers gate on
    /// [`crate::episode::episodes_enabled`]; an unwired agent has zero episode
    /// behavior.
    pub(crate) fn episode_store(mut self, store: crate::episode::SharedEpisodeStore) -> Self {
        self.episode_store = Some(store);
        self
    }

    /// Cap the agent's tool-call iterations. Subagents set this low to
    /// bound a runaway nested run.
    pub(crate) fn max_iterations(mut self, max_iterations: usize) -> Self {
        self.max_iterations = max_iterations.max(1);
        self
    }

    pub(crate) fn build(self) -> Result<Agent> {
        Agent::build(self)
    }
}
