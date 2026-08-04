//! Observed-state completion evidence shared by TUI and headless surfaces.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::agent::UsageTotals;
use crate::diff::{DiffStatus, FileDiff};
use crate::output::ToolExecutionStatus;
use crate::run_budget::RunBudgetExhaustion;
use crate::self_review::{SelfReviewDisposition, SelfReviewRunRecord};
use crate::storage::AuthorizationDecisionRecord;
use crate::verification::{VerificationRunRecord, VerificationRunStatus};

const MAX_INTENT_CHARS: usize = 240;
const MAX_RENDERED_FILES: usize = 5;

/// Terminal state described by a completion report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionStatus {
    Completed,
    Failed,
    Interrupted,
    BudgetExhausted,
}

impl CompletionStatus {
    const fn headline(self) -> &'static str {
        match self {
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Interrupted => "Interrupted",
            Self::BudgetExhausted => "Budget exhausted",
        }
    }

    const fn caveat(self) -> Option<&'static str> {
        match self {
            Self::Completed => None,
            Self::Failed => Some("Run finished with unresolved tool or verification failures."),
            Self::Interrupted => Some("Run was interrupted; partial work may remain."),
            Self::BudgetExhausted => {
                Some("Run stopped at a configured or provider-enforced limit.")
            }
        }
    }
}

/// One file mutation observed from a successful structured diff result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionFileChange {
    pub(crate) path: String,
    pub(crate) status: DiffStatus,
    pub(crate) intent: String,
    pub(crate) added_lines: usize,
    pub(crate) removed_lines: usize,
}

/// Authorization evidence summarized without exposing command arguments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionAuthorization {
    pub(crate) decisions: usize,
    pub(crate) allowed: usize,
    pub(crate) denied: usize,
    pub(crate) user_approved: usize,
    pub(crate) policy_bypasses: usize,
}

/// Cumulative provider usage observed for the resumable session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionUsage {
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cost_micros: Option<u64>,
    pub(crate) session_budget: crate::run_budget::SessionBudgetUsage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) budget_exhaustion: Option<RunBudgetExhaustion>,
}

/// Delivery-time freshness of the verification evidence shown in a report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CompletionVerificationFreshness {
    Fresh,
    Stale,
    Skipped,
    Blocked,
}

/// Structured summary for consumers that must not infer freshness from prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompletionVerificationEvidence {
    pub(crate) freshness: CompletionVerificationFreshness,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workspace_binding: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
}

/// Compact terminal recap derived only from tool, verification, review,
/// authorization, and usage evidence Bonsai directly observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionReport {
    pub(crate) status: CompletionStatus,
    pub(crate) changed_files: Vec<CompletionFileChange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) verification: Option<VerificationRunRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) verification_evidence: Option<CompletionVerificationEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) review: Option<SelfReviewRunRecord>,
    pub(crate) authorization: CompletionAuthorization,
    pub(crate) usage: CompletionUsage,
    pub(crate) caveats: Vec<String>,
}

/// Session-level ledgers folded into one terminal report construction.
#[derive(Debug)]
pub(crate) struct CompletionSessionEvidence<'a> {
    pub(crate) verification: Option<VerificationRunRecord>,
    pub(crate) review: Option<SelfReviewRunRecord>,
    pub(crate) authorization_decisions: &'a [AuthorizationDecisionRecord],
    pub(crate) usage: UsageTotals,
    pub(crate) session_budget: crate::run_budget::SessionBudgetUsage,
    pub(crate) budget_exhaustion: Option<RunBudgetExhaustion>,
}

