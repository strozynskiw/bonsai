use std::collections::{HashMap, HashSet};

use async_openai::types::chat::{
    ChatCompletionRequestMessage, ChatCompletionRequestUserMessageArgs, ChatCompletionTool,
};

use crate::context::ProjectContextSnapshot;
use crate::provider::{
    EstimateConfidence, InputCacheUsage, PromptEstimate, PromptEstimator, ProviderRequestPreview,
    TokenCounterKind,
};
use crate::tool::ReadEvidence;

pub(crate) mod cache_diagnosis;
mod control_id;
mod ledger;
pub(crate) mod telemetry;
mod tool_args;
mod types;

pub(crate) use control_id::*;
pub(crate) use ledger::*;
pub(crate) use tool_args::*;
pub use types::{
    CompactionEvent, ContextControlAction, ContextControlState, ContextEntry, ContextInclusion,
    ContextNode, ContextNodeId, ContextNodeKind, ContextPreviewInput, ContextPreviewUserInput,
    ContextRole, ContextSourceKind, ContextSourceRef, ContextStubReason,
    ContextUsageReconciliation, EpisodeReport, UsageTurnReport,
};
pub(crate) use types::{
    ContextTokenMetadata, PendingContextMessage, ToolContextDetail, ToolContextResult,
    ToolImageContext,
};

/// A snapshot of everything that fills the context window — the actual entries
/// the model sees (post-compaction), each with content readable in `/ctx`.
/// Token figures are estimates (the same char/4 heuristic the agent loop uses
/// for compaction) — exact provider-reported counts are not surfaced here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContextReport {
    pub budget_tokens: usize,
    pub entries: Vec<ContextEntry>,
    pub ledger: Vec<ContextNode>,
    pub estimate_source: TokenCounterKind,
    pub estimate_confidence: EstimateConfidence,
    pub prompt_estimate_tokens: usize,
    pub tool_schema_tokens: usize,
    pub cacheable_prefix_tokens: usize,
    pub volatile_tail_tokens: usize,
    /// Real prompt/completion tokens from the last turn, if the provider
    /// reported them (otherwise the figures above are char/4 estimates).
    pub last_prompt_tokens: Option<u32>,
    pub last_completion_tokens: Option<u32>,
    pub last_input_cache: Option<InputCacheUsage>,
    pub last_turn_cost_micros: Option<u64>,
    /// Prompt-cache savings (full-rate cost minus cache-aware cost) for the last
    /// turn and the session, when priced.
    pub last_turn_savings_micros: Option<u64>,
    /// Cumulative real token usage across the session.
    pub session_prompt_tokens: u64,
    pub session_completion_tokens: u64,
    pub session_input_cache: Option<InputCacheUsage>,
    pub session_cost_micros: Option<u64>,
    pub session_savings_micros: Option<u64>,
    pub controls: HashMap<String, ContextControlState>,
    pub summary_sources: HashSet<String>,
    /// Per-session compaction history, oldest first, for the `/ctx` view.
    pub compaction_events: Vec<CompactionEvent>,
    /// Episode ledger rows, oldest first (dark behind `BONSAI_EPISODES`;
    /// empty when the feature is unwired).
    pub episodes: Vec<EpisodeReport>,
    /// Per-provider-response usage diagnostics, oldest first, for `/cost`.
    pub usage_turns: Vec<UsageTurnReport>,
    pub payload_preview: Option<ProviderRequestPreview>,
    pub reconciliation: Option<ContextUsageReconciliation>,
}

impl ContextReport {
    pub fn used_tokens(&self) -> usize {
        if self.prompt_estimate_tokens > 0 {
            self.prompt_estimate_tokens
        } else {
            self.entries.iter().map(|e| e.tokens).sum()
        }
    }

    pub fn tokens_for(&self, role: ContextRole) -> usize {
        self.entries
            .iter()
            .filter(|e| e.role == role)
            .map(|e| e.tokens)
            .sum()
    }

    pub fn message_count(&self) -> usize {
        self.entries.len()
    }

    pub fn last_input_tokens(&self) -> Option<u64> {
        self.last_prompt_tokens.map(|prompt_tokens| {
            self.last_input_cache
                .map(|cache| cache.total_input_tokens)
                .unwrap_or(u64::from(prompt_tokens))
                .max(u64::from(prompt_tokens))
        })
    }

    pub fn session_input_tokens(&self) -> u64 {
        self.session_input_cache
            .map(|cache| cache.total_input_tokens)
            .unwrap_or(self.session_prompt_tokens)
            .max(self.session_prompt_tokens)
    }

    /// Distinct models that ran a turn with reported usage but no price — the
    /// reason a session's cost is only partial. First-seen order. A model whose
    /// catalog entry prices it at `$0` (free tier) is *priced*, not unpriced, so
    /// it never appears here. Drives the "partial cost" marker so a frozen dollar
    /// figure can never masquerade as a stopped counter.
    pub fn unpriced_models(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for turn in &self.usage_turns {
            let reported = turn.prompt_tokens.is_some() || turn.completion_tokens.is_some();
            if reported
                && turn.turn_cost_micros.is_none()
                && let Some(model) = &turn.model
                && !seen.iter().any(|known| known == model)
            {
                seen.push(model.clone());
            }
        }
        seen
    }

    /// The session has some known cost but at least one turn ran unpriced, so
    /// the total is a lower bound, not exact.
    pub fn session_cost_is_partial(&self) -> bool {
        self.session_cost_micros.is_some() && !self.unpriced_models().is_empty()
    }

    pub fn default_expanded_node_ids(&self) -> HashSet<String> {
        self.ledger
            .iter()
            .filter(|node| !node.children.is_empty())
            .map(|node| node.id.as_str().to_string())
            .collect()
    }

    pub fn visible_node_ids(&self, expanded: &HashSet<String>) -> Vec<String> {
        let mut ids = Vec::new();
        for node in &self.ledger {
            collect_visible_node_ids(node, expanded, &mut ids);
        }
        ids
    }

    pub fn control_for(&self, node_id: &str) -> ContextControlState {
        self.controls
            .get(&canonical_context_control_id(node_id))
            .copied()
            .unwrap_or_default()
    }

    pub fn summary_source_available(&self, node_id: &str) -> bool {
        self.summary_sources
            .contains(&canonical_context_control_id(node_id))
    }
}

fn collect_visible_node_ids(node: &ContextNode, expanded: &HashSet<String>, ids: &mut Vec<String>) {
    ids.push(node.id.as_str().to_string());
    if expanded.contains(node.id.as_str()) {
        for child in &node.children {
            collect_visible_node_ids(child, expanded, ids);
        }
    }
}

/// Per-entry display cap so a large tool result can't make the `/ctx` modal
/// unscrollably long; the header still shows the real token estimate.
const CONTEXT_ENTRY_PREVIEW_CHARS: usize = 4_000;

