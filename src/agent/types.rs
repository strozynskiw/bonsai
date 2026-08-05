//! Public data types in the agent's API surface: run results, the queued-input
//! channel payloads, and the compaction request/report vocabulary.

use async_openai::types::chat::ChatCompletionRequestMessage;
use tokio_util::sync::CancellationToken;

use crate::model_catalog::ConnectionId;
use crate::provider::{EstimateConfidence, InputCacheUsage, TokenCounterKind};
use crate::storage::SessionId;

/// An image attached to a user turn, already encoded and ready to become an
/// image content part. `base64` is the STANDARD-encoded image bytes and `mime`
/// their media type (always `image/png` for clipboard pastes today).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    pub mime: String,
    pub base64: String,
    /// Encoded (pre-base64) byte length, for size display and token estimation.
    pub byte_len: usize,
}

/// One user turn's worth of input: the model-facing text plus any attached
/// images. `text` already has paste-chip placeholders expanded (see
/// `Composer::submission`); mention expansion still runs on it downstream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<ImageAttachment>,
}

impl UserInput {
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }
}

/// A user message queued by the composer while a run is in flight, identified
/// so the UI can later cancel it before it is sent. `display_text` is the
/// compact placeholder shown while queued; `transcript_text` is what the
/// transcript row becomes once sent (paste chips expanded to their content);
/// `input` is the expanded model payload (tagged chip payloads, images
/// attached).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedUserMessage {
    pub id: u64,
    pub display_text: String,
    pub transcript_text: String,
    pub input: UserInput,
}

/// A command on the queued-message channel: enqueue a new message or cancel a
/// previously queued one by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueuedUserMessageCommand {
    Send(QueuedUserMessage),
    Cancel(u64),
}

/// Outcome of an agent run: either the model produced a final answer, or the
/// run was interrupted (cancelled) with whatever partial content was produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRunResult {
    Completed(String),
    /// The model attempted to finish, but structured task evidence still
    /// showed unresolved work after Bonsai's single bounded continuation.
    Incomplete {
        output: String,
        failure: super::CompletionGuardFailure,
    },
    Interrupted(String),
    Waiting(WaitReason),
}

/// Nonterminal reason an agent released its foreground task slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitReason {
    Subagents(Vec<String>),
    /// Waiting for another live session to finish its current run.
    Peer(PeerWait),
    /// Waiting for one versioned process-local background task or PTY update.
    BackgroundWork(crate::background_wake::BackgroundWorkWait),
    /// The coding agent confirmed a fresh planning continuation. Before
    /// switching persona, the TUI protects any non-empty canvas in the saved
    /// plan library and clears the working canvas. Failure leaves the coding
    /// persona and canvas untouched.
    StartNewPlan,
}

/// Exact one-shot peer wait whose done notice may resume a parked turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerWait {
    pub session_id: SessionId,
    pub subscription_id: i64,
}

/// Origin and trust class of a message injected into model context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MessageProvenance {
    Human,
    Harness,
    ProjectState,
    Peer,
    Background,
    Terminal,
    UntrustedData,
}

impl MessageProvenance {
    pub(crate) const fn wire_name(self) -> Option<&'static str> {
        match self {
            Self::Human => None,
            Self::Harness => Some("bonsai_harness"),
            Self::ProjectState => Some(crate::context::PROJECT_STATE_MESSAGE_NAME),
            Self::Peer => Some("bonsai_peer"),
            Self::Background => Some("bonsai_background"),
            Self::Terminal => Some("bonsai_terminal"),
            Self::UntrustedData => Some("bonsai_untrusted"),
        }
    }

    pub(crate) const fn is_trusted_system(self) -> bool {
        matches!(self, Self::Harness)
    }
}

/// Outcome of one network attempt inside a logical provider turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderAttemptOutcome {
    Completed,
    Incomplete,
    Interrupted,
    Failed,
}

