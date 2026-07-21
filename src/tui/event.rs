/// Top-level workspace views. Agent is the default conversation view; Plan
/// splits into chat (left) and the plan canvas (right).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Agent,
    Plan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Transcript,
    /// The visible TODO section in the agent-view sidebar.
    Todo,
    Input,
    /// The plan canvas. Only meaningful in `View::Plan`; the run loop
    /// falls back to `Transcript` when the user toggles to the agent view.
    Plan,
    Modal,
}

use std::sync::Arc;
use std::time::Instant;

use crate::diff::FileDiff;
use crate::interaction::QuestionOption;
use crate::output::ToolCallStart;
use crate::provider::ReasoningSelection;
use crate::storage::SavedPlanId;
use crate::tui::pickers::{ModelOption, ProviderOption};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalKind {
    /// Guided first-run setup. Complex provider/model selection delegates to
    /// the normal pickers while `AppState::first_run_step` retains the return
    /// point.
    Onboarding {
        step: crate::onboarding::FirstRunStep,
        cursor: usize,
    },
    Help,
    CommandHelp,
    ToolDetail {
        tool_id: String,
    },
    BlockDetail {
        item_index: usize,
    },
    /// Full detail of a single review finding, with in-modal navigation across
    /// the plan's findings (most-severe first) and a jump to its source tool
    /// card when the evidence id still resolves in the current transcript.
    PlanFindingDetail {
        index: usize,
    },
    DiffPreview {
        tool_id: String,
    },
    ApiKeyPrompt {
        provider_id: String,
        initial_form: Option<crate::tui::app::ProviderAuthForm>,
    },
    /// `/authorize` with no argument — pick a provider to authorize from a
    /// type-to-filter list. `query` narrows `providers` (matched on id + label);
    /// `cursor` indexes the *filtered* view, so every reader resolves the
    /// selection through [`crate::tui::pickers::filter_authorize_providers`].
    AuthorizeProviderPicker {
        providers: Vec<ProviderOption>,
        query: String,
        cursor: usize,
    },
    ModelPicker {
        entries: Vec<ModelOption>,
    },
    SessionPicker {
        sessions: Vec<crate::storage::SessionSummary>,
        cursor: usize,
    },
    PlanPicker {
        plans: Vec<crate::storage::SavedPlanSummary>,
        query: String,
        cursor: usize,
    },
    PlanOpenChoice {
        plan: crate::storage::SavedPlanSummary,
        cursor: usize,
    },
    StartPlanChoice {
        cursor: usize,
    },
    PlanDeleteConfirm {
        plan: crate::storage::SavedPlanSummary,
    },
    /// `/discard` confirm — throwing away the canvas plan would also delete its
    /// saved library record, which is irreversible. Only opened when the canvas
    /// plan is linked to a saved record; an unsaved canvas is cleared without a
    /// prompt.
    PlanDiscardConfirm {
        saved_plan_id: crate::storage::SavedPlanId,
        title: String,
    },
    /// `/review` — choose which set of pending changes to review.
    ReviewScopePicker {
        cursor: usize,
    },
    LocalModelWizard {
        state: Box<crate::tui::local_model_wizard::LocalModelWizardState>,
    },
    /// `/agents` — an interactive list of built-in + custom subagents. `a` add,
    /// `e` edit, `d` delete (custom only).
    AgentBrowser {
        rows: Vec<crate::tui::agent_composer::AgentBrowserRow>,
        cursor: usize,
    },
    /// `/providers` — manage catalog providers: `a` add (wizard), `e` edit,
    /// `d` remove custom entries / disable built-ins (re-enable disabled ones).
    ///
    /// `/` starts an inline search that narrows `rows` by `filter` (matched on
    /// display name + id). `searching` distinguishes "typing the filter" (bare
    /// letters edit `filter`) from "filter applied, shortcuts live" (bare
    /// `a`/`e`/`d` act on the filtered selection). `cursor` always indexes the
    /// *filtered* view — resolve via
    /// [`crate::tui::provider_manager::provider_manager_filtered`].
    ProviderManager {
        rows: Vec<crate::tui::provider_manager::ProviderManagerRow>,
        filter: String,
        searching: bool,
        cursor: usize,
    },
    /// Read-only inspector for a single provider (opened from the manager with
    /// Enter): connection info, environment variables, and available models.
    /// Esc returns to the manager list carried in `return_rows`/`return_cursor`.
    ProviderDetail {
        detail: Box<crate::tui::provider_manager::ProviderDetail>,
    },
    /// Confirm removing a custom provider or disabling a built-in before
    /// mutating catalog files.
    ProviderRemoveConfirm {
        connection_id: String,
        display_name: String,
        /// `true` → disable the built-in (credentials kept for re-enable);
        /// `false` → delete the custom provider's files and credentials.
        disable_builtin: bool,
    },
    /// `/skills` — an interactive manager: list every skill with its status and
    /// loaded state, review the selected one's body in the detail pane, load it
    /// into context (`l`), and enable/disable built-ins (`Space`).
    SkillManager {
        rows: Vec<crate::tui::skill_manager::SkillRow>,
        cursor: usize,
    },
    /// `/memory` — a memory manager with exact row selection, toggles, delete,
    /// and create wizard.
    MemoryManager {
        rows: Vec<crate::memory::entry::MemoryEntry>,
        cursor: usize,
    },
    /// The create/edit memory wizard opened from the manager (`state.editing`
    /// carries the original identity in edit mode).
    MemoryAddWizard {
        state: Box<crate::tui::memory_manager::MemoryAddWizardState>,
    },
    /// `/settings` — a combined settings screen: model + theme (Enter opens the
    /// relevant picker) plus cyclable autonomy, self-review, SMOL, serenity,
    /// and sandbox
    /// axes (Left/Right/Space), each applied live. Rows are seeded at open.
    Settings {
        rows: Vec<SettingsRow>,
        cursor: usize,
    },
    /// The create/edit agent wizard (Details → Tools → Prompt → Review).
    AgentComposer {
        state: Box<crate::tui::agent_composer::AgentComposerState>,
    },
    /// Confirm deletion of a custom agent file before removing it.
    AgentDeleteConfirm {
        name: String,
        path: std::path::PathBuf,
    },
    /// `/mcp` — review detected MCP servers and their discovered methods.
    McpServers {
        rows: Vec<crate::tui::mcp::McpServerRow>,
        cursor: usize,
    },
    SandboxStatus {
        cursor: usize,
    },
    /// `/doctor` — a read-only release-diagnostics summary: the aggregate
    /// pass/warn/fail counts as a header, one selectable row per check, and the
    /// highlighted check's remediation in the footer.
    Doctor {
        report: Box<crate::doctor::DoctorReport>,
        cursor: usize,
    },
    /// `/theme` — browse built-in themes with live preview. Cancelling restores
    /// `original_theme`; submitting persists the selected theme.
    ThemePicker {
        cursor: usize,
        original_theme: String,
    },
    /// `/mode` — toggle runtime posture axes (autonomy, sandbox confinement,
    /// sandbox network). Renders as a static hierarchical picker: each axis has
    /// a header row and one or more value rows. The rows vector carries the
    /// current state so reopening the picker reflects the latest values.
    ModePicker {
        rows: Vec<ModeRow>,
        cursor: usize,
    },
    BusyCommand {
        input: String,
        rows: Vec<BusyCommandRow>,
        cursor: usize,
    },
    TaskList {
        tasks: Vec<crate::background::BackgroundTaskSnapshot>,
        cursor: usize,
    },
    /// `/peers` — live view of the other bonsai sessions in this project root,
    /// their claims, and recently changed files (peers P5). View-only.
    PeerList {
        peers: Vec<crate::peer::PeerOverview>,
        cursor: usize,
    },
    /// `/subagents` — live view of nested subagent runs.
    SubtaskList {
        subtasks: Vec<crate::subagent::SubagentSnapshot>,
        cursor: usize,
        pane: SubtaskListPane,
    },
    PermissionPrompt {
        request_id: u64,
        command: String,
        origin: Option<String>,
    },
    /// Approval to run a single command OUTSIDE the OS sandbox (the enforcement
    /// floor — never auto-approved). Offers once / session / deny.
    SandboxEscalationPrompt {
        request_id: u64,
        command: String,
        origin: Option<String>,
    },
    /// Approval to access a web domain (WebSearch/WebFetch). Offers once /
    /// session / project / deny, and warns when the host was reached via a
    /// cross-domain redirect.
    WebDomainPrompt {
        request_id: u64,
        url: String,
        host: String,
        redirected_from: Option<String>,
        origin: Option<String>,
    },
    /// Approval to run one namespaced extension tool call. Offers
    /// once / session / project / deny, like [`Self::WebDomainPrompt`].
    ExtensionToolPrompt {
        request_id: u64,
        id: String,
        server: String,
        capabilities: Vec<String>,
        args_preview: String,
    },
    /// One-time trust approval for a project-config hook's shell/http action
    ///. Offers once / session / project / deny, like
    /// [`Self::ExtensionToolPrompt`].
    HookTrustPrompt {
        request_id: u64,
        name: String,
        event: String,
        action_kind: String,
        action_preview: String,
    },
    QuestionPrompt {
        request_id: u64,
        prompt: String,
        header: Option<String>,
        options: Vec<QuestionOption>,
        multiple: bool,
        origin: Option<String>,
        cursor: usize,
        selected: Vec<bool>,
    },
    /// `/ctx` — a visual breakdown of the current context window.
    Context(Box<crate::agent::ContextReport>),
    /// `/episodes` — task-scoped context lifecycle, backed by the cached context snapshot.
    Episodes {
        report: Box<crate::agent::ContextReport>,
        cursor: usize,
    },
    /// `/perf` and `/cost` — a readable report overlay instead of a transcript
    /// status block.
    PerfReport {
        title: String,
        lines: Vec<String>,
    },
    /// `/usage` — global (all-projects) usage analytics dashboard. Data is
    /// loaded once at open; rendering is pure over the boxed aggregates.
    UsageDashboard {
        dashboard: Box<crate::storage::UsageDashboard>,
        tab: UsageTab,
    },
    /// `/refresh` — live per-source refresh status modal. Rows start `Pending`
    /// (yellow dot) and flip to green (Ok, with added/removed model counts) or
    /// red (Failed) as each source settles. Stays open after completion so the
    /// diffs are readable; Esc/Enter dismisses.
    Refresh {
        sources: Vec<crate::commands::RefreshSourceState>,
        cursor: usize,
        /// Generation tag: stale `RefreshSourceCompleted` events (from a
        /// superseded refresh) are dropped when the id doesn't match.
        generation: u64,
    },
}

