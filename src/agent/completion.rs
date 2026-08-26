//! Structured completion gate for action-oriented agent runs.
//!
//! Model prose is deliberately only a supporting signal here. The terminal
//! decision is derived from todo, mutation, verification, review, and pending
//! runtime state that Bonsai observed itself.

use serde::{Deserialize, Serialize};

use super::*;
use crate::task_intent::{TaskPromptKind, action_directive, normalized_words};

/// Semantic purpose of the current human task.
///
/// This is descriptive evidence for traces and steering merges. It never
/// grants tool authority; permissions and observable completion effects remain
/// separate enforcement boundaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskIntent {
    #[default]
    Informational,
    Diagnosis,
    Review,
    Mutation,
    Verification,
    Monitoring,
}

impl TaskIntent {
    const fn requires_action(self) -> bool {
        !matches!(self, Self::Informational)
    }

    const fn merge(self, other: Self) -> Self {
        if other.precedence() > self.precedence() {
            other
        } else {
            self
        }
    }

    const fn precedence(self) -> u8 {
        match self {
            Self::Informational => 0,
            Self::Diagnosis => 1,
            Self::Review => 2,
            Self::Monitoring => 3,
            Self::Verification => 4,
            Self::Mutation => 5,
        }
    }
}

/// Broad kind of work the current human turn requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionGoalKind {
    Informational,
    Action,
}

/// Observable effect an action task must produce before it may succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionEffectRequirement {
    None,
    AnyAction,
    WorkspaceMutation,
}

/// Structured contract inferred or explicitly installed for one task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TaskCompletionContract {
    /// What the user is asking Bonsai to accomplish, independent of effects.
    #[serde(default)]
    pub(crate) intent: TaskIntent,
    pub(crate) goal_kind: CompletionGoalKind,
    pub(crate) effect: CompletionEffectRequirement,
    pub(crate) verification_required: bool,
    #[serde(default)]
    bounded_read_only: bool,
}

impl Default for TaskCompletionContract {
    fn default() -> Self {
        Self::informational()
    }
}

impl TaskCompletionContract {
    pub(crate) const fn informational() -> Self {
        Self {
            intent: TaskIntent::Informational,
            goal_kind: CompletionGoalKind::Informational,
            effect: CompletionEffectRequirement::None,
            verification_required: false,
            bounded_read_only: false,
        }
    }

    pub(crate) const fn workspace_action() -> Self {
        Self {
            intent: TaskIntent::Mutation,
            goal_kind: CompletionGoalKind::Action,
            effect: CompletionEffectRequirement::WorkspaceMutation,
            verification_required: false,
            bounded_read_only: false,
        }
    }

    pub(crate) const fn action() -> Self {
        Self {
            intent: TaskIntent::Mutation,
            goal_kind: CompletionGoalKind::Action,
            effect: CompletionEffectRequirement::AnyAction,
            verification_required: false,
            bounded_read_only: false,
        }
    }

    pub(crate) const fn verification_action() -> Self {
        Self {
            intent: TaskIntent::Verification,
            goal_kind: CompletionGoalKind::Action,
            effect: CompletionEffectRequirement::AnyAction,
            verification_required: true,
            bounded_read_only: false,
        }
    }

    fn inferred(prompt: &str) -> Self {
        let normalized = normalized_words(prompt);
        let directive = action_directive(&normalized);
        let continuation = TaskPromptKind::classify(prompt).is_continuation();
        let explicit_read_only = contains_any_phrase(
            &normalized,
            &[
                "read only",
                "do not modify",
                "don t modify",
                "do not change",
                "don t change",
                "do not edit",
                "don t edit",
                "do not fix",
                "don t fix",
                "without mutation",
                "without modifying",
                "without changing",
                "without editing",
                "do not run commands",
                "don t run commands",
            ],
        );
        let workspace_mutation = contains_directive_clause(
            directive,
            &[
                "fix",
                "implement",
                "add",
                "address",
                "apply",
                "update",
                "change",
                "modify",
                "create",
                "delete",
                "remove",
                "rename",
                "resolve",
                "refactor",
                "write",
                "edit",
                "upgrade",
                "bump",
                "finish",
                "finish topic",
                "record",
                "preserve",
                "carry",
                "napraw",
                "zaimplementuj",
                "dodaj",
                "zaktualizuj",
                "zmien",
                "zmień",
                "usun",
                "usuń",
                "przemianuj",
                "zrefaktoryzuj",
            ],
        );
        let mutation = continuation
            || workspace_mutation
            || contains_directive_clause(
                directive,
                &[
                    "build",
                    "close",
                    "commit",
                    "deploy",
                    "merge",
                    "move",
                    "open",
                    "publish",
                    "push",
                    "work on",
                    "start working on",
                    "continue working on",
                    "pozamykaj",
                    "przenies",
                    "przenieś",
                    "zmerguj",
                    "zacznij",
                    "zacznij pracowac nad",
                    "zacznij pracować nad",
                ],
            );
        let verification = contains_directive_clause(
            directive,
            &[
                "run tests",
                "run the tests",
                "run checks",
                "run cargo test",
                "run cargo check",
                "run cargo build",
                "cargo test",
                "cargo check",
                "cargo build",
                "test",
                "verify",
                "build",
                "sprawdz testy",
                "sprawdź testy",
                "uruchom testy",
            ],
        );
        let generic_action = contains_directive_clause(directive, &["run", "uruchom"]);
        let explicit_broad_effect = mutation
            || verification
            || generic_action
            || contains_any_phrase(
                directive,
                &[
                    "ask ",
                    "delegate ",
                    "fetch ",
                    "search the web",
                    "browse ",
                    "download ",
                    "call ",
                    "finish it",
                    "complete it",
                ],
            );
        let monitoring = contains_directive_clause(
            directive,
            &[
                "monitor",
                "watch",
                "wait",
                "keep watching",
                "follow",
                "obserwuj",
                "poczekaj",
                "czekaj",
            ],
        );
        let review =
            contains_directive_clause(directive, &["audit", "review", "przejrzyj", "zrecenzuj"]);
        let diagnosis = contains_directive_clause(
            directive,
            &[
                "analyze",
                "check",
                "diagnose",
                "inspect",
                "investigate",
                "research",
                "trace",
                "sprawdz",
                "sprawdź",
                "przeanalizuj",
                "zbadaj",
            ],
        );

        let explicit_broad_effect = explicit_broad_effect || monitoring;

        let intent = if mutation {
            TaskIntent::Mutation
        } else if verification {
            TaskIntent::Verification
        } else if monitoring {
            TaskIntent::Monitoring
        } else if review {
            TaskIntent::Review
        } else if diagnosis {
            TaskIntent::Diagnosis
        } else if generic_action {
            TaskIntent::Mutation
        } else {
            TaskIntent::Informational
        };
        let effect = if workspace_mutation {
            CompletionEffectRequirement::WorkspaceMutation
        } else if mutation || verification || monitoring || review || diagnosis || generic_action {
            CompletionEffectRequirement::AnyAction
        } else {
            CompletionEffectRequirement::None
        };
        Self {
            intent,
            goal_kind: if effect == CompletionEffectRequirement::None {
                CompletionGoalKind::Informational
            } else {
                CompletionGoalKind::Action
            },
            effect,
            verification_required: verification,
            bounded_read_only: explicit_read_only && !explicit_broad_effect,
        }
    }

