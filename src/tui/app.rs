use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use ratatui::text::Line;

use crate::agent::AgentMode;
use crate::output::ToolCallStart;
use crate::provider::ReasoningSelection;
use crate::storage::{SavedPlanId, SessionId, SessionStatus};
use crate::todo::TodoItem;
use crate::tui::event::{
    AgentRunOutcome, AppAction, CommandOutcomeEvent, CommandOutputKind, Focus, ModalKind,
    RuntimeEvent, TaskState, UiEvent, View,
};
use crate::tui::pickers::{ModelOption, ProviderOption};

pub(crate) const CONTEXT_WIRE_RAW_JSON_ID: &str = "wire-raw-json";

/// Most recent turns shown in the `/ctx` Turns view; older rows scroll off so
/// a long session cannot make the modal unmanageable.
pub(crate) const CONTEXT_TURNS_ROW_LIMIT: usize = 50;

mod completion_state;
mod composer;
mod context_view_state;
mod copy;
mod execution_group;
mod model_picker;
mod mouse;
mod provider_auth;
mod queue;
mod reducer;
mod tool_activity;
mod transcript_selection;
pub use crate::tui::transcript::{
    ExecutionGroup, InlineToolSelection, ItemSelection, SelectionKind, ToolActivity, ToolStatus,
    TranscriptItem, TranscriptModel, TranscriptPosition, TranscriptSelection,
};
pub use completion_state::{CompletionCandidate, CompletionState};
pub use composer::{ChipPayload, Composer, ComposerContent, ComposerSubmission};
pub use copy::{CopyNotice, CopyNoticeKind, SessionHint, SessionToast};
pub(crate) use model_picker::reconcile_viewport;
pub use model_picker::{ModelPickerPane, ModelPickerState, ModelPickerTarget};
pub use mouse::{LastMouseClick, MouseArea, next_mouse_click};
pub use provider_auth::{ProviderAuthField, ProviderAuthForm};
pub use queue::{DeferredCommand, DeferredCommandPayload, QueuedInput};
pub(crate) use reducer::scroll::clamped_scroll;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanPosition {
    pub line: usize,
    pub grapheme: usize,
    pub width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanSelection {
    pub anchor: PlanPosition,
    pub caret: PlanPosition,
}

impl PlanSelection {
    pub fn range(self) -> (PlanPosition, PlanPosition) {
        if (self.anchor.line, self.anchor.grapheme) <= (self.caret.line, self.caret.grapheme) {
            (self.anchor, self.caret)
        } else {
            (self.caret, self.anchor)
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextViewMode {
    #[default]
    Ledger,
    Wire,
    Turns,
}

impl ContextViewMode {
    pub const fn is_ledger(self) -> bool {
        matches!(self, Self::Ledger)
    }

    pub const fn is_wire(self) -> bool {
        matches!(self, Self::Wire)
    }

    pub const fn is_turns(self) -> bool {
        matches!(self, Self::Turns)
    }

    pub const fn cycled(self) -> Self {
        match self {
            Self::Ledger => Self::Wire,
            Self::Wire => Self::Turns,
            Self::Turns => Self::Ledger,
        }
    }
}

/// Per-view state for the context inspector modal (ledger + wire + turns
/// views). Grouped so the inspector's fields live together instead of spread
/// across `AppState`.
#[derive(Debug, Clone, Default)]
pub struct ContextModalState {
    pub cursor: usize,
    pub expanded: HashSet<String>,
    pub wire_cursor: usize,
    pub wire_expanded: HashSet<String>,
    /// Turns-view selection, an ordinal into the visible turn rows (compaction
    /// event rows are display-only and never selected).
    pub turns_cursor: usize,
    /// Turns expanded to their per-turn detail, keyed by turn `seq`.
    pub turns_expanded: HashSet<usize>,
    pub manual_scroll: bool,
    /// One-shot: set when a row was just expanded so the next scroll pass
    /// brings the opened content into view (not only the selected row).
    pub reveal_expanded: bool,
    pub view_mode: ContextViewMode,
}

/// Tracks an in-flight phased-plan implementation: which phase the coding
/// agent is currently working through. The event loop advances it as each
/// phase's run completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanExecution {
    pub phase_index: usize,
}

/// What to do when a phased plan's run terminates, decided by the
/// `AgentFinished` reducer and acted on by the event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhaseAdvance {
    /// The phase succeeded — advance to the next pending phase.
    Continue,
    /// The phase errored or was interrupted — stop auto-advancing.
    Halt,
}

/// A grapheme-offset selection inside a modal body. `anchor` is where the
/// pointer went down; `caret` is where it is now (or where it was released).
/// Offsets are into the flat grapheme sequence of the unwrapped body text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModalSelection {
    pub anchor: usize,
    pub caret: usize,
}

impl ModalSelection {
    pub fn range(self) -> (usize, usize) {
        if self.anchor <= self.caret {
            (self.anchor, self.caret)
        } else {
            (self.caret, self.anchor)
        }
    }
}

/// A cheap identity for the open modal, used to drop a stale text selection
/// when the modal changes. The discriminant catches variant swaps; the key
/// catches same-variant content swaps (next finding, another tool's detail)
/// without comparing whole payloads.
fn modal_identity(kind: &ModalKind) -> (std::mem::Discriminant<ModalKind>, u64) {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    match kind {
        ModalKind::Picker(modal) => std::mem::discriminant(modal).hash(&mut hasher),
        ModalKind::Manager(modal) => std::mem::discriminant(modal).hash(&mut hasher),
        ModalKind::Wizard(modal) => std::mem::discriminant(modal).hash(&mut hasher),
        ModalKind::Confirm(modal) => std::mem::discriminant(modal).hash(&mut hasher),
        ModalKind::Detail(modal) => std::mem::discriminant(modal).hash(&mut hasher),
    }
    match kind {
        ModalKind::Detail(crate::tui::event::DetailModal::ToolDetail { tool_id })
        | ModalKind::Detail(crate::tui::event::DetailModal::DiffPreview { tool_id }) => {
            tool_id.hash(&mut hasher)
        }
        ModalKind::Detail(crate::tui::event::DetailModal::BlockDetail { item_index }) => {
            item_index.hash(&mut hasher)
        }
        ModalKind::Detail(crate::tui::event::DetailModal::PlanFindingDetail { index }) => {
            index.hash(&mut hasher)
        }
        _ => {}
    }
    (std::mem::discriminant(kind), hasher.finish())
}

pub struct AppState {
    pub(crate) transcript: TranscriptModel,
    /// Per-item rendered-line cache for the chat transcript. Interior-mutable
    /// because the draw path only holds `&AppState`; see
    /// [`crate::tui::widgets::transcript::TranscriptLayoutCache`].
    pub(crate) transcript_layout: crate::tui::widgets::transcript::TranscriptLayoutCache,
    pub(crate) composer: Composer,
    clipboard: crate::copy::Clipboard,
    pub(crate) view: View,
    /// The persona the agent will use on the next dispatch: a built-in mode or an
    /// enabled custom agent. Cycled by Shift+Tab (`ToggleView`) and set by `SetView` /
    /// `/review`. Single source of truth — read `active_mode()` for the built-in
    /// mode (custom personas map to a neutral built-in for view/keymap purposes).
    pub(crate) active_persona: crate::agent::ActivePersona,
    /// Mirror of the persona the in-flight run was dispatched with (see
    /// [`TaskController::active_agent_persona`]), refreshed once per frame by
    /// the event loop. `None` while idle. When it differs from
    /// `active_persona`, the composer meta line renders the transition
    /// (`Running → Selected`) so a mid-run mode switch reads as "next run",
    /// never as an interruption of the current one.
    pub(crate) running_persona: Option<crate::agent::ActivePersona>,
    /// Shared custom-agent registry, so the reducer/renderer can resolve a custom
    /// persona's view / color / name live (the composer hot-swaps it).
    pub(crate) custom_agents: crate::resource::agent::SharedAgentRegistry,
    /// Shared skill registry handle, so `/skills` can snapshot rows without the
    /// agent lock — a running turn holds that lock for its entire duration.
    pub(crate) skills: crate::resource::skill::SharedSkillRegistry,
    /// Shared built-in subagent settings, so `/agents` browser rows build
    /// without the agent lock (same hot-swap handle the resolver reads).
    pub(crate) builtin_subagents: crate::subagent::SharedBuiltinSubagentSettings,
    /// Mirror of the agent's loaded-skills set (see [`Self::refresh_agent_mirrors`]):
    /// read by the lock-free `/skills` row builders. A skill the running turn
    /// loads via the `skill` tool shows up at the next turn boundary.
    pub(crate) loaded_skills: std::collections::BTreeSet<String>,
    /// A skill enable/disable happened while a run held the agent lock: the
    /// shared registry is already swapped (the `skill` tool sees it live), but
    /// the prompt's Skills index still needs `Agent::reload_skills` at the next
    /// lock boundary.
    pub(crate) skills_index_refresh_pending: bool,
    /// Same deferral for the prompt's Subagents index after a mid-run agent
    /// edit (`Agent::refresh_agents_index` at the next lock boundary).
    pub(crate) agents_index_refresh_pending: bool,
    pub(crate) focus: Focus,
    pub(crate) modal: Option<ModalKind>,
    pub(crate) modal_return_focus: Option<Focus>,
    /// Live per-agent subagent model overrides (set from `/subagents`). Shared with
    /// the provider factory: written here by the model picker, read there when a
    /// subagent is minted. Also read by the `/subagents` renderer for the badge.
    pub(crate) subagent_model_overrides: crate::subagent::SubagentModelOverrides,
    /// The agent name awaiting a model-picker selection (set when the user presses
    /// `m` on a `/subagents` row, consumed by the picker submit).
    pub(crate) pending_agent_model_override: Option<String>,
    /// Set while the `/self-review model` picker is open. On submit the choice is
    /// persisted under `BuiltinSubagentId::SelfReview` (not the live name-keyed
    /// override map) so the pinned reviewer model survives a restart.
    pub(crate) pending_self_review_model: bool,
    /// The agent composer, stashed while the `/model` picker is open on its behalf.
    /// The picker submit writes the chosen model into it and reopens; picker cancel
    /// (`CloseModal`) reopens it unchanged.
    pub(crate) pending_composer_state: Option<Box<crate::tui::agent_composer::AgentComposerState>>,
    pub(crate) task_list_status: Option<String>,
    pub(crate) shutdown_notice: Option<String>,
    /// Self-update hint ("updated to vX — restart to apply" / "update vX
    /// available — …") shown in the composer meta line in place of the
    /// version tag. Set once by the startup update task; never cleared, and
    /// never written to the transcript.
    pub(crate) update_notice: Option<String>,
    /// Auto-expiring orientation banner for an actionable session-lifecycle hint
    /// (e.g. "Interrupted session found. Use /resume…"). Ephemeral UI only —
    /// never persisted into the transcript, so resumes don't accumulate notices.
    pub(crate) session_hint: Option<SessionHint>,
    pub(crate) plan_selection: Option<PlanSelection>,
    pub(crate) transcript_selection: Option<TranscriptSelection>,
    pub(crate) transcript_focus: Option<usize>,
    /// Transcript index where the currently streaming provider attempt's
    /// cells begin (set by `UiEvent::AttemptStarted`). Bounds the removal on
    /// `UiEvent::AttemptDiscarded` so retraction can never eat committed
    /// cells from an earlier model call.
    attempt_stream_floor: Option<usize>,
    /// Forces the next streamed reasoning/assistant delta to open a fresh
    /// transcript cell instead of merging into a trailing cell left by an
    /// earlier (committed) model call, so a retraction removes whole cells.
    stream_cell_break: bool,
    pub(crate) active_group_tool_selection: Option<InlineToolSelection>,
    pub(crate) expanded_execution_groups: HashSet<u64>,
    pub(crate) last_mouse_click: Option<LastMouseClick>,
    /// True while a left-button pointer selection gesture is in progress
    /// (between mouse-down and mouse-up). Gates the copy-on-release so stray
    /// mouse-ups — e.g. after clicking a tool card — don't re-copy a stale
    /// selection.
    pub(crate) pointer_selecting: bool,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) reasoning: ReasoningSelection,
    /// The autonomy / approval level (mirrors the shared `YoloMode` holder).
    /// Set via `/autonomy`, `/yolo`, or Alt+M; surfaced in the status bar.
    pub(crate) approval_level: crate::tool::ApprovalLevel,
    /// Self-review-before-done policy, mirrored from the live agent for `/mode`.
    pub(crate) self_review_mode: crate::self_review::SelfReviewMode,
    /// Session-scoped SMOL profile mirrored from the live agent for status UI.
    pub(crate) smol_mode: bool,
    /// Session-scoped pure-mode mirror from the live agent.
    pub(crate) pure_mode: bool,
    /// TUI-only calm presentation mode: suppress reasoning rows and fold tool
    /// groups by default without changing provider reasoning.
    pub(crate) serenity_mode: bool,
    /// Mirror of the opt-in support lifecycle log preference, for the
    /// `/settings` row; the live gate is `crate::logging`'s atomic.
    pub(crate) support_log_enabled: bool,
    /// Freeze decorative animation while keeping real timers and expiry ticks live.
    pub(crate) reduced_motion: bool,
    /// Render a labelled, linear transcript for assistive terminal readers.
    pub(crate) screen_reader_mode: bool,
    /// Desired mouse-capture state (the "copy mode" toggle). `true` = in-app
    /// mouse (scroll/click) is live; `false` = capture released so the terminal
    /// handles native click-drag selection. The run loop reconciles the real
    /// terminal to this flag; the meta line shows a marker while it is off.
    pub(crate) mouse_capture: bool,
    /// Optional user-selected caps for future foreground runs.
    pub(crate) run_budget: crate::run_budget::RunBudget,
    /// Shared OS-sandbox handle, set by the run loop after construction. Read at
    /// render time so the status bar shows a `sandbox` marker while confinement is
    /// active. `None` in tests (no marker).
    pub(crate) sandbox: Option<crate::sandbox::CommandSandbox>,
    pub(crate) current_session_id: Option<SessionId>,
    /// Status of the live session, shown next to the id in the header so the user
    /// can always see which session they're in. The live session is always
    /// `Active`; transient "resumed" wording rides on `session_toast` instead.
    pub(crate) current_session_status: SessionStatus,
    pub(crate) current_terminal_reason: Option<crate::run_budget::RunBudgetExhaustion>,
    pub(crate) current_session_name: String,
    pub(crate) current_session_summary: String,
    pub(crate) active_saved_plan_session_id: Option<SavedPlanId>,
    pub(crate) cwd: String,
    /// Absolute project root, set by the run loop after construction. Used to
    /// display tool paths under it as root-relative (`src/foo.rs`) instead of the
    /// full absolute path. Empty until set (e.g. in tests) — paths show as-is.
    pub(crate) project_root: std::path::PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) run_started_at: Option<Instant>,
    pub(crate) completed_run_elapsed: Option<Duration>,
    pub(crate) current_phase: Option<String>,
    /// Tool calls currently executing in this turn, in `started_at` order.
    /// More than one entry means the agent is running concurrent tool calls.
    pub(crate) active_tools: Vec<(String, Instant)>,
    pub(crate) latest_context_report: Option<crate::agent::ContextReport>,
    pub(crate) context_state: ContextModalState,
    pub(crate) todo: Vec<TodoItem>,
    pub(crate) background_tasks: Vec<crate::background::BackgroundTaskSnapshot>,
    /// Cached snapshot of subagent runs, refreshed from the registry by
    /// the run loop so the `/subagents` modal renders without an async lock.
    pub(crate) subtasks: Vec<crate::subagent::SubagentSnapshot>,
    /// The model each subagent run actually uses, keyed by the `agent` tool
    /// call that launched it, adopted from the registry as runs mint their
    /// providers. Read by the tool-detail modal; kept out of `ToolActivity`
    /// so the transcript enum stays small.
    pub(crate) subagent_models: std::collections::HashMap<String, String>,
    /// Mirror of the shared plan store, refreshed each tick by the run loop
    /// so rendering never has to take the async lock.
    pub(crate) plan: crate::plan::PlanDoc,
    pub(crate) task_state: TaskState,
    /// When a phased plan is being implemented, which phase is currently
    /// running. `None` for flat plans or when no plan run is in flight.
    pub(crate) plan_execution: Option<PlanExecution>,
    /// Set by the `AgentFinished` reducer when a phased run terminates, then
    /// consumed by the event loop to auto-advance to the next phase or halt.
    pub(crate) phase_advance: Option<PhaseAdvance>,
    /// Set by the `AgentFinished` reducer when the coding agent ended its turn
    /// with a confirmed `enter_plan_mode` switch, then consumed by the event
    /// loop to swap the active persona and re-dispatch a continuation under the
    /// new mode. `None` on any other finish.
    pub(crate) pending_persona_switch: Option<AgentMode>,
    pub(crate) tick: u64,
    /// The bonsai in the empty todo sidebar. Fully grown at startup; `/bonsai`
    /// replants the tree and replays the growth.
    pub(crate) sidebar_bonsai: crate::tui::widgets::bonsai::BonsaiGrowth,
    pub(crate) copy_notice: Option<CopyNotice>,
    /// Auto-expiring confirmation toast for a completed session-lifecycle action
    /// (e.g. "Resumed session #9."). Ephemeral UI only — cleared on a tick window
    /// like `copy_notice`, never written to the transcript.
    pub(crate) session_toast: Option<SessionToast>,
    pub(crate) next_execution_group_id: u64,
    pub(crate) active_execution_group_id: Option<u64>,
    pub(crate) transcript_scroll: u16,
    pub(crate) sidebar_scroll: u16,
    pub(crate) plan_scroll: u16,
    pub(crate) modal_scroll: u16,
    /// Active text selection inside the modal body. `None` when no selection
    /// is in progress. Cleared on scroll, modal close, and modal switch.
    pub(crate) modal_selection: Option<ModalSelection>,
    /// Unwrapped body lines cached at render time so the mouse handler can
    /// resolve screen coordinates to grapheme offsets without re-generating
    /// content. Populated by `render_scrollable_modal` / `render_detail_pane`.
    /// Interior-mutable because the draw path only holds `&AppState`.
    pub(crate) modal_body_lines: std::cell::RefCell<Vec<Line<'static>>>,
    /// The body-area rect cached at render time. The mouse handler reads this
    /// instead of recomputing the layout (which would need the exact
    /// header/footer line counts that only the renderer knows).
    pub(crate) modal_body_rect: std::cell::Cell<Option<ratatui::layout::Rect>>,
    pub(crate) composer_scroll: u16,
    pub(crate) todo_focus_available: bool,
    /// Which phase the todo card shows. `None` follows the active execution
    /// phase; `Some(i)` means the user parked the view on phase `i` (Left/Right).
    /// Reset to `None` whenever the model rewrites the todo list.
    pub(crate) todo_phase_view: Option<usize>,
    /// The phase the todo card rests on once `plan_execution` clears (a phased
    /// run finished all phases, halted, or was interrupted). Without it the card
    /// would snap back to Phase 1 the instant execution ends; instead it holds
    /// the phase that was last active so the user still sees where work stopped.
    /// Reset when a fresh phased run starts.
    pub(crate) resting_todo_phase: Option<usize>,
    pub(crate) composer_follow: bool,
    pub(crate) pending_composer_page: Option<i16>,
    pub(crate) pending_composer_extend: Option<i16>,
    pub(crate) pending_question_visibility: bool,
    pub(crate) transcript_autoscroll: bool,
    /// Count of assistant replies the user had already seen the last time the
    /// transcript was sitting at the bottom. Frozen the moment they scroll up
    /// so [`AppState::unseen_message_count`] can report how many new replies
    /// arrived while they were reading back — drives the jump-to-latest pill.
    pub(crate) transcript_seen_messages: usize,
    pub(crate) scroll_transcript_focus_into_view: bool,
    /// Pending request to scroll the in-progress todo item into view, resolved in
    /// `clamp_scrolls` once the sidebar `Rect` is known. Mirrors
    /// `scroll_transcript_focus_into_view`. Set when the model rewrites the todo
    /// list or the user browses to another phase.
    pub(crate) scroll_todo_in_progress_into_view: bool,
    /// Default storage selected for the next newly entered provider credential.
    pub(crate) credential_persistence: crate::session::CredentialPersistence,
    /// Active guided-setup checkpoint, retained while provider/model submodals
    /// temporarily replace the onboarding modal.
    pub(crate) first_run_step: Option<crate::onboarding::FirstRunStep>,
    /// Generation of the session-model command submitted from first-run setup.
    /// Only its matching outcome may advance or clear the Model checkpoint.
    pub(crate) first_run_model_selection_pending: Option<u64>,
    pub(crate) provider_auth_form: ProviderAuthForm,
    pub(crate) model_picker: ModelPickerState,
    /// Viewport offset of the `/providers` list. Interior-mutable because it is
    /// reconciled at render time (the only place the list capacity is known),
    /// like the modal body caches. The cursor moves inside the visible window
    /// and the window shifts only at the edges (model-picker semantics).
    pub(crate) provider_manager_offset: std::cell::Cell<usize>,
    /// Viewport offset of the `/authorize` provider list. Same
    /// reconcile-at-render-time semantics — the cursor moves inside the window
    /// and the list scrolls only when it reaches an edge.
    pub(crate) authorize_provider_offset: std::cell::Cell<usize>,
    pub(crate) completion: CompletionState,
    pub(crate) queued_inputs: Vec<QueuedInput>,
    /// Peer messages that have arrived but await injection on the next turn
    /// (peers). Drives the composer "peers waiting" badge; self-heals to 0 once
    /// a turn drains the inbox.
    pub(crate) pending_peer_inbox: usize,
    /// UI delivery leases awaiting the same transaction that persists their
    /// transcript items. Keyed by durable message id for replay deduplication.
    pub(crate) pending_peer_delivery_receipts: BTreeMap<i64, crate::storage::PeerDeliveryReceipt>,
    /// The peer this session's last run parked on (`wake_when_done`), if the
    /// park is still pending. Structured state for the wake sweep and tests —
    /// the "Waiting for peer #N" phase string is display only.
    pub(crate) waiting_for_peer: Option<crate::agent::PeerWait>,
    pub(crate) deferred_commands: Vec<DeferredCommand>,
    pub(crate) next_queued_input_id: u64,
    pub(crate) next_deferred_command_id: u64,
    pub(crate) next_local_model_wizard_request_id: u64,
    /// Generation tag for command-style background work (auth/model/local-model
    /// commits) that runs on a raw `tokio::spawn` outside `TaskController`.
    /// Cancelling such a command bumps this so its late `CommandFinished` is
    /// recognised as stale and dropped instead of flipping provider/model or
    /// rewriting the transcript after the UI returned to idle.
    pub(crate) command_generation: u64,
    /// Whether at least one provider is authorized. Seeded at startup from the
    /// session store; flips true when a provider selection is applied (only an
    /// authorized provider can be selected). Drives the welcome screen's
    /// authorize-first guidance.
    pub(crate) has_authorized_provider: bool,
    /// Mirror of the registry's provider metadata, used by the keymap for completion
    /// without taking the session lock on every keystroke.
    pub(crate) provider_choices: Vec<ProviderOption>,
    /// Mirror of cached models across all authorized providers, used by the keymap.
    pub(crate) cached_model_choices: Vec<ModelOption>,
    /// Recent non-active persisted sessions for synchronous command completion.
    pub(crate) session_choices: Vec<crate::storage::SessionSummary>,
    pub(crate) path_search: Option<crate::tui::path_search::PathSearch>,
}