/// The five approval-prompt modals that share the AllowOnce / AllowSession /
/// AllowProject / Deny decision shape. Keys, mouse dismissal, and the runtime
/// responder are generic over this; only the outcome mapping and the sandbox
/// audit banner are family-specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptFamily {
    Permission,
    SandboxEscalation,
    WebDomain,
    ExtensionTool,
    HookTrust,
}

impl PromptFamily {
    /// The prompt family a modal belongs to, if it is one of the five
    /// approval prompts. `None` for every other modal kind.
    pub fn of_modal(kind: &ModalKind) -> Option<Self> {
        Some(match kind {
            ModalKind::PermissionPrompt { .. } => Self::Permission,
            ModalKind::SandboxEscalationPrompt { .. } => Self::SandboxEscalation,
            ModalKind::WebDomainPrompt { .. } => Self::WebDomain,
            ModalKind::ExtensionToolPrompt { .. } => Self::ExtensionTool,
            ModalKind::HookTrustPrompt { .. } => Self::HookTrust,
            _ => return None,
        })
    }
}

/// The decision tiers an approval prompt offers. Sandbox escalation has no
/// project tier (escapes are never persisted) — its keymap never emits
/// `AllowProject`, and the responder maps a stray one to the fail-safe deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDecision {
    AllowOnce,
    AllowSession,
    AllowProject,
    Deny,
}

