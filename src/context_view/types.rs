use std::fmt;

use serde::{Deserialize, Serialize};

use crate::agent::{AgentMode, ContextRewriteKind, UsageTurn, UsageTurnStatus};
use crate::provider::{
    EstimateConfidence, PromptEstimate, PromptEstimator, ReasoningSelection, TokenCounterKind,
};
use crate::tool::{OutputTruncationContext, ReadEvidence};

use super::*;

/// The role of a context entry, for grouping and colouring in the `/ctx` view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextRole {
    System,
    User,
    Assistant,
    Tool,
    ToolSchema,
}

impl ContextRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool result",
            Self::ToolSchema => "tool schemas",
        }
    }
}

/// One message currently in the context window: its role, estimated tokens, and
/// a readable rendering of its content (capped for display).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextEntry {
    pub role: ContextRole,
    pub tokens: usize,
    pub text: String,
}

/// Durable category for a human-readable `/ctx` provenance edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContextSourceKind {
    ContextMessage,
    ToolCall,
    ToolInput,
    ToolResult,
    SystemPrompt,
    ProjectContext,
    SteeringFile,
    ToolSchema,
    CompactionSource,
    PlanState,
    TodoState,
    ComposerDraft,
    QueuedInput,
    BackgroundTask,
    Framing,
}

/// Human-readable source metadata explaining why a context ledger node exists.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextSourceRef {
    pub kind: ContextSourceKind,
    pub id: String,
    pub label: String,
    pub detail: Option<String>,
    pub restorable: bool,
}