pub(crate) fn user_request_message(text: &str) -> Option<ChatCompletionRequestMessage> {
    ChatCompletionRequestUserMessageArgs::default()
        .content(text)
        .build()
        .ok()
        .map(ChatCompletionRequestMessage::User)
}

pub(crate) fn background_context_label(message: &str) -> String {
    if is_background_status_text(message) {
        "Background task status".to_string()
    } else if is_background_context_text(message) {
        "Background task output".to_string()
    } else {
        "Background context".to_string()
    }
}

pub(crate) fn is_background_context_text(message: &str) -> bool {
    const CANONICAL_SOURCES: [&str; 6] = [
        "source=\"background command completion\"",
        "source=\"background subagent completion\"",
        "source=\"interactive terminal update\"",
        "source=\"running background command status\"",
        "source=\"running interactive terminal status\"",
        "source=\"running background subagent status\"",
    ];

    CANONICAL_SOURCES
        .iter()
        .any(|source| message.contains(source))
        // Retain support for persisted sessions created before the canonical
        // untrusted runtime frame was introduced.
        || message.contains("[Untrusted background command output]")
        || message.contains("[Untrusted background task status]")
}

fn is_background_status_text(message: &str) -> bool {
    message.contains("source=\"running background command status\"")
        || message.contains("source=\"running interactive terminal status\"")
        || message.contains("source=\"running background subagent status\"")
        || message.contains("[Untrusted background task status]")
}

#[cfg(test)]
mod background_context_tests {
    use super::{background_context_label, is_background_context_text};

    #[test]
    fn canonical_runtime_sources_remain_background_context() {
        let output = "<<<untrusted-content source=\"background subagent completion\">>>\nresult";
        let status =
            "<<<untrusted-content source=\"running background command status\">>>\nrunning";

        assert!(is_background_context_text(output));
        assert_eq!(background_context_label(output), "Background task output");
        assert!(is_background_context_text(status));
        assert_eq!(background_context_label(status), "Background task status");
        assert!(!is_background_context_text("ordinary human request"));
    }
}

pub(crate) fn context_preview(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() > CONTEXT_ENTRY_PREVIEW_CHARS {
        let cut = text
            .chars()
            .take(CONTEXT_ENTRY_PREVIEW_CHARS)
            .collect::<String>();
        let remaining = text.chars().count() - CONTEXT_ENTRY_PREVIEW_CHARS;
        format!("{cut}\n…(+{remaining} more chars)")
    } else {
        text.to_string()
    }
}

fn context_message_source(
    id: &str,
    role: ContextRole,
    pending: Option<&PendingContextMessage>,
) -> ContextSourceRef {
    if let Some(pending) = pending {
        let kind = match pending.kind {
            ContextNodeKind::ComposerDraft => ContextSourceKind::ComposerDraft,
            ContextNodeKind::QueuedInput => ContextSourceKind::QueuedInput,
            ContextNodeKind::Background => ContextSourceKind::BackgroundTask,
            _ => ContextSourceKind::ContextMessage,
        };
        return ContextSourceRef::new(kind, id.to_string(), pending.label.clone())
            .with_detail("pending");
    }
    ContextSourceRef::new(
        ContextSourceKind::ContextMessage,
        id.to_string(),
        format!("context message {id}"),
    )
    .with_detail(role.label())
}

fn compaction_source(id: &str, message_count: usize) -> ContextSourceRef {
    let noun = if message_count == 1 {
        "message"
    } else {
        "messages"
    };
    ContextSourceRef::new(
        ContextSourceKind::CompactionSource,
        id,
        "compaction originals",
    )
    .with_detail(format!("{message_count} original {noun}"))
    .restorable()
}

fn dedupe_sources(sources: impl IntoIterator<Item = ContextSourceRef>) -> Vec<ContextSourceRef> {
    let mut deduped = Vec::new();
    let mut seen = HashSet::new();
    for source in sources {
        if seen.insert(source.clone()) {
            deduped.push(source);
        }
    }
    deduped
}

fn child_sources(children: &[ContextNode]) -> Vec<ContextSourceRef> {
    dedupe_sources(
        children
            .iter()
            .flat_map(|child| child.sources.iter().cloned()),
    )
}

fn annotate_summary_sources(nodes: &mut [ContextNode], counts: &HashMap<String, usize>) {
    for node in nodes {
        annotate_summary_sources_for_node(node, counts);
    }
}

fn annotate_summary_sources_for_node(
    node: &mut ContextNode,
    counts: &HashMap<String, usize>,
) -> Vec<ContextSourceRef> {
    let mut sources = node.sources.clone();
    for child in &mut node.children {
        sources.extend(annotate_summary_sources_for_node(child, counts));
    }
    let id = canonical_context_control_id(node.id.as_str());
    if let Some(message_count) = counts.get(&id).copied() {
        sources.push(compaction_source(&id, message_count));
    }
    node.sources = dedupe_sources(sources);
    node.sources.clone()
}

pub(crate) fn system_project_context_text<'a>(
    system_text: &'a str,
    persona: &str,
) -> Option<&'a str> {
    let suffix = system_text.strip_prefix(persona)?;
    let project_text = suffix.strip_prefix("\n\n# Project context\n\n")?;
    (!project_text.trim().is_empty()).then_some(project_text)
}

pub(crate) fn add_framing_adjustment(
    ledger: &mut Vec<ContextNode>,
    prompt_estimate_tokens: usize,
    estimate: &PromptEstimate,
) {
    let counted = ledger
        .iter()
        .map(ContextNode::counted_tokens)
        .sum::<usize>();
    if prompt_estimate_tokens <= counted {
        return;
    }
    let delta = prompt_estimate_tokens - counted;
    ledger.push(ContextNode::leaf(
        ContextNodeId::raw("framing-adjustment"),
        ContextNodeKind::FramingAdjustment,
        ContextInclusion::Adjustment,
        None,
        "Provider framing/tokenizer delta",
        delta,
        "Difference between per-row report counts and the provider preflight estimate.",
        ContextTokenMetadata::from_estimate(estimate),
    ));
}

pub(crate) struct ContextLedgerBuildInput<'a> {
    pub(crate) messages: &'a [ChatCompletionRequestMessage],
    pub(crate) message_ids: &'a [String],
    pub(crate) message_tokens: &'a [usize],
    pub(crate) tools: &'a [ChatCompletionTool],
    pub(crate) estimate: &'a PromptEstimate,
    pub(crate) row_metadata: ContextTokenMetadata,
    pub(crate) base_message_count: usize,
    pub(crate) pending_by_index: &'a HashMap<usize, &'a PendingContextMessage>,
    pub(crate) preview: &'a ContextPreviewInput,
    pub(crate) persona: &'a str,
    pub(crate) project_context: Option<&'a ProjectContextSnapshot>,
    pub(crate) prompt_estimator: &'a PromptEstimator,
    pub(crate) tool_context_details: &'a HashMap<String, ToolContextDetail>,
    pub(crate) mention_read_evidence: &'a HashMap<String, Vec<ReadEvidence>>,
    pub(crate) summary_source_counts: &'a HashMap<String, usize>,
    pub(crate) message_inclusions: &'a HashMap<usize, ContextInclusion>,
}