fn move_index(current: usize, delta: i16, max: usize) -> usize {
    if delta == i16::MIN {
        0
    } else if delta == i16::MAX {
        max
    } else if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        current.saturating_add(delta as usize)
    }
    .min(max)
}

impl AppState {
    /// The current composer draft. Derived from `composer.text` rather than
    /// stored, so it can never drift out of sync with the editing buffer.
    pub fn input(&self) -> &str {
        &self.composer.text
    }

    /// Tick used only for decorative rendering. State timers continue to use
    /// [`Self::tick`] even when motion is reduced.
    pub(crate) fn animation_tick(&self) -> u64 {
        if self.reduced_motion || self.screen_reader_mode {
            0
        } else {
            self.tick
        }
    }

    /// True while a bonsai growth animation is in flight and allowed to move —
    /// the event loop repaints at the poll cadence for the duration instead of
    /// the 1s idle interval, so the growth stays smooth.
    pub(crate) fn bonsai_growing(&self) -> bool {
        if self.reduced_motion || self.screen_reader_mode {
            return false;
        }
        self.sidebar_bonsai.is_growing()
    }

    /// Growth progress for a bonsai, snapping straight to the fully grown
    /// tree when motion is reduced (mirrors the site hero's reduced-motion
    /// handling).
    pub(crate) fn bonsai_progress(
        &self,
        growth: &crate::tui::widgets::bonsai::BonsaiGrowth,
    ) -> f64 {
        if self.reduced_motion {
            1.0
        } else {
            growth.progress()
        }
    }

    pub fn new(provider_id: &str, model: String, cwd: String, branch: Option<String>) -> Self {
        Self {
            transcript: TranscriptModel::default(),
            transcript_layout: Default::default(),
            clipboard: crate::copy::Clipboard::system(),
            view: View::Agent,
            active_persona: crate::agent::ActivePersona::default(),
            running_persona: None,
            custom_agents: crate::resource::agent::shared_registry(
                crate::resource::agent::AgentRegistry::empty(),
            ),
            skills: crate::resource::skill::shared_registry(
                crate::resource::skill::SkillRegistry::empty(),
            ),
            builtin_subagents: crate::subagent::SharedBuiltinSubagentSettings::default(),
            loaded_skills: std::collections::BTreeSet::new(),
            skills_index_refresh_pending: false,
            agents_index_refresh_pending: false,
            focus: Focus::Input,
            modal: None,
            modal_return_focus: None,
            subagent_model_overrides: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            pending_agent_model_override: None,
            pending_self_review_model: false,
            pending_composer_state: None,
            task_list_status: None,
            shutdown_notice: None,
            update_notice: None,
            session_hint: None,
            plan_selection: None,
            transcript_selection: None,
            transcript_focus: None,
            attempt_stream_floor: None,
            stream_cell_break: false,
            active_group_tool_selection: None,
            expanded_execution_groups: HashSet::new(),
            last_mouse_click: None,
            pointer_selecting: false,
            provider: provider_id.to_string(),
            model,
            reasoning: ReasoningSelection::default(),
            approval_level: crate::tool::ApprovalLevel::default(),
            self_review_mode: crate::self_review::SelfReviewMode::default(),
            smol_mode: false,
            pure_mode: false,
            serenity_mode: false,
            support_log_enabled: false,
            reduced_motion: false,
            screen_reader_mode: false,
            mouse_capture: true,
            run_budget: crate::run_budget::RunBudget::default(),
            sandbox: None,
            current_session_id: None,
            current_session_status: SessionStatus::Active,
            current_terminal_reason: None,
            current_session_name: String::new(),
            current_session_summary: String::new(),
            active_saved_plan_session_id: None,
            cwd,
            project_root: std::path::PathBuf::new(),
            branch,
            run_started_at: None,
            completed_run_elapsed: None,
            current_phase: None,
            active_tools: Vec::new(),
            latest_context_report: None,
            context_state: ContextModalState::default(),
            todo: Vec::new(),
            background_tasks: Vec::new(),
            subtasks: Vec::new(),
            subagent_models: std::collections::HashMap::new(),
            plan: crate::plan::PlanDoc::default(),
            task_state: TaskState::Idle,
            plan_execution: None,
            phase_advance: None,
            pending_persona_switch: None,
            tick: 0,
            sidebar_bonsai: crate::tui::widgets::bonsai::BonsaiGrowth::sprout(),
            copy_notice: None,
            session_toast: None,
            next_execution_group_id: 1,
            active_execution_group_id: None,
            transcript_scroll: 0,
            sidebar_scroll: 0,
            plan_scroll: 0,
            modal_scroll: 0,
            modal_selection: None,
            modal_body_lines: std::cell::RefCell::new(Vec::new()),
            modal_body_rect: std::cell::Cell::new(None),
            composer_scroll: 0,
            todo_focus_available: false,
            todo_phase_view: None,
            resting_todo_phase: None,
            composer_follow: true,
            pending_composer_page: None,
            pending_composer_extend: None,
            pending_question_visibility: false,
            transcript_autoscroll: true,
            transcript_seen_messages: 0,
            scroll_transcript_focus_into_view: false,
            scroll_todo_in_progress_into_view: false,
            credential_persistence: crate::session::CredentialPersistence::default(),
            first_run_step: None,
            first_run_model_selection_pending: None,
            provider_auth_form: ProviderAuthForm::default(),
            model_picker: ModelPickerState::default(),
            provider_manager_offset: std::cell::Cell::new(0),
            authorize_provider_offset: std::cell::Cell::new(0),
            completion: CompletionState::default(),
            queued_inputs: Vec::new(),
            pending_peer_inbox: 0,
            pending_peer_delivery_receipts: BTreeMap::new(),
            waiting_for_peer: None,
            deferred_commands: Vec::new(),
            next_queued_input_id: 1,
            next_deferred_command_id: 1,
            next_local_model_wizard_request_id: 1,
            command_generation: 0,
            composer: Composer::default(),
            // Optimistic default: tests and most sessions have a provider; the
            // TUI bootstrap overwrites this from the session store before the
            // first frame.
            has_authorized_provider: true,
            provider_choices: Vec::new(),
            cached_model_choices: Vec::new(),
            session_choices: Vec::new(),
            path_search: None,
        }
    }

    pub(crate) fn next_local_model_wizard_request_id(&mut self) -> u64 {
        let request_id = self.next_local_model_wizard_request_id;
        self.next_local_model_wizard_request_id =
            self.next_local_model_wizard_request_id.wrapping_add(1);
        if self.next_local_model_wizard_request_id == 0 {
            self.next_local_model_wizard_request_id = 1;
        }
        request_id
    }

    /// The generation a command spawned now must carry to still be applied. A
    /// `CommandFinished` whose generation no longer matches is dropped as stale.
    pub(crate) fn command_generation(&self) -> u64 {
        self.command_generation
    }