fn completion_verification_evidence(
    run: Option<&VerificationRunRecord>,
) -> Option<CompletionVerificationEvidence> {
    let run = run?;
    let has_fresh_binding = !run.checks.is_empty()
        && run.checks.iter().all(|check| {
            matches!(
                (check.binding.as_ref(), check.delivered_binding.as_ref()),
                (
                    Some(crate::verification::VerificationBinding::Bound {
                        digest: recorded,
                        ..
                    }),
                    Some(crate::verification::VerificationBinding::Bound {
                        digest: delivered,
                        ..
                    })
                ) if recorded == delivered
            )
        });
    let freshness = match run.status {
        VerificationRunStatus::Passed | VerificationRunStatus::Unstable
            if run.observed_final_workspace == Some(true) && has_fresh_binding =>
        {
            CompletionVerificationFreshness::Fresh
        }
        VerificationRunStatus::Stale
        | VerificationRunStatus::Passed
        | VerificationRunStatus::Unstable => CompletionVerificationFreshness::Stale,
        VerificationRunStatus::Blocked => CompletionVerificationFreshness::Blocked,
        VerificationRunStatus::Incomplete | VerificationRunStatus::Interrupted => {
            CompletionVerificationFreshness::Skipped
        }
        VerificationRunStatus::Running | VerificationRunStatus::Failed => {
            CompletionVerificationFreshness::Blocked
        }
    };
    let workspace_binding = run
        .delivered_workspace_binding
        .as_ref()
        .or_else(|| run.checks.last().and_then(|check| check.binding.as_ref()))
        .and_then(|binding| match binding {
            crate::verification::VerificationBinding::Bound { digest, .. } => {
                Some(digest.chars().take(12).collect())
            }
            crate::verification::VerificationBinding::Blocked { .. } => None,
        });
    Some(CompletionVerificationEvidence {
        freshness,
        workspace_binding,
        reason: run
            .terminal_reason_kind
            .as_ref()
            .map(|reason| reason.label().to_string())
            .or_else(|| run.terminal_reason.clone()),
    })
}

impl CompletionReport {
    /// Build a report from canonical evidence captured by the current session.
    pub(crate) fn from_evidence(
        status: CompletionStatus,
        evidence: CompletionEvidenceSnapshot,
        session: CompletionSessionEvidence<'_>,
    ) -> Self {
        let authorization = summarize_authorization(
            session
                .authorization_decisions
                .iter()
                .filter(|record| record.created_at_ms >= evidence.started_at_ms),
        );
        let mut caveats = Vec::new();
        if let Some(caveat) = status.caveat() {
            caveats.push(caveat.to_string());
        }
        if evidence.unresolved_tool_failure {
            caveats.push("At least one tool operation remains failed or interrupted.".to_string());
        }
        match session.verification.as_ref() {
            None if !evidence.changed_files.is_empty() => {
                caveats.push("No verification run observed after file changes.".to_string());
            }
            Some(run) if run.status != VerificationRunStatus::Passed => caveats.push(format!(
                "Latest {} verification is {}.",
                run.kind.label(),
                run.status.label()
            )),
            Some(run) if run.observed_final_workspace != Some(true) => caveats.push(format!(
                "Latest {} verification did not observe the final workspace.",
                run.kind.label()
            )),
            _ => {}
        }
        if let Some(review) = session.review.as_ref()
            && review.findings.total() > 0
            && review.disposition != Some(SelfReviewDisposition::Fixed)
        {
            caveats.push(format!(
                "Self-review left {} finding(s) without a fixed disposition.",
                review.findings.total()
            ));
        }
        if authorization.denied > 0 {
            caveats.push(format!(
                "{} authorization decision(s) denied an action.",
                authorization.denied
            ));
        }
        if session.session_budget.cost_limit_micros.is_some()
            && session.session_budget.exact_cost_micros.is_none()
        {
            caveats.push(
                "Exact session cost is unavailable, so the configured cost budget could not be enforced."
                    .to_string(),
            );
        }

        let verification_evidence = completion_verification_evidence(session.verification.as_ref());
        Self {
            status,
            changed_files: evidence.changed_files,
            verification: session.verification,
            verification_evidence,
            review: session.review,
            authorization,
            usage: CompletionUsage {
                prompt_tokens: session.usage.prompt_tokens,
                completion_tokens: session.usage.completion_tokens,
                total_tokens: session
                    .usage
                    .prompt_tokens
                    .saturating_add(session.usage.completion_tokens),
                cost_micros: session.usage.cost_micros,
                session_budget: session.session_budget,
                budget_exhaustion: session.budget_exhaustion,
            },
            caveats,
        }
    }