pub(crate) fn context_ledger_for_messages(input: ContextLedgerBuildInput<'_>) -> Vec<ContextNode> {
    let mut system_nodes = Vec::new();
    let mut chat_nodes = Vec::new();
    let mut pending_nodes = Vec::new();
    let mut tool_calls = ToolLedgerCalls::default();
    let mut counters = MessageCounters::default();

    for (index, message) in input.messages.iter().enumerate() {
        let tokens = input.message_tokens.get(index).copied().unwrap_or(0);
        let (role, text) = describe_message_full(message);
        let pending = input.pending_by_index.get(&index).copied();
        let id = input
            .message_ids
            .get(index)
            .cloned()
            .unwrap_or_else(|| ContextNodeId::message(index).into_string());
        let message_source = context_message_source(&id, role, pending);
        let inclusion = if pending.is_some() || index >= input.base_message_count {
            ContextInclusion::PendingNextTurn
        } else {
            input
                .message_inclusions
                .get(&index)
                .copied()
                .unwrap_or(ContextInclusion::Included)
        };

        if inclusion == ContextInclusion::Included
            && let Some(calls) = assistant_tool_calls(message)
        {
            for call in calls {
                tool_calls.record_input(
                    call,
                    input.row_metadata,
                    input.prompt_estimator,
                    message_source.clone(),
                );
            }
        }

        if inclusion == ContextInclusion::Included && role == ContextRole::Tool {
            tool_calls.record_output(
                message,
                tokens,
                input.row_metadata,
                input.prompt_estimator,
                input.tool_context_details,
                message_source,
            );
            continue;
        }

        let (kind, label) = if let Some(pending) = pending {
            (pending.kind, pending.label.clone())
        } else {
            classify_message(
                message,
                role,
                index,
                &text,
                tool_calls.output_count() + 1,
                &mut counters,
            )
        };
        let id = ContextNodeId::raw(id);
        let children = message_children(
            &input,
            &id,
            message,
            role,
            kind,
            index,
            &text,
            inclusion,
            &message_source,
        );

        if role == ContextRole::Assistant
            && children.is_empty()
            && assistant_tool_calls(message).is_some()
            && inclusion == ContextInclusion::Included
        {
            continue;
        }

        let node = message_node_with_framing(
            ContextNode::parent(
                id,
                kind,
                inclusion,
                Some(role),
                label.clone(),
                tokens,
                &text,
                input.row_metadata,
                children,
            )
            .with_source(message_source),
        );
        match inclusion {
            ContextInclusion::PendingNextTurn => pending_nodes.push(node),
            ContextInclusion::Included
            | ContextInclusion::Adjustment
            | ContextInclusion::NotSent => match role {
                ContextRole::System => system_nodes.push(node),
                ContextRole::User | ContextRole::Assistant => chat_nodes.push(node),
                ContextRole::Tool => tool_calls.push_orphan_result(node),
                ContextRole::ToolSchema => {}
            },
        }
    }

    let schema_nodes = if input.tools.is_empty() {
        Vec::new()
    } else {
        vec![tool_schema_node(
            input.tools,
            input.estimate,
            input.row_metadata,
            input.prompt_estimator,
        )]
    };
    // Every root aggregates the same way; the table keeps them in display
    // order and an empty group simply contributes no root.
    let roots = [
        (
            "root-system",
            ContextNodeKind::SystemRoot,
            ContextInclusion::Included,
            Some(ContextRole::System),
            "System",
            system_nodes,
        ),
        (
            "root-chat",
            ContextNodeKind::ChatRoot,
            ContextInclusion::Included,
            None,
            "Chat",
            chat_nodes,
        ),
        (
            "root-tools",
            ContextNodeKind::ToolsRoot,
            ContextInclusion::Included,
            Some(ContextRole::Tool),
            "Tools",
            tool_calls.into_nodes(),
        ),
        (
            "root-tool-schemas",
            ContextNodeKind::ToolSchemasRoot,
            ContextInclusion::Included,
            Some(ContextRole::ToolSchema),
            "Tool schemas",
            schema_nodes,
        ),
        (
            "root-pending",
            ContextNodeKind::PendingRoot,
            ContextInclusion::PendingNextTurn,
            None,
            "Pending next turn",
            pending_nodes,
        ),
        (
            "root-not-sent",
            ContextNodeKind::NotSentRoot,
            ContextInclusion::NotSent,
            None,
            "Not sent",
            not_sent_state_nodes(&input),
        ),
    ];
    let mut nodes = Vec::new();
    for (id, kind, inclusion, role, label, children) in roots {
        if children.is_empty() {
            continue;
        }
        nodes.push(aggregate_context_node(
            ContextNodeId::raw(id),
            kind,
            inclusion,
            role,
            label,
            input.row_metadata,
            children,
        ));
    }
    annotate_summary_sources(&mut nodes, input.summary_source_counts);

    nodes
}

/// Running per-role counters that number the chat rows ("User message 3").
#[derive(Default)]
struct MessageCounters {
    user: usize,
    assistant: usize,
    summary: usize,
}

/// The ledger row kind + label for one non-pending message.
fn classify_message(
    message: &ChatCompletionRequestMessage,
    role: ContextRole,
    index: usize,
    text: &str,
    next_tool_result_index: usize,
    counters: &mut MessageCounters,
) -> (ContextNodeKind, String) {
    match role {
        ContextRole::System if index == 0 => {
            (ContextNodeKind::Persona, "System prompt".to_string())
        }
        ContextRole::System => {
            counters.summary = counters.summary.saturating_add(1);
            (
                ContextNodeKind::Summary,
                format!("System summary {}", counters.summary),
            )
        }
        ContextRole::User if is_project_state_message(message) => (
            ContextNodeKind::VolatileState,
            "Project state update".to_string(),
        ),
        ContextRole::User if is_background_context_text(text) => {
            (ContextNodeKind::Background, background_context_label(text))
        }
        ContextRole::User => {
            counters.user = counters.user.saturating_add(1);
            (
                ContextNodeKind::ChatMessage,
                format!("User message {}", counters.user),
            )
        }
        ContextRole::Assistant => {
            counters.assistant = counters.assistant.saturating_add(1);
            (
                ContextNodeKind::ChatMessage,
                format!("Assistant message {}", counters.assistant),
            )
        }
        ContextRole::Tool => (
            ContextNodeKind::ToolResult,
            tool_result_label(message, next_tool_result_index),
        ),
        ContextRole::ToolSchema => (
            ContextNodeKind::ToolSchema,
            format!("Tool schema {}", index + 1),
        ),
    }
}

fn is_project_state_message(message: &ChatCompletionRequestMessage) -> bool {
    matches!(
        message,
        ChatCompletionRequestMessage::User(user)
            if user.name.as_deref()
                == crate::agent::MessageProvenance::ProjectState.wire_name()
    )
}

