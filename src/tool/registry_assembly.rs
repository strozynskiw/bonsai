//! Assembles the per-profile tool registries (coding, planning, SMOL,
//! subagent) from runtime dependencies. Extracted from bootstrap so registry
//! composition evolves separately from startup wiring.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::background::BackgroundTaskRegistry;
use crate::lsp::LspHub;
use crate::permissions::PermissionManager;
use crate::storage::Storage;
use crate::tool::apply_patch::ApplyPatchTool;
use crate::tool::diagnostics::DiagnosticsTool;
use crate::tool::enter_plan_mode::EnterPlanModeTool;
use crate::tool::git::GitTool;
use crate::tool::glob::GlobTool;
use crate::tool::grep::GrepTool;
use crate::tool::lsp::{
    DefinitionTool, HoverTool, ReferencesTool, RenameSymbolTool, WorkspaceSymbolTool,
};
use crate::tool::memory_write::MemoryWriteTool;
use crate::tool::peers::PeersTool;
use crate::tool::plan::{
    PlanAddFindingTool, PlanAddQuestionTool, PlanAssociateFindingTool, PlanCheckTaskTool,
    PlanInsertTaskTool, PlanMovePhaseTool, PlanMoveSectionTool, PlanPatchSectionTool,
    PlanRemovePhaseTool, PlanRemoveQuestionTool, PlanRemoveSectionTool, PlanRemoveTaskTool,
    PlanReplaceDraftTool, PlanResolveFindingTool, PlanUncheckTaskTool, PlanUpdateTaskTool,
};
use crate::tool::profile::{RegistrationStage, ToolFactoryKey, ToolProfile, descriptors_for};
use crate::tool::question::QuestionTool;
use crate::tool::recall::RecallTool;
use crate::tool::set_session_title::SetSessionTitleTool;
use crate::tool::skill::SkillTool;
use crate::tool::symbol_search::SymbolSearchTool;
use crate::tool::tasks::TasksTool;
use crate::tool::terminal::TerminalTool;
use crate::tool::todo_write::TodoWriteTool;
use crate::tool::websearch::WebSearchTool;
use crate::tool::{
    ActionPolicy, AgentTool, BashExecutionPolicy, BashRuntimeDeps, BashTool, EditTool,
    ProjectInfoRuntime, ProjectInfoTool, ReadRegionTool, ReadSymbolTool, ReadTool, ReadTracker,
    SharedActiveSessionId, SubagentRunner, SubagentToolRegistryFactory, Tool, ToolRegistry,
    WriteTool,
};

pub(crate) struct ToolRegistryDeps {
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) read_tracker: ReadTracker,
    pub(crate) lsp_hub: Arc<LspHub>,
    pub(crate) project_info_runtime: Arc<ProjectInfoRuntime>,
    pub(crate) permissions: PermissionManager,
    /// Per-domain allowlist for the WebFetch tool. A separate manager from
    /// `permissions` (different `kind` namespace), so bash and domain rules never
    /// match each other.
    pub(crate) domain_permissions: PermissionManager,
    pub(crate) interaction: Arc<crate::interaction::InteractionService>,
    pub(crate) todo_store: crate::todo::SharedTodoStore,
    pub(crate) plan_store: crate::plan::SharedPlanStore,
    pub(crate) background_tasks: Arc<BackgroundTaskRegistry>,
    pub(crate) terminals: Arc<crate::terminal::TerminalRegistry>,
    pub(crate) yolo_mode: crate::yolo::YoloMode,
    pub(crate) sandbox: crate::sandbox::CommandSandbox,
    pub(crate) workspace_locks: crate::tool::WorkspaceLockContext,
    pub(crate) session_title: SessionTitleToolDeps,
    pub(crate) skills: crate::resource::skill::SharedSkillRegistry,
    /// Provider factory for nested subagents. `None` disables the `agent`
    /// tool (e.g. in evals), so no subagents are spawnable there.
    pub(crate) subagent_provider_factory: Option<crate::tool::SubagentProviderFactory>,
    /// Whether this session has a background wake path that drains completed
    /// detached subagents back into the parent agent.
    pub(crate) subagent_background_wake: bool,
    /// Live registry of subagent runs, shared with the `/subagents` view.
    pub(crate) subagents: Arc<crate::subagent::SubagentRegistry>,
    /// User-defined custom agents, shared with the `agent` tool.
    pub(crate) custom_agents: crate::resource::agent::SharedAgentRegistry,
    /// Durable settings for compiled delegated subagents, shared with every
    /// agent-tool instance and the interactive editor.
    pub(crate) builtin_subagent_settings: crate::subagent::SharedBuiltinSubagentSettings,
    /// Inter-agent communication bus (peers P2). `None` disables the `peers`
    /// tool (evals stay deterministic; no peer surface exists there).
    pub(crate) peer_bus: Option<Arc<crate::peer::PeerBus>>,
    /// Whether this surface can park a turn on `AgentRunResult::Waiting` and
    /// resume it later. Only the TUI can; headless/eval runs treat a
    /// nonterminal wait as a hard error, so `peers wake_when_done` is gated
    /// out of their schema entirely.
    pub(crate) peer_wake_when_done: bool,
    /// Persistent memory. `None` disables the `memory_write` tool
    /// (evals stay deterministic and never touch the user's memory stores).
    pub(crate) memory: Option<Arc<crate::memory::MemoryService>>,
    /// Discovered MCP tools, already connected — registration only
    /// needs the collision guard, not another round-trip. Empty in evals.
    pub(crate) mcp_tools: Vec<Arc<crate::mcp::McpTool>>,
    /// Shared extension status store, for the collision-degrade path
    /// and later `/mcp`/`/hooks` commands.
    pub(crate) extensions: Arc<crate::extension::status::ExtensionRegistry>,
    /// The hooks engine, for bash and the file-mutation tools.
    pub(crate) hooks: Arc<crate::hooks::HookEngine>,
    pub(crate) authorization_ledger: crate::tool::AuthorizationLedger,
    /// Episode ledger for the `recall` tool. `Some` only when the episodes
    /// feature is on for this session; `None` keeps every registry tail
    /// byte-identical to the pre-episodes wire arrays.
    pub(crate) episode_store: Option<crate::episode::SharedEpisodeStore>,
}

