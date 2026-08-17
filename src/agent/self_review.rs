//! The self-review-before-done pass. Right before the coding agent would
//! hand control back, this optionally injects a critique turn — "review your own
//! diff against the original request and fix anything wrong" — into the existing
//! conversation, so the agent gets one more pass with full context and the coding
//! tool set before finishing. See [`crate::self_review`] for the policy/gating.

use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static SELF_REVIEW_TOOL_ID: AtomicU64 = AtomicU64::new(1);
const SELF_REVIEW_RESULT_MAX_CHARS: usize = 8_000;

/// Self-review policy plus the per-turn arming state it gates: whether a pass
/// may still fire this turn, the pre-task git baseline to diff against, and
/// the captured task request. `armed`/`baseline`/`request` are always
/// set together ([`Self::arm`]) and cleared together ([`Self::disarm`]), so no
/// field can be forgotten independently of the others.
#[derive(Debug, Default)]
pub(super) struct SelfReviewState {
    mode: SelfReviewMode,
    armed: bool,
    baseline: Option<ReviewBaseline>,
    request: Option<String>,
    /// Configured/detected verification commands recognized for this turn.
    verification_commands: std::collections::BTreeSet<String>,
    /// Foreground verification evidence in execution order.
    checks_run: Vec<(String, bool)>,
    /// Wall-clock timestamp for the start of the armed window. Foreground Bash
    /// snapshots cannot prove ownership, so peer file-ledger entries from this
    /// window remove only their matching low-confidence paths before review.
    armed_at_ms: Option<i64>,
    /// Whether the agent itself touched the workspace this turn: a successful
    /// `edit`/`write`, or a `bash`/`agent` call (either may mutate files). The
    /// baseline diff alone cannot distinguish the agent's changes from
    /// concurrent external edits — another session or the user's editor working
    /// in the same tree — so without this gate a pure question/research turn
    /// can end in a spurious review of someone else's diff.
    workspace_mutated: bool,
    /// Project-relative paths reported by typed mutation tools. These are the
    /// only paths that can be described to a reviewer as the agent's own.
    typed_paths: std::collections::BTreeSet<String>,
    /// Paths observed only by a before/after foreground-Bash snapshot. A Bash
    /// process shares the worktree with peers, so these remain in the review
    /// diff but are explicitly low-confidence attribution.
    bash_window_paths: std::collections::BTreeSet<String>,
    /// Set when a mutation escapes path tracking. Automatic review must not
    /// widen this to the full dirty worktree because ownership is unknown.
    unscoped_mutation: bool,
    /// Review awaiting classification after the parent's fix/confirmation turn.
    pending_run_index: Option<usize>,
    pending_run_mutated: bool,
}

/// The scoped review diff and its ownership confidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SelfReviewAttribution {
    typed_paths: Vec<String>,
    bash_window_paths: Vec<String>,
    unscoped_mutation: bool,
}

impl SelfReviewAttribution {
    fn review_scope(&self) -> Option<Vec<String>> {
        if self.unscoped_mutation {
            return None;
        }

        let paths = self
            .typed_paths
            .iter()
            .chain(&self.bash_window_paths)
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        (!paths.is_empty()).then(|| paths.into_iter().collect())
    }

    fn typed_paths(&self) -> &[String] {
        &self.typed_paths
    }

    fn bash_window_paths(&self) -> &[String] {
        &self.bash_window_paths
    }

    fn has_unscoped_mutation(&self) -> bool {
        self.unscoped_mutation
    }
}

impl SelfReviewState {
    pub(super) fn with_mode(mode: SelfReviewMode) -> Self {
        Self {
            mode,
            ..Self::default()
        }
    }

    pub(super) fn mode(&self) -> SelfReviewMode {
        self.mode
    }

    pub(super) fn set_mode(&mut self, mode: SelfReviewMode) {
        self.mode = mode;
    }

    pub(super) const fn is_armed(&self) -> bool {
        self.armed
    }

    pub(super) fn baseline(&self) -> Option<&ReviewBaseline> {
        self.baseline.as_ref()
    }

    pub(super) fn request(&self) -> Option<&str> {
        self.request.as_deref()
    }