/// Child rows for one ledger message, dispatched by role/kind: the system
/// prompt splits into persona + project context, user messages into text and
/// attachments, assistant messages into text and reasoning.
#[expect(
    clippy::too_many_arguments,
    reason = "pure dispatch over loop-local state"
)]
fn message_children(
    input: &ContextLedgerBuildInput<'_>,
    id: &ContextNodeId,
    message: &ChatCompletionRequestMessage,
    role: ContextRole,
    kind: ContextNodeKind,
    index: usize,
    text: &str,
    inclusion: ContextInclusion,
    message_source: &ContextSourceRef,
) -> Vec<ContextNode> {
    match (role, kind) {
        (ContextRole::System, ContextNodeKind::Persona) if index == 0 => system_context_children(
            id.as_str(),
            text,
            input.persona,
            input.project_context,
            input.prompt_estimator,
            input.row_metadata,
            message_source.clone(),
        ),
        (ContextRole::User, ContextNodeKind::Background) => background_message_children(
            id.as_str(),
            text,
            input.row_metadata,
            input.prompt_estimator,
            inclusion,
            message_source.clone(),
        ),
        (ContextRole::User, _) => user_message_children(
            id.as_str(),
            message,
            text,
            input
                .mention_read_evidence
                .get(id.as_str())
                .map(Vec::as_slice),
            input.row_metadata,
            input.prompt_estimator,
            inclusion,
            message_source.clone(),
        ),
        (ContextRole::Assistant, _) => assistant_message_children(
            id.as_str(),
            message,
            input.row_metadata,
            input.prompt_estimator,
            inclusion,
            message_source.clone(),
        ),
        _ => Vec::new(),
    }
}

/// The "Not sent" leaves: session state that exists but is not part of the
/// prompt (plan canvas, todo list, memory index). One table row per state
/// kind; blank state contributes no leaf.
fn not_sent_state_nodes(input: &ContextLedgerBuildInput<'_>) -> Vec<ContextNode> {
    let candidates = [
        (
            "not-sent-plan",
            ContextNodeKind::PlanState,
            "Plan canvas",
            input.preview.plan_markdown.as_deref(),
            ContextSourceRef::new(ContextSourceKind::PlanState, "plan-canvas", "plan canvas"),
        ),
        (
            "not-sent-todo",
            ContextNodeKind::TodoState,
            "Todo list",
            input.preview.todo_markdown.as_deref(),
            ContextSourceRef::new(ContextSourceKind::TodoState, "todo-state", "todo state"),
        ),
        (
            "not-sent-memory-index",
            ContextNodeKind::MemoryIndex,
            "Memory index",
            input
                .project_context
                .map(|context| context.memory_index.as_str()),
            ContextSourceRef::new(
                ContextSourceKind::ProjectContext,
                "memory-index",
                "persistent memory index",
            ),
        ),
    ];
    candidates
        .into_iter()
        .filter_map(|(id, kind, label, text, source)| {
            let text = text.map(str::trim).filter(|text| !text.is_empty())?;
            Some(not_sent_node(
                ContextNodeId::raw(id),
                kind,
                label,
                text,
                input.row_metadata,
                input.prompt_estimator,
                source,
            ))
        })
        .collect()
}

pub(crate) fn prompt_cache_token_totals(
    messages: &[ChatCompletionRequestMessage],
    message_tokens: &[usize],
    base_message_count: usize,
    persona: &str,
    project_context: Option<&ProjectContextSnapshot>,
    estimator: &PromptEstimator,
    tool_schema_tokens: usize,
) -> (usize, usize) {
    let (system_cache_tokens, volatile_tail_tokens) =
        system_cache_token_totals(messages, persona, project_context, estimator);
    let included_messages = base_message_count
        .min(messages.len())
        .min(message_tokens.len());
    // Skip the first message only when it is the system message that
    // system_cache_token_totals already counted; otherwise (a conversation with
    // no system message) an unconditional skip(1) would drop message[0]'s tokens
    // from both the system and history buckets.
    let system_counted = matches!(
        messages.first(),
        Some(ChatCompletionRequestMessage::System(_))
    );
    let history_tokens = message_tokens
        .iter()
        .take(included_messages)
        .skip(usize::from(system_counted))
        .copied()
        .sum::<usize>();
    (
        system_cache_tokens
            .saturating_add(history_tokens)
            .saturating_add(tool_schema_tokens),
        volatile_tail_tokens,
    )
}

fn system_cache_token_totals(
    messages: &[ChatCompletionRequestMessage],
    persona: &str,
    project_context: Option<&ProjectContextSnapshot>,
    estimator: &PromptEstimator,
) -> (usize, usize) {
    let Some(message) = messages.first() else {
        return (0, 0);
    };
    let (role, system_text) = describe_message_full(message);
    if role != ContextRole::System {
        return (0, 0);
    }

    if !system_text.starts_with(persona) {
        return (estimator.estimate_text_for_report(&system_text), 0);
    }

    let Some(project_text) = system_project_context_text(&system_text, persona) else {
        return (estimator.estimate_text_for_report(persona), 0);
    };
    let Some(context) = project_context else {
        return (estimator.estimate_text_for_report(&system_text), 0);
    };
    let stable_context = context.cacheable_prefix() == project_text
        || context.cacheable_prefix_smol() == project_text;
    if stable_context {
        // IMPORTANT CACHE INVARIANT: volatile state is now an append-only named
        // user message. Count the complete stable system message as reusable and
        // do not teach diagnostics that the old mutable system tail still exists.
        return (estimator.estimate_text_for_report(&system_text), 0);
    }
    if context.render() != project_text {
        return (estimator.estimate_text_for_report(&system_text), 0);
    }

    let cacheable_project = context.cacheable_prefix();
    let volatile_tail = context.volatile_tail();
    let mut cacheable_prefix = persona.to_string();
    if !cacheable_project.trim().is_empty() || !volatile_tail.trim().is_empty() {
        cacheable_prefix.push_str("\n\n# Project context\n\n");
        cacheable_prefix.push_str(&cacheable_project);
    }
    if !volatile_tail.trim().is_empty() {
        cacheable_prefix.push_str("\n\n");
    }

    let volatile_tail_tokens = if volatile_tail.trim().is_empty() {
        0
    } else {
        estimator.estimate_text_for_report(&volatile_tail)
    };

    (
        estimator.estimate_text_for_report(&cacheable_prefix),
        volatile_tail_tokens,
    )
}