    /// Render a bounded, scan-friendly form for terminal output.
    pub(crate) fn render_compact(&self) -> String {
        let added = self
            .changed_files
            .iter()
            .map(|change| change.added_lines)
            .sum::<usize>();
        let removed = self
            .changed_files
            .iter()
            .map(|change| change.removed_lines)
            .sum::<usize>();
        let mut outcome = self.status.headline().to_string();
        if !self.changed_files.is_empty() {
            let noun = if self.changed_files.len() == 1 {
                "file"
            } else {
                "files"
            };
            outcome.push_str(&format!(" · {} {noun} changed", self.changed_files.len()));
            if added > 0 || removed > 0 {
                outcome.push_str(&format!(" · +{added} -{removed}"));
            }
        }

        let mut lines = vec![outcome];
        if !self.changed_files.is_empty() {
            for change in self.changed_files.iter().take(MAX_RENDERED_FILES) {
                lines.push(format!("  {} · {}", change.path, change.intent));
            }
            if self.changed_files.len() > MAX_RENDERED_FILES {
                lines.push(format!(
                    "  … {} more file(s)",
                    self.changed_files.len() - MAX_RENDERED_FILES
                ));
            }
        }

        if let Some(run) = self.verification.as_ref() {
            let mut summary = format!(
                "Checks · {} {} · {}/{} passed",
                run.kind.label(),
                run.status.label(),
                run.checks
                    .iter()
                    .filter(|check| check.status
                        == crate::verification::VerificationCheckStatus::Passed)
                    .count(),
                run.checks.len()
            );
            if run.observed_final_workspace != Some(true) {
                summary.push_str(" · final workspace not observed");
            }
            if let Some(evidence) = self.verification_evidence.as_ref() {
                let freshness = match evidence.freshness {
                    CompletionVerificationFreshness::Fresh => "fresh",
                    CompletionVerificationFreshness::Stale => "stale",
                    CompletionVerificationFreshness::Skipped => "skipped",
                    CompletionVerificationFreshness::Blocked => "blocked",
                };
                summary.push_str(&format!(" · {freshness}"));
                if let Some(binding) = evidence.workspace_binding.as_deref() {
                    summary.push_str(&format!(" · workspace {binding}"));
                }
            }
            lines.push(summary);
        }
        if let Some(review) = self.review.as_ref() {
            let findings = review.findings.total();
            if findings == 0 {
                lines.push("Review · no findings".to_string());
            } else {
                let noun = if findings == 1 { "finding" } else { "findings" };
                lines.push(format!(
                    "Review · {findings} {noun} · {}",
                    review
                        .disposition
                        .map_or("no disposition", SelfReviewDisposition::label)
                ));
            }
        }
        if self.authorization.decisions > 0 {
            let mut decisions = Vec::new();
            if self.authorization.allowed > 0 {
                decisions.push(format!("{} allowed", self.authorization.allowed));
            }
            if self.authorization.user_approved > 0 {
                decisions.push(format!(
                    "{} user-approved",
                    self.authorization.user_approved
                ));
            }
            if self.authorization.policy_bypasses > 0 {
                decisions.push(format!("{} bypassed", self.authorization.policy_bypasses));
            }
            if self.authorization.denied > 0 {
                decisions.push(format!("{} denied", self.authorization.denied));
            }
            if decisions.is_empty() {
                decisions.push(format!("{} recorded", self.authorization.decisions));
            }
            lines.push(format!("Permissions · {}", decisions.join(" · ")));
        }
        if self.usage.total_tokens > 0 || self.usage.cost_micros.is_some() {
            lines.push(format!(
                "Usage · {} tokens{}",
                format_count(self.usage.total_tokens as u128),
                self.usage
                    .cost_micros
                    .map(|cost| format!(" · ${:.6}", cost as f64 / 1_000_000.0))
                    .unwrap_or_default()
            ));
        }
        if let Some(reason) = self.usage.budget_exhaustion {
            lines.push(format!("Budget · exhausted: {reason}"));
        } else if self.usage.session_budget.turn_limit.is_some()
            || self.usage.session_budget.output_char_limit.is_some()
            || self.usage.session_budget.active_limit_seconds.is_some()
            || self.usage.session_budget.cost_limit_micros.is_some()
        {
            let mut values = Vec::new();
            if self.usage.session_budget.turn_limit.is_some() {
                values.push(format!(
                    "turns {}",
                    format_budget_value(
                        self.usage.session_budget.turns,
                        self.usage.session_budget.turn_limit
                    )
                ));
            }
            if self.usage.session_budget.output_char_limit.is_some() {
                values.push(format!(
                    "output {}",
                    format_budget_value(
                        self.usage.session_budget.output_chars,
                        self.usage.session_budget.output_char_limit
                    )
                ));
            }
            if let Some(limit_seconds) = self.usage.session_budget.active_limit_seconds {
                values.push(format!(
                    "active time {}/{}s",
                    self.usage.session_budget.active_run_ms / 1_000,
                    limit_seconds
                ));
            }
            if let Some(limit_micros) = self.usage.session_budget.cost_limit_micros {
                let used = self
                    .usage
                    .session_budget
                    .exact_cost_micros
                    .map(|micros| format!("${:.6}", micros as f64 / 1_000_000.0))
                    .unwrap_or_else(|| "n/a".to_string());
                values.push(format!(
                    "cost {used}/${:.6}",
                    limit_micros as f64 / 1_000_000.0
                ));
            }
            lines.push(format!("Budget · session {}", values.join(" · ")));
        }
        let caveats = self
            .caveats
            .iter()
            .filter(|caveat| Some(caveat.as_str()) != self.status.caveat())
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !caveats.is_empty() {
            lines.push(format!("Caveats · {}", caveats.join(" ")));
        }
        lines.join("\n")
    }
}