    fn merge(self, other: Self) -> Self {
        let effect = match (self.effect, other.effect) {
            (CompletionEffectRequirement::WorkspaceMutation, _)
            | (_, CompletionEffectRequirement::WorkspaceMutation) => {
                CompletionEffectRequirement::WorkspaceMutation
            }
            (CompletionEffectRequirement::AnyAction, _)
            | (_, CompletionEffectRequirement::AnyAction) => CompletionEffectRequirement::AnyAction,
            _ => CompletionEffectRequirement::None,
        };
        Self {
            intent: self.intent.merge(other.intent),
            goal_kind: if effect == CompletionEffectRequirement::None {
                CompletionGoalKind::Informational
            } else {
                CompletionGoalKind::Action
            },
            effect,
            verification_required: self.verification_required || other.verification_required,
            bounded_read_only: self.bounded_read_only && other.bounded_read_only,
        }
    }

    const fn is_read_only_task(self) -> bool {
        self.bounded_read_only
            && matches!(
                self.intent,
                TaskIntent::Informational | TaskIntent::Diagnosis | TaskIntent::Review
            )
    }
}

#[derive(Debug, Default)]
pub(super) struct ReadOnlyTaskProgress {
    inspection_turns: usize,
    conclusion_nudge_sent: bool,
    conclusion_turn: bool,
}

impl ReadOnlyTaskProgress {
    fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn observe_inspection_turn(
        &mut self,
        tool_calls: &[ToolCall],
    ) -> Option<&'static str> {
        if tool_calls.is_empty()
            || tool_calls
                .iter()
                .any(|tool_call| tool_call.name == "set_session_title")
        {
            return None;
        }
        self.inspection_turns = self.inspection_turns.saturating_add(1);
        if self.inspection_turns >= 8 && !self.conclusion_nudge_sent {
            self.conclusion_nudge_sent = true;
            self.conclusion_turn = true;
            return Some(
                "Read-only task evidence is sufficient after eight inspection turns. Do not run commands, emit tool-call syntax, or continue broad orientation; produce the requested explanation or review findings now using the evidence already collected.",
            );
        }
        None
    }

    pub(super) fn take_conclusion_turn(&mut self) -> bool {
        std::mem::take(&mut self.conclusion_turn)
    }
}

/// Todo state captured at a terminal candidate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionTodoEvidence {
    pub(crate) pending: usize,
    pub(crate) in_progress: usize,
    pub(crate) completed: usize,
    pub(crate) cancelled: usize,
}

/// Typed disposition of a policy-required check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub(crate) enum CompletionCheckEvidence {
    NotRequired,
    Satisfied,
    Skipped(String),
    Missing,
    Stale,
    Blocked(String),
}

/// Runtime work that must not be silently abandoned by a success result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionPendingEvidence {
    pub(crate) interactions: usize,
    pub(crate) subagents: usize,
    pub(crate) background_tasks: usize,
    pub(crate) terminals: usize,
    pub(crate) external_waits: usize,
}

/// Canonical observed state used for one completion decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionDecisionEvidence {
    pub(crate) contract: TaskCompletionContract,
    pub(crate) todos: CompletionTodoEvidence,
    pub(crate) action_observed: bool,
    pub(crate) workspace_mutated: bool,
    pub(crate) mutation_declared_unnecessary: bool,
    #[serde(default)]
    pub(crate) pending_work_started: bool,
    pub(crate) verification: CompletionCheckEvidence,
    pub(crate) review: CompletionCheckEvidence,
    pub(crate) pending: CompletionPendingEvidence,
}

/// One unmet structured completion condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum CompletionGap {
    PendingTodos(usize),
    InProgressTodos(usize),
    RequestedActionMissing,
    WorkspaceMutationMissing,
    VerificationMissing,
    VerificationStale,
    VerificationBlocked(String),
    ReviewMissing,
    ReviewStale,
    ReviewBlocked(String),
    InteractionPending(usize),
    SubagentPending(usize),
    BackgroundTaskPending(usize),
    TerminalPending(usize),
    ExternalWaitPending(usize),
    ImplementationStall(usize),
}