fn system_context_children(
    id: &str,
    system_text: &str,
    persona: &str,
    project_context: Option<&ProjectContextSnapshot>,
    estimator: &PromptEstimator,
    row_metadata: ContextTokenMetadata,
    message_source: ContextSourceRef,
) -> Vec<ContextNode> {
    if !system_text.starts_with(persona) {
        return Vec::new();
    }

    let mut children = vec![
        ContextNode::leaf(
            format!("{id}-persona"),
            ContextNodeKind::Persona,
            ContextInclusion::Included,
            Some(ContextRole::System),
            "Persona",
            estimator.estimate_text_for_report(persona),
            persona,
            row_metadata,
        )
        .with_sources([
            message_source.clone(),
            ContextSourceRef::new(
                ContextSourceKind::SystemPrompt,
                "persona",
                "runtime persona",
            ),
        ]),
    ];
    let Some(project_text) = system_project_context_text(system_text, persona) else {
        return children;
    };

    // The generic "Project context" leaf is emitted verbatim from two mutually
    // exclusive branches below (no snapshot, and SMOL/legacy layouts). Build it
    // once here so the two paths cannot drift apart.
    let project_context_leaf = || {
        ContextNode::leaf(
            format!("{id}-project"),
            ContextNodeKind::ProjectEnvironment,
            ContextInclusion::Included,
            Some(ContextRole::System),
            "Project context",
            estimator.estimate_text_for_report(project_text),
            project_text,
            row_metadata,
        )
        .with_sources([
            message_source.clone(),
            ContextSourceRef::new(
                ContextSourceKind::ProjectContext,
                format!("{id}-project"),
                "project context from system message",
            ),
        ])
    };

    let Some(context) = project_context else {
        children.push(project_context_leaf());
        return children;
    };
    let includes_legacy_volatile_tail = context.render() == project_text;
    // The detailed children below render the normal steering/context layout.
    // Keep SMOL as one exact generic row; expanding it with normal-profile
    // children would claim bytes that were never sent to the model.
    let is_current_stable_context = context.cacheable_prefix() == project_text;
    if !includes_legacy_volatile_tail && !is_current_stable_context {
        children.push(project_context_leaf());
        return children;
    }

    let mut project_children = vec![
        ContextNode::leaf(
            format!("{id}-project-env"),
            ContextNodeKind::ProjectEnvironment,
            ContextInclusion::Included,
            Some(ContextRole::System),
            "Environment",
            estimator.estimate_text_for_report(&context.environment),
            &context.environment,
            row_metadata,
        )
        .with_sources([
            message_source.clone(),
            ContextSourceRef::new(
                ContextSourceKind::ProjectContext,
                "project-environment",
                "project context snapshot",
            )
            .with_detail("environment"),
        ]),
    ];
    if !context.steering_files.is_empty() {
        let instructions =
            "## Project instructions\nFollow these steering files (most specific first):";
        project_children.push(
            ContextNode::leaf(
                format!("{id}-project-instructions"),
                ContextNodeKind::ProjectInstructions,
                ContextInclusion::Included,
                Some(ContextRole::System),
                "Project instructions",
                estimator.estimate_text_for_report(instructions),
                instructions,
                row_metadata,
            )
            .with_sources([
                message_source.clone(),
                ContextSourceRef::new(
                    ContextSourceKind::ProjectContext,
                    "project-instructions",
                    "project instructions index",
                ),
            ]),
        );
    }
    for (index, steering) in context.steering_files.iter().enumerate() {
        let rendered = steering.render();
        let truncation = if steering.truncated {
            " (truncated)"
        } else {
            ""
        };
        project_children.push(
            ContextNode::leaf(
                format!("{id}-steering-{index}"),
                ContextNodeKind::SteeringFile,
                ContextInclusion::Included,
                Some(ContextRole::System),
                format!(
                    "{} ({}){}",
                    steering.name,
                    steering.directory.display(),
                    truncation
                ),
                estimator.estimate_text_for_report(&rendered),
                &rendered,
                row_metadata,
            )
            .with_sources([
                message_source.clone(),
                ContextSourceRef::new(
                    ContextSourceKind::SteeringFile,
                    steering
                        .directory
                        .join(&steering.name)
                        .display()
                        .to_string(),
                    steering.name.clone(),
                )
                .with_detail(steering.directory.display().to_string()),
            ]),
        );
    }
    let volatile = context.volatile_tail();
    if includes_legacy_volatile_tail && !volatile.is_empty() {
        project_children.push(
            ContextNode::leaf(
                format!("{id}-project-volatile"),
                ContextNodeKind::VolatileState,
                ContextInclusion::Included,
                Some(ContextRole::System),
                "Volatile state",
                estimator.estimate_text_for_report(&volatile),
                &volatile,
                row_metadata,
            )
            .with_sources([
                message_source.clone(),
                ContextSourceRef::new(
                    ContextSourceKind::ProjectContext,
                    "volatile-state",
                    "volatile project state",
                ),
            ]),
        );
    }
    let rendered = project_text;
    let project_sources = child_sources(&project_children);
    children.push(
        ContextNode::parent(
            format!("{id}-project"),
            ContextNodeKind::ProjectEnvironment,
            ContextInclusion::Included,
            Some(ContextRole::System),
            "Project context",
            estimator.estimate_text_for_report(rendered),
            rendered,
            row_metadata,
            project_children,
        )
        .with_sources(project_sources),
    );
    children
}

fn tool_schema_node(
    tools: &[ChatCompletionTool],
    estimate: &PromptEstimate,
    row_metadata: ContextTokenMetadata,
    estimator: &PromptEstimator,
) -> ContextNode {
    let text = serde_json::to_string_pretty(tools).unwrap_or_else(|_err| "[]".to_string());
    let children: Vec<_> = tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let tool_text =
                serde_json::to_string_pretty(tool).unwrap_or_else(|_err| "{}".to_string());
            ContextNode::leaf(
                format!("tool-schema-{index}"),
                ContextNodeKind::ToolSchema,
                ContextInclusion::Included,
                Some(ContextRole::ToolSchema),
                tool_schema_label(tool, index),
                estimator.estimate_text_for_report(&tool_text),
                &tool_text,
                row_metadata,
            )
            .with_source(ContextSourceRef::new(
                ContextSourceKind::ToolSchema,
                format!("tool-schema-{index}"),
                tool_schema_label(tool, index),
            ))
        })
        .collect();
    let sources = child_sources(&children);
    ContextNode::parent(
        "tool-schemas",
        ContextNodeKind::ToolSchema,
        ContextInclusion::Included,
        Some(ContextRole::ToolSchema),
        format!("{} active tool definitions", tools.len()),
        estimate.tool_schema_tokens,
        &text,
        row_metadata,
        children,
    )
    .with_sources(sources)
}

fn not_sent_node(
    id: ContextNodeId,
    kind: ContextNodeKind,
    label: &'static str,
    text: &str,
    row_metadata: ContextTokenMetadata,
    estimator: &PromptEstimator,
    source: ContextSourceRef,
) -> ContextNode {
    ContextNode::leaf(
        id,
        kind,
        ContextInclusion::NotSent,
        None,
        label,
        estimator.estimate_text_for_report(text),
        text,
        row_metadata,
    )
    .with_source(source)
}