    pub(crate) fn pending_peer_delivery_receipts(
        &self,
    ) -> Vec<crate::storage::PeerDeliveryReceipt> {
        self.pending_peer_delivery_receipts
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn clear_peer_delivery_receipts(
        &mut self,
        acknowledged: &[crate::storage::PeerDeliveryReceipt],
    ) {
        for receipt in acknowledged {
            if self
                .pending_peer_delivery_receipts
                .get(&receipt.message_id())
                == Some(receipt)
            {
                self.pending_peer_delivery_receipts
                    .remove(&receipt.message_id());
            }
        }
    }

    /// Invalidate every in-flight command spawn (their captured generation no
    /// longer matches), so a command cancelled with Ctrl+C can't apply its
    /// result after the fact. Called on the command-cancel path.
    pub(crate) fn invalidate_pending_commands(&mut self) {
        self.command_generation = self.command_generation.wrapping_add(1);
    }

    pub(crate) fn is_task_list_open(&self) -> bool {
        matches!(
            self.modal,
            Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::TaskList { .. }
            ))
        )
    }

    pub(crate) fn is_subtask_list_open(&self) -> bool {
        matches!(
            self.modal,
            Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::SubtaskList { .. }
            ))
        )
    }

    pub(crate) fn is_peer_list_open(&self) -> bool {
        matches!(
            self.modal,
            Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::PeerList { .. }
            ))
        )
    }

    pub(crate) fn selected_background_task(
        &self,
    ) -> Option<&crate::background::BackgroundTaskSnapshot> {
        let Some(ModalKind::Manager(crate::tui::event::ManagerModal::TaskList { tasks, cursor })) =
            self.modal.as_ref()
        else {
            return None;
        };
        tasks.get((*cursor).min(tasks.len().saturating_sub(1)))
    }

    /// The built-in mode backing the active persona (a custom persona maps to a
    /// neutral built-in for view/keymap purposes).
    pub(crate) fn active_mode(&self) -> AgentMode {
        self.active_persona.builtin().unwrap_or(AgentMode::Coding)
    }

    /// The UI surface (chat / todo / canvas) the active persona renders. Drives
    /// the layout, so the view follows the agent rather than a fixed enum. For a
    /// custom persona it reads the agent's declared `view` from the live registry.
    pub(crate) fn surface(&self) -> crate::agent::PersonaView {
        match self.active_persona.custom_name() {
            Some(name) => crate::agent::PersonaView::parse(
                self.custom_agent_field(name, |def| def.view.clone())
                    .flatten()
                    .as_deref(),
            ),
            None => self.active_mode().view(),
        }
    }

    /// The active persona's display label (custom agent name, else the mode).
    pub(crate) fn persona_label(&self) -> String {
        Self::persona_label_for(&self.active_persona)
    }

    /// Display label for an arbitrary persona (custom agent name, else the mode).
    pub(crate) fn persona_label_for(persona: &crate::agent::ActivePersona) -> String {
        match persona.custom_name() {
            Some(name) => name.to_string(),
            None => {
                let mode = persona.builtin().unwrap_or(AgentMode::Coding);
                let mut chars = mode.label().chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        }
    }

    /// The active persona's accent color spec (custom agent `color:`, else the
    /// built-in mode's), if any.
    pub(crate) fn persona_color_spec(&self) -> Option<String> {
        self.persona_color_spec_for(&self.active_persona)
    }

    /// Accent color spec for an arbitrary persona (custom agent `color:`, else
    /// the built-in mode's), if any.
    pub(crate) fn persona_color_spec_for(
        &self,
        persona: &crate::agent::ActivePersona,
    ) -> Option<String> {
        match persona.custom_name() {
            Some(name) => self
                .custom_agent_field(name, |def| def.color.clone())
                .flatten(),
            None => Some(
                persona
                    .builtin()
                    .unwrap_or(AgentMode::Coding)
                    .color_spec()
                    .to_string(),
            ),
        }
    }

    /// Read a field off a custom agent def by name from the live registry.
    fn custom_agent_field<T>(
        &self,
        name: &str,
        pick: impl FnOnce(&crate::resource::agent::AgentDef) -> T,
    ) -> Option<T> {
        let registry = crate::resource::agent::snapshot(&self.custom_agents);
        registry.get(name).map(pick)
    }

    pub(crate) fn selected_subtask(&self) -> Option<&crate::subagent::SubagentSnapshot> {
        let Some(ModalKind::Manager(crate::tui::event::ManagerModal::SubtaskList {
            subtasks,
            cursor,
            ..
        })) = self.modal.as_ref()
        else {
            return None;
        };
        subtasks.get((*cursor).min(subtasks.len().saturating_sub(1)))
    }

    pub fn set_path_search(&mut self, path_search: crate::tui::path_search::PathSearch) {
        self.path_search = Some(path_search);
    }

    pub fn scroll_to_bottom_current(&mut self) {
        self.reduce(AppAction::ScrollBottom);
    }

    pub fn current_scroll(&self) -> u16 {
        self.transcript_scroll
    }

    pub fn clamp_current_scroll(&mut self, max_scroll: u16) {
        if self.transcript_autoscroll {
            self.transcript_scroll = max_scroll;
            self.transcript_seen_messages = self.assistant_message_count();
            return;
        }

        if self.transcript_scroll > max_scroll {
            self.transcript_scroll = max_scroll;
        }
        if self.transcript_scroll == max_scroll {
            self.transcript_autoscroll = true;
            self.transcript_seen_messages = self.assistant_message_count();
        }
    }

    /// Number of assistant replies currently in the transcript. The unit for
    /// the "N new messages" jump-to-latest pill: each streamed reply is a
    /// single [`TranscriptItem::AssistantMessage`], so counting them tracks
    /// turns rather than tool cards or reasoning lines.
    fn assistant_message_count(&self) -> usize {
        self.transcript.assistant_message_count()
    }

    /// How many assistant replies have arrived since the user last sat at the
    /// bottom of the transcript. Zero while auto-following (caught up) so the
    /// pill quietly disappears once the reader returns to the latest message.
    pub fn unseen_message_count(&self) -> usize {
        if self.transcript_autoscroll {
            return 0;
        }
        self.assistant_message_count()
            .saturating_sub(self.transcript_seen_messages)
    }

    pub fn clamp_sidebar_scroll(&mut self, max_scroll: u16) {
        self.sidebar_scroll = self.sidebar_scroll.min(max_scroll);
    }

    /// Phase index the todo card should display, or `None` when the plan has
    /// fewer than two phases (render the flat `app.todo` as before). Honors the
    /// user's parked browse index, else follows the active execution phase, else
    /// defaults to phase 0; always clamped in range so a stale parked index
    /// survives the plan shrinking.
    pub fn resolved_todo_phase(&self) -> Option<usize> {
        let total = self.plan.phases.len();
        if total < 2 {
            return None;
        }
        // Follow the live execution phase while a run is in flight; once it
        // clears, rest on the phase that was last active (`resting_todo_phase`)
        // rather than snapping back to Phase 1.
        let default = self
            .plan_execution
            .map(|execution| execution.phase_index)
            .or(self.resting_todo_phase)
            .unwrap_or(0);
        Some(self.todo_phase_view.unwrap_or(default).min(total - 1))
    }

    /// Clear phased-execution state, remembering the phase that was active so
    /// the todo card keeps showing it (`resting_todo_phase`) instead of snapping
    /// back to Phase 1 the moment a phased run finishes, halts, or is
    /// interrupted. A no-op stash for flat runs (no `plan_execution`).
    pub fn clear_plan_execution(&mut self) {
        if let Some(execution) = self.plan_execution.take() {
            self.resting_todo_phase = Some(execution.phase_index);
        }
    }

    pub fn clamp_plan_scroll(&mut self, max_scroll: u16) {
        self.plan_scroll = self.plan_scroll.min(max_scroll);
    }

    /// Pin the composer scroll to the auto-follow value when `composer_follow`
    /// is set; otherwise clamp to `[0, max_scroll]` so a manual offset can't
    /// push past the end of the rendered lines.
    pub fn clamp_composer_scroll(&mut self, max_scroll: u16) {
        if self.composer_follow {
            return;
        }
        self.composer_scroll = self.composer_scroll.min(max_scroll);
    }

    /// Reset the composer's scroll state to the default (auto-follow at the
    /// cursor). Called when the input is cleared or submitted so the next
    /// draft starts at the top.
    fn reset_composer_scroll(&mut self) {
        self.composer_scroll = 0;
        self.composer_follow = true;
        self.pending_composer_page = None;
        self.pending_composer_extend = None;
    }

    fn normalize_hidden_focus(&mut self) {
        if (self.focus == Focus::Plan && !matches!(self.view, View::Plan))
            || (self.focus == Focus::Todo && !matches!(self.view, View::Agent))
        {
            self.focus = Focus::Transcript;
        }
    }

    pub fn elapsed_label(&self) -> String {
        let elapsed = self
            .run_started_at
            .map(|started_at| started_at.elapsed())
            .or(self.completed_run_elapsed)
            .unwrap_or_default();

        Self::format_elapsed_label(elapsed)
    }

    pub fn mark_run_started(&mut self, started_at: Instant) {
        self.run_started_at = Some(started_at);
        self.completed_run_elapsed = None;
    }

    pub fn mark_run_finished(&mut self, finished_at: Instant) {
        if let Some(started_at) = self.run_started_at.take() {
            self.completed_run_elapsed = Some(finished_at.saturating_duration_since(started_at));
        }
    }

    pub fn clear_run_timer(&mut self) {
        self.run_started_at = None;
        self.completed_run_elapsed = None;
    }

    fn format_elapsed_label(elapsed: Duration) -> String {
        let secs = elapsed.as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else {
            format!("{}m {}s", secs / 60, secs % 60)
        }
    }

    /// Re-read the status-bar mirrors from the live agent. The mirrors are
    /// plain fields so render stays lock-free; every site that already holds
    /// the agent guard refreshes them through this single point (plus the
    /// command/turn boundaries in the event loop), so a path that flips agent
    /// state without writing a mirror goes stale for at most one turn instead
    /// of forever.
    pub(crate) fn refresh_agent_mirrors(&mut self, agent: &crate::agent::Agent) {
        self.smol_mode = agent.smol_mode();
        self.pure_mode = agent.pure_mode();
        self.self_review_mode = agent.self_review_mode();
        self.loaded_skills = agent.loaded_skills().clone();
    }

    pub fn reduce(&mut self, action: AppAction) {
        // A modal text selection is grapheme offsets into a specific modal's
        // body; when an action swaps the open modal (tool detail → diff
        // preview, finding next/prev, a picker chain) the offsets would land
        // in unrelated text, so clear it on any modal change. `CloseModal`
        // clears eagerly; this covers the ~100 direct `modal = Some(..)`
        // swap sites with one chokepoint.
        let modal_before = self.modal.as_ref().map(modal_identity);
        reducer::reduce(self, action);
        if self.modal_selection.is_some() && self.modal.as_ref().map(modal_identity) != modal_before
        {
            self.modal_selection = None;
        }
    }

    fn apply_event(&mut self, event: UiEvent) {
        match event {
            UiEvent::AssistantDelta(text) => {
                if !text.is_empty() {
                    self.push_or_append_assistant(text);
                }
            }
            UiEvent::ReasoningDelta(text) => {
                if !text.is_empty() {
                    self.push_or_append_reasoning(text);
                }
            }
            UiEvent::AttemptStarted => {
                self.attempt_stream_floor = Some(self.transcript.first_trailing_queued_index());
                self.stream_cell_break = true;
            }
            UiEvent::AttemptDiscarded => {
                self.retract_streamed_attempt();
            }
            UiEvent::AssistantDone => {
                if self.current_phase.as_deref() != Some("Reading the model response…") {
                    self.current_phase = Some("Reading the model response…".to_string());
                }
            }
            UiEvent::Thinking(text) => {
                if self.current_phase.as_deref() != Some(text.as_str()) {
                    self.current_phase = Some(text);
                }
            }
            UiEvent::ToolStarted {
                id,
                name,
                arguments,
                started_at,
            } => {
                self.current_phase = Some(self.active_phase_text());
                self.record_tool_started(id, name, arguments, started_at);
                self.maybe_scroll_to_bottom_current();
            }
            UiEvent::ToolCallsStarted { calls, started_at } => {
                self.current_phase = Some(self.active_phase_text());
                self.record_tools_started(calls, started_at);
                self.maybe_scroll_to_bottom_current();
            }
            UiEvent::ToolOutput {
                id,
                output,
                updated_at,
            } => {
                self.update_tool_output(&id, output, updated_at);
                self.recompute_active_tools();
                self.current_phase = Some(self.active_phase_text());
                self.maybe_scroll_to_bottom_current();
            }
            UiEvent::ToolFinished {
                id,
                result,
                success,
                finished_at,
            } => {
                let finished_background_bash =
                    self.tool_activity(&id).is_some_and(is_background_bash_call);
                let tool_name = self
                    .tool_activity(&id)
                    .map(|activity| activity.name.clone());
                if let Some(name) = tool_name.as_deref()
                    && (name == "set_session_title" || name == "plan_set_title")
                    && let Some(title) = session_title_from_tool_result(&result)
                {
                    // `plan_set_title` mirrors the new title to the session too;
                    // keep the TUI header in lockstep with the plan rename.
                    self.current_session_summary = title;
                }
                self.finish_tool(&id, result, success, finished_at);
                self.recompute_active_tools();
                if matches!(self.task_state, TaskState::Idle | TaskState::Exiting)
                    && finished_background_bash
                {
                    self.close_active_execution_group_if_no_running_tools();
                }
                self.current_phase = Some(self.active_phase_text());
                self.maybe_scroll_to_bottom_current();
            }
            UiEvent::ToolFinishedWithDiff {
                id,
                result,
                success,
                diff,
                finished_at,
            } => {
                self.finish_tool_with_diff(&id, result, success, *diff, finished_at);
                self.recompute_active_tools();
                self.current_phase = Some(self.active_phase_text());
                self.maybe_scroll_to_bottom_current();
            }
            UiEvent::WorkspaceChanged { .. } => {}
            UiEvent::QueuedUserMessageSent { id, text } => {
                self.mark_queued_user_message_sent(id, text);
                self.maybe_scroll_to_bottom_current();
            }
            UiEvent::ContextUpdated(report) => {
                self.latest_context_report = Some(*report);
            }
            UiEvent::TransientStatus(text) => self.set_session_toast(text),
            UiEvent::Status(text) => {
                self.close_active_execution_group();
                self.push_transcript_item(TranscriptItem::CommandOutput {
                    kind: CommandOutputKind::Status,
                    text,
                });
                self.maybe_scroll_to_bottom_current();
            }
            UiEvent::CompactionStatus(text) => {
                self.close_active_execution_group();
                self.push_or_collapse_compaction_status(text);
                self.maybe_scroll_to_bottom_current();
            }
            UiEvent::Error(text) => {
                self.close_active_execution_group();
                self.push_transcript_item(TranscriptItem::Error { error: text });
                self.maybe_scroll_to_bottom_current();
            }
            UiEvent::Interrupted => {
                self.task_state = TaskState::Idle;
                self.current_session_status = SessionStatus::Interrupted;
                self.current_phase = None;
                self.reconcile_orphaned_running_tools(Instant::now());
                self.active_tools.clear();
                self.close_active_execution_group();
            }
        }
    }

    /// Render the active-phase text for the header pill. When multiple
    /// tools are running concurrently, say so explicitly; when one is
    /// running, name it; when none, return an empty string.
    fn active_phase_text(&self) -> String {
        match self.active_tools.len() {
            0 => String::new(),
            1 => format!("Running {}", self.active_tools[0].0),
            n => format!("Running {n} tools"),
        }
    }

    fn apply_composer_click(&mut self, char_index: usize, kind: SelectionKind) {
        match kind {
            SelectionKind::Position => self.composer.move_to(char_index),
            SelectionKind::Word => self.composer.select_word_at(char_index),
            SelectionKind::Line => self.composer.select_line_at(char_index),
        }
    }

    fn apply_runtime_event(&mut self, event: RuntimeEvent) {
        match event {
            RuntimeEvent::AgentStarted => {
                self.current_session_status = SessionStatus::Active;
                self.current_terminal_reason = None;
                self.waiting_for_peer = None;
            }
            // Completion reports remain available to headless and persistence
            // consumers, but are not interactive transcript cards.
            RuntimeEvent::CompletionReport(report) => drop(report),
            RuntimeEvent::AgentFinished(result) => {
                let finished_at = Instant::now();
                let waiting_phase = match &result {
                    Ok(AgentRunOutcome::Waiting(crate::agent::WaitReason::Peer(wait))) => {
                        Some(format!("Waiting for peer #{}", wait.session_id))
                    }
                    Ok(AgentRunOutcome::Waiting(crate::agent::WaitReason::Subagents(_))) => {
                        Some("Waiting for subagents".to_string())
                    }
                    _ => None,
                };
                self.waiting_for_peer = match &result {
                    Ok(AgentRunOutcome::Waiting(crate::agent::WaitReason::Peer(wait))) => {
                        Some(*wait)
                    }
                    _ => None,
                };
                let waiting = waiting_phase.is_some();
                if !waiting {
                    self.mark_run_finished(finished_at);
                    // The turn is over: reconcile any tool whose completion
                    // event never landed so its spinner and group timer stop.
                    // Skipped while waiting — subagents/peers are legitimately
                    // still Running and must keep their spinners.
                    self.reconcile_orphaned_running_tools(finished_at);
                }
                // The event itself says whether the run was interrupted — the
                // old task_state heuristic raced with `UiEvent::Interrupted`
                // (which resets Cancelling → Idle before this event lands),
                // making a Ctrl+C'd phase read as a success and auto-advance.
                // Keep the state check as a backstop for cancels the run
                // couldn't observe before finishing.
                let interrupted = matches!(
                    &result,
                    Ok(AgentRunOutcome::Interrupted | AgentRunOutcome::BudgetExhausted(_))
                ) || matches!(self.task_state, TaskState::Cancelling);
                let run_failed = result.is_err() || matches!(&result, Ok(AgentRunOutcome::Failed));
                // BudgetExhausted is treated as success for phase advancement —
                // the agent completed its work but hit the turn/time limit.
                // Only a real interrupt (Ctrl+C) or actual failure halts phases.
                let phase_interrupted = matches!(self.task_state, TaskState::Cancelling)
                    || matches!(&result, Ok(AgentRunOutcome::Interrupted));
                // The coding agent confirmed a plan-mode switch: hand the mode
                // to the event loop, which swaps persona and re-dispatches a
                // continuation. Ignored if the run was interrupted before the
                // switch could take effect. Computed here while `result` is
                // still borrowable (the `match result` below moves it).
                self.pending_persona_switch =
                    match &result {
                        Ok(AgentRunOutcome::Waiting(crate::agent::WaitReason::PersonaSwitch(
                            mode,
                        ))) if !interrupted => Some(*mode),
                        _ => None,
                    };
                self.current_terminal_reason = match &result {
                    Ok(AgentRunOutcome::BudgetExhausted(reason)) => Some(*reason),
                    _ => None,
                };
                self.current_session_status = if waiting {
                    SessionStatus::Active
                } else if run_failed {
                    SessionStatus::Failed
                } else if interrupted {
                    SessionStatus::Interrupted
                } else {
                    SessionStatus::Completed
                };
                self.task_state = TaskState::Idle;
                let keep_background_group =
                    self.active_execution_group_id.is_some_and(|group_id| {
                        self.execution_group(group_id)
                            .is_some_and(|group| group.tools.iter().any(is_running_background_bash))
                    });
                if let Some(group_id) = self.active_execution_group_id
                    && !keep_background_group
                    && let Some(group) = self.execution_group_mut(group_id)
                {
                    group.finished_at = Some(finished_at);
                    self.active_execution_group_id = None;
                }
                if keep_background_group {
                    self.recompute_active_tools();
                    self.current_phase = Some(self.active_phase_text());
                } else {
                    self.active_tools.clear();
                    self.current_phase = waiting_phase;
                }
                match result {
                    Err(err) => {
                        self.push_transcript_item(TranscriptItem::Error { error: err });
                        self.maybe_scroll_to_bottom_current();
                    }
                    Ok(AgentRunOutcome::BudgetExhausted(reason)) => {
                        self.push_transcript_item(TranscriptItem::CommandOutput {
                            kind: CommandOutputKind::Status,
                            text: format!(
                                "Budget exhausted: {reason}. Partial work and session state were preserved."
                            ),
                        });
                        self.maybe_scroll_to_bottom_current();
                    }
                    Ok(AgentRunOutcome::Interrupted) => {
                        self.push_transcript_item(TranscriptItem::CommandOutput {
                            kind: CommandOutputKind::Status,
                            text: "Run interrupted.".to_string(),
                        });
                        self.maybe_scroll_to_bottom_current();
                    }
                    Ok(
                        AgentRunOutcome::Completed
                        | AgentRunOutcome::Failed
                        | AgentRunOutcome::Waiting(_),
                    ) => {}
                }
                // Signal the event loop to advance (or halt) a phased run. Only
                // meaningful while a phased plan is executing; flat runs leave
                // this `None` and behave exactly as before.
                if self.plan_execution.is_some() && !waiting {
                    self.phase_advance = Some(if run_failed || phase_interrupted {
                        PhaseAdvance::Halt
                    } else {
                        PhaseAdvance::Continue
                    });
                }
            }
            RuntimeEvent::CommandFinished(event) => match *event {
                CommandOutcomeEvent::Applied {
                    generation,
                    clear_transcript,
                    messages,
                    provider,
                    context_report,
                    quit,
                    open_modal,
                } => {
                    // A generation-tagged command that was cancelled (which bumped
                    // the generation) must not apply its stale result after the
                    // UI returned to idle — drop it.
                    if generation.is_some_and(|generation| generation != self.command_generation) {
                        return;
                    }
                    self.task_state = if quit {
                        TaskState::Exiting
                    } else {
                        TaskState::Idle
                    };
                    if clear_transcript {
                        self.transcript.clear();
                        self.transcript_focus = None;
                        self.transcript_selection = None;
                        self.active_group_tool_selection = None;
                        self.expanded_execution_groups.clear();
                        self.active_execution_group_id = None;
                        self.active_tools.clear();
                        self.reset_next_execution_group_id();
                        self.scroll_transcript_focus_into_view = false;
                    }
                    for message in messages {
                        self.push_transcript_item(TranscriptItem::CommandOutput {
                            kind: message.kind,
                            text: message.text,
                        });
                    }
                    if let Some(selection) = provider {
                        let selection = *selection;
                        self.provider = selection.provider;
                        self.model = selection.model;
                        self.reasoning = selection.reasoning;
                        // Only an authorized provider can be selected, so the
                        // welcome screen can drop its authorize-first guidance.
                        self.has_authorized_provider = true;
                    }
                    if let Some(report) = context_report {
                        self.latest_context_report = Some(*report);
                    }
                    if let Some(modal) = open_modal {
                        self.reduce(AppAction::OpenModal(modal));
                    }
                    self.maybe_scroll_to_bottom_current();
                }
            },
            RuntimeEvent::PersonaModelApplied(selection) => {
                let selection = *selection;
                self.provider = selection.provider;
                self.model = selection.model;
                self.reasoning = selection.reasoning;
            }
            RuntimeEvent::LocalModelWizardFetchFinished { request_id, result } => {
                if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard {
                    state,
                })) = &mut self.modal
                {
                    if state.active_fetch_request_id != Some(request_id) {
                        return;
                    }
                    match result {
                        Ok(outcome) => state.apply_fetch_success(outcome),
                        Err(err) => state.apply_fetch_error(err.detail),
                    }
                }
            }
            RuntimeEvent::AgentComposerPromptGenerated { request_id, result } => {
                if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::AgentComposer {
                    state,
                })) = &mut self.modal
                {
                    if state.active_request_id != Some(request_id) {
                        return;
                    }
                    match result {
                        Ok(prompt) => state.apply_generated(prompt),
                        Err(err) => state.apply_generate_error(err.detail),
                    }
                }
            }
            RuntimeEvent::CatalogReloaded { outcome, .. } => {
                self.apply_runtime_event(RuntimeEvent::CommandFinished(outcome));
            }
            RuntimeEvent::BackgroundTaskRemovalFinished { task_id, error } => {
                let status = if let Some(error) = error {
                    format!("Failed to remove {task_id}: {}", error.detail)
                } else {
                    format!("Removed {task_id}.")
                };
                self.task_list_status = Some(status);
            }
            // Applied in the event loop (it holds the agent lock to rebuild the
            // system message); never routed through the reducer.
            RuntimeEvent::RepoMapReady(_) => {}
            // Intercepted by the event loop (rendered via `AppAction::PeerMessage`).
            RuntimeEvent::PeerMessagesArrived(_) => {}
            // Intercepted by the event loop (refreshes the `/peers` view via
            // `AppAction::RefreshPeerList`); nothing reaches the reducer.
            RuntimeEvent::PeersChanged(_) => {}
            // Intercepted by the event loop (updates the composer badge via
            // `AppAction::PeerInboxChanged`); nothing reaches the reducer.
            RuntimeEvent::PeerInboxChanged(_) => {}
            RuntimeEvent::RefreshSourceCompleted {
                generation,
                index,
                source,
            } => {
                self.reduce(AppAction::RefreshSourceUpdate {
                    generation,
                    index,
                    source,
                });
            }
            RuntimeEvent::RefreshAllSourcesCompleted { generation } => {
                self.reduce(AppAction::RefreshFinished { generation });
            }
            RuntimeEvent::BranchChanged(branch) => {
                self.branch = branch;
            }
            RuntimeEvent::UpdateNotice(notice) => {
                self.update_notice = Some(notice);
            }
            RuntimeEvent::UpdateCommandFinished {
                message,
                kind,
                staged_notice,
            } => {
                if let Some(notice) = staged_notice {
                    self.update_notice = Some(notice);
                }
                if matches!(kind, CommandOutputKind::Error) {
                    self.push_transcript_item(TranscriptItem::CommandOutput {
                        kind,
                        text: message,
                    });
                    self.maybe_scroll_to_bottom_current();
                } else {
                    self.set_session_toast(message);
                }
            }
            RuntimeEvent::TaskPanicked(text) => {
                self.mark_run_finished(Instant::now());
                self.task_state = TaskState::Idle;
                // A panic skips the normal AgentFinished path, so halt any phased
                // execution here too — otherwise a stale `plan_execution` could
                // arm phase_advance on a later unrelated run and hijack it into a
                // phase advance.
                self.clear_plan_execution();
                self.phase_advance = None;
                self.push_transcript_item(TranscriptItem::Error { error: text });
                self.maybe_scroll_to_bottom_current();
            }
        }
    }

    fn push_or_append_assistant(&mut self, text: String) {
        self.close_active_execution_group();
        if !std::mem::take(&mut self.stream_cell_break)
            && let Some(index) = self.transcript.first_trailing_queued_index().checked_sub(1)
            && let Some(TranscriptItem::AssistantMessage { text: last_text }) =
                self.transcript.get_mut(index)
        {
            last_text.push_str(&text);
            self.maybe_scroll_to_bottom_current();
            return;
        }
        self.push_transcript_item(TranscriptItem::AssistantMessage { text });
        self.maybe_scroll_to_bottom_current();
    }

    fn push_or_append_reasoning(&mut self, text: String) {
        self.close_active_execution_group();
        if !std::mem::take(&mut self.stream_cell_break)
            && let Some(index) = self.transcript.first_trailing_queued_index().checked_sub(1)
            && let Some(TranscriptItem::ReasoningSummary { text: last_text }) =
                self.transcript.get_mut(index)
        {
            last_text.push_str(&text);
            self.maybe_scroll_to_bottom_current();
            return;
        }
        self.push_transcript_item(TranscriptItem::ReasoningSummary { text });
        self.maybe_scroll_to_bottom_current();
    }

    /// Withdraw the streamed cells of an abandoned provider attempt (failed
    /// and about to be retried, generation budget, or terminal provider
    /// error). The floor recorded at `AttemptStarted` bounds the removal so
    /// committed cells from earlier calls are never touched — a plain
    /// walk-from-the-end could eat an answer flushed by a blank-turn
    /// continue. Within the floor region only stream cell types are removed;
    /// status rows or tool cards that landed mid-attempt stay.
    fn retract_streamed_attempt(&mut self) {
        let Some(floor) = self.attempt_stream_floor.take() else {
            return;
        };
        self.stream_cell_break = false;
        let mut index = self.transcript.first_trailing_queued_index();
        let mut removed = 0usize;
        let mut focus = self.transcript_focus;
        let mut focus_lost = false;
        while index > floor {
            index -= 1;
            if matches!(
                self.transcript.get(index),
                Some(
                    TranscriptItem::AssistantMessage { .. }
                        | TranscriptItem::ReasoningSummary { .. }
                )
            ) {
                self.transcript.remove(index);
                removed += 1;
                match &mut focus {
                    Some(f) if *f == index => focus_lost = true,
                    Some(f) if *f > index => *f -= 1,
                    _ => {}
                }
            }
        }
        if removed == 0 {
            return;
        }
        if focus_lost {
            focus = if self.transcript.is_empty() {
                None
            } else {
                Some(floor.min(self.transcript.len() - 1))
            };
        }
        self.transcript_focus = focus;
        self.transcript_selection = None;
        self.maybe_scroll_to_bottom_current();
    }

    /// New content arrived. Autoscroll (when enabled) is applied at draw time
    /// by `clamp_current_scroll`, so this only exists to document intent at
    /// the call sites; a manual scroll position is deliberately left alone.
    fn maybe_scroll_to_bottom_current(&mut self) {}

    pub fn transcript_autoscroll_enabled(&self) -> bool {
        self.transcript_autoscroll
    }
}

fn is_running_background_bash(activity: &ToolActivity) -> bool {
    matches!(activity.status, ToolStatus::Running) && is_background_bash_call(activity)
}