impl ContextSourceRef {
    pub(crate) fn new(
        kind: ContextSourceKind,
        id: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            id: id.into(),
            label: label.into(),
            detail: None,
            restorable: false,
        }
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub(crate) fn restorable(mut self) -> Self {
        self.restorable = true;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextNodeKind {
    SystemRoot,
    ChatRoot,
    ToolsRoot,
    ToolSchemasRoot,
    PendingRoot,
    NotSentRoot,
    Persona,
    ProjectEnvironment,
    VolatileState,
    ProjectInstructions,
    SteeringFile,
    MemoryIndex,
    Summary,
    ChatMessage,
    Attachment,
    Mention,
    ToolCall,
    ToolInput,
    ToolResult,
    Stdout,
    Stderr,
    OutputText,
    ReadFreshness,
    Diff,
    Image,
    TruncationFile,
    TaskStatus,
    ToolSchema,
    Background,
    PlanState,
    TodoState,
    QueuedInput,
    ComposerDraft,
    FramingAdjustment,
}

impl ContextNodeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::SystemRoot => "system",
            Self::ChatRoot => "chat",
            Self::ToolsRoot => "tools",
            Self::ToolSchemasRoot => "tool schemas",
            Self::PendingRoot => "pending",
            Self::NotSentRoot => "not sent",
            Self::Persona => "persona",
            Self::ProjectEnvironment => "project",
            Self::VolatileState => "volatile",
            Self::ProjectInstructions => "instructions",
            Self::SteeringFile => "steering",
            Self::MemoryIndex => "memory",
            Self::Summary => "summary",
            Self::ChatMessage => "chat",
            Self::Attachment => "attachment",
            Self::Mention => "mention",
            Self::ToolCall => "tool call",
            Self::ToolInput => "tool input",
            Self::ToolResult => "tool result",
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
            Self::OutputText => "output text",
            Self::ReadFreshness => "freshness",
            Self::Diff => "diff",
            Self::Image => "image",
            Self::TruncationFile => "truncation",
            Self::TaskStatus => "task status",
            Self::ToolSchema => "tool schema",
            Self::Background => "background",
            Self::PlanState => "plan",
            Self::TodoState => "todo",
            Self::QueuedInput => "queued",
            Self::ComposerDraft => "draft",
            Self::FramingAdjustment => "framing",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextInclusion {
    Included,
    PendingNextTurn,
    NotSent,
    Adjustment,
}

impl ContextInclusion {
    pub fn label(self) -> &'static str {
        match self {
            Self::Included => "sent",
            Self::PendingNextTurn => "pending",
            Self::NotSent => "not sent",
            Self::Adjustment => "adjustment",
        }
    }

    pub(crate) fn counts_toward_prompt(self) -> bool {
        matches!(
            self,
            Self::Included | Self::PendingNextTurn | Self::Adjustment
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextControlState {
    pub pinned: bool,
    pub drop_next_turn: bool,
    pub stubbed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stub_reason: Option<ContextStubReason>,
}

impl ContextControlState {
    pub fn is_active(self) -> bool {
        self.pinned || self.drop_next_turn || self.stubbed
    }
}

/// One recorded context-compaction event, surfaced in the `/ctx` compaction
/// history and persisted per session. Token figures use the same estimates the
/// compaction loop runs on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CompactionEvent {
    /// 1-based ordinal within the session.
    pub seq: usize,
    /// Wall-clock time the compaction was applied (epoch milliseconds).
    pub occurred_at_ms: i64,
    pub before_tokens: usize,
    pub after_tokens: usize,
    pub messages_omitted: usize,
    pub tool_outputs_stubbed: usize,
    /// Whether the omitted/stubbed originals remain restorable via `/ctx`.
    pub summary_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repack_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repack_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_hash_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_hash_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cacheable_prefix_tokens_before: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cacheable_prefix_tokens_after: Option<usize>,
}

/// Lightweight mirror of one episode ledger row for `ContextReport` (`/ctx`
/// modal and the `/episodes` command). Labels are the persisted db strings so
/// this stays a plain view type with no dependency on the episode module.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EpisodeReport {
    /// 1-based ordinal within the session.
    pub seq: usize,
    pub title: String,
    pub status_label: String,
    pub close_reason_label: Option<String>,
    /// Opening user message, one line.
    pub goal: String,
    /// Message count of the span while it still resolves in live context;
    /// `None` once evicted or when the span no longer resolves.
    pub live_span_messages: Option<usize>,
    pub evicted_tokens: Option<usize>,
    pub recall_count: usize,
    pub archived_messages: usize,
    pub completable: bool,
    pub repaired: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ContextStubReason {
    User,
    SupersededRead,
    OldSuccessfulToolOutput,
}

impl ContextStubReason {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "user stubbed",
            Self::SupersededRead => "superseded read",
            Self::OldSuccessfulToolOutput => "old successful output",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextControlAction {
    TogglePin,
    ToggleDropNextTurn,
    ToggleStub,
    RestoreSummarySource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextUsageReconciliation {
    pub estimated_prompt_tokens: usize,
    pub actual_prompt_tokens: u32,
    pub actual_completion_tokens: u32,
    pub provider_delta_tokens: i64,
    pub estimate_source: TokenCounterKind,
    pub estimate_confidence: EstimateConfidence,
}

/// Report-facing projection of one persisted model-response usage row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageTurnReport {
    pub seq: usize,
    pub lane_kind: crate::agent::ExecutionLaneKind,
    pub lane_id: String,
    pub lane_seq: usize,
    pub parent_tool_call_id: Option<String>,
    pub launch_group_id: Option<String>,
    pub status: UsageTurnStatus,
    pub finish_reason: Option<crate::provider::FinishReason>,
    pub reasoning_chars: usize,
    pub provider_attempts: Vec<crate::agent::ProviderAttemptReport>,
    /// Provider/model this turn ran against; must survive the report round-trip
    /// (the persistence flush maps reports back through [`Self::to_usage_turn`]).
    pub provider_id: Option<String>,
    pub model: Option<String>,
    /// Exact request-local reasoning setting. Legacy turns have no value.
    #[serde(serialize_with = "serialize_optional_reasoning")]
    pub effective_reasoning: Option<crate::provider::ReasoningSelection>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_measured_input_tokens: Option<u64>,
    pub turn_cost_micros: Option<u64>,
    pub no_cache_cost_micros: Option<u64>,
    pub estimated_prompt_tokens: Option<usize>,
    pub estimate_source: Option<TokenCounterKind>,
    pub estimate_confidence: Option<EstimateConfidence>,
    pub tool_schema_tokens: Option<usize>,
    pub tool_schema_hash: Option<String>,
    pub tool_schema_names: Vec<String>,
    pub request_body_bytes: Option<usize>,
    pub request_body_hash: Option<String>,
    pub cache_mechanism: Option<String>,
    pub cache_route_fingerprint: Option<String>,
    pub expected_cacheable_percent: Option<u64>,
    pub actual_cache_read_percent: Option<u64>,
    pub local_reusable_prefix_tokens: Option<usize>,
    pub local_reusable_prefix_percent: Option<u64>,
    pub cacheable_prefix_tokens: Option<usize>,
    pub volatile_tail_tokens: Option<usize>,
    pub context_window_tokens: Option<usize>,
    pub rewrite_kind: ContextRewriteKind,
    pub rewrite_saved_tokens: Option<usize>,
    /// Evicted-episode attribution for a single-episode `Episode` rewrite.
    pub episode_seq: Option<usize>,
    /// Wall-clock time this turn was recorded (real per-request timing).
    pub created_at_ms: i64,
    /// Total provider request latency, including retries.
    pub latency_ms: Option<u64>,
    /// Time to first output token (assistant or reasoning delta).
    pub ttft_ms: Option<u64>,
    /// Fingerprint of the byte-stable system prefix this turn sent; an
    /// adjacent-turn change flags prompt-cache prefix churn.
    pub prefix_hash: Option<String>,
    pub inspection_executed: usize,
    pub inspection_reused: usize,
    pub inspection_rejected: usize,
    pub inspection_returned_chars: usize,
    pub inspection_avoided_chars: usize,
    pub delegated_parent_overlap: usize,
}

fn serialize_optional_reasoning<S>(
    reasoning: &Option<ReasoningSelection>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match reasoning {
        Some(reasoning) => serializer.serialize_some(&reasoning.label()),
        None => serializer.serialize_none(),
    }
}

impl From<&UsageTurn> for UsageTurnReport {
    fn from(turn: &UsageTurn) -> Self {
        Self {
            seq: turn.seq,
            lane_kind: turn.lane_kind,
            lane_id: turn.lane_id.clone(),
            lane_seq: turn.lane_seq,
            parent_tool_call_id: turn.parent_tool_call_id.clone(),
            launch_group_id: turn.launch_group_id.clone(),
            status: turn.status,
            finish_reason: turn.finish_reason.clone(),
            reasoning_chars: turn.reasoning_chars,
            provider_attempts: turn.provider_attempts.clone(),
            provider_id: turn.provider_id.clone(),
            model: turn.model.clone(),
            effective_reasoning: turn.effective_reasoning,
            prompt_tokens: turn.prompt_tokens,
            completion_tokens: turn.completion_tokens,
            cache_read_input_tokens: turn.cache_read_input_tokens,
            cache_creation_input_tokens: turn.cache_creation_input_tokens,
            cache_measured_input_tokens: turn.cache_measured_input_tokens,
            turn_cost_micros: turn.turn_cost_micros,
            no_cache_cost_micros: turn.no_cache_cost_micros,
            estimated_prompt_tokens: turn.estimated_prompt_tokens,
            estimate_source: turn.estimate_source,
            estimate_confidence: turn.estimate_confidence,
            tool_schema_tokens: turn.tool_schema_tokens,
            tool_schema_hash: turn.tool_schema_hash.clone(),
            tool_schema_names: turn.tool_schema_names.clone(),
            request_body_bytes: turn.request_body_bytes,
            request_body_hash: turn.request_body_hash.clone(),
            cache_mechanism: turn.cache_mechanism.clone(),
            cache_route_fingerprint: turn.cache_route_fingerprint.clone(),
            expected_cacheable_percent: turn.expected_cacheable_percent,
            actual_cache_read_percent: turn.actual_cache_read_percent,
            local_reusable_prefix_tokens: turn.local_reusable_prefix_tokens,
            local_reusable_prefix_percent: turn.local_reusable_prefix_percent,
            cacheable_prefix_tokens: turn.cacheable_prefix_tokens,
            volatile_tail_tokens: turn.volatile_tail_tokens,
            context_window_tokens: turn.context_window_tokens,
            rewrite_kind: turn.rewrite_kind,
            rewrite_saved_tokens: turn.rewrite_saved_tokens,
            episode_seq: turn.episode_seq,
            created_at_ms: turn.created_at_ms,
            latency_ms: turn.latency_ms,
            ttft_ms: turn.ttft_ms,
            prefix_hash: turn.prefix_hash.clone(),
            inspection_executed: turn.inspection_executed,
            inspection_reused: turn.inspection_reused,
            inspection_rejected: turn.inspection_rejected,
            inspection_returned_chars: turn.inspection_returned_chars,
            inspection_avoided_chars: turn.inspection_avoided_chars,
            delegated_parent_overlap: turn.delegated_parent_overlap,
        }
    }
}

impl UsageTurnReport {
    pub(crate) fn to_usage_turn(&self) -> UsageTurn {
        UsageTurn {
            seq: self.seq,
            lane_kind: self.lane_kind,
            lane_id: self.lane_id.clone(),
            lane_seq: self.lane_seq,
            parent_tool_call_id: self.parent_tool_call_id.clone(),
            launch_group_id: self.launch_group_id.clone(),
            status: self.status,
            finish_reason: self.finish_reason.clone(),
            reasoning_chars: self.reasoning_chars,
            provider_attempts: self.provider_attempts.clone(),
            // Preserved, not defaulted: the persistence flush maps reports back
            // through here, so a `None` would erase per-turn attribution.
            provider_id: self.provider_id.clone(),
            model: self.model.clone(),
            effective_reasoning: self.effective_reasoning,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            cache_read_input_tokens: self.cache_read_input_tokens,
            cache_creation_input_tokens: self.cache_creation_input_tokens,
            cache_measured_input_tokens: self.cache_measured_input_tokens,
            turn_cost_micros: self.turn_cost_micros,
            no_cache_cost_micros: self.no_cache_cost_micros,
            estimated_prompt_tokens: self.estimated_prompt_tokens,
            estimate_source: self.estimate_source,
            estimate_confidence: self.estimate_confidence,
            tool_schema_tokens: self.tool_schema_tokens,
            tool_schema_hash: self.tool_schema_hash.clone(),
            tool_schema_names: self.tool_schema_names.clone(),
            request_body_bytes: self.request_body_bytes,
            request_body_hash: self.request_body_hash.clone(),
            cache_mechanism: self.cache_mechanism.clone(),
            cache_route_fingerprint: self.cache_route_fingerprint.clone(),
            expected_cacheable_percent: self.expected_cacheable_percent,
            actual_cache_read_percent: self.actual_cache_read_percent,
            local_reusable_prefix_tokens: self.local_reusable_prefix_tokens,
            local_reusable_prefix_percent: self.local_reusable_prefix_percent,
            cacheable_prefix_tokens: self.cacheable_prefix_tokens,
            volatile_tail_tokens: self.volatile_tail_tokens,
            context_window_tokens: self.context_window_tokens,
            rewrite_kind: self.rewrite_kind,
            rewrite_saved_tokens: self.rewrite_saved_tokens,
            episode_seq: self.episode_seq,
            created_at_ms: self.created_at_ms,
            latency_ms: self.latency_ms,
            ttft_ms: self.ttft_ms,
            prefix_hash: self.prefix_hash.clone(),
            inspection_executed: self.inspection_executed,
            inspection_reused: self.inspection_reused,
            inspection_rejected: self.inspection_rejected,
            inspection_returned_chars: self.inspection_returned_chars,
            inspection_avoided_chars: self.inspection_avoided_chars,
            delegated_parent_overlap: self.delegated_parent_overlap,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextNodeId(String);

impl ContextNodeId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn contains(&self, needle: &str) -> bool {
        self.0.contains(needle)
    }

    pub(crate) fn message(index: usize) -> Self {
        Self(format!("msg-{index}"))
    }

    pub(crate) fn tool(call_id: &str) -> Self {
        Self(format!("tool-{call_id}"))
    }

    pub(crate) fn tool_child(call_id: &str, suffix: &str) -> Self {
        Self(format!("tool-{call_id}-{suffix}"))
    }

    pub(crate) fn raw(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub(crate) fn child(&self, suffix: &str) -> Self {
        Self(format!("{}-{suffix}", self.0))
    }
}

impl fmt::Display for ContextNodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ContextNodeId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for ContextNodeId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for ContextNodeId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<ContextNodeId> for String {
    fn from(value: ContextNodeId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextNode {
    pub id: ContextNodeId,
    pub kind: ContextNodeKind,
    pub inclusion: ContextInclusion,
    pub role: Option<ContextRole>,
    pub label: String,
    pub tokens: usize,
    pub chars: usize,
    pub bytes: usize,
    pub source: TokenCounterKind,
    pub confidence: EstimateConfidence,
    pub preview: String,
    pub sources: Vec<ContextSourceRef>,
    pub children: Vec<ContextNode>,
}

impl ContextNode {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn leaf(
        id: impl Into<ContextNodeId>,
        kind: ContextNodeKind,
        inclusion: ContextInclusion,
        role: Option<ContextRole>,
        label: impl Into<String>,
        tokens: usize,
        text: &str,
        token_metadata: ContextTokenMetadata,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            inclusion,
            role,
            label: label.into(),
            tokens,
            chars: text.chars().count(),
            bytes: text.len(),
            source: token_metadata.source,
            confidence: token_metadata.confidence,
            preview: context_preview(text),
            sources: Vec::new(),
            children: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn parent(
        id: impl Into<ContextNodeId>,
        kind: ContextNodeKind,
        inclusion: ContextInclusion,
        role: Option<ContextRole>,
        label: impl Into<String>,
        tokens: usize,
        text: &str,
        token_metadata: ContextTokenMetadata,
        children: Vec<ContextNode>,
    ) -> Self {
        Self {
            children,
            ..Self::leaf(
                id,
                kind,
                inclusion,
                role,
                label,
                tokens,
                text,
                token_metadata,
            )
        }
    }

    pub(crate) fn with_source(mut self, source: ContextSourceRef) -> Self {
        self.sources.push(source);
        self
    }

    pub(crate) fn with_sources(
        mut self,
        sources: impl IntoIterator<Item = ContextSourceRef>,
    ) -> Self {
        self.sources.extend(sources);
        self
    }

    pub fn counted_tokens(&self) -> usize {
        if self.inclusion.counts_toward_prompt() {
            self.tokens
        } else {
            0
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextTokenMetadata {
    pub(crate) source: TokenCounterKind,
    pub(crate) confidence: EstimateConfidence,
}

impl ContextTokenMetadata {
    pub(crate) fn from_estimate(estimate: &PromptEstimate) -> Self {
        Self {
            source: estimate.source,
            confidence: estimate.confidence,
        }
    }
}

/// Group of values that repeat as trailing parameters in every `*_children`
/// function. Constructed once at the dispatch site and passed by reference
/// so builders can absorb `inclusion`, `row_metadata`, and the default source.
#[derive(Debug, Clone)]
pub(crate) struct NodeBuildContext<'a> {
    pub(crate) row_metadata: ContextTokenMetadata,
    pub(crate) estimator: &'a PromptEstimator,
    pub(crate) inclusion: ContextInclusion,
    pub(crate) source: ContextSourceRef,
}

impl<'a> NodeBuildContext<'a> {
    /// Build a leaf node, absorbing `self.inclusion`, `self.row_metadata`,
    /// and `self.source`.
    pub(crate) fn leaf(
        &self,
        id: impl Into<ContextNodeId>,
        kind: ContextNodeKind,
        role: Option<ContextRole>,
        label: impl Into<String>,
        tokens: usize,
        text: &str,
    ) -> ContextNode {
        ContextNode::leaf(
            id,
            kind,
            self.inclusion,
            role,
            label,
            tokens,
            text,
            self.row_metadata,
        )
        .with_source(self.source.clone())
    }

    /// Build a parent node, absorbing `self.inclusion`, `self.row_metadata`,
    /// and `self.source`.
    #[allow(dead_code)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn parent(
        &self,
        id: impl Into<ContextNodeId>,
        kind: ContextNodeKind,
        role: Option<ContextRole>,
        label: impl Into<String>,
        tokens: usize,
        text: &str,
        children: Vec<ContextNode>,
    ) -> ContextNode {
        ContextNode::parent(
            id,
            kind,
            self.inclusion,
            role,
            label,
            tokens,
            text,
            self.row_metadata,
            children,
        )
        .with_source(self.source.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPreviewUserInput {
    pub id: Option<u64>,
    pub text: String,
    pub mode: AgentMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextPreviewInput {
    pub composer_draft: Option<String>,
    pub queued_inputs: Vec<ContextPreviewUserInput>,
    pub plan_markdown: Option<String>,
    pub todo_markdown: Option<String>,
    pub target_mode: Option<AgentMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingContextMessage {
    pub(crate) index: usize,
    pub(crate) label: String,
    pub(crate) kind: ContextNodeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolContextDetail {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
    pub(crate) read_evidence: Option<ReadEvidence>,
    pub(crate) result: ToolContextResult,
    /// The `call_id` this detail's result points at, when `result` is a read-reuse
    /// pointer rather than real content. Structural so pointer detection
    /// and target lookup never re-parse the human-readable pointer sentence in
    /// `result`, which describes the same target but is free to reword.
    pub(crate) reuse_target_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolContextResult {
    Text {
        rendered: String,
    },
    Command {
        rendered: String,
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
        timed_out: bool,
        truncation: Option<OutputTruncationContext>,
    },
    BackgroundTaskStarted {
        task_id: String,
        message: String,
    },
    SubagentStarted {
        subtask_id: String,
        message: String,
    },
    Edit {
        summary: String,
        diff_preview: String,
    },
    Image {
        description: String,
        image: ToolImageContext,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolImageContext {
    pub(crate) mime_type: String,
    pub(crate) base64_bytes: usize,
}