impl CompletionGap {
    fn label(&self) -> String {
        match self {
            Self::PendingTodos(count) => format!("{count} todo(s) pending"),
            Self::InProgressTodos(count) => format!("{count} todo(s) in progress"),
            Self::RequestedActionMissing => "requested action not observed".to_string(),
            Self::WorkspaceMutationMissing => {
                "requested workspace mutation not observed or declared unnecessary".to_string()
            }
            Self::VerificationMissing => "required verification is missing".to_string(),
            Self::VerificationStale => "verification is stale".to_string(),
            Self::VerificationBlocked(reason) => format!("verification blocked: {reason}"),
            Self::ReviewMissing => "required self-review disposition is missing".to_string(),
            Self::ReviewStale => "self-review evidence is stale".to_string(),
            Self::ReviewBlocked(reason) => format!("self-review blocked: {reason}"),
            Self::InteractionPending(count) => format!("{count} interaction(s) pending"),
            Self::SubagentPending(count) => format!("{count} subagent(s) running"),
            Self::BackgroundTaskPending(count) => {
                format!("{count} background task group(s) running")
            }
            Self::TerminalPending(count) => format!("{count} terminal group(s) running"),
            Self::ExternalWaitPending(count) => format!("{count} external wait(s) unresolved"),
            Self::ImplementationStall(turns) => {
                format!("implementation stalled after {turns} consecutive tool turns")
            }
        }
    }

    fn is_external_blocker(&self) -> bool {
        matches!(
            self,
            Self::VerificationBlocked(_)
                | Self::ReviewBlocked(_)
                | Self::InteractionPending(_)
                | Self::SubagentPending(_)
                | Self::BackgroundTaskPending(_)
                | Self::TerminalPending(_)
                | Self::ExternalWaitPending(_)
        )
    }
}

/// Supporting prose signals extracted from the candidate final response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionResponseSignals {
    pub(crate) promises_future_action: bool,
    pub(crate) explicit_blocker: bool,
    pub(crate) explicit_cancellation: bool,
    pub(crate) mutation_unnecessary: bool,
}

/// Typed non-success class returned when the bounded recovery cannot complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionFailureOutcome {
    Blocked,
    Failed,
    Cancelled,
}

/// Explainable terminal rejection from the structured completion guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionGuardFailure {
    pub(crate) outcome: CompletionFailureOutcome,
    pub(crate) gaps: Vec<CompletionGap>,
    pub(crate) detail: String,
}

impl CompletionGuardFailure {
    pub(crate) fn compact_detail(&self) -> &str {
        &self.detail
    }

    pub(super) fn implementation_stall(turns: usize, evidence: &str) -> Self {
        Self {
            outcome: CompletionFailureOutcome::Failed,
            gaps: vec![CompletionGap::ImplementationStall(turns)],
            detail: format!(
                "Agent stopped after {turns} consecutive tool turns without durable semantic \
                 progress. The bounded nudge and recovery window were exhausted. Recent evidence: \
                 {evidence}. Partial workspace and conversation state were preserved; steer or \
                 retry this session to resume."
            ),
        }
    }
}

/// One recorded terminal candidate and its observed evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionGuardAttempt {
    pub(crate) ordinal: usize,
    pub(crate) evidence: CompletionDecisionEvidence,
    pub(crate) response_signals: CompletionResponseSignals,
    pub(crate) gaps: Vec<CompletionGap>,
}

/// Final gate disposition retained in completion reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionGuardDisposition {
    Accepted,
    Continued,
    Blocked,
    Failed,
    Cancelled,
}

/// Durable, machine-readable explanation of the gate's decisions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionGuardTrace {
    pub(crate) contract: TaskCompletionContract,
    pub(crate) superseded_goals: usize,
    pub(crate) attempts: Vec<CompletionGuardAttempt>,
    pub(crate) disposition: CompletionGuardDisposition,
}