/// Promote a nominally completed run to failed only on strong evidence: a
/// detected tool-call loop (the same invocation failing repeatedly) or a
/// verification run that actually failed. Non-completion terminal states
/// retain their more specific interruption or budget status.
///
/// DELIBERATE POLICY (do not re-tighten): ordinary one-off tool failures the
/// model recovers from by adapting — a failed `cargo fmt` fixed by editing the
/// file, a rejected patch retried through `edit`, one of bonsai's own
/// protective guards bouncing a call — previously counted as
/// `unresolved_tool_failure` because "resolved" required the *exact same*
/// invocation to be retried verbatim, which adaptive recovery almost never
/// does. That marked healthy sessions failed (observed live: all todos and
/// verification green, 105/116 calls succeeded, still `failed`) and, in phased
/// runs, cascaded into halting the plan without marking the finished phase
/// done. Tool noise now stays a report caveat; only loops and genuinely failed
/// verification demote the run. Non-`Failed` verification states
/// (stale/unstable/blocked) likewise stay caveats, not failures.
pub(crate) fn classify_completion_status(
    status: CompletionStatus,
    evidence: &CompletionEvidenceSnapshot,
    verification: Option<&VerificationRunRecord>,
) -> CompletionStatus {
    if status != CompletionStatus::Completed {
        return status;
    }
    let verification_failed =
        verification.is_some_and(|run| run.status == VerificationRunStatus::Failed);
    if evidence.repeated_tool_failure || verification_failed {
        CompletionStatus::Failed
    } else {
        CompletionStatus::Completed
    }
}

/// How many times one *identical* tool invocation (same tool, same normalized
/// arguments) must fail, without an intervening success of that invocation,
/// before the run is treated as looping rather than merely noisy. Distinct
/// adaptive failures — a failed command the model recovers from by editing a
/// file or trying a different command — never reach this threshold because
/// each variant is its own key.
const REPEATED_TOOL_FAILURE_LOOP_THRESHOLD: usize = 3;

/// Run-local evidence captured synchronously from output events.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CompletionEvidenceSnapshot {
    pub(crate) changed_files: Vec<CompletionFileChange>,
    pub(crate) unresolved_tool_failure: bool,
    /// A single identical invocation failed [`REPEATED_TOOL_FAILURE_LOOP_THRESHOLD`]+
    /// times in a row — the detected-loop signal that (unlike ordinary tool
    /// noise) still fails a nominally completed run.
    pub(crate) repeated_tool_failure: bool,
    pub(crate) failed_tool_attempts: usize,
    pub(crate) started_at_ms: i64,
}