pub(crate) fn describe_message_full(
    message: &ChatCompletionRequestMessage,
) -> (ContextRole, String) {
    use serde_json::Value;

    let value = serde_json::to_value(message).unwrap_or(Value::Null);
    let role = role_from_message_value(&value);
    let mut text = message_content_text(&value);
    if let Some(tool_calls) = value.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let name = call
                .pointer("/function/name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let args = call
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("→ {name}({args})"));
        }
    }
    (role, text.trim().to_string())
}

fn role_from_message_value(value: &serde_json::Value) -> ContextRole {
    match value.get("role").and_then(serde_json::Value::as_str) {
        Some("user") => ContextRole::User,
        Some("assistant") => ContextRole::Assistant,
        Some("tool") => ContextRole::Tool,
        _ => ContextRole::System,
    }
}

pub(crate) fn message_content_text(value: &serde_json::Value) -> String {
    use serde_json::Value;

    match value.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

pub(crate) fn tool_result_label(message: &ChatCompletionRequestMessage, fallback: usize) -> String {
    let value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    value
        .get("tool_call_id")
        .and_then(serde_json::Value::as_str)
        .map(|id| format!("Tool result {id}"))
        .unwrap_or_else(|| format!("Tool result {fallback}"))
}

#[expect(
    clippy::too_many_arguments,
    reason = "pure ledger splitter over loop-local state plus mention sidecar"
)]
pub(crate) fn user_message_children(
    id: &str,
    message: &ChatCompletionRequestMessage,
    text: &str,
    mention_read_evidence: Option<&[ReadEvidence]>,
    row_metadata: ContextTokenMetadata,
    estimator: &PromptEstimator,
    inclusion: ContextInclusion,
    source: ContextSourceRef,
) -> Vec<ContextNode> {
    let value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    if let Some(parts) = value.get("content").and_then(serde_json::Value::as_array) {
        return user_content_part_children(
            id,
            parts,
            mention_read_evidence,
            row_metadata,
            estimator,
            inclusion,
            source,
        );
    }

    const MENTION_MARKER: &str = "# @-mention context";
    let Some(marker_start) = text.find(MENTION_MARKER) else {
        return (!text.trim().is_empty())
            .then(|| {
                ContextNode::leaf(
                    format!("{id}-text"),
                    ContextNodeKind::ChatMessage,
                    inclusion,
                    Some(ContextRole::User),
                    "Message text",
                    estimator.estimate_text_for_report(text),
                    text,
                    row_metadata,
                )
                .with_source(source.clone())
            })
            .into_iter()
            .collect();
    };
    let body = text[..marker_start].trim_end();
    let mention_block = text[marker_start + MENTION_MARKER.len()..].trim_start();

    let mut children = Vec::new();
    if !body.trim().is_empty() {
        children.push(
            ContextNode::leaf(
                format!("{id}-text"),
                ContextNodeKind::ChatMessage,
                inclusion,
                Some(ContextRole::User),
                "Message text",
                estimator.estimate_text_for_report(body),
                body,
                row_metadata,
            )
            .with_source(source.clone()),
        );
    }

    let mention_evidence = mention_read_evidence.unwrap_or_default();
    let mut used_evidence = HashSet::new();
    for (index, section) in mention_sections(mention_block).into_iter().enumerate() {
        let label = section
            .lines()
            .next()
            .map(|line| line.trim_start_matches('#').trim().to_string())
            .filter(|line| !line.is_empty())
            .unwrap_or_else(|| format!("@-mention {}", index + 1));
        children.push(
            ContextNode::leaf(
                format!("{id}-mention-{index}"),
                ContextNodeKind::Mention,
                inclusion,
                Some(ContextRole::User),
                label.clone(),
                estimator.estimate_text_for_report(&section),
                &section,
                row_metadata,
            )
            .with_sources([
                source.clone(),
                ContextSourceRef::new(
                    ContextSourceKind::ContextMessage,
                    format!("{id}-mention-{index}"),
                    "expanded @-mention",
                )
                .with_detail(label.clone()),
            ]),
        );
        if let Some(evidence) =
            mention_section_read_evidence(&section, mention_evidence, &mut used_evidence)
        {
            children.push(read_freshness_node(
                &format!("{id}-mention-{index}"),
                Some(ContextRole::User),
                ContextSourceRef::new(
                    ContextSourceKind::ContextMessage,
                    format!("{id}-mention-{index}"),
                    "@-mention freshness",
                )
                .with_detail(evidence.observation().display_path().to_string()),
                evidence,
                row_metadata,
            ));
        }
    }
    children
}

fn mention_section_read_evidence<'a>(
    section: &str,
    evidence: &'a [ReadEvidence],
    used_indices: &mut HashSet<usize>,
) -> Option<&'a ReadEvidence> {
    let display_path = section.lines().find_map(|line| {
        line.strip_prefix("File: ")
            .map(|rest| rest.split_once(" (").map_or(rest, |(path, _rest)| path))
    })?;
    evidence
        .iter()
        .enumerate()
        .find(|(index, candidate)| {
            !used_indices.contains(index) && candidate.observation().display_path() == display_path
        })
        .map(|(index, evidence)| {
            used_indices.insert(index);
            evidence
        })
}

