//! Grouped sub-state of [`AppState`](super::AppState).

use std::collections::{BTreeMap, VecDeque};

use ratatui::text::Line;

use super::{DeferredCommand, Focus, ModalSelection, PhaseAdvance, PlanExecution, QueuedInput};

/// Scroll offsets, follow flags and pending scroll requests.
pub(crate) struct ScrollState {
    pub(crate) transcript_scroll: usize,
    pub(crate) sidebar_scroll: u16,
    pub(crate) plan_scroll: u16,
    pub(crate) composer_scroll: u16,
    pub(crate) composer_follow: bool,
    pub(crate) pending_composer_page: Option<i16>,
    pub(crate) pending_composer_extend: Option<i16>,
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
}

/// Queued follow-up inputs, deferred commands and peer/background wait bookkeeping.
pub(crate) struct QueueState {
    pub(crate) queued_inputs: Vec<QueuedInput>,
    /// Recently withdrawn queue IDs, retained long enough to detect a late
    /// acknowledgement from the running agent.
    pub(crate) removed_queued_input_ids: VecDeque<u64>,
    pub(crate) next_queued_input_id: u64,
    pub(crate) deferred_commands: Vec<DeferredCommand>,
    pub(crate) next_deferred_command_id: u64,
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
    /// Exact process-local wait whose claimed version may resume this run.
    pub(crate) waiting_for_background_work: Option<crate::background_wake::BackgroundWorkWait>,
}

/// Phased plan execution state and one-shot plan hand-off flags.
pub(crate) struct PlanRunState {
    /// When a phased plan is being implemented, which phase is currently
    /// running. `None` for flat plans or when no plan run is in flight.
    pub(crate) plan_execution: Option<PlanExecution>,
    /// Set by the `AgentFinished` reducer when a phased run terminates, then
    /// consumed by the event loop to auto-advance to the next phase or halt.
    pub(crate) phase_advance: Option<PhaseAdvance>,
    /// Set by the `AgentFinished` reducer when the coding agent asks to start a
    /// fresh plan, then consumed by the event loop after it protects and clears
    /// the old canvas. `None` on any other finish.
    pub(crate) pending_start_new_plan: bool,
    /// Armed by the fresh-plan transition and cleared by the first idle frame
    /// that can see whether the planning agent actually wrote a draft. A canvas
    /// that is still empty at that point means the turn produced no plan, which
    /// used to pass silently: the previous canvas was already saved and cleared.
    pub(crate) fresh_plan_awaiting_draft: bool,
    /// One-shot authority for the literal natural-language confirmations
    /// `go`/`continue` to enter the existing `/start` plan handoff. It is armed
    /// only by a completed planning run and never applies to permission,
    /// destructive, privileged, or external-action prompts.
    pub(crate) pending_safe_plan_start_confirmation: bool,
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
}

/// Modal body state: scroll, selection, render caches and picker hand-offs.
pub(crate) struct ModalUiState {
    pub(crate) modal_return_focus: Option<Focus>,
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
}

/// Session-scoped presentation/policy flags mirrored from the agent or settings.
pub(crate) struct ModeFlags {
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
    /// Self-review-before-done policy, mirrored from the live agent for `/mode`.
    pub(crate) self_review_mode: crate::self_review::SelfReviewMode,
    /// The autonomy / approval level (mirrors the shared `YoloMode` holder).
    /// Set via `/autonomy`, `/yolo`, or Alt+M; surfaced in the status bar.
    pub(crate) approval_level: crate::tool::ApprovalLevel,
}
