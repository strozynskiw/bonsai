//! Structured completion gate for action-oriented agent runs.
//!
//! Model prose is deliberately only a supporting signal here. The terminal
//! decision is derived from todo, mutation, verification, review, and pending
//! runtime state that Bonsai observed itself.

use serde::{Deserialize, Serialize};

use super::*;

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
    pub(crate) goal_kind: CompletionGoalKind,
    pub(crate) effect: CompletionEffectRequirement,
    pub(crate) verification_required: bool,
}

impl Default for TaskCompletionContract {
    fn default() -> Self {
        Self::informational()
    }
}

impl TaskCompletionContract {
    pub(crate) const fn informational() -> Self {
        Self {
            goal_kind: CompletionGoalKind::Informational,
            effect: CompletionEffectRequirement::None,
            verification_required: false,
        }
    }

    pub(crate) const fn workspace_action() -> Self {
        Self {
            goal_kind: CompletionGoalKind::Action,
            effect: CompletionEffectRequirement::WorkspaceMutation,
            verification_required: false,
        }
    }

    pub(crate) const fn action() -> Self {
        Self {
            goal_kind: CompletionGoalKind::Action,
            effect: CompletionEffectRequirement::AnyAction,
            verification_required: false,
        }
    }

    pub(crate) const fn verification_action() -> Self {
        Self {
            goal_kind: CompletionGoalKind::Action,
            effect: CompletionEffectRequirement::AnyAction,
            verification_required: true,
        }
    }

    fn inferred(prompt: &str) -> Self {
        let normalized = normalized_words(prompt);
        let directive = action_directive(&normalized);
        if directive.starts_with("run tests")
            || directive.starts_with("run the tests")
            || directive.starts_with("run checks")
            || directive.starts_with("test ")
            || directive == "test"
            || directive.starts_with("verify ")
            || directive == "verify"
            || directive.starts_with("build ")
            || directive == "build"
            || contains_any_phrase(
                directive,
                &["sprawdz testy", "sprawdź testy", "uruchom testy"],
            )
        {
            return Self::verification_action();
        }
        if first_word_is(
            directive,
            &[
                "fix",
                "implement",
                "add",
                "update",
                "change",
                "modify",
                "create",
                "delete",
                "remove",
                "rename",
                "refactor",
                "write",
                "edit",
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
        ) {
            return Self::workspace_action();
        }
        if first_word_is(
            directive,
            &[
                "close",
                "commit",
                "deploy",
                "merge",
                "move",
                "open",
                "publish",
                "push",
                "pozamykaj",
                "przenies",
                "przenieś",
                "zmerguj",
            ],
        ) || directive == "zacznij"
            || directive.starts_with("zacznij ")
            || directive.starts_with("work on ")
            || directive.starts_with("start working on")
            || directive.starts_with("continue working on")
            || directive.starts_with("zacznij pracowac nad")
            || directive.starts_with("zacznij pracować nad")
            || first_word_is(
                directive,
                &[
                    "analyze",
                    "audit",
                    "check",
                    "inspect",
                    "investigate",
                    "research",
                    "review",
                    "run",
                    "trace",
                    "sprawdz",
                    "sprawdź",
                    "przeanalizuj",
                    "przejrzyj",
                    "zbadaj",
                    "uruchom",
                ],
            )
        {
            return Self::action();
        }
        Self::informational()
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
            goal_kind: if effect == CompletionEffectRequirement::None {
                CompletionGoalKind::Informational
            } else {
                CompletionGoalKind::Action
            },
            effect,
            verification_required: self.verification_required || other.verification_required,
        }
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
    pub(super) fn begin_inferred_completion_task(&mut self, prompt: &str) {
        let contract = if self.execution_lane.kind == ExecutionLaneKind::Parent {
            TaskCompletionContract::inferred(prompt)
        } else {
            TaskCompletionContract::informational()
        };
        self.begin_completion_task(contract);
    }

    pub(super) fn begin_completion_task(&mut self, contract: TaskCompletionContract) {
        self.completion.begin(
            contract,
            self.verification.verification_runs.len(),
            self.self_review_runs.len(),
        );
    }

    pub(super) fn retry_completion_task(&mut self) {
        self.completion.continuation_used = false;
        self.completion.attempts.clear();
        self.completion.disposition = None;
    }

    pub(super) fn merge_completion_steering(&mut self, prompt: &str) {
        self.completion.merge_steering(
            prompt,
            self.verification.verification_runs.len(),
            self.self_review_runs.len(),
        );
    }

    pub(super) fn note_completion_action(&mut self) {
        self.completion.action_observed = true;
    }

    pub(super) fn note_completion_workspace_mutation(&mut self) {
        self.completion.action_observed = true;
        self.completion.workspace_mutated = true;
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

fn normalized_words(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    for character in text.to_lowercase().chars() {
        if character.is_alphanumeric() {
            normalized.push(character);
        } else {
            normalized.push(' ');
        }
    }
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn first_word_is(text: &str, words: &[&str]) -> bool {
    text.split_whitespace()
        .next()
        .is_some_and(|candidate| words.contains(&candidate))
}

fn contains_any_phrase(text: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| text.contains(phrase))
}

fn action_directive(normalized: &str) -> &str {
    let mut directive = normalized;
    loop {
        let mut stripped = None;
        for prefix in [
            "please ",
            "pls ",
            "can you ",
            "could you ",
            "would you ",
            "i need you to ",
            "we need to ",
            "lets ",
            "let us ",
            "ok ",
            "okay ",
            "no to ",
            "dobra ",
            "prosze ",
            "proszę ",
            "czy mozesz ",
            "czy możesz ",
        ] {
            if let Some(remainder) = directive.strip_prefix(prefix) {
                stripped = Some(remainder);
                break;
            }
        }
        match stripped {
            Some(remainder) => directive = remainder,
            None => return directive,
        }
    }
}

fn supersedes_goal(prompt: &str) -> bool {
    let normalized = normalized_words(prompt);
    contains_any_phrase(
        &normalized,
        &[
            "instead do",
            "do this instead",
            "new goal",
            "stop that",
            "actually do",
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
    fn inference_is_conservative_but_recognizes_mutation_and_verification() {
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
            TaskCompletionContract::inferred("Check the parser state"),
            TaskCompletionContract::action()
        );
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