fn user_content_part_children(
    id: &str,
    parts: &[serde_json::Value],
    mention_read_evidence: Option<&[ReadEvidence]>,
    row_metadata: ContextTokenMetadata,
    estimator: &PromptEstimator,
    inclusion: ContextInclusion,
    source: ContextSourceRef,
) -> Vec<ContextNode> {
    let mut children = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        if let Some(text) = part.get("text").and_then(serde_json::Value::as_str) {
            if text.trim().is_empty() {
                continue;
            }
            children.push(
                ContextNode::leaf(
                    format!("{id}-text-part-{index}"),
                    ContextNodeKind::ChatMessage,
                    inclusion,
                    Some(ContextRole::User),
                    if children.is_empty() {
                        "Message text".to_string()
                    } else {
                        format!("Text part {}", index + 1)
                    },
                    estimator.estimate_text_for_report(text),
                    text,
                    row_metadata,
                )
                .with_source(source.clone()),
            );
            continue;
        }

        if let Some(image_url) = part
            .get("image_url")
            .and_then(|value| value.get("url"))
            .and_then(serde_json::Value::as_str)
        {
            let detail = part
                .get("image_url")
                .and_then(|value| value.get("detail"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("auto");
            let text = format!(
                "image_url bytes: {}\ndetail: {}\n{}",
                image_url.len(),
                detail,
                image_url
            );
            children.push(
                ContextNode::leaf(
                    format!("{id}-image-part-{index}"),
                    ContextNodeKind::Attachment,
                    inclusion,
                    Some(ContextRole::User),
                    format!("Image attachment {}", index + 1),
                    estimator.estimate_text_for_report(&text),
                    &text,
                    row_metadata,
                )
                .with_sources([
                    source.clone(),
                    ContextSourceRef::new(
                        ContextSourceKind::ContextMessage,
                        format!("{id}-image-part-{index}"),
                        "user attachment",
                    )
                    .with_detail("image_url"),
                ]),
            );
            continue;
        }

        let text = serde_json::to_string_pretty(part).unwrap_or_else(|_err| "{}".to_string());
        children.push(
            ContextNode::leaf(
                format!("{id}-attachment-{index}"),
                ContextNodeKind::Attachment,
                inclusion,
                Some(ContextRole::User),
                format!("Attachment {}", index + 1),
                estimator.estimate_text_for_report(&text),
                &text,
                row_metadata,
            )
            .with_sources([
                source.clone(),
                ContextSourceRef::new(
                    ContextSourceKind::ContextMessage,
                    format!("{id}-attachment-{index}"),
                    "user attachment",
                ),
            ]),
        );
    }
    if let Some(mention_read_evidence) = mention_read_evidence {
        for (index, evidence) in mention_read_evidence.iter().enumerate() {
            children.push(read_freshness_node(
                &format!("{id}-mention-{index}"),
                Some(ContextRole::User),
                ContextSourceRef::new(
                    ContextSourceKind::ContextMessage,
                    format!("{id}-mention-{index}"),
                    "@-mention freshness",
                )
                .with_detail(evidence.observation().display_path().to_string()),
                evidence,
                row_metadata,
            ));
        }
    }
    children
}

pub(crate) fn background_message_children(
    id: &str,
    text: &str,
    row_metadata: ContextTokenMetadata,
    estimator: &PromptEstimator,
    inclusion: ContextInclusion,
    source: ContextSourceRef,
) -> Vec<ContextNode> {
    let kind = if is_background_status_text(text) {
        ContextNodeKind::TaskStatus
    } else {
        ContextNodeKind::OutputText
    };
    let label = if kind == ContextNodeKind::TaskStatus {
        "Background task status"
    } else {
        "Background task output"
    };
    vec![
        ContextNode::leaf(
            format!("{id}-background"),
            kind,
            inclusion,
            Some(ContextRole::User),
            label,
            estimator.estimate_text_for_report(text),
            text,
            row_metadata,
        )
        .with_sources([
            source,
            ContextSourceRef::new(ContextSourceKind::BackgroundTask, id, label),
        ]),
    ]
}

fn mention_sections(block: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut current = String::new();
    for line in block.lines() {
        if line.starts_with("## ") && !current.trim().is_empty() {
            sections.push(current.trim_end().to_string());
            current.clear();
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        sections.push(current.trim_end().to_string());
    }
    sections
}

pub(crate) fn assistant_message_children(
    id: &str,
    message: &ChatCompletionRequestMessage,
    row_metadata: ContextTokenMetadata,
    estimator: &PromptEstimator,
    inclusion: ContextInclusion,
    source: ContextSourceRef,
) -> Vec<ContextNode> {
    let value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    let content = message_content_text(&value);
    let mut children = Vec::new();
    if !content.trim().is_empty() {
        children.push(
            ContextNode::leaf(
                format!("{id}-content"),
                ContextNodeKind::ChatMessage,
                inclusion,
                Some(ContextRole::Assistant),
                "Assistant content",
                estimator.estimate_text_for_report(&content),
                &content,
                row_metadata,
            )
            .with_source(source),
        );
    }
    children
}

pub(crate) fn tool_schema_label(
    tool: &async_openai::types::chat::ChatCompletionTool,
    fallback: usize,
) -> String {
    let value = serde_json::to_value(tool).unwrap_or(serde_json::Value::Null);
    value
        .pointer("/function/name")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string)
        .unwrap_or_else(|| format!("tool {}", fallback + 1))
}

pub(crate) fn aggregate_context_node(
    id: impl Into<ContextNodeId>,
    kind: ContextNodeKind,
    inclusion: ContextInclusion,
    role: Option<ContextRole>,
    label: impl Into<String>,
    token_metadata: ContextTokenMetadata,
    children: Vec<ContextNode>,
) -> ContextNode {
    let tokens = children.iter().map(|child| child.tokens).sum();
    let chars = children.iter().map(|child| child.chars).sum();
    let bytes = children.iter().map(|child| child.bytes).sum();
    let sources = child_sources(&children);
    ContextNode {
        id: id.into(),
        kind,
        inclusion,
        role,
        label: label.into(),
        tokens,
        chars,
        bytes,
        source: token_metadata.source,
        confidence: token_metadata.confidence,
        preview: String::new(),
        sources,
        children,
    }
}

pub(crate) fn message_node_with_framing(mut node: ContextNode) -> ContextNode {
    if node.children.is_empty() || !node.inclusion.counts_toward_prompt() {
        return node;
    }
    let child_tokens = node
        .children
        .iter()
        .filter(|child| child.inclusion.counts_toward_prompt())
        .map(|child| child.tokens)
        .sum::<usize>();
    if node.tokens <= child_tokens {
        return node;
    }
    let delta = node.tokens - child_tokens;
    node.children.push(
        ContextNode::leaf(
            node.id.child("framing"),
            ContextNodeKind::FramingAdjustment,
            ContextInclusion::Adjustment,
            node.role,
            "Message framing",
            delta,
            "Provider message framing and tokenizer overhead for this row.",
            ContextTokenMetadata {
                source: node.source,
                confidence: node.confidence,
            },
        )
        .with_source(ContextSourceRef::new(
            ContextSourceKind::Framing,
            node.id.child("framing").into_string(),
            "provider framing",
        )),
    );
    node
}

#[derive(Debug)]
pub(crate) struct AssistantToolCallContext {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

pub(crate) fn assistant_tool_calls(
    message: &ChatCompletionRequestMessage,
) -> Option<Vec<AssistantToolCallContext>> {
    let value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    let calls = value
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)?;
    let parsed = calls
        .iter()
        .enumerate()
        .map(|(index, call)| AssistantToolCallContext {
            call_id: call
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| format!("tool-call-{index}")),
            name: call
                .pointer("/function/name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool")
                .to_string(),
            arguments: call
                .pointer("/function/arguments")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
        .collect::<Vec<_>>();
    (!parsed.is_empty()).then_some(parsed)
}

pub(crate) fn tool_message_call_id(message: &ChatCompletionRequestMessage) -> Option<String> {
    serde_json::to_value(message).ok().and_then(|value| {
        value
            .get("tool_call_id")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
    })
}

pub(crate) fn message_control_ids(
    index: usize,
    message: &ChatCompletionRequestMessage,
    message_id: Option<&str>,
) -> Vec<String> {
    let mut ids = vec![
        message_id
            .map(ToString::to_string)
            .unwrap_or_else(|| ContextNodeId::message(index).into_string()),
    ];
    if let Some(calls) = assistant_tool_calls(message) {
        ids.extend(
            calls
                .into_iter()
                .map(|call| ContextNodeId::tool(&call.call_id).into_string()),
        );
    }
    if let Some(call_id) = tool_message_call_id(message) {
        ids.push(ContextNodeId::tool(&call_id).into_string());
    }
    ids
}

/// Builds the `/ctx` leaves under one tool call. Owns the shared id prefix,
/// call id, estimator, and row metadata so each leaf reads as one call. Every
/// leaf id derives from `id` — the call's own node id, which embeds the call
/// id — so no post-hoc id fixup is needed.
struct ToolLeafBuilder<'a> {
    id: &'a str,
    call_id: &'a str,
    row_metadata: ContextTokenMetadata,
    estimator: &'a PromptEstimator,
}

impl ToolLeafBuilder<'_> {
    /// A leaf sourced from the tool result: `{id}-{suffix}`, with
    /// `source_detail` naming which part of the result it shows.
    fn result_leaf(
        &self,
        suffix: &str,
        kind: ContextNodeKind,
        label: impl Into<String>,
        text: &str,
        source_detail: &'static str,
    ) -> ContextNode {
        self.leaf_with_source(
            suffix,
            kind,
            label,
            text,
            ContextSourceRef::new(ContextSourceKind::ToolResult, self.call_id, source_detail),
        )
    }

    fn leaf_with_source(
        &self,
        suffix: &str,
        kind: ContextNodeKind,
        label: impl Into<String>,
        text: &str,
        source: ContextSourceRef,
    ) -> ContextNode {
        ContextNode::leaf(
            format!("{}-{suffix}", self.id),
            kind,
            ContextInclusion::Included,
            Some(ContextRole::Tool),
            label,
            self.estimator.estimate_text_for_report(text),
            text,
            self.row_metadata,
        )
        .with_source(source)
    }

    /// The "Model-visible output" leaf — the text the model actually received
    /// — carrying both the message source and the tool-result source.
    fn model_visible_output(&self, text: &str, message_source: ContextSourceRef) -> ContextNode {
        ContextNode::leaf(
            format!("{}-output", self.id),
            ContextNodeKind::OutputText,
            ContextInclusion::Included,
            Some(ContextRole::Tool),
            "Model-visible output",
            self.estimator.estimate_text_for_report(text),
            text,
            self.row_metadata,
        )
        .with_sources([
            message_source,
            ContextSourceRef::new(ContextSourceKind::ToolResult, self.call_id, "tool result"),
        ])
    }
}