impl CompletionGuardTrace {
    pub(crate) fn render_compact(&self) -> String {
        let attempts = self.attempts.len();
        let disposition = match self.disposition {
            CompletionGuardDisposition::Accepted => "accepted",
            CompletionGuardDisposition::Continued => "continued",
            CompletionGuardDisposition::Blocked => "blocked",
            CompletionGuardDisposition::Failed => "failed",
            CompletionGuardDisposition::Cancelled => "cancelled",
        };
        let final_gaps = self
            .attempts
            .last()
            .map(|attempt| attempt.gaps.as_slice())
            .unwrap_or_default();
        if final_gaps.is_empty() {
            return format!("{disposition} · {attempts} attempt(s)");
        }
        let detail = final_gaps
            .iter()
            .map(CompletionGap::label)
            .collect::<Vec<_>>()
            .join("; ");
        format!("{disposition} · {attempts} attempt(s) · unresolved: {detail}",)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CompletionGuardVerdict {
    Accept,
    Continue { note: String },
    Reject(CompletionGuardFailure),
}

#[derive(Debug, Default)]
pub(super) struct CompletionGuardState {
    contract: TaskCompletionContract,
    verification_baseline: usize,
    review_baseline: usize,
    action_observed: bool,
    workspace_mutated: bool,
    pending_work_started: bool,
    continuation_used: bool,
    superseded_goals: usize,
    attempts: Vec<CompletionGuardAttempt>,
    disposition: Option<CompletionGuardDisposition>,
}

impl CompletionGuardState {
    fn begin(
        &mut self,
        contract: TaskCompletionContract,
        verification_baseline: usize,
        review_baseline: usize,
    ) {
        *self = Self {
            contract,
            verification_baseline,
            review_baseline,
            ..Self::default()
        };
    }

    fn merge_steering(
        &mut self,
        prompt: &str,
        verification_baseline: usize,
        review_baseline: usize,
    ) {
        let inferred = TaskCompletionContract::inferred(prompt);
        if supersedes_goal(prompt) {
            let superseded_goals = self.superseded_goals.saturating_add(1);
            self.begin(inferred, verification_baseline, review_baseline);
            self.superseded_goals = superseded_goals;
        } else {
            self.contract = self.contract.merge(inferred);
            self.continuation_used = false;
            self.attempts.clear();
            self.disposition = None;
        }
    }

    fn trace(&self) -> Option<CompletionGuardTrace> {
        self.disposition.map(|disposition| CompletionGuardTrace {
            contract: self.contract,
            superseded_goals: self.superseded_goals,
            attempts: self.attempts.clone(),
            disposition,
        })
    }
}

impl Agent {
    pub(super) fn tool_registry_for_current_task(&self) -> Arc<ToolRegistry> {
        self.tool_registry.clone()
    }

    pub(super) fn begin_inferred_completion_task(&mut self, prompt: &str) -> Option<String> {
        let prompt_kind = TaskPromptKind::classify(prompt);
        let continuation_goal = prompt_kind
            .is_continuation()
            .then(|| self.latest_explicit_human_goal())
            .flatten();
        let mut contract = if self.execution_lane.kind == ExecutionLaneKind::Parent {
            if prompt_kind.is_continuation() {
                inherited_continuation_contract(
                    self.completion.contract,
                    continuation_goal.as_deref(),
                )
            } else {
                TaskCompletionContract::inferred(prompt)
            }
        } else {
            TaskCompletionContract::informational()
        };
        if self.execution_lane.kind == ExecutionLaneKind::Parent && prompt_kind.requests_action() {
            contract = contract.merge(TaskCompletionContract::action());
        }
        self.begin_completion_task(contract);
        continuation_goal
    }

    fn latest_explicit_human_goal(&self) -> Option<String> {
        self.messages.iter().rev().find_map(|message| {
            let ChatCompletionRequestMessage::User(user) = message else {
                return None;
            };
            if user.name.is_some() {
                return None;
            }
            let text = try_message_content_string(message)?;
            let text = text.trim();
            (!text.is_empty() && !TaskPromptKind::classify(text).is_continuation())
                .then(|| one_line_preview(text, 512))
        })
    }

    pub(super) fn begin_completion_task(&mut self, contract: TaskCompletionContract) {
        self.completion.begin(
            contract,
            self.verification.verification_runs.len(),
            self.self_review_runs.len(),
        );
        self.read_only_task_progress.reset();
        self.finalization.begin_task();
    }

    pub(super) fn retry_completion_task(&mut self) {
        self.completion.continuation_used = false;
        self.completion.attempts.clear();
        self.completion.disposition = None;
        self.finalization.begin_task();
    }

    pub(super) fn merge_completion_steering(&mut self, prompt: &str) {
        self.completion.merge_steering(
            prompt,
            self.verification.verification_runs.len(),
            self.self_review_runs.len(),
        );
        self.read_only_task_progress.reset();
        // Queued human steering is explicit authority for more work. It must
        // reopen finalization so a prior green gate blocks only unsolicited
        // model activity, never a newly requested review or verification.
        self.finalization.begin_task();
    }

    pub(super) fn read_only_conclusion_nudge(
        &mut self,
        tool_calls: &[ToolCall],
    ) -> Option<&'static str> {
        self.completion
            .contract
            .is_read_only_task()
            .then(|| {
                self.read_only_task_progress
                    .observe_inspection_turn(tool_calls)
            })
            .flatten()
    }

    pub(super) fn take_read_only_conclusion_turn(&mut self) -> bool {
        self.read_only_task_progress.take_conclusion_turn()
    }

    pub(super) fn note_completion_action(&mut self) {
        self.completion.action_observed = true;
    }

    pub(super) fn note_completion_workspace_mutation(&mut self) {
        self.completion.action_observed = true;
        self.completion.workspace_mutated = true;
        self.finalization.note_workspace_change();
    }

    pub(super) fn note_completion_pending_work_started(&mut self) {
        self.completion.action_observed = true;
        self.completion.pending_work_started = true;
    }

    pub(crate) fn completion_guard_trace(&self) -> Option<CompletionGuardTrace> {
        self.completion.trace()
    }

    pub(super) async fn completion_guard_verdict(
        &mut self,
        response: &str,
    ) -> CompletionGuardVerdict {
        self.revalidate_verification_for_delivery(self.completion.verification_baseline)
            .await;
        let delivered_gate_became_stale = self
            .verification
            .verification_runs
            .get(self.completion.verification_baseline..)
            .and_then(|runs| runs.last())
            .is_some_and(|run| run.status == crate::verification::VerificationRunStatus::Stale);
        if delivered_gate_became_stale {
            // Keep the finalization phase aligned with the typed delivery
            // binding. This also covers headless runs and repository-state
            // changes that do not surface through the interactive volatile
            // context refresh.
            self.note_external_finalization_workspace_change();
        }
        let signals = response_signals(response);
        let evidence = self.completion_decision_evidence(signals).await;
        let gaps = completion_gaps(&evidence);
        let ordinal = self.completion.attempts.len().saturating_add(1);
        self.completion.attempts.push(CompletionGuardAttempt {
            ordinal,
            evidence,
            response_signals: signals,
            gaps: gaps.clone(),
        });

        if signals.explicit_cancellation {
            return self.reject_reported_completion(
                CompletionFailureOutcome::Cancelled,
                gaps,
                "Completion cancelled: the assistant explicitly reported cancellation.",
            );
        }
        if signals.explicit_blocker {
            return self.reject_reported_completion(
                CompletionFailureOutcome::Blocked,
                gaps,
                "Completion blocked: the assistant explicitly reported an unresolved blocker.",
            );
        }
        if gaps.is_empty() {
            self.completion.disposition = Some(CompletionGuardDisposition::Accepted);
            return CompletionGuardVerdict::Accept;
        }
        if !self.completion.continuation_used {
            self.completion.continuation_used = true;
            self.completion.disposition = Some(CompletionGuardDisposition::Continued);
            let detail = gaps
                .iter()
                .map(CompletionGap::label)
                .collect::<Vec<_>>()
                .join("; ");
            return CompletionGuardVerdict::Continue {
                note: format!(
                    "Completion gate: structured work remains ({detail}). Continue now in this same run. Resolve the listed state with tools or explicit typed skip/block evidence, update every todo to completed/cancelled, and then give the final answer. This is the only automatic completion retry."
                ),
            };
        }

        let outcome = if gaps.iter().any(CompletionGap::is_external_blocker) {
            CompletionFailureOutcome::Blocked
        } else {
            CompletionFailureOutcome::Failed
        };
        self.reject_completion(outcome, gaps)
    }

    fn reject_completion(
        &mut self,
        outcome: CompletionFailureOutcome,
        gaps: Vec<CompletionGap>,
    ) -> CompletionGuardVerdict {
        let detail = format!(
            "Completion rejected: {}.",
            gaps.iter()
                .map(CompletionGap::label)
                .collect::<Vec<_>>()
                .join("; ")
        );
        self.reject_reported_completion(outcome, gaps, &detail)
    }

    fn reject_reported_completion(
        &mut self,
        outcome: CompletionFailureOutcome,
        gaps: Vec<CompletionGap>,
        detail: &str,
    ) -> CompletionGuardVerdict {
        self.completion.disposition = Some(match outcome {
            CompletionFailureOutcome::Blocked => CompletionGuardDisposition::Blocked,
            CompletionFailureOutcome::Failed => CompletionGuardDisposition::Failed,
            CompletionFailureOutcome::Cancelled => CompletionGuardDisposition::Cancelled,
        });
        CompletionGuardVerdict::Reject(CompletionGuardFailure {
            outcome,
            gaps,
            detail: detail.to_string(),
        })
    }

    async fn completion_decision_evidence(
        &self,
        signals: CompletionResponseSignals,
    ) -> CompletionDecisionEvidence {
        let todos = if let Some(store) = &self.todo_store {
            let store = store.lock().await;
            store
                .todos()
                .iter()
                .fold(CompletionTodoEvidence::default(), |mut evidence, todo| {
                    match todo.status {
                        crate::todo::TodoStatus::Pending => evidence.pending += 1,
                        crate::todo::TodoStatus::InProgress => evidence.in_progress += 1,
                        crate::todo::TodoStatus::Completed => evidence.completed += 1,
                        crate::todo::TodoStatus::Cancelled => evidence.cancelled += 1,
                    }
                    evidence
                })
        } else {
            CompletionTodoEvidence::default()
        };
        let interactions = match &self.interaction {
            Some(interaction) => interaction.pending_count().await,
            None => 0,
        };
        let (background_running, terminal_running) = tokio::join!(
            self.background_tasks.has_running(),
            self.terminals.has_running()
        );
        let subagents = usize::from(
            self.subagent_runner
                .as_ref()
                .is_some_and(|runner| runner.subagents().has_running()),
        );
        CompletionDecisionEvidence {
            contract: self.completion.contract,
            todos,
            action_observed: self.completion.action_observed,
            workspace_mutated: self.completion.workspace_mutated,
            mutation_declared_unnecessary: signals.mutation_unnecessary,
            pending_work_started: self.completion.pending_work_started,
            verification: self.completion_verification_evidence(),
            review: self.completion_review_evidence(),
            pending: CompletionPendingEvidence {
                interactions,
                subagents,
                background_tasks: usize::from(background_running),
                terminals: usize::from(terminal_running),
                // Peer/background waits return `AgentRunResult::Waiting` before
                // this gate. Keep an explicit typed slot so future wait kinds
                // cannot be accidentally omitted from the decision contract.
                external_waits: 0,
            },
        }
    }

    fn completion_verification_evidence(&self) -> CompletionCheckEvidence {
        let required =
            self.completion.contract.verification_required || self.completion.workspace_mutated;
        if !required {
            return CompletionCheckEvidence::NotRequired;
        }
        let Some(run) = self
            .verification
            .verification_runs
            .get(self.completion.verification_baseline..)
            .and_then(|runs| runs.last())
        else {
            return CompletionCheckEvidence::Missing;
        };
        use crate::verification::{VerificationBinding, VerificationRunStatus};
        let bindings_fresh = !run.checks.is_empty()
            && run.checks.iter().all(|check| {
                matches!(
                    (check.binding.as_ref(), check.delivered_binding.as_ref()),
                    (
                        Some(VerificationBinding::Bound { digest: before, .. }),
                        Some(VerificationBinding::Bound { digest: delivered, .. })
                    ) if before == delivered
                )
            });
        match run.status {
            VerificationRunStatus::Passed | VerificationRunStatus::Unstable
                if run.observed_final_workspace == Some(true) && bindings_fresh =>
            {
                CompletionCheckEvidence::Satisfied
            }
            VerificationRunStatus::Passed
            | VerificationRunStatus::Unstable
            | VerificationRunStatus::Stale => CompletionCheckEvidence::Stale,
            VerificationRunStatus::Incomplete | VerificationRunStatus::Interrupted
                if run.terminal_reason_kind.is_some_and(|reason| {
                    matches!(
                        reason,
                        crate::verification::VerificationTerminalReason::Irrelevant
                            | crate::verification::VerificationTerminalReason::PolicyDisabled
                            | crate::verification::VerificationTerminalReason::UserSkipped
                            | crate::verification::VerificationTerminalReason::Delegated
                    )
                }) =>
            {
                CompletionCheckEvidence::Skipped(run.terminal_reason_kind.map_or_else(
                    || "typed_skip".to_string(),
                    |reason| reason.label().to_string(),
                ))
            }
            VerificationRunStatus::Failed | VerificationRunStatus::Blocked => {
                CompletionCheckEvidence::Blocked(run.terminal_reason_kind.map_or_else(
                    || run.status.label().to_string(),
                    |reason| reason.label().to_string(),
                ))
            }
            VerificationRunStatus::Running
            | VerificationRunStatus::Incomplete
            | VerificationRunStatus::Interrupted => CompletionCheckEvidence::Missing,
        }
    }

    fn completion_review_evidence(&self) -> CompletionCheckEvidence {
        if !self.completion.workspace_mutated {
            return CompletionCheckEvidence::NotRequired;
        }
        let Some(review) = self
            .self_review_runs
            .get(self.completion.review_baseline..)
            .and_then(|runs| runs.last())
        else {
            return CompletionCheckEvidence::Skipped(format!(
                "policy_or_eligibility:{}",
                self.self_review.mode().describe(self.approval_level())
            ));
        };
        match (review.status, review.disposition) {
            (_, Some(_)) => CompletionCheckEvidence::Satisfied,
            (SelfReviewRunStatus::Running | SelfReviewRunStatus::Succeeded, None) => {
                CompletionCheckEvidence::Missing
            }
            (status, None) => CompletionCheckEvidence::Blocked(status.label().to_string()),
        }
    }
}

fn completion_gaps(evidence: &CompletionDecisionEvidence) -> Vec<CompletionGap> {
    let mut gaps = Vec::new();
    if evidence.todos.pending > 0 {
        gaps.push(CompletionGap::PendingTodos(evidence.todos.pending));
    }
    if evidence.todos.in_progress > 0 {
        gaps.push(CompletionGap::InProgressTodos(evidence.todos.in_progress));
    }
    match evidence.contract.effect {
        CompletionEffectRequirement::None => {}
        CompletionEffectRequirement::AnyAction if !evidence.action_observed => {
            gaps.push(CompletionGap::RequestedActionMissing);
        }
        CompletionEffectRequirement::WorkspaceMutation
            if !evidence.workspace_mutated && !evidence.mutation_declared_unnecessary =>
        {
            gaps.push(CompletionGap::WorkspaceMutationMissing);
        }
        CompletionEffectRequirement::AnyAction | CompletionEffectRequirement::WorkspaceMutation => {
        }
    }
    push_check_gap(&mut gaps, &evidence.verification, true);
    push_check_gap(&mut gaps, &evidence.review, false);
    let structured_work = evidence.contract.goal_kind == CompletionGoalKind::Action
        || evidence.contract.intent.requires_action()
        || evidence.pending_work_started
        || evidence.todos.pending > 0
        || evidence.todos.in_progress > 0;
    if structured_work {
        if evidence.pending.interactions > 0 {
            gaps.push(CompletionGap::InteractionPending(
                evidence.pending.interactions,
            ));
        }
        if evidence.pending.subagents > 0 {
            gaps.push(CompletionGap::SubagentPending(evidence.pending.subagents));
        }
        if evidence.pending.background_tasks > 0 {
            gaps.push(CompletionGap::BackgroundTaskPending(
                evidence.pending.background_tasks,
            ));
        }
        if evidence.pending.terminals > 0 {
            gaps.push(CompletionGap::TerminalPending(evidence.pending.terminals));
        }
        if evidence.pending.external_waits > 0 {
            gaps.push(CompletionGap::ExternalWaitPending(
                evidence.pending.external_waits,
            ));
        }
    }
    gaps
}

fn push_check_gap(
    gaps: &mut Vec<CompletionGap>,
    evidence: &CompletionCheckEvidence,
    verification: bool,
) {
    match evidence {
        CompletionCheckEvidence::NotRequired
        | CompletionCheckEvidence::Satisfied
        | CompletionCheckEvidence::Skipped(_) => {}
        CompletionCheckEvidence::Missing if verification => {
            gaps.push(CompletionGap::VerificationMissing)
        }
        CompletionCheckEvidence::Missing => gaps.push(CompletionGap::ReviewMissing),
        CompletionCheckEvidence::Stale if verification => {
            gaps.push(CompletionGap::VerificationStale)
        }
        CompletionCheckEvidence::Stale => gaps.push(CompletionGap::ReviewStale),
        CompletionCheckEvidence::Blocked(reason) if verification => {
            gaps.push(CompletionGap::VerificationBlocked(reason.clone()))
        }
        CompletionCheckEvidence::Blocked(reason) => {
            gaps.push(CompletionGap::ReviewBlocked(reason.clone()))
        }
    }
}

fn response_signals(response: &str) -> CompletionResponseSignals {
    let normalized = normalized_words(response);
    CompletionResponseSignals {
        promises_future_action: contains_any_phrase(
            &normalized,
            &[
                "i will",
                "i ll",
                "next i",
                "i need to",
                "i can now",
                "then i",
                "zaraz zrobie",
                "nastepnie",
                "musze jeszcze",
            ],
        ),
        explicit_blocker: normalized == "blocked"
            || normalized.starts_with("blocked ")
            || contains_any_phrase(
                &normalized,
                &[
                    "i am blocked",
                    "i m blocked",
                    "cannot proceed",
                    "can t proceed",
                    "unable to proceed",
                    "need your input",
                    "requires user input",
                    "permission denied",
                    "jestem zablokowany",
                    "nie moge kontynuowac",
                    "nie mogę kontynuować",
                    "potrzebuje decyzji",
                    "potrzebuję decyzji",
                ],
            ),
        explicit_cancellation: contains_any_phrase(
            &normalized,
            &[
                "task canceled",
                "task cancelled",
                "request canceled",
                "request cancelled",
                "zadanie anulowane",
            ],
        ),
        mutation_unnecessary: contains_any_phrase(
            &normalized,
            &[
                "no change was needed",
                "no changes were needed",
                "no code change is needed",
                "nothing to change",
                "already implemented",
                "already correct",
                "zmiany nie sa potrzebne",
                "zmiany nie są potrzebne",
                "nie trzeba nic zmieniac",
                "nie trzeba nic zmieniać",
                "juz zaimplementowane",
                "już zaimplementowane",
            ],
        ),
    }
}

fn inherited_continuation_contract(
    current: TaskCompletionContract,
    previous_goal: Option<&str>,
) -> TaskCompletionContract {
    if current != TaskCompletionContract::informational() {
        return current;
    }
    previous_goal
        .map(TaskCompletionContract::inferred)
        .unwrap_or_else(TaskCompletionContract::action)
}

fn contains_directive_clause(text: &str, commands: &[&str]) -> bool {
    if starts_with_command(text, commands) {
        return true;
    }
    [
        " and ",
        " then ",
        " also ",
        " oraz ",
        " potem ",
        " następnie ",
    ]
    .iter()
    .any(|connector| {
        let mut remainder = text;
        while let Some((_, tail)) = remainder.split_once(connector) {
            if starts_with_command(tail, commands) {
                return true;
            }
            remainder = tail;
        }
        false
    })
}

fn starts_with_command(text: &str, commands: &[&str]) -> bool {
    commands.iter().any(|command| {
        text == *command
            || text
                .strip_prefix(command)
                .is_some_and(|remainder| remainder.starts_with(' '))
    })
}

fn contains_any_phrase(text: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| text.contains(phrase))
}