#[derive(Debug, Default)]
struct CompletionEvidenceState {
    changed_files: BTreeMap<String, CompletionFileChange>,
    active_tools: HashMap<String, ActiveToolInvocation>,
    latest_tool_outcomes: BTreeMap<ToolInvocationKey, ToolInvocationOutcome>,
    /// Consecutive-failure streak per identical invocation key; a success of
    /// that key resets its streak (the loop was broken).
    failure_streaks: BTreeMap<ToolInvocationKey, usize>,
    next_tool_sequence: u64,
    failed_tool_attempts: usize,
    started_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ToolInvocationKey {
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveToolInvocation {
    key: ToolInvocationKey,
    sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ToolInvocationOutcome {
    sequence: u64,
    status: ToolExecutionStatus,
}

/// Thread-safe collector shared with an output sink and its presentation loop.
#[derive(Debug, Clone, Default)]
pub(crate) struct CompletionEvidenceCollector {
    inner: Arc<Mutex<CompletionEvidenceState>>,
}

impl CompletionEvidenceCollector {
    /// Clear run-local evidence immediately before dispatching a new agent run.
    pub(crate) fn begin_run(&self) {
        if let Ok(mut state) = self.inner.lock() {
            *state = CompletionEvidenceState {
                started_at_ms: crate::util::time::now_ms(),
                ..CompletionEvidenceState::default()
            };
        }
    }

    /// Register one tool invocation so a later rerun of the same operation can
    /// supersede its earlier terminal outcome.
    pub(crate) fn record_tool_started(&self, id: &str, name: &str, arguments: &str) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        let sequence = state.next_tool_sequence;
        state.next_tool_sequence = state.next_tool_sequence.saturating_add(1);
        state.active_tools.insert(
            id.to_string(),
            ActiveToolInvocation {
                key: ToolInvocationKey {
                    name: name.to_string(),
                    arguments: normalized_tool_arguments(name, arguments),
                },
                sequence,
            },
        );
    }

    /// Record the terminal outcome and optional structured diff of one tool.
    pub(crate) fn record_tool_result(
        &self,
        id: &str,
        result: &str,
        status: ToolExecutionStatus,
        diff: Option<&FileDiff>,
    ) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        let invocation = state.active_tools.remove(id).unwrap_or_else(|| {
            let sequence = state.next_tool_sequence;
            state.next_tool_sequence = state.next_tool_sequence.saturating_add(1);
            ActiveToolInvocation {
                key: ToolInvocationKey {
                    name: "unknown".to_string(),
                    arguments: id.to_string(),
                },
                sequence,
            }
        });
        if status.is_success() || status.is_failure() {
            if status.is_failure() {
                state.failed_tool_attempts = state.failed_tool_attempts.saturating_add(1);
                let streak = state
                    .failure_streaks
                    .entry(invocation.key.clone())
                    .or_insert(0);
                *streak = streak.saturating_add(1);
            } else {
                // A success of the same invocation breaks its failure loop.
                state.failure_streaks.remove(&invocation.key);
            }
            let outcome = ToolInvocationOutcome {
                sequence: invocation.sequence,
                status,
            };
            match state.latest_tool_outcomes.get_mut(&invocation.key) {
                Some(current) if current.sequence <= outcome.sequence => *current = outcome,
                None => {
                    state.latest_tool_outcomes.insert(invocation.key, outcome);
                }
                Some(_) => {}
            }
        }
        if !status.is_success() {
            return;
        }
        let Some(diff) = diff else {
            return;
        };
        let intent = compact_intent(result);
        for file in diff.files() {
            let path = crate::redact::redact(&file.path).into_owned();
            let entry =
                state
                    .changed_files
                    .entry(path.clone())
                    .or_insert_with(|| CompletionFileChange {
                        path,
                        status: file.status,
                        intent: intent.clone(),
                        added_lines: 0,
                        removed_lines: 0,
                    });
            entry.status = file.status;
            entry.intent.clone_from(&intent);
            entry.added_lines = entry.added_lines.saturating_add(file.added_lines);
            entry.removed_lines = entry.removed_lines.saturating_add(file.removed_lines);
        }
    }

    /// Record paths observed by a workspace snapshot when no structured diff
    /// was available (for example, a shell command that rewrote files).
    pub(crate) fn record_workspace_changes(&self, paths: &[String], intent: &str) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        let intent = compact_intent(intent);
        for path in paths {
            let path = crate::redact::redact(path).into_owned();
            state
                .changed_files
                .entry(path.clone())
                .or_insert_with(|| CompletionFileChange {
                    path,
                    status: DiffStatus::Modified,
                    intent: intent.clone(),
                    added_lines: 0,
                    removed_lines: 0,
                });
        }
    }

    /// Snapshot current run evidence without clearing it.
    pub(crate) fn snapshot(&self) -> CompletionEvidenceSnapshot {
        self.inner
            .lock()
            .map(|state| CompletionEvidenceSnapshot {
                changed_files: state.changed_files.values().cloned().collect(),
                unresolved_tool_failure: state
                    .latest_tool_outcomes
                    .values()
                    .any(|outcome| outcome.status.is_failure()),
                repeated_tool_failure: state
                    .failure_streaks
                    .values()
                    .any(|streak| *streak >= REPEATED_TOOL_FAILURE_LOOP_THRESHOLD),
                failed_tool_attempts: state.failed_tool_attempts,
                started_at_ms: state.started_at_ms,
            })
            .unwrap_or_else(|_| CompletionEvidenceSnapshot {
                changed_files: Vec::new(),
                unresolved_tool_failure: true,
                repeated_tool_failure: true,
                failed_tool_attempts: 1,
                started_at_ms: crate::util::time::now_ms(),
            })
    }
}