/// The shared instances of the read-only exploration core — the tools every
/// registry (coding, planning, subagent) offers. `project_info`/`read`/`grep`
/// are NOT here: they bind a read tracker / info runtime, so each registry
/// supplies its own instances to [`ReadOnlyCore::insert_into`].
struct ReadOnlyCore {
    glob: Arc<GlobTool>,
    symbol: Arc<SymbolSearchTool>,
    definition: Arc<DefinitionTool>,
    references: Arc<ReferencesTool>,
    hover: Arc<HoverTool>,
    workspace_symbol: Arc<WorkspaceSymbolTool>,
    git: Arc<GitTool>,
}

#[derive(Clone, Default)]
struct ToolInstances {
    tools: HashMap<ToolFactoryKey, Arc<dyn Tool>>,
}

impl ToolInstances {
    fn insert(&mut self, key: ToolFactoryKey, tool: Arc<dyn Tool>) {
        let previous = self.tools.insert(key, tool);
        assert!(
            previous.is_none(),
            "duplicate tool factory binding: {key:?}"
        );
    }

    fn get(&self, key: ToolFactoryKey) -> Option<Arc<dyn Tool>> {
        self.tools.get(&key).cloned()
    }

    fn register_stage(
        &self,
        registry: &mut ToolRegistry,
        profile: ToolProfile,
        stage: RegistrationStage,
    ) {
        for descriptor in descriptors_for(profile, stage) {
            let Some(tool) = self.tools.get(&descriptor.factory) else {
                assert!(
                    descriptor.optional(profile),
                    "missing required {profile:?} tool binding: {} ({:?})",
                    descriptor.name,
                    descriptor.factory
                );
                continue;
            };
            assert_eq!(tool.name(), descriptor.name);
            assert_eq!(tool.effect_policy(), descriptor.effect);
            assert_eq!(tool.parallel_policy(), descriptor.parallel);
            registry.register(tool.clone());
        }
    }
}

impl ReadOnlyCore {
    /// Binds the tracker-independent read-only core to profile-local tools.
    /// The descriptor table, rather than insertion order here, owns wire order.
    fn insert_into(
        &self,
        instances: &mut ToolInstances,
        project_info: Arc<dyn Tool>,
        read: Arc<dyn Tool>,
        read_region: Arc<dyn Tool>,
        read_symbol: Arc<dyn Tool>,
        grep: Arc<dyn Tool>,
    ) {
        instances.insert(ToolFactoryKey::ProjectInfo, project_info);
        instances.insert(ToolFactoryKey::Read, read);
        instances.insert(ToolFactoryKey::Glob, self.glob.clone());
        instances.insert(ToolFactoryKey::Grep, grep);
        instances.insert(ToolFactoryKey::SymbolSearch, self.symbol.clone());
        instances.insert(ToolFactoryKey::Definition, self.definition.clone());
        instances.insert(ToolFactoryKey::References, self.references.clone());
        instances.insert(ToolFactoryKey::Hover, self.hover.clone());
        instances.insert(
            ToolFactoryKey::WorkspaceSymbol,
            self.workspace_symbol.clone(),
        );
        instances.insert(ToolFactoryKey::Git, self.git.clone());
        instances.insert(ToolFactoryKey::ReadRegion, read_region);
        instances.insert(ToolFactoryKey::ReadSymbol, read_symbol);
    }
}

/// The write/apply_patch/edit trio bound to one read tracker (the parent's or
/// a subagent's). Bash and rename stay at the call sites — their constructor
/// inputs differ per registry.
fn insert_file_mutation_tools(
    instances: &mut ToolInstances,
    project_root: &Path,
    read_tracker: &ReadTracker,
    yolo_mode: &crate::yolo::YoloMode,
    hooks: &Arc<crate::hooks::HookEngine>,
    workspace_locks: &crate::tool::WorkspaceLockContext,
    action_policy: &ActionPolicy,
) {
    instances.insert(
        ToolFactoryKey::Write,
        Arc::new(WriteTool::with_hooks_and_locks(
            project_root.to_path_buf(),
            read_tracker.clone(),
            yolo_mode.clone(),
            hooks.clone(),
            workspace_locks.clone(),
            action_policy.clone(),
        )),
    );
    instances.insert(
        ToolFactoryKey::ApplyPatch,
        Arc::new(ApplyPatchTool::with_hooks_and_locks(
            project_root.to_path_buf(),
            read_tracker.clone(),
            yolo_mode.clone(),
            hooks.clone(),
            workspace_locks.clone(),
            action_policy.clone(),
        )),
    );
    instances.insert(
        ToolFactoryKey::Edit,
        Arc::new(EditTool::with_hooks_and_locks(
            project_root.to_path_buf(),
            read_tracker.clone(),
            yolo_mode.clone(),
            hooks.clone(),
            workspace_locks.clone(),
            action_policy.clone(),
        )),
    );
}