    /// Arm for the current user turn: capture the pre-task baseline and the
    /// (optional) task request a reviewer would judge the diff against. Resets
    /// the workspace-mutation state — each turn earns its review anew.
    pub(super) fn arm(&mut self, baseline: Option<ReviewBaseline>, request: Option<String>) {
        self.baseline = baseline;
        self.armed = true;
        self.request = request;
        self.armed_at_ms = Some(crate::util::time::now_ms());
        self.workspace_mutated = false;
        self.verification_commands.clear();
        self.checks_run.clear();
        self.typed_paths.clear();
        self.bash_window_paths.clear();
        self.unscoped_mutation = false;
        self.pending_run_index = None;
        self.pending_run_mutated = false;
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
        self.baseline = None;
        self.request = None;
        self.armed_at_ms = None;
        self.workspace_mutated = false;
        self.verification_commands.clear();
        self.checks_run.clear();
        self.typed_paths.clear();
        self.bash_window_paths.clear();
        self.unscoped_mutation = false;
    }

    /// Clear every task/session-owned review field while preserving the
    /// operator's configured review mode.
    pub(super) fn reset_for_new_session(&mut self) {
        *self = Self::with_mode(self.mode);
    }

    /// Record a successful mutation-tool call. `paths` are known agent-owned
    /// paths; an empty list marks an unscoped mutation (for example, a delegated
    /// `agent`) and widens the eventual review to the full diff.
    pub(super) fn note_typed_mutation(&mut self, paths: Vec<String>) {
        self.workspace_mutated = true;
        self.pending_run_mutated |= self.pending_run_index.is_some();
        if paths.is_empty() {
            self.unscoped_mutation = true;
        } else {
            self.typed_paths.extend(paths);
        }
    }

    /// Record paths seen while foreground Bash ran. Snapshot observations do
    /// not establish ownership, even though they remain reviewable.
    pub(super) fn note_bash_window_mutation(&mut self, paths: Vec<String>) {
        self.workspace_mutated = true;
        self.pending_run_mutated |= self.pending_run_index.is_some();
        self.bash_window_paths.extend(paths);
    }

    pub(super) fn set_verification_commands(&mut self, commands: impl IntoIterator<Item = String>) {
        self.verification_commands.extend(commands);
    }

    pub(super) fn note_check_result(&mut self, command: &str, passed: bool, explicit: bool) {
        if self.armed && (explicit || self.verification_commands.contains(command)) {
            self.checks_run.push((command.to_string(), passed));
        }
    }

    pub(super) fn checks_run(&self) -> &[(String, bool)] {
        &self.checks_run
    }

    pub(super) fn append_request(&mut self, text: &str) {
        let text = text.trim();
        if !self.armed || text.is_empty() {
            return;
        }
        match self.request.as_mut() {
            Some(request) => {
                request.push_str("\n\nQueued steering:\n");
                request.push_str(text);
            }
            None => self.request = Some(text.to_string()),
        }
    }

    fn begin_disposition_tracking(&mut self, run_index: usize) {
        self.pending_run_index = Some(run_index);
        self.pending_run_mutated = false;
    }

    fn take_pending_disposition(&mut self) -> Option<(usize, bool)> {
        let run_index = self.pending_run_index.take()?;
        Some((run_index, std::mem::take(&mut self.pending_run_mutated)))
    }

    pub(super) const fn workspace_mutated(&self) -> bool {
        self.workspace_mutated
    }

    pub(super) fn armed_at_ms(&self) -> Option<i64> {
        self.armed_at_ms
    }

    /// Build the review attribution, removing only Bash-window paths that the
    /// peer ledger identifies as another session's edits during this arm window.
    /// If peer lookup is absent or fails, pass `None` so all Bash paths remain
    /// visible with their low-confidence disclaimer.
    pub(super) fn review_attribution(
        &self,
        peer_changed_paths: Option<&std::collections::BTreeSet<String>>,
    ) -> SelfReviewAttribution {
        let bash_window_paths = self
            .bash_window_paths
            .iter()
            .filter(|path| match peer_changed_paths {
                Some(paths) => !paths.contains(*path),
                None => true,
            })
            .cloned()
            .collect();
        SelfReviewAttribution {
            typed_paths: self.typed_paths.iter().cloned().collect(),
            bash_window_paths,
            unscoped_mutation: self.unscoped_mutation,
        }
    }
}