fn normalized_tool_arguments(name: &str, arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.trim().to_string();
    };
    if name == "bash"
        && let Some(command) = value.get("command").and_then(serde_json::Value::as_str)
    {
        return command.to_string();
    }
    serde_json::to_string(&value).unwrap_or_else(|_| arguments.trim().to_string())
}

fn compact_intent(result: &str) -> String {
    let redacted = crate::redact::redact(result);
    let normalized = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut compact = normalized
        .chars()
        .take(MAX_INTENT_CHARS)
        .collect::<String>();
    if normalized.chars().count() > MAX_INTENT_CHARS {
        compact.push('…');
    }
    if compact.is_empty() {
        "file updated".to_string()
    } else {
        compact
    }
}

fn format_budget_value(used: usize, limit: Option<usize>) -> String {
    limit.map_or_else(|| "off".to_string(), |limit| format!("{used}/{limit}"))
}

fn format_count(value: u128) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

fn summarize_authorization<'a>(
    decisions: impl IntoIterator<Item = &'a AuthorizationDecisionRecord>,
) -> CompletionAuthorization {
    let mut summary = CompletionAuthorization::default();
    for record in decisions {
        summary.decisions = summary.decisions.saturating_add(1);
        match record.decision.as_str() {
            "allow" => summary.allowed = summary.allowed.saturating_add(1),
            "deny" => summary.denied = summary.denied.saturating_add(1),
            _ => {}
        }
        if record.reason.starts_with("user allowed") {
            summary.user_approved = summary.user_approved.saturating_add(1);
        }
        if record.reason == "yolo bypass" {
            summary.policy_bypasses = summary.policy_bypasses.saturating_add(1);
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::build_file_diff;

    #[test]
    fn collector_merges_repeated_file_changes_and_keeps_latest_intent() {
        let collector = CompletionEvidenceCollector::default();
        collector.record_tool_started("call-1", "edit", r#"{"path":"a.rs","version":1}"#);
        collector.record_tool_result(
            "call-1",
            "create config",
            ToolExecutionStatus::Succeeded,
            Some(&build_file_diff("a.rs".to_string(), None, "one\n")),
        );
        collector.record_tool_started("call-2", "edit", r#"{"path":"a.rs","version":2}"#);
        collector.record_tool_result(
            "call-2",
            "refine config",
            ToolExecutionStatus::Succeeded,
            Some(&build_file_diff("a.rs".to_string(), Some("one\n"), "two\n")),
        );

        let snapshot = collector.snapshot();
        assert_eq!(snapshot.changed_files.len(), 1);
        assert_eq!(snapshot.changed_files[0].intent, "refine config");
        assert_eq!(snapshot.changed_files[0].added_lines, 2);
        assert_eq!(snapshot.changed_files[0].removed_lines, 1);
    }

    #[test]
    fn collector_begin_run_clears_previous_terminal_evidence() {
        let collector = CompletionEvidenceCollector::default();
        collector.record_tool_started("call-1", "bash", r#"{"command":"cargo test"}"#);
        collector.record_tool_result("call-1", "failed", ToolExecutionStatus::Failed, None);
        collector.begin_run();

        let snapshot = collector.snapshot();
        assert!(snapshot.changed_files.is_empty());
        assert!(!snapshot.unresolved_tool_failure);
        assert!(snapshot.started_at_ms > 0);
    }

    #[test]
    fn collector_successful_rerun_resolves_same_tool_failure() {
        let collector = CompletionEvidenceCollector::default();
        collector.record_tool_started(
            "call-1",
            "bash",
            r#"{"command":"cargo test","timeout_ms":1000}"#,
        );
        collector.record_tool_result("call-1", "tests failed", ToolExecutionStatus::Failed, None);
        collector.record_tool_started(
            "call-2",
            "bash",
            r#"{"timeout_ms":5000,"command":"cargo test"}"#,
        );
        collector.record_tool_result(
            "call-2",
            "tests passed",
            ToolExecutionStatus::Succeeded,
            None,
        );

        assert!(!collector.snapshot().unresolved_tool_failure);
        assert_eq!(collector.snapshot().failed_tool_attempts, 1);
    }

    #[test]
    fn single_adaptive_tool_failure_keeps_completed_status() {
        // Session 239 regression: a one-off failure the model recovers from by
        // doing something *different* (never retrying the identical call) must
        // not demote a completed, verified run to failed.
        let collector = CompletionEvidenceCollector::default();
        collector.record_tool_started("call-1", "bash", r#"{"command":"cargo fmt --check"}"#);
        collector.record_tool_result("call-1", "fmt errors", ToolExecutionStatus::Failed, None);

        let snapshot = collector.snapshot();
        assert!(snapshot.unresolved_tool_failure, "still surfaces as caveat");
        assert!(!snapshot.repeated_tool_failure);
        assert_eq!(
            classify_completion_status(CompletionStatus::Completed, &snapshot, None),
            CompletionStatus::Completed
        );
    }

    #[test]
    fn repeated_identical_tool_failure_is_a_loop_and_fails_the_run() {
        let collector = CompletionEvidenceCollector::default();
        for call in ["call-1", "call-2", "call-3"] {
            collector.record_tool_started(call, "edit", r#"{"path":"a.rs","old_string":"x"}"#);
            collector.record_tool_result(call, "not found", ToolExecutionStatus::Failed, None);
        }

        let snapshot = collector.snapshot();
        assert!(snapshot.repeated_tool_failure);
        assert_eq!(
            classify_completion_status(CompletionStatus::Completed, &snapshot, None),
            CompletionStatus::Failed
        );
    }

    #[test]
    fn success_of_the_same_invocation_breaks_the_failure_loop() {
        let collector = CompletionEvidenceCollector::default();
        for call in ["call-1", "call-2"] {
            collector.record_tool_started(call, "bash", r#"{"command":"cargo test"}"#);
            collector.record_tool_result(call, "failed", ToolExecutionStatus::Failed, None);
        }
        collector.record_tool_started("call-3", "bash", r#"{"command":"cargo test"}"#);
        collector.record_tool_result("call-3", "passed", ToolExecutionStatus::Succeeded, None);
        // Two more failures after the success stay below the loop threshold.
        for call in ["call-4", "call-5"] {
            collector.record_tool_started(call, "bash", r#"{"command":"cargo test"}"#);
            collector.record_tool_result(call, "failed", ToolExecutionStatus::Failed, None);
        }

        let snapshot = collector.snapshot();
        assert!(!snapshot.repeated_tool_failure);
        assert_eq!(
            classify_completion_status(CompletionStatus::Completed, &snapshot, None),
            CompletionStatus::Completed
        );
    }

    #[test]
    fn only_strictly_failed_verification_demotes_completed() {
        let snapshot = CompletionEvidenceSnapshot::default();
        let run = |status| VerificationRunRecord {
            kind: crate::verification::VerificationKind::Test,
            status,
            checks: Vec::new(),
            started_at_ms: 0,
            finished_at_ms: None,
            observed_final_workspace: Some(true),
            workspace_changes_after_last_check: Vec::new(),
            repair_attempts: 0,
            reasoning_escalations: Vec::new(),
            terminal_reason: None,
            terminal_reason_kind: None,
            delivered_workspace_binding: None,
        };
        assert_eq!(
            classify_completion_status(
                CompletionStatus::Completed,
                &snapshot,
                Some(&run(VerificationRunStatus::Failed))
            ),
            CompletionStatus::Failed
        );
        // Stale/unstable stay caveats, not failures.
        for status in [
            VerificationRunStatus::Stale,
            VerificationRunStatus::Unstable,
        ] {
            assert_eq!(
                classify_completion_status(
                    CompletionStatus::Completed,
                    &snapshot,
                    Some(&run(status))
                ),
                CompletionStatus::Completed
            );
        }
    }

    #[test]
    fn verification_evidence_is_structured_without_parsing_prose() {
        let run = VerificationRunRecord {
            kind: crate::verification::VerificationKind::Test,
            status: VerificationRunStatus::Stale,
            checks: Vec::new(),
            started_at_ms: 0,
            finished_at_ms: Some(1),
            observed_final_workspace: Some(false),
            workspace_changes_after_last_check: Vec::new(),
            repair_attempts: 0,
            reasoning_escalations: Vec::new(),
            terminal_reason: Some("arbitrary prose".to_string()),
            terminal_reason_kind: None,
            delivered_workspace_binding: None,
        };

        let evidence = completion_verification_evidence(Some(&run)).expect("evidence");

        assert_eq!(evidence.freshness, CompletionVerificationFreshness::Stale);
        assert_eq!(evidence.reason.as_deref(), Some("arbitrary prose"));
    }

    #[test]
    fn collector_keeps_snapshot_observed_workspace_changes_without_diff_stats() {
        let collector = CompletionEvidenceCollector::default();
        collector.record_workspace_changes(
            &["Cargo.lock".to_string()],
            "workspace changes observed after bash",
        );

        let snapshot = collector.snapshot();
        assert_eq!(snapshot.changed_files.len(), 1);
        assert_eq!(snapshot.changed_files[0].path, "Cargo.lock");
        assert_eq!(snapshot.changed_files[0].added_lines, 0);
        assert_eq!(snapshot.changed_files[0].removed_lines, 0);
    }

    #[test]
    fn report_names_missing_verification_and_failed_tool_as_caveats() {
        let report = CompletionReport::from_evidence(
            CompletionStatus::Completed,
            CompletionEvidenceSnapshot {
                changed_files: vec![CompletionFileChange {
                    path: "src/main.rs".to_string(),
                    status: DiffStatus::Modified,
                    intent: "update entry point".to_string(),
                    added_lines: 1,
                    removed_lines: 0,
                }],
                unresolved_tool_failure: true,
                repeated_tool_failure: false,
                failed_tool_attempts: 1,
                started_at_ms: 0,
            },
            CompletionSessionEvidence {
                verification: None,
                review: None,
                authorization_decisions: &[],
                usage: UsageTotals::default(),
                session_budget: crate::run_budget::SessionBudgetUsage::default(),
                budget_exhaustion: None,
            },
        );

        assert!(
            report
                .caveats
                .iter()
                .any(|value| value.contains("tool operation"))
        );
        assert!(
            report
                .caveats
                .iter()
                .any(|value| value.contains("No verification"))
        );
        assert!(
            report
                .render_compact()
                .contains("Completed · 1 file changed · +1 -0")
        );
    }

    #[test]
    fn report_omits_empty_sections_and_duplicate_status_caveat() {
        let report = CompletionReport::from_evidence(
            CompletionStatus::Interrupted,
            CompletionEvidenceSnapshot::default(),
            CompletionSessionEvidence {
                verification: None,
                review: None,
                authorization_decisions: &[],
                usage: UsageTotals {
                    prompt_tokens: 67_552,
                    completion_tokens: 2_618,
                    cost_micros: Some(157_717),
                    no_cache_cost_micros: Some(157_717),
                    input_cache: None,
                },
                session_budget: crate::run_budget::SessionBudgetUsage::default(),
                budget_exhaustion: None,
            },
        );

        assert_eq!(
            report.render_compact(),
            "Interrupted\nUsage · 70,170 tokens · $0.157717"
        );
    }

    #[test]
    fn report_renders_cumulative_session_budget_consumption() {
        let report = CompletionReport::from_evidence(
            CompletionStatus::Completed,
            CompletionEvidenceSnapshot::default(),
            CompletionSessionEvidence {
                verification: None,
                review: None,
                authorization_decisions: &[],
                usage: UsageTotals::default(),
                session_budget: crate::run_budget::SessionBudgetUsage {
                    turns: 12,
                    turn_limit: Some(100),
                    output_chars: 34_000,
                    output_char_limit: Some(256_000),
                    active_run_ms: 45_000,
                    active_limit_seconds: Some(7_200),
                    exact_cost_micros: Some(1_250_000),
                    cost_limit_micros: Some(5_000_000),
                },
                budget_exhaustion: None,
            },
        );

        assert!(report.render_compact().contains(
            "Budget · session turns 12/100 · output 34000/256000 · active time 45/7200s · cost $1.250000/$5.000000"
        ));
    }

    #[test]
    fn report_discloses_when_exact_cost_budget_cannot_be_enforced() {
        let report = CompletionReport::from_evidence(
            CompletionStatus::Completed,
            CompletionEvidenceSnapshot::default(),
            CompletionSessionEvidence {
                verification: None,
                review: None,
                authorization_decisions: &[],
                usage: UsageTotals::default(),
                session_budget: crate::run_budget::SessionBudgetUsage {
                    cost_limit_micros: Some(5_000_000),
                    ..crate::run_budget::SessionBudgetUsage::default()
                },
                budget_exhaustion: None,
            },
        );

        assert!(
            report
                .render_compact()
                .contains("Budget · session cost n/a/$5.000000")
        );
        assert!(report.render_compact().contains(
            "Exact session cost is unavailable, so the configured cost budget could not be enforced."
        ));
    }
}