/// Persisted diagnostics for one provider attempt, including failed retries.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Deserialize, serde::Serialize)]
pub struct ProviderAttemptReport {
    pub(crate) attempt: usize,
    pub(crate) outcome: ProviderAttemptOutcome,
    pub(crate) latency_ms: u64,
    pub(crate) assistant_chars: usize,
    pub(crate) reasoning_chars: usize,
    pub(crate) finish_reason: Option<crate::provider::FinishReason>,
    pub(crate) error_class: Option<String>,
    pub(crate) backoff_ms: Option<u64>,
    pub(crate) prompt_tokens: Option<u64>,
    pub(crate) completion_tokens: Option<u64>,
    pub(crate) cache_read_input_tokens: Option<u64>,
    pub(crate) cache_creation_input_tokens: Option<u64>,
    pub(crate) cache_measured_input_tokens: Option<u64>,
}

/// Persistable snapshot of the model-facing message buffer and its stable row
/// IDs. The IDs are sidecar metadata only; they are never serialized into the
/// provider payload.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ContextMessageSnapshot {
    pub(crate) messages: Vec<ChatCompletionRequestMessage>,
    pub(crate) ids: Vec<String>,
}

pub(super) fn format_context_message_id(seq: u64) -> String {
    format!("msg-{seq}")
}

pub(super) fn context_message_seq(id: &str) -> Option<u64> {
    let rest = id.strip_prefix("msg-")?;
    let digits = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Why context compaction is being run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionMode {
    /// The user explicitly requested `/compact`.
    Manual,
    /// The agent is compacting because the prompt would exceed the window.
    Automatic,
}

impl CompactionMode {
    /// Machine-readable label recorded on a repack event, one source for what
    /// was previously two parallel `match request.mode` arms.
    pub(super) fn repack_reason_label(self) -> &'static str {
        match self {
            Self::Manual => "manual-compaction",
            Self::Automatic => "automatic-compaction",
        }
    }

    /// The [`ContextRewriteKind`] a compaction of this mode counts as.
    pub(super) fn rewrite_kind(self) -> ContextRewriteKind {
        match self {
            Self::Manual => ContextRewriteKind::Manual,
            Self::Automatic => ContextRewriteKind::Compaction,
        }
    }
}

/// How the compacted summary was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionSummarySource {
    /// No mutation was applied; metrics describe the planned rewrite.
    Preview,
    /// A hidden provider call produced the summary.
    Provider,
    /// A deterministic reversible marker was used.
    Deterministic,
    /// No summary row was needed.
    None,
}

/// Which summary strategy context compaction should use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CompactionSummaryPolicy {
    /// Ask the provider first, then use a deterministic summary if that fails.
    #[default]
    ProviderThenDeterministic,
    /// Require a provider-produced summary.
    ProviderOnly,
    /// Skip the provider and use the deterministic summary.
    DeterministicOnly,
}

/// Request options for [`Agent::compact_context`].
#[derive(Debug, Clone)]
pub struct CompactionRequest {
    pub mode: CompactionMode,
    pub preview: bool,
    pub target_tokens: Option<usize>,
    pub summary_policy: CompactionSummaryPolicy,
    pub cancellation_token: CancellationToken,
}

impl CompactionRequest {
    pub fn manual(preview: bool, cancellation_token: CancellationToken) -> Self {
        Self {
            mode: CompactionMode::Manual,
            preview,
            target_tokens: None,
            summary_policy: CompactionSummaryPolicy::ProviderThenDeterministic,
            cancellation_token,
        }
    }

    pub fn automatic(target_tokens: usize, cancellation_token: CancellationToken) -> Self {
        Self {
            mode: CompactionMode::Automatic,
            preview: false,
            target_tokens: Some(target_tokens),
            summary_policy: CompactionSummaryPolicy::ProviderThenDeterministic,
            cancellation_token,
        }
    }

    pub fn with_target_tokens(mut self, target_tokens: usize) -> Self {
        self.target_tokens = Some(target_tokens);
        self
    }

    pub fn with_summary_policy(mut self, summary_policy: CompactionSummaryPolicy) -> Self {
        self.summary_policy = summary_policy;
        self
    }
}