fn supersedes_goal(prompt: &str) -> bool {
    let normalized = normalized_words(prompt);
    let directive = action_directive(&normalized);
    starts_with_command(
        directive,
        &[
            "instead do",
            "do this instead",
            "new goal",
            "stop that",
            "actually do",
            "scratch that",
            "forget that instead",
            "forget that do",
            "do not do that",
            "don t do that",
            "zamiast tego",
            "nowy cel",
            "przestan robic",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(contract: TaskCompletionContract) -> CompletionDecisionEvidence {
        CompletionDecisionEvidence {
            contract,
            todos: CompletionTodoEvidence::default(),
            action_observed: false,
            workspace_mutated: false,
            mutation_declared_unnecessary: false,
            pending_work_started: false,
            verification: CompletionCheckEvidence::NotRequired,
            review: CompletionCheckEvidence::NotRequired,
            pending: CompletionPendingEvidence::default(),
        }
    }

    #[test]
    fn informational_answer_without_structured_work_is_complete() {
        assert!(completion_gaps(&evidence(TaskCompletionContract::informational())).is_empty());
    }

    #[test]
    fn bare_continue_infers_action_authority() {
        assert_eq!(
            TaskCompletionContract::inferred("continue"),
            TaskCompletionContract::action()
        );
    }

    #[test]
    fn continuation_inherits_the_active_workspace_contract() {
        let contract = inherited_continuation_contract(
            TaskCompletionContract::workspace_action(),
            Some("Explain the API"),
        );

        assert_eq!(contract, TaskCompletionContract::workspace_action());
    }

    #[test]
    fn restored_continuation_recovers_contract_from_prior_goal() {
        let contract = inherited_continuation_contract(
            TaskCompletionContract::informational(),
            Some("Fix the parser and run tests"),
        );

        assert_eq!(contract.intent, TaskIntent::Mutation);
        assert_eq!(
            contract.effect,
            CompletionEffectRequirement::WorkspaceMutation
        );
        assert!(contract.verification_required);
    }

    #[test]
    fn pending_and_in_progress_todos_prevent_success() {
        let mut evidence = evidence(TaskCompletionContract::workspace_action());
        evidence.todos.pending = 2;
        evidence.todos.in_progress = 1;
        evidence.workspace_mutated = true;

        assert_eq!(
            completion_gaps(&evidence),
            vec![
                CompletionGap::PendingTodos(2),
                CompletionGap::InProgressTodos(1)
            ]
        );
    }

    #[test]
    fn future_tense_prose_never_supplies_missing_action_evidence() {
        let evidence = evidence(TaskCompletionContract::workspace_action());
        let signals = response_signals("I will implement it next.");

        assert!(signals.promises_future_action);
        assert_eq!(
            completion_gaps(&evidence),
            vec![CompletionGap::WorkspaceMutationMissing]
        );
    }

    #[test]
    fn explicit_no_change_disposition_satisfies_mutation_contract() {
        let mut evidence = evidence(TaskCompletionContract::workspace_action());
        evidence.mutation_declared_unnecessary =
            response_signals("No change was needed; it is already implemented.")
                .mutation_unnecessary;

        assert!(completion_gaps(&evidence).is_empty());
    }

    #[test]
    fn stale_or_failed_verification_prevents_success() {
        let mut stale = evidence(TaskCompletionContract::verification_action());
        stale.action_observed = true;
        stale.verification = CompletionCheckEvidence::Stale;
        assert_eq!(
            completion_gaps(&stale),
            vec![CompletionGap::VerificationStale]
        );

        stale.verification = CompletionCheckEvidence::Blocked("tests_failed".to_string());
        assert_eq!(
            completion_gaps(&stale),
            vec![CompletionGap::VerificationBlocked(
                "tests_failed".to_string()
            )]
        );
    }

    #[test]
    fn typed_verification_skip_is_terminal_evidence() {
        let mut evidence = evidence(TaskCompletionContract::verification_action());
        evidence.action_observed = true;
        evidence.verification = CompletionCheckEvidence::Skipped("user_skipped".to_string());
        assert!(completion_gaps(&evidence).is_empty());
    }

    #[test]
    fn running_children_and_background_work_prevent_success() {
        let mut evidence = evidence(TaskCompletionContract::action());
        evidence.action_observed = true;
        evidence.pending.subagents = 1;
        evidence.pending.background_tasks = 1;
        evidence.pending.terminals = 1;
        assert_eq!(completion_gaps(&evidence).len(), 3);
    }

    #[test]
    fn preexisting_runtime_work_is_ignored_until_this_task_starts_work() {
        let mut evidence = evidence(TaskCompletionContract::informational());
        evidence.pending.background_tasks = 1;
        assert!(completion_gaps(&evidence).is_empty());

        evidence.pending_work_started = true;
        assert_eq!(
            completion_gaps(&evidence),
            vec![CompletionGap::BackgroundTaskPending(1)]
        );
    }

    #[test]
    fn read_only_progress_bounds_inspection_and_resets_for_new_tasks() {
        let inspection = || ToolCall {
            id: "read".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"src/lib.rs"}"#.to_string(),
        };
        let mut progress = ReadOnlyTaskProgress::default();

        for _ in 0..7 {
            assert!(progress.observe_inspection_turn(&[inspection()]).is_none());
        }
        assert!(progress.observe_inspection_turn(&[inspection()]).is_some());
        assert!(progress.take_conclusion_turn());
        assert!(!progress.take_conclusion_turn());
        assert!(progress.observe_inspection_turn(&[inspection()]).is_none());

        progress.reset();
        assert!(progress.observe_inspection_turn(&[inspection()]).is_none());
    }

    #[test]
    fn inference_separates_semantic_intent_from_required_effects() {
        assert_eq!(
            TaskCompletionContract::inferred("Explain how this module works"),
            TaskCompletionContract::informational()
        );
        assert_eq!(
            TaskCompletionContract::inferred("Fix the parser bug"),
            TaskCompletionContract::workspace_action()
        );
        assert_eq!(
            TaskCompletionContract::inferred("Run the tests"),
            TaskCompletionContract::verification_action()
        );
        assert_eq!(
            TaskCompletionContract::inferred("NO to zacznij 139"),
            TaskCompletionContract::action()
        );
        assert_eq!(
            TaskCompletionContract::inferred("Finish topic A and wait for the next request."),
            TaskCompletionContract::workspace_action()
        );
        assert_eq!(
            TaskCompletionContract::inferred(
                "Carry the release qualification through context pressure and finish README.md."
            ),
            TaskCompletionContract::workspace_action()
        );
        let diagnosis = TaskCompletionContract::inferred("Check the parser state");
        assert_eq!(diagnosis.intent, TaskIntent::Diagnosis);
        assert_eq!(diagnosis.effect, CompletionEffectRequirement::AnyAction);
        assert!(!diagnosis.verification_required);

        let review = TaskCompletionContract::inferred(
            "Review the parser module, but do not modify files or run commands.",
        );
        assert_eq!(review.intent, TaskIntent::Review);
        assert_eq!(review.effect, CompletionEffectRequirement::AnyAction);
        assert!(review.is_read_only_task());

        for prompt in [
            "Apply this patch",
            "Address the review findings",
            "Resolve the issue",
            "Upgrade the dependency",
            "Bump serde",
        ] {
            assert_eq!(
                TaskCompletionContract::inferred(prompt).intent,
                TaskIntent::Mutation,
                "{prompt}"
            );
        }

        let monitoring = TaskCompletionContract::inferred("Monitor the deployment");
        assert_eq!(monitoring.intent, TaskIntent::Monitoring);
        assert_eq!(monitoring.effect, CompletionEffectRequirement::AnyAction);

        let execution = TaskCompletionContract::inferred("Run pwd and report the output");
        assert_eq!(execution.intent, TaskIntent::Mutation);
        assert_eq!(execution.effect, CompletionEffectRequirement::AnyAction);
        assert!(!execution.verification_required);
    }

    #[test]
    fn compound_requests_keep_mutation_intent_and_verification_evidence() {
        let contract = TaskCompletionContract::inferred(
            "Analyze why the parser fails, then fix it and run tests",
        );

        assert_eq!(contract.intent, TaskIntent::Mutation);
        assert_eq!(
            contract.effect,
            CompletionEffectRequirement::WorkspaceMutation
        );
        assert!(contract.verification_required);

        let build = TaskCompletionContract::inferred("Build the requested integration");
        assert_eq!(build.intent, TaskIntent::Mutation);
        assert_eq!(build.effect, CompletionEffectRequirement::AnyAction);
        assert!(build.verification_required);
    }

    #[test]
    fn steering_merges_intent_without_deriving_it_from_effect_strength() {
        let merged = TaskCompletionContract::workspace_action()
            .merge(TaskCompletionContract::verification_action());

        assert_eq!(merged.intent, TaskIntent::Mutation);
        assert_eq!(
            merged.effect,
            CompletionEffectRequirement::WorkspaceMutation
        );
        assert!(merged.verification_required);
    }

    #[test]
    fn legacy_completion_contracts_default_the_new_intent_field() {
        let contract: TaskCompletionContract = serde_json::from_value(serde_json::json!({
            "goal_kind": "action",
            "effect": "any_action",
            "verification_required": false
        }))
        .unwrap();

        assert_eq!(contract.intent, TaskIntent::Informational);
        assert_eq!(contract.goal_kind, CompletionGoalKind::Action);
    }

    #[test]
    fn additive_and_status_steering_preserve_active_work_and_evidence() {
        let mut state = CompletionGuardState::default();
        state.begin(TaskCompletionContract::workspace_action(), 2, 3);
        state.action_observed = true;
        state.workspace_mutated = true;

        state.merge_steering(
            "Don't forget that we also need docs, and run the tests",
            4,
            5,
        );
        assert_eq!(state.contract.intent, TaskIntent::Mutation);
        assert_eq!(
            state.contract.effect,
            CompletionEffectRequirement::WorkspaceMutation
        );
        assert!(state.contract.verification_required);
        assert!(state.action_observed);
        assert!(state.workspace_mutated);

        state.merge_steering("What is the status?", 6, 7);
        assert_eq!(state.contract.intent, TaskIntent::Mutation);
        assert!(state.contract.verification_required);
        assert!(state.action_observed);
        assert!(state.workspace_mutated);
        assert_eq!(state.superseded_goals, 0);
    }

    #[test]
    fn contradictory_steering_replaces_the_prior_contract() {
        let mut state = CompletionGuardState::default();
        state.begin(TaskCompletionContract::workspace_action(), 2, 3);
        state.pending_work_started = true;
        state.merge_steering("Do not do that; explain the API instead", 4, 5);

        assert_eq!(state.contract, TaskCompletionContract::informational());
        assert!(!state.pending_work_started);
        assert_eq!(state.superseded_goals, 1);
    }

    #[test]
    fn superseding_steering_replaces_the_prior_contract() {
        let mut state = CompletionGuardState::default();
        state.begin(TaskCompletionContract::workspace_action(), 2, 3);
        state.action_observed = true;
        state.merge_steering("Actually do explain the API instead", 4, 5);

        assert_eq!(state.contract, TaskCompletionContract::informational());
        assert!(!state.action_observed);
        assert_eq!(state.superseded_goals, 1);
    }
}