/// Tabs of the `/usage` dashboard, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageTab {
    Activity,
    Models,
    Cost,
    Sessions,
    Tools,
    Cache,
}

impl UsageTab {
    pub const ALL: [Self; 6] = [
        Self::Activity,
        Self::Models,
        Self::Cost,
        Self::Sessions,
        Self::Tools,
        Self::Cache,
    ];

    pub const fn title(self) -> &'static str {
        match self {
            Self::Activity => "Activity",
            Self::Models => "Models",
            Self::Cost => "Cost",
            Self::Sessions => "Sessions",
            Self::Tools => "Tools",
            Self::Cache => "Cache",
        }
    }

    /// The tab `steps` positions away, wrapping in both directions.
    pub fn cycled(self, steps: i8) -> Self {
        let len = Self::ALL.len() as i8;
        let index = Self::ALL
            .iter()
            .position(|tab| *tab == self)
            .unwrap_or_default() as i8;
        Self::ALL[(index + steps).rem_euclid(len) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtaskListPane {
    List,
    Detail,
}

impl SubtaskListPane {
    pub const fn toggled(self) -> Self {
        match self {
            Self::List => Self::Detail,
            Self::Detail => Self::List,
        }
    }
}

impl ModalKind {
    pub(crate) fn perf_report(title: impl Into<String>, text: &str) -> Self {
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        if lines.is_empty() {
            lines.push("No performance data is available yet.".to_string());
        }
        Self::PerfReport {
            title: title.into(),
            lines,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusyCommandRow {
    pub action: BusyCommandAction,
    pub label: &'static str,
    pub description: &'static str,
}

impl BusyCommandRow {
    pub fn queue() -> Self {
        Self {
            action: BusyCommandAction::QueueDeferred,
            label: "Queue for next run",
            description: "Apply after the current run finishes",
        }
    }

    pub fn open_read_only() -> Self {
        Self {
            action: BusyCommandAction::OpenReadOnly,
            label: "Open read-only view",
            description: "Inspect without touching the active run",
        }
    }

    pub fn cancel_current_run() -> Self {
        Self {
            action: BusyCommandAction::CancelCurrentRun,
            label: "Cancel current run",
            description: "Stop the active run, then decide again",
        }
    }

    pub fn dismiss() -> Self {
        Self {
            action: BusyCommandAction::Dismiss,
            label: "Dismiss",
            description: "Leave the current run untouched",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyCommandAction {
    QueueDeferred,
    OpenReadOnly,
    CancelCurrentRun,
    Dismiss,
}

/// Static axis definition backing the `/mode` picker. Each axis declares a
/// header label and one or more (key, axis_id, values) tuples that map onto
/// `ModeRow::Header` / `ModeRow::Value` rows. New axes are added here (and a
/// handler in `runtime_actions`); the picker widget, keymap, and reducer
/// don't need other changes.
#[derive(Debug)]
pub(crate) struct ModeAxisDef {
    pub header: &'static str,
    pub values: &'static [ModeValueDef],
}

/// One cyclable value row inside a `ModeAxisDef`.
#[derive(Debug)]
pub(crate) struct ModeValueDef {
    pub key: &'static str,
    pub axis: ModeAxisId,
    pub values: &'static [&'static str],
}

pub(crate) const MODE_AXES: &[ModeAxisDef] = &[
    ModeAxisDef {
        header: "Autonomy",
        values: &[ModeValueDef {
            key: "level",
            axis: ModeAxisId::Autonomy,
            values: &["ask", "conservative", "balanced", "auto-accept", "yolo"],
        }],
    },
    ModeAxisDef {
        header: "Self-review",
        values: &[ModeValueDef {
            key: "mode",
            axis: ModeAxisId::SelfReview,
            values: &["auto", "off", "ask", "on"],
        }],
    },
    ModeAxisDef {
        header: "Sandbox",
        values: &[
            ModeValueDef {
                key: "confinement",
                axis: ModeAxisId::SandboxConfinement,
                values: &["off", "on"],
            },
            ModeValueDef {
                key: "network",
                axis: ModeAxisId::SandboxNetwork,
                values: &["allow", "deny"],
            },
        ],
    },
];

/// Identifies which posture axis a `ModeRow::Value` writes through when the
/// user cycles it. The runtime-action dispatch uses this to pick the matching
/// holder (autonomy → `YoloMode`, sandbox → `CommandSandbox`) without coupling
/// the picker to any concrete setter signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeAxisId {
    Autonomy,
    SelfReview,
    SandboxConfinement,
    SandboxNetwork,
}

/// A single row in the `/mode` picker. Headers are axis labels and never
/// cycle; values carry the cycle order and the user's current position in it.
/// Adding a new axis means adding a `ModeAxisId` variant and the
/// corresponding entries in `MODE_AXES` — the picker doesn't need any other
/// changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeRow {
    Header(&'static str),
    Value {
        axis: ModeAxisId,
        /// Display label for the value row (e.g. `level`, `confinement`).
        key: &'static str,
        /// Cycle order. `current` indexes into this array; `cycled()` advances
        /// it modulo `values.len()`.
        values: &'static [&'static str],
        current: usize,
        /// Optional inline note shown after the value (e.g. `no backend`,
        /// `unenforced`). Not cyclable.
        note: Option<&'static str>,
    },
}

impl ModeRow {
    /// Whether this row is a selectable, cyclable value rather than an axis
    /// header. Navigation skips headers so the cursor only ever rests on a
    /// value the user can actually change.
    pub fn is_value(&self) -> bool {
        matches!(self, Self::Value { .. })
    }

    /// Advance `current` modulo `values.len()` for a `Value` row; `Header`
    /// rows are unchanged. Returns a fresh row with the new index so the
    /// picker can be reseeded on close/reopen.
    pub fn cycled(self, delta: i16) -> Self {
        match self {
            Self::Header(_) => self,
            Self::Value {
                axis,
                key,
                values,
                current,
                note,
            } => {
                let len = values.len() as i32;
                if len == 0 {
                    return self;
                }
                let delta = i32::from(delta);
                let next = (current as i32 + delta).rem_euclid(len) as usize;
                Self::Value {
                    axis,
                    key,
                    values,
                    current: next,
                    note,
                }
            }
        }
    }
}

/// Identifies a setting shown in the `/settings` screen, so the runtime
/// dispatch can apply a cycle (or open a picker) through the right holder
/// without the picker knowing any setter signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingId {
    Autonomy,
    SelfReview,
    Smol,
    Serenity,
    /// Opt-in redacted lifecycle JSONL for `/bug` support bundles.
    SupportLog,
    BudgetTurns,
    BudgetRunTime,
    BudgetGenerationTime,
    BudgetOutput,
    BudgetToolTime,
    BudgetSessionTurns,
    BudgetSessionOutput,
    BudgetSessionTime,
    BudgetSessionCost,
    SandboxConfinement,
    SandboxNetwork,
    /// Default storage for newly entered provider credentials.
    CredentialStorage,
    /// Opens the model picker on activate.
    Model,
    /// Opens the theme picker on activate.
    Theme,
}

/// A row in the `/settings` screen. `Header` labels a section and never rests
/// under the cursor; `Choice` cycles a small enum in place (applied live);
/// `Action` opens a dedicated picker on Enter (model, theme). Rows are seeded
/// fresh at open, so values track the live holders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsRow {
    Header(&'static str),
    Choice {
        id: SettingId,
        key: &'static str,
        values: &'static [&'static str],
        current: usize,
        /// Inline caveat shown after the value (e.g. `no backend`); not cycled.
        note: Option<String>,
    },
    Action {
        id: SettingId,
        key: &'static str,
        /// Current value shown to the right (e.g. the model name / theme).
        value: String,
    },
}

impl SettingsRow {
    /// Whether the cursor may rest here (everything but a section header).
    pub fn is_selectable(&self) -> bool {
        !matches!(self, Self::Header(_))
    }

    /// The `SettingId` for a selectable row, or `None` for a header.
    pub fn id(&self) -> Option<SettingId> {
        match self {
            Self::Header(_) => None,
            Self::Choice { id, .. } | Self::Action { id, .. } => Some(*id),
        }
    }

    /// Advance a `Choice` row's `current` modulo its values; other rows are
    /// unchanged. Returns a fresh row so the picker can update in place.
    pub fn cycled(self, delta: i16) -> Self {
        match self {
            Self::Choice {
                id,
                key,
                values,
                current,
                note,
            } => {
                let len = values.len() as i32;
                if len == 0 {
                    return Self::Choice {
                        id,
                        key,
                        values,
                        current,
                        note,
                    };
                }
                let next = (current as i32 + i32::from(delta)).rem_euclid(len) as usize;
                Self::Choice {
                    id,
                    key,
                    values,
                    current: next,
                    note,
                }
            }
            other => other,
        }
    }

    /// The cycled value label for a `Choice`, if this is one.
    pub fn choice_label(&self) -> Option<&'static str> {
        match self {
            Self::Choice {
                values, current, ..
            } => values.get(*current).copied(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanOpenMode {
    CleanContext,
    ResumeSourceSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPlanMode {
    CleanContext,
    KeepContext,
}

impl StartPlanMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::CleanContext => "Start clean",
            Self::KeepContext => "Keep context",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::CleanContext => "Plan only",
            Self::KeepContext => "Plan + current transcript",
        }
    }
}

impl PlanOpenMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::CleanContext => "Load clean",
            Self::ResumeSourceSession => "Load session",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::CleanContext => "Plan only",
            Self::ResumeSourceSession => "Plan + transcript",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderRunSelection {
    pub provider: String,
    pub model: String,
    pub reasoning: ReasoningSelection,
}

#[derive(Debug, Clone)]
pub enum UiEvent {
    AssistantDelta(String),
    AssistantDone,
    ReasoningDelta(String),
    /// A provider attempt is about to stream; marks the retraction floor for
    /// a possible `AttemptDiscarded` and breaks streamed-cell merging so the
    /// attempt's output lives in its own transcript cells.
    AttemptStarted,
    /// The streaming attempt was abandoned (retryable failure, generation
    /// budget, or terminal error): withdraw its streamed cells so a retry's
    /// re-stream cannot splice into abandoned text.
    AttemptDiscarded,
    Thinking(String),
    ToolStarted {
        id: String,
        name: String,
        arguments: String,
        started_at: Instant,
    },
    ToolCallsStarted {
        calls: Vec<ToolCallStart>,
        started_at: Instant,
    },
    ToolOutput {
        id: String,
        output: String,
        updated_at: Instant,
    },
    ToolFinished {
        id: String,
        result: String,
        success: bool,
        finished_at: Instant,
    },
    ToolFinishedWithDiff {
        id: String,
        result: String,
        success: bool,
        // Boxed: `FileDiff` carries a `Vec<DiffHunk>` and is far larger than the
        // other `UiEvent` variants, which would otherwise inflate every event
        // moved through the channel.
        diff: Box<FileDiff>,
        finished_at: Instant,
    },
    /// Paths observed changing after an unstructured mutation such as Bash.
    WorkspaceChanged {
        paths: Vec<String>,
    },
    QueuedUserMessageSent {
        id: u64,
        text: String,
    },
    ContextUpdated(Box<crate::agent::ContextReport>),
    Status(String),
    /// A context-compaction status; the app collapses consecutive ones.
    CompactionStatus(String),
    Error(UiError),
    Interrupted,
}

/// How an agent run ended, carried on [`RuntimeEvent::AgentFinished`]. An
/// interrupted run must be distinguishable from a clean finish *from the event
/// itself*: inferring it from UI state raced with `UiEvent::Interrupted`
/// (which resets the state to Idle first), making Ctrl+C on a phased run look
/// like a successful phase and auto-advance to the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRunOutcome {
    Completed,
    Failed,
    Interrupted,
    BudgetExhausted(crate::run_budget::RunBudgetExhaustion),
    Waiting(crate::agent::WaitReason),
}

#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    AgentStarted,
    CompletionReport(Box<crate::completion_report::CompletionReport>),
    AgentFinished(Result<AgentRunOutcome, UiError>),
    CommandFinished(Box<CommandOutcomeEvent>),
    LocalModelWizardFetchFinished {
        request_id: u64,
        result: Result<crate::tui::local_model_wizard::WizardFetchOutcome, UiError>,
    },
    /// A background agent-composer prompt-generation call finished. Guarded by
    /// `request_id` against a stale reply landing in a reused modal.
    AgentComposerPromptGenerated {
        request_id: u64,
        result: Result<String, UiError>,
    },
    CatalogReloaded {
        model_catalog: Arc<crate::model_catalog::ModelCatalog>,
        registry: Arc<crate::provider::ProviderRegistry>,
        outcome: Box<CommandOutcomeEvent>,
    },
    BackgroundTaskRemovalFinished {
        task_id: String,
        error: Option<UiError>,
    },
    /// The async repository-map build finished; inject it into the agent's
    /// context. Carries the rendered, non-empty map.
    RepoMapReady(String),
    /// The git branch changed since the last poll (e.g. the user ran
    /// `git checkout` in another terminal). Carries the new branch, or `None`
    /// when the working copy left a branch (detached HEAD) or the repo went
    /// away. Only emitted on an actual change.
    BranchChanged(Option<String>),
    /// The startup self-update task staged a new binary or found an update it
    /// could not install. Rendered as a persistent composer-meta hint; sent at
    /// most once per session.
    UpdateNotice(String),
    /// A run dispatch applied the persona's own model (per-mode `/model` entry
    /// or a custom agent's `model:`); update the provider/model/reasoning
    /// mirrors so the composer meta line names the model actually serving the
    /// run.
    PersonaModelApplied(Box<ProviderRunSelection>),
    /// A `/update` background run finished; render the outcome as a transcript
    /// status/error row. `staged_notice` additionally arms the persistent
    /// composer-meta hint (the same surface as the startup check) when a
    /// binary was staged or an update exists that could not be installed.
    UpdateCommandFinished {
        message: String,
        kind: CommandOutputKind,
        staged_notice: Option<String>,
    },
    /// The peer watcher claimed inter-agent messages addressed to this session
    /// (peers P2); render them as blue chat messages. Only emitted non-empty.
    PeerMessagesArrived(Vec<crate::storage::LeasedPeerMessage>),
    /// The live-peer set/claims/changed-files changed since the last poll
    /// (peers P5); refresh the `/peers` view if it is open. Only emitted on an
    /// actual change.
    PeersChanged(Vec<crate::peer::PeerOverview>),
    /// One source in a `/refresh` cycle finished. `generation` must match the
    /// open `ModalKind::Refresh` generation; stale events are dropped.
    RefreshSourceCompleted {
        generation: u64,
        index: usize,
        source: crate::commands::RefreshSourceState,
    },
    /// Every source in a `/refresh` cycle has settled; the modal can mark the
    /// refresh as finished if its generation still matches.
    RefreshAllSourcesCompleted {
        generation: u64,
    },
    /// The count of peer messages that have arrived but not yet been injected
    /// into the agent (parked, awaiting the next turn) changed. Drives the
    /// composer "peers waiting" badge; self-heals to 0 once a turn drains them.
    /// Only emitted on an actual change.
    PeerInboxChanged(usize),
    TaskPanicked(UiError),
}

#[derive(Debug, Clone)]
pub struct UiError {
    pub title: String,
    pub detail: String,
}

impl UiError {
    pub fn new(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum CommandOutcomeEvent {
    Applied {
        /// Generation tag for command-style spawns that run outside
        /// `TaskController`. `Some(g)` means "apply only while the app's command
        /// generation is still `g`" — cancelling bumps the generation so a late
        /// result is dropped as stale. `None` is not generation-guarded (e.g.
        /// `TaskController`-routed slash commands, cancelled by aborting the
        /// task handle).
        generation: Option<u64>,
        clear_transcript: bool,
        messages: Vec<CommandOutputEvent>,
        provider: Option<Box<ProviderRunSelection>>,
        context_report: Option<Box<crate::agent::ContextReport>>,
        quit: bool,
        open_modal: Option<ModalKind>,
    },
}

#[derive(Debug, Clone)]
pub struct CommandOutputEvent {
    pub kind: CommandOutputKind,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum AppAction {
    Tick,
    /// `/bonsai` — reseed and regrow the decorative trees (welcome screen and
    /// todo sidebar).
    ReplantBonsai,
    InputChar(char),
    Backspace,
    ComposerUndo,
    ComposerRedo,
    ClearInput,
    SubmitInput(String),
    SubmitCommandInput(String),
    QueueInput {
        id: u64,
        /// Placeholder text shown in the transcript's queued row.
        text: String,
        /// Composer snapshot (buffer + chip payloads) for withdraw-restore.
        content: crate::tui::app::ComposerContent,
        mode: crate::agent::AgentMode,
    },
    QueueDeferredCommand {
        input: String,
        label: String,
    },
    QueueDeferredModelSelection {
        input: String,
        label: String,
        selection: crate::tui::pickers::ModelSelection,
    },
    RemoveDeferredCommand {
        id: u64,
    },
    CancelQueuedInput {
        id: u64,
    },
    CancelFocusedQueuedInput,
    WithdrawQueuedInput,
    DropQueuedInput,
    /// Page-scroll the composer. The sign is interpreted as up/down; the
    /// body height is resolved in the reducer so the keymap doesn't have
    /// to know the live layout (completion popover changes the body).
    ComposerPage(i16),
    /// Set the composer scroll to an absolute visual row. Used by
    /// Ctrl+Home/Ctrl+End and the (future) mouse-wheel "jump" path.
    SetComposerScroll(u16),
    /// Shift+PageUp/PageDown: scroll one page AND extend the text selection
    /// by moving the cursor one page. The sign is up/down.
    ExtendComposerByPage(i16),
    ToggleView,
    SetView(View),
    /// Cycle the approval level along the confined ladder (Alt+M). The shared
    /// holder is updated in `runtime_actions`, which mirrors it via
    /// `SetApprovalLevel`.
    CycleApprovalLevel,
    /// Set the approval level directly. The enforcement holder is driven
    /// separately by the dispatcher; this only mirrors it into view state.
    SetApprovalLevel(crate::tool::ApprovalLevel),
    /// Mirror the live agent's effective SMOL profile into view state.
    SetSmolMode(bool),
    /// Toggle the TUI-only calm transcript presentation.
    SetSerenityMode(bool),
    ScrollCurrent(i16),
    SetCurrentScroll(u16),
    ScrollTop,
    ScrollBottom,
    ScrollSidebar(i16),
    SetSidebarScroll(u16),
    /// Browse phases in the todo card while it has focus. Sign = direction;
    /// `i16::MIN`/`MAX` jump to the first/last phase. No wrap; inert unless the
    /// plan has multiple phases.
    MoveTodoPhase(i16),
    ScrollPlan(i16),
    SetPlanScroll(u16),
    ImplementPlan,
    ScrollModal(i16),
    MoveTranscriptFocus(i16),
    FocusTranscriptFirst,
    FocusTranscriptLast,
    OpenFocusedDetail,
    ClearTranscriptFocus,
    OpenToolDetail(String),
    /// Open the findings detail modal at the most-severe finding.
    OpenPlanFindings,
    /// Move between findings within the open findings modal (clamped).
    CyclePlanFinding(i16),
    /// From the findings modal, jump to the focused finding's source tool card
    /// if its evidence id still resolves in the transcript; no-op otherwise.
    OpenFindingEvidence,
    OpenDiffPreview(String),
    OpenContextModal,
    OpenEpisodesModal,
    OpenContextPreviewModal(crate::agent::ContextReport),
    RefreshContextModal(crate::agent::ContextReport),
    ContextMove(i16),
    ContextToggleSelected,
    ContextPinSelected,
    ContextDropSelected,
    ContextStubSelected,
    ContextRestoreSelected,
    ContextToggleView,
    ContextClick {
        row_index: usize,
    },
    ContextExpandSelected,
    ContextCollapseSelected,
    EpisodesMove(i16),
    OpenTaskList,
    RefreshTaskList {
        tasks: Vec<crate::background::BackgroundTaskSnapshot>,
    },
    TaskListMove(i16),
    TaskListDeleteSelected,
    SetTaskListStatus(Option<String>),
    OpenPeerList {
        peers: Vec<crate::peer::PeerOverview>,
    },
    RefreshPeerList {
        peers: Vec<crate::peer::PeerOverview>,
    },
    PeerListMove(i16),
    DoctorMove(i16),
    OpenUsageDashboard {
        dashboard: Box<crate::storage::UsageDashboard>,
    },
    /// `/refresh` — open the live refresh-status modal and spawn the async
    /// driver. Runtime handler (`runtime_actions`) owns the spawn.
    StartRefresh,
    /// Move the selection in the `/refresh` modal.
    RefreshMove(i16),
    /// Close the `/refresh` modal.
    RefreshClose,
    /// Replace the source at `index` in the open `/refresh` modal. Stale iff
    /// `generation` doesn't match the modal's generation.
    RefreshSourceUpdate {
        generation: u64,
        index: usize,
        source: crate::commands::RefreshSourceState,
    },
    /// Mark the `/refresh` modal as finished (every source settled).
    RefreshFinished {
        generation: u64,
    },
    /// Cycle the `/usage` tab by the given step (wraps both ways).
    UsageDashboardCycleTab(i8),
    UsageDashboardSelectTab(UsageTab),
    OpenSubtaskList,
    RefreshSubtaskList {
        subtasks: Vec<crate::subagent::SubagentSnapshot>,
    },
    SubtaskListMove(i16),
    SubtaskListSetPane(SubtaskListPane),
    SubtaskListTogglePane,
    /// Open the model picker to override the selected `/subagents` agent's model.
    SubtaskOverrideModel,
    /// Clear the selected `/subagents` agent's model override so its next run
    /// inherits the default (session) model again.
    SubtaskClearModelOverride,
    SetShutdownNotice(Option<String>),
    OpenExecutionGroup {
        group_id: u64,
    },
    ToggleExecutionGroup {
        group_id: u64,
    },
    ExecutionGroupMoveSelection {
        delta: i16,
    },
    ClearExecutionGroupSelection,
    OpenSelectedExecutionGroupTool,
    OpenSelectedExecutionGroupDiff,
    SetTaskState(TaskState),
    CommandOutput {
        kind: CommandOutputKind,
        text: String,
    },
    /// Render an inter-agent chat message (peers P2) as a blue message in the
    /// conversation flow. `session_id` is the other end; `outgoing` marks a
    /// message this session sent.
    PeerMessage {
        source_message_id: Option<i64>,
        delivery_receipt: Option<crate::storage::PeerDeliveryReceipt>,
        session_id: i64,
        outgoing: bool,
        text: String,
    },
    /// Update the parked-inbox count shown in the composer meta line (peers): the
    /// number of peer messages awaiting injection on the next turn.
    PeerInboxChanged {
        count: usize,
    },
    #[cfg(test)]
    ProviderModelChanged {
        provider: String,
        model: String,
        reasoning: ReasoningSelection,
    },
    SetFocus(Focus),
    OpenModal(ModalKind),
    CloseModal,
    OnboardingMove(i16),
    OnboardingSubmit,
    ApiKeyInputChar(char),
    ApiKeyInputBackspace,
    ApiKeyInputSubmit,
    ApiKeyInputPaste(String),
    ApiKeyInputMoveField(i16),
    ApiKeyPersistenceToggle,
    AuthorizeProviderPickerMove(i16),
    /// Append a typed character to the authorize picker's filter (resets cursor).
    AuthorizeProviderPickerInputChar(char),
    /// Delete the last character of the authorize picker's filter.
    AuthorizeProviderPickerInputBackspace,
    AuthorizeProviderPickerSubmit,
    ModelPickerInputChar(char),
    ModelPickerInputBackspace,
    ModelPickerMove(i16),
    ModelPickerMovePane(i16),
    ModelPickerAssignShortcut(crate::model_role::ModelShortcutKey),
    ModelPickerSubmit,
    SessionPickerMove(i16),
    SessionPickerSubmit,
    PlanPickerInputChar(char),
    PlanPickerInputBackspace,
    PlanPickerMove(i16),
    PlanPickerSubmit,
    PlanPickerDeleteSelected,
    PlanOpenChoiceMove(i16),
    PlanOpenChoiceSubmit,
    StartPlanChoiceMove(i16),
    StartPlanChoiceSubmit,
    PlanDeleteConfirmSubmit,
    PlanDiscardConfirmSubmit,
    ReviewScopePickerMove(i16),
    ReviewScopePickerSubmit,
    LocalModelWizardInputChar(char),
    LocalModelWizardBackspace,
    LocalModelWizardPaste(String),
    LocalModelWizardMoveField(i16),
    LocalModelWizardMoveModel(i16),
    LocalModelWizardToggle,
    /// Cycle the active Setup choice field (Server preset / Store key)
    /// backward or forward — bound to ←/→ while such a field is focused.
    LocalModelWizardCycleChoice(i16),
    LocalModelWizardSubmit,
    LocalModelWizardBack,
    AgentBrowserMove(i16),
    AgentBrowserAdd,
    AgentBrowserEdit,
    AgentBrowserDelete,
    AgentBrowserToggleEnabled,
    /// `/providers` manager: move the list selection.
    ProviderManagerMove(i16),
    /// Enter inline-search mode (`/`): bare letters now edit the filter.
    ProviderManagerBeginSearch,
    /// Append a typed character to the manager filter (resets cursor).
    ProviderManagerSearchChar(char),
    /// Delete the last character of the manager filter.
    ProviderManagerSearchBackspace,
    /// Leave search-typing mode but keep the filter applied (so `a`/`e`/`d`
    /// operate on the narrowed selection).
    ProviderManagerSearchExit,
    /// Clear an applied filter without closing the manager.
    ProviderManagerClearFilter,
    /// Open the add-provider wizard from the manager.
    ProviderManagerAdd,
    /// Edit the selected custom provider in the wizard.
    ProviderManagerEdit,
    /// Open the read-only detail view for the selected provider.
    ProviderManagerShowDetail,
    /// Close the provider detail view and return to the manager list.
    ProviderDetailBack,
    /// Remove the selected custom provider / disable the selected built-in
    /// (or re-enable a disabled built-in directly).
    ProviderManagerRemove,
    ProviderRemoveConfirmSubmit,
    ProviderRemoveConfirmCancel,
    /// Skills manager (`/skills`): move the list selection (resets detail scroll).
    SkillManagerMove(i16),
    /// Enable/disable the selected built-in skill (writes `.disabled`, reloads).
    SkillManagerToggleDisabled,
    /// Load the selected skill's body into the conversation, or unload it if it
    /// is already loaded.
    SkillManagerToggleLoaded,
    /// `/memory`: move the exact-row selection.
    MemoryManagerMove(i16),
    /// Toggle the selected memory entry enabled/disabled.
    MemoryManagerToggleEnabled,
    /// Delete the selected memory entry.
    MemoryManagerDelete,
    /// Open the add-memory wizard from the manager.
    MemoryManagerAdd,
    /// Edit the selected memory entry in the wizard (prefilled, identity kept).
    MemoryManagerEdit,
    /// Move between wizard fields.
    MemoryAddWizardMoveField(i16),
    /// Input into the active wizard field.
    MemoryAddWizardInputChar(char),
    MemoryAddWizardBackspace,
    /// Cycle or toggle the active wizard field's current value.
    MemoryAddWizardCycleValue(i16),
    MemoryAddWizardToggleValue,
    /// Advance the wizard to the next step or save on the review step.
    MemoryAddWizardSubmit,
    /// Back up one wizard step.
    MemoryAddWizardBack,
    /// `/settings`: move the cursor between selectable rows (skips headers).
    SettingsMove(i16),
    /// Cycle the selected `Choice` setting by `delta` and apply it live.
    SettingsCycle(i16),
    /// Activate the selected row: open the model/theme picker for an `Action`,
    /// or cycle forward for a `Choice`.
    SettingsActivate,
    AgentComposerInputChar(char),
    AgentComposerBackspace,
    AgentComposerPaste(String),
    AgentComposerMove(i16),
    AgentComposerToggle(i16),
    AgentComposerBack,
    AgentComposerNextPage,
    AgentComposerSubmit,
    AgentComposerGenerate,
    /// Open the `/model` picker to choose the composer's model (any authorized
    /// provider); the picker submit writes back into the composer.
    AgentComposerOpenModelPicker,
    /// Clear the focused model-chain slot: primary reverts to inheriting the
    /// parent, backup disables failover.
    AgentComposerDeleteModel,
    /// Move the caret within the focused composer text field.
    AgentComposerCursor(crate::tui::agent_composer::CursorMotion),
    /// Delete the grapheme after the caret in the focused text field.
    AgentComposerDeleteForward,
    /// Insert a newline in the multi-line Prompt field.
    AgentComposerInsertNewline,
    AgentDeleteConfirmSubmit,
    AgentDeleteConfirmCancel,
    SetActiveSavedPlan(Option<SavedPlanId>),
    /// A decision on one of the approval prompts (permission, sandbox
    /// escalation, web domain, extension tool, hook trust). The five prompt
    /// families share the AllowOnce / AllowSession / AllowProject / Deny
    /// shape; `family` names the prompt and `decision` names the tier.
    /// `tui::runtime_actions` maps the tier onto the family's outcome type
    /// (`PermissionDecision`, or `EscalationDecision` for sandbox escapes).
    PromptDecision {
        family: PromptFamily,
        decision: PromptDecision,
    },
    ResetPermissionToolTimer {
        command: String,
        /// Supplied by the caller so the reducer stays a pure, clock-free
        /// state transition.
        at: std::time::Instant,
    },
    QuestionMove(i16),
    QuestionToggle,
    QuestionSubmit,
    QuestionCancel,
    McpServersMove(i16),
    McpServersToggle,
    McpServersReload,
    McpServersAuthorize,
    SandboxMove(i16),
    SandboxToggle,
    ThemePickerMove(i16),
    ThemePickerSubmit,
    ThemePickerCancel,
    BusyCommandMove(i16),
    BusyCommandSubmit,
    /// Move the `/mode` picker cursor. Delta is clamped against the rows
    /// length. On `Header` rows, Enter/Right jump to the next value row of
    /// the same axis instead of cycling.
    ModePickerMove(i16),
    /// Cycle the focused `/mode` value row by `delta` (positive = forward).
    /// On `Header` rows this is a no-op; runtime handlers can also use it
    /// to advance the value at a specific axis when invoked programmatically.
    ModePickerCycle(i16),
    InsertNewline,
    PasteText(String),
    /// Read the system clipboard and paste its image (preferred) or text into
    /// the composer. Intercepted in the event loop (it needs clipboard + model
    /// catalog access) rather than handled purely in the reducer.
    PasteFromClipboard,
    /// Insert an already-encoded pasted image as an `[Image N]` chip.
    PasteImage {
        base64: String,
        byte_len: usize,
        width: u32,
        height: u32,
    },
    HistoryPrev,
    HistoryNext,
    CompleteInputTo(String),
    CompleteInputRange {
        start: usize,
        end: usize,
        replacement: String,
    },
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    CursorStart,
    CursorEnd,
    CursorWordLeft,
    CursorWordRight,
    DeleteForward,
    DeleteWordBack,
    DeleteToLineStart,
    DeleteToLineEnd,
    CompletionMove(i16),
    CompletionDismiss,
    ExtendCursorLeft,
    ExtendCursorRight,
    ExtendCursorUp,
    ExtendCursorDown,
    ExtendCursorStart,
    ExtendCursorEnd,
    ComposerClick {
        char_index: usize,
        kind: crate::tui::app::SelectionKind,
        column: u16,
        row: u16,
    },
    ComposerDrag {
        char_index: usize,
    },
    ComposerSelectAll,
    TranscriptClick {
        position: crate::tui::app::TranscriptPosition,
        kind: crate::tui::app::SelectionKind,
        extend: bool,
        column: u16,
        row: u16,
    },
    TranscriptDrag {
        position: crate::tui::app::TranscriptPosition,
        scroll_delta: i16,
    },
    PlanClick {
        position: crate::tui::app::PlanPosition,
        kind: crate::tui::app::SelectionKind,
        column: u16,
        row: u16,
    },
    PlanDrag {
        position: crate::tui::app::PlanPosition,
        scroll_delta: i16,
    },
    /// Move the transcript selection caret by N rendered lines, extending
    /// the selection. Negative = up, positive = down. Wraps across item
    /// boundaries.
    ExtendTranscriptCursor(i16),
    /// Select the whole transcript (anchor = (0, 0), caret = last).
    TranscriptSelectAll,
    /// The left mouse button was released. Finalizes a pointer-driven text
    /// selection by copying it once (see the transcript reducer), so the copy
    /// notice fires on release rather than on every drag tick.
    PointerSelectionEnd,
    ClearSelection,
    /// A click landed inside a modal body. `kind` encodes single/double/triple.
    ModalClick {
        offset: usize,
        kind: crate::tui::app::SelectionKind,
        column: u16,
        row: u16,
    },
    /// A drag (left button held) landed inside a modal body.
    ModalDrag {
        offset: usize,
    },
    Agent(UiEvent),
    Runtime(RuntimeEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Idle,
    Running,
    Cancelling,
    Command,
    Exiting,
}

impl TaskState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Command => "command",
            Self::Exiting => "exiting",
        }
    }

    pub fn is_busy(self) -> bool {
        !matches!(self, Self::Idle)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutputKind {
    Status,
    Error,
    /// A context-compaction status; rendered like `Status` but kept as a
    /// distinct kind so consecutive rows collapse instead of piling up.
    CompactionStatus,
}

impl CommandOutputKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Error => "command_error",
            Self::CompactionStatus => "compaction_status",
        }
    }

    pub fn message_role(self) -> &'static str {
        match self {
            Self::Status | Self::CompactionStatus => "system",
            Self::Error => "error",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Error => "command error",
            Self::CompactionStatus => "context compacted",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CommandOutputKind;

    #[test]
    fn command_output_kind_db_strings_and_labels_are_stable() {
        assert_eq!(CommandOutputKind::Status.as_db_str(), "status");
        assert_eq!(CommandOutputKind::Status.message_role(), "system");
        assert_eq!(CommandOutputKind::Status.label(), "status");
        assert_eq!(CommandOutputKind::Error.as_db_str(), "command_error");
        assert_eq!(CommandOutputKind::Error.message_role(), "error");
        assert_eq!(CommandOutputKind::Error.label(), "command error");
        assert_eq!(
            CommandOutputKind::CompactionStatus.as_db_str(),
            "compaction_status"
        );
        assert_eq!(CommandOutputKind::CompactionStatus.message_role(), "system");
        assert_eq!(
            CommandOutputKind::CompactionStatus.label(),
            "context compacted"
        );
    }
}