fn next_self_review_tool_id() -> String {
    let id = SELF_REVIEW_TOOL_ID.fetch_add(1, Ordering::Relaxed);
    format!("self-review-{id}")
}

impl Agent {
    /// Run the self-review pass if the policy calls for it.
    ///
    /// Returns `true` when a critique turn was injected, in which case the run
    /// loop should `continue` rather than completing. Armed once per user turn
    /// and disarmed here, so the agent's reply to the critique completes
    /// normally instead of looping.
    pub(super) async fn maybe_self_review(
        &mut self,
        sink: &SharedSink,
        cancellation_token: CancellationToken,
    ) -> bool {
        if !self.self_review.is_armed() {
            return false;
        }
        if !self.persona_self_review() {
            self.disarm_self_review();
            return false;
        }
        // Only a turn in which the agent itself touched the workspace ends in
        // a review. Question/research turns finish immediately — even when the
        // baseline diff is non-empty because something *else* (another session,
        // the user's editor) changed the tree while this turn ran.
        if !self.self_review.workspace_mutated() {
            self.disarm_self_review();
            return false;
        }

        let review_mode = self.self_review.mode();
        let wants_prompt = match review_mode.decision(self.approval_level()) {
            SelfReviewDecision::Skip => {
                self.disarm_self_review();
                return false;
            }
            SelfReviewDecision::Run => false,
            SelfReviewDecision::Ask => true,
        };

        // Capture the task-scoped diff before prompting: when nothing changed
        // since the task was armed, there is nothing to review (and no reason
        // to bother the user in `ask` mode). The diff is scoped to typed paths
        // plus surviving foreground-Bash observations; peer-recorded Bash
        // paths are removed before the diff is captured.
        let Some(baseline) = self.self_review.baseline() else {
            self.disarm_self_review();
            return false;
        };
        const PEER_ATTRIBUTION_SLOP_MS: i64 = 2_000;
        let peer_changed_paths = match (self.self_review.armed_at_ms(), &self.peer_bus) {
            (Some(armed_at_ms), Some(bus)) => {
                match bus
                    .peer_file_changes_since(armed_at_ms.saturating_sub(PEER_ATTRIBUTION_SLOP_MS))
                    .await
                {
                    Ok(paths) => Some(paths.into_iter().collect::<std::collections::BTreeSet<_>>()),
                    Err(err) => {
                        tracing::debug!(
                            error = %format!("{err:#}"),
                            "failed to query peer file changes for self-review attribution"
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        let attribution = self
            .self_review
            .review_attribution(peer_changed_paths.as_ref());
        if attribution.has_unscoped_mutation() {
            sink.status(
                "Self-review skipped: workspace changes could not be attributed to task-owned paths.",
            );
            self.disarm_self_review();
            return false;
        }
        let review_scope = attribution.review_scope();
        if review_scope.is_none() {
            self.disarm_self_review();
            return false;
        }
        let Some(diff) = capture_diff_since_baseline_scoped(
            &self.project_root,
            baseline,
            review_scope.as_deref(),
        )
        .await
        else {
            self.disarm_self_review();
            return false;
        };

        // Explicit `on`/`ask` modes retain the lower general eligibility bar.
        // Auto mode uses the higher cost gate, except that a small edit on a
        // high-risk boundary is still review-worthy.
        let below_review_threshold = if review_mode == SelfReviewMode::Auto {
            diff.is_below_auto_review_threshold()
        } else {
            diff.is_below_review_threshold()
        };
        if below_review_threshold {
            self.disarm_self_review();
            return false;
        }

        // Capture the request before disarming clears it (disarm zeroes all
        // self-review state, including the captured request).
        let request = self.self_review.request().map(str::to_string);
        let checks_run = self.self_review.checks_run().to_vec();

        // Disarm regardless of the branch taken below: a decline finishes the
        // turn, and a run must not re-trigger when the critique reply completes.
        self.disarm_self_review();

        if wants_prompt && !self.confirm_self_review(cancellation_token.clone()).await {
            return false;
        }

        if self.subagent_runner.is_some() {
            sink.status("Self-review: delegating to reviewer subagent…");
        } else {
            sink.status("Self-review: checking the changes against your request…");
        }
        self.run_self_review_pass(
            &diff,
            request.as_deref(),
            &checks_run,
            &attribution,
            sink,
            cancellation_token,
        )
        .await
    }

    /// Run the reviewer subagent (fresh, read-only) and inject its critique as a
    /// harness advisory for the parent's fix turn. Falls back to an equivalent
    /// harness advisory when no subagent runner is available or the reviewer
    /// errors. Returns `true` when a turn was injected, so the run loop continues.
    async fn run_self_review_pass(
        &mut self,
        diff: &CapturedDiff,
        request: Option<&str>,
        checks_run: &[(String, bool)],
        attribution: &SelfReviewAttribution,
        sink: &SharedSink,
        cancellation_token: CancellationToken,
    ) -> bool {
        let started_at_ms = crate::util::time::now_ms();
        let started = std::time::Instant::now();
        let Some(runner) = self.subagent_runner.clone() else {
            // No subagent runner (eval / headless without a factory): fall back to
            // the original in-conversation self-review pass.
            let run_index = self.begin_self_review_run(diff, attribution, started_at_ms, None);
            self.self_review.begin_disposition_tracking(run_index);
            return self.fall_back_to_in_conversation_self_review(diff, request, attribution);
        };

        let task = review_subagent_prompt(
            diff,
            request,
            checks_run,
            attribution.typed_paths(),
            attribution.bash_window_paths(),
            attribution.has_unscoped_mutation(),
        );
        // Git-excluded registry: the reviewer judges the baseline-scoped diff in
        // its task and must not run its own `git diff` over unrelated work.
        let registry = runner.review_registry();
        let tool_id = next_self_review_tool_id();
        let run_index =
            self.begin_self_review_run(diff, attribution, started_at_ms, Some(&tool_id));
        sink.tool_started(
            &tool_id,
            "agent",
            &serde_json::json!({
                "agent": "self-review",
                "prompt": "Review this turn's task-scoped diff before finishing."
            })
            .to_string(),
        );
        // Box the nested run: it builds an `Agent` whose own (disabled) self-review
        // would otherwise make this an infinitely-sized recursive async fn.
        // The reviewer keeps the parent model unless the user pinned one with
        // `/self-review model`, stored under the SelfReview settings id (never
        // in the delegation catalog, so it is not delegatable). A cheaper or
        // independent model here cuts the cost of a pass that fires on every
        // completed coding task and reduces correlated self-critique blind
        // spots. The fallback assignment is unused for this single-shot lane.
        let model_override = self
            .builtin_subagent_settings
            .get(crate::subagent::BuiltinSubagentId::SelfReview)
            .map(|settings| crate::tool::builtin_settings_model_chain(&settings))
            .and_then(|chain| chain.primary);
        let run = Box::pin(runner.run_self_review(
            &tool_id,
            SELF_REVIEW_SUBAGENT_INSTRUCTIONS,
            &task,
            registry,
            cancellation_token.clone(),
            crate::tool::SelfReviewRunOptions {
                model_override,
                remaining_billed_tokens: self.budget.max_session_billed_tokens.and_then(|limit| {
                    self.usage
                        .exact_session_billed_tokens()
                        .map(|used| limit.saturating_sub(used))
                }),
            },
        ));
        let (result, usage, mut usage_turns, delegated_read_evidence, child_status) = run.await;
        // Fold the reviewer's token usage into the parent's totals on every path
        // — even an errored/timed-out reviewer really spent those tokens.
        self.absorb_usage_totals(usage);
        for turn in &mut usage_turns {
            turn.attach_parent_context(&tool_id, None);
        }
        self.absorb_usage_turns(&usage_turns);
        self.import_delegated_read_evidence(&delegated_read_evidence);
        self.refresh_stale_read_advisory();
        let lifecycle_status = self_review_run_status(
            child_status,
            cancellation_token.is_cancelled(),
            result.is_ok(),
        );
        match result {
            Ok(critique) if lifecycle_status == SelfReviewRunStatus::Succeeded => {
                let critique = bounded_self_review_result(&critique);
                self.finish_self_review_run(
                    run_index,
                    started.elapsed(),
                    usage,
                    lifecycle_status,
                    &critique,
                );
                // Show the reviewer's actual critique on the tool card — a generic
                // "finished" placeholder left the modal's "subagent response" section
                // empty of the one thing worth reading. The card renders agent results
                // as markdown, so the raw critique displays as written.
                sink.tool_finished(
                    &tool_id,
                    &critique,
                    crate::output::ToolExecutionStatus::Succeeded,
                );
                await_output_delivery(sink, &tool_id).await;
                self.self_review.begin_disposition_tracking(run_index);
                self.push_harness_note(&format!(
                    "A reviewer examined your changes against the request and reported:\n\n{critique}\n\nFix anything that is genuinely wrong, then confirm. If the review found nothing that needs changing, reply with a one-line confirmation and stop."
                ));
                true
            }
            outcome => {
                let detail = match outcome {
                    Ok(critique) => format!(
                        "Reviewer ended as {} despite returning a conclusion: {critique}",
                        lifecycle_status.label()
                    ),
                    Err(err) => format!("{err:#}"),
                };
                let parent_interrupted = lifecycle_status == SelfReviewRunStatus::ParentInterrupted;
                let payload = if parent_interrupted {
                    "Reviewer subagent interrupted by parent cancellation.".to_string()
                } else {
                    bounded_self_review_result(&format!(
                        "Reviewer subagent ended as {} ({detail}); injected in-conversation self-review fallback.",
                        lifecycle_status.label()
                    ))
                };
                self.finish_self_review_run(
                    run_index,
                    started.elapsed(),
                    usage,
                    lifecycle_status,
                    &payload,
                );
                sink.tool_finished(
                    &tool_id,
                    &payload,
                    if matches!(
                        lifecycle_status,
                        SelfReviewRunStatus::Cancelled | SelfReviewRunStatus::ParentInterrupted
                    ) {
                        crate::output::ToolExecutionStatus::Interrupted
                    } else {
                        crate::output::ToolExecutionStatus::Failed
                    },
                );
                await_output_delivery(sink, &tool_id).await;
                if parent_interrupted {
                    return false;
                }
                tracing::warn!(
                    status = lifecycle_status.label(),
                    error = %detail,
                    "self-review subagent failed; falling back to in-conversation pass"
                );
                self.self_review.begin_disposition_tracking(run_index);
                self.fall_back_to_in_conversation_self_review(diff, request, attribution)
            }
        }
    }

    fn begin_self_review_run(
        &mut self,
        diff: &CapturedDiff,
        attribution: &SelfReviewAttribution,
        started_at_ms: i64,
        tool_call_id: Option<&str>,
    ) -> usize {
        let run_index = self.self_review_runs.len();
        self.self_review_runs.push(SelfReviewRunRecord {
            tool_call_id: tool_call_id.map(str::to_string),
            started_at_ms,
            mode: self.self_review.mode(),
            scope: if attribution.has_unscoped_mutation() {
                SelfReviewScope::Unscoped
            } else {
                SelfReviewScope::Scoped
            },
            diff_line_count: u32::try_from(diff.changed_line_count()).unwrap_or(u32::MAX),
            reviewer_duration_ms: 0,
            reviewer_prompt_tokens: 0,
            reviewer_completion_tokens: 0,
            reviewer_cost_micros: Some(0),
            status: SelfReviewRunStatus::Running,
            result: None,
            findings: SelfReviewFindingCounts::default(),
            disposition: None,
        });
        run_index
    }

    fn finish_self_review_run(
        &mut self,
        run_index: usize,
        duration: std::time::Duration,
        usage: UsageTotals,
        status: SelfReviewRunStatus,
        result: &str,
    ) {
        let Some(run) = self.self_review_runs.get_mut(run_index) else {
            return;
        };
        run.reviewer_duration_ms = duration.as_millis().try_into().unwrap_or(u64::MAX);
        run.reviewer_prompt_tokens = usage.prompt_tokens;
        run.reviewer_completion_tokens = usage.completion_tokens;
        run.reviewer_cost_micros = usage.cost_micros;
        run.status = status;
        run.result = Some(result.to_string());
        run.findings = if status == SelfReviewRunStatus::Succeeded {
            SelfReviewFindingCounts::parse(result)
        } else {
            SelfReviewFindingCounts::default()
        };
    }

    pub(super) fn finalize_pending_self_review(&mut self, response: &str) {
        let Some((run_index, mutated)) = self.self_review.take_pending_disposition() else {
            return;
        };
        let Some(run) = self.self_review_runs.get_mut(run_index) else {
            return;
        };
        if run.status == SelfReviewRunStatus::Running {
            let response = bounded_self_review_result(response);
            run.status = SelfReviewRunStatus::Succeeded;
            run.result = Some(response);
        }
        if run.findings.total() == 0 && !mutated {
            run.findings = SelfReviewFindingCounts::parse(response);
        }
        run.disposition = Some(infer_self_review_disposition(mutated, response));
    }

    pub(crate) fn self_review_runs(&self) -> &[SelfReviewRunRecord] {
        &self.self_review_runs
    }

    pub(crate) fn restore_self_review_runs(&mut self, mut runs: Vec<SelfReviewRunRecord>) {
        self.self_review.reset_for_new_session();
        crate::self_review::reconcile_abandoned_runs(&mut runs);
        self.self_review_runs = runs;
    }

    /// Inject the original self-review prompt as a harness advisory (the
    /// fallback when no reviewer subagent is available or it errored). Reviewer
    /// text is evidence, not a synthetic user instruction that could widen task
    /// authority. Returns `true` so the run loop continues for the fix turn.
    fn fall_back_to_in_conversation_self_review(
        &mut self,
        diff: &CapturedDiff,
        request: Option<&str>,
        attribution: &SelfReviewAttribution,
    ) -> bool {
        self.push_harness_note(&self_review_prompt(
            diff,
            request,
            attribution.typed_paths(),
            attribution.bash_window_paths(),
            attribution.has_unscoped_mutation(),
        ));
        true
    }

    pub(super) fn disarm_self_review(&mut self) {
        self.self_review.disarm();
    }

    /// Ask the user whether to run a self-review pass (the `ask` mode). Returns
    /// `true` to proceed. Degrades to `false` (skip) when there is no interactive
    /// surface, e.g. headless or eval.
    async fn confirm_self_review(&self, cancellation_token: CancellationToken) -> bool {
        let Some(interaction) = self.interaction.as_ref() else {
            return false;
        };
        let outcome = interaction
            .request_with_cancellation(
                |request_id| InteractionRequest::Question {
                    request_id,
                    prompt: "Run a self-review pass over the changes before finishing?".to_string(),
                    header: Some("Self-review".to_string()),
                    options: vec![
                        QuestionOption {
                            label: "Review".to_string(),
                            description: "Critique the diff and fix any issues".to_string(),
                            preselected: false,
                        },
                        QuestionOption {
                            label: "Finish".to_string(),
                            description: "Skip the review and complete the task".to_string(),
                            preselected: false,
                        },
                    ],
                    multiple: false,
                    origin: None,
                },
                cancellation_token,
            )
            .await;
        matches!(
            outcome,
            Ok(InteractionOutcome::Question(Some(InteractionAnswer::Choices(choices))))
                if choices.first() == Some(&0)
        )
    }
}

fn self_review_run_status(
    child_status: crate::subagent::SubagentStatus,
    parent_cancelled: bool,
    returned_conclusion: bool,
) -> SelfReviewRunStatus {
    if parent_cancelled {
        return SelfReviewRunStatus::ParentInterrupted;
    }
    match child_status {
        crate::subagent::SubagentStatus::Running => SelfReviewRunStatus::Failed,
        crate::subagent::SubagentStatus::Succeeded if returned_conclusion => {
            SelfReviewRunStatus::Succeeded
        }
        crate::subagent::SubagentStatus::Succeeded | crate::subagent::SubagentStatus::Failed => {
            SelfReviewRunStatus::Failed
        }
        crate::subagent::SubagentStatus::TimedOut => SelfReviewRunStatus::TimedOut,
        crate::subagent::SubagentStatus::Cancelled => SelfReviewRunStatus::Cancelled,
    }
}

fn bounded_self_review_result(result: &str) -> String {
    if result.chars().count() <= SELF_REVIEW_RESULT_MAX_CHARS {
        return result.to_string();
    }
    let head = result
        .chars()
        .take(SELF_REVIEW_RESULT_MAX_CHARS.saturating_sub(1))
        .collect::<String>();
    format!("{head}…")
}

async fn await_output_delivery(sink: &SharedSink, tool_id: &str) {
    let Some(barrier) = sink.delivery_barrier() else {
        return;
    };
    if !barrier.wait().await {
        tracing::debug!(
            tool_call_id = tool_id,
            "self-review output receiver closed before its terminal marker was applied"
        );
    }
}

fn infer_self_review_disposition(mutated: bool, response: &str) -> SelfReviewDisposition {
    if mutated {
        SelfReviewDisposition::Fixed
    } else if is_no_change_confirmation(response) {
        SelfReviewDisposition::NoneNeeded
    } else {
        SelfReviewDisposition::Rebutted
    }
}

fn is_no_change_confirmation(response: &str) -> bool {
    let response = response.trim();
    if response.lines().count() > 1 {
        return false;
    }
    let normalized = response.to_ascii_lowercase();
    [
        "looks correct",
        "look correct",
        "no changes needed",
        "nothing to fix",
        "no issues",
        "no findings",
    ]
    .iter()
    .any(|phrase| normalized.contains(phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_routes_typed_and_bash_paths_and_resets_them() {
        let mut state = SelfReviewState::default();
        state.arm(None, None);
        state.note_typed_mutation(vec!["src/typed.rs".to_string()]);
        state.note_bash_window_mutation(vec!["src/bash.rs".to_string()]);

        let attribution = state.review_attribution(None);
        assert_eq!(attribution.typed_paths(), ["src/typed.rs"]);
        assert_eq!(attribution.bash_window_paths(), ["src/bash.rs"]);
        assert_eq!(
            attribution.review_scope(),
            Some(vec!["src/bash.rs".to_string(), "src/typed.rs".to_string()])
        );

        state.arm(None, None);
        let rearmed = state.review_attribution(None);
        assert!(rearmed.typed_paths().is_empty());
        assert!(rearmed.bash_window_paths().is_empty());
        assert_eq!(rearmed.review_scope(), None);

        state.disarm();
        assert!(!state.workspace_mutated());
        assert!(state.armed_at_ms().is_none());
        let reset = state.review_attribution(None);
        assert!(reset.typed_paths().is_empty());
        assert!(reset.bash_window_paths().is_empty());
        assert_eq!(reset.review_scope(), None);
    }

    #[test]
    fn arming_new_task_abandons_pending_disposition() {
        let mut state = SelfReviewState::with_mode(SelfReviewMode::On);
        state.begin_disposition_tracking(7);
        state.note_typed_mutation(vec!["src/lib.rs".to_string()]);

        state.arm(None, Some("new task".to_string()));

        assert_eq!(state.take_pending_disposition(), None);
        assert!(!state.workspace_mutated());
        assert_eq!(state.request(), Some("new task"));
    }

    #[test]
    fn new_session_reset_preserves_mode_and_clears_pending_disposition() {
        let mut state = SelfReviewState::with_mode(SelfReviewMode::On);
        state.arm(None, Some("old task".to_string()));
        state.note_typed_mutation(vec!["src/lib.rs".to_string()]);
        state.begin_disposition_tracking(7);

        state.reset_for_new_session();

        assert_eq!(state.mode(), SelfReviewMode::On);
        assert!(!state.is_armed());
        assert_eq!(state.baseline(), None);
        assert_eq!(state.request(), None);
        assert!(state.checks_run().is_empty());
        assert_eq!(state.take_pending_disposition(), None);
        assert!(!state.workspace_mutated());
    }

    #[test]
    fn peer_subtraction_removes_only_bash_window_paths() {
        let mut state = SelfReviewState::default();
        state.note_typed_mutation(vec![
            "src/shared.rs".to_string(),
            "src/typed.rs".to_string(),
        ]);
        state.note_bash_window_mutation(vec![
            "src/shared.rs".to_string(),
            "src/bash.rs".to_string(),
        ]);
        let peer_paths = std::collections::BTreeSet::from([
            "src/shared.rs".to_string(),
            "src/peer.rs".to_string(),
        ]);

        let attribution = state.review_attribution(Some(&peer_paths));
        assert_eq!(
            attribution.typed_paths(),
            ["src/shared.rs", "src/typed.rs"],
            "a peer claim must not erase typed mutation ownership"
        );
        assert_eq!(attribution.bash_window_paths(), ["src/bash.rs"]);
        assert_eq!(
            attribution.review_scope(),
            Some(vec![
                "src/bash.rs".to_string(),
                "src/shared.rs".to_string(),
                "src/typed.rs".to_string()
            ])
        );
    }

    #[test]
    fn unscoped_typed_mutation_widens_review_despite_known_paths() {
        let mut state = SelfReviewState::default();
        state.note_typed_mutation(Vec::new());
        state.note_bash_window_mutation(vec!["src/bash.rs".to_string()]);

        assert_eq!(state.review_attribution(None).review_scope(), None);
    }

    #[test]
    fn check_evidence_and_queued_request_reset_with_the_arm_window() {
        let mut state = SelfReviewState::default();
        state.arm(None, Some("initial request".to_string()));
        state.set_verification_commands(["cargo test --locked".to_string()]);
        state.note_check_result("cargo test --locked", true, false);
        state.note_check_result("cargo fmt --all", true, false);
        state.note_check_result("custom explicit check", false, true);
        state.append_request("also update the docs");

        assert_eq!(
            state.checks_run(),
            &[
                ("cargo test --locked".to_string(), true),
                ("custom explicit check".to_string(), false),
            ]
        );
        let request = state.request().unwrap_or_default();
        assert!(request.contains("initial request"));
        assert!(request.contains("also update the docs"));

        state.disarm();
        assert!(state.checks_run().is_empty());
        assert!(state.request().is_none());
    }

    #[test]
    fn disposition_inference_distinguishes_fixes_confirmations_and_rebuttals() {
        assert_eq!(
            infer_self_review_disposition(true, "fixed it"),
            SelfReviewDisposition::Fixed
        );
        assert_eq!(
            infer_self_review_disposition(false, "The changes look correct."),
            SelfReviewDisposition::NoneNeeded
        );
        assert_eq!(
            infer_self_review_disposition(false, "That finding belongs to a peer session."),
            SelfReviewDisposition::Rebutted
        );
        assert_eq!(
            infer_self_review_disposition(
                false,
                "The finding concerns a peer-owned file.\nNo change was appropriate."
            ),
            SelfReviewDisposition::Rebutted
        );
    }

    #[test]
    fn reviewer_lifecycle_mapping_preserves_every_terminal_reason() {
        use crate::subagent::SubagentStatus;

        assert_eq!(
            self_review_run_status(SubagentStatus::Succeeded, false, true),
            SelfReviewRunStatus::Succeeded
        );
        assert_eq!(
            self_review_run_status(SubagentStatus::Succeeded, false, false),
            SelfReviewRunStatus::Failed
        );
        assert_eq!(
            self_review_run_status(SubagentStatus::Failed, false, false),
            SelfReviewRunStatus::Failed
        );
        assert_eq!(
            self_review_run_status(SubagentStatus::TimedOut, false, false),
            SelfReviewRunStatus::TimedOut
        );
        assert_eq!(
            self_review_run_status(SubagentStatus::Cancelled, false, false),
            SelfReviewRunStatus::Cancelled
        );
        assert_eq!(
            self_review_run_status(SubagentStatus::Cancelled, true, false),
            SelfReviewRunStatus::ParentInterrupted
        );
    }

    #[test]
    fn reviewer_result_is_bounded_without_splitting_characters() {
        let oversized = "ą".repeat(SELF_REVIEW_RESULT_MAX_CHARS + 10);

        let bounded = bounded_self_review_result(&oversized);

        assert_eq!(bounded.chars().count(), SELF_REVIEW_RESULT_MAX_CHARS);
        assert!(bounded.ends_with('…'));
    }
}