fn is_background_bash_call(activity: &ToolActivity) -> bool {
    if activity.name != "bash" {
        return false;
    }
    let Ok(serde_json::Value::Object(map)) =
        serde_json::from_str::<serde_json::Value>(&activity.arguments)
    else {
        return false;
    };
    map.get("run_in_background")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
        || map
            .get("interactive")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

fn session_title_from_tool_result(result: &str) -> Option<String> {
    // Both `set_session_title` and `plan_set_title` (which mirrors to the
    // active session) emit one of these prefixes; accept either here.
    let title = result
        .strip_prefix("Session title set to: ")
        .or_else(|| result.strip_prefix("Plan title set to: "))?
        .trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentMode;
    use crate::background::{BackgroundTaskSnapshot, BackgroundTaskStatus};
    use crate::provider::{ReasoningEffort, ReasoningSelection};
    use crate::tui::event::{ModeAxisId, ModeRow, SubtaskListPane};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn app() -> AppState {
        AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        )
    }

    #[test]
    fn toggle_mouse_capture_flips_flag_and_notifies() {
        let mut app = app();
        assert!(app.mouse_capture, "capture is on by default");

        app.reduce(AppAction::ToggleMouseCapture);
        assert!(!app.mouse_capture, "toggle releases capture");
        let notice = app.copy_notice.as_ref().expect("a notice is shown");
        assert!(
            notice.text.contains("select"),
            "off notice mentions selection"
        );

        app.reduce(AppAction::ToggleMouseCapture);
        assert!(app.mouse_capture, "toggle again restores capture");
        assert!(
            app.copy_notice
                .as_ref()
                .is_some_and(|notice| notice.text.contains("on")),
            "on notice confirms restore"
        );
    }

    #[test]
    fn elapsed_label_freezes_when_agent_finishes() {
        let mut app = app();
        app.task_state = TaskState::Running;
        app.mark_run_started(Instant::now() - Duration::from_secs(65));

        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));

        assert_eq!(app.task_state, TaskState::Idle);
        assert!(app.run_started_at.is_none());
        assert_eq!(app.elapsed_label(), "1m 5s");
    }

    #[test]
    fn agent_finish_reconciles_orphaned_running_tool() {
        // A subagent tool card whose `ToolFinished` never landed: it has a
        // recorded result but is still `Running`. Finishing the turn must flip
        // it to a terminal state and stamp `finished_at` so the spinner stops
        // and the group timer freezes instead of counting to `Instant::now()`.
        let mut app = app();
        app.task_state = TaskState::Running;
        app.mark_run_started(Instant::now());
        let started_at = Instant::now();
        app.record_tool_started(
            "self-review-1".to_string(),
            "agent".to_string(),
            r#"{"agent":"self-review"}"#.to_string(),
            started_at,
        );
        app.update_tool_output("self-review-1", "review complete".to_string(), started_at);
        assert!(matches!(
            app.tool_activity("self-review-1").map(|a| a.status),
            Some(ToolStatus::Running)
        ));

        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));

        let activity = app.tool_activity("self-review-1").expect("tool present");
        // Result was present -> treated as succeeded, and the timer is frozen.
        assert_eq!(activity.status, ToolStatus::Succeeded);
        assert!(activity.finished_at.is_some());
        assert!(app.active_tools.is_empty());
    }

    #[test]
    fn agent_finish_leaves_background_bash_running() {
        // Background bash is meant to outlive the turn — the reconciler must not
        // terminate it, or its still-live output card would read as finished.
        let mut app = app();
        app.task_state = TaskState::Running;
        app.mark_run_started(Instant::now());
        app.record_tool_started(
            "bash-1".to_string(),
            "bash".to_string(),
            r#"{"command":"sleep 100","run_in_background":true}"#.to_string(),
            Instant::now(),
        );

        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));

        let activity = app.tool_activity("bash-1").expect("tool present");
        assert_eq!(activity.status, ToolStatus::Running);
        assert!(activity.finished_at.is_none());
    }

    #[test]
    fn peer_wait_keeps_session_active_and_names_target() {
        let mut app = app();
        app.task_state = TaskState::Running;
        app.mark_run_started(Instant::now());

        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Waiting(crate::agent::WaitReason::Peer(crate::agent::PeerWait {
                session_id: SessionId::from_raw(45),
                subscription_id: 7,
            })),
        ))));

        assert_eq!(app.task_state, TaskState::Idle);
        assert_eq!(app.current_session_status, SessionStatus::Active);
        assert_eq!(app.current_phase.as_deref(), Some("Waiting for peer #45"));
        assert_eq!(
            app.waiting_for_peer,
            Some(crate::agent::PeerWait {
                session_id: SessionId::from_raw(45),
                subscription_id: 7,
            })
        );
        assert!(app.run_started_at.is_some());

        // The park's structured state clears when a new run starts…
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentStarted));
        assert_eq!(app.waiting_for_peer, None);

        // …and never survives a non-waiting finish.
        app.waiting_for_peer = Some(crate::agent::PeerWait {
            session_id: SessionId::from_raw(45),
            subscription_id: 7,
        });
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));
        assert_eq!(app.waiting_for_peer, None);
    }

    #[test]
    fn starting_run_clears_previous_elapsed_label() {
        let mut app = app();
        app.completed_run_elapsed = Some(Duration::from_secs(65));

        app.mark_run_started(Instant::now());

        assert!(app.completed_run_elapsed.is_none());
        assert_eq!(app.elapsed_label(), "0s");
    }

    #[test]
    fn resolved_todo_phase_follows_active_then_parked_then_clamps() {
        use crate::plan::{PlanDoc, PlanPhase};
        let phase = |name: &str| PlanPhase {
            name: name.to_string(),
            tasks: Vec::new(),
        };
        let mut app = app();

        // Flat plan -> None.
        assert_eq!(app.resolved_todo_phase(), None);

        // A single phase is treated as flat for the todo view.
        app.plan = PlanDoc {
            phases: vec![phase("only")],
            ..Default::default()
        };
        assert_eq!(app.resolved_todo_phase(), None);

        // Multi-phase, idle -> defaults to phase 0.
        app.plan = PlanDoc {
            phases: vec![phase("a"), phase("b"), phase("c")],
            ..Default::default()
        };
        assert_eq!(app.resolved_todo_phase(), Some(0));

        // Follows the active execution phase.
        app.plan_execution = Some(PlanExecution { phase_index: 2 });
        assert_eq!(app.resolved_todo_phase(), Some(2));

        // A parked browse index wins over the active phase.
        app.todo_phase_view = Some(1);
        assert_eq!(app.resolved_todo_phase(), Some(1));

        // A stale parked index is clamped when the plan shrinks.
        app.todo_phase_view = Some(9);
        assert_eq!(app.resolved_todo_phase(), Some(2));
    }

    #[test]
    fn resolved_todo_phase_rests_on_last_active_after_execution_clears() {
        use crate::plan::{PlanDoc, PlanPhase};
        let phase = |name: &str| PlanPhase {
            name: name.to_string(),
            tasks: Vec::new(),
        };
        let mut app = app();
        app.plan = PlanDoc {
            phases: vec![phase("a"), phase("b"), phase("c")],
            ..Default::default()
        };

        // A run finishes on phase 2 (done / halted / interrupted): the card
        // rests on that phase instead of snapping back to phase 0.
        app.plan_execution = Some(PlanExecution { phase_index: 2 });
        app.clear_plan_execution();
        assert_eq!(app.plan_execution, None);
        assert_eq!(app.resting_todo_phase, Some(2));
        assert_eq!(app.resolved_todo_phase(), Some(2));

        // The user can still browse away from the resting phase.
        app.todo_phase_view = Some(0);
        assert_eq!(app.resolved_todo_phase(), Some(0));

        // A fresh phased run drops the resting phase and follows its own.
        app.todo_phase_view = None;
        app.resting_todo_phase = None;
        app.plan_execution = Some(PlanExecution { phase_index: 1 });
        assert_eq!(app.resolved_todo_phase(), Some(1));

        // Clearing with no execution in flight is a no-op stash (flat run).
        app.clear_plan_execution();
        app.resting_todo_phase = None;
        app.clear_plan_execution();
        assert_eq!(app.resting_todo_phase, None);
    }

    fn background_task(id: &str, status: BackgroundTaskStatus) -> BackgroundTaskSnapshot {
        BackgroundTaskSnapshot {
            id: id.to_string(),
            command: format!("printf {id}"),
            cwd: PathBuf::from("/tmp/project"),
            status,
            started_at: SystemTime::now(),
            finished_at: None,
            exit_code: None,
            timeout_secs: 30,
            timed_out: false,
            tail: String::new(),
            tail_truncated: false,
            total_output_chars: 0,
            tool_call_id: None,
        }
    }

    fn context_report(tokens: usize) -> crate::agent::ContextReport {
        crate::agent::ContextReport {
            budget_tokens: 120_000,
            entries: vec![crate::agent::ContextEntry {
                role: crate::agent::ContextRole::User,
                tokens,
                text: "hello".to_string(),
            }],
            last_prompt_tokens: Some(100),
            last_completion_tokens: Some(25),
            session_prompt_tokens: 100,
            session_completion_tokens: 25,
            ..Default::default()
        }
    }

    fn context_report_with_wire_rows() -> crate::agent::ContextReport {
        crate::agent::ContextReport {
            payload_preview: Some(crate::provider::ProviderRequestPreview::with_wire_sections(
                "POST",
                "/v1/messages",
                serde_json::json!({"system": "prompt", "messages": []}),
                vec![
                    crate::provider::ProviderWireSection::from_value(
                        "wire-messages",
                        "Messages",
                        "$.messages",
                        &serde_json::json!([]),
                        None,
                    ),
                    crate::provider::ProviderWireSection::from_value(
                        "wire-system",
                        "System",
                        "$.system",
                        &serde_json::json!("prompt"),
                        None,
                    ),
                ],
            )),
            ..context_report(42)
        }
    }

    fn usage_turn_report(seq: usize) -> crate::agent::UsageTurnReport {
        crate::agent::UsageTurnReport {
            seq,
            lane_kind: crate::agent::ExecutionLaneKind::Parent,
            lane_id: "parent-42".to_string(),
            lane_seq: seq,
            parent_tool_call_id: None,
            launch_group_id: None,
            status: crate::agent::UsageTurnStatus::Reported,
            finish_reason: None,
            reasoning_chars: 0,
            provider_attempts: Vec::new(),
            provider_id: None,
            model: None,
            effective_reasoning: None,
            prompt_tokens: Some(1_000),
            completion_tokens: Some(100),
            cache_read_input_tokens: Some(900),
            cache_creation_input_tokens: Some(0),
            cache_measured_input_tokens: Some(1_000),
            turn_cost_micros: Some(100),
            no_cache_cost_micros: Some(200),
            estimated_prompt_tokens: Some(1_000),
            estimate_source: None,
            estimate_confidence: None,
            tool_schema_tokens: None,
            tool_schema_hash: None,
            tool_schema_names: Vec::new(),
            request_body_bytes: None,
            request_body_hash: None,
            cache_mechanism: None,
            cache_route_fingerprint: None,
            expected_cacheable_percent: None,
            actual_cache_read_percent: Some(90),
            local_reusable_prefix_tokens: Some(900),
            local_reusable_prefix_percent: Some(90),
            cacheable_prefix_tokens: None,
            volatile_tail_tokens: None,
            context_window_tokens: None,
            rewrite_kind: crate::agent::ContextRewriteKind::None,
            rewrite_saved_tokens: None,
            episode_seq: None,
            created_at_ms: 1_700_000_000_000 + seq as i64 * 60_000,
            latency_ms: Some(2_000),
            ttft_ms: Some(400),
            prefix_hash: Some("aaaa1111".to_string()),
            inspection_executed: 0,
            inspection_reused: 0,
            inspection_rejected: 0,
            inspection_returned_chars: 0,
            inspection_avoided_chars: 0,
            delegated_parent_overlap: 0,
        }
    }

    fn context_report_with_usage_turns() -> crate::agent::ContextReport {
        crate::agent::ContextReport {
            usage_turns: vec![
                usage_turn_report(1),
                usage_turn_report(2),
                usage_turn_report(3),
            ],
            ..context_report(42)
        }
    }

    fn authorize_provider_entry(
        provider_id: &str,
        provider_label: &str,
    ) -> crate::tui::pickers::ProviderOption {
        crate::tui::pickers::ProviderOption {
            provider_id: provider_id.to_string(),
            provider_label: provider_label.to_string(),
            authorized: false,
            current: false,
            uses_endpoint_auth_form: false,
        }
    }

    fn tool_activity(id: &str, name: &str) -> ToolActivity {
        ToolActivity {
            id: id.to_string(),
            name: name.to_string(),
            arguments: "{}".to_string(),
            status: ToolStatus::Succeeded,
            result: Some("ok".to_string()),
            diff: None,
            started_at: Instant::now(),
            finished_at: Some(Instant::now()),
        }
    }

    #[test]
    fn fresh_app_starts_with_input_focus() {
        let app = app();
        assert_eq!(app.focus, Focus::Input, "startup must focus the composer");
    }

    #[test]
    fn context_updated_event_stores_latest_report() {
        let mut app = app();
        let report = context_report(42);

        app.reduce(AppAction::Agent(UiEvent::ContextUpdated(Box::new(
            report.clone(),
        ))));

        assert_eq!(app.latest_context_report, Some(report));
    }

    #[test]
    fn open_context_modal_uses_stored_report() {
        let mut app = app();
        let report = context_report(42);
        app.focus = Focus::Todo;
        app.reduce(AppAction::Agent(UiEvent::ContextUpdated(Box::new(
            report.clone(),
        ))));

        app.reduce(AppAction::OpenContextModal);

        assert_eq!(app.focus, Focus::Modal);
        assert!(
            matches!(app.modal, Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(ref modal_report))) if modal_report.as_ref() == &report)
        );
        assert_eq!(app.modal_return_focus, Some(Focus::Todo));
        assert_eq!(app.context_state.view_mode, ContextViewMode::Ledger);
    }

    #[test]
    fn open_episodes_modal_uses_stored_report() {
        let mut app = app();
        let report = context_report(42);
        app.focus = Focus::Todo;
        app.latest_context_report = Some(report.clone());

        app.reduce(AppAction::OpenEpisodesModal);

        assert_eq!(app.focus, Focus::Modal);
        assert!(matches!(
            app.modal,
            Some(ModalKind::Detail(crate::tui::event::DetailModal::Episodes {
                report: ref modal_report,
                cursor: 0,
            })) if modal_report.as_ref() == &report
        ));
        assert_eq!(app.modal_return_focus, Some(Focus::Todo));
    }

    #[test]
    fn open_context_preview_modal_does_not_replace_latest_report() {
        let mut app = app();
        let stored = context_report(42);
        let preview = context_report(84);
        app.focus = Focus::Todo;
        app.latest_context_report = Some(stored.clone());

        app.reduce(AppAction::OpenContextPreviewModal(preview.clone()));

        assert_eq!(app.latest_context_report, Some(stored));
        assert!(
            matches!(app.modal, Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(ref modal_report))) if modal_report.as_ref() == &preview)
        );
        assert_eq!(app.modal_return_focus, Some(Focus::Todo));
        assert_eq!(app.context_state.view_mode, ContextViewMode::Ledger);
    }

    #[test]
    fn modal_double_click_selects_and_copies_word() {
        let mut app = app();
        let (clipboard, copied) = crate::copy::test_support::fake_clipboard();
        app.set_clipboard(clipboard);
        app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Help));
        *app.modal_body_lines.borrow_mut() = vec![Line::from("hello world")];

        app.reduce(AppAction::ModalClick {
            offset: 7,
            kind: SelectionKind::Word,
            column: 5,
            row: 5,
        });

        assert_eq!(
            app.modal_selection,
            Some(ModalSelection {
                anchor: 6,
                caret: 11
            })
        );
        assert_eq!(copied.lock().unwrap().as_deref(), Some("world"));
    }

    #[test]
    fn modal_drag_release_copies_selected_range() {
        let mut app = app();
        let (clipboard, copied) = crate::copy::test_support::fake_clipboard();
        app.set_clipboard(clipboard);
        app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Help));
        *app.modal_body_lines.borrow_mut() = vec![Line::from("abc"), Line::from("def")];

        app.reduce(AppAction::ModalClick {
            offset: 1,
            kind: SelectionKind::Position,
            column: 1,
            row: 0,
        });
        app.reduce(AppAction::ModalDrag { offset: 5 });
        app.reduce(AppAction::PointerSelectionEnd);

        // "abc\ndef" graphemes 1..5 span the line break: "bc\nd".
        assert_eq!(copied.lock().unwrap().as_deref(), Some("bc\nd"));
        assert!(!app.pointer_selecting);
    }

    #[test]
    fn modal_swap_clears_text_selection() {
        let mut app = app();
        app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Help));
        app.modal_selection = Some(ModalSelection {
            anchor: 0,
            caret: 3,
        });
        app.latest_context_report = Some(context_report(42));

        app.reduce(AppAction::OpenContextModal);

        assert!(
            app.modal_selection.is_none(),
            "a selection must not survive into a different modal's body"
        );
    }

    #[test]
    fn context_toggle_view_cycles_modes_and_resets_scroll() {
        let mut app = app();
        app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
            Box::new(context_report(42)),
        )));
        app.context_state.view_mode = ContextViewMode::Ledger;
        app.modal_scroll = 12;
        app.context_state.manual_scroll = true;

        app.reduce(AppAction::ContextToggleView);

        assert_eq!(app.context_state.view_mode, ContextViewMode::Wire);
        assert_eq!(app.modal_scroll, 0);
        assert!(!app.context_state.manual_scroll);

        app.reduce(AppAction::ScrollModal(5));
        app.reduce(AppAction::ContextToggleView);

        assert_eq!(app.context_state.view_mode, ContextViewMode::Turns);
        assert_eq!(app.modal_scroll, 0);

        app.reduce(AppAction::ContextToggleView);

        assert_eq!(app.context_state.view_mode, ContextViewMode::Ledger);
    }

    #[test]
    fn context_wire_view_moves_and_expands_wire_rows_only() {
        let mut app = app();
        app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
            Box::new(context_report_with_wire_rows()),
        )));
        app.context_state.view_mode = ContextViewMode::Wire;
        app.context_state.cursor = 3;
        app.context_state.wire_cursor = 0;

        app.reduce(AppAction::ContextMove(1));

        assert_eq!(app.context_state.cursor, 3);
        assert_eq!(app.context_state.wire_cursor, 1);

        app.reduce(AppAction::ContextToggleSelected);

        assert_eq!(app.context_state.cursor, 3);
        assert!(app.context_state.wire_expanded.contains("wire-system"));
    }

    #[test]
    fn context_turns_view_moves_and_expands_turn_details_only() {
        let mut app = app();
        app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
            Box::new(context_report_with_usage_turns()),
        )));
        app.context_state.view_mode = ContextViewMode::Turns;
        app.context_state.cursor = 3;

        app.reduce(AppAction::ContextMove(1));

        // The ledger cursor is untouched; the turns cursor moves.
        assert_eq!(app.context_state.cursor, 3);
        assert_eq!(app.context_state.turns_cursor, 1);

        // Toggle expands the selected turn (keyed by seq), and toggles back.
        app.reduce(AppAction::ContextToggleSelected);
        assert!(app.context_state.turns_expanded.contains(&2));
        app.reduce(AppAction::ContextToggleSelected);
        assert!(!app.context_state.turns_expanded.contains(&2));
    }

    #[test]
    fn refresh_context_modal_clamps_turns_cursor() {
        let mut app = app();
        app.modal = Some(ModalKind::Detail(crate::tui::event::DetailModal::Context(
            Box::new(context_report_with_usage_turns()),
        )));
        app.context_state.view_mode = ContextViewMode::Turns;
        app.reduce(AppAction::ContextMove(2));
        assert_eq!(app.context_state.turns_cursor, 2);

        let mut refreshed = context_report(42);
        refreshed.usage_turns = vec![usage_turn_report(1)];
        app.reduce(AppAction::RefreshContextModal(refreshed));

        assert_eq!(app.context_state.turns_cursor, 0);
    }

    #[test]
    fn task_list_modal_opens_from_latest_snapshot() {
        let mut app = app();
        app.reduce(AppAction::RefreshTaskList {
            tasks: vec![background_task("bg-1", BackgroundTaskStatus::Running)],
        });

        app.reduce(AppAction::OpenTaskList);

        assert!(matches!(
            app.modal,
            Some(ModalKind::Manager(crate::tui::event::ManagerModal::TaskList {
                ref tasks,
                cursor: 0,
            })) if tasks[0].id == "bg-1"
        ));
        assert_eq!(app.focus, Focus::Modal);
    }

    #[test]
    fn peer_inbox_changed_updates_badge_count() {
        let mut app = app();
        assert_eq!(app.pending_peer_inbox, 0);

        app.reduce(AppAction::PeerInboxChanged { count: 3 });
        assert_eq!(app.pending_peer_inbox, 3);

        // Draining on the next turn brings it back to zero.
        app.reduce(AppAction::PeerInboxChanged { count: 0 });
        assert_eq!(app.pending_peer_inbox, 0);
    }

    #[test]
    fn refresh_modal_updates_sources_and_closes() {
        use crate::commands::{RefreshSourceState, RefreshSourceStatus};

        let mut app = app();
        let sources = vec![
            RefreshSourceState::pending("Models.dev"),
            RefreshSourceState::pending("Anthropic"),
        ];
        app.reduce(AppAction::OpenModal(ModalKind::Detail(
            crate::tui::event::DetailModal::Refresh {
                sources,
                cursor: 0,
                generation: 1,
            },
        )));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Detail(crate::tui::event::DetailModal::Refresh { .. })) if app.focus == Focus::Modal
        ));

        // A source completes — the row flips from Pending to Ok.
        app.reduce(AppAction::RefreshSourceUpdate {
            generation: 1,
            index: 0,
            source: RefreshSourceState {
                display_name: "Models.dev".to_string(),
                status: RefreshSourceStatus::Ok,
                model_count: Some(300),
                added: vec!["new-model".to_string()],
                removed: vec![],
            },
        });
        if let Some(ModalKind::Detail(crate::tui::event::DetailModal::Refresh {
            sources, ..
        })) = &app.modal
        {
            assert_eq!(sources[0].status, RefreshSourceStatus::Ok);
            assert_eq!(sources[0].model_count, Some(300));
            assert_eq!(sources[1].status, RefreshSourceStatus::Pending);
        }

        // A stale-generation event is dropped.
        app.reduce(AppAction::RefreshSourceUpdate {
            generation: 999,
            index: 1,
            source: RefreshSourceState {
                display_name: "Anthropic".to_string(),
                status: RefreshSourceStatus::Ok,
                model_count: Some(50),
                added: vec![],
                removed: vec![],
            },
        });
        if let Some(ModalKind::Detail(crate::tui::event::DetailModal::Refresh {
            sources, ..
        })) = &app.modal
        {
            // Still pending — stale event was ignored.
            assert_eq!(sources[1].status, RefreshSourceStatus::Pending);
        }

        // Move the cursor.
        app.reduce(AppAction::RefreshMove(1));
        if let Some(ModalKind::Detail(crate::tui::event::DetailModal::Refresh { cursor, .. })) =
            &app.modal
        {
            assert_eq!(*cursor, 1);
        }

        // Close the modal.
        app.reduce(AppAction::RefreshClose);
        assert!(app.modal.is_none());
        assert_eq!(app.focus, Focus::Input);
    }

    fn subtask_snapshot(
        id: &str,
        status: crate::subagent::SubagentStatus,
    ) -> crate::subagent::SubagentSnapshot {
        crate::subagent::SubagentSnapshot {
            id: id.into(),
            agent: "explore".into(),
            prompt: "find the thing".into(),
            detached: false,
            status,
            started_at: SystemTime::now(),
            finished_at: None,
            activity: String::new().into(),
            result: None,
            model: None,
            tool_call_id: None,
            launch_group_id: None,
        }
    }

    #[test]
    fn subtask_list_modal_opens_and_moves_selection() {
        use crate::subagent::SubagentStatus;
        let mut app = app();
        app.reduce(AppAction::RefreshSubtaskList {
            subtasks: vec![
                subtask_snapshot("sub-1", SubagentStatus::Running),
                subtask_snapshot("sub-2", SubagentStatus::Succeeded),
            ],
        });
        app.reduce(AppAction::OpenSubtaskList);
        assert_eq!(app.focus, Focus::Modal);
        assert_eq!(app.selected_subtask().map(|s| s.id.as_ref()), Some("sub-1"));

        app.reduce(AppAction::SubtaskListMove(1));
        assert_eq!(app.selected_subtask().map(|s| s.id.as_ref()), Some("sub-2"));
    }

    #[test]
    fn closing_a_modal_clears_a_pending_agent_model_override() {
        // A cancelled subtask model-override picker must not leak into a later
        // normal `/model` submit.
        let mut app = app();
        app.pending_agent_model_override = Some("explore".to_string());
        app.reduce(AppAction::CloseModal);
        assert_eq!(app.pending_agent_model_override, None);
    }

    #[test]
    fn clearing_a_subtask_model_override_reverts_to_default() {
        use crate::provider::ReasoningSelection;
        use crate::subagent::{SubagentModelOverride, SubagentStatus};
        let mut app = app();
        app.reduce(AppAction::RefreshSubtaskList {
            subtasks: vec![subtask_snapshot("sub-1", SubagentStatus::Succeeded)],
        });
        app.reduce(AppAction::OpenSubtaskList);
        // Pin an override for the selected agent ("explore"), then clear it.
        app.subagent_model_overrides.lock().unwrap().insert(
            "explore".to_string(),
            SubagentModelOverride::selector(
                "codex:openai/gpt-5.6".to_string(),
                ReasoningSelection::Default,
            ),
        );

        app.reduce(AppAction::SubtaskClearModelOverride);

        assert!(
            app.subagent_model_overrides.lock().unwrap().is_empty(),
            "clearing should drop the override so the next run inherits the default model"
        );
    }

    #[test]
    fn subtask_list_move_returns_focus_to_list_and_resets_detail_scroll() {
        use crate::subagent::SubagentStatus;
        let mut app = app();
        app.reduce(AppAction::RefreshSubtaskList {
            subtasks: vec![
                subtask_snapshot("sub-1", SubagentStatus::Running),
                subtask_snapshot("sub-2", SubagentStatus::Succeeded),
            ],
        });
        app.reduce(AppAction::OpenSubtaskList);
        app.reduce(AppAction::SubtaskListSetPane(SubtaskListPane::Detail));
        app.reduce(AppAction::ScrollModal(10));

        app.reduce(AppAction::SubtaskListMove(1));

        assert_eq!(app.selected_subtask().map(|s| s.id.as_ref()), Some("sub-2"));
        assert_eq!(app.modal_scroll, 0);
        assert!(
            matches!(
                app.modal,
                Some(ModalKind::Manager(
                    crate::tui::event::ManagerModal::SubtaskList {
                        pane: SubtaskListPane::List,
                        ..
                    }
                ))
            ),
            "moving selection should put focus back on the list"
        );
    }

    #[test]
    fn subtask_list_refresh_preserves_selection_by_id() {
        use crate::subagent::SubagentStatus;
        let mut app = app();
        app.reduce(AppAction::RefreshSubtaskList {
            subtasks: vec![
                subtask_snapshot("sub-1", SubagentStatus::Succeeded),
                subtask_snapshot("sub-2", SubagentStatus::Running),
            ],
        });
        app.reduce(AppAction::OpenSubtaskList);
        app.reduce(AppAction::SubtaskListMove(1));
        // A live refresh that reorders the list keeps the selected id selected.
        app.reduce(AppAction::RefreshSubtaskList {
            subtasks: vec![
                subtask_snapshot("sub-2", SubagentStatus::Running),
                subtask_snapshot("sub-1", SubagentStatus::Succeeded),
            ],
        });
        assert_eq!(app.selected_subtask().map(|s| s.id.as_ref()), Some("sub-2"));
    }

    #[test]
    fn task_list_selection_clamps_to_available_rows() {
        let mut app = app();
        app.reduce(AppAction::RefreshTaskList {
            tasks: vec![
                background_task("bg-1", BackgroundTaskStatus::Succeeded),
                background_task("bg-2", BackgroundTaskStatus::Running),
            ],
        });
        app.reduce(AppAction::OpenTaskList);

        app.reduce(AppAction::TaskListMove(i16::MAX));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::TaskList { cursor: 1, .. }
            ))
        ));

        app.reduce(AppAction::RefreshTaskList {
            tasks: vec![background_task("bg-1", BackgroundTaskStatus::Succeeded)],
        });
        assert!(matches!(
            app.modal,
            Some(ModalKind::Manager(
                crate::tui::event::ManagerModal::TaskList { cursor: 0, .. }
            ))
        ));
    }

    #[test]
    fn task_list_refresh_preserves_selected_task_by_id() {
        let mut app = app();
        app.reduce(AppAction::RefreshTaskList {
            tasks: vec![
                background_task("bg-1", BackgroundTaskStatus::Succeeded),
                background_task("bg-2", BackgroundTaskStatus::Running),
            ],
        });
        app.reduce(AppAction::OpenTaskList);
        app.reduce(AppAction::TaskListMove(1));

        app.reduce(AppAction::RefreshTaskList {
            tasks: vec![
                background_task("bg-0", BackgroundTaskStatus::Succeeded),
                background_task("bg-1", BackgroundTaskStatus::Succeeded),
                background_task("bg-2", BackgroundTaskStatus::Stopped),
            ],
        });

        assert!(matches!(
            app.selected_background_task(),
            Some(task) if task.id == "bg-2" && task.status == BackgroundTaskStatus::Stopped
        ));
    }

    #[test]
    fn peer_list_open_move_and_refresh_preserve_selection_by_id() {
        use crate::peer::PeerOverview;
        use crate::storage::SessionId;

        let overview = |id: i64, title: &str| PeerOverview {
            id: SessionId::from_raw(id),
            title: title.to_string(),
            working: false,
            waiting_on_peer: false,
            peer_waiting_on_you: false,
            claims: Vec::new(),
            changed_files: Vec::new(),
        };

        let mut app = app();
        app.reduce(AppAction::OpenPeerList {
            peers: vec![overview(45, "tests"), overview(46, "docs")],
        });
        assert!(app.is_peer_list_open());
        app.reduce(AppAction::PeerListMove(1));

        // A refresh that reorders and adds a peer keeps #46 selected by id.
        app.reduce(AppAction::RefreshPeerList {
            peers: vec![
                overview(44, "new"),
                overview(46, "docs — updated"),
                overview(45, "tests"),
            ],
        });
        let Some(ModalKind::Manager(crate::tui::event::ManagerModal::PeerList { peers, cursor })) =
            &app.modal
        else {
            panic!("peer list should still be open");
        };
        assert_eq!(peers[*cursor].id, SessionId::from_raw(46));
        assert_eq!(peers[*cursor].title, "docs — updated");
    }

    #[test]
    fn command_finished_stores_context_report() {
        let mut app = app();
        let report = context_report(84);

        app.reduce(AppAction::Runtime(RuntimeEvent::CommandFinished(Box::new(
            CommandOutcomeEvent::Applied {
                generation: None,
                clear_transcript: false,
                messages: Vec::new(),
                provider: None,
                context_report: Some(Box::new(report.clone())),
                quit: false,
                open_modal: None,
            },
        ))));

        assert_eq!(app.latest_context_report, Some(report));
    }

    #[test]
    fn stale_command_result_is_dropped_after_cancel() {
        let mut app = app();
        app.reduce(AppAction::SetTaskState(TaskState::Command));
        let generation = app.command_generation();

        // Ctrl+C cancels the in-flight command, bumping the generation.
        app.invalidate_pending_commands();

        // The orphaned command finishes late and tries to apply its result.
        app.reduce(AppAction::Runtime(RuntimeEvent::CommandFinished(Box::new(
            CommandOutcomeEvent::Applied {
                generation: Some(generation),
                clear_transcript: false,
                messages: Vec::new(),
                provider: None,
                context_report: Some(Box::new(context_report(7))),
                quit: false,
                open_modal: None,
            },
        ))));

        // Dropped: task state was not reset to idle and nothing was applied.
        assert_eq!(app.task_state, TaskState::Command);
        assert_eq!(app.latest_context_report, None);
    }

    #[test]
    fn matching_generation_command_result_is_applied() {
        let mut app = app();
        app.reduce(AppAction::SetTaskState(TaskState::Command));
        let generation = app.command_generation();
        let report = context_report(7);

        // No cancel: the generation still matches, so the result applies.
        app.reduce(AppAction::Runtime(RuntimeEvent::CommandFinished(Box::new(
            CommandOutcomeEvent::Applied {
                generation: Some(generation),
                clear_transcript: false,
                messages: Vec::new(),
                provider: None,
                context_report: Some(Box::new(report.clone())),
                quit: false,
                open_modal: None,
            },
        ))));

        assert_eq!(app.task_state, TaskState::Idle);
        assert_eq!(app.latest_context_report, Some(report));
    }

    #[test]
    fn local_model_wizard_reducer_routes_fields_selection_and_metadata() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::Wizard(
            crate::tui::event::WizardModal::LocalModelWizard {
                state: Box::default(),
            },
        )));

        // Focus starts on the server-preset row; move to the name field first.
        app.reduce(AppAction::LocalModelWizard(
            crate::tui::event::LocalModelWizardAction::MoveField(1),
        ));
        app.reduce(AppAction::LocalModelWizard(
            crate::tui::event::LocalModelWizardAction::Paste("My Local".to_string()),
        ));
        let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard { state })) =
            &app.modal
        else {
            panic!("wizard should be open");
        };
        assert_eq!(state.display_name, "My Local");
        assert_eq!(state.provider_id, "my-local");

        if let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard { state })) =
            &mut app.modal
        {
            state.mark_fetch_started(1);
        }
        app.reduce(AppAction::Runtime(
            RuntimeEvent::LocalModelWizardFetchFinished {
                request_id: 1,
                result: Ok(crate::tui::local_model_wizard::WizardFetchOutcome {
                    models: vec![
                        crate::model_catalog::AvailableModel::remote("alpha"),
                        crate::model_catalog::AvailableModel::remote("beta"),
                    ],
                    detected: None,
                }),
            },
        ));
        app.reduce(AppAction::LocalModelWizard(
            crate::tui::event::LocalModelWizardAction::Toggle,
        ));
        app.reduce(AppAction::LocalModelWizard(
            crate::tui::event::LocalModelWizardAction::Submit,
        ));
        let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard { state })) =
            &app.modal
        else {
            panic!("wizard should remain open");
        };
        assert_eq!(
            state.step,
            crate::tui::local_model_wizard::LocalModelWizardStep::Metadata
        );
        assert_eq!(state.selected_model_count(), 1);
        assert_eq!(
            state
                .active_model()
                .map(|model| model.remote_model.as_str()),
            Some("beta")
        );

        app.reduce(AppAction::LocalModelWizard(
            crate::tui::event::LocalModelWizardAction::MoveField(1),
        ));
        app.reduce(AppAction::LocalModelWizard(
            crate::tui::event::LocalModelWizardAction::InputChar('8'),
        ));
        app.reduce(AppAction::LocalModelWizard(
            crate::tui::event::LocalModelWizardAction::InputChar('1'),
        ));
        app.reduce(AppAction::LocalModelWizard(
            crate::tui::event::LocalModelWizardAction::InputChar('9'),
        ));
        app.reduce(AppAction::LocalModelWizard(
            crate::tui::event::LocalModelWizardAction::InputChar('2'),
        ));
        app.reduce(AppAction::LocalModelWizard(
            crate::tui::event::LocalModelWizardAction::Submit,
        ));

        let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard { state })) =
            &app.modal
        else {
            panic!("wizard should remain open");
        };
        assert_eq!(
            state.step,
            crate::tui::local_model_wizard::LocalModelWizardStep::Review
        );
        assert_eq!(
            state
                .active_model()
                .map(|model| model.output_limit.as_str()),
            Some("8192")
        );
    }

    #[test]
    fn local_model_wizard_ignores_stale_fetch_results() {
        let mut app = app();
        let mut state = crate::tui::local_model_wizard::LocalModelWizardState::default();
        state.mark_fetch_started(2);
        app.reduce(AppAction::OpenModal(ModalKind::Wizard(
            crate::tui::event::WizardModal::LocalModelWizard {
                state: Box::new(state),
            },
        )));

        app.reduce(AppAction::Runtime(
            RuntimeEvent::LocalModelWizardFetchFinished {
                request_id: 1,
                result: Ok(crate::tui::local_model_wizard::WizardFetchOutcome {
                    models: vec![crate::model_catalog::AvailableModel::remote("stale")],
                    detected: None,
                }),
            },
        ));

        let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard { state })) =
            &app.modal
        else {
            panic!("wizard should remain open");
        };
        assert!(state.loading);
        assert_eq!(state.active_fetch_request_id, Some(2));
        assert!(state.models.is_empty());

        app.reduce(AppAction::Runtime(
            RuntimeEvent::LocalModelWizardFetchFinished {
                request_id: 2,
                result: Ok(crate::tui::local_model_wizard::WizardFetchOutcome {
                    models: vec![crate::model_catalog::AvailableModel::remote("fresh")],
                    detected: None,
                }),
            },
        ));

        let Some(ModalKind::Wizard(crate::tui::event::WizardModal::LocalModelWizard { state })) =
            &app.modal
        else {
            panic!("wizard should remain open");
        };
        assert!(!state.loading);
        assert_eq!(state.active_fetch_request_id, None);
        assert_eq!(
            state
                .models
                .iter()
                .map(|model| model.remote_model.as_str())
                .collect::<Vec<_>>(),
            vec!["fresh"]
        );
    }

    #[test]
    fn submit_input_adds_user_block() {
        let mut app = app();

        app.reduce(AppAction::SubmitInput("hello".to_string()));

        assert_eq!(app.transcript.len(), 1);
        assert!(
            matches!(app.transcript[0], TranscriptItem::UserMessage { ref text } if text == "hello")
        );
    }

    #[test]
    fn submit_command_input_clears_without_user_block() {
        for command in ["/authorize", "/model"] {
            let mut app = app();
            app.composer.set_text(command.to_string());

            app.reduce(AppAction::SubmitCommandInput(command.to_string()));

            assert!(app.transcript.is_empty());
            assert_eq!(app.input(), "");
            assert_eq!(app.composer.text, "");
            assert_eq!(app.composer.history, vec![command.to_string()]);
        }
    }

    #[test]
    fn suppressed_transcript_items_are_not_inserted() {
        let mut app = app();
        let noisy_context_status = "[context] prompt estimate 1 + reserve 16000 exceeds context window 19000 (heuristic, low confidence); continuing with heuristic fallback";

        app.push_transcript_item(TranscriptItem::AssistantMessage {
            text: String::new(),
        });
        app.push_transcript_item(TranscriptItem::ReasoningSummary {
            text: " \n\t".to_string(),
        });
        app.push_transcript_item(TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text: noisy_context_status.to_string(),
        });
        app.push_transcript_item(TranscriptItem::ExecutionGroup(ExecutionGroup::new(
            1,
            Instant::now(),
        )));
        app.push_transcript_item(TranscriptItem::UserMessage {
            text: "visible".to_string(),
        });

        assert_eq!(app.transcript.len(), 1);
        assert!(matches!(
            &app.transcript[0],
            TranscriptItem::UserMessage { text } if text == "visible"
        ));
    }

    #[test]
    fn assistant_stream_appends_above_trailing_queue() {
        let mut app = app();
        app.reduce(AppAction::QueueInput {
            id: 1,
            text: "queued".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Coding,
        });

        app.reduce(AppAction::Agent(UiEvent::AssistantDelta("hel".to_string())));
        app.reduce(AppAction::Agent(UiEvent::AssistantDelta("lo".to_string())));

        assert!(matches!(
            app.transcript.as_slice(),
            [
                TranscriptItem::AssistantMessage { text },
                TranscriptItem::QueuedUserMessage { text: queued, .. }
            ] if text == "hello" && queued == "queued"
        ));
    }

    #[test]
    fn scroll_disables_autoscroll_and_clamps() {
        let mut app = app();

        app.reduce(AppAction::ScrollCurrent(5));

        assert_eq!(app.transcript_scroll, 5);
        assert!(!app.transcript_autoscroll);

        app.reduce(AppAction::ScrollCurrent(-20));

        assert_eq!(app.transcript_scroll, 0);
    }

    #[test]
    fn autoscroll_tracks_visible_bottom_before_manual_scroll() {
        let mut app = app();
        app.transcript_autoscroll = true;
        app.transcript_scroll = 12;

        app.clamp_current_scroll(80);
        app.reduce(AppAction::ScrollCurrent(-3));

        assert_eq!(app.transcript_scroll, 77);
        assert!(!app.transcript_autoscroll);
    }

    #[test]
    fn unseen_message_count_tracks_new_replies_while_scrolled_up() {
        let mut app = app();
        // Two replies already read while sitting at the bottom (auto-following).
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "one".to_string(),
        });
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "two".to_string(),
        });
        app.clamp_current_scroll(50); // caught up → baseline = 2 seen
        assert_eq!(app.unseen_message_count(), 0);

        // Scroll up: stop auto-following. Nothing new has arrived yet.
        app.reduce(AppAction::ScrollCurrent(-5));
        assert!(!app.transcript_autoscroll);
        assert_eq!(app.unseen_message_count(), 0);

        // Two more replies stream in while the reader is scrolled away.
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "three".to_string(),
        });
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "four".to_string(),
        });
        assert_eq!(app.unseen_message_count(), 2);

        // Reasoning and tool cards are not "messages" — they don't inflate it.
        app.transcript.push(TranscriptItem::ReasoningSummary {
            text: "thinking".to_string(),
        });
        assert_eq!(app.unseen_message_count(), 2);

        // Returning to the bottom clears the badge immediately.
        app.reduce(AppAction::ScrollBottom);
        assert_eq!(app.unseen_message_count(), 0);
    }

    #[test]
    fn sidebar_and_plan_scroll_independently() {
        let mut app = app();

        app.reduce(AppAction::ScrollSidebar(7));
        app.reduce(AppAction::ScrollPlan(11));

        assert_eq!(app.sidebar_scroll, 7);
        assert_eq!(app.plan_scroll, 11);
    }

    #[test]
    fn set_plan_scroll_max_jumps_to_bottom_after_clamp() {
        // The run loop calls SetPlanScroll(u16::MAX) whenever the plan
        // revision bumps; the render path clamps it to max_scroll. The
        // reducer itself should accept the sentinel without panicking.
        let mut app = app();
        app.reduce(AppAction::SetPlanScroll(u16::MAX));
        assert_eq!(app.plan_scroll, u16::MAX);
        app.clamp_plan_scroll(42);
        assert_eq!(app.plan_scroll, 42);
    }

    #[test]
    fn composer_page_queues_pending_with_default_body_height() {
        // The reducer can't know the live body height, so the page delta
        // is stored as a pending field for the run loop to resolve.
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "hello".chars() {
            app.reduce(AppAction::InputChar(ch));
        }

        app.reduce(AppAction::ComposerPage(1));
        assert_eq!(app.pending_composer_page, Some(1));
        assert!(!app.composer_follow, "manual scroll disengages auto-follow");
    }

    #[test]
    fn composer_page_ignored_outside_input_focus() {
        let mut app = app();
        app.focus = Focus::Transcript;

        app.reduce(AppAction::ComposerPage(1));
        app.reduce(AppAction::SetComposerScroll(7));
        app.reduce(AppAction::ExtendComposerByPage(1));

        assert_eq!(app.pending_composer_page, None);
        assert_eq!(app.pending_composer_extend, None);
        assert_eq!(app.composer_scroll, 0);
        assert!(app.composer_follow);
    }

    #[test]
    fn clear_input_resets_composer_scroll_state() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "hello".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::ComposerPage(1));
        app.composer_scroll = 5;
        app.composer_follow = false;
        app.pending_composer_extend = Some(-1);

        app.reduce(AppAction::ClearInput);

        assert_eq!(app.composer_scroll, 0);
        assert!(app.composer_follow);
        assert_eq!(app.pending_composer_extend, None);
        assert_eq!(app.pending_composer_page, None);
    }

    #[test]
    fn submit_input_resets_composer_scroll_state() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "hello".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.composer_scroll = 4;
        app.composer_follow = false;

        app.reduce(AppAction::SubmitInput(app.composer.text.clone()));

        assert_eq!(app.composer_scroll, 0);
        assert!(app.composer_follow);
    }

    #[test]
    fn paste_with_live_credential_is_refused() {
        let mut app = app();
        app.focus = Focus::Input;
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));

        app.reduce(AppAction::PasteText(token));

        assert_eq!(app.input(), "", "credential must not enter the composer");
        let notice = app.copy_notice.as_ref().expect("a refusal notice is shown");
        assert_eq!(notice.kind, CopyNoticeKind::Error);
        assert!(
            notice.text.contains("GitHub token"),
            "notice should name the credential kind: {}",
            notice.text
        );
    }

    #[test]
    fn paste_without_credential_inserts_normally() {
        let mut app = app();
        app.focus = Focus::Input;

        app.reduce(AppAction::PasteText("just some pasted text".to_string()));

        assert_eq!(app.input(), "just some pasted text");
        assert!(app.copy_notice.is_none());
    }

    #[test]
    fn text_mutation_re_engages_auto_follow() {
        // Deliberate: typing while scrolled away should snap the
        // viewport back to the cursor; only PageUp/PageDown/mouse-wheel
        // keep the user off the auto-follow path.
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "hello".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.composer_follow = false;
        app.composer_scroll = 5;

        app.reduce(AppAction::InputChar('!'));

        assert!(app.composer_follow, "text edits must re-engage auto-follow");
    }

    #[test]
    fn cursor_moves_do_not_re_engage_auto_follow() {
        // The user may scroll up to inspect the text, then move the
        // cursor to a different position without snapping back.
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "hello world".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.composer_follow = false;
        app.composer_scroll = 5;

        app.reduce(AppAction::CursorLeft);

        assert!(
            !app.composer_follow,
            "cursor moves should not re-engage auto-follow"
        );
    }

    #[test]
    fn extend_composer_by_page_keeps_anchor_and_queues_extend() {
        let mut app = app();
        app.focus = Focus::Input;
        for ch in "abcdef".chars() {
            app.reduce(AppAction::InputChar(ch));
        }
        app.reduce(AppAction::CursorStart);
        app.reduce(AppAction::CursorEnd);
        let start_anchor = app.composer.selection_anchor;

        app.reduce(AppAction::ExtendComposerByPage(-1));

        assert_eq!(app.pending_composer_extend, Some(-1));
        assert_eq!(app.pending_composer_page, Some(-1));
        assert!(!app.composer_follow);
        assert_eq!(
            app.composer.selection_anchor, start_anchor,
            "extend-by-page should not clear the existing anchor"
        );
    }

    #[test]
    fn set_composer_scroll_disengages_auto_follow() {
        let mut app = app();
        app.focus = Focus::Input;
        app.composer_follow = true;

        app.reduce(AppAction::SetComposerScroll(12));

        assert_eq!(app.composer_scroll, 12);
        assert!(!app.composer_follow);
    }

    #[test]
    fn extend_by_chars_keeps_anchor_and_saturates() {
        let mut app = app();
        app.composer.text = "abcdef".to_string();
        app.composer.cursor = 3;
        app.composer.selection_anchor = Some(3);

        app.composer.extend_by_chars(-10);
        assert_eq!(app.composer.cursor, 0);
        assert_eq!(app.composer.selection_anchor, Some(3));

        app.composer.extend_by_chars(100);
        assert_eq!(app.composer.cursor, 6, "must saturate at text length");
        assert_eq!(app.composer.selection_anchor, Some(3));
    }

    #[test]
    fn extend_by_chars_zero_is_noop() {
        let mut app = app();
        app.composer.text = "abc".to_string();
        app.composer.cursor = 1;
        app.composer.selection_anchor = Some(1);

        app.composer.extend_by_chars(0);

        assert_eq!(app.composer.cursor, 1);
        assert_eq!(app.composer.selection_anchor, Some(1));
    }

    #[test]
    fn set_view_and_toggle_switch_views() {
        let mut app = app();

        app.reduce(AppAction::SetView(View::Plan));
        assert_eq!(app.view, View::Plan);

        app.reduce(AppAction::ToggleView);
        assert_eq!(app.view, View::Agent);
    }

    #[test]
    fn switching_out_of_plan_view_resets_focus_plan_to_transcript() {
        let mut app = app();
        app.view = View::Plan;
        app.focus = Focus::Plan;

        app.reduce(AppAction::SetView(View::Agent));
        assert_eq!(app.view, View::Agent);
        assert_eq!(
            app.focus,
            Focus::Transcript,
            "leaving plan view while canvas is focused should fall back to chat"
        );
    }

    #[test]
    fn switching_out_of_agent_view_resets_focus_todo_to_transcript() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Todo;

        app.reduce(AppAction::SetView(View::Plan));
        assert_eq!(app.view, View::Plan);
        assert_eq!(
            app.focus,
            Focus::Transcript,
            "leaving agent view while todo is focused should fall back to chat"
        );
    }

    #[test]
    fn switching_into_plan_view_keeps_focus() {
        let mut app = app();
        app.view = View::Agent;
        app.focus = Focus::Transcript;

        app.reduce(AppAction::SetView(View::Plan));
        assert_eq!(app.view, View::Plan);
        assert_eq!(app.focus, Focus::Transcript);
    }

    #[test]
    fn question_move_scrolls_to_keep_cursor_visible() {
        let mut app = app();
        let options = (0..12)
            .map(|idx| crate::interaction::QuestionOption {
                label: format!("Option {idx}"),
                description: format!("Description {idx}"),
                preselected: false,
            })
            .collect::<Vec<_>>();
        app.modal = Some(ModalKind::Detail(
            crate::tui::event::DetailModal::QuestionPrompt {
                request_id: 1,
                prompt: "Pick one".to_string(),
                header: None,
                options,
                multiple: false,
                origin: None,
                cursor: 0,
                selected: vec![false; 12],
            },
        ));

        for _ in 0..8 {
            app.reduce(AppAction::QuestionMove(1));
        }

        assert_eq!(app.modal_scroll, 0);
        assert!(
            app.pending_question_visibility,
            "run-loop clamp resolves question visibility after layout is known"
        );

        app.reduce(AppAction::QuestionMove(i16::MIN));

        assert_eq!(app.modal_scroll, 0);
        assert!(app.pending_question_visibility);
    }

    #[test]
    fn view_switch_re_engages_transcript_autoscroll() {
        let mut app = app();

        // Simulate the user scrolling up in the Agent view's wide chat column.
        app.transcript_autoscroll = false;
        app.transcript_scroll = 0;

        // Switching to Plan re-engages autoscroll so the next clamp_current_scroll
        // lands the user at the bottom of the (narrower) Plan chat column.
        app.reduce(AppAction::SetView(View::Plan));
        assert!(app.transcript_autoscroll);
        let max_plan = 80u16;
        app.clamp_current_scroll(max_plan);
        assert_eq!(app.transcript_scroll, max_plan);

        // Scrolling up in Plan, then toggling back to Agent, must also reset.
        app.transcript_autoscroll = false;
        app.transcript_scroll = 5;
        app.reduce(AppAction::ToggleView);
        assert!(app.transcript_autoscroll);
        let max_agent = 30u16;
        app.clamp_current_scroll(max_agent);
        assert_eq!(app.transcript_scroll, max_agent);
    }

    #[test]
    fn phase_advance_signal_set_only_during_phased_run() {
        use crate::tui::event::UiError;

        let mut app = app();

        // No phased run in flight → AgentFinished leaves the signal untouched.
        app.task_state = TaskState::Running;
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));
        assert_eq!(app.phase_advance, None);
        assert_eq!(app.current_session_status, SessionStatus::Completed);

        // Phased run, clean finish → advance to the next phase.
        app.plan_execution = Some(PlanExecution { phase_index: 0 });
        app.task_state = TaskState::Running;
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));
        assert_eq!(app.phase_advance, Some(PhaseAdvance::Continue));

        // Phased run, error → halt.
        app.phase_advance = None;
        app.task_state = TaskState::Running;
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Err(
            UiError::new("Agent failed", "boom"),
        ))));
        assert_eq!(app.phase_advance, Some(PhaseAdvance::Halt));
        assert_eq!(app.current_session_status, SessionStatus::Failed);

        // Phased run interrupted (Cancelling) reports Ok but must still halt.
        app.phase_advance = None;
        app.task_state = TaskState::Cancelling;
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));
        assert_eq!(app.phase_advance, Some(PhaseAdvance::Halt));

        // Regression (Ctrl+C advanced phases): the run loop's
        // `UiEvent::Interrupted` resets Cancelling → Idle *before*
        // AgentFinished lands, so the state heuristic alone reads a Ctrl+C'd
        // phase as success. The outcome carried on the event must halt anyway.
        app.phase_advance = None;
        app.task_state = TaskState::Cancelling;
        app.reduce(AppAction::Agent(UiEvent::Interrupted));
        assert_eq!(app.task_state, TaskState::Idle, "Interrupted resets state");
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Interrupted,
        ))));
        assert_eq!(
            app.phase_advance,
            Some(PhaseAdvance::Halt),
            "an interrupted phase must never auto-advance"
        );
        assert_eq!(app.current_session_status, SessionStatus::Interrupted);

        app.phase_advance = None;
        app.task_state = TaskState::Running;
        let reason = crate::run_budget::RunBudgetExhaustion::MaxTurns { limit: 25 };
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::BudgetExhausted(reason),
        ))));
        assert_eq!(
            app.phase_advance,
            Some(PhaseAdvance::Continue),
            "BudgetExhausted should auto-advance phases"
        );
        assert_eq!(app.current_session_status, SessionStatus::Interrupted);
        assert_eq!(app.current_terminal_reason, Some(reason));
        assert!(app.transcript.iter().any(|item| matches!(
            item,
            TranscriptItem::CommandOutput { kind: CommandOutputKind::Status, text }
                if text.contains("Budget exhausted") && text.contains("25")
        )));

        app.reduce(AppAction::SetTaskState(TaskState::Running));
        assert_eq!(app.current_session_status, SessionStatus::Active);
        assert_eq!(app.current_terminal_reason, None);
    }

    #[test]
    fn completion_report_events_do_not_alter_transcript_and_failed_outcome_marks_session_failed() {
        let mut app = app();
        let transcript_len = app.transcript.len();

        for status in [
            crate::completion_report::CompletionStatus::Completed,
            crate::completion_report::CompletionStatus::Failed,
        ] {
            let report = crate::completion_report::CompletionReport::from_evidence(
                status,
                crate::completion_report::CompletionEvidenceSnapshot::default(),
                crate::completion_report::CompletionSessionEvidence {
                    verification: None,
                    review: None,
                    authorization_decisions: &[],
                    usage: crate::agent::UsageTotals::default(),
                    session_budget: crate::run_budget::SessionBudgetUsage::default(),
                    budget_exhaustion: None,
                },
            );
            app.reduce(AppAction::Runtime(RuntimeEvent::CompletionReport(
                Box::new(report),
            )));
        }

        assert_eq!(app.transcript.len(), transcript_len);
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Failed,
        ))));

        assert_eq!(app.current_session_status, SessionStatus::Failed);
        assert_eq!(app.transcript.len(), transcript_len);
    }

    #[test]
    fn interrupted_outcome_adds_compact_status_and_preserves_session_status() {
        let mut app = app();

        app.reduce(AppAction::Agent(UiEvent::Interrupted));
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Interrupted,
        ))));

        assert_eq!(app.current_session_status, SessionStatus::Interrupted);
        assert!(matches!(
            app.transcript.as_slice(),
            [TranscriptItem::CommandOutput {
                kind: CommandOutputKind::Status,
                text,
            }] if text == "Run interrupted."
        ));
    }

    #[test]
    fn task_panicked_halts_phased_execution() {
        use crate::tui::event::UiError;

        let mut app = app();
        app.plan_execution = Some(PlanExecution { phase_index: 1 });
        app.phase_advance = Some(PhaseAdvance::Continue);
        app.task_state = TaskState::Running;

        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(
            UiError::new("boom", "panic"),
        )));

        assert_eq!(app.task_state, TaskState::Idle);
        assert!(
            app.plan_execution.is_none(),
            "a panic must clear stale phased-execution state"
        );
        assert!(app.phase_advance.is_none());
    }

    #[test]
    fn agent_events_update_state() {
        let mut app = app();

        app.task_state = TaskState::Running;
        app.reduce(AppAction::Agent(UiEvent::Thinking("planning".to_string())));
        assert_eq!(app.current_phase.as_deref(), Some("planning"));
        app.reduce(AppAction::Agent(UiEvent::ReasoningDelta(
            "I should inspect the file first".to_string(),
        )));
        let started_at = Instant::now();
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "read_file".to_string(),
            arguments: "{}".to_string(),
            started_at,
        }));
        assert_eq!(app.active_tools.len(), 1);
        assert_eq!(app.active_tools[0].0, "read_file");
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-1".to_string(),
            result: "ok".to_string(),
            success: true,
            finished_at: started_at + Duration::from_millis(12),
        }));
        app.reduce(AppAction::Agent(UiEvent::AssistantDelta("hi".to_string())));
        app.reduce(AppAction::Agent(UiEvent::AssistantDelta(
            " there".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::AssistantDone));
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));

        assert_eq!(app.current_phase, None);
        assert_eq!(app.task_state, TaskState::Idle);
        assert_eq!(app.transcript.len(), 3);
        assert!(
            matches!(app.transcript[0], TranscriptItem::ReasoningSummary { ref text } if text == "I should inspect the file first")
        );
        assert!(matches!(
            &app.transcript[1],
            TranscriptItem::ExecutionGroup(group) if group.tools.iter().any(|activity|
                activity.name == "read_file" && activity.status == ToolStatus::Succeeded)
        ));
        assert!(
            matches!(app.transcript[2], TranscriptItem::AssistantMessage { ref text } if text == "hi there")
        );
    }

    #[test]
    fn attempt_retraction_withdraws_streamed_cells() {
        let mut app = app();
        app.push_transcript_item(TranscriptItem::UserMessage {
            text: "prompt".to_string(),
        });

        app.reduce(AppAction::Agent(UiEvent::AttemptStarted));
        app.reduce(AppAction::Agent(UiEvent::ReasoningDelta(
            "half a thought".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::AssistantDelta(
            "half an answer".to_string(),
        )));
        assert_eq!(app.transcript.len(), 3);

        app.reduce(AppAction::Agent(UiEvent::AttemptDiscarded));

        assert_eq!(app.transcript.len(), 1);
        assert!(
            matches!(app.transcript[0], TranscriptItem::UserMessage { ref text } if text == "prompt")
        );

        // The retry streams into fresh cells.
        app.reduce(AppAction::Agent(UiEvent::AttemptStarted));
        app.reduce(AppAction::Agent(UiEvent::ReasoningDelta(
            "full thought".to_string(),
        )));
        assert_eq!(app.transcript.len(), 2);
        assert!(
            matches!(app.transcript[1], TranscriptItem::ReasoningSummary { ref text } if text == "full thought")
        );
    }

    #[test]
    fn attempt_retraction_spares_committed_cells_from_earlier_calls() {
        let mut app = app();
        // First model call streams reasoning and commits (no retraction).
        app.reduce(AppAction::Agent(UiEvent::AttemptStarted));
        app.reduce(AppAction::Agent(UiEvent::ReasoningDelta(
            "committed thought".to_string(),
        )));
        // Next call in the same turn: the break flag forces a fresh cell, so
        // the doomed reasoning cannot merge into the committed trailing cell
        // and retraction removes whole cells only.
        app.reduce(AppAction::Agent(UiEvent::AttemptStarted));
        app.reduce(AppAction::Agent(UiEvent::ReasoningDelta(
            "doomed thought".to_string(),
        )));
        assert_eq!(app.transcript.len(), 2);

        app.reduce(AppAction::Agent(UiEvent::AttemptDiscarded));

        assert_eq!(app.transcript.len(), 1);
        assert!(
            matches!(app.transcript[0], TranscriptItem::ReasoningSummary { ref text } if text == "committed thought")
        );
    }

    #[test]
    fn attempt_retraction_preserves_queued_tail_and_focus() {
        let mut app = app();
        app.push_transcript_item(TranscriptItem::UserMessage {
            text: "prompt".to_string(),
        });
        app.reduce(AppAction::Agent(UiEvent::AttemptStarted));
        app.reduce(AppAction::Agent(UiEvent::ReasoningDelta(
            "doomed".to_string(),
        )));
        app.push_transcript_item(TranscriptItem::QueuedUserMessage {
            id: 7,
            text: "steer".to_string(),
        });
        app.transcript_focus = Some(2);

        app.reduce(AppAction::Agent(UiEvent::AttemptDiscarded));

        assert_eq!(app.transcript.len(), 2);
        assert!(matches!(
            app.transcript[1],
            TranscriptItem::QueuedUserMessage { .. }
        ));
        assert_eq!(app.transcript_focus, Some(1));
    }

    #[test]
    fn attempt_retraction_works_unchanged_under_serenity() {
        // Serenity only changes how reasoning cells render; retraction must
        // remove the same cells so no orphaned "Thinking" placeholder
        // survives a failed attempt.
        let mut app = app();
        app.serenity_mode = true;
        app.task_state = TaskState::Running;
        app.push_transcript_item(TranscriptItem::UserMessage {
            text: "prompt".to_string(),
        });

        app.reduce(AppAction::Agent(UiEvent::AttemptStarted));
        app.reduce(AppAction::Agent(UiEvent::ReasoningDelta(
            "doomed thought".to_string(),
        )));
        assert_eq!(app.transcript.len(), 2);

        app.reduce(AppAction::Agent(UiEvent::AttemptDiscarded));

        assert_eq!(app.transcript.len(), 1);
        assert!(
            !app.transcript
                .iter()
                .any(|item| matches!(item, TranscriptItem::ReasoningSummary { .. }))
        );
    }

    #[test]
    fn background_tool_output_updates_running_card_until_finish() {
        let mut app = app();
        let started_at = Instant::now();

        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-bg".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"sleep 5","run_in_background":true}"#.to_string(),
            started_at,
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolOutput {
            id: "call-bg".to_string(),
            output: "Started background task bg-1".to_string(),
            updated_at: started_at + Duration::from_millis(1),
        }));
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));

        let activity = app.tool_activity("call-bg").expect("tool exists");
        assert_eq!(activity.status, ToolStatus::Running);
        assert_eq!(
            activity.result.as_deref(),
            Some("Started background task bg-1")
        );
        assert_eq!(app.active_tools.len(), 1);

        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-bg".to_string(),
            result: "bg-1 succeeded".to_string(),
            success: true,
            finished_at: started_at + Duration::from_millis(10),
        }));

        let activity = app.tool_activity("call-bg").expect("tool exists");
        assert_eq!(activity.status, ToolStatus::Succeeded);
        assert_eq!(activity.result.as_deref(), Some("bg-1 succeeded"));
        assert!(app.active_tools.is_empty());
        assert!(app.active_execution_group_id.is_none());
    }

    #[test]
    fn interactive_bash_card_stays_running_after_agent_turn() {
        let mut app = app();
        let started_at = Instant::now();
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-pty".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"repl","interactive":true}"#.to_string(),
            started_at,
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolOutput {
            id: "call-pty".to_string(),
            output: "Started interactive terminal pty-1".to_string(),
            updated_at: started_at,
        }));

        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));

        assert_eq!(
            app.tool_activity("call-pty")
                .map(|activity| activity.status),
            Some(ToolStatus::Running)
        );
        assert!(app.active_execution_group_id.is_some());
    }

    #[test]
    fn set_session_title_tool_finish_updates_current_session_summary() {
        let mut app = app();
        let started_at = Instant::now();
        app.set_session_identity(crate::storage::SessionId::from_raw(42), "workspace", "");
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-title".to_string(),
            name: "set_session_title".to_string(),
            arguments: r#"{"title":"Polish resume picker"}"#.to_string(),
            started_at,
        }));

        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-title".to_string(),
            result: "Session title set to: Polish resume picker".to_string(),
            success: true,
            finished_at: started_at + Duration::from_millis(12),
        }));

        assert_eq!(app.current_session_summary, "Polish resume picker");
    }

    #[test]
    fn plan_set_title_tool_finish_updates_current_session_summary() {
        // The plan-title tool mirrors its new title to the active session, so
        // the TUI header has to reflect the rename immediately.
        let mut app = app();
        let started_at = Instant::now();
        app.set_session_identity(crate::storage::SessionId::from_raw(43), "workspace", "");
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-plan-title".to_string(),
            name: "plan_set_title".to_string(),
            arguments: r#"{"title":"Refactor editor"}"#.to_string(),
            started_at,
        }));

        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-plan-title".to_string(),
            result: "Plan title set to: Refactor editor".to_string(),
            success: true,
            finished_at: started_at + Duration::from_millis(12),
        }));

        assert_eq!(app.current_session_summary, "Refactor editor");
    }

    #[test]
    fn assistant_done_does_not_flip_status_to_idle_while_agent_iterates() {
        let mut app = app();

        app.task_state = TaskState::Running;

        // First model response: emits a tool call. AssistantDone fires here, but
        // the agent loop will iterate, so the status must stay Running.
        app.reduce(AppAction::Agent(UiEvent::AssistantDelta(
            "let me check".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "read_file".to_string(),
            arguments: "{}".to_string(),
            started_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-1".to_string(),
            result: "ok".to_string(),
            success: true,
            finished_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::AssistantDone));
        assert_eq!(
            app.task_state,
            TaskState::Running,
            "AssistantDone after a tool-calling response must not flip to Idle"
        );

        // Second model response: no tool call, agent loop returns.
        app.reduce(AppAction::Agent(UiEvent::AssistantDelta(
            "done".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::AssistantDone));
        assert_eq!(
            app.task_state,
            TaskState::Running,
            "AssistantDone with no tool call is still not the terminal signal"
        );

        // The terminal signal — RuntimeEvent::AgentFinished — is the only thing
        // that should flip the status to Idle for the agent path.
        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));
        assert_eq!(app.task_state, TaskState::Idle);
        assert_eq!(app.current_phase, None);
    }

    #[test]
    fn scratch_fragment_before_tool_group_is_downgraded_to_worklog() {
        let mut app = app();
        app.task_state = TaskState::Running;

        app.reduce(AppAction::Agent(UiEvent::AssistantDelta(
            "Need edit.".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::AssistantDone));
        assert_eq!(app.transcript.assistant_message_count(), 1);

        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "edit".to_string(),
            arguments: "{}".to_string(),
            started_at: Instant::now(),
        }));

        assert!(
            app.transcript
                .iter()
                .any(|i| matches!(i, TranscriptItem::WorkLog { text } if text == "Need edit.")),
            "the scratch note should be downgraded to a WorkLog row"
        );
        assert!(
            !app.transcript
                .iter()
                .any(|i| matches!(i, TranscriptItem::AssistantMessage { .. })),
            "no AssistantMessage should remain after the downgrade"
        );
        assert_eq!(app.transcript.assistant_message_count(), 0);
    }

    #[test]
    fn genuine_narration_before_tool_group_stays_assistant_message() {
        let mut app = app();
        app.task_state = TaskState::Running;

        app.reduce(AppAction::Agent(UiEvent::AssistantDelta(
            "Refactoring the parser to handle nested groups.".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::AssistantDone));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "edit".to_string(),
            arguments: "{}".to_string(),
            started_at: Instant::now(),
        }));

        assert!(
            app.transcript
                .iter()
                .any(|i| matches!(i, TranscriptItem::AssistantMessage { .. })),
            "genuine narration must remain an AssistantMessage"
        );
        assert!(
            !app.transcript
                .iter()
                .any(|i| matches!(i, TranscriptItem::WorkLog { .. })),
            "genuine narration must not be downgraded"
        );
    }

    #[test]
    fn short_status_before_tool_group_stays_assistant_message() {
        let mut app = app();
        app.task_state = TaskState::Running;

        app.reduce(AppAction::Agent(UiEvent::AssistantDelta(
            "Running tests.".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::AssistantDone));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            started_at: Instant::now(),
        }));

        assert!(
            app.transcript.iter().any(
                |i| matches!(i, TranscriptItem::AssistantMessage { text } if text == "Running tests.")
            ),
            "complete short status must remain an AssistantMessage"
        );
        assert!(
            !app.transcript
                .iter()
                .any(|i| matches!(i, TranscriptItem::WorkLog { .. })),
            "complete short status must not be downgraded"
        );
        assert_eq!(app.transcript.assistant_message_count(), 1);
    }

    #[test]
    fn scratch_looking_final_answer_without_tool_stays_assistant_message() {
        let mut app = app();
        app.task_state = TaskState::Running;

        // A scratch-shaped message that is the turn's final answer — no tool
        // call follows it — must never be downgraded (structural immunity).
        app.reduce(AppAction::Agent(UiEvent::AssistantDelta(
            "Need edit.".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::AssistantDone));

        assert!(
            app.transcript.iter().any(
                |i| matches!(i, TranscriptItem::AssistantMessage { text } if text == "Need edit.")
            ),
            "a final answer with no following tool call must stay an AssistantMessage"
        );
    }

    fn add_three_text_blocks(app: &mut AppState) {
        app.reduce(AppAction::SubmitInput("first".to_string()));
        app.transcript.push(TranscriptItem::AssistantMessage {
            text: "second".to_string(),
        });
        app.transcript.push(TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text: "third".to_string(),
        });
    }

    #[test]
    fn transcript_click_from_input_focuses_clicked_item() {
        let mut app = app();
        add_three_text_blocks(&mut app);
        app.focus = Focus::Input;
        app.active_group_tool_selection = Some(InlineToolSelection {
            group_id: 99,
            selected_tool: 0,
        });
        let position = TranscriptPosition {
            item: 1,
            grapheme: 2,
            width: 80,
        };

        app.reduce(AppAction::TranscriptClick {
            position,
            kind: SelectionKind::Position,
            extend: false,
            column: 0,
            row: 0,
        });

        assert_eq!(app.focus, Focus::Transcript);
        assert_eq!(app.transcript_focus, Some(1));
        assert_eq!(app.active_group_tool_selection, None);
        assert_eq!(
            app.transcript_selection,
            Some(TranscriptSelection {
                anchor: position,
                caret: position,
            })
        );
    }

    #[test]
    fn top_level_tool_click_focuses_tool_before_detail() {
        let mut app = app();
        app.focus = Focus::Input;
        app.transcript
            .push(TranscriptItem::ToolActivity(tool_activity(
                "call-1", "bash",
            )));

        app.reduce(AppAction::OpenToolDetail("call-1".to_string()));

        assert!(matches!(
            app.modal,
            Some(ModalKind::Detail(crate::tui::event::DetailModal::ToolDetail { ref tool_id })) if tool_id == "call-1"
        ));
        assert_eq!(app.focus, Focus::Modal);
        assert_eq!(app.modal_return_focus, Some(Focus::Transcript));
        assert_eq!(app.transcript_focus, Some(0));
        assert_eq!(app.active_group_tool_selection, None);
    }

    #[test]
    fn group_summary_click_enters_inline_tool_selection() {
        let mut app = app();
        app.focus = Focus::Input;
        app.transcript
            .push(TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 1,
                finished_at: None,
                tools: vec![
                    tool_activity("call-1", "bash"),
                    tool_activity("call-2", "read"),
                ],
            }));

        app.reduce(AppAction::OpenExecutionGroup { group_id: 1 });

        assert_eq!(app.modal, None);
        assert_eq!(app.focus, Focus::Transcript);
        assert_eq!(app.transcript_focus, Some(0));
        assert_eq!(
            app.active_group_tool_selection,
            Some(InlineToolSelection {
                group_id: 1,
                selected_tool: 0,
            })
        );
    }

    #[test]
    fn serenity_group_summary_toggle_expands_and_collapses_group() {
        let mut app = app();
        app.serenity_mode = true;
        app.focus = Focus::Input;
        app.transcript
            .push(TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 1,
                finished_at: None,
                tools: vec![
                    tool_activity("call-1", "bash"),
                    tool_activity("call-2", "read"),
                ],
            }));

        app.reduce(AppAction::ToggleExecutionGroup { group_id: 1 });

        assert_eq!(app.modal, None);
        assert_eq!(app.focus, Focus::Transcript);
        assert_eq!(app.transcript_focus, Some(0));
        assert_eq!(app.active_group_tool_selection, None);
        assert!(app.expanded_execution_groups.contains(&1));

        app.reduce(AppAction::ToggleExecutionGroup { group_id: 1 });

        assert!(!app.expanded_execution_groups.contains(&1));
    }

    #[test]
    fn enabling_serenity_clears_inline_tool_selection() {
        let mut app = app();
        app.transcript
            .push(TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 1,
                finished_at: None,
                tools: vec![tool_activity("call-1", "bash")],
            }));
        app.reduce(AppAction::OpenExecutionGroup { group_id: 1 });
        assert!(app.active_group_tool_selection.is_some());

        app.reduce(AppAction::SetSerenityMode(true));

        assert!(app.serenity_mode);
        assert_eq!(app.active_group_tool_selection, None);
    }

    #[test]
    fn nested_group_tool_click_focuses_group_and_selected_row() {
        let mut app = app();
        app.focus = Focus::Input;
        app.transcript
            .push(TranscriptItem::ExecutionGroup(ExecutionGroup {
                id: 1,
                finished_at: None,
                tools: vec![
                    tool_activity("call-1", "bash"),
                    tool_activity("call-2", "read"),
                ],
            }));

        app.reduce(AppAction::OpenToolDetail("call-2".to_string()));

        assert!(matches!(
            app.modal,
            Some(ModalKind::Detail(crate::tui::event::DetailModal::ToolDetail { ref tool_id })) if tool_id == "call-2"
        ));
        assert_eq!(app.focus, Focus::Modal);
        assert_eq!(app.modal_return_focus, Some(Focus::Transcript));
        assert_eq!(app.transcript_focus, Some(0));
        assert_eq!(
            app.active_group_tool_selection,
            Some(InlineToolSelection {
                group_id: 1,
                selected_tool: 1,
            })
        );
    }

    #[test]
    fn transcript_focus_navigation_seeds_last_and_clamps() {
        let mut app = app();
        add_three_text_blocks(&mut app);
        app.focus = Focus::Transcript;

        app.reduce(AppAction::MoveTranscriptFocus(-1));
        assert_eq!(app.transcript_focus, Some(2));

        app.reduce(AppAction::MoveTranscriptFocus(-1));
        assert_eq!(app.transcript_focus, Some(1));

        app.reduce(AppAction::MoveTranscriptFocus(-20));
        assert_eq!(app.transcript_focus, Some(0));

        app.reduce(AppAction::MoveTranscriptFocus(20));
        assert_eq!(app.transcript_focus, Some(2));
    }

    #[test]
    fn transcript_focus_home_end_and_scroll_request() {
        let mut app = app();
        add_three_text_blocks(&mut app);
        app.focus = Focus::Transcript;
        app.transcript_autoscroll = true;

        app.reduce(AppAction::FocusTranscriptFirst);
        assert_eq!(app.transcript_focus, Some(0));
        assert!(!app.transcript_autoscroll);
        assert!(app.scroll_transcript_focus_into_view);

        app.scroll_transcript_focus_into_view = false;
        app.reduce(AppAction::FocusTranscriptLast);
        assert_eq!(app.transcript_focus, Some(2));
        assert!(app.scroll_transcript_focus_into_view);
    }

    #[test]
    fn focusing_input_clears_transcript_selection_and_focus() {
        let mut app = app();
        add_three_text_blocks(&mut app);
        app.focus = Focus::Transcript;
        app.transcript_focus = Some(1);
        app.transcript_selection = Some(TranscriptSelection {
            anchor: TranscriptPosition {
                item: 0,
                grapheme: 0,
                width: 80,
            },
            caret: TranscriptPosition {
                item: 1,
                grapheme: 0,
                width: 80,
            },
        });
        app.scroll_transcript_focus_into_view = true;

        app.reduce(AppAction::SetFocus(Focus::Input));

        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.transcript_selection, None);
        assert_eq!(app.transcript_focus, None);
        assert!(!app.scroll_transcript_focus_into_view);
    }

    #[test]
    fn clicking_composer_clears_transcript_selection_and_focus() {
        let mut app = app();
        add_three_text_blocks(&mut app);
        app.focus = Focus::Transcript;
        app.transcript_focus = Some(1);
        app.transcript_selection = Some(TranscriptSelection {
            anchor: TranscriptPosition {
                item: 0,
                grapheme: 0,
                width: 80,
            },
            caret: TranscriptPosition {
                item: 1,
                grapheme: 0,
                width: 80,
            },
        });
        app.scroll_transcript_focus_into_view = true;

        app.reduce(AppAction::ComposerClick {
            char_index: 0,
            kind: SelectionKind::Position,
            column: 0,
            row: 0,
        });

        assert_eq!(app.focus, Focus::Input);
        assert_eq!(app.transcript_selection, None);
        assert_eq!(app.transcript_focus, None);
        assert!(!app.scroll_transcript_focus_into_view);
    }

    #[test]
    fn transcript_focus_moves_without_destroying_text_selection() {
        let mut app = app();
        add_three_text_blocks(&mut app);
        app.focus = Focus::Transcript;
        app.transcript_selection = Some(TranscriptSelection {
            anchor: TranscriptPosition {
                item: 0,
                grapheme: 0,
                width: 80,
            },
            caret: TranscriptPosition {
                item: 1,
                grapheme: 0,
                width: 80,
            },
        });
        let selection = app.transcript_selection;

        app.reduce(AppAction::MoveTranscriptFocus(1));
        app.reduce(AppAction::ClearTranscriptFocus);

        assert_eq!(app.transcript_selection, selection);
        assert_eq!(app.transcript_focus, None);

        app.reduce(AppAction::MoveTranscriptFocus(1));
        app.reduce(AppAction::ClearSelection);
        assert_eq!(app.transcript_selection, None);
        assert_eq!(app.transcript_focus, None);
    }

    #[test]
    fn open_focused_detail_routes_text_blocks_and_returns_to_transcript() {
        let mut app = app();
        add_three_text_blocks(&mut app);
        app.focus = Focus::Transcript;
        app.transcript_focus = Some(1);

        app.reduce(AppAction::OpenFocusedDetail);
        assert!(matches!(
            app.modal,
            Some(ModalKind::Detail(
                crate::tui::event::DetailModal::BlockDetail { item_index: 1 }
            ))
        ));
        assert_eq!(app.focus, Focus::Modal);

        app.reduce(AppAction::CloseModal);
        assert_eq!(app.focus, Focus::Transcript);
    }

    #[test]
    fn open_focused_detail_routes_tool_blocks() {
        let mut app = app();
        app.focus = Focus::Transcript;
        app.transcript
            .push(TranscriptItem::ToolActivity(ToolActivity {
                id: "call-1".to_string(),
                name: "bash".to_string(),
                arguments: "{\"command\":\"date\"}".to_string(),
                status: ToolStatus::Succeeded,
                result: Some("ok".to_string()),
                diff: None,
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
            }));
        app.transcript_focus = Some(0);

        app.reduce(AppAction::OpenFocusedDetail);

        assert!(
            matches!(app.modal, Some(ModalKind::Detail(crate::tui::event::DetailModal::ToolDetail { ref tool_id })) if tool_id == "call-1")
        );
    }

    #[test]
    fn plan_findings_modal_opens_cycles_and_jumps_to_evidence() {
        let mut app = app();
        // A tool card the finding's evidence id points at.
        app.transcript
            .push(TranscriptItem::ToolActivity(ToolActivity {
                id: "call-7".to_string(),
                name: "read".to_string(),
                arguments: "{}".to_string(),
                status: ToolStatus::Succeeded,
                result: Some("ok".to_string()),
                diff: None,
                started_at: Instant::now(),
                finished_at: Some(Instant::now()),
            }));
        app.plan.edit().add_finding(crate::plan::Finding {
            severity: crate::plan::Severity::Blocker,
            file: Some("src/foo.rs".to_string()),
            line: Some(1),
            issue: "blocker issue".to_string(),
            required_fix: "fix".to_string(),
            acceptance_tests: vec![],
            source_ids: vec!["call-7".to_string()],
            task: None,
            resolved: false,
        });
        app.plan.edit().add_finding(crate::plan::Finding {
            severity: crate::plan::Severity::Nit,
            file: None,
            line: None,
            issue: "nit issue".to_string(),
            required_fix: "fix".to_string(),
            acceptance_tests: vec![],
            source_ids: vec![],
            task: None,
            resolved: false,
        });

        // Enter from the plan pane opens the most-severe finding.
        app.reduce(AppAction::OpenPlanFindings);
        assert!(matches!(
            app.modal,
            Some(ModalKind::Detail(
                crate::tui::event::DetailModal::PlanFindingDetail { index: 0 }
            ))
        ));

        // Cycle forward to the Nit, then clamp at the end.
        app.reduce(AppAction::CyclePlanFinding(1));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Detail(
                crate::tui::event::DetailModal::PlanFindingDetail { index: 1 }
            ))
        ));
        app.reduce(AppAction::CyclePlanFinding(1));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Detail(
                crate::tui::event::DetailModal::PlanFindingDetail { index: 1 }
            ))
        ));

        // Back to the Blocker and jump to its resolvable evidence card.
        app.reduce(AppAction::CyclePlanFinding(-5));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Detail(
                crate::tui::event::DetailModal::PlanFindingDetail { index: 0 }
            ))
        ));
        app.reduce(AppAction::OpenFindingEvidence);
        assert!(
            matches!(app.modal, Some(ModalKind::Detail(crate::tui::event::DetailModal::ToolDetail { ref tool_id })) if tool_id == "call-7")
        );
    }

    #[test]
    fn open_focused_detail_enters_inline_execution_group_selection() {
        let mut app = app();
        app.focus = Focus::Transcript;
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{\"command\":\"echo hi\"}".to_string(),
            started_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-2".to_string(),
            name: "read".to_string(),
            arguments: "{\"file_path\":\"src/main.rs\"}".to_string(),
            started_at: Instant::now(),
        }));
        app.transcript_focus = Some(0);

        app.reduce(AppAction::OpenFocusedDetail);

        assert!(
            app.modal.is_none(),
            "inline selection must not open a modal"
        );
        assert_eq!(app.focus, Focus::Transcript);
        assert_eq!(app.transcript_focus, Some(0));
        assert!(matches!(
            app.active_group_tool_selection,
            Some(InlineToolSelection {
                group_id: 1,
                selected_tool: 0
            })
        ));
    }

    #[test]
    fn nested_tool_detail_returns_to_inline_execution_group_selection() {
        let mut app = app();
        app.focus = Focus::Transcript;
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{\"command\":\"echo hi\"}".to_string(),
            started_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-2".to_string(),
            name: "read".to_string(),
            arguments: "{\"file_path\":\"src/main.rs\"}".to_string(),
            started_at: Instant::now(),
        }));
        app.transcript_focus = Some(0);
        app.reduce(AppAction::OpenFocusedDetail);

        app.reduce(AppAction::ExecutionGroupMoveSelection { delta: 1 });
        let selected_id = app
            .selected_execution_group_tool()
            .map(|activity| activity.id.clone());
        app.reduce(AppAction::OpenSelectedExecutionGroupTool);
        assert!(
            matches!(app.modal, Some(ModalKind::Detail(crate::tui::event::DetailModal::ToolDetail { ref tool_id })) if Some(tool_id.clone()) == selected_id)
        );
        app.reduce(AppAction::CloseModal);
        assert_eq!(app.focus, Focus::Transcript);
        assert!(app.modal.is_none());
        assert!(matches!(
            app.active_group_tool_selection,
            Some(InlineToolSelection {
                group_id: 1,
                selected_tool: 1
            })
        ));

        app.reduce(AppAction::ClearExecutionGroupSelection);
        assert_eq!(app.focus, Focus::Transcript);
        assert_eq!(app.active_group_tool_selection, None);
        assert_eq!(app.transcript_focus, Some(0));
    }

    #[test]
    fn nested_tool_diff_returns_to_inline_execution_group_selection() {
        let mut app = app();
        app.focus = Focus::Transcript;
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "write".to_string(),
            arguments: "{\"file_path\":\"src/main.rs\"}".to_string(),
            started_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolFinishedWithDiff {
            id: "call-1".to_string(),
            result: "ok".to_string(),
            success: true,
            diff: Box::new(crate::diff::FileDiff {
                path: "src/main.rs".to_string(),
                status: crate::diff::DiffStatus::Modified,
                hunks: Vec::new(),
                truncated: false,
                old_size: Some(0),
                new_size: 1,
                added_lines: 0,
                removed_lines: 0,
                additional_files: Box::default(),
            }),
            finished_at: Instant::now(),
        }));
        app.transcript_focus = Some(0);
        app.reduce(AppAction::OpenFocusedDetail);

        app.reduce(AppAction::OpenSelectedExecutionGroupDiff);
        assert!(
            matches!(app.modal, Some(ModalKind::Detail(crate::tui::event::DetailModal::DiffPreview { ref tool_id })) if tool_id == "call-1")
        );
        app.reduce(AppAction::CloseModal);
        assert_eq!(app.focus, Focus::Transcript);
        assert!(matches!(
            app.active_group_tool_selection,
            Some(InlineToolSelection {
                group_id: 1,
                selected_tool: 0
            })
        ));
    }

    #[test]
    fn tool_run_is_aggregated_into_single_execution_group() {
        let mut app = app();
        app.focus = Focus::Transcript;
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            started_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-2".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
            started_at: Instant::now(),
        }));

        assert_eq!(app.transcript.len(), 1);
        assert!(app.active_execution_group_id.is_some());
        match &app.transcript[0] {
            TranscriptItem::ExecutionGroup(group) => {
                assert_eq!(group.tools.len(), 2);
                assert!(group.tools.iter().any(|activity| activity.id == "call-1"));
                assert!(group.tools.iter().any(|activity| activity.id == "call-2"));
            }
            _ => panic!("expected execution group wrapping tools"),
        }
    }

    #[test]
    fn batched_tool_start_adds_single_execution_group() {
        let mut app = app();
        let started_at = Instant::now();

        app.reduce(AppAction::Agent(UiEvent::ToolCallsStarted {
            calls: vec![
                ToolCallStart::new("call-1", "bash", "{}"),
                ToolCallStart::new("call-2", "read", "{}"),
            ],
            started_at,
        }));

        assert_eq!(app.transcript.len(), 1);
        assert_eq!(app.active_execution_group_id, Some(1));
        match &app.transcript[0] {
            TranscriptItem::ExecutionGroup(group) => {
                assert_eq!(group.tools.len(), 2);
                assert_eq!(group.tools[0].id, "call-1");
                assert_eq!(group.tools[1].id, "call-2");
            }
            _ => panic!("expected execution group wrapping batched tools"),
        }
    }

    #[test]
    fn adopt_subagent_models_keys_by_launching_call() {
        let mut app = app();

        let assignments = vec![("call-2".to_string(), "openrouter:glm-4.7".to_string())];
        assert!(app.adopt_subagent_models(&assignments));
        assert_eq!(app.subagent_model_for("call-2"), Some("openrouter:glm-4.7"));
        assert_eq!(app.subagent_model_for("call-1"), None);

        // Re-adopting the same value reports no change (no redraw needed);
        // a run that fails over to another model still updates.
        assert!(!app.adopt_subagent_models(&assignments));
        assert!(app.adopt_subagent_models(&[("call-2".to_string(), "codex:gpt-5.6".to_string())]));
        assert_eq!(app.subagent_model_for("call-2"), Some("codex:gpt-5.6"));
    }

    #[test]
    fn restored_execution_group_ids_do_not_create_empty_new_groups() {
        let mut app = app();
        let started_at = Instant::now();
        let mut restored = ExecutionGroup::new(7, started_at);
        restored.tools.push(ToolActivity::new(
            "old-call".to_string(),
            "read".to_string(),
            "{}".to_string(),
            started_at,
        ));
        app.transcript
            .push(TranscriptItem::ExecutionGroup(restored));
        app.next_execution_group_id = 1;

        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "new-call".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            started_at: started_at + Duration::from_millis(10),
        }));

        assert!(matches!(
            &app.transcript[0],
            TranscriptItem::ExecutionGroup(group)
                if group.id == 7
                    && group.tools.len() == 1
                    && group.tools[0].id == "old-call"
        ));
        assert!(matches!(
            &app.transcript[1],
            TranscriptItem::ExecutionGroup(group)
                if group.id == 8
                    && group.tools.len() == 1
                    && group.tools[0].id == "new-call"
        ));
        assert_eq!(app.next_execution_group_id, 9);
    }

    #[test]
    fn assistant_text_between_tool_phases_starts_new_execution_group() {
        let mut app = app();
        let started_at = Instant::now();

        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
            started_at,
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-1".to_string(),
            result: "ok".to_string(),
            success: true,
            finished_at: started_at + Duration::from_millis(5),
        }));
        app.reduce(AppAction::Agent(UiEvent::AssistantDelta(
            "checking one more thing".to_string(),
        )));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-2".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            started_at: started_at + Duration::from_millis(10),
        }));

        assert_eq!(app.transcript.len(), 3);
        assert!(matches!(
            &app.transcript[0],
            TranscriptItem::ExecutionGroup(group)
                if group.id == 1
                    && group.finished_at.is_some()
                    && group.tools.len() == 1
                    && group.tools[0].id == "call-1"
        ));
        assert!(matches!(
            &app.transcript[1],
            TranscriptItem::AssistantMessage { text } if text == "checking one more thing"
        ));
        assert!(matches!(
            &app.transcript[2],
            TranscriptItem::ExecutionGroup(group)
                if group.id == 2
                    && group.tools.len() == 1
                    && group.tools[0].id == "call-2"
        ));
        assert_eq!(app.active_execution_group_id, Some(2));
    }

    #[test]
    fn empty_assistant_delta_between_tool_phases_keeps_execution_group_open() {
        let mut app = app();
        let started_at = Instant::now();

        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
            started_at,
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-1".to_string(),
            result: "ok".to_string(),
            success: true,
            finished_at: started_at + Duration::from_millis(5),
        }));
        app.reduce(AppAction::Agent(UiEvent::AssistantDelta(String::new())));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-2".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            started_at: started_at + Duration::from_millis(10),
        }));

        assert_eq!(app.transcript.len(), 1);
        assert!(matches!(
            &app.transcript[0],
            TranscriptItem::ExecutionGroup(group)
                if group.id == 1
                    && group.finished_at.is_none()
                    && group.tools.len() == 2
                    && group.tools[0].id == "call-1"
                    && group.tools[1].id == "call-2"
        ));
        assert_eq!(app.active_execution_group_id, Some(1));
    }

    #[test]
    fn empty_reasoning_delta_between_tool_phases_keeps_execution_group_open() {
        let mut app = app();
        let started_at = Instant::now();

        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
            started_at,
        }));
        app.reduce(AppAction::Agent(UiEvent::ReasoningDelta(String::new())));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-2".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            started_at: started_at + Duration::from_millis(10),
        }));

        assert_eq!(app.transcript.len(), 1);
        assert!(matches!(
            &app.transcript[0],
            TranscriptItem::ExecutionGroup(group)
                if group.id == 1
                    && group.finished_at.is_none()
                    && group.tools.len() == 2
                    && group.tools[0].id == "call-1"
                    && group.tools[1].id == "call-2"
        ));
        assert_eq!(app.active_execution_group_id, Some(1));
    }

    #[test]
    fn tool_finish_updates_group_tool() {
        let mut app = app();
        let start = Instant::now();
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            started_at: start,
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-2".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
            started_at: start,
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-2".to_string(),
            result: "ok".to_string(),
            success: true,
            finished_at: start + Duration::from_millis(5),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-1".to_string(),
            result: "fail".to_string(),
            success: false,
            finished_at: start + Duration::from_millis(10),
        }));

        let Some(group) = app.execution_group(1) else {
            panic!("expected first group");
        };
        let call1 = group
            .tools
            .iter()
            .find(|activity| activity.id == "call-1")
            .unwrap_or_else(|| panic!("missing call-1"));
        assert_eq!(call1.status, ToolStatus::Failed);
        assert_eq!(call1.result.as_deref(), Some("fail"));

        let call2 = group
            .tools
            .iter()
            .find(|activity| activity.id == "call-2")
            .unwrap_or_else(|| panic!("missing call-2"));
        assert_eq!(call2.status, ToolStatus::Succeeded);
        assert_eq!(call2.result.as_deref(), Some("ok"));
    }

    #[test]
    fn agent_finished_closes_active_execution_group() {
        let mut app = app();
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            started_at: Instant::now(),
        }));

        app.reduce(AppAction::Runtime(RuntimeEvent::AgentFinished(Ok(
            AgentRunOutcome::Completed,
        ))));

        assert!(app.active_execution_group_id.is_none());
        let Some(group) = app.execution_group(1) else {
            panic!("expected group to still be in transcript");
        };
        assert!(group.finished_at.is_some());
        assert_eq!(app.task_state, TaskState::Idle);
    }

    #[test]
    fn execution_group_tool_order_preserves_start_order() {
        let mut app = app();
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
            started_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-2".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
            started_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-3".to_string(),
            name: "write".to_string(),
            arguments: "{}".to_string(),
            started_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-2".to_string(),
            result: "ok".to_string(),
            success: true,
            finished_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-3".to_string(),
            result: "ok".to_string(),
            success: true,
            finished_at: Instant::now(),
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-1".to_string(),
            result: "fail".to_string(),
            success: false,
            finished_at: Instant::now(),
        }));

        let group = app
            .execution_group(1)
            .unwrap_or_else(|| panic!("expected group"));
        let order = group.tool_indices().collect::<Vec<_>>();
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(group.tools[order[0]].id, "call-1");
        assert_eq!(group.tools[order[1]].id, "call-2");
        assert_eq!(group.tools[order[2]].id, "call-3");
        assert_eq!(group.tools[order[0]].status, ToolStatus::Failed);
        assert_eq!(group.tools[order[1]].status, ToolStatus::Succeeded);
        assert_eq!(group.tools[order[2]].status, ToolStatus::Succeeded);
    }

    #[test]
    fn permission_answer_resets_matching_bash_tool_timer() {
        let mut app = app();
        let started_at = Instant::now() - Duration::from_secs(30);

        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"sleep 5"}"#.to_string(),
            started_at,
        }));
        app.reduce(AppAction::ResetPermissionToolTimer {
            command: "sleep 5".to_string(),
            at: std::time::Instant::now(),
        });

        let activity = app
            .tool_activity("call-1")
            .unwrap_or_else(|| panic!("expected tool activity"));
        assert!(
            activity.started_at > started_at,
            "permission wait time should be excluded from the visible tool duration"
        );
        assert_eq!(
            app.active_tools.first().map(|(name, _)| name.as_str()),
            Some("bash")
        );
    }

    #[test]
    fn ordinary_modal_close_returns_to_input() {
        let mut app = app();
        app.focus = Focus::Transcript;

        app.reduce(AppAction::OpenModal(ModalKind::Detail(
            crate::tui::event::DetailModal::Help,
        )));
        app.reduce(AppAction::CloseModal);

        assert_eq!(app.focus, Focus::Input);
    }

    #[test]
    fn opens_latest_tool_detail() {
        let mut app = app();
        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{\"command\":\"date\"}".to_string(),
            started_at: Instant::now(),
        }));
        app.reduce(AppAction::OpenToolDetail("call-1".to_string()));

        assert_eq!(app.latest_tool_id().as_deref(), Some("call-1"));
        assert!(
            matches!(app.modal, Some(ModalKind::Detail(crate::tui::event::DetailModal::ToolDetail { ref tool_id })) if tool_id == "call-1")
        );
    }

    #[test]
    fn tool_duration_uses_event_timestamps() {
        let mut app = app();
        let started_at = Instant::now();
        let finished_at = started_at + Duration::from_millis(25);

        app.reduce(AppAction::Agent(UiEvent::ToolStarted {
            id: "call-1".to_string(),
            name: "read".to_string(),
            arguments: "{}".to_string(),
            started_at,
        }));
        app.reduce(AppAction::Agent(UiEvent::ToolFinished {
            id: "call-1".to_string(),
            result: "ok".to_string(),
            success: true,
            finished_at,
        }));

        let Some(activity) = app.tool_activity("call-1") else {
            panic!("expected tool activity");
        };
        assert_eq!(activity.duration(), Duration::from_millis(25));
    }

    #[test]
    fn command_output_and_view_toggles_update_state() {
        let mut app = app();

        app.reduce(AppAction::CommandOutput {
            kind: CommandOutputKind::Error,
            text: "boom".to_string(),
        });
        app.reduce(AppAction::ToggleView);
        app.reduce(AppAction::ToggleView);
        app.reduce(AppAction::ProviderModelChanged {
            provider: "codex".to_string(),
            model: "o3".to_string(),
            reasoning: ReasoningSelection::from_effort(ReasoningEffort::High),
        });

        assert!(matches!(
            app.transcript[0],
            TranscriptItem::CommandOutput {
                kind: CommandOutputKind::Error,
                ..
            }
        ));
        assert_eq!(app.view, View::Agent);
        assert_eq!(app.provider, "codex");
        assert_eq!(app.model, "o3");
        assert_eq!(app.reasoning, ReasoningSelection::High);
    }

    #[test]
    fn transient_status_sets_toast_without_transcript_item() {
        let mut app = app();

        app.reduce(AppAction::Agent(UiEvent::TransientStatus(
            "Settings saved.".to_string(),
        )));

        assert!(app.transcript.is_empty());
        assert_eq!(
            app.session_toast.as_ref().map(|toast| toast.text.as_str()),
            Some("Settings saved.")
        );
    }

    #[test]
    fn durable_status_remains_in_transcript() {
        let mut app = app();

        app.reduce(AppAction::Agent(UiEvent::Status(
            "Action required.".to_string(),
        )));

        assert_eq!(app.session_toast, None);
        assert!(matches!(
            app.transcript.as_slice(),
            [TranscriptItem::CommandOutput {
                kind: CommandOutputKind::Status,
                text,
            }] if text == "Action required."
        ));
    }

    #[test]
    fn successful_update_command_is_transient_but_errors_remain_durable() {
        let mut app = app();

        app.reduce(AppAction::Runtime(RuntimeEvent::UpdateCommandFinished {
            message: "Bonsai is up to date.".to_string(),
            kind: CommandOutputKind::Status,
            staged_notice: None,
        }));

        assert!(app.transcript.is_empty());
        assert_eq!(
            app.session_toast.as_ref().map(|toast| toast.text.as_str()),
            Some("Bonsai is up to date.")
        );

        app.reduce(AppAction::Runtime(RuntimeEvent::UpdateCommandFinished {
            message: "Update failed.".to_string(),
            kind: CommandOutputKind::Error,
            staged_notice: None,
        }));

        assert!(matches!(
            app.transcript.as_slice(),
            [TranscriptItem::CommandOutput {
                kind: CommandOutputKind::Error,
                text,
            }] if text == "Update failed."
        ));
    }

    #[test]
    fn transcript_click_selects_item_and_shift_extends() {
        let mut app = app();
        app.reduce(AppAction::SubmitInput("first".to_string()));
        app.reduce(AppAction::SubmitInput("second".to_string()));
        app.reduce(AppAction::SubmitInput("third".to_string()));
        app.focus = Focus::Transcript;
        app.reduce(AppAction::TranscriptClick {
            position: TranscriptPosition {
                item: 0,
                grapheme: 0,
                width: 80,
            },
            kind: SelectionKind::Position,
            extend: false,
            column: 0,
            row: 0,
        });
        assert_eq!(
            app.transcript_selection,
            Some(TranscriptSelection {
                anchor: TranscriptPosition {
                    item: 0,
                    grapheme: 0,
                    width: 80,
                },
                caret: TranscriptPosition {
                    item: 0,
                    grapheme: 0,
                    width: 80,
                },
            })
        );
        app.reduce(AppAction::TranscriptClick {
            position: TranscriptPosition {
                item: 2,
                grapheme: 0,
                width: 80,
            },
            kind: SelectionKind::Position,
            extend: true,
            column: 0,
            row: 0,
        });
        assert_eq!(
            app.transcript_selection,
            Some(TranscriptSelection {
                anchor: TranscriptPosition {
                    item: 0,
                    grapheme: 0,
                    width: 80,
                },
                caret: TranscriptPosition {
                    item: 2,
                    grapheme: 0,
                    width: 80,
                },
            })
        );
    }

    #[test]
    fn transcript_drag_extends_from_anchor() {
        let mut app = app();
        app.reduce(AppAction::SubmitInput("a".to_string()));
        app.reduce(AppAction::SubmitInput("b".to_string()));
        app.reduce(AppAction::SubmitInput("c".to_string()));
        app.focus = Focus::Transcript;
        app.reduce(AppAction::TranscriptClick {
            position: TranscriptPosition {
                item: 0,
                grapheme: 0,
                width: 80,
            },
            kind: SelectionKind::Position,
            extend: false,
            column: 0,
            row: 0,
        });
        app.reduce(AppAction::TranscriptDrag {
            position: TranscriptPosition {
                item: 2,
                grapheme: 0,
                width: 80,
            },
            scroll_delta: 0,
        });
        assert_eq!(
            app.transcript_selection,
            Some(TranscriptSelection {
                anchor: TranscriptPosition {
                    item: 0,
                    grapheme: 0,
                    width: 80,
                },
                caret: TranscriptPosition {
                    item: 2,
                    grapheme: 0,
                    width: 80,
                },
            })
        );
    }

    #[test]
    fn transcript_pointer_selection_does_not_schedule_focus_scroll() {
        let mut app = app();
        app.reduce(AppAction::SubmitInput("a".to_string()));
        app.transcript_scroll = 12;

        app.reduce(AppAction::TranscriptClick {
            position: TranscriptPosition {
                item: 0,
                grapheme: 0,
                width: 80,
            },
            kind: SelectionKind::Position,
            extend: false,
            column: 0,
            row: 0,
        });
        assert_eq!(app.transcript_focus, Some(0));
        assert_eq!(app.transcript_scroll, 12);
        assert!(!app.scroll_transcript_focus_into_view);

        app.scroll_transcript_focus_into_view = true;
        app.reduce(AppAction::TranscriptDrag {
            position: TranscriptPosition {
                item: 0,
                grapheme: 1,
                width: 80,
            },
            scroll_delta: 0,
        });
        assert_eq!(app.transcript_focus, Some(0));
        assert_eq!(app.transcript_scroll, 12);
        assert!(!app.scroll_transcript_focus_into_view);
    }

    #[test]
    fn transcript_drag_applies_edge_scroll_delta() {
        let mut app = app();
        app.reduce(AppAction::SubmitInput("abcdef".to_string()));
        app.transcript_scroll = 4;

        app.reduce(AppAction::TranscriptClick {
            position: TranscriptPosition {
                item: 0,
                grapheme: 0,
                width: 80,
            },
            kind: SelectionKind::Position,
            extend: false,
            column: 0,
            row: 0,
        });
        app.reduce(AppAction::TranscriptDrag {
            position: TranscriptPosition {
                item: 0,
                grapheme: 3,
                width: 80,
            },
            scroll_delta: -1,
        });
        assert_eq!(app.transcript_scroll, 3);

        app.reduce(AppAction::TranscriptDrag {
            position: TranscriptPosition {
                item: 0,
                grapheme: 4,
                width: 80,
            },
            scroll_delta: 2,
        });
        assert_eq!(app.transcript_scroll, 5);
    }

    #[test]
    fn plan_drag_applies_edge_scroll_delta() {
        let mut app = app();
        app.plan.edit().add_task("copy this task");
        app.plan_scroll = 2;
        app.reduce(AppAction::PlanClick {
            position: PlanPosition {
                line: 1,
                grapheme: 0,
                width: 80,
            },
            kind: SelectionKind::Position,
            column: 0,
            row: 0,
        });
        app.reduce(AppAction::PlanDrag {
            position: PlanPosition {
                line: 1,
                grapheme: 4,
                width: 80,
            },
            scroll_delta: -1,
        });
        assert_eq!(app.plan_scroll, 1);
    }

    #[test]
    fn authorize_provider_picker_move_clamps_to_entries() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::Picker(
            crate::tui::event::PickerModal::AuthorizeProviderPicker {
                providers: vec![
                    authorize_provider_entry("opencode", "OpenCode Go"),
                    authorize_provider_entry("codex", "Codex"),
                ],
                query: String::new(),
                cursor: 20,
            },
        )));

        assert!(matches!(
            app.modal,
            Some(ModalKind::Picker(
                crate::tui::event::PickerModal::AuthorizeProviderPicker { cursor: 1, .. }
            ))
        ));
        app.reduce(AppAction::AuthorizeProviderPickerMove(-20));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Picker(
                crate::tui::event::PickerModal::AuthorizeProviderPicker { cursor: 0, .. }
            ))
        ));
    }

    #[test]
    fn authorize_provider_picker_filters_by_query_and_clamps() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::Picker(
            crate::tui::event::PickerModal::AuthorizeProviderPicker {
                providers: vec![
                    authorize_provider_entry("opencode", "OpenCode Go"),
                    authorize_provider_entry("codex", "Codex"),
                    authorize_provider_entry("qwencloud", "Qwen Cloud API"),
                ],
                query: String::new(),
                cursor: 2,
            },
        )));

        // Typing narrows to the single Qwen row; the cursor resets to it and
        // End can't move past the filtered length.
        for ch in "qwen".chars() {
            app.reduce(AppAction::AuthorizeProviderPickerInputChar(ch));
        }
        app.reduce(AppAction::AuthorizeProviderPickerMove(i16::MAX));
        let Some(ModalKind::Picker(crate::tui::event::PickerModal::AuthorizeProviderPicker {
            query,
            cursor,
            ..
        })) = &app.modal
        else {
            panic!("authorize picker should be open");
        };
        assert_eq!(query, "qwen");
        assert_eq!(*cursor, 0);

        // Backspacing widens the match set again.
        app.reduce(AppAction::AuthorizeProviderPickerInputBackspace);
        let Some(ModalKind::Picker(crate::tui::event::PickerModal::AuthorizeProviderPicker {
            query,
            ..
        })) = &app.modal
        else {
            panic!("authorize picker should be open");
        };
        assert_eq!(query, "qwe");
    }

    #[test]
    fn provider_manager_search_narrows_then_exit_and_clear() {
        use crate::tui::provider_manager::{ProviderManagerRow, ProviderOrigin};
        let row = |id: &str, name: &str| ProviderManagerRow {
            connection_id: id.to_string(),
            display_name: name.to_string(),
            origin: ProviderOrigin::BuiltIn,
            enabled: true,
            authorized: false,
            current: false,
            model_count: 0,
            discovery: crate::model_catalog::DiscoveryKind::Generic,
            base_url: String::new(),
            credential_label: None,
            auth_hint: None,
        };
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::Manager(
            crate::tui::event::ManagerModal::ProviderManager {
                rows: vec![
                    row("opencode", "OpenCode Go"),
                    row("qwencloud", "Qwen Cloud API"),
                    row("qwencloud-token-plan", "Qwen Cloud Token Plan"),
                ],
                filter: String::new(),
                searching: false,
                cursor: 0,
            },
        )));

        // Before `/`, letters are shortcuts, not filter input.
        app.reduce(AppAction::ProviderManager(
            crate::tui::event::ProviderManagerAction::SearchChar('q'),
        ));
        let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
            filter,
            ..
        })) = &app.modal
        else {
            panic!("manager open");
        };
        assert!(filter.is_empty(), "typing is inert until search begins");

        // `/` engages search; typing narrows to the two Qwen rows and the
        // cursor can't leave the filtered range.
        app.reduce(AppAction::ProviderManager(
            crate::tui::event::ProviderManagerAction::BeginSearch,
        ));
        for ch in "qwen".chars() {
            app.reduce(AppAction::ProviderManager(
                crate::tui::event::ProviderManagerAction::SearchChar(ch),
            ));
        }
        app.reduce(AppAction::ProviderManager(
            crate::tui::event::ProviderManagerAction::Move(i16::MAX),
        ));
        let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
            filter,
            searching,
            cursor,
            rows,
        })) = &app.modal
        else {
            panic!("manager open");
        };
        assert_eq!(filter, "qwen");
        assert!(*searching);
        assert_eq!(
            crate::tui::provider_manager::provider_manager_filtered(rows, filter).len(),
            2
        );
        assert_eq!(*cursor, 1, "End lands on the last filtered row, not row 2");

        // Exit search keeps the filter applied (so a/e/d act on the narrowing).
        app.reduce(AppAction::ProviderManager(
            crate::tui::event::ProviderManagerAction::SearchExit,
        ));
        let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
            filter,
            searching,
            ..
        })) = &app.modal
        else {
            panic!("manager open");
        };
        assert_eq!(filter, "qwen");
        assert!(!*searching);

        // Clear resets to the full list at the top.
        app.reduce(AppAction::ProviderManager(
            crate::tui::event::ProviderManagerAction::ClearFilter,
        ));
        let Some(ModalKind::Manager(crate::tui::event::ManagerModal::ProviderManager {
            filter,
            cursor,
            ..
        })) = &app.modal
        else {
            panic!("manager open");
        };
        assert!(filter.is_empty());
        assert_eq!(*cursor, 0);
    }

    #[test]
    fn review_scope_picker_move_wraps_and_clamps() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::Picker(
            crate::tui::event::PickerModal::ReviewScopePicker { cursor: 5 },
        )));

        // Overshoot clamps to the last scope (index 2).
        assert!(matches!(
            app.modal,
            Some(ModalKind::Picker(
                crate::tui::event::PickerModal::ReviewScopePicker { cursor: 2 }
            ))
        ));
        // Move down past the end wraps/clamps to the last entry.
        app.reduce(AppAction::ReviewScopePickerMove(1));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Picker(
                crate::tui::event::PickerModal::ReviewScopePicker { cursor: 2 }
            ))
        ));
        // Move back to the first entry.
        app.reduce(AppAction::ReviewScopePickerMove(-5));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Picker(
                crate::tui::event::PickerModal::ReviewScopePicker { cursor: 0 }
            ))
        ));
    }

    #[test]
    fn theme_picker_open_and_move_clamp_to_themes() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::Picker(
            crate::tui::event::PickerModal::ThemePicker {
                cursor: usize::MAX,
                original_theme: "forest".to_string(),
            },
        )));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Picker(crate::tui::event::PickerModal::ThemePicker { cursor, .. }))
                if cursor == crate::tui::theme::theme_count().saturating_sub(1)
        ));

        app.reduce(AppAction::Modal(
            crate::tui::event::ModalAction::ThemePicker(
                crate::tui::event::ThemePickerAction::Move(i16::MIN),
            ),
        ));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Picker(
                crate::tui::event::PickerModal::ThemePicker { cursor: 0, .. }
            ))
        ));

        app.reduce(AppAction::Modal(
            crate::tui::event::ModalAction::ThemePicker(
                crate::tui::event::ThemePickerAction::Move(i16::MAX),
            ),
        ));
        assert!(matches!(
            app.modal,
            Some(ModalKind::Picker(crate::tui::event::PickerModal::ThemePicker { cursor, .. }))
                if cursor == crate::tui::theme::theme_count().saturating_sub(1)
        ));
    }

    #[test]
    fn set_active_saved_plan_tracks_and_clears_association() {
        let mut app = app();

        app.reduce(AppAction::SetActiveSavedPlan(Some(
            crate::storage::SavedPlanId::from_raw(42),
        )));
        assert_eq!(
            app.active_saved_plan_session_id,
            Some(crate::storage::SavedPlanId::from_raw(42))
        );

        app.reduce(AppAction::SetActiveSavedPlan(None));
        assert_eq!(app.active_saved_plan_session_id, None);
    }

    #[test]
    fn scroll_delta_does_not_wrap_past_i16_range() {
        let mut app = app();
        app.transcript_autoscroll = false;
        app.transcript_scroll = 40_000;

        app.reduce(AppAction::ScrollCurrent(1));

        assert_eq!(app.transcript_scroll, 40_001);
    }

    #[test]
    fn mode_picker_open_seeds_rows_from_current_state() {
        let mut app = app();
        app.approval_level = crate::tool::ApprovalLevel::AutoAccept;
        app.reduce(AppAction::OpenModal(ModalKind::Picker(
            crate::tui::event::PickerModal::ModePicker {
                rows: Vec::new(),
                cursor: 0,
            },
        )));
        match &app.modal {
            Some(ModalKind::Picker(crate::tui::event::PickerModal::ModePicker {
                rows,
                cursor,
            })) => {
                // 3 axes: autonomy has 1 value row, self-review has 1,
                // sandbox has 2.
                assert_eq!(rows.len(), 3 + 4, "rows: {rows:?}");
                // Opens on the first value row, not the leading header.
                assert_eq!(*cursor, 1);
                assert!(rows[*cursor].is_value(), "cursor must start on a value row");
                let autonomy = rows
                    .iter()
                    .find_map(|row| match row {
                        ModeRow::Value {
                            axis: ModeAxisId::Autonomy,
                            values,
                            current,
                            ..
                        } => Some((values.get(*current).copied().unwrap_or(""), *current)),
                        _ => None,
                    })
                    .expect("autonomy value row");
                assert_eq!(autonomy.0, "auto-accept");
            }
            other => panic!("expected ModePicker modal, got {other:?}"),
        }
    }

    #[test]
    fn mode_picker_move_clamps_to_value_rows() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::Picker(
            crate::tui::event::PickerModal::ModePicker {
                rows: Vec::new(),
                cursor: 0,
            },
        )));
        let rows_len = match &app.modal {
            Some(ModalKind::Picker(crate::tui::event::PickerModal::ModePicker {
                rows, ..
            })) => rows.len(),
            _ => panic!(),
        };
        // Overshoot clamps to the last row, which is always a value row.
        app.reduce(AppAction::Modal(
            crate::tui::event::ModalAction::ModePicker(crate::tui::event::ModePickerAction::Move(
                100,
            )),
        ));
        let cursor = match &app.modal {
            Some(ModalKind::Picker(crate::tui::event::PickerModal::ModePicker {
                cursor, ..
            })) => *cursor,
            _ => panic!(),
        };
        assert_eq!(cursor, rows_len - 1);
        // Undershoot clamps to the first value row (index 1), skipping the
        // leading Autonomy header at index 0.
        app.reduce(AppAction::Modal(
            crate::tui::event::ModalAction::ModePicker(crate::tui::event::ModePickerAction::Move(
                -100,
            )),
        ));
        let cursor = match &app.modal {
            Some(ModalKind::Picker(crate::tui::event::PickerModal::ModePicker {
                cursor, ..
            })) => *cursor,
            _ => panic!(),
        };
        assert_eq!(cursor, 1);
    }

    #[test]
    fn mode_picker_move_skips_headers() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::Picker(
            crate::tui::event::PickerModal::ModePicker {
                rows: Vec::new(),
                cursor: 0,
            },
        )));
        // Rows start [Header, Value(level), Header, Value(self-review), ...].
        // Stepping down from the autonomy value (1) must jump over the
        // Self-review header (2) and land on its value (3).
        app.reduce(AppAction::Modal(
            crate::tui::event::ModalAction::ModePicker(crate::tui::event::ModePickerAction::Move(
                1,
            )),
        ));
        let cursor = match &app.modal {
            Some(ModalKind::Picker(crate::tui::event::PickerModal::ModePicker {
                rows,
                cursor,
            })) => {
                assert!(rows[*cursor].is_value(), "must land on a value row");
                *cursor
            }
            _ => panic!(),
        };
        assert_eq!(cursor, 3);
    }

    #[test]
    fn mode_picker_cycle_advances_value_rows_only() {
        let mut app = app();
        app.reduce(AppAction::OpenModal(ModalKind::Picker(
            crate::tui::event::PickerModal::ModePicker {
                rows: Vec::new(),
                cursor: 0,
            },
        )));
        // The picker opens on the first value row and navigation only ever
        // rests on value rows, so move down to land on another value row.
        app.reduce(AppAction::Modal(
            crate::tui::event::ModalAction::ModePicker(crate::tui::event::ModePickerAction::Move(
                1,
            )),
        ));
        let row = match &app.modal {
            Some(ModalKind::Picker(crate::tui::event::PickerModal::ModePicker {
                rows,
                cursor,
            })) => rows[*cursor].clone(),
            _ => panic!(),
        };

        // Production cycles in the runtime handler (`handle_runtime_action`) via
        // `ModeRow::cycled` — `app.reduce(Modal(ModePicker(Cycle)))` is intentionally
        // unhandled by the reducer. Exercise the underlying math here: a Value
        // row advances modulo its value count, and a Header stays put.
        let (before, count) = match &row {
            ModeRow::Value {
                current, values, ..
            } => (*current, values.len()),
            _ => panic!("expected a value row after moving off the header"),
        };
        let after = match row.cycled(1) {
            ModeRow::Value { current, .. } => current,
            _ => panic!("cycling a value row must stay a value row"),
        };
        assert_eq!(after, (before + 1) % count);
        assert!(matches!(
            ModeRow::Header("Autonomy").cycled(1),
            ModeRow::Header(_)
        ));
    }
}