/// Auditable result of a context compaction or preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    pub mode: CompactionMode,
    pub preview: bool,
    pub summary_source: CompactionSummarySource,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub target_tokens: usize,
    pub before_messages: usize,
    pub after_messages: usize,
    pub messages_omitted: usize,
    pub tool_outputs_stubbed: usize,
    pub summary_source_available: bool,
    pub repacked: bool,
    pub target_reached: bool,
}

impl CompactionReport {
    pub fn has_changes(&self) -> bool {
        self.messages_omitted > 0 || self.tool_outputs_stubbed > 0
    }
}

/// Cumulative session usage exposed to callers (`/ctx`, `/perf`, persistence):
/// real token totals plus priced actual and no-cache costs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct UsageTotals {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_micros: Option<u64>,
    pub no_cache_cost_micros: Option<u64>,
    pub input_cache: Option<InputCacheUsage>,
}

/// The provider/model a turn actually ran against. Carried alongside the
/// provider handle (which erases identity behind `dyn Provider`) so each
/// [`UsageTurn`] can be attributed to the model that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActiveModelIdentity {
    pub provider_id: ConnectionId,
    pub model: String,
}

/// Independent provider-conversation category used for cache accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLaneKind {
    Parent,
    Subagent,
    SelfReview,
    Compaction,
}

impl ExecutionLaneKind {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Subagent => "subagent",
            Self::SelfReview => "self_review",
            Self::Compaction => "compaction",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Option<Self> {
        match value {
            "parent" => Some(Self::Parent),
            "subagent" => Some(Self::Subagent),
            "self_review" => Some(Self::SelfReview),
            "compaction" => Some(Self::Compaction),
            _ => None,
        }
    }
}

/// Stable identity of one independent provider conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecutionLane {
    pub(crate) kind: ExecutionLaneKind,
    pub(crate) id: String,
}

impl ExecutionLane {
    pub(crate) fn parent(id: impl Into<String>) -> Self {
        Self::new(ExecutionLaneKind::Parent, id)
    }

    pub(crate) fn subagent(id: impl Into<String>) -> Self {
        Self::new(ExecutionLaneKind::Subagent, id)
    }

    pub(crate) fn self_review(id: impl Into<String>) -> Self {
        Self::new(ExecutionLaneKind::SelfReview, id)
    }

    pub(crate) fn compaction(id: impl Into<String>) -> Self {
        Self::new(ExecutionLaneKind::Compaction, id)
    }

    fn new(kind: ExecutionLaneKind, id: impl Into<String>) -> Self {
        let id = id.into();
        debug_assert!(!id.trim().is_empty(), "execution lane id must not be empty");
        Self { kind, id }
    }

    pub(crate) fn label(&self) -> String {
        format!("{}:{}", self.kind.as_db_str(), self.id)
    }
}

impl Default for ExecutionLane {
    fn default() -> Self {
        Self::parent("local")
    }
}

/// Whether a provider response reported usage, omitted it, or was interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageTurnStatus {
    Reported,
    Missing,
    Interrupted,
}

impl UsageTurnStatus {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Missing => "missing",
            Self::Interrupted => "interrupted",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Self {
        match value {
            "reported" => Self::Reported,
            "interrupted" => Self::Interrupted,
            _ => Self::Missing,
        }
    }
}

/// Context rewrite that may explain a cache-cold provider turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextRewriteKind {
    #[default]
    None,
    Gc,
    Compaction,
    Manual,
    /// A batched episode eviction replaced one or more closed episodes' whole
    /// message groups with card markers.
    Episode,
}

impl ContextRewriteKind {
    pub(crate) const fn as_db_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Gc => "gc",
            Self::Compaction => "compaction",
            Self::Manual => "manual",
            Self::Episode => "episode",
        }
    }

    pub(crate) fn from_db_str(value: &str) -> Self {
        match value {
            "gc" => Self::Gc,
            "compaction" => Self::Compaction,
            "manual" => Self::Manual,
            "episode" => Self::Episode,
            _ => Self::None,
        }
    }

    pub(crate) const fn priority(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Gc => 1,
            Self::Manual => 2,
            Self::Episode => 3,
            Self::Compaction => 4,
        }
    }
}