/// The plan-canvas mutation tools, planning-registry only. `session_title` is
/// threaded in so `plan_replace_draft` can mirror the new title onto the active
/// session; it's cloned out before `SetSessionTitleTool` is built above so the
/// same storage and active-session handle are reused.
fn insert_plan_tools(
    instances: &mut ToolInstances,
    plan_store: crate::plan::SharedPlanStore,
    session_title: SessionTitleToolDeps,
) {
    instances.insert(
        ToolFactoryKey::PlanReplaceDraft,
        Arc::new(PlanReplaceDraftTool::new(
            plan_store.clone(),
            session_title.storage.clone(),
            session_title.active_session_id,
        )),
    );
    instances.insert(
        ToolFactoryKey::PlanRemoveSection,
        Arc::new(PlanRemoveSectionTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanMoveSection,
        Arc::new(PlanMoveSectionTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanPatchSection,
        Arc::new(PlanPatchSectionTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanRemovePhase,
        Arc::new(PlanRemovePhaseTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanMovePhase,
        Arc::new(PlanMovePhaseTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanInsertTask,
        Arc::new(PlanInsertTaskTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanUpdateTask,
        Arc::new(PlanUpdateTaskTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanRemoveTask,
        Arc::new(PlanRemoveTaskTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanCheckTask,
        Arc::new(PlanCheckTaskTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanUncheckTask,
        Arc::new(PlanUncheckTaskTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanAddQuestion,
        Arc::new(PlanAddQuestionTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanRemoveQuestion,
        Arc::new(PlanRemoveQuestionTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanAddFinding,
        Arc::new(PlanAddFindingTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanAssociateFinding,
        Arc::new(PlanAssociateFindingTool::new(plan_store.clone())),
    );
    instances.insert(
        ToolFactoryKey::PlanResolveFinding,
        Arc::new(PlanResolveFindingTool::new(plan_store)),
    );
}

pub(crate) fn build_tool_registries(
    deps: ToolRegistryDeps,
) -> (
    Arc<ToolRegistry>,
    Arc<ToolRegistry>,
    Arc<ToolRegistry>,
    Option<SubagentRunner>,
) {
    let ToolRegistryDeps {
        project_root,
        read_tracker,
        lsp_hub,
        project_info_runtime,
        permissions,
        domain_permissions,
        interaction,
        todo_store,
        plan_store,
        background_tasks,
        terminals,
        yolo_mode,
        sandbox,
        workspace_locks,
        session_title,
        skills,
        subagent_provider_factory,
        subagent_background_wake,
        subagents,
        custom_agents,
        builtin_subagent_settings,
        peer_bus,
        peer_wake_when_done,
        memory,
        mcp_tools,
        extensions,
        hooks,
        authorization_ledger,
        episode_store,
    } = deps;

    let read_tool = Arc::new(ReadTool::new(project_root.clone(), read_tracker.clone()));
    let read_region_tool = Arc::new(ReadRegionTool::new(
        project_root.clone(),
        read_tracker.clone(),
    ));
    let read_symbol_tool = Arc::new(ReadSymbolTool::new(
        project_root.clone(),
        read_tracker.clone(),
    ));
    let skill_tool = Arc::new(SkillTool::new(skills.clone()));
    let project_info_tool = Arc::new(ProjectInfoTool::new(
        project_root.clone(),
        project_info_runtime.clone(),
    ));
    let grep_tool = Arc::new(GrepTool::new(project_root.clone(), read_tracker.clone()));
    let core = ReadOnlyCore {
        glob: Arc::new(GlobTool::new(project_root.clone())),
        symbol: Arc::new(SymbolSearchTool::new(project_root.clone())),
        definition: Arc::new(DefinitionTool::new(lsp_hub.clone())),
        references: Arc::new(ReferencesTool::new(lsp_hub.clone())),
        hover: Arc::new(HoverTool::new(lsp_hub.clone())),
        workspace_symbol: Arc::new(WorkspaceSymbolTool::new(lsp_hub.clone())),
        git: Arc::new(GitTool::new(project_root.clone())),
    };
    let question_tool = Arc::new(QuestionTool::new(interaction.clone()));
    let action_policy = ActionPolicy::with_ledger(
        permissions.clone(),
        interaction.clone(),
        yolo_mode.clone(),
        authorization_ledger.clone(),
    );
    let terminal_tool = Arc::new(TerminalTool::new(terminals.clone(), action_policy.clone()));
    // WebFetch: one shared instance across the coding/planning/subagent
    // registries so the per-domain allowlist is consistent wherever it's granted.
    let webfetch_tool = Arc::new(crate::tool::WebFetchTool::new(
        domain_permissions,
        interaction.clone(),
        yolo_mode.clone(),
        sandbox.clone(),
        authorization_ledger.clone(),
    ));
    let websearch_tool = Arc::new(WebSearchTool::new(webfetch_tool.clone()));

    // The `agent` tool runs nested subagents. Built-ins use a read-only
    // subset and custom agents scope from the grantable template registry. Both
    // omit the `agent` tool itself, so a subagent cannot spawn subagents.
    let subagent_runner = subagent_provider_factory.map(|provider_factory| {
        let subagent_read_tracker = ReadTracker::new();
        // A separate project_info runtime, so a subagent's `project_info`
        // advertises its own read-only toolset rather than the parent's
        // (which includes write/edit/bash/agent the subagent does not have).
        let subagent_info_runtime = Arc::new(ProjectInfoRuntime::new(None));
        // Read-only tools that must bind the subagent's *own* read tracker /
        // project-info runtime (the rest are shared clones of the parent's).
        let sub_project_info: Arc<dyn Tool> = Arc::new(ProjectInfoTool::new(
            project_root.clone(),
            subagent_info_runtime.clone(),
        ));
        let sub_read: Arc<dyn Tool> = Arc::new(ReadTool::new(
            project_root.clone(),
            subagent_read_tracker.clone(),
        ));
        let sub_read_region: Arc<dyn Tool> = Arc::new(ReadRegionTool::new(
            project_root.clone(),
            subagent_read_tracker.clone(),
        ));
        let sub_read_symbol: Arc<dyn Tool> = Arc::new(ReadSymbolTool::new(
            project_root.clone(),
            subagent_read_tracker.clone(),
        ));
        let sub_grep: Arc<dyn Tool> = Arc::new(GrepTool::new(
            project_root.clone(),
            subagent_read_tracker.clone(),
        ));
        // The read-only set built-in subagents run with (and a custom agent's
        // default when it declares no `tools:`).
        let mut subagent_instances = ToolInstances::default();
        core.insert_into(
            &mut subagent_instances,
            sub_project_info.clone(),
            sub_read.clone(),
            sub_read_region.clone(),
            sub_read_symbol.clone(),
            sub_grep.clone(),
        );
        // The full grantable set a *custom* subagent's `tools:` scopes from:
        // the read-only set plus the mutating tools (write/edit/bash/rename) and
        // skill/question. These template tools supply names/schemas for scoping;
        // the runner replaces tracker-bound tools with fresh instances for each
        // nested run before execution.
        let mut full_sub_instances = subagent_instances.clone();
        insert_file_mutation_tools(
            &mut full_sub_instances,
            &project_root,
            &subagent_read_tracker,
            &yolo_mode,
            &hooks,
            &workspace_locks,
            &action_policy,
        );
        full_sub_instances.insert(
            ToolFactoryKey::Bash,
            Arc::new(BashTool::from_runtime(BashRuntimeDeps::new(
                project_root.clone(),
                permissions.clone(),
                subagent_read_tracker.clone(),
                interaction.clone(),
                background_tasks.clone(),
                terminals.clone(),
                BashExecutionPolicy::new(
                    yolo_mode.clone(),
                    sandbox.clone(),
                    hooks.clone(),
                    authorization_ledger.clone(),
                ),
            ))),
        );
        full_sub_instances.insert(ToolFactoryKey::Terminal, terminal_tool.clone());
        full_sub_instances.insert(
            ToolFactoryKey::RenameSymbol,
            Arc::new(RenameSymbolTool::with_hooks_and_locks(
                project_root.clone(),
                lsp_hub.clone(),
                subagent_read_tracker.clone(),
                yolo_mode.clone(),
                hooks.clone(),
                workspace_locks.clone(),
                action_policy.clone(),
            )),
        );
        full_sub_instances.insert(ToolFactoryKey::Skill, skill_tool.clone());
        full_sub_instances.insert(ToolFactoryKey::Question, question_tool.clone());
        full_sub_instances.insert(ToolFactoryKey::WebFetch, webfetch_tool.clone());
        let mut subagent_registry = ToolRegistry::new();
        subagent_registry.set_authorization_ledger(authorization_ledger.clone());
        subagent_instances.register_stage(
            &mut subagent_registry,
            ToolProfile::BuiltinSubagent,
            RegistrationStage::Standard,
        );
        let mut full_sub_registry = ToolRegistry::new();
        full_sub_registry.set_authorization_ledger(authorization_ledger.clone());
        full_sub_instances.register_stage(
            &mut full_sub_registry,
            ToolProfile::CustomSubagent,
            RegistrationStage::Standard,
        );
        let registry_factory = subagent_tool_registry_factory(SubagentToolRunDeps {
            project_root: project_root.clone(),
            lsp_hub: lsp_hub.clone(),
            permissions: permissions.clone(),
            interaction: interaction.clone(),
            background_tasks: background_tasks.clone(),
            terminals: terminals.clone(),
            yolo_mode: yolo_mode.clone(),
            sandbox: sandbox.clone(),
            hooks: hooks.clone(),
            workspace_locks: workspace_locks.clone(),
            authorization_ledger: authorization_ledger.clone(),
        });
        let runner = SubagentRunner::new_with_registry_factory(
            provider_factory,
            Arc::new(subagent_registry),
            Arc::new(full_sub_registry),
            registry_factory,
            subagent_info_runtime,
            subagents.clone(),
            project_root.clone(),
        );
        if subagent_background_wake {
            runner.with_background_wake()
        } else {
            runner
        }
    });
    // The same runner backs the `agent` tool and the parent's self-review pass,
    // so both share one `/subagents` registry and concurrency cap. `agent` is
    // the single delegation tool in the schema; models that reach for the
    // Claude-style `task` name land on it via the registry's dispatch alias
    // instead of a second identical schema entry.
    let agent_tool = subagent_runner.clone().map(|runner| {
        Arc::new(AgentTool::new_with_settings(
            runner,
            custom_agents.clone(),
            builtin_subagent_settings.clone(),
        ))
    });

    let bash_tool = Arc::new(BashTool::from_runtime(BashRuntimeDeps::new(
        project_root.clone(),
        permissions,
        read_tracker.clone(),
        interaction.clone(),
        background_tasks.clone(),
        terminals.clone(),
        BashExecutionPolicy::new(
            yolo_mode.clone(),
            sandbox.clone(),
            hooks.clone(),
            authorization_ledger.clone(),
        ),
    )));
    let mut coding_instances = ToolInstances::default();
    coding_instances.insert(ToolFactoryKey::ProjectInfo, project_info_tool.clone());
    coding_instances.insert(ToolFactoryKey::Read, read_tool.clone());
    insert_file_mutation_tools(
        &mut coding_instances,
        &project_root,
        &read_tracker,
        &yolo_mode,
        &hooks,
        &workspace_locks,
        &action_policy,
    );
    coding_instances.insert(ToolFactoryKey::Bash, bash_tool.clone());
    coding_instances.insert(ToolFactoryKey::Terminal, terminal_tool.clone());
    coding_instances.insert(ToolFactoryKey::Glob, core.glob.clone());
    coding_instances.insert(ToolFactoryKey::Grep, grep_tool.clone());
    coding_instances.insert(ToolFactoryKey::SymbolSearch, core.symbol.clone());
    coding_instances.insert(ToolFactoryKey::Definition, core.definition.clone());
    coding_instances.insert(ToolFactoryKey::References, core.references.clone());
    coding_instances.insert(ToolFactoryKey::Hover, core.hover.clone());
    coding_instances.insert(
        ToolFactoryKey::WorkspaceSymbol,
        core.workspace_symbol.clone(),
    );
    coding_instances.insert(
        ToolFactoryKey::RenameSymbol,
        Arc::new(RenameSymbolTool::with_hooks_and_locks(
            project_root.clone(),
            lsp_hub.clone(),
            read_tracker.clone(),
            yolo_mode.clone(),
            hooks.clone(),
            workspace_locks.clone(),
            action_policy.clone(),
        )),
    );
    coding_instances.insert(ToolFactoryKey::Git, core.git.clone());
    coding_instances.insert(ToolFactoryKey::ReadRegion, read_region_tool.clone());
    coding_instances.insert(ToolFactoryKey::ReadSymbol, read_symbol_tool.clone());
    coding_instances.insert(ToolFactoryKey::Skill, skill_tool.clone());
    coding_instances.insert(
        ToolFactoryKey::EnterPlanMode,
        Arc::new(EnterPlanModeTool::new(interaction.clone())),
    );
    if let Some(agent_tool) = &agent_tool {
        coding_instances.insert(ToolFactoryKey::Agent, agent_tool.clone());
    }
    coding_instances.insert(
        ToolFactoryKey::Diagnostics,
        Arc::new(DiagnosticsTool::with_lsp(
            project_root.clone(),
            lsp_hub.clone(),
            sandbox.clone(),
            action_policy.clone(),
        )),
    );
    let todo_write_tool = Arc::new(TodoWriteTool::new(todo_store));
    coding_instances.insert(ToolFactoryKey::TodoWrite, todo_write_tool.clone());
    let set_session_title_tool = Arc::new(SetSessionTitleTool::new(
        session_title.storage.clone(),
        session_title.active_session_id.clone(),
    ));
    coding_instances.insert(
        ToolFactoryKey::SetSessionTitle,
        set_session_title_tool.clone(),
    );
    coding_instances.insert(
        ToolFactoryKey::Tasks,
        Arc::new(TasksTool::with_subagents_and_terminals(
            background_tasks.clone(),
            subagents.clone(),
            terminals.clone(),
        )),
    );
    // Inter-agent messaging (peers P2): coding only — planning/review are
    // read-only surfaces, subagents stay off the bus, and eval passes `None`.
    if let Some(peer_bus) = &peer_bus {
        coding_instances.insert(
            ToolFactoryKey::Peers,
            Arc::new(PeersTool::new(peer_bus.clone(), peer_wake_when_done)),
        );
    }
    coding_instances.insert(ToolFactoryKey::Question, question_tool.clone());
    // Memory capture: one shared instance for the two write surfaces,
    // coding and planning. Appended last in each so the existing tool arrays
    // stay byte-stable for prompt caching. Ungated like `todowrite` (confined
    // to the two memory dirs; the rendered diff is the transparency).
    let memory_write_tool = memory
        .as_ref()
        .map(|memory| Arc::new(MemoryWriteTool::new(memory.clone())));
    if let Some(memory_write_tool) = &memory_write_tool {
        coding_instances.insert(ToolFactoryKey::MemoryWrite, memory_write_tool.clone());
    }
    // WebFetch: appended last so the existing tool array stays byte-stable
    // for prompt caching. Not in SMOL (kept tiny).
    coding_instances.insert(ToolFactoryKey::WebFetch, webfetch_tool.clone());
    // WebSearch follows WebFetch so existing prompt prefixes remain stable. It
    // shares WebFetch's network authorization and never enters SMOL.
    if websearch_tool.is_configured() {
        coding_instances.insert(ToolFactoryKey::WebSearch, websearch_tool.clone());
    }
    let recall_tool = episode_store.as_ref().map(|episode_store| {
        Arc::new(RecallTool::new(
            episode_store.clone(),
            session_title.storage.clone(),
            session_title.active_session_id.clone(),
        ))
    });
    if let Some(recall_tool) = &recall_tool {
        coding_instances.insert(ToolFactoryKey::Recall, recall_tool.clone());
    }
    let mut coding = ToolRegistry::new();
    coding.set_authorization_ledger(authorization_ledger.clone());
    coding_instances.register_stage(
        &mut coding,
        ToolProfile::Coding,
        RegistrationStage::Standard,
    );
    // MCP tools: appended after the built-in web tools, in the (server, tool) sorted
    // order `McpHub::connect_and_discover` already produced — the next
    // byte-stable seam. Not in SMOL/planning in v1. A wire-name collision
    // degrades that server visibly instead of shadowing the builtin.
    crate::mcp::register_mcp_tools(&mut coding, &mcp_tools, &extensions);
    // Episode recall: appended after every
    // existing builtin/MCP registration so the disabled path preserves the
    // existing tool arrays byte-for-byte. Deliberately absent from SMOL and
    // the subagent registries in v1.
    coding_instances.register_stage(
        &mut coding,
        ToolProfile::Coding,
        RegistrationStage::AfterExtensions,
    );
    let mut smol_instances = ToolInstances::default();
    // read/write/edit are the same instances the coding registry binds, so
    // SMOL mutations share the coding read tracker, hooks, and action policy;
    // exact-string edits replace the fragile scripted-sed rewrites small
    // models were previously steered toward.
    smol_instances.insert(ToolFactoryKey::Read, read_tool.clone());
    for key in [ToolFactoryKey::Write, ToolFactoryKey::Edit] {
        let tool = coding_instances
            .get(key)
            .expect("coding instances bind write/edit");
        smol_instances.insert(key, tool);
    }
    smol_instances.insert(
        ToolFactoryKey::Bash,
        Arc::new(
            bash_tool.with_shared_session_and_output_budget(crate::tool::BashOutputBudget::smol()),
        ),
    );
    smol_instances.insert(ToolFactoryKey::Terminal, terminal_tool.clone());
    smol_instances.insert(ToolFactoryKey::TodoWrite, todo_write_tool);
    smol_instances.insert(ToolFactoryKey::SetSessionTitle, set_session_title_tool);
    if crate::resource::skill::snapshot(&skills).has_user_skills() {
        smol_instances.insert(ToolFactoryKey::Skill, skill_tool.clone());
    }
    let mut smol = ToolRegistry::new();
    smol.set_authorization_ledger(authorization_ledger.clone());
    smol_instances.register_stage(&mut smol, ToolProfile::Smol, RegistrationStage::Standard);

    let mut planning_instances = ToolInstances::default();
    core.insert_into(
        &mut planning_instances,
        project_info_tool,
        read_tool,
        read_region_tool,
        read_symbol_tool,
        grep_tool,
    );
    planning_instances.insert(ToolFactoryKey::Question, question_tool);
    planning_instances.insert(ToolFactoryKey::Skill, skill_tool);
    if let Some(agent_tool) = agent_tool {
        planning_instances.insert(ToolFactoryKey::Agent, agent_tool);
    }
    insert_plan_tools(&mut planning_instances, plan_store, session_title);
    planning_instances.insert(ToolFactoryKey::WebFetch, webfetch_tool);
    if websearch_tool.is_configured() {
        planning_instances.insert(ToolFactoryKey::WebSearch, websearch_tool);
    }
    if let Some(memory_write_tool) = memory_write_tool {
        planning_instances.insert(ToolFactoryKey::MemoryWrite, memory_write_tool);
    }
    if let Some(recall_tool) = recall_tool {
        planning_instances.insert(ToolFactoryKey::Recall, recall_tool);
    }
    let mut planning = ToolRegistry::new();
    planning.set_authorization_ledger(authorization_ledger.clone());
    planning_instances.register_stage(
        &mut planning,
        ToolProfile::Planning,
        RegistrationStage::Standard,
    );
    planning_instances.register_stage(
        &mut planning,
        ToolProfile::Planning,
        RegistrationStage::AfterExtensions,
    );

    (
        Arc::new(coding),
        Arc::new(planning),
        Arc::new(smol),
        subagent_runner,
    )
}

#[derive(Clone)]
struct SubagentToolRunDeps {
    project_root: PathBuf,
    lsp_hub: Arc<LspHub>,
    permissions: PermissionManager,
    interaction: Arc<crate::interaction::InteractionService>,
    background_tasks: Arc<BackgroundTaskRegistry>,
    terminals: Arc<crate::terminal::TerminalRegistry>,
    yolo_mode: crate::yolo::YoloMode,
    sandbox: crate::sandbox::CommandSandbox,
    hooks: Arc<crate::hooks::HookEngine>,
    workspace_locks: crate::tool::WorkspaceLockContext,
    authorization_ledger: crate::tool::AuthorizationLedger,
}

impl SubagentToolRunDeps {
    fn registry_for_run(
        &self,
        source: &ToolRegistry,
        runtime: Arc<ProjectInfoRuntime>,
        read_tracker: ReadTracker,
    ) -> anyhow::Result<Arc<ToolRegistry>> {
        let mut isolated = ToolRegistry::new();
        isolated.set_authorization_ledger(self.authorization_ledger.clone());
        for name in source.names() {
            isolated.register(self.tool_for_run(
                name,
                source,
                runtime.clone(),
                read_tracker.clone(),
            )?);
        }
        Ok(Arc::new(isolated))
    }

    fn tool_for_run(
        &self,
        name: &str,
        source: &ToolRegistry,
        runtime: Arc<ProjectInfoRuntime>,
        read_tracker: ReadTracker,
    ) -> anyhow::Result<Arc<dyn Tool>> {
        let tool: Arc<dyn Tool> = match name {
            "project_info" => Arc::new(ProjectInfoTool::new(self.project_root.clone(), runtime)),
            "read" => Arc::new(ReadTool::new(self.project_root.clone(), read_tracker)),
            "read_region" => Arc::new(ReadRegionTool::new(self.project_root.clone(), read_tracker)),
            "read_symbol" => Arc::new(ReadSymbolTool::new(self.project_root.clone(), read_tracker)),
            "grep" => Arc::new(GrepTool::new(self.project_root.clone(), read_tracker)),
            "write" => Arc::new(WriteTool::with_hooks_and_locks(
                self.project_root.clone(),
                read_tracker,
                self.yolo_mode.clone(),
                self.hooks.clone(),
                self.workspace_locks.clone(),
                ActionPolicy::with_ledger(
                    self.permissions.clone(),
                    self.interaction.clone(),
                    self.yolo_mode.clone(),
                    self.authorization_ledger.clone(),
                ),
            )),
            "apply_patch" => Arc::new(ApplyPatchTool::with_hooks_and_locks(
                self.project_root.clone(),
                read_tracker,
                self.yolo_mode.clone(),
                self.hooks.clone(),
                self.workspace_locks.clone(),
                ActionPolicy::with_ledger(
                    self.permissions.clone(),
                    self.interaction.clone(),
                    self.yolo_mode.clone(),
                    self.authorization_ledger.clone(),
                ),
            )),
            "edit" => Arc::new(EditTool::with_hooks_and_locks(
                self.project_root.clone(),
                read_tracker,
                self.yolo_mode.clone(),
                self.hooks.clone(),
                self.workspace_locks.clone(),
                ActionPolicy::with_ledger(
                    self.permissions.clone(),
                    self.interaction.clone(),
                    self.yolo_mode.clone(),
                    self.authorization_ledger.clone(),
                ),
            )),
            "bash" => Arc::new(BashTool::from_runtime(BashRuntimeDeps::new(
                self.project_root.clone(),
                self.permissions.clone(),
                read_tracker,
                self.interaction.clone(),
                self.background_tasks.clone(),
                self.terminals.clone(),
                BashExecutionPolicy::new(
                    self.yolo_mode.clone(),
                    self.sandbox.clone(),
                    self.hooks.clone(),
                    self.authorization_ledger.clone(),
                ),
            ))),
            "rename_symbol" => Arc::new(RenameSymbolTool::with_hooks_and_locks(
                self.project_root.clone(),
                self.lsp_hub.clone(),
                read_tracker,
                self.yolo_mode.clone(),
                self.hooks.clone(),
                self.workspace_locks.clone(),
                ActionPolicy::with_ledger(
                    self.permissions.clone(),
                    self.interaction.clone(),
                    self.yolo_mode.clone(),
                    self.authorization_ledger.clone(),
                ),
            )),
            _ => source.get(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "subagent registry entry '{name}' disappeared while preparing a run"
                )
            })?,
        };
        Ok(tool)
    }
}

fn subagent_tool_registry_factory(deps: SubagentToolRunDeps) -> SubagentToolRegistryFactory {
    Arc::new(move |source, runtime, read_tracker| {
        deps.registry_for_run(source, runtime, read_tracker)
    })
}

pub(crate) struct SessionTitleToolDeps {
    pub(crate) storage: Storage,
    pub(crate) active_session_id: SharedActiveSessionId,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::{SessionTitleToolDeps, ToolRegistryDeps, build_tool_registries};
    use crate::background::BackgroundTaskRegistry;
    use crate::permissions::PermissionManager;
    use crate::plan::PlanDoc;
    use crate::todo::TodoStore;
    use crate::tool::profile::{ToolProfile, descriptor_for};
    use crate::tool::{ReadTracker, ToolRegistry};

    fn assert_profile_contract(profile: ToolProfile, registry: &ToolRegistry) {
        for name in registry.names() {
            let descriptor = descriptor_for(profile, name)
                .unwrap_or_else(|| panic!("missing {profile:?} descriptor for {name}"));
            let tool = registry.get(name).expect("registered tool must resolve");
            assert_eq!(tool.effect_policy(), descriptor.effect, "effect for {name}");
            assert_eq!(
                tool.parallel_policy(),
                descriptor.parallel,
                "parallel policy for {name}"
            );
        }
    }

    #[tokio::test]
    async fn planning_registry_omits_mutating_tools() {
        let (interaction, _rx) = crate::interaction::InteractionService::new();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let project_root = temp_dir.path().to_path_buf();
        let storage = crate::storage::Storage::open_at(temp_dir.path().join("bonsai.db"))
            .await
            .unwrap();
        let (coding, planning, smol, _runner) = build_tool_registries(ToolRegistryDeps {
            project_root: project_root.clone(),
            read_tracker: ReadTracker::new(),
            lsp_hub: Arc::new(crate::lsp::LspHub::new(project_root)),
            project_info_runtime: Arc::new(crate::tool::ProjectInfoRuntime::default()),
            permissions: PermissionManager::memory_only(),
            domain_permissions: PermissionManager::memory_only_domains(),
            interaction: Arc::new(interaction),
            todo_store: Arc::new(Mutex::new(TodoStore::new())),
            plan_store: Arc::new(Mutex::new(PlanDoc::default())),
            background_tasks: Arc::new(BackgroundTaskRegistry::new()),
            terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
            yolo_mode: crate::yolo::YoloMode::new(),
            sandbox: crate::sandbox::CommandSandbox::disabled(),
            workspace_locks: crate::tool::WorkspaceLockContext::disabled(
                temp_dir.path().to_path_buf(),
            ),
            session_title: SessionTitleToolDeps {
                storage,
                active_session_id: Arc::new(Mutex::new(None)),
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
            peer_bus: None,
            memory: None,
            mcp_tools: Vec::new(),
            extensions: Arc::new(crate::extension::status::ExtensionRegistry::new()),
            hooks: Arc::new(crate::hooks::HookEngine::disabled()),
            authorization_ledger: crate::tool::AuthorizationLedger::disabled(),
            episode_store: None,
        });

        let websearch_configured = coding.get("websearch").is_some();
        let mut expected_coding = vec![
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
            "read_region",
            "read_symbol",
            "write",
            "apply_patch",
            "edit",
            "bash",
            "terminal",
            "rename_symbol",
            "skill",
            "diagnostics",
            "todowrite",
            "set_session_title",
            "tasks",
            "question",
            "webfetch",
            "enter_plan_mode",
        ];
        if websearch_configured {
            expected_coding.insert(expected_coding.len() - 1, "websearch");
        }
        assert_eq!(coding.names().collect::<Vec<_>>(), expected_coding);

        let mut expected_planning = vec![
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
            "read_region",
            "read_symbol",
            "question",
            "skill",
            "plan_replace_draft",
            "plan_remove_section",
            "plan_move_section",
            "plan_patch_section",
            "plan_remove_phase",
            "plan_move_phase",
            "plan_insert_task",
            "plan_update_task",
            "plan_remove_task",
            "plan_check_task",
            "plan_uncheck_task",
            "plan_add_question",
            "plan_remove_question",
            "plan_add_finding",
            "plan_associate_finding",
            "plan_resolve_finding",
            "webfetch",
        ];
        if websearch_configured {
            expected_planning.push("websearch");
        }
        assert_eq!(planning.names().collect::<Vec<_>>(), expected_planning);
        assert_eq!(
            smol.names().collect::<Vec<_>>(),
            [
                "read",
                "write",
                "edit",
                "bash",
                "terminal",
                "todowrite",
                "set_session_title"
            ]
        );
        assert_profile_contract(ToolProfile::Coding, &coding);
        assert_profile_contract(ToolProfile::Planning, &planning);
        assert_profile_contract(ToolProfile::Smol, &smol);

        for tool in [
            "bash",
            "terminal",
            "write",
            "edit",
            "todowrite",
            "set_session_title",
            "tasks",
        ] {
            assert!(
                planning.get(tool).is_none(),
                "planning registry must not include {tool}"
            );
            assert!(
                coding.get(tool).is_some(),
                "coding registry must include {tool}"
            );
        }

        for tool in [
            "read",
            "write",
            "edit",
            "bash",
            "terminal",
            "todowrite",
            "set_session_title",
        ] {
            assert!(
                smol.get(tool).is_some(),
                "SMOL registry must include {tool}"
            );
        }

        for tool in ["project_info", "apply_patch", "glob", "grep", "agent"] {
            assert!(smol.get(tool).is_none(), "SMOL registry must omit {tool}");
        }

        for tool in [
            "project_info",
            "read",
            "read_region",
            "read_symbol",
            "glob",
            "grep",
            "symbol_search",
            "definition",
            "references",
            "hover",
            "workspace_symbol",
            "git",
            "question",
            "skill",
            "webfetch",
        ] {
            assert!(
                planning.get(tool).is_some(),
                "planning registry must include {tool}"
            );
            assert!(
                coding.get(tool).is_some(),
                "coding registry must include {tool}"
            );
        }

        for tool in ["webfetch", "websearch"] {
            assert!(smol.get(tool).is_none(), "SMOL registry must omit {tool}");
        }
        assert_eq!(
            planning.get("websearch").is_some(),
            websearch_configured,
            "coding and planning must agree on websearch availability"
        );

        let tool = "diagnostics";
        assert!(
            coding.get(tool).is_some(),
            "coding registry must include {tool}"
        );
        assert!(
            planning.get(tool).is_none(),
            "planning registry must not include {tool}"
        );

        let tool = "rename_symbol";
        assert!(
            coding.get(tool).is_some(),
            "coding registry must include {tool}"
        );
        assert!(
            planning.get(tool).is_none(),
            "planning registry must not include {tool}"
        );

        for tool in [
            "plan_replace_draft",
            "plan_remove_section",
            "plan_move_section",
            "plan_patch_section",
            "plan_remove_phase",
            "plan_move_phase",
            "plan_insert_task",
            "plan_update_task",
            "plan_remove_task",
            "plan_check_task",
            "plan_uncheck_task",
            "plan_add_question",
            "plan_remove_question",
            "plan_add_finding",
            "plan_associate_finding",
            "plan_resolve_finding",
        ] {
            assert!(
                planning.get(tool).is_some(),
                "planning registry must include {tool}"
            );
            assert!(
                coding.get(tool).is_none(),
                "coding registry must not include {tool}"
            );
        }
    }

    #[tokio::test]
    async fn memory_write_registers_at_coding_and_planning_tails() {
        let (interaction, _rx) = crate::interaction::InteractionService::new();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let home_dir = tempfile::TempDir::new().unwrap();
        let project_root = temp_dir.path().to_path_buf();
        let storage = crate::storage::Storage::open_at(temp_dir.path().join("bonsai.db"))
            .await
            .unwrap();
        let memory = Arc::new(crate::memory::MemoryService::load(
            home_dir.path(),
            &project_root,
            storage.clone(),
            0,
        ));
        let (coding, planning, smol, _runner) = build_tool_registries(ToolRegistryDeps {
            project_root: project_root.clone(),
            read_tracker: ReadTracker::new(),
            lsp_hub: Arc::new(crate::lsp::LspHub::new(project_root)),
            project_info_runtime: Arc::new(crate::tool::ProjectInfoRuntime::default()),
            permissions: PermissionManager::memory_only(),
            domain_permissions: PermissionManager::memory_only_domains(),
            interaction: Arc::new(interaction),
            todo_store: Arc::new(Mutex::new(TodoStore::new())),
            plan_store: Arc::new(Mutex::new(PlanDoc::default())),
            background_tasks: Arc::new(BackgroundTaskRegistry::new()),
            terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
            yolo_mode: crate::yolo::YoloMode::new(),
            sandbox: crate::sandbox::CommandSandbox::disabled(),
            workspace_locks: crate::tool::WorkspaceLockContext::disabled(
                temp_dir.path().to_path_buf(),
            ),
            session_title: SessionTitleToolDeps {
                storage,
                active_session_id: Arc::new(Mutex::new(None)),
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
            peer_bus: None,
            memory: Some(memory),
            mcp_tools: Vec::new(),
            extensions: Arc::new(crate::extension::status::ExtensionRegistry::new()),
            hooks: Arc::new(crate::hooks::HookEngine::disabled()),
            authorization_ledger: crate::tool::AuthorizationLedger::disabled(),
            episode_store: None,
        });

        // Both write surfaces carry the tool; SMOL stays memory-free.
        assert!(coding.get("memory_write").is_some());
        assert!(planning.get("memory_write").is_some());
        assert!(smol.get("memory_write").is_none());
        // Appended at planning's tail: the byte-stable seam for prompt caching.
        assert_eq!(planning.names().last(), Some("memory_write"));
        // No episode store → no recall anywhere, and the tail is unchanged.
        assert!(coding.get("recall").is_none());
        assert!(planning.get("recall").is_none());
    }

    #[tokio::test]
    async fn recall_registers_at_registry_tails_only_with_an_episode_store() {
        let (interaction, _rx) = crate::interaction::InteractionService::new();
        let temp_dir = tempfile::TempDir::new().unwrap();
        let project_root = temp_dir.path().to_path_buf();
        let storage = crate::storage::Storage::open_at(temp_dir.path().join("bonsai.db"))
            .await
            .unwrap();
        let (coding, planning, smol, _runner) = build_tool_registries(ToolRegistryDeps {
            project_root: project_root.clone(),
            read_tracker: ReadTracker::new(),
            lsp_hub: Arc::new(crate::lsp::LspHub::new(project_root)),
            project_info_runtime: Arc::new(crate::tool::ProjectInfoRuntime::default()),
            permissions: PermissionManager::memory_only(),
            domain_permissions: PermissionManager::memory_only_domains(),
            interaction: Arc::new(interaction),
            todo_store: Arc::new(Mutex::new(TodoStore::new())),
            plan_store: Arc::new(Mutex::new(PlanDoc::default())),
            background_tasks: Arc::new(BackgroundTaskRegistry::new()),
            terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
            yolo_mode: crate::yolo::YoloMode::new(),
            sandbox: crate::sandbox::CommandSandbox::disabled(),
            workspace_locks: crate::tool::WorkspaceLockContext::disabled(
                temp_dir.path().to_path_buf(),
            ),
            session_title: SessionTitleToolDeps {
                storage,
                active_session_id: Arc::new(Mutex::new(None)),
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
            peer_bus: None,
            memory: None,
            mcp_tools: Vec::new(),
            extensions: Arc::new(crate::extension::status::ExtensionRegistry::new()),
            hooks: Arc::new(crate::hooks::HookEngine::disabled()),
            authorization_ledger: crate::tool::AuthorizationLedger::disabled(),
            episode_store: Some(crate::episode::SharedEpisodeStore::default()),
        });

        // Recall rides both conversational registries at their byte-stable
        // tails; SMOL deliberately stays without it in v1.
        assert_eq!(coding.names().last(), Some("recall"));
        assert_eq!(planning.names().last(), Some("recall"));
        assert!(smol.get("recall").is_none());
    }

    #[tokio::test]
    async fn agent_tool_registered_only_when_subagent_factory_present() {
        async fn registries(
            factory: Option<crate::tool::SubagentProviderFactory>,
        ) -> (
            Arc<crate::tool::ToolRegistry>,
            Arc<crate::tool::ToolRegistry>,
            Arc<crate::tool::ToolRegistry>,
            Option<crate::tool::SubagentRunner>,
        ) {
            let (interaction, _rx) = crate::interaction::InteractionService::new();
            let temp_dir = tempfile::TempDir::new().unwrap();
            let project_root = temp_dir.path().to_path_buf();
            let storage = crate::storage::Storage::open_at(temp_dir.path().join("bonsai.db"))
                .await
                .unwrap();
            build_tool_registries(ToolRegistryDeps {
                project_root: project_root.clone(),
                read_tracker: ReadTracker::new(),
                lsp_hub: Arc::new(crate::lsp::LspHub::new(project_root)),
                project_info_runtime: Arc::new(crate::tool::ProjectInfoRuntime::default()),
                permissions: PermissionManager::memory_only(),
                domain_permissions: PermissionManager::memory_only_domains(),
                interaction: Arc::new(interaction),
                todo_store: Arc::new(Mutex::new(TodoStore::new())),
                plan_store: Arc::new(Mutex::new(PlanDoc::default())),
                background_tasks: Arc::new(BackgroundTaskRegistry::new()),
                terminals: Arc::new(crate::terminal::TerminalRegistry::new()),
                yolo_mode: crate::yolo::YoloMode::new(),
                sandbox: crate::sandbox::CommandSandbox::disabled(),
                workspace_locks: crate::tool::WorkspaceLockContext::disabled(
                    temp_dir.path().to_path_buf(),
                ),
                session_title: SessionTitleToolDeps {
                    storage,
                    active_session_id: Arc::new(Mutex::new(None)),
                },
                skills: crate::resource::skill::shared_registry(
                    crate::resource::skill::SkillRegistry::empty(),
                ),
                subagent_provider_factory: factory,
                subagent_background_wake: false,
                peer_wake_when_done: false,
                subagents: Arc::new(crate::subagent::SubagentRegistry::new()),
                custom_agents: crate::resource::agent::shared_registry(
                    crate::resource::agent::AgentRegistry::empty(),
                ),
                builtin_subagent_settings: crate::subagent::SharedBuiltinSubagentSettings::default(
                ),
                peer_bus: None,
                memory: None,
                mcp_tools: Vec::new(),
                extensions: Arc::new(crate::extension::status::ExtensionRegistry::new()),
                hooks: Arc::new(crate::hooks::HookEngine::disabled()),
                authorization_ledger: crate::tool::AuthorizationLedger::disabled(),
                episode_store: None,
            })
        }

        // The factory is captured but never invoked during registration, so an
        // unreachable closure is a safe stand-in.
        let factory: crate::tool::SubagentProviderFactory = Arc::new(
            |_agent: String, _chain: crate::subagent::SubagentModelChain| {
                Box::pin(async {
                    unreachable!("provider factory must not run during registration")
                })
            },
        );
        let (coding, planning, _smol, runner) = registries(Some(factory)).await;
        assert!(coding.get("agent").is_some(), "coding must include agent");
        assert!(
            planning.get("agent").is_some(),
            "planning must include agent"
        );
        let runner = runner.expect("subagent factory must build a runner");
        assert_eq!(
            runner.read_only_registry().names().collect::<Vec<_>>(),
            [
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
                "read_region",
                "read_symbol",
            ]
        );
        assert_eq!(
            runner.full_registry().names().collect::<Vec<_>>(),
            [
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
                "read_region",
                "read_symbol",
                "write",
                "apply_patch",
                "edit",
                "bash",
                "terminal",
                "rename_symbol",
                "skill",
                "question",
                "webfetch",
            ]
        );
        assert_profile_contract(ToolProfile::BuiltinSubagent, runner.read_only_registry());
        assert_profile_contract(ToolProfile::CustomSubagent, runner.full_registry());

        let (coding, planning, _smol, _) = registries(None).await;
        assert!(
            coding.get("agent").is_none(),
            "coding omits agent w/o factory"
        );
        assert!(
            planning.get("agent").is_none(),
            "planning omits agent w/o factory"
        );
    }
}