fn tool_result_children(
    id: &str,
    call_id: &str,
    output_text: &str,
    detail: Option<&ToolContextDetail>,
    row_metadata: ContextTokenMetadata,
    estimator: &PromptEstimator,
    source: ContextSourceRef,
) -> Vec<ContextNode> {
    let leaves = ToolLeafBuilder {
        id,
        call_id,
        row_metadata,
        estimator,
    };
    let Some(detail) = detail else {
        return vec![leaves.model_visible_output(output_text, source)];
    };

    match &detail.result {
        ToolContextResult::Text { rendered } => {
            let mut children = vec![leaves.model_visible_output(rendered, source)];
            if let Some(evidence) = &detail.read_evidence {
                children.push(read_freshness_node(
                    id,
                    Some(ContextRole::Tool),
                    ContextSourceRef::new(ContextSourceKind::ToolResult, call_id, "read freshness"),
                    evidence,
                    row_metadata,
                ));
            }
            children
        }
        ToolContextResult::Command {
            rendered,
            stdout,
            stderr,
            exit_code,
            timed_out,
            truncation,
        } => {
            let status_text = command_status_text(*exit_code, *timed_out);
            let mut children = vec![
                leaves.model_visible_output(rendered, source.clone()),
                leaves.result_leaf(
                    "status",
                    ContextNodeKind::TaskStatus,
                    "Command status",
                    &status_text,
                    "tool execution status",
                ),
            ];
            if !stdout.is_empty() {
                children.push(leaves.result_leaf(
                    "stdout",
                    ContextNodeKind::Stdout,
                    "stdout",
                    stdout,
                    "tool stdout",
                ));
            }
            if !stderr.is_empty() {
                children.push(leaves.result_leaf(
                    "stderr",
                    ContextNodeKind::Stderr,
                    "stderr",
                    stderr,
                    "tool stderr",
                ));
            }
            if let Some(truncation) = truncation {
                let text = format!(
                    "Full output saved to: {}\n{} chars total; {} chars included in the model-visible preview.",
                    truncation.path, truncation.total_chars, truncation.preview_chars
                );
                children.push(leaves.result_leaf(
                    "truncation",
                    ContextNodeKind::TruncationFile,
                    "Truncation file",
                    &text,
                    "truncated tool output",
                ));
            }
            children
        }
        ToolContextResult::BackgroundTaskStarted { task_id, message } => vec![
            // Sourced from the background task itself, not the tool result.
            leaves.leaf_with_source(
                "status",
                ContextNodeKind::TaskStatus,
                format!("Background task {task_id}"),
                message,
                ContextSourceRef::new(
                    ContextSourceKind::BackgroundTask,
                    task_id,
                    "background task",
                ),
            ),
            leaves.model_visible_output(message, source),
        ],
        ToolContextResult::SubagentStarted {
            subtask_id,
            message,
        } => vec![
            leaves.leaf_with_source(
                "status",
                ContextNodeKind::TaskStatus,
                format!("Background subagent {subtask_id}"),
                message,
                ContextSourceRef::new(
                    ContextSourceKind::BackgroundTask,
                    subtask_id,
                    "background subagent",
                ),
            ),
            leaves.model_visible_output(message, source),
        ],
        ToolContextResult::Edit {
            summary,
            diff_preview,
        } => vec![
            leaves.model_visible_output(summary, source),
            leaves.result_leaf(
                "diff",
                ContextNodeKind::Diff,
                "Diff",
                diff_preview,
                "tool diff",
            ),
        ],
        ToolContextResult::Image { description, image } => {
            let image_text = format!(
                "mime_type: {}\nbase64 bytes: {}",
                image.mime_type, image.base64_bytes
            );
            vec![
                leaves.model_visible_output(description, source),
                leaves.result_leaf(
                    "image",
                    ContextNodeKind::Image,
                    "Image metadata",
                    &image_text,
                    "tool image metadata",
                ),
            ]
        }
    }
}

fn read_freshness_node(
    id: &str,
    role: Option<ContextRole>,
    source: ContextSourceRef,
    evidence: &ReadEvidence,
    row_metadata: ContextTokenMetadata,
) -> ContextNode {
    let text = evidence.ledger_text();
    ContextNode::leaf(
        format!("{id}-read-freshness"),
        ContextNodeKind::ReadFreshness,
        ContextInclusion::NotSent,
        role,
        format!("Read {}", evidence.ledger_label()),
        0,
        &text,
        row_metadata,
    )
    .with_source(source)
}

pub(crate) fn command_status_text(exit_code: Option<i32>, timed_out: bool) -> String {
    if timed_out {
        "timed out: true".to_string()
    } else {
        match exit_code {
            Some(code) => format!("exit code: {code}\ntimed out: false"),
            None => "exit code: unknown\ntimed out: false".to_string(),
        }
    }
}