/// Persisted usage/cache diagnostics for one provider response.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UsageTurn {
    pub(crate) seq: usize,
    pub(crate) lane_kind: ExecutionLaneKind,
    pub(crate) lane_id: String,
    pub(crate) lane_seq: usize,
    pub(crate) parent_tool_call_id: Option<String>,
    pub(crate) launch_group_id: Option<String>,
    pub(crate) status: UsageTurnStatus,
    pub(crate) finish_reason: Option<crate::provider::FinishReason>,
    pub(crate) reasoning_chars: usize,
    pub(crate) provider_attempts: Vec<ProviderAttemptReport>,
    /// Provider/model this turn ran against. `None` on rows persisted before
    /// per-turn attribution existed (and in tests without a run selection);
    /// analytics fall back to the session's last-used model for those.
    pub(crate) provider_id: Option<String>,
    pub(crate) model: Option<String>,
    /// Reasoning setting that governed this exact provider request. `None` is
    /// reserved for rows persisted before per-turn reasoning attribution.
    pub(crate) effective_reasoning: Option<crate::provider::ReasoningSelection>,
    pub(crate) prompt_tokens: Option<u64>,
    pub(crate) completion_tokens: Option<u64>,
    pub(crate) cache_read_input_tokens: Option<u64>,
    pub(crate) cache_creation_input_tokens: Option<u64>,
    pub(crate) cache_measured_input_tokens: Option<u64>,
    pub(crate) turn_cost_micros: Option<u64>,
    pub(crate) no_cache_cost_micros: Option<u64>,
    pub(crate) estimated_prompt_tokens: Option<usize>,
    pub(crate) estimate_source: Option<TokenCounterKind>,
    pub(crate) estimate_confidence: Option<EstimateConfidence>,
    pub(crate) tool_schema_tokens: Option<usize>,
    pub(crate) tool_schema_hash: Option<String>,
    pub(crate) tool_schema_names: Vec<String>,
    pub(crate) request_body_bytes: Option<usize>,
    pub(crate) request_body_hash: Option<String>,
    pub(crate) cache_mechanism: Option<String>,
    pub(crate) cache_route_fingerprint: Option<String>,
    pub(crate) expected_cacheable_percent: Option<u64>,
    pub(crate) actual_cache_read_percent: Option<u64>,
    pub(crate) local_reusable_prefix_tokens: Option<usize>,
    pub(crate) local_reusable_prefix_percent: Option<u64>,
    pub(crate) cacheable_prefix_tokens: Option<usize>,
    pub(crate) volatile_tail_tokens: Option<usize>,
    pub(crate) context_window_tokens: Option<usize>,
    pub(crate) rewrite_kind: ContextRewriteKind,
    pub(crate) rewrite_saved_tokens: Option<usize>,
    /// The evicted episode a `rewrite_kind == Episode` turn is attributed to,
    /// when exactly one episode caused the batch; `None` for multi-episode
    /// batches (their per-episode deltas live on the episode rows).
    pub(crate) episode_seq: Option<usize>,
    /// Wall-clock time this turn was recorded (real per-request timing; not the
    /// persistence flush time, which is shared across every row of a snapshot).
    pub(crate) created_at_ms: i64,
    /// Total provider request latency, including retries.
    pub(crate) latency_ms: Option<u64>,
    /// Time to first output token (assistant or reasoning delta).
    pub(crate) ttft_ms: Option<u64>,
    /// Fingerprint of the byte-stable system prefix this turn sent; an
    /// adjacent-turn change flags prompt-cache prefix churn.
    pub(crate) prefix_hash: Option<String>,
    pub(crate) inspection_executed: usize,
    pub(crate) inspection_reused: usize,
    pub(crate) inspection_rejected: usize,
    pub(crate) inspection_returned_chars: usize,
    pub(crate) inspection_avoided_chars: usize,
    pub(crate) delegated_parent_overlap: usize,
}

impl UsageTurn {
    pub(crate) fn attach_parent_context(
        &mut self,
        parent_tool_call_id: &str,
        launch_group_id: Option<&str>,
    ) {
        self.parent_tool_call_id = Some(parent_tool_call_id.to_string());
        self.launch_group_id = launch_group_id.map(str::to_string);
    }
}
