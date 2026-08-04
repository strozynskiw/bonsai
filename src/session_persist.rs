use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use anyhow::Result;
use async_openai::types::chat::ChatCompletionRequestMessage;

use crate::agent::{Agent, ContextControlState, ContextMessageSnapshot, UsageTotals, UsageTurn};
use crate::context_view::CompactionEvent;
use crate::storage::{SessionId, SessionSnapshot, Storage};
use crate::todo::TodoItem;
use crate::tool::read_evidence::{InspectionEventRecord, ReadEvidenceRecord};
use crate::tui::app::{ToolActivity, TranscriptItem};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AgentStateSignatures {
    pub(crate) context: u64,
    pub(crate) context_controls: u64,
    pub(crate) usage: u64,
    pub(crate) verification: u64,
    pub(crate) self_review: u64,
    pub(crate) episodes: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SessionSnapshotSignatures {
    pub(crate) transcript: u64,
    pub(crate) plan: u64,
    pub(crate) todo: u64,
    pub(crate) context: u64,
    pub(crate) context_controls: u64,
    pub(crate) usage: u64,
    pub(crate) verification: u64,
    pub(crate) self_review: u64,
    pub(crate) episodes: u64,
}

impl SessionSnapshotSignatures {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn reset_to(
        &mut self,
        transcript: &[TranscriptItem],
        plan: &crate::plan::PlanDoc,
        todos: &[TodoItem],
    ) {
        self.transcript = transcript_signature(transcript);
        self.plan = plan_signature(plan);
        self.todo = todo_signature(todos);
        self.context = 0;
        self.context_controls = 0;
        self.usage = 0;
        self.verification = 0;
        self.self_review = 0;
        self.episodes = 0;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AgentStateSnapshot {
    context: ContextMessageSnapshot,
    read_evidence: Vec<ReadEvidenceRecord>,
    inspection_events: Vec<InspectionEventRecord>,
    usage: UsageTotals,
    controls: HashMap<String, ContextControlState>,
    sources: HashMap<String, Vec<ChatCompletionRequestMessage>>,
    compaction_events: Vec<CompactionEvent>,
    usage_turns: Vec<UsageTurn>,
    verification_runs: Vec<crate::verification::VerificationRunRecord>,
    self_review_runs: Vec<crate::self_review::SelfReviewRunRecord>,
    /// `None` when the episodes feature is unwired — persistence then never
    /// touches the episode tables, not even with a bare DELETE.
    episodes: Option<Vec<crate::episode::Episode>>,
    peer_delivery_receipts: Vec<crate::storage::PeerDeliveryReceipt>,
}

impl AgentStateSnapshot {
    pub(crate) fn capture(agent: &Agent) -> Self {
        Self {
            context: agent.context_message_snapshot(),
            read_evidence: agent.read_evidence_snapshot(),
            inspection_events: agent.inspection_event_snapshot(),
            usage: agent.usage_totals(),
            controls: agent.context_controls().clone(),
            sources: agent.summary_sources().clone(),
            compaction_events: agent.compaction_events().to_vec(),
            usage_turns: agent.usage_turns().to_vec(),
            verification_runs: agent.verification_runs().to_vec(),
            self_review_runs: agent.self_review_runs().to_vec(),
            episodes: agent.episodes_snapshot(),
            peer_delivery_receipts: agent.pending_peer_delivery_receipts(),
        }
    }

    pub(crate) fn context(&self) -> &ContextMessageSnapshot {
        &self.context
    }

    pub(crate) fn read_evidence(&self) -> &[ReadEvidenceRecord] {
        &self.read_evidence
    }

    pub(crate) fn inspection_events(&self) -> &[InspectionEventRecord] {
        &self.inspection_events
    }

    pub(crate) fn controls(&self) -> &HashMap<String, ContextControlState> {
        &self.controls
    }

    pub(crate) fn sources(&self) -> &HashMap<String, Vec<ChatCompletionRequestMessage>> {
        &self.sources
    }

    pub(crate) fn compaction_events(&self) -> &[CompactionEvent] {
        &self.compaction_events
    }

    pub(crate) fn usage(&self) -> &UsageTotals {
        &self.usage
    }

    pub(crate) fn usage_turns(&self) -> &[UsageTurn] {
        &self.usage_turns
    }

    pub(crate) fn verification_runs(&self) -> &[crate::verification::VerificationRunRecord] {
        &self.verification_runs
    }

    pub(crate) fn self_review_runs(&self) -> &[crate::self_review::SelfReviewRunRecord] {
        &self.self_review_runs
    }

    pub(crate) fn episodes(&self) -> Option<&[crate::episode::Episode]> {
        self.episodes.as_deref()
    }

    pub(crate) fn peer_delivery_receipts(&self) -> &[crate::storage::PeerDeliveryReceipt] {
        &self.peer_delivery_receipts
    }
}

pub(crate) fn agent_state_signatures(snapshot: &AgentStateSnapshot) -> AgentStateSignatures {
    AgentStateSignatures {
        context: context_signature(
            &snapshot.context,
            &snapshot.read_evidence,
            &snapshot.inspection_events,
        ),
        context_controls: context_control_signature(
            &snapshot.controls,
            &snapshot.sources,
            &snapshot.compaction_events,
        ),
        usage: usage_signature(snapshot.usage, &snapshot.usage_turns),
        verification: verification_signature(&snapshot.verification_runs),
        self_review: self_review_signature(&snapshot.self_review_runs),
        episodes: episodes_signature(snapshot.episodes.as_deref()),
    }
}

pub(crate) fn usage_state_signature(usage: UsageTotals, turns: &[UsageTurn]) -> u64 {
    usage_signature(usage, turns)
}

#[derive(Debug)]
pub(crate) struct UsageStateSnapshot {
    pub(crate) usage: UsageTotals,
    pub(crate) turns: Vec<UsageTurn>,
}

#[derive(Debug)]
pub(crate) struct SessionSnapshotData<'a> {
    pub(crate) transcript: &'a [TranscriptItem],
    pub(crate) plan: &'a crate::plan::PlanDoc,
    pub(crate) todos: &'a [TodoItem],
    pub(crate) agent: Option<&'a AgentStateSnapshot>,
    pub(crate) fallback_usage: Option<&'a UsageStateSnapshot>,
    pub(crate) ui_peer_delivery_receipts: &'a [crate::storage::PeerDeliveryReceipt],
    pub(crate) agent_peer_delivery_receipts: &'a [crate::storage::PeerDeliveryReceipt],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SnapshotGroup {
    Transcript,
    Plan,
    Todos,
    Context,
    ContextControls,
    Usage,
    Verification,
    SelfReview,
    Episodes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SnapshotWriteOutcome {
    pub(crate) signatures: SessionSnapshotSignatures,
    pub(crate) changed: bool,
}

#[derive(Debug)]
pub(crate) struct SessionSnapshotWriter<'a> {
    storage: &'a Storage,
    session_id: SessionId,
}

impl<'a> SessionSnapshotWriter<'a> {
    pub(crate) fn new(storage: &'a Storage, session_id: SessionId) -> Self {
        Self {
            storage,
            session_id,
        }
    }

    pub(crate) async fn persist(
        &self,
        snapshot: SessionSnapshotData<'_>,
        previous: SessionSnapshotSignatures,
    ) -> Result<SnapshotWriteOutcome> {
        self.persist_with_checkpoint(snapshot, previous, |_| Ok(()))
            .await
    }

    async fn persist_with_checkpoint(
        &self,
        snapshot: SessionSnapshotData<'_>,
        previous: SessionSnapshotSignatures,
        mut checkpoint: impl FnMut(SnapshotGroup) -> Result<()>,
    ) -> Result<SnapshotWriteOutcome> {
        let transcript_signature = transcript_signature(snapshot.transcript);
        let plan_signature = plan_signature(snapshot.plan);
        let todo_signature = todo_signature(snapshot.todos);
        let agent_signatures = snapshot.agent.map(agent_state_signatures);
        let context_signature = agent_signatures.map_or(previous.context, |value| value.context);
        let context_control_signature =
            agent_signatures.map_or(previous.context_controls, |value| value.context_controls);
        let fallback_usage_signature = snapshot
            .fallback_usage
            .map(|usage| usage_state_signature(usage.usage, &usage.turns));
        let usage_signature = agent_signatures
            .map(|value| value.usage)
            .or(fallback_usage_signature)
            .unwrap_or(previous.usage);
        let verification_signature =
            agent_signatures.map_or(previous.verification, |value| value.verification);
        let self_review_signature =
            agent_signatures.map_or(previous.self_review, |value| value.self_review);
        let episodes_signature = agent_signatures.map_or(previous.episodes, |value| value.episodes);

        let transcript_changed = transcript_signature != previous.transcript;
        let plan_changed = plan_signature != previous.plan;
        let todo_changed = todo_signature != previous.todo;
        let context_changed = context_signature != previous.context;
        let context_controls_changed = context_control_signature != previous.context_controls;
        let usage_changed = usage_signature != previous.usage;
        let verification_changed = verification_signature != previous.verification;
        let self_review_changed = self_review_signature != previous.self_review;
        let episodes_changed = snapshot
            .agent
            .and_then(AgentStateSnapshot::episodes)
            .is_some_and(|_| episodes_signature != previous.episodes);
        let changed = transcript_changed
            || plan_changed
            || todo_changed
            || context_changed
            || context_controls_changed
            || usage_changed
            || verification_changed
            || self_review_changed
            || episodes_changed
            || !snapshot.ui_peer_delivery_receipts.is_empty()
            || !snapshot.agent_peer_delivery_receipts.is_empty();
        if !changed {
            return Ok(SnapshotWriteOutcome {
                signatures: previous,
                changed: false,
            });
        }

        let storage = self.storage;
        let session_id = self.session_id;
        storage
            .with_session_snapshot_tx("session snapshot batch", async move |tx, now| {
                if transcript_changed || !snapshot.ui_peer_delivery_receipts.is_empty() {
                    if transcript_changed {
                        storage
                            .replace_transcript_snapshot_in_tx(
                                tx,
                                session_id,
                                snapshot.transcript,
                                now,
                            )
                            .await?;
                    }
                    if !snapshot.ui_peer_delivery_receipts.is_empty() {
                        storage
                            .acknowledge_peer_deliveries_in_tx(
                                tx,
                                session_id,
                                crate::storage::PeerDeliveryConsumer::Ui,
                                snapshot.ui_peer_delivery_receipts,
                                now,
                            )
                            .await?;
                    }
                    checkpoint(SnapshotGroup::Transcript)?;
                }
                if plan_changed {
                    storage
                        .replace_plan_snapshot_in_tx(tx, session_id, snapshot.plan, now)
                        .await?;
                    checkpoint(SnapshotGroup::Plan)?;
                }
                if todo_changed {
                    storage
                        .replace_todos_snapshot_in_tx(tx, session_id, snapshot.todos, now)
                        .await?;
                    checkpoint(SnapshotGroup::Todos)?;
                }
                if let Some(agent) = snapshot.agent {
                    if context_changed || !snapshot.agent_peer_delivery_receipts.is_empty() {
                        storage
                            .replace_context_snapshot_in_tx(tx, session_id, agent.context(), now)
                            .await?;
                        storage
                            .replace_read_evidence_snapshot_in_tx(
                                tx,
                                session_id,
                                agent.read_evidence(),
                                now,
                            )
                            .await?;
                        storage
                            .replace_inspection_events_snapshot_in_tx(
                                tx,
                                session_id,
                                agent.inspection_events(),
                                now,
                            )
                            .await?;
                        if !snapshot.agent_peer_delivery_receipts.is_empty() {
                            storage
                                .acknowledge_peer_deliveries_in_tx(
                                    tx,
                                    session_id,
                                    crate::storage::PeerDeliveryConsumer::Agent,
                                    snapshot.agent_peer_delivery_receipts,
                                    now,
                                )
                                .await?;
                        }
                        checkpoint(SnapshotGroup::Context)?;
                    }
                    if context_controls_changed {
                        storage
                            .replace_context_control_snapshot_in_tx(
                                tx,
                                session_id,
                                agent.controls(),
                                agent.sources(),
                                now,
                            )
                            .await?;
                        storage
                            .replace_compaction_events_snapshot_in_tx(
                                tx,
                                session_id,
                                agent.compaction_events(),
                                now,
                            )
                            .await?;
                        checkpoint(SnapshotGroup::ContextControls)?;
                    }
                    if usage_changed {
                        storage
                            .update_session_usage_in_tx(tx, session_id, agent.usage(), now)
                            .await?;
                        storage
                            .sync_usage_turns_snapshot_in_tx(
                                tx,
                                session_id,
                                agent.usage_turns(),
                                now,
                            )
                            .await?;
                        checkpoint(SnapshotGroup::Usage)?;
                    }
                    if verification_changed {
                        storage
                            .replace_verification_runs_snapshot_in_tx(
                                tx,
                                session_id,
                                agent.verification_runs(),
                                now,
                            )
                            .await?;
                        checkpoint(SnapshotGroup::Verification)?;
                    }
                    if self_review_changed {
                        storage
                            .replace_self_review_runs_snapshot_in_tx(
                                tx,
                                session_id,
                                agent.self_review_runs(),
                                now,
                            )
                            .await?;
                        checkpoint(SnapshotGroup::SelfReview)?;
                    }
                    if episodes_changed && let Some(episodes) = agent.episodes() {
                        storage
                            .replace_episodes_snapshot_in_tx(tx, session_id, episodes, now)
                            .await?;
                        checkpoint(SnapshotGroup::Episodes)?;
                    }
                } else if usage_changed && let Some(usage) = snapshot.fallback_usage {
                    storage
                        .update_session_usage_in_tx(tx, session_id, &usage.usage, now)
                        .await?;
                    storage
                        .sync_usage_turns_snapshot_in_tx(tx, session_id, &usage.turns, now)
                        .await?;
                    checkpoint(SnapshotGroup::Usage)?;
                }
                Ok(())
            })
            .await?;

        Ok(SnapshotWriteOutcome {
            signatures: SessionSnapshotSignatures {
                transcript: transcript_signature,
                plan: plan_signature,
                todo: todo_signature,
                context: context_signature,
                context_controls: context_control_signature,
                usage: usage_signature,
                verification: verification_signature,
                self_review: self_review_signature,
                episodes: if episodes_changed {
                    episodes_signature
                } else {
                    previous.episodes
                },
            },
            changed: true,
        })
    }
}

#[cfg(test)]
pub(crate) async fn persist_agent_state(
    storage: &Storage,
    session_id: SessionId,
    snapshot: &AgentStateSnapshot,
    signatures: &mut AgentStateSignatures,
) {
    let agent_signatures = agent_state_signatures(snapshot);
    let context_signature = agent_signatures.context;
    if context_signature != signatures.context || !snapshot.peer_delivery_receipts.is_empty() {
        match storage
            .replace_agent_context_snapshot(
                session_id,
                &snapshot.context,
                &snapshot.read_evidence,
                &snapshot.inspection_events,
                &snapshot.peer_delivery_receipts,
            )
            .await
        {
            Ok(()) => signatures.context = context_signature,
            Err(err) => tracing::warn!(
                session_id = %session_id,
                error = %err,
                "failed to persist agent context snapshot"
            ),
        }
    }

    let context_control_signature = agent_signatures.context_controls;
    if context_control_signature != signatures.context_controls {
        match storage
            .replace_context_control_snapshot(session_id, &snapshot.controls, &snapshot.sources)
            .await
        {
            Ok(()) => signatures.context_controls = context_control_signature,
            Err(err) => tracing::warn!(
                session_id = %session_id,
                error = %err,
                "failed to persist context controls"
            ),
        }
        if let Err(err) = storage
            .replace_compaction_events_snapshot(session_id, &snapshot.compaction_events)
            .await
        {
            tracing::warn!(
                session_id = %session_id,
                error = %err,
                "failed to persist compaction events"
            );
        }
    }

    let usage_signature = agent_signatures.usage;
    if usage_signature != signatures.usage {
        let usage_result = storage
            .update_session_usage_totals(session_id, &snapshot.usage)
            .await;
        let turns_result = storage
            .sync_usage_turns_snapshot(session_id, &snapshot.usage_turns)
            .await;
        match (usage_result, turns_result) {
            (Ok(()), Ok(())) => signatures.usage = usage_signature,
            (Err(err), _) => tracing::warn!(
                session_id = %session_id,
                error = %err,
                "failed to persist usage totals"
            ),
            (_, Err(err)) => tracing::warn!(
                session_id = %session_id,
                error = %err,
                "failed to persist usage turns"
            ),
        }
    }

    let verification_signature = verification_signature(&snapshot.verification_runs);
    if verification_signature != signatures.verification {
        match storage
            .replace_verification_runs_snapshot(session_id, &snapshot.verification_runs)
            .await
        {
            Ok(()) => signatures.verification = verification_signature,
            Err(err) => tracing::warn!(
                session_id = %session_id,
                error = %err,
                "failed to persist verification runs"
            ),
        }
    }

    let self_review_signature = self_review_signature(&snapshot.self_review_runs);
    if self_review_signature != signatures.self_review {
        match storage
            .replace_self_review_runs_snapshot(session_id, &snapshot.self_review_runs)
            .await
        {
            Ok(()) => signatures.self_review = self_review_signature,
            Err(err) => tracing::warn!(
                session_id = %session_id,
                error = %err,
                "failed to persist self-review runs"
            ),
        }
    }

    // Episodes persist only when the feature is wired (`snapshot.episodes` is
    // `Some`); a disabled session leaves the episode tables entirely alone.
    if let Some(episodes) = snapshot.episodes.as_deref() {
        let episodes_signature = agent_signatures.episodes;
        if episodes_signature != signatures.episodes {
            match storage
                .replace_episodes_snapshot(session_id, episodes)
                .await
            {
                Ok(()) => signatures.episodes = episodes_signature,
                Err(err) => tracing::warn!(
                    session_id = %session_id,
                    error = %err,
                    "failed to persist episodes"
                ),
            }
        }
    }
}

pub(crate) async fn restore_agent_state(agent: &mut Agent, snapshot: &SessionSnapshot) {
    if snapshot.context_messages.is_empty() {
        agent
            .restore_persisted_text_history(&snapshot.message_history)
            .await;
    } else {
        agent
            .restore_persisted_context_messages_with_ids(
                snapshot.context_messages.clone(),
                snapshot.context_message_ids.clone(),
            )
            .await;
    }
    agent.restore_context_controls(
        snapshot.context_controls.clone(),
        snapshot.context_sources.clone(),
    );
    agent
        .restore_read_evidence(snapshot.read_evidence.clone())
        .await;
    agent.restore_inspection_events(snapshot.inspection_events.clone());
    agent.restore_compaction_events(snapshot.compaction_events.clone());
    agent.restore_usage_totals(
        snapshot.summary.prompt_token_count.max(0) as u64,
        snapshot.summary.completion_token_count.max(0) as u64,
        crate::storage::decode_cost_micros(snapshot.summary.cost_micros),
        crate::storage::decode_cost_micros(snapshot.summary.no_cache_cost_micros),
        crate::storage::decode_input_cache_usage(
            snapshot.summary.cache_read_input_token_count,
            snapshot.summary.cache_creation_input_token_count,
            snapshot.summary.cache_measured_input_token_count,
        ),
    );
    agent.restore_usage_turns(snapshot.usage_turns.clone());
    agent.restore_verification_runs(snapshot.verification_runs.clone());
    agent.restore_self_review_runs(snapshot.self_review_runs.clone());
    // After context messages/ids and controls/sources, so active-span
    // validation can run against the restored stable ids.
    agent.restore_episodes(snapshot.episodes.clone());
}

pub(crate) fn derive_session_title(input: &str) -> Option<String> {
    const MAX_CHARS: usize = 60;
    let collapsed = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() <= MAX_CHARS {
        return Some(collapsed);
    }
    let truncated: String = collapsed.chars().take(MAX_CHARS).collect();
    let head = match truncated.rfind(' ') {
        Some(idx) if idx >= MAX_CHARS / 2 => &truncated[..idx],
        _ => truncated.trim_end(),
    };
    Some(format!("{}…", head.trim_end()))
}

pub(crate) fn transcript_signature(transcript: &[TranscriptItem]) -> u64 {
    let mut hasher = DefaultHasher::new();
    transcript
        .iter()
        .filter(|item| !item.is_suppressed())
        .count()
        .hash(&mut hasher);
    for item in transcript {
        if item.is_suppressed() {
            continue;
        }
        match item {
            TranscriptItem::UserMessage { text } => {
                "user".hash(&mut hasher);
                text.hash(&mut hasher);
            }
            TranscriptItem::QueuedUserMessage { id, text, delivery } => {
                "queued_user".hash(&mut hasher);
                id.hash(&mut hasher);
                text.hash(&mut hasher);
                delivery.pending_label().hash(&mut hasher);
            }
            TranscriptItem::AssistantMessage { text } => {
                "assistant".hash(&mut hasher);
                text.hash(&mut hasher);
            }
            TranscriptItem::ReasoningSummary { text } => {
                "thinking".hash(&mut hasher);
                text.hash(&mut hasher);
            }
            TranscriptItem::ToolActivity(activity) => {
                "tool".hash(&mut hasher);
                hash_tool_activity(activity, &mut hasher);
            }
            TranscriptItem::CommandOutput { kind, text } => {
                "command".hash(&mut hasher);
                kind.as_db_str().hash(&mut hasher);
                text.hash(&mut hasher);
            }
            TranscriptItem::CompletionReport(report) => {
                "completion_report".hash(&mut hasher);
                if let Ok(raw) = serde_json::to_string(report) {
                    raw.hash(&mut hasher);
                }
            }
            TranscriptItem::ExecutionGroup(group) => {
                "group".hash(&mut hasher);
                group.id.hash(&mut hasher);
                group.finished_at.is_some().hash(&mut hasher);
                group.tools.len().hash(&mut hasher);
                for activity in &group.tools {
                    hash_tool_activity(activity, &mut hasher);
                }
            }
            TranscriptItem::Error { error } => {
                "error".hash(&mut hasher);
                error.title.hash(&mut hasher);
                error.detail.hash(&mut hasher);
            }
            TranscriptItem::Edit { text } => {
                "edit".hash(&mut hasher);
                text.hash(&mut hasher);
            }
            TranscriptItem::TodoUpdate { text } => {
                "todo".hash(&mut hasher);
                text.hash(&mut hasher);
            }
            TranscriptItem::WorkLog { text } => {
                "worklog".hash(&mut hasher);
                text.hash(&mut hasher);
            }
            TranscriptItem::PeerMessage {
                source_message_id,
                session_id,
                outgoing,
                text,
            } => {
                "peer_message".hash(&mut hasher);
                source_message_id.hash(&mut hasher);
                session_id.hash(&mut hasher);
                outgoing.hash(&mut hasher);
                text.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

pub(crate) fn plan_signature(plan: &crate::plan::PlanDoc) -> u64 {
    let mut hasher = DefaultHasher::new();
    plan.title.hash(&mut hasher);
    plan.revision.hash(&mut hasher);
    plan.sections.len().hash(&mut hasher);
    for section in &plan.sections {
        section.heading.hash(&mut hasher);
        section.body.hash(&mut hasher);
    }
    plan.tasks.len().hash(&mut hasher);
    for task in &plan.tasks {
        task.text.hash(&mut hasher);
        task.done.hash(&mut hasher);
    }
    plan.findings.len().hash(&mut hasher);
    for finding in &plan.findings {
        finding.severity.as_db_str().hash(&mut hasher);
        finding.file.hash(&mut hasher);
        finding.line.hash(&mut hasher);
        finding.issue.hash(&mut hasher);
        finding.required_fix.hash(&mut hasher);
        finding.acceptance_tests.hash(&mut hasher);
        finding.source_ids.hash(&mut hasher);
        finding.task.hash(&mut hasher);
        finding.resolved.hash(&mut hasher);
    }
    hasher.finish()
}

pub(crate) fn todo_signature(todos: &[TodoItem]) -> u64 {
    let mut hasher = DefaultHasher::new();
    todos.len().hash(&mut hasher);
    for todo in todos {
        todo.content.hash(&mut hasher);
        todo.status.as_db_str().hash(&mut hasher);
    }
    hasher.finish()
}

fn context_signature(
    snapshot: &ContextMessageSnapshot,
    read_evidence: &[ReadEvidenceRecord],
    inspection_events: &[InspectionEventRecord],
) -> u64 {
    let mut hasher = DefaultHasher::new();
    snapshot.messages.len().hash(&mut hasher);
    for (index, message) in snapshot.messages.iter().enumerate() {
        snapshot.ids.get(index).hash(&mut hasher);
        if let Ok(raw) = serde_json::to_string(message) {
            raw.hash(&mut hasher);
        }
    }
    read_evidence.hash(&mut hasher);
    inspection_events.hash(&mut hasher);
    hasher.finish()
}

fn context_control_signature(
    controls: &HashMap<String, ContextControlState>,
    sources: &HashMap<String, Vec<ChatCompletionRequestMessage>>,
    compaction_events: &[CompactionEvent],
) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut control_keys = controls.keys().collect::<Vec<_>>();
    control_keys.sort();
    for key in control_keys {
        key.hash(&mut hasher);
        if let Ok(raw) = serde_json::to_string(&controls[key]) {
            raw.hash(&mut hasher);
        }
    }
    let mut source_keys = sources.keys().collect::<Vec<_>>();
    source_keys.sort();
    for key in source_keys {
        key.hash(&mut hasher);
        for message in &sources[key] {
            if let Ok(raw) = serde_json::to_string(message) {
                raw.hash(&mut hasher);
            }
        }
    }
    compaction_events.hash(&mut hasher);
    hasher.finish()
}

fn usage_signature(usage: UsageTotals, turns: &[UsageTurn]) -> u64 {
    let mut hasher = DefaultHasher::new();
    usage.hash(&mut hasher);
    turns.hash(&mut hasher);
    hasher.finish()
}

fn verification_signature(runs: &[crate::verification::VerificationRunRecord]) -> u64 {
    let mut hasher = DefaultHasher::new();
    if let Ok(raw) = serde_json::to_string(runs) {
        raw.hash(&mut hasher);
    }
    hasher.finish()
}

fn self_review_signature(runs: &[crate::self_review::SelfReviewRunRecord]) -> u64 {
    let mut hasher = DefaultHasher::new();
    if let Ok(raw) = serde_json::to_string(runs) {
        raw.hash(&mut hasher);
    }
    hasher.finish()
}

/// Hash every persisted episode field, each serialized archive message, and
/// the recall counter, so archive- or telemetry-only mutations still flush.
/// `None` (feature unwired) hashes to 0, distinct from an empty wired ledger.
fn episodes_signature(episodes: Option<&[crate::episode::Episode]>) -> u64 {
    let Some(episodes) = episodes else {
        return 0;
    };
    let mut hasher = DefaultHasher::new();
    episodes.len().hash(&mut hasher);
    for episode in episodes {
        episode.seq().hash(&mut hasher);
        episode.title().hash(&mut hasher);
        episode.status().as_db_str().hash(&mut hasher);
        episode.goal().hash(&mut hasher);
        episode.card_md().hash(&mut hasher);
        episode
            .close_reason()
            .map(crate::episode::EpisodeCloseReason::as_db_str)
            .hash(&mut hasher);
        episode.start_stable_id().hash(&mut hasher);
        episode.end_stable_id().hash(&mut hasher);
        episode.marker_stable_id().hash(&mut hasher);
        episode.files_touched().hash(&mut hasher);
        episode.opened_at_ms().hash(&mut hasher);
        episode.closed_at_ms().hash(&mut hasher);
        episode.evicted_at_ms().hash(&mut hasher);
        episode.evicted_tokens().hash(&mut hasher);
        episode.recall_count().hash(&mut hasher);
        episode.is_completable().hash(&mut hasher);
        episode.archive().len().hash(&mut hasher);
        for item in episode.archive() {
            item.stable_id.hash(&mut hasher);
            if let Ok(raw) = serde_json::to_string(&item.message) {
                raw.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

fn hash_tool_activity(activity: &ToolActivity, hasher: &mut DefaultHasher) {
    activity.id.hash(hasher);
    activity.name.hash(hasher);
    activity.arguments.hash(hasher);
    activity.status.as_db_str().hash(hasher);
    activity.result.hash(hasher);
    activity.finished_at.is_some().hash(hasher);
    if let Some(diff) = &activity.diff {
        diff.path.hash(hasher);
        format!("{:?}", diff.status).hash(hasher);
        diff.hunks.len().hash(hasher);
        diff.truncated.hash(hasher);
        diff.old_size.hash(hasher);
        diff.new_size.hash(hasher);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_snapshot() -> AgentStateSnapshot {
        AgentStateSnapshot {
            context: ContextMessageSnapshot {
                messages: Vec::new(),
                ids: Vec::new(),
            },
            read_evidence: Vec::new(),
            inspection_events: Vec::new(),
            usage: UsageTotals::default(),
            controls: HashMap::new(),
            sources: HashMap::new(),
            compaction_events: Vec::new(),
            usage_turns: Vec::new(),
            verification_runs: Vec::new(),
            self_review_runs: Vec::new(),
            episodes: Some(Vec::new()),
            peer_delivery_receipts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn every_snapshot_group_failure_rolls_back_the_whole_batch() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let session_id = fixture.start_session().await;
        let transcript = vec![TranscriptItem::UserMessage {
            text: "atomic transcript".to_string(),
        }];
        let plan = crate::plan::PlanDoc {
            title: "atomic plan".to_string(),
            ..crate::plan::PlanDoc::default()
        };
        let todos = vec![TodoItem {
            content: "atomic todo".to_string(),
            status: crate::todo::TodoStatus::Pending,
        }];
        let agent = agent_snapshot();
        let writer = SessionSnapshotWriter::new(&fixture.storage, session_id);

        for failed_group in [
            SnapshotGroup::Transcript,
            SnapshotGroup::Plan,
            SnapshotGroup::Todos,
            SnapshotGroup::Context,
            SnapshotGroup::ContextControls,
            SnapshotGroup::Usage,
            SnapshotGroup::Verification,
            SnapshotGroup::SelfReview,
            SnapshotGroup::Episodes,
        ] {
            let snapshot = SessionSnapshotData {
                transcript: &transcript,
                plan: &plan,
                todos: &todos,
                agent: Some(&agent),
                fallback_usage: None,
                ui_peer_delivery_receipts: &[],
                agent_peer_delivery_receipts: &[],
            };
            let mut reached = false;
            let error = writer
                .persist_with_checkpoint(snapshot, SessionSnapshotSignatures::default(), |group| {
                    if group == failed_group {
                        reached = true;
                        anyhow::bail!("injected failure after {group:?}");
                    }
                    Ok(())
                })
                .await
                .unwrap_err();

            assert!(reached, "checkpoint {failed_group:?} was not reached");
            assert!(error.to_string().contains("injected failure"));
            let stored = fixture
                .storage
                .load_session_snapshot(session_id)
                .await
                .unwrap()
                .unwrap();
            assert!(stored.transcript.is_empty(), "failed at {failed_group:?}");
            assert!(stored.plan.is_empty(), "failed at {failed_group:?}");
            assert!(stored.todos.is_empty(), "failed at {failed_group:?}");
        }

        let snapshot = SessionSnapshotData {
            transcript: &transcript,
            plan: &plan,
            todos: &todos,
            agent: Some(&agent),
            fallback_usage: None,
            ui_peer_delivery_receipts: &[],
            agent_peer_delivery_receipts: &[],
        };
        let outcome = writer
            .persist(snapshot, SessionSnapshotSignatures::default())
            .await
            .unwrap();
        assert!(outcome.changed);
        let stored = fixture
            .storage
            .load_session_snapshot(session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.transcript.len(), 1);
        assert!(matches!(
            &stored.transcript[0],
            TranscriptItem::UserMessage { text } if text == "atomic transcript"
        ));
        assert_eq!(stored.plan.title, "atomic plan");
        assert_eq!(stored.todos, todos);
    }

    #[tokio::test]
    async fn only_durable_transcript_rows_are_persisted() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let session_id = fixture.start_session().await;
        let transcript = vec![TranscriptItem::CommandOutput {
            kind: crate::tui::event::CommandOutputKind::Status,
            text: "Durable action required.".to_string(),
        }];
        let plan = crate::plan::PlanDoc::default();
        let todos = Vec::new();
        let writer = SessionSnapshotWriter::new(&fixture.storage, session_id);

        let outcome = writer
            .persist(
                SessionSnapshotData {
                    transcript: &transcript,
                    plan: &plan,
                    todos: &todos,
                    agent: None,
                    fallback_usage: None,
                    ui_peer_delivery_receipts: &[],
                    agent_peer_delivery_receipts: &[],
                },
                SessionSnapshotSignatures::default(),
            )
            .await
            .unwrap();

        assert!(outcome.changed);
        let stored = fixture
            .storage
            .load_session_snapshot(session_id)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            stored.transcript.as_slice(),
            [TranscriptItem::CommandOutput {
                kind: crate::tui::event::CommandOutputKind::Status,
                text,
            }] if text == "Durable action required."
        ));
    }
}
