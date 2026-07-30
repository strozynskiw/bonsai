use super::*;
use crate::util::tool_args::normalize_tool_call_arguments_json;

mod context;
mod coordinator;
mod tools;
mod verification;

const REPEATED_INSPECTION_TURN_LIMIT: usize = 2;
/// Structured file reads get one extra turn so the sequence can be: real
/// bytes, compact pointer, explicit real-byte refresh.
const STRUCTURED_READ_REPEAT_TURN_LIMIT: usize = 3;
const REPEATED_INSPECTION_REJECTION_LIMIT: usize = 1;
/// How many recent inspection-only turn signatures to remember, so an
/// *alternating* loop (e.g. `read` ↔ `git status` as separate turns) is caught
/// as re-visiting a small set — not just byte-identical consecutive repeats
///.
const INSPECTION_WINDOW: usize = 6;
const REPAIR_ADVISORY_LINES: usize = 10;
const REPAIR_ADVISORY_LINE_CHARS: usize = 220;
/// Consecutive one-small-call turns before the batching advisory fires.
/// Static prompt guidance alone is not enough: a gpt-5.5 run averaged
/// 1.16 tool calls per turn across 86 turns with the batching rule already in
/// its system prompt, and every extra round-trip re-sends the full prompt.
const BATCHING_HINT_STREAK: usize = 3;
const BATCHING_HINT_REARM_STREAK: usize = BATCHING_HINT_STREAK * 5;
const BATCHING_HINT_LIMIT: usize = 3;
/// Distinct model turns that may touch the same file-read target before Bonsai
/// treats it as a loop: one real read, one compact pointer, and one explicit
/// refresh. Counts turns, not calls, so one batched multi-window inspection is
/// still allowed.
const READ_STORM_TARGET_TURN_LIMIT: usize = 3;
const READ_STORM_REJECTION_LIMIT: usize = 1;
const PLANNING_RESEARCH_REJECTION_LIMIT: usize = 1;
/// Let a failed call retry once in case the failure was transient. A third
/// identical request is rejected with corrective guidance; repeating it after
/// that ends the run instead of burning the full iteration budget.
const REPEATED_FAILED_CALL_LIMIT: usize = 2;
const REPEATED_FAILED_CALL_REJECTION_LIMIT: usize = 1;
const FAILED_CALL_WINDOW: usize = 8;
/// Detects and steers the coding persona's silent research spiral (observed live: codex
/// gpt-5.6-terra at max reasoning spent 47 minutes on ~60 consecutive
/// one-read turns "assessing uncommitted peer changes", never editing and
/// never telling the user why). Distinct-file reads defeat
/// [`RepeatedInspectionGuard`]/[`ReadStormGuard`] (a fresh signature every
/// turn), and on-disk churn from concurrent WIP kept disarming their
/// freshness resets — so this guard counts *turns without progress*, not
/// repeats, and deliberately ignores file versions.
/// Allow a substantial initial investigation phase before interrupting with a
/// progress nudge; complex cross-module changes often require several turns
/// to locate the relevant implementation and tests. This guard never ends the
/// run; explicit turn/time/cost budgets and cancellation remain its boundaries.
pub(in crate::agent) const IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS: usize = 10;
pub(in crate::agent) const IMPLEMENTATION_STALL_SECOND_NUDGE_TURNS: usize = 14;
pub(in crate::agent) const IMPLEMENTATION_STALL_REPEATED_NUDGE_START_TURNS: usize = 18;
pub(in crate::agent) const IMPLEMENTATION_STALL_REPEATED_NUDGE_INTERVAL_TURNS: usize = 16;

/// Result of [`Agent::call_model`]: either the provider responded, or
/// compaction was cancelled mid-preflight and the turn should end as
/// interrupted rather than treat that as an error.
enum ModelCallOutcome {
    Response(StreamedResponse),
    Interrupted,
}

#[derive(Debug, Default, Clone)]
struct ToolExecutionOutcome {
    interrupted_mid_tools: bool,
    detached_subagent_ids: Vec<String>,
    wait_reason: Option<WaitReason>,
    tool_observations: Vec<ToolCallObservation>,
    reset_loop_guards: bool,
}

#[derive(Debug, Clone)]
struct ToolCallObservation {
    signature: String,
    tool_name: String,
    status: crate::output::ToolExecutionStatus,
    makes_progress: bool,
}

struct QueuedMessageState<'state, 'receiver> {
    receiver: &'state mut Option<&'receiver mut mpsc::UnboundedReceiver<QueuedUserMessageCommand>>,
    pending: &'state mut VecDeque<QueuedUserMessage>,
    cancelled_ids: &'state mut HashSet<u64>,
}

struct ToolResultBatchContext<'state, 'group> {
    trusted_contexts: &'state mut Vec<String>,
    successful_rust_edits: &'state mut Vec<PathBuf>,
    launch_group_id: Option<&'group str>,
}

impl QueuedMessageState<'_, '_> {
    fn next(&mut self) -> Vec<QueuedUserMessage> {
        next_queued_messages(self.receiver, self.pending, self.cancelled_ids)
    }
}

#[derive(Default)]
struct DeferredAssistantOutput {
    /// Accumulated answer text. One growing buffer, not a `Vec` of per-delta
    /// `String`s: this sink sits on the per-token path of deferred turns, and
    /// the flush never needed the original delta boundaries — downstream
    /// sinks only append.
    text: String,
    done: bool,
}

struct DeferredAssistantSink {
    inner: SharedSink,
    assistant: std::sync::Mutex<DeferredAssistantOutput>,
}

impl DeferredAssistantSink {
    fn new(inner: SharedSink) -> Arc<Self> {
        Arc::new(Self {
            inner,
            assistant: std::sync::Mutex::new(DeferredAssistantOutput::default()),
        })
    }

    fn flush(&self) {
        let output = self.take_output();
        if !output.text.is_empty() {
            self.inner.assistant_delta(&output.text);
        }
        if output.done {
            self.inner.assistant_done();
        }
    }

    fn discard(&self) {
        let _ = self.take_output();
    }

    fn take_output(&self) -> DeferredAssistantOutput {
        self.assistant
            .lock()
            .map(|mut output| std::mem::take(&mut *output))
            .unwrap_or_default()
    }
}

impl crate::output::OutputSink for DeferredAssistantSink {
    fn assistant_delta(&self, text: &str) {
        if let Ok(mut output) = self.assistant.lock() {
            output.text.push_str(text);
        }
    }

    fn assistant_done(&self) {
        if let Ok(mut output) = self.assistant.lock() {
            output.done = true;
        }
    }

    fn reasoning_delta(&self, text: &str) {
        self.inner.reasoning_delta(text);
    }

    fn attempt_started(&self) {
        self.inner.attempt_started();
    }

    fn attempt_discarded(&self) {
        // The abandoned attempt's buffered answer text must go with it, or
        // the retry's deltas would splice onto it at the next flush.
        self.discard();
        self.inner.attempt_discarded();
    }

    fn thinking(&self, text: &str) {
        self.inner.thinking(text);
    }

    fn tool_calls_started(&self, calls: &[crate::output::ToolCallStart]) {
        self.inner.tool_calls_started(calls);
    }

    fn tool_started(&self, id: &str, name: &str, arguments: &str) {
        self.inner.tool_started(id, name, arguments);
    }

    fn tool_output(&self, id: &str, output: &str) {
        self.inner.tool_output(id, output);
    }

    fn tool_finished(&self, id: &str, result: &str, status: crate::output::ToolExecutionStatus) {
        self.inner.tool_finished(id, result, status);
    }

    fn tool_finished_with_diff(
        &self,
        id: &str,
        result: &str,
        status: crate::output::ToolExecutionStatus,
        diff: crate::diff::FileDiff,
    ) {
        self.inner.tool_finished_with_diff(id, result, status, diff);
    }

    fn workspace_changed(&self, paths: &[String], intent: &str) {
        self.inner.workspace_changed(paths, intent);
    }

    fn queued_user_message_sent(&self, id: u64, text: &str) {
        self.inner.queued_user_message_sent(id, text);
    }

    fn context_updated(&self, report: ContextReport) {
        self.inner.context_updated(report);
    }

    fn transient_status(&self, text: &str) {
        self.inner.transient_status(text);
    }

    fn status(&self, text: &str) {
        self.inner.status(text);
    }

    fn compaction_status(&self, text: &str) {
        self.inner.compaction_status(text);
    }

    fn error(&self, text: &str) {
        self.inner.error(text);
    }
}

struct SubagentCancelWatcher {
    handle: Option<tokio::task::JoinHandle<()>>,
    runner: Option<SubagentRunner>,
    cancellation_token: CancellationToken,
}

impl SubagentCancelWatcher {
    fn spawn(
        runner: Option<SubagentRunner>,
        cancellation_token: CancellationToken,
    ) -> SubagentCancelWatcher {
        // The spawned task exists for promptness: it stops subagents the
        // moment cancellation lands, while the parent run may still be winding
        // down a stream or tool call. Correctness is guaranteed by Drop below.
        let handle = runner.clone().map(|runner| {
            let cancellation_token = cancellation_token.clone();
            tokio::spawn(async move {
                cancellation_token.cancelled().await;
                runner.cancel_all_running();
            })
        });
        Self {
            handle,
            runner,
            cancellation_token,
        }
    }
}

impl Drop for SubagentCancelWatcher {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        // The run can observe cancellation and exit before the watcher task is
        // ever scheduled — aborting it above would then silently skip the sweep
        // and leave detached subagents (whose tokens are not children of the
        // parent's) running forever. Sweep synchronously on the way out; this
        // also covers hard parent aborts, where this Drop is the only code
        // that still runs. Idempotent with the watcher and with the explicit
        // turn-boundary sweeps.
        if self.cancellation_token.is_cancelled()
            && let Some(runner) = &self.runner
        {
            runner.cancel_all_running();
        }
    }
}

/// The assistant message recording the model's tool-call requests: response
/// content (if any) plus one `Function` tool call per request, arguments
/// compacted for context. Free of `self` — a pure projection of `response`.
fn build_assistant_message(response: &StreamedResponse) -> Result<ChatCompletionRequestMessage> {
    let mut assistant_tool_calls = Vec::new();
    for tool_call in &response.tool_calls {
        let compact_arguments =
            compact_tool_arguments_for_context(&tool_call.name, &tool_call.arguments);
        assistant_tool_calls.push(ChatCompletionMessageToolCalls::Function(
            ChatCompletionMessageToolCall {
                id: tool_call.id.clone(),
                function: FunctionCall {
                    name: tool_call.name.clone(),
                    arguments: normalize_tool_call_arguments_json(&compact_arguments),
                },
            },
        ));
    }
    let mut assistant_builder = ChatCompletionRequestAssistantMessageArgs::default();
    if !response.content.is_empty() {
        assistant_builder.content(response.content.as_str());
    }
    assistant_builder.tool_calls(assistant_tool_calls);
    Ok(ChatCompletionRequestMessage::Assistant(
        assistant_builder.build()?,
    ))
}

impl Agent {
    /// Shared preamble of [`Self::run`] and [`Self::run_with_queue`]: arm the
    /// turn with the fresh user input before either hands off to the run loop.
    async fn begin_run(&mut self, input: &UserInput, sink: &SharedSink) -> Result<()> {
        // A human-submitted turn resets the peer hop chain (anti-loop): only
        // auto-wake turns carry hops forward.
        if let Some(bus) = &self.peer_bus {
            bus.begin_turn(crate::peer::TurnOrigin::Human);
        }
        self.set_planning_advisory(None);
        // Volatile project state (git status) refreshes at the top of every
        // run-loop iteration, which covers the first model call too — no
        // begin_run refresh needed. Self-review and memory recall key off the
        // model-facing text (chip placeholders already expanded).
        self.arm_self_review_for_coding_task(&input.text).await;
        self.push_live_user_message(input).await?;
        self.observe_user_turn_for_episodes(&input.text);
        self.last_retryable_turn = true;
        // Memory recall runs only after the live user message is
        // accepted. If mention expansion fails, no background note or dedup
        // state is left behind for a turn the model never sees.
        self.inject_recalled_memory(&input.text).await;
        self.emit_context_updated(sink);
        Ok(())
    }

    pub async fn run(
        &mut self,
        user_input: &str,
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> Result<AgentRunResult> {
        self.begin_run(&UserInput::from_text(user_input), &sink)
            .await?;
        self.run_current_context(cancellation_token, sink).await
    }

    pub async fn run_with_queue(
        &mut self,
        input: UserInput,
        cancellation_token: CancellationToken,
        sink: SharedSink,
        mut queued_messages: mpsc::UnboundedReceiver<QueuedUserMessageCommand>,
    ) -> Result<AgentRunResult> {
        self.begin_run(&input, &sink).await?;
        self.run_current_context_inner(cancellation_token, sink, Some(&mut queued_messages))
            .await
    }

    pub async fn run_current_context(
        &mut self,
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> Result<AgentRunResult> {
        let result = self
            .run_current_context_inner(cancellation_token, sink, None)
            .await;
        self.finish_verification_run(&result).await;
        result
    }

    pub async fn run_current_context_with_queue(
        &mut self,
        cancellation_token: CancellationToken,
        sink: SharedSink,
        mut queued_messages: mpsc::UnboundedReceiver<QueuedUserMessageCommand>,
    ) -> Result<AgentRunResult> {
        let result = self
            .run_current_context_inner(cancellation_token, sink, Some(&mut queued_messages))
            .await;
        self.finish_verification_run(&result).await;
        result
    }

    pub(super) async fn run_current_context_inner(
        &mut self,
        cancellation_token: CancellationToken,
        sink: SharedSink,
        queued_messages: Option<&mut mpsc::UnboundedReceiver<QueuedUserMessageCommand>>,
    ) -> Result<AgentRunResult> {
        let result = coordinator::run(self, cancellation_token, sink, queued_messages).await;
        // Every run variant funnels through here, so this is the one terminal
        // outcome line the support lifecycle log needs.
        let outcome = match &result {
            Ok(AgentRunResult::Completed(_)) => "completed",
            Ok(AgentRunResult::Interrupted(_)) => "interrupted",
            Ok(AgentRunResult::Waiting(_)) => "waiting",
            Err(_) => "error",
        };
        tracing::info!(
            target: "bonsai::run",
            lane = %self.execution_lane.label(),
            outcome,
            turns = self.usage.usage_turns.len(),
            "run finished"
        );
        result
    }

    pub(crate) fn cancel_running_subagents(&self) {
        if let Some(runner) = &self.subagent_runner {
            runner.cancel_all_running();
        }
    }

    /// Drain and push any messages queued by the composer while this run was
    /// in flight, then surface one context update for the batch. A no-op when
    /// nothing was queued (the common case).
    async fn drain_queued_messages(
        &mut self,
        queued_messages: &mut Option<&mut mpsc::UnboundedReceiver<QueuedUserMessageCommand>>,
        pending_queued_messages: &mut VecDeque<QueuedUserMessage>,
        cancelled_queued_message_ids: &mut HashSet<u64>,
        sink: &SharedSink,
    ) -> Result<bool> {
        let queued = next_queued_messages(
            queued_messages,
            pending_queued_messages,
            cancelled_queued_message_ids,
        );
        if queued.is_empty() {
            return Ok(false);
        }
        for message in queued {
            self.self_review.append_request(&message.input.text);
            self.push_live_user_message(&message.input).await?;
            // Mid-run steering only moves the episode boundary anchor (and
            // opens a span when none is active); it never closes an episode.
            self.observe_user_turn_for_episodes(&message.input.text);
            sink.queued_user_message_sent(message.id, &message.transcript_text);
        }
        self.emit_context_updated(sink);
        Ok(true)
    }

    /// Prepare and send one model request: preflight context estimation
    /// (which may trigger compaction), the retried provider call, and the
    /// perf/usage bookkeeping that follows a response. Returns
    /// [`ModelCallOutcome::Interrupted`] when compaction was cancelled
    /// mid-preflight — the run loop ends the turn on that signal rather than
    /// treating it as an error.
    async fn call_model(
        &mut self,
        tool_schema: &ToolSchemaPayload,
        sink: &SharedSink,
        cancellation_token: CancellationToken,
    ) -> Result<ModelCallOutcome> {
        if let Some(exhaustion) = self.session_budget_exhaustion() {
            return Err(anyhow::Error::new(exhaustion));
        }
        let mut preflight_capture = PreflightPerfCapture::default();
        let preflight_started_at = std::time::Instant::now();
        let request_messages = match self
            .prepare_context_for_model(
                tool_schema,
                sink,
                cancellation_token.clone(),
                &mut preflight_capture,
            )
            .await
        {
            Ok(request_messages) => request_messages,
            Err(err) if is_compaction_cancelled(&err) => {
                return Ok(ModelCallOutcome::Interrupted);
            }
            Err(err) => return Err(err),
        };
        let preflight = preflight_capture.finish(preflight_started_at.elapsed());
        sink.thinking("Calling model");
        self.log_request(&request_messages, tool_schema.tools());
        let provider_started_at = std::time::Instant::now();
        let (provider_sink, first_output) = PerfSink::shared(sink.clone());
        let verification_reasoning = self
            .verification
            .active_verification
            .as_ref()
            .and_then(|active| active.reasoning_override);
        let request_options = verification_reasoning
            .map(crate::provider::ProviderRequestOptions::with_reasoning)
            .unwrap_or_default();
        let first_call = match self
            .chat_stream_with_retry_options(
                &request_messages,
                tool_schema.tools(),
                request_options,
                cancellation_token.clone(),
                provider_sink,
            )
            .await
        {
            Ok(response) => response,
            Err(failure) => {
                let (err, attempts) = failure.into_parts();
                let provider = ProviderPerf {
                    first_output_duration: first_output.duration_since(provider_started_at),
                    total_duration: provider_started_at.elapsed(),
                    attempts: attempts.clone(),
                    generation_budget: self.generation_budget(),
                };
                self.caches.last_perf_report = Some(self.build_perf_report(
                    preflight,
                    provider,
                    tool_schema,
                    &request_messages,
                ));
                let effective_reasoning = request_options
                    .reasoning
                    .unwrap_or_else(|| self.provider.reasoning());
                self.record_usage_details(None, false, None, 0, attempts, effective_reasoning);
                self.clear_drop_next_turn_controls();
                return Err(err);
            }
        };
        let mut attempts = first_call.attempts;
        let mut response = first_call.response;
        let mut budget_exhaustion = first_call.budget_exhaustion;
        let mut effective_reasoning = request_options
            .reasoning
            .unwrap_or_else(|| self.provider.reasoning());
        if response.tool_calls.is_empty()
            && response.content.trim().is_empty()
            && !matches!(
                budget_exhaustion,
                Some(crate::run_budget::RunBudgetExhaustion::SessionOutput { .. })
            )
            && matches!(
                response.finish_reason(),
                Some(
                    crate::provider::FinishReason::Length
                        | crate::provider::FinishReason::GenerationBudget
                )
            )
            && let Some(lower_effort) = verification_reasoning
                .unwrap_or_else(|| self.provider.reasoning())
                .lower_for_retry()
        {
            self.record_streamed_usage(&response, attempts.clone(), effective_reasoning);
            sink.status(&format!(
                "Generation reached its output budget; retrying this turn once at {lower_effort} effort."
            ));
            let retry = self
                .chat_stream_with_retry_options(
                    &request_messages,
                    tool_schema.tools(),
                    crate::provider::ProviderRequestOptions::with_reasoning(lower_effort),
                    cancellation_token.clone(),
                    sink.clone(),
                )
                .await?;
            attempts.extend(retry.attempts);
            response = retry.response;
            budget_exhaustion = retry.budget_exhaustion;
            effective_reasoning = lower_effort;
        }
        let terminal_budget_exhaustion = matches!(
            response.finish_reason(),
            Some(crate::provider::FinishReason::GenerationBudget)
        )
        .then_some(
            budget_exhaustion.unwrap_or(crate::run_budget::RunBudgetExhaustion::ProviderGeneration),
        );
        let provider = ProviderPerf {
            first_output_duration: first_output.duration_since(provider_started_at),
            total_duration: provider_started_at.elapsed(),
            attempts: attempts.clone(),
            generation_budget: self.generation_budget(),
        };
        self.caches.last_perf_report =
            Some(self.build_perf_report(preflight, provider, tool_schema, &request_messages));
        self.clear_drop_next_turn_controls();

        self.record_streamed_usage(&response, attempts, effective_reasoning);
        if let Some(warning) = self.new_cache_warning() {
            sink.status(&warning);
        }
        let usage_report = self.context_report();
        sink.context_updated(usage_report);

        if let Some(exhaustion) = terminal_budget_exhaustion {
            return Err(anyhow::Error::new(exhaustion));
        }

        Ok(ModelCallOutcome::Response(response))
    }

    /// Batch `tool_calls`, execute each batch (selecting against
    /// `cancellation_token` so Esc/Ctrl+C lands mid-batch rather than only
    /// between batches), and apply every result to the conversation.
    ///
    /// A long-running foreground tool (e.g. bash) must not pin the agent past
    /// a cancel: dropping the in-flight futures aborts them (bash's
    /// `kill_on_drop` terminates the shell). The assistant message already
    /// references every tool-call id, so dropped and not-yet-started calls are
    /// answered with a synthetic interrupted result to keep the next request
    /// well-formed.
    /// Answer a set of not-yet-run tool calls with synthetic "skipped" results:
    /// announce each, then report + apply the skip so the transcript and model
    /// history stay consistent. Shared by the peer-wait and queued-message exit
    /// paths so the two can't drift.
    async fn drain_skipped_calls(
        &mut self,
        skipped_calls: Vec<ToolCall>,
        reason: &str,
        sink: &SharedSink,
        trusted_contexts: &mut Vec<String>,
    ) -> Result<()> {
        for call in &skipped_calls {
            sink.tool_started(&call.id, &call.name, &call.arguments);
        }
        let mut skipped_rust_edits = Vec::new();
        for (tool_call, result, status) in skipped_tool_results(skipped_calls, reason) {
            report_tool_completion(sink, &tool_call, &result, status);
            self.apply_tool_result(
                tool_call,
                result,
                status,
                sink,
                ToolResultBatchContext {
                    trusted_contexts,
                    successful_rust_edits: &mut skipped_rust_edits,
                    launch_group_id: None,
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn execute_tool_calls(
        &mut self,
        tool_calls: &[ToolCall],
        tool_registry: &Arc<ToolRegistry>,
        tool_rejections: ToolRejections,
        sink: &SharedSink,
        cancellation_token: CancellationToken,
        mut queued_messages: QueuedMessageState<'_, '_>,
    ) -> Result<ToolExecutionOutcome> {
        let mut interrupted_mid_tools = false;
        let mut outcome = ToolExecutionOutcome::default();
        let planned_batches = tool_call_batches_with_yolo(
            tool_calls,
            tool_registry,
            &self.project_root,
            self.yolo_enabled(),
            &self.hooks.serialized_tool_names(),
        );
        // Batching telemetry: how the turn's tool calls were grouped. A
        // serialized batch (width 1) for >1 calls means a conflict forced
        // ordering — useful when diagnosing why a turn ran slowly.
        tracing::debug!(
            target: "bonsai::batching",
            tool_calls = tool_calls.len(),
            batches = planned_batches.len(),
            widths = ?planned_batches.iter().map(Vec::len).collect::<Vec<_>>(),
            "planned tool-call batches",
        );
        let mut batches = planned_batches.into_iter().peekable();
        let mut trusted_contexts = Vec::new();
        let launch_group_id = tool_calls
            .iter()
            .any(|tool_call| {
                tool_registry.get(&tool_call.name).is_some_and(|tool| {
                    tool.effect_policy() == crate::tool::ToolEffectPolicy::Delegated
                })
            })
            .then(|| {
                let id = format!("group-{}", self.next_subagent_launch_group_id);
                self.next_subagent_launch_group_id =
                    self.next_subagent_launch_group_id.saturating_add(1);
                id
            });
        while let Some(batch) = batches.next() {
            let foreground_bash = batch.iter().any(foreground_bash_call);
            let diagnostic_baseline = if foreground_bash && self.lsp_hub.is_some() {
                Some(crate::lsp::DiagnosticSnapshot::default())
            } else {
                self.lsp_diagnostic_baseline_for_batch(&batch).await
            };
            let bash_baseline = if foreground_bash {
                capture_worktree_snapshot(&self.project_root).await
            } else {
                None
            };
            let delegated_baseline = if batch.iter().any(|tool_call| {
                tool_registry.get(&tool_call.name).is_some_and(|tool| {
                    tool.effect_policy() == crate::tool::ToolEffectPolicy::Delegated
                })
            }) {
                capture_worktree_snapshot(&self.project_root).await
            } else {
                None
            };
            let batch_width = batch.len();
            let batch_started = std::time::Instant::now();
            let mut results = if interrupted_mid_tools {
                interrupted_tool_results(batch)
            } else {
                Self::run_tool_batch(
                    batch,
                    tool_registry,
                    &tool_rejections,
                    sink,
                    &cancellation_token,
                    &mut interrupted_mid_tools,
                    &self.hooks,
                    self.tool_origin.clone(),
                    self.budget.max_tool_duration,
                )
                .await
            };

            tracing::debug!(
                target: "bonsai::batching",
                width = batch_width,
                elapsed_ms = batch_started.elapsed().as_millis() as u64,
                interrupted = interrupted_mid_tools,
                "executed tool-call batch",
            );

            // A write-capable delegated agent reports an unscoped capability.
            // A shared-worktree snapshot can prove that a lone delegation was a
            // no-op, but a non-empty delta cannot prove path ownership because
            // peers/external processes may write during the same window. Keep
            // such results unscoped rather than claiming task-owned paths.
            if let Some(baseline) = delegated_baseline {
                let observed_paths =
                    capture_worktree_snapshot_including(&self.project_root, &baseline.paths())
                        .await
                        .map(|current| baseline.changed_paths(&current));
                refine_delegated_workspace_effects(&mut results, observed_paths.as_deref());
            }

            // Apply every result in the batch, then emit a single context
            // update. Emitting per result would rebuild the full report
            // (re-tokenizing the whole history) once per tool, which is the
            // hottest path in a turn on long conversations.
            let mut successful_rust_edits = Vec::new();
            let observation_start = outcome.tool_observations.len();
            let batch_wait_reason = results.iter().find_map(|(_, result, status)| {
                if status.is_success()
                    && let ToolOutput::WaitStarted { reason, .. } = result
                {
                    Some(reason.clone())
                } else {
                    None
                }
            });
            let detached_subagents = results
                .iter()
                .filter_map(|(tool_call, result, _)| match result {
                    ToolOutput::SubagentStarted { subtask_id, .. } => {
                        Some((subtask_id.as_str(), tool_call.id.as_str()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if !detached_subagents.is_empty()
                && let Some(runner) = &self.subagent_runner
            {
                for (subtask_id, tool_call_id) in detached_subagents {
                    let _ = runner.subagents().attach_tool_call_in_group(
                        subtask_id,
                        tool_call_id,
                        launch_group_id.clone(),
                    );
                }
            }
            for (tool_call, result, status) in results {
                let mut observation = ToolCallObservation::new(&tool_call, status);
                observation.observe_result(&result);
                outcome.tool_observations.push(observation);
                if let ToolOutput::SubagentStarted { subtask_id, .. } = &result {
                    outcome.detached_subagent_ids.push(subtask_id.clone());
                }
                self.apply_tool_result(
                    tool_call,
                    result,
                    status,
                    sink,
                    ToolResultBatchContext {
                        trusted_contexts: &mut trusted_contexts,
                        successful_rust_edits: &mut successful_rust_edits,
                        launch_group_id: launch_group_id.as_deref(),
                    },
                )
                .await?;
            }
            let bash_changed_paths = if let Some(baseline) = bash_baseline {
                match capture_worktree_snapshot_including(&self.project_root, &baseline.paths())
                    .await
                {
                    Some(current) => baseline.changed_paths(&current),
                    None => Vec::new(),
                }
            } else {
                Vec::new()
            };
            if !bash_changed_paths.is_empty() {
                sink.workspace_changed(
                    &bash_changed_paths,
                    "workspace changes observed after a successful bash command",
                );
                self.note_bash_window_verification_worthy_mutation(bash_changed_paths.clone());
                for observation in &mut outcome.tool_observations[observation_start..] {
                    if observation.tool_name == "bash" && observation.status.is_success() {
                        observation.makes_progress = true;
                    }
                }
                successful_rust_edits.extend(
                    bash_changed_paths
                        .iter()
                        .filter(|path| path_has_extension(path, "rs"))
                        .map(|path| self.project_root.join(path)),
                );
            }
            self.inject_new_lsp_diagnostics(diagnostic_baseline, &successful_rust_edits, sink)
                .await;
            self.emit_context_updated(sink);

            if let Some(reason) = batch_wait_reason {
                outcome.wait_reason = Some(reason);
                let skipped_calls = batches.by_ref().flatten().collect::<Vec<_>>();
                self.drain_skipped_calls(
                    skipped_calls,
                    "Tool skipped because the agent entered a nonterminal peer wait.",
                    sink,
                    &mut trusted_contexts,
                )
                .await?;
                self.emit_context_updated(sink);
                break;
            }

            if batches.peek().is_some() {
                let queued = queued_messages.next();
                if !queued.is_empty() {
                    let skipped_calls = batches.by_ref().flatten().collect::<Vec<_>>();
                    self.drain_skipped_calls(
                        skipped_calls,
                        "Tool skipped because a new queued user message took priority.",
                        sink,
                        &mut trusted_contexts,
                    )
                    .await?;
                    for message in queued {
                        self.self_review.append_request(&message.input.text);
                        self.push_live_user_message(&message.input).await?;
                        sink.queued_user_message_sent(message.id, &message.transcript_text);
                    }
                    outcome.reset_loop_guards = true;
                    self.emit_context_updated(sink);
                    break;
                }
            }
        }
        if !trusted_contexts.is_empty() {
            for content in trusted_contexts {
                self.push_message(trusted_context_message(&content));
            }
            self.emit_context_updated(sink);
        }
        outcome.interrupted_mid_tools = interrupted_mid_tools;
        Ok(outcome)
    }

    fn planning_research_action(
        &mut self,
        tool_calls: &[ToolCall],
        guard: &mut PlanningResearchGuard,
    ) -> Option<PlanningResearchAction> {
        if !self.persona_planning_budget() {
            return None;
        }
        if tool_calls
            .iter()
            .any(|tool_call| planning_tool_makes_progress(&tool_call.name))
        {
            guard.reset();
            self.set_planning_advisory(None);
            return None;
        }

        guard.turns_without_progress = guard.turns_without_progress.saturating_add(1);
        if guard.turns_without_progress == PLANNING_RESEARCH_TURN_LIMIT {
            self.set_planning_advisory(Some(planning_research_advisory_message()));
        }
        if guard.turns_without_progress <= PLANNING_RESEARCH_TURN_LIMIT {
            return None;
        }

        guard.rejections = guard.rejections.saturating_add(1);
        if guard.rejections > PLANNING_RESEARCH_REJECTION_LIMIT {
            return Some(PlanningResearchAction::Stop(
                planning_research_stop_message(),
            ));
        }
        Some(PlanningResearchAction::Reject(
            planning_research_limit_message(),
        ))
    }

    /// Best-effort debug transcript write. `BONSAI_TRANSCRIPT_LOG` is a debug
    /// aid, not a correctness dependency, so an I/O failure (disk full,
    /// permissions) logs a warning instead of aborting the live turn.
    pub(super) fn log_request(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[async_openai::types::chat::ChatCompletionTool],
    ) {
        let Some(logger) = &self.transcript_logger else {
            return;
        };

        if let Err(err) = logger.append(format!(
            "\n====================\n[{ts}] request\nmessages = {messages:#?}\ntools = {tools:#?}\n",
            ts = unix_timestamp_secs(),
        )) {
            tracing::warn!(%err, "transcript log_request failed");
        }
    }

    pub(super) fn log_response(&self, response: &StreamedResponse, interrupted: bool) {
        let Some(logger) = &self.transcript_logger else {
            return;
        };

        if let Err(err) = logger.append(format!(
            "[{ts}] response interrupted={interrupted}\ncontent = {content:#?}\ntool_calls = {tool_calls:#?}\n====================\n",
            ts = unix_timestamp_secs(),
            content = response.content.as_str(),
            tool_calls = response.tool_calls,
        )) {
            tracing::warn!(%err, "transcript log_response failed");
        }
    }

    /// One default-on `info!` line per model response, so a misbehaving run
    /// (observed live: 60 read-only turns over 47 minutes, visible only in the
    /// DB) can be spotted and diagnosed by tailing the session log file.
    /// `made_progress` is `None` on turns without tool calls.
    fn log_turn_line(
        &self,
        tool_calls: &[ToolCall],
        made_progress: Option<bool>,
        stall_turns: usize,
    ) {
        let turn = self.usage.usage_turns.last();
        let tools = tool_calls
            .iter()
            .map(|tool_call| tool_call.name.as_str())
            .collect::<Vec<_>>()
            .join(",");
        tracing::info!(
            target: "bonsai::turn",
            seq = turn.map(|turn| turn.seq),
            lane = %self.execution_lane.label(),
            model = turn.and_then(|turn| turn.model.as_deref()),
            prompt_tokens = turn.and_then(|turn| turn.prompt_tokens),
            completion_tokens = turn.and_then(|turn| turn.completion_tokens),
            latency_ms = turn.and_then(|turn| turn.latency_ms),
            ttft_ms = turn.and_then(|turn| turn.ttft_ms),
            tools = %tools,
            progress = made_progress,
            stall_turns,
            "turn"
        );
    }
}

/// The file paths a successful mutation call wrote. write/edit/rename_symbol
/// expose a single `path` arg (0 or 1 path); `apply_patch` carries many inside
/// its patch body, extracted by parsing it. Centralizes the single-vs-multi-file
/// split so the LSP-diagnostic hooks treat every mutation tool uniformly.
fn mutation_paths(tool_call: &ToolCall) -> Vec<String> {
    if tool_call.name == "apply_patch" {
        return crate::tool::patched_paths_from_arguments(&tool_call.arguments);
    }
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&tool_call.arguments) else {
        return Vec::new();
    };
    path_arg(&args).map(str::to_string).into_iter().collect()
}

fn path_has_extension(path: &str, extension: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext == extension)
}

fn planning_tool_makes_progress(name: &str) -> bool {
    name.starts_with("plan_") || name == "question"
}

fn tool_clears_repair_advisory(name: &str) -> bool {
    is_mutation_tool(name) || matches!(name, "bash" | "agent")
}

fn completed_tool_workspace_effect(
    tool_call: &ToolCall,
    result: &ToolOutput,
    typed_paths: Vec<String>,
) -> crate::tool::ToolWorkspaceEffect {
    if is_mutation_tool(&tool_call.name) {
        return if typed_paths.is_empty() {
            crate::tool::ToolWorkspaceEffect::Unscoped
        } else {
            crate::tool::ToolWorkspaceEffect::ScopedMutation(typed_paths)
        };
    }
    match result.workspace_effect() {
        Some(effect) => effect.clone(),
        // Preserve the conservative legacy fallback for a synthetic/older
        // `agent` implementation that does not report its resolved effect.
        None if tool_call.name == "agent" => crate::tool::ToolWorkspaceEffect::Unscoped,
        None => crate::tool::ToolWorkspaceEffect::NoMutation,
    }
}

fn refine_delegated_workspace_effects(
    results: &mut [(ToolCall, ToolOutput, crate::output::ToolExecutionStatus)],
    observed_paths: Option<&[String]>,
) {
    let Some(observed_paths) = observed_paths else {
        return;
    };
    let mut unscoped = results
        .iter_mut()
        .filter(|(_, result, _)| {
            matches!(
                result.workspace_effect(),
                Some(crate::tool::ToolWorkspaceEffect::Unscoped)
            )
        })
        .collect::<Vec<_>>();
    if unscoped.len() == 1 {
        let effect = if observed_paths.is_empty() {
            crate::tool::ToolWorkspaceEffect::NoMutation
        } else {
            crate::tool::ToolWorkspaceEffect::WindowMutation(observed_paths.to_vec())
        };
        unscoped[0].1.set_workspace_effect(effect);
    }
}

fn foreground_bash_call(tool_call: &ToolCall) -> bool {
    if tool_call.name != "bash" {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&tool_call.arguments)
        .ok()
        .and_then(|args| {
            args.get("run_in_background")
                .and_then(serde_json::Value::as_bool)
        })
        != Some(true)
}

/// A successful command's output below this many chars always reaches the
/// model verbatim. Withholding output is justified by SIZE alone — a command's
/// stdout is its return value, the evidence the model ran it for. The old
/// command-name allowlist stubbed even `rustc --version` (observed live: the
/// model burned eight turns smuggling the version string through xxd, /tmp,
/// and a scratch file, then stopped trusting bash entirely). Deliberate, don't
/// simplify back to name-based hiding. The threshold must stay generous enough
/// that routine check/build output passes whole — comparable coding agents keep
/// far more than this before truncating.
const COMMAND_OUTPUT_KEEP_FULL_CHARS: usize = 10_000;
const COMMAND_OUTPUT_EXCERPT_HEAD_CHARS: usize = 4_000;
const COMMAND_OUTPUT_EXCERPT_TAIL_CHARS: usize = 4_000;

/// Model-context policy for a successful bash command's output: keep small
/// outputs whole; excerpt genuinely large ones to verbatim head + tail slices
/// around an explicit elision marker (build logs put errors at the end and
/// banners at the start, so both edges carry signal). The `[Command summary]`
/// footer survives untouched. The marker names the on-disk spill path when the
/// bash tool saved one — a location the MODEL can read — never a UI-only
/// surface like the transcript card.
fn compact_successful_command_model_content(
    tool_call: &ToolCall,
    result: &ToolOutput,
    rendered: &str,
    success: bool,
) -> Option<String> {
    let ToolOutput::Command {
        exit_code,
        timed_out,
        truncation,
        ..
    } = result
    else {
        return None;
    };
    if !success || tool_call.name != "bash" || *timed_out || *exit_code != Some(0) {
        return None;
    }
    // Excerpt only the output body; the summary footer stays verbatim so exit
    // code, byte counts, and last_output remain visible.
    let (body, footer) = match rendered.rsplit_once("[Command summary]") {
        Some((body, footer)) => (body, Some(footer)),
        None => (rendered, None),
    };
    if body.chars().count() <= COMMAND_OUTPUT_KEEP_FULL_CHARS {
        return None;
    }
    let command = tool_argument_string(&tool_call.arguments, "command").unwrap_or_default();
    if bash_command_should_keep_output(&command) {
        return None;
    }

    let head = leading_line_chunk(body, COMMAND_OUTPUT_EXCERPT_HEAD_CHARS);
    let tail = trailing_line_chunk(&body[head.len()..], COMMAND_OUTPUT_EXCERPT_TAIL_CHARS);
    let omitted = body
        .chars()
        .count()
        .saturating_sub(head.chars().count())
        .saturating_sub(tail.chars().count());
    if omitted == 0 {
        return None;
    }
    let retrieval = match truncation {
        Some(truncation) => format!("full output saved to: {}", truncation.path),
        None => {
            "the elided middle was not retained — re-run piped through grep/head/tail if you need it"
                .to_string()
        }
    };
    let mut compacted = format!(
        "{}\n[… {omitted} of {total} output chars elided (command succeeded, exit 0); {retrieval}]\n{}",
        head.trim_end_matches('\n'),
        tail.trim_start_matches('\n'),
        total = body.chars().count(),
    );
    if let Some(footer) = footer {
        compacted.push_str("[Command summary]");
        compacted.push_str(footer);
    }
    Some(compacted)
}

/// The longest prefix of `text` within `max_chars`, rounded down to a whole
/// line when a newline is available so the excerpt never ends mid-line.
fn leading_line_chunk(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => {
            let slice = &text[..byte_idx];
            match slice.rfind('\n') {
                Some(newline) if newline > 0 => &slice[..newline],
                _ => slice,
            }
        }
        None => text,
    }
}

/// The longest suffix of `text` within `max_chars`, rounded forward to a whole
/// line when a newline is available so the excerpt never starts mid-line.
fn trailing_line_chunk(text: &str, max_chars: usize) -> &str {
    let total = text.chars().count();
    if total <= max_chars {
        return text;
    }
    let start = text
        .char_indices()
        .nth(total - max_chars)
        .map(|(byte_idx, _)| byte_idx)
        .unwrap_or(0);
    let slice = &text[start..];
    match slice.find('\n') {
        Some(newline) if newline + 1 < slice.len() => &slice[newline + 1..],
        _ => slice,
    }
}

fn bash_command_should_keep_output(command: &str) -> bool {
    let mut tokens = command
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| matches!(ch, '(' | ')' | ';')));
    let Some(first) = tokens.next() else {
        return false;
    };
    // Inspection commands whose stdout *is* the evidence the model just gathered.
    // Compacting these away only pushes the model to re-run them next turn — the
    // very inspection churn the compaction exists to reduce. Interpreter/query
    // one-liners (DB reads, `jq`) are output-as-payload; so are read-only git
    // subcommands (`git status`/`diff`/`show`/`log`), which models routinely run
    // through bash instead of the structured `git` tool.
    if matches!(
        first,
        "cat"
            | "head"
            | "tail"
            | "sed"
            | "awk"
            | "grep"
            | "rg"
            | "ls"
            | "find"
            | "pwd"
            | "python"
            | "python3"
            | "jq"
            | "sqlite3"
    ) {
        return true;
    }
    if first == "git" {
        return matches!(
            tokens.next(),
            Some("status" | "diff" | "show" | "log" | "blame")
        );
    }
    if first == "cargo" {
        return matches!(tokens.next(), Some("test"));
    }
    false
}

fn repair_advisory_for_tool_result(
    tool_call: &ToolCall,
    result: &ToolOutput,
    success: bool,
) -> Option<String> {
    match result {
        ToolOutput::Command {
            rendered,
            exit_code,
            timed_out,
            ..
        } if *timed_out || matches!(exit_code, Some(code) if *code != 0) => Some(
            format_command_repair_advisory(tool_call, rendered, *exit_code, *timed_out),
        ),
        ToolOutput::Text(text)
            if !success && matches!(tool_call.name.as_str(), "edit" | "write" | "bash") =>
        {
            Some(format_text_repair_advisory(tool_call, text))
        }
        _ => None,
    }
}

fn format_command_repair_advisory(
    tool_call: &ToolCall,
    rendered: &str,
    exit_code: Option<i32>,
    timed_out: bool,
) -> String {
    let mut lines = vec![
        "### Current repair target".to_string(),
        format!("- Last failed tool: {}", tool_call.name),
    ];
    if let Some(command) = tool_argument_string(&tool_call.arguments, "command") {
        lines.push(format!("- Command: {}", compact_repair_line(&command)));
    }
    lines.push(format!(
        "- Exit: {}",
        exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "none".to_string())
    ));
    lines.push(format!("- Timed out: {timed_out}"));
    append_repair_excerpt(&mut lines, rendered);
    lines.push("- Next action: fix this specific failure with edit/write or a targeted command, then rerun the check. Do not refresh unchanged reads first.".to_string());
    lines.join("\n")
}

fn format_text_repair_advisory(tool_call: &ToolCall, text: &str) -> String {
    let mut lines = vec![
        "### Current repair target".to_string(),
        format!("- Last failed tool: {}", tool_call.name),
    ];
    if let Some(path) = tool_argument_string(&tool_call.arguments, "path") {
        lines.push(format!("- Path: {}", compact_repair_line(&path)));
    }
    append_repair_excerpt(&mut lines, text);
    lines.push("- Next action: correct the failing edit/write call or provide the final answer if blocked. Do not refresh unchanged reads first.".to_string());
    lines.join("\n")
}

fn append_repair_excerpt(lines: &mut Vec<String>, text: &str) {
    let excerpt = diagnostic_excerpt_lines(text, REPAIR_ADVISORY_LINES, REPAIR_ADVISORY_LINE_CHARS);
    if excerpt.is_empty() {
        return;
    }
    lines.push("- Diagnostic:".to_string());
    for line in excerpt {
        lines.push(format!("  {line}"));
    }
}

fn compact_repair_line(text: &str) -> String {
    const MAX_CHARS: usize = 220;

    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX_CHARS {
        return compact;
    }
    let mut truncated = compact.chars().take(MAX_CHARS).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn tool_argument_string(arguments: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| {
            value
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

/// A hook execution warning (a timeout, a nonzero exit, an ignored
/// out-of-contract decision) is visible but never fatal — surfaced as a
/// status line rather than folded into the tool result.
fn emit_hook_warnings(sink: &SharedSink, warnings: &[String]) {
    for warning in warnings {
        sink.status(&format!("[hooks] {warning}"));
    }
}

fn planning_research_limit_message() -> String {
    format!(
        "Error: planning research budget reached after {PLANNING_RESEARCH_TURN_LIMIT} tool-call turns without a canvas update. Stop calling read, grep, project_info, bash, or other inspection tools. If a consequential choice is unresolved, ask the user now with the question tool. Otherwise, write the complete initial canvas with plan_replace_draft and state any non-consequential assumption in the plan. Never put open questions on the canvas."
    )
}

fn planning_research_stop_message() -> String {
    "Agent stopped after the planning research limit: the model repeated non-progress planning tools after Bonsai rejected the over-budget turn. Ask the user with the question tool, update the plan canvas with no open questions, or start a new turn with narrower instructions."
        .to_string()
}

fn planning_research_advisory_message() -> String {
    format!(
        "### Planning decision required\n- Planning research has reached the {PLANNING_RESEARCH_TURN_LIMIT}-turn budget without a canvas update.\n- Do not inspect further. If a consequential choice is unresolved, ask the user now with the question tool.\n- Otherwise, call plan_replace_draft once with the title, sections, and complete tasks or phases. State any non-consequential assumption in a section; never put open questions on the canvas."
    )
}

#[derive(Debug, Default)]
struct PlanningResearchGuard {
    turns_without_progress: usize,
    rejections: usize,
}

impl PlanningResearchGuard {
    fn reset(&mut self) {
        self.turns_without_progress = 0;
        self.rejections = 0;
    }
}

/// One guard's verdict on a turn's tool calls: `Reject` answers the offending
/// calls with synthetic results (the payload — a message, target set, or
/// per-call rejection map — varies by guard) and lets the run continue; `Stop`
/// terminates the run with a message. Every guard shares this shape so they all
/// resolve identically through [`resolve_guard`].
enum GuardAction<T> {
    Reject(T),
    Stop(String),
}

/// Resolve one guard's action. A `Reject` warns and hands its payload back for
/// the caller to answer the offending calls; a `Stop` warns, logs the discarded
/// response, and bails the run; `None` passes through. Centralized so no guard
/// path can forget the warn / `log_response` / bail ritual.
fn resolve_guard<T>(
    agent: &Agent,
    name: &'static str,
    action: Option<GuardAction<T>>,
    response: &StreamedResponse,
) -> Result<Option<T>> {
    match action {
        Some(GuardAction::Reject(payload)) => {
            tracing::warn!(target: "bonsai::guard", guard = name, action = "reject", "guard rejected tool calls");
            Ok(Some(payload))
        }
        Some(GuardAction::Stop(message)) => {
            tracing::warn!(target: "bonsai::guard", guard = name, action = "stop", "guard stopped run");
            agent.log_response(response, false);
            bail!("{message}")
        }
        None => Ok(None),
    }
}

type PlanningResearchAction = GuardAction<String>;

#[derive(Debug, Clone, Default)]
struct ToolRejections {
    /// Fresh interval coverage resolved before execution. These calls return a
    /// successful compact pointer without touching the filesystem again.
    precomputed_read: HashMap<String, PrecomputedReadReuse>,
    /// Partially covered reads rewritten to their one uncovered interval.
    precomputed_read_delta: HashMap<String, PrecomputedReadDelta>,
    /// Known slow ad-hoc verification calls promoted off the foreground lane.
    auto_background: HashSet<String>,
    planning_research: Option<String>,
    /// One-time guidance for broad parent reads that duplicate full-file work
    /// just completed by a child. A retry executes normally so the parent can
    /// still obtain edit authorization when it genuinely needs the full body.
    delegated_read: HashMap<String, String>,
    repeated_inspection: Option<String>,
    /// Read targets that stormed this turn. A call is rejected only when its own
    /// target is in the set, so sibling reads of other files pass through.
    read_storm: Option<HashSet<String>>,
    /// Identical calls that already failed twice. Keyed by call id so a fresh
    /// sibling in the same batch still executes.
    repeated_failure: HashMap<String, String>,
}

impl ToolRejections {
    fn message_for(&self, tool_call: &ToolCall) -> Option<String> {
        if let Some(message) = self.planning_research.as_deref()
            && !planning_tool_makes_progress(&tool_call.name)
        {
            return Some(message.to_string());
        }

        if let Some(message) = self.delegated_read.get(&tool_call.id) {
            return Some(message.clone());
        }

        if let Some(stormed) = self.read_storm.as_ref()
            && let Some(target) = read_storm_target(tool_call)
            && stormed.contains(&target)
        {
            return Some(read_storm_loop_message(&target));
        }

        if let Some(message) = self.repeated_failure.get(&tool_call.id) {
            return Some(message.clone());
        }

        self.repeated_inspection.as_deref().map(str::to_string)
    }
}

impl Agent {
    fn auto_background_verification_calls(&self, tool_calls: &[ToolCall]) -> HashSet<String> {
        if self.verification.active_verification.is_some() {
            return HashSet::new();
        }
        tool_calls
            .iter()
            .filter(|call| call.name == "bash")
            .filter_map(|call| {
                let value = serde_json::from_str::<serde_json::Value>(&call.arguments).ok()?;
                let command = value.get("command")?.as_str()?;
                known_slow_verification(command).then(|| call.id.clone())
            })
            .collect()
    }

    pub(in crate::agent) fn delegated_read_rejections(
        &mut self,
        tool_calls: &[ToolCall],
    ) -> HashMap<String, String> {
        let mut rejections = HashMap::new();
        for tool_call in tool_calls {
            let Some((canonical_path, display_path)) =
                broad_read_target(tool_call, &self.project_root)
            else {
                continue;
            };
            let mut subtasks = self
                .read_evidence
                .delegated_read_evidence
                .iter()
                .filter(|delegated| {
                    let evidence = &delegated.evidence;
                    evidence.observation_is_current()
                        && evidence.observation().coverage() == crate::tool::ReadCoverage::Full
                        && evidence.observation().canonical_path() == canonical_path
                })
                .map(|delegated| delegated.subtask_id.as_str())
                .collect::<Vec<_>>();
            if subtasks.is_empty() {
                continue;
            }
            subtasks.sort_unstable();
            subtasks.dedup();
            let advice_key = format!("{}:{}", canonical_path.display(), subtasks.join(","));
            if !self
                .read_evidence
                .delegated_overlap_advised
                .insert(advice_key)
            {
                continue;
            }
            self.usage
                .record_delegated_parent_overlap(&self.execution_lane);
            tracing::info!(
                target: "bonsai::delegation",
                path = %display_path,
                subtasks = ?subtasks,
                "deferred broad parent reread already covered by delegation",
            );
            rejections.insert(
                tool_call.id.clone(),
                format!(
                    "Error: broad parent reread deferred for {display_path}: completed delegation {} already observed the full file. Use its report, or read a narrow cited/risky range with read_region/read_symbol. Retry this broad read only when you need the complete source in parent context or must authorize an edit.",
                    subtasks.join(", ")
                ),
            );
        }
        rejections
    }

    pub(in crate::agent) fn read_follows_compact_reuse(&self, tool_call: &ToolCall) -> bool {
        if self
            .current_covering_compact_reuse_target(tool_call)
            .is_some()
        {
            return true;
        }
        if self.most_recent_live_structured_read_is_reuse(tool_call) {
            return true;
        }
        self.read_evidence
            .inspection_events
            .iter()
            .any(|(call_id, admission)| {
                if admission.outcome != InspectionOutcome::Reused {
                    return false;
                }
                if !self.tool_result_is_live(call_id) {
                    return false;
                }
                let Some(prior_detail) = self.tool_context_details.get(call_id) else {
                    return false;
                };
                if prior_detail.name != tool_call.name
                    || normalize_tool_call_arguments_json(&prior_detail.arguments)
                        != normalize_tool_call_arguments_json(&tool_call.arguments)
                {
                    return false;
                }
                let Some(target_call_id) = admission.reuse_target_tool_call_id.as_deref() else {
                    return false;
                };
                if !self.tool_result_is_live(target_call_id) {
                    return false;
                }
                if tool_call.name == "bash" {
                    // Bash read details do not carry typed ReadEvidence. The prior
                    // Reused event itself proves that this exact clean cat/head/tail
                    // call matched a live full result; execution has already
                    // produced current bytes, so return them on the explicit retry.
                    return true;
                }
                self.tool_context_details
                    .get(target_call_id)
                    .and_then(|detail| detail.read_evidence.as_ref())
                    .is_some_and(ReadEvidence::observation_is_current)
            })
    }

    fn most_recent_live_structured_read_is_reuse(&self, tool_call: &ToolCall) -> bool {
        if !structured_file_read_tool(&tool_call.name) {
            return false;
        }
        let Some((requested_target, _path)) = file_read_target(tool_call) else {
            return false;
        };
        self.messages
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(index, message)| {
                let call_id = tool_message_call_id(message)?;
                if self.message_has_control(index, message, |state| {
                    state.stubbed || state.drop_next_turn
                }) {
                    return None;
                }
                let detail = self.tool_context_details.get(&call_id)?;
                if !structured_file_read_tool(&detail.name) {
                    return None;
                }
                let prior = ToolCall {
                    id: call_id,
                    name: detail.name.clone(),
                    arguments: detail.arguments.clone(),
                };
                let (target, _path) = file_read_target(&prior)?;
                (target == requested_target).then_some(detail)
            })
            .next()
            .is_some_and(|detail| {
                let Some(target_call_id) = detail.reuse_target_call_id.as_deref() else {
                    return false;
                };
                self.tool_result_is_live(target_call_id)
                    && self
                        .tool_context_details
                        .get(target_call_id)
                        .and_then(|target| target.read_evidence.as_ref())
                        .is_some_and(ReadEvidence::observation_is_current)
            })
    }

    fn tool_result_is_live(&self, call_id: &str) -> bool {
        self.messages.iter().enumerate().any(|(index, message)| {
            tool_message_call_id(message).as_deref() == Some(call_id)
                && !self.message_has_control(index, message, |state| {
                    state.stubbed || state.drop_next_turn
                })
        })
    }

    async fn read_target_versions(&self, tool_calls: &[ToolCall]) -> ReadTargetVersions {
        // A path alone is not a loop identity. Concurrent edits can make the
        // same read arguments the correct next action, so only arm the guards
        // when the last read still describes the live on-disk bytes. Check at
        // guard time rather than trusting preflight freshness: the file can
        // change while the model request is in flight.
        let mut versions = ReadTargetVersions::default();
        let resolver = crate::tool::ProjectPathResolver::new(&self.project_root).action("read");
        let mut paths_by_target = HashMap::new();
        for tool_call in tool_calls {
            let Some((target, path)) = file_read_target(tool_call) else {
                continue;
            };
            let resolved = match resolver.resolve_existing(&path) {
                Ok(resolved) => resolved,
                Err(_) if tool_call.name == "grep" => {
                    versions.excluded_targets.insert(target);
                    continue;
                }
                Err(_) => {
                    versions.file_targets.insert(target);
                    continue;
                }
            };
            if tool_call.name == "grep" && !resolved.canonical_path().is_file() {
                versions.excluded_targets.insert(target);
                continue;
            }
            versions.file_targets.insert(target.clone());
            paths_by_target.insert(target, resolved.canonical_path().to_path_buf());
        }
        let checks = paths_by_target
            .into_iter()
            .map(|(target, path)| async move {
                let unchanged = self.read_tracker.is_unchanged_since_read(&path).await;
                (target, unchanged)
            });
        for (target, unchanged) in futures::future::join_all(checks).await {
            if unchanged {
                versions.unchanged_targets.insert(target);
            }
        }
        versions
    }
}

fn known_slow_verification(command: &str) -> bool {
    let normalized = command.split_whitespace().collect::<Vec<_>>();
    matches!(normalized.as_slice(), ["cargo", "test", ..])
        || (matches!(normalized.as_slice(), ["cargo", "clippy", ..])
            && normalized.contains(&"--all-targets"))
        || (matches!(normalized.as_slice(), ["cargo", "build", ..])
            && normalized.contains(&"--release"))
}

fn broad_read_target(tool_call: &ToolCall, project_root: &Path) -> Option<(PathBuf, String)> {
    if tool_call.name != "read" {
        return None;
    }
    let arguments = serde_json::from_str::<serde_json::Value>(&tool_call.arguments).ok()?;
    let path = arguments.get("path")?.as_str()?;
    let explicit_narrow_range = arguments
        .get("limit")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|limit| limit <= 200);
    if explicit_narrow_range {
        return None;
    }
    let resolved = crate::tool::ProjectPathResolver::new(project_root)
        .action("read")
        .resolve_existing(path)
        .ok()?;
    Some((resolved.canonical_path().to_path_buf(), path.to_string()))
}

#[derive(Debug, Default)]
struct RepeatedFailureGuard {
    turn: usize,
    failures: HashMap<String, FailedCallState>,
}

#[derive(Debug, Default)]
struct FailedCallState {
    failures: usize,
    rejections: usize,
    last_seen_turn: usize,
}

type RepeatedFailureAction = GuardAction<HashMap<String, String>>;

impl RepeatedFailureGuard {
    fn action_for(&mut self, tool_calls: &[ToolCall]) -> Option<RepeatedFailureAction> {
        self.turn = self.turn.saturating_add(1);
        self.failures.retain(|_, state| {
            self.turn.saturating_sub(state.last_seen_turn) <= FAILED_CALL_WINDOW
        });

        // A repair and its verification often belong in one model turn. Let
        // that batch run; a successful mutation clears stale failure state,
        // while a failed repair leaves the guard armed for the next turn.
        let has_new_progress_attempt = tool_calls.iter().any(|tool_call| {
            if !failed_call_progress_candidate(tool_call) {
                return false;
            }
            let signature = tool_call_failure_signature(tool_call);
            self.failures
                .get(&signature)
                .is_none_or(|state| state.failures < REPEATED_FAILED_CALL_LIMIT)
        });
        if has_new_progress_attempt {
            return None;
        }

        let mut rejected = HashMap::new();
        let mut rejected_signatures = HashSet::new();
        for tool_call in tool_calls {
            let signature = tool_call_failure_signature(tool_call);
            let Some(state) = self.failures.get_mut(&signature) else {
                continue;
            };
            if state.failures < REPEATED_FAILED_CALL_LIMIT {
                continue;
            }
            if state.rejections >= REPEATED_FAILED_CALL_REJECTION_LIMIT {
                return Some(RepeatedFailureAction::Stop(
                    repeated_failed_call_stop_message(&tool_call.name),
                ));
            }
            if rejected_signatures.insert(signature) {
                state.rejections = state.rejections.saturating_add(1);
                state.last_seen_turn = self.turn;
            }
            rejected.insert(
                tool_call.id.clone(),
                repeated_failed_call_message(&tool_call.name),
            );
        }

        (!rejected.is_empty()).then_some(RepeatedFailureAction::Reject(rejected))
    }

    fn observe(&mut self, observations: &[ToolCallObservation], reset: bool) {
        for observation in observations {
            if observation.makes_progress {
                self.failures.clear();
            }
            match observation.status {
                crate::output::ToolExecutionStatus::Succeeded => {
                    self.failures.remove(&observation.signature);
                }
                crate::output::ToolExecutionStatus::Failed => {
                    let state = self
                        .failures
                        .entry(observation.signature.clone())
                        .or_default();
                    state.failures = state.failures.saturating_add(1);
                    state.last_seen_turn = self.turn;
                }
                crate::output::ToolExecutionStatus::Started
                | crate::output::ToolExecutionStatus::Skipped
                | crate::output::ToolExecutionStatus::Interrupted => {}
            }
        }
        if reset {
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.turn = 0;
        self.failures.clear();
    }
}

impl ToolCallObservation {
    fn new(tool_call: &ToolCall, status: crate::output::ToolExecutionStatus) -> Self {
        Self {
            signature: tool_call_failure_signature(tool_call),
            tool_name: tool_call.name.clone(),
            status,
            makes_progress: status.is_success() && failed_call_progress_candidate(tool_call),
        }
    }

    fn observe_result(&mut self, result: &ToolOutput) {
        if matches!(result, ToolOutput::BackgroundTaskStarted { .. }) {
            self.makes_progress = true;
        }
    }
}

fn tool_call_failure_signature(tool_call: &ToolCall) -> String {
    format!(
        "{}:{}",
        tool_call.name,
        normalized_tool_arguments_for_signature(&tool_call.arguments)
    )
}

fn failed_call_progress_candidate(tool_call: &ToolCall) -> bool {
    failed_call_progress_tool_name(&tool_call.name)
        || (tool_call.name == "bash" && bash_file_read_target(tool_call).is_none())
}

fn failed_call_progress_tool_name(name: &str) -> bool {
    is_mutation_tool(name) || name == "agent" || planning_tool_makes_progress(name)
}

fn repeated_failed_call_message(name: &str) -> String {
    format!(
        "Error: identical `{name}` tool call already failed {REPEATED_FAILED_CALL_LIMIT} times. The prior error is still in context. Do not submit the same call again; change its arguments or approach, repair the cause, ask a focused question if blocked, or finish with the limitation."
    )
}

fn repeated_failed_call_stop_message(name: &str) -> String {
    format!(
        "Agent stopped after an identical `{name}` tool-call failure loop: the model repeated the same call after Bonsai returned a bounded-retry rejection."
    )
}

#[derive(Default)]
struct RepeatedInspectionGuard {
    last_signature: Option<String>,
    turns: usize,
    repair_read_only_rejections: usize,
    /// Recent inspection-only turn signatures (oldest first), capped at
    /// [`INSPECTION_WINDOW`]. Lets an alternating loop trip the counter, not
    /// only byte-identical consecutive repeats.
    recent: std::collections::VecDeque<String>,
}

/// Outcome of a read-storm check for one turn. Carries the *set* of stormed
/// targets rather than a single pre-rendered message so the rejection can be
/// matched per tool call: a batch that pairs an over-read file with a fresh one
/// blocks only the over-read target and names *that* file, instead of painting
/// the innocent sibling with the wrong file's storm message.
type ReadStormAction = GuardAction<HashSet<String>>;

#[derive(Default)]
struct ReadStormGuard {
    turns_by_target: HashMap<String, usize>,
    rejections_by_target: HashMap<String, usize>,
}

#[derive(Debug, Default)]
struct ReadTargetVersions {
    file_targets: HashSet<String>,
    unchanged_targets: HashSet<String>,
    excluded_targets: HashSet<String>,
}

impl ReadTargetVersions {
    fn has_changed_or_untracked_file(&self) -> bool {
        self.file_targets
            .iter()
            .any(|target| !self.unchanged_targets.contains(target))
    }

    #[cfg(test)]
    fn assume_unchanged(tool_calls: &[ToolCall]) -> Self {
        let file_targets = tool_calls
            .iter()
            .filter_map(file_read_target)
            .map(|(target, _)| target)
            .collect::<HashSet<_>>();
        Self {
            unchanged_targets: file_targets.clone(),
            file_targets,
            excluded_targets: HashSet::new(),
        }
    }
}

impl ReadStormGuard {
    #[cfg(test)]
    fn action_for(&mut self, tool_calls: &[ToolCall]) -> Option<ReadStormAction> {
        let versions = ReadTargetVersions::assume_unchanged(tool_calls);
        self.action_for_with_versions(tool_calls, &versions)
    }

    fn action_for_with_versions(
        &mut self,
        tool_calls: &[ToolCall],
        versions: &ReadTargetVersions,
    ) -> Option<ReadStormAction> {
        if tool_calls
            .iter()
            .any(|tool_call| is_mutation_tool(&tool_call.name))
        {
            self.turns_by_target.clear();
            self.rejections_by_target.clear();
            return None;
        }

        let targets = read_storm_targets(tool_calls)
            .difference(&versions.excluded_targets)
            .cloned()
            .collect::<HashSet<_>>();
        self.decay_targets_not_seen(&targets);
        if targets.is_empty() {
            return None;
        }

        // Count every read target this turn, then collect the ones that crossed
        // the limit. Rejecting each over-limit target on its own (not just the
        // single worst) keeps innocent sibling reads in the same batch flowing
        // and lets every rejection name its own file.
        let mut over_limit: Vec<(String, usize)> = Vec::new();
        for target in targets {
            if versions.file_targets.contains(&target)
                && !versions.unchanged_targets.contains(&target)
            {
                self.turns_by_target.insert(target.clone(), 1);
                self.rejections_by_target.remove(&target);
                continue;
            }
            let turns = self
                .turns_by_target
                .entry(target.clone())
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
            if *turns > READ_STORM_TARGET_TURN_LIMIT {
                over_limit.push((target, *turns));
            }
        }
        if over_limit.is_empty() {
            return None;
        }

        let mut stormed = HashSet::new();
        let mut stop_target: Option<String> = None;
        for (target, turns) in over_limit {
            let rejections = self
                .rejections_by_target
                .entry(target.clone())
                .and_modify(|count| *count = count.saturating_add(1))
                .or_insert(1);
            if turns > READ_STORM_TARGET_TURN_LIMIT + READ_STORM_REJECTION_LIMIT
                || *rejections > READ_STORM_REJECTION_LIMIT
            {
                stop_target.get_or_insert_with(|| target.clone());
            }
            // A rejected call returned no file evidence, so do not let it push
            // the target permanently above the admission limit. An immediate
            // repeat still stops through `rejections_by_target`; a turn spent
            // following the redirect may decay the target and make a later
            // focused read admissible again.
            self.turns_by_target
                .insert(target.clone(), READ_STORM_TARGET_TURN_LIMIT);
            stormed.insert(target);
        }
        if let Some(target) = stop_target {
            return Some(ReadStormAction::Stop(read_storm_stop_message(&target)));
        }
        Some(ReadStormAction::Reject(stormed))
    }

    fn decay_targets_not_seen(&mut self, targets: &HashSet<String>) {
        let stale = self
            .turns_by_target
            .keys()
            .filter(|target| !targets.contains(*target))
            .cloned()
            .collect::<Vec<_>>();
        for target in stale {
            // A different inspection is compliance with the rejection's
            // redirect, so a later return to this target starts a new strike.
            // This keeps terminal stops for models that immediately repeat the
            // rejected call rather than punishing a corrected approach.
            self.rejections_by_target.remove(&target);
            if let Some(count) = self.turns_by_target.get_mut(&target) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.turns_by_target.remove(&target);
                }
            }
        }
    }

    fn reset(&mut self) {
        self.turns_by_target.clear();
        self.rejections_by_target.clear();
    }
}

fn read_storm_targets(tool_calls: &[ToolCall]) -> HashSet<String> {
    tool_calls
        .iter()
        .filter_map(read_storm_target)
        .collect::<HashSet<_>>()
}

fn read_storm_target(tool_call: &ToolCall) -> Option<String> {
    match tool_call.name.as_str() {
        name if structured_file_read_tool(name) => {
            tool_argument_string(&tool_call.arguments, "path")
                .map(|path| format!("read {}", normalize_inspection_path(&path)))
        }
        "grep" => tool_argument_string(&tool_call.arguments, "path")
            .map(|path| format!("read {}", normalize_inspection_path(&path))),
        "bash" => bash_file_read_target(tool_call),
        "git" => {
            let op = tool_argument_string(&tool_call.arguments, "op")?;
            if op != "diff" && op != "show" {
                return None;
            }
            tool_argument_string(&tool_call.arguments, "path")
                .map(|path| format!("git {op} {}", normalize_inspection_path(&path)))
        }
        _ => None,
    }
}

fn file_read_target(tool_call: &ToolCall) -> Option<(String, String)> {
    if structured_file_read_tool(&tool_call.name) || tool_call.name == "grep" {
        let path = tool_argument_string(&tool_call.arguments, "path")?;
        return Some((format!("read {}", normalize_inspection_path(&path)), path));
    }
    let path = bash_file_read_path(tool_call)?;
    Some((format!("read {}", normalize_inspection_path(&path)), path))
}

fn structured_file_read_tool(name: &str) -> bool {
    matches!(name, "read" | "read_region" | "read_symbol")
}

fn bash_file_read_target(tool_call: &ToolCall) -> Option<String> {
    bash_file_read_path(tool_call).map(|path| format!("read {}", normalize_inspection_path(&path)))
}

fn bash_file_read_path(tool_call: &ToolCall) -> Option<String> {
    if tool_call.name != "bash" {
        return None;
    }
    let command = tool_argument_string(&tool_call.arguments, "command")?;
    crate::tool::single_read_path(&command, "")
}

fn file_read_inspection_call(tool_call: &ToolCall) -> bool {
    structured_file_read_tool(&tool_call.name) || bash_file_read_target(tool_call).is_some()
}

fn normalize_inspection_path(path: &str) -> String {
    let trimmed = path.trim();
    trimmed.strip_prefix("./").unwrap_or(trimmed).to_string()
}

impl RepeatedInspectionGuard {
    #[cfg(test)]
    fn action_for(
        &mut self,
        tool_calls: &[ToolCall],
        repair_advisory_active: bool,
    ) -> Option<RepeatedInspectionAction> {
        let versions = ReadTargetVersions::assume_unchanged(tool_calls);
        self.action_for_with_versions(tool_calls, repair_advisory_active, &versions)
    }

    fn action_for_with_versions(
        &mut self,
        tool_calls: &[ToolCall],
        repair_advisory_active: bool,
        versions: &ReadTargetVersions,
    ) -> Option<RepeatedInspectionAction> {
        if versions.has_changed_or_untracked_file() {
            self.reset();
            return None;
        }
        let Some(signature) = repeated_inspection_signature(tool_calls) else {
            self.reset();
            return None;
        };
        let turn_limit = repeated_inspection_turn_limit(tool_calls);

        if repair_advisory_active {
            // A *fresh* inspection while repairing is legitimate recovery — a
            // failed build points at files the model has to read before it can
            // fix them. Rejecting those reads was observed to
            // push models into bypassing the read tool with bash `cat`. Only
            // repeating the *same* inspection while the failure summary is
            // already in context is a loop worth breaking.
            if self.last_signature.as_deref() != Some(signature.as_str()) {
                self.last_signature = Some(signature);
                self.turns = 1;
                self.repair_read_only_rejections = 0;
                return None;
            }
            self.repair_read_only_rejections = self.repair_read_only_rejections.saturating_add(1);
            if self.repair_read_only_rejections > REPEATED_INSPECTION_REJECTION_LIMIT {
                return Some(RepeatedInspectionAction::Stop(
                    repair_read_only_stop_message(),
                ));
            }
            return Some(RepeatedInspectionAction::Reject(
                repair_read_only_loop_message(),
            ));
        }

        self.repair_read_only_rejections = 0;
        // A "repeat" is either the same batch as last turn, or a batch the model
        // cycled back to within the recent window — so read↔git alternation
        // counts, not only byte-identical consecutive turns. A
        // genuinely new inspection (exploring a fresh file) resets the counter,
        // so legitimate research never trips.
        let is_repeat = self.last_signature.as_deref() == Some(signature.as_str())
            || self.recent.iter().any(|prev| prev == &signature);
        if is_repeat {
            self.turns = self.turns.saturating_add(1);
        } else {
            self.turns = 1;
        }
        self.last_signature = Some(signature.clone());
        self.record_recent(signature);

        if self.turns > turn_limit + REPEATED_INSPECTION_REJECTION_LIMIT {
            return Some(RepeatedInspectionAction::Stop(
                repeated_inspection_stop_message(turn_limit),
            ));
        }

        (self.turns > turn_limit)
            .then(|| RepeatedInspectionAction::Reject(repeated_inspection_loop_message(turn_limit)))
    }

    fn reset(&mut self) {
        self.last_signature = None;
        self.turns = 0;
        self.repair_read_only_rejections = 0;
        self.recent.clear();
    }

    fn record_recent(&mut self, signature: String) {
        if self.recent.len() >= INSPECTION_WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(signature);
    }
}

type RepeatedInspectionAction = GuardAction<String>;

/// Nudges the model to batch independent tool calls, at the point of failure.
/// Fires after [`BATCHING_HINT_STREAK`] consecutive turns that each carried
/// exactly one cheap inspection or todo-bookkeeping call. A model that remains
/// serialized may receive another hint only after
/// [`BATCHING_HINT_REARM_STREAK`] more such turns, capped by
/// [`BATCHING_HINT_LIMIT`]; a multi-call turn or any other tool resets the
/// current streak. Distinct from
/// [`RepeatedInspectionGuard`]: that guard breaks loops re-running the *same*
/// inspection, this one addresses fresh-but-serialized single calls that
/// should have shared one turn.
#[derive(Default)]
struct SingleCallStreakGuard {
    streak: usize,
    hints_emitted: usize,
}

const SERIAL_DELEGATION_NUDGE: &str =
    "the previous delegation was serial — batch remaining scans in one call.";

#[derive(Default)]
struct SerialDelegationGuard {
    turn: usize,
    last_read_only_turn: Option<usize>,
}

impl SerialDelegationGuard {
    fn hint_for(&mut self, serial_read_only_delegation: bool) -> Option<&'static str> {
        self.turn = self.turn.saturating_add(1);
        if !serial_read_only_delegation {
            return None;
        }
        let hint = self
            .last_read_only_turn
            .is_some_and(|previous| self.turn.saturating_sub(previous) <= 3)
            .then_some(SERIAL_DELEGATION_NUDGE);
        self.last_read_only_turn = Some(self.turn);
        hint
    }
}

fn is_serial_read_only_delegation(tool_calls: &[ToolCall], tool_registry: &ToolRegistry) -> bool {
    let [tool_call] = tool_calls else {
        return false;
    };
    let Ok(arguments) = serde_json::from_str(&tool_call.arguments) else {
        return false;
    };
    tool_registry
        .get(&tool_call.name)
        .and_then(|tool| tool.delegation_is_read_only(&arguments))
        == Some(true)
}

impl SingleCallStreakGuard {
    fn hint_for(&mut self, tool_calls: &[ToolCall]) -> Option<String> {
        let is_single_batchable_call =
            matches!(tool_calls, [only] if batchable_single_call_tool(&only.name));
        if !is_single_batchable_call {
            self.streak = 0;
            return None;
        }
        self.streak = self.streak.saturating_add(1);
        if self.hints_emitted >= BATCHING_HINT_LIMIT {
            return None;
        }
        let threshold = if self.hints_emitted == 0 {
            BATCHING_HINT_STREAK
        } else {
            BATCHING_HINT_REARM_STREAK
        };
        if self.streak < threshold {
            return None;
        }
        self.streak = 0;
        self.hints_emitted = self.hints_emitted.saturating_add(1);
        Some(batching_hint_message())
    }
}

/// Tools cheap and side-effect-free enough that several of them should ride
/// one turn: the read-only inspectors plus `todowrite` (one observed run spent
/// whole turns on todo updates, four of them no-ops).
fn batchable_single_call_tool(name: &str) -> bool {
    structured_file_read_tool(name) || repeated_inspection_tool(name) || name == "todowrite"
}

fn batching_hint_message() -> String {
    // Deliberately no "extra turns are expensive" cost framing: a model given that framing
    // read that as "do everything in one turn" and answered with a single
    // 70k-token reasoning marathon instead of acting. The hint is about
    // emitting several tool calls per response, never about doing more
    // thinking before acting.
    "Recent turns each issued exactly one small tool call. When the next checks are already clear, emit a small batch of 2-4 independent read/glob/grep/diagnostics/todowrite calls; they run in parallel. Sequence calls that depend on earlier output. Once several related edit sites are understood, prefer one multi-file apply_patch over one edit turn per file, but keep uncertain or repair-driven changes serial. This is only about tool emission: do not front-load extra design or private reasoning to make a turn \"bigger\"."
        .to_string()
}

/// How many consecutive blank turns (no tool calls, no answer text) are
/// re-prompted before the run fails loudly. A blank turn usually means the
/// model spent its entire output on private reasoning and the stream ended
/// before any action (GLM-5.2 died this way: 70k-token
/// thinking marathons, then an empty turn silently treated as success).
/// We re-prompt such a turn ("You did not use a tool…"), capped by a mistake
/// counter, rather than ending it silently as success — ending silently is
/// exactly the failure mode this guards against.
const EMPTY_RESPONSE_NUDGE_LIMIT: usize = 2;

fn empty_response_nudge_message() -> String {
    "Your previous turn produced no tool calls and no answer text. If you were \
     reasoning, that output was cut off before any action was taken — very long private \
     reasoning can exceed the output limit and is then lost. Do not re-derive the plan in \
     reasoning. Take the next concrete step NOW with tool calls (edit/write/bash/todowrite), \
     keeping private reasoning to a few sentences; verify by running commands, not by proving \
     correctness in your head. If the task is already complete, reply with a short final \
     summary instead. (Automated message — do not respond to it conversationally.)"
        .to_string()
}

/// Counts consecutive parent-lane coding turns whose tool observations all
/// have `makes_progress == false` — turns that only inspect (read/grep/
/// search/diff) without a single mutation, delegation, or plan step. Unlike
/// [`RepeatedInspectionGuard`]/[`ReadStormGuard`] it never consults on-disk
/// file versions: freshness resets are exactly the loophole that let a session
/// loop forever amid peer worktree churn. Reset only by a progress-making
/// observation or live user steering. Not model-gated; armed only for the
/// built-in coding persona on the parent lane
/// (see `Agent::implementation_stall_guarded`).
#[derive(Debug, Default)]
struct ImplementationStallGuard {
    turns_without_progress: usize,
}

#[derive(Debug)]
struct ImplementationStallNudge {
    note: String,
}

impl ImplementationStallGuard {
    fn observe_turn(
        &mut self,
        observations: &[ToolCallObservation],
    ) -> Option<ImplementationStallNudge> {
        if observations
            .iter()
            .any(|observation| observation.makes_progress)
        {
            self.reset();
            return None;
        }
        self.turns_without_progress = self.turns_without_progress.saturating_add(1);
        if self.turns_without_progress == IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS {
            return Some(ImplementationStallNudge {
                note: implementation_stall_first_nudge_message(self.turns_without_progress),
            });
        }
        if self.turns_without_progress == IMPLEMENTATION_STALL_SECOND_NUDGE_TURNS {
            return Some(ImplementationStallNudge {
                note: implementation_stall_second_nudge_message(self.turns_without_progress),
            });
        }
        if self.turns_without_progress >= IMPLEMENTATION_STALL_REPEATED_NUDGE_START_TURNS
            && (self.turns_without_progress - IMPLEMENTATION_STALL_REPEATED_NUDGE_START_TURNS)
                .is_multiple_of(IMPLEMENTATION_STALL_REPEATED_NUDGE_INTERVAL_TURNS)
        {
            return Some(ImplementationStallNudge {
                note: implementation_stall_repeated_nudge_message(self.turns_without_progress),
            });
        }
        None
    }

    fn reset(&mut self) {
        self.turns_without_progress = 0;
    }
}

fn implementation_stall_first_nudge_message(turns: usize) -> String {
    format!(
        "You have spent {turns} consecutive turns on read-only exploration (read/grep/search/diff) \
         without a single file edit, delegation, or user-visible answer. Stop assessing and act NOW \
         — pick one: (1) begin the edits your task and todo list call for, starting with the \
         smallest safe change; or (2) if something genuinely blocks editing, finish this run with a \
         short reply naming the concrete blocker and what you need. Pre-existing uncommitted \
         changes in this worktree are the expected baseline — other work in this repository, not \
         yours to protect, reconcile, or re-verify. Make your edits on top of them and leave them \
         alone. Do not restart your survey of the code. (Automated message — do not respond to it \
         conversationally.)"
    )
}

fn implementation_stall_second_nudge_message(turns: usize) -> String {
    format!(
        "Second notice: {turns} consecutive exploration-only turns and still no edit or answer. \
         Further inspection is allowed when the task genuinely needs it, but keep it bounded to \
         one unanswered question. Use the evidence already gathered to make the smallest safe edit, \
         delegate that specific gap, or reply to the user naming the concrete blocker. Reminder: \
         pre-existing uncommitted worktree changes are baseline from other work — edit on top of \
         them; do not reconcile, protect, or re-verify them. (Automated message — do not respond to \
         it conversationally.)"
    )
}

fn implementation_stall_repeated_nudge_message(turns: usize) -> String {
    format!(
        "Persistent implementation nudge: {turns} consecutive exploration-only turns have \
         produced no edit, delegation, or user-visible answer. Convert the evidence already in \
         context into a concrete next step now. Make the smallest safe edit; if one specific fact \
         is still missing, inspect or delegate only that bounded question before acting; if an \
         external blocker prevents progress, tell the user exactly what it is. Do not restart a \
         broad survey. (Automated message — do not respond to it conversationally.)"
    )
}

fn repeated_inspection_signature(tool_calls: &[ToolCall]) -> Option<String> {
    if tool_calls.is_empty()
        || tool_calls
            .iter()
            .any(|tool_call| !repeated_inspection_call(tool_call))
    {
        return None;
    }

    let mut parts = tool_calls
        .iter()
        .map(|tool_call| {
            format!(
                "{}:{}",
                tool_call.name,
                normalized_tool_arguments_for_signature(&tool_call.arguments)
            )
        })
        .collect::<Vec<_>>();
    parts.sort();
    Some(parts.join("\n"))
}

fn repeated_inspection_turn_limit(tool_calls: &[ToolCall]) -> usize {
    if tool_calls.iter().all(file_read_inspection_call) {
        STRUCTURED_READ_REPEAT_TURN_LIMIT
    } else {
        REPEATED_INSPECTION_TURN_LIMIT
    }
}

fn normalized_tool_arguments_for_signature(arguments: &str) -> String {
    let normalized = normalize_tool_call_arguments_json(arguments);
    serde_json::from_str::<serde_json::Value>(&normalized)
        .ok()
        .and_then(|value| serde_json::to_string(&value).ok())
        .unwrap_or(normalized)
}

fn repeated_inspection_call(tool_call: &ToolCall) -> bool {
    repeated_inspection_tool(&tool_call.name) || bash_file_read_target(tool_call).is_some()
}

fn repeated_inspection_tool(name: &str) -> bool {
    structured_file_read_tool(name)
        || matches!(
            name,
            "project_info"
                | "glob"
                | "grep"
                | "symbol_search"
                | "definition"
                | "references"
                | "hover"
                | "workspace_symbol"
                | "git"
                | "diagnostics"
                | "websearch"
        )
}

fn repeated_inspection_loop_message(turn_limit: usize) -> String {
    format!(
        "Error: repeated inspection loop detected after {turn_limit} identical read-only tool-call turns. The previous results already reported the current state. Stop re-running the same project_info/read/grep/git checks; take a concrete next step now, ask a focused question if blocked, or provide the final answer with assumptions."
    )
}

fn repair_read_only_loop_message() -> String {
    "Error: this exact inspection already ran while repairing, and the failed tool's output is summarized in the system context. Do not repeat the same read/grep/project_info/git call; act on what you have — use edit/write, run a targeted command that changes or verifies the fix, inspect a different relevant file, ask a focused question if blocked, or provide the final answer with assumptions.".to_string()
}

fn repeated_inspection_stop_message(turn_limit: usize) -> String {
    format!(
        "Agent stopped after repeated inspection loop: the model requested the same read-only tool batch more than {} times and ignored the corrective tool result.",
        turn_limit + REPEATED_INSPECTION_REJECTION_LIMIT
    )
}

fn repair_read_only_stop_message() -> String {
    "Agent stopped after repair loop: the model kept requesting read-only inspection after a concrete failed-tool repair target was already available.".to_string()
}

fn read_storm_loop_message(target: &str) -> String {
    format!(
        "Error: repeated read storm detected for {target} after {READ_STORM_TARGET_TURN_LIMIT} prior turns. The current evidence for this target is already in the conversation or restorable through /ctx. Do not immediately request another range from the same file. For a large source file, navigate symbol-first: use `symbol_search` with the file path and a type/function name, then `read_symbol` for that definition; use `grep` once when you need call sites rather than the definition. Example: `symbol_search {{\"path\":\"src/tui/run/event_loop.rs\",\"query\":\"RuntimeEventDrainDeps\"}}`, then `read_symbol {{\"path\":\"src/tui/run/event_loop.rs\",\"query\":\"RuntimeEventDrainDeps\"}}`. Otherwise use the existing evidence, inspect a different target, make a concrete change, ask a focused question if blocked, or provide the final answer. A changed approach clears this rejection; immediately repeating the same target stops the run."
    )
}

fn read_storm_stop_message(target: &str) -> String {
    format!(
        "Agent stopped after repeated read storm for {target}: the model kept requesting the same read target after Bonsai rejected the loop."
    )
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::*;

    struct SlowTool {
        cancellation: Arc<StdMutex<Option<CancellationToken>>>,
    }

    struct ReadyTool;

    #[async_trait]
    impl crate::tool::Tool for ReadyTool {
        fn name(&self) -> &str {
            "ready"
        }

        fn description(&self) -> &str {
            "Return immediately"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
            Ok(ToolOutput::Text("completed".to_string()))
        }
    }

    #[async_trait]
    impl crate::tool::Tool for SlowTool {
        fn name(&self) -> &str {
            "slow"
        }

        fn description(&self) -> &str {
            "Wait forever"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        }

        async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput> {
            std::future::pending().await
        }

        async fn execute_with_context(
            &self,
            _args: serde_json::Value,
            context: crate::tool::ToolExecutionContext,
        ) -> Result<ToolOutput> {
            *self
                .cancellation
                .lock()
                .expect("slow-tool token mutex should not be poisoned") =
                context.cancellation_token();
            std::future::pending().await
        }
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: format!("{name}-1"),
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }

    #[tokio::test]
    async fn tool_runtime_budget_interrupts_a_stalled_tool() {
        let mut registry = ToolRegistry::new();
        let observed_cancellation = Arc::new(StdMutex::new(None));
        registry.register(Arc::new(SlowTool {
            cancellation: observed_cancellation.clone(),
        }));
        let sink: SharedSink = Arc::new(crate::output::StdoutSink);
        let started = std::time::Instant::now();

        let (_, output, status) = Agent::execute_single_tool_call(
            call("slow", "{}"),
            Arc::new(registry),
            ToolRejections::default(),
            sink,
            CancellationToken::new(),
            Arc::new(crate::hooks::HookEngine::disabled()),
            None,
            Some(Duration::from_millis(10)),
        )
        .await;

        assert_eq!(status, crate::output::ToolExecutionStatus::Interrupted);
        assert!(output.rendered_summary().contains("runtime budget"));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            observed_cancellation
                .lock()
                .expect("slow-tool token mutex should not be poisoned")
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        );
    }

    #[tokio::test]
    async fn zero_duration_budget_wins_over_ready_tool_execution() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ReadyTool));

        let (_, output, status) = Agent::execute_single_tool_call(
            call("ready", "{}"),
            Arc::new(registry),
            ToolRejections::default(),
            Arc::new(crate::output::StdoutSink),
            CancellationToken::new(),
            Arc::new(crate::hooks::HookEngine::disabled()),
            None,
            Some(Duration::ZERO),
        )
        .await;

        assert_eq!(status, crate::output::ToolExecutionStatus::Interrupted);
        assert!(output.rendered_summary().contains("runtime budget"));
    }

    #[test]
    fn completed_effect_tracks_registered_file_mutations() {
        let plain = ToolOutput::Text("ok".to_string());
        assert_eq!(
            completed_tool_workspace_effect(
                &call("bash", r#"{"command":"git commit -m update"}"#),
                &plain,
                Vec::new(),
            ),
            crate::tool::ToolWorkspaceEffect::NoMutation
        );
        assert_eq!(
            completed_tool_workspace_effect(
                &call("write", r#"{"path":"src/lib.rs","content":"x"}"#),
                &plain,
                vec!["src/lib.rs".to_string()],
            ),
            crate::tool::ToolWorkspaceEffect::ScopedMutation(vec!["src/lib.rs".to_string()])
        );
        assert_eq!(
            completed_tool_workspace_effect(
                &call(
                    "rename_symbol",
                    r#"{"path":"src/lib.rs","line":1,"character":1,"new_name":"renamed"}"#,
                ),
                &plain,
                vec!["src/lib.rs".to_string()],
            ),
            crate::tool::ToolWorkspaceEffect::ScopedMutation(vec!["src/lib.rs".to_string()])
        );
        assert_eq!(
            completed_tool_workspace_effect(
                &call("agent", r#"{"agent":"fixer","prompt":"fix it"}"#),
                &plain,
                Vec::new(),
            ),
            crate::tool::ToolWorkspaceEffect::Unscoped
        );
        // A multi-file patch is a mutation: it must arm self-review just like
        // write/edit, so a greenfield burst is not silently unreviewable.
        assert_eq!(
            completed_tool_workspace_effect(
                &call(
                    "apply_patch",
                    r#"{"input":"*** Begin Patch\n*** Add File: src/lib.rs\n+fn x() {}\n*** End Patch"}"#,
                ),
                &plain,
                vec!["src/lib.rs".to_string()],
            ),
            crate::tool::ToolWorkspaceEffect::ScopedMutation(vec!["src/lib.rs".to_string()])
        );
    }

    #[test]
    fn completed_effect_uses_observed_delegation_effect() {
        let call = call("agent", r#"{"agent":"explore","prompt":"inspect"}"#);
        let output = |workspace_effect| ToolOutput::TextWithUsage {
            text: "done".to_string(),
            status: crate::output::ToolExecutionStatus::Succeeded,
            usage: UsageTotals::default(),
            usage_turns: Vec::new(),
            delegated_read_evidence: Vec::new(),
            workspace_effect,
        };

        assert_eq!(
            completed_tool_workspace_effect(
                &call,
                &output(crate::tool::ToolWorkspaceEffect::NoMutation),
                Vec::new(),
            ),
            crate::tool::ToolWorkspaceEffect::NoMutation
        );
        assert_eq!(
            completed_tool_workspace_effect(
                &call,
                &output(crate::tool::ToolWorkspaceEffect::ScopedMutation(vec![
                    "src/lib.rs".to_string(),
                ])),
                Vec::new(),
            ),
            crate::tool::ToolWorkspaceEffect::ScopedMutation(vec!["src/lib.rs".to_string()])
        );
    }

    #[test]
    fn delegated_effect_refinement_only_proves_a_lone_noop() {
        let result = |id: &str| {
            (
                ToolCall {
                    id: id.to_string(),
                    name: "agent".to_string(),
                    arguments: r#"{"agent":"fixer","prompt":"fix"}"#.to_string(),
                },
                ToolOutput::TextWithUsage {
                    text: "done".to_string(),
                    status: crate::output::ToolExecutionStatus::Succeeded,
                    usage: UsageTotals::default(),
                    usage_turns: Vec::new(),
                    delegated_read_evidence: Vec::new(),
                    workspace_effect: crate::tool::ToolWorkspaceEffect::Unscoped,
                },
                crate::output::ToolExecutionStatus::Succeeded,
            )
        };

        let mut noop = vec![result("noop")];
        refine_delegated_workspace_effects(&mut noop, Some(&[]));
        assert_eq!(
            noop[0].1.workspace_effect(),
            Some(&crate::tool::ToolWorkspaceEffect::NoMutation)
        );

        let mut changed = vec![result("changed")];
        refine_delegated_workspace_effects(&mut changed, Some(&["src/lib.rs".to_string()]));
        assert_eq!(
            changed[0].1.workspace_effect(),
            Some(&crate::tool::ToolWorkspaceEffect::WindowMutation(vec![
                "src/lib.rs".to_string()
            ])),
            "a lone shared-worktree delta stays reviewable at low confidence"
        );

        let mut batched_noops = vec![result("first"), result("second")];
        refine_delegated_workspace_effects(&mut batched_noops, Some(&[]));
        assert!(batched_noops.iter().all(|(_, output, _)| matches!(
            output.workspace_effect(),
            Some(crate::tool::ToolWorkspaceEffect::Unscoped)
        )));
    }

    #[test]
    fn mutation_paths_extracts_every_apply_patch_target() {
        let patch = "*** Begin Patch\n\
                     *** Add File: src/model.rs\n\
                     +pub struct M;\n\
                     *** Update File: src/lib.rs\n\
                     *** Move to: src/root.rs\n\
                     -old\n\
                     +new\n\
                     *** Delete File: gone.rs\n\
                     *** End Patch";
        let args = serde_json::json!({ "input": patch }).to_string();
        let mut paths = mutation_paths(&call("apply_patch", &args));
        paths.sort();
        // Add → target, Update+Move → destination, Delete → omitted.
        assert_eq!(paths, vec!["src/model.rs", "src/root.rs"]);
    }

    fn is_reject(action: Option<RepeatedInspectionAction>) -> bool {
        matches!(action, Some(RepeatedInspectionAction::Reject(_)))
    }

    fn is_storm_reject(action: Option<ReadStormAction>) -> bool {
        matches!(action, Some(ReadStormAction::Reject(_)))
    }

    fn is_failure_reject(action: Option<RepeatedFailureAction>) -> bool {
        matches!(action, Some(RepeatedFailureAction::Reject(_)))
    }

    #[test]
    fn single_call_streak_guard_rearms_slowly_and_caps_hints() {
        let mut guard = SingleCallStreakGuard::default();
        // Fresh targets each turn — RepeatedInspectionGuard territory this is not.
        assert!(
            guard
                .hint_for(&[call("read", r#"{"path":"a.rs"}"#)])
                .is_none()
        );
        assert!(
            guard
                .hint_for(&[call("grep", r#"{"pattern":"x"}"#)])
                .is_none()
        );
        let hint = guard.hint_for(&[call("todowrite", r#"{"todos":[]}"#)]);
        assert!(
            hint.is_some_and(|text| text.contains("batch")),
            "third consecutive single call must produce the batching hint",
        );
        for index in 1..BATCHING_HINT_REARM_STREAK {
            assert!(
                guard
                    .hint_for(&[call("read", &format!(r#"{{"path":"b{index}.rs"}}"#))])
                    .is_none(),
                "repeat hint fired before the long re-arm threshold"
            );
        }
        assert!(
            guard
                .hint_for(&[call("grep", r#"{"pattern":"second"}"#)])
                .is_some(),
            "a persistently serialized model should receive a second hint"
        );

        for _ in 1..BATCHING_HINT_REARM_STREAK {
            assert!(
                guard
                    .hint_for(&[call("read", r#"{"path":"repeat.rs"}"#)])
                    .is_none()
            );
        }
        assert!(
            guard
                .hint_for(&[call("grep", r#"{"pattern":"third"}"#)])
                .is_some(),
            "the final allowed hint should fire"
        );

        for _ in 0..BATCHING_HINT_REARM_STREAK * 2 {
            assert!(
                guard
                    .hint_for(&[call("read", r#"{"path":"capped.rs"}"#)])
                    .is_none(),
                "the hard hint cap must suppress further reminders"
            );
        }
    }

    #[test]
    fn serial_delegation_guard_nudges_on_second_nearby_scan() {
        let mut guard = SerialDelegationGuard::default();

        assert_eq!(guard.hint_for(true), None);
        assert_eq!(guard.hint_for(false), None);
        assert_eq!(guard.hint_for(true), Some(SERIAL_DELEGATION_NUDGE));

        assert_eq!(guard.hint_for(false), None);
        assert_eq!(guard.hint_for(false), None);
        assert_eq!(guard.hint_for(false), None);
        assert_eq!(
            guard.hint_for(true),
            None,
            "a delegation outside the three-turn window starts a new sequence"
        );
    }

    #[test]
    fn single_call_streak_guard_resets_on_batched_or_mutating_turns() {
        let mut guard = SingleCallStreakGuard::default();
        assert!(
            guard
                .hint_for(&[call("read", r#"{"path":"a.rs"}"#)])
                .is_none()
        );
        assert!(
            guard
                .hint_for(&[call("read", r#"{"path":"b.rs"}"#)])
                .is_none()
        );
        // A two-call turn is exactly the behavior we want — reset.
        assert!(
            guard
                .hint_for(&[
                    call("read", r#"{"path":"c.rs"}"#),
                    call("read", r#"{"path":"d.rs"}"#),
                ])
                .is_none()
        );
        assert!(
            guard
                .hint_for(&[call("read", r#"{"path":"e.rs"}"#)])
                .is_none()
        );
        // A mutating turn resets too: edit → build → fix sequencing is legitimate.
        assert!(
            guard
                .hint_for(&[call("edit", r#"{"path":"e.rs"}"#)])
                .is_none()
        );
        assert!(
            guard
                .hint_for(&[call("read", r#"{"path":"f.rs"}"#)])
                .is_none()
        );
        assert!(
            guard
                .hint_for(&[call("read", r#"{"path":"g.rs"}"#)])
                .is_none()
        );
        assert!(
            guard
                .hint_for(&[call("read", r#"{"path":"h.rs"}"#)])
                .is_some(),
            "a full new streak after resets must still hint",
        );
    }

    fn stall_observation(
        name: &str,
        args: &str,
        status: crate::output::ToolExecutionStatus,
    ) -> ToolCallObservation {
        ToolCallObservation::new(&call(name, args), status)
    }

    #[test]
    fn implementation_stall_guard_keeps_nudging_without_stopping_distinct_reads() {
        // The repeated-read shape: a *distinct* read every turn, which the
        // signature-based guards treat as fresh work forever.
        let mut guard = ImplementationStallGuard::default();
        let last_turn = IMPLEMENTATION_STALL_REPEATED_NUDGE_START_TURNS
            + IMPLEMENTATION_STALL_REPEATED_NUDGE_INTERVAL_TURNS;
        let mut nudge_turns = Vec::new();
        for turn in 1..=last_turn {
            let action = guard.observe_turn(&[stall_observation(
                "read",
                &format!(r#"{{"path":"src/file{turn}.rs"}}"#),
                crate::output::ToolExecutionStatus::Succeeded,
            )]);
            if action.is_some() {
                nudge_turns.push(turn);
            }
        }
        assert_eq!(
            nudge_turns,
            vec![
                IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS,
                IMPLEMENTATION_STALL_SECOND_NUDGE_TURNS,
                IMPLEMENTATION_STALL_REPEATED_NUDGE_START_TURNS,
                last_turn,
            ]
        );
    }

    #[test]
    fn implementation_stall_guard_resets_on_progress_and_rearms_nudges() {
        let mut guard = ImplementationStallGuard::default();
        for turn in 1..IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS {
            assert!(
                guard
                    .observe_turn(&[stall_observation(
                        "read",
                        &format!(r#"{{"path":"src/a{turn}.rs"}}"#),
                        crate::output::ToolExecutionStatus::Succeeded,
                    )])
                    .is_none()
            );
        }
        // A successful mutation resets the counter and the nudge budget.
        assert!(
            guard
                .observe_turn(&[stall_observation(
                    "edit",
                    r#"{"path":"src/a.rs"}"#,
                    crate::output::ToolExecutionStatus::Succeeded,
                )])
                .is_none()
        );
        assert_eq!(guard.turns_without_progress, 0);
        for turn in 1..=IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS {
            let action = guard.observe_turn(&[stall_observation(
                "read",
                &format!(r#"{{"path":"src/b{turn}.rs"}}"#),
                crate::output::ToolExecutionStatus::Succeeded,
            )]);
            if turn == IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS {
                assert!(action.is_some(), "reset must re-arm the first nudge");
            } else {
                assert!(action.is_none());
            }
        }
    }

    #[test]
    fn implementation_stall_guard_resets_when_background_work_starts() {
        let mut guard = ImplementationStallGuard {
            turns_without_progress: IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS - 1,
        };
        let mut observation = stall_observation(
            "bash",
            r#"{"command":"cargo test --locked","run_in_background":true}"#,
            crate::output::ToolExecutionStatus::Started,
        );
        observation.observe_result(&ToolOutput::BackgroundTaskStarted {
            task_id: "bg-1".to_string(),
            message: "Started background task bg-1".to_string(),
        });

        assert!(guard.observe_turn(&[observation]).is_none());
        assert_eq!(guard.turns_without_progress, 0);
    }

    #[test]
    fn implementation_stall_guard_ignores_failed_mutations() {
        // A *failed* edit is not progress: the repeated-read pathology includes
        // edits the model aborts or that bounce off validation.
        let mut guard = ImplementationStallGuard::default();
        for turn in 1..=IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS {
            let action = guard.observe_turn(&[stall_observation(
                "edit",
                &format!(r#"{{"path":"src/c{turn}.rs"}}"#),
                crate::output::ToolExecutionStatus::Failed,
            )]);
            if turn == IMPLEMENTATION_STALL_FIRST_NUDGE_TURNS {
                assert!(action.is_some());
            } else {
                assert!(action.is_none(), "failed edits must count toward the stall");
            }
        }
    }

    #[test]
    fn inspection_guard_trips_on_identical_consecutive_reads() {
        let mut guard = RepeatedInspectionGuard::default();
        let batch = [call("read", r#"{"path":"a.rs"}"#)];
        assert!(guard.action_for(&batch, false).is_none()); // turn 1
        assert!(guard.action_for(&batch, false).is_none()); // turn 2
        assert!(guard.action_for(&batch, false).is_none()); // turn 3
        assert!(is_reject(guard.action_for(&batch, false))); // turn 4 → reject
    }

    #[test]
    fn changed_file_version_resets_both_reread_guards() {
        let batch = [call("read", r#"{"path":"a.rs"}"#)];
        let unchanged = ReadTargetVersions::assume_unchanged(&batch);
        let changed = ReadTargetVersions {
            file_targets: unchanged.file_targets.clone(),
            unchanged_targets: HashSet::new(),
            excluded_targets: HashSet::new(),
        };

        let mut inspection = RepeatedInspectionGuard::default();
        for _ in 0..STRUCTURED_READ_REPEAT_TURN_LIMIT {
            assert!(
                inspection
                    .action_for_with_versions(&batch, false, &unchanged)
                    .is_none()
            );
        }
        assert!(
            inspection
                .action_for_with_versions(&batch, false, &changed)
                .is_none(),
            "a changed file is a new inspection, even with identical arguments"
        );

        let mut storm = ReadStormGuard::default();
        for _ in 0..READ_STORM_TARGET_TURN_LIMIT {
            assert!(storm.action_for_with_versions(&batch, &unchanged).is_none());
        }
        assert!(
            storm.action_for_with_versions(&batch, &changed).is_none(),
            "a changed file version must reset the per-path turn count"
        );
        for _ in 1..READ_STORM_TARGET_TURN_LIMIT {
            assert!(storm.action_for_with_versions(&batch, &unchanged).is_none());
        }
        assert!(is_storm_reject(
            storm.action_for_with_versions(&batch, &unchanged)
        ));
    }

    #[test]
    fn file_read_variants_share_repeat_and_path_storm_guards() {
        for (name, arguments) in [
            (
                "read_region",
                r#"{"path":"src/review.rs","start_line":455,"end_line":570}"#,
            ),
            (
                "read_symbol",
                r#"{"path":"src/review.rs","query":"self_review_prompt"}"#,
            ),
            ("bash", r#"{"command":"cat src/review.rs"}"#),
        ] {
            let batch = [call(name, arguments)];
            let mut repeat_guard = RepeatedInspectionGuard::default();
            for _ in 0..STRUCTURED_READ_REPEAT_TURN_LIMIT {
                assert!(repeat_guard.action_for(&batch, false).is_none());
            }
            assert!(is_reject(repeat_guard.action_for(&batch, false)));

            let mut storm_guard = ReadStormGuard::default();
            for _ in 0..READ_STORM_TARGET_TURN_LIMIT {
                assert!(storm_guard.action_for(&batch).is_none());
            }
            assert!(is_storm_reject(storm_guard.action_for(&batch)));
        }

        let redirect = [call(
            "bash",
            r#"{"command":"cat src/review.rs > /tmp/review.rs"}"#,
        )];
        assert!(
            repeated_inspection_signature(&redirect).is_none(),
            "a shell command with redirection must not be classified as a read-only file inspection"
        );
        assert!(read_storm_targets(&redirect).is_empty());
    }

    #[test]
    fn repeated_failure_guard_allows_one_retry_then_rejects_and_stops() {
        let mut guard = RepeatedFailureGuard::default();
        let first = call("bash", r#"{"command":"cargo test --locked"}"#);
        assert!(guard.action_for(std::slice::from_ref(&first)).is_none());
        guard.observe(
            &[ToolCallObservation::new(
                &first,
                crate::output::ToolExecutionStatus::Failed,
            )],
            false,
        );

        let retry = call("bash", r#"{"command":"cargo test --locked"}"#);
        assert!(
            guard.action_for(std::slice::from_ref(&retry)).is_none(),
            "one identical retry may recover from a transient failure"
        );
        guard.observe(
            &[ToolCallObservation::new(
                &retry,
                crate::output::ToolExecutionStatus::Failed,
            )],
            false,
        );

        let rejected = call("bash", r#"{"command":"cargo test --locked"}"#);
        assert!(is_failure_reject(
            guard.action_for(std::slice::from_ref(&rejected))
        ));
        guard.observe(
            &[ToolCallObservation::new(
                &rejected,
                crate::output::ToolExecutionStatus::Failed,
            )],
            false,
        );

        let ignored = call("bash", r#"{"command":"cargo test --locked"}"#);
        assert!(matches!(
            guard.action_for(&[ignored]),
            Some(RepeatedFailureAction::Stop(_))
        ));
    }

    #[test]
    fn repeated_failure_guard_does_not_treat_failed_mutation_as_progress() {
        let mut guard = RepeatedFailureGuard::default();
        for _ in 0..REPEATED_FAILED_CALL_LIMIT {
            let edit = call(
                "edit",
                r#"{"path":"a.rs","old_string":"missing","new_string":"fixed"}"#,
            );
            assert!(guard.action_for(std::slice::from_ref(&edit)).is_none());
            guard.observe(
                &[ToolCallObservation::new(
                    &edit,
                    crate::output::ToolExecutionStatus::Failed,
                )],
                false,
            );
        }
        let repeated = call(
            "edit",
            r#"{"path":"a.rs","old_string":"missing","new_string":"fixed"}"#,
        );
        assert!(is_failure_reject(
            guard.action_for(std::slice::from_ref(&repeated))
        ));
    }

    #[test]
    fn inspection_guard_trips_on_alternating_read_and_git() {
        // The model alternated `read` and `git status` as separate
        // turns, so the old last-signature-only guard reset every turn and never
        // fired. The recent-window makes the cycle trip.
        let mut guard = RepeatedInspectionGuard::default();
        let read = [call("read", r#"{"path":"parser.rs","offset":220}"#)];
        let git = [call("git", r#"{"op":"status"}"#)];
        assert!(guard.action_for(&read, false).is_none()); // t1 read (new)
        assert!(guard.action_for(&git, false).is_none()); // t2 git (new)
        assert!(guard.action_for(&read, false).is_none()); // t3 read (repeat) turns=2
        assert!(is_reject(guard.action_for(&git, false))); // t4 git (repeat) turns=3
    }

    #[test]
    fn inspection_guard_ignores_distinct_exploration() {
        // Reading a fresh file each turn is legitimate research, not a loop.
        let mut guard = RepeatedInspectionGuard::default();
        for path in ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs", "g.rs"] {
            let batch = [call("read", &format!(r#"{{"path":"{path}"}}"#))];
            assert!(
                guard.action_for(&batch, false).is_none(),
                "distinct reads must not trip: {path}"
            );
        }
    }

    #[test]
    fn inspection_guard_resets_after_a_mutation_turn() {
        let mut guard = RepeatedInspectionGuard::default();
        let read = [call("read", r#"{"path":"a.rs"}"#)];
        let edit = [call(
            "edit",
            r#"{"path":"a.rs","old_string":"x","new_string":"y"}"#,
        )];
        guard.action_for(&read, false);
        guard.action_for(&read, false);
        guard.action_for(&read, false);
        // A mutation (non-inspection) turn is progress → the guard resets.
        assert!(guard.action_for(&edit, false).is_none());
        assert!(guard.action_for(&read, false).is_none());
        assert!(guard.action_for(&read, false).is_none());
        assert!(guard.action_for(&read, false).is_none());
        assert!(is_reject(guard.action_for(&read, false)));
    }

    #[test]
    fn read_storm_guard_trips_across_mixed_review_turns() {
        let mut guard = ReadStormGuard::default();
        for turn in 0..READ_STORM_TARGET_TURN_LIMIT {
            let calls = [
                call(
                    "read",
                    &format!(r#"{{"path":"src/tool/agent.rs","offset":{}}}"#, 300 + turn),
                ),
                call("agent", r#"{"agent":"review","prompt":"check"}"#),
            ];
            assert!(
                guard.action_for(&calls).is_none(),
                "turn {turn} should stay below the storm limit"
            );
        }

        let rejected =
            guard.action_for(&[call("read", r#"{"path":"src/tool/agent.rs","offset":480}"#)]);
        assert!(
            is_storm_reject(rejected),
            "same-path shifted reads should trip even when earlier turns included agent calls"
        );
    }

    #[test]
    fn read_storm_guard_counts_file_scoped_grep_as_the_same_target() {
        let path = "src/storage/workspace_locks.rs";
        let turns = [
            call("read", &format!(r#"{{"path":"{path}"}}"#)),
            call(
                "grep",
                &format!(r#"{{"path":"{path}","pattern":"fn refresh"}}"#),
            ),
            call(
                "grep",
                &format!(r#"{{"path":"{path}","pattern":"safety_margin"}}"#),
            ),
        ];
        let mut guard = ReadStormGuard::default();
        for tool_call in turns {
            assert!(guard.action_for(&[tool_call]).is_none());
        }

        let rejected = guard.action_for(&[call(
            "read_region",
            &format!(r#"{{"path":"{path}","start_line":405,"end_line":495}}"#),
        )]);
        assert!(
            is_storm_reject(rejected),
            "session 78 alternated reads and file-scoped greps to evade the per-path guard"
        );
    }

    #[test]
    fn read_storm_guard_ignores_excluded_search_targets() {
        let grep = call("grep", r#"{"path":"src","pattern":"workspace_lock"}"#);
        let target = read_storm_target(&grep).expect("grep path should have a target");
        let versions = ReadTargetVersions {
            excluded_targets: HashSet::from([target]),
            ..Default::default()
        };
        let mut guard = ReadStormGuard::default();

        for _ in 0..READ_STORM_TARGET_TURN_LIMIT + 2 {
            assert!(
                guard
                    .action_for_with_versions(std::slice::from_ref(&grep), &versions)
                    .is_none(),
                "directory-wide searches must remain available"
            );
        }
    }

    #[test]
    fn read_storm_guard_allows_redirect_before_returning_to_target() {
        let target = call("read_region", r#"{"path":"src/tui/run/event_loop.rs"}"#);
        let redirect = call(
            "symbol_search",
            r#"{"path":"src/tui/run/event_loop.rs","query":"RuntimeEventDrainDeps"}"#,
        );
        let mut guard = ReadStormGuard::default();

        for _ in 0..READ_STORM_TARGET_TURN_LIMIT {
            assert!(guard.action_for(std::slice::from_ref(&target)).is_none());
        }
        assert!(is_storm_reject(
            guard.action_for(std::slice::from_ref(&target))
        ));
        assert!(guard.action_for(&[redirect]).is_none());
        assert!(
            guard.action_for(&[target]).is_none(),
            "following the redirect must clear the rejection strike"
        );
    }

    #[test]
    fn read_storm_guard_stops_an_immediate_repeat_after_rejection() {
        let target = call("read", r#"{"path":"src/tool/agent.rs"}"#);
        let mut guard = ReadStormGuard::default();

        for _ in 0..READ_STORM_TARGET_TURN_LIMIT {
            assert!(guard.action_for(std::slice::from_ref(&target)).is_none());
        }
        assert!(is_storm_reject(
            guard.action_for(std::slice::from_ref(&target))
        ));
        assert!(matches!(
            guard.action_for(&[target]),
            Some(ReadStormAction::Stop(_))
        ));
    }

    #[test]
    fn read_storm_guard_decays_when_path_is_not_seen() {
        let mut guard = ReadStormGuard::default();
        for _ in 0..READ_STORM_TARGET_TURN_LIMIT {
            assert!(
                guard
                    .action_for(&[call("read", r#"{"path":"src/tool/agent.rs"}"#)])
                    .is_none()
            );
        }
        for _ in 0..READ_STORM_TARGET_TURN_LIMIT {
            assert!(
                guard
                    .action_for(&[call("read", r#"{"path":"src/other.rs"}"#)])
                    .is_none()
            );
        }
        assert!(
            guard
                .action_for(&[call("read", r#"{"path":"src/tool/agent.rs"}"#)])
                .is_none(),
            "a path that has not been seen for several turns should decay instead of tripping"
        );
    }

    #[test]
    fn read_storm_rejection_only_blocks_the_stormed_target() {
        let stormed = HashSet::from(["read src/tool/agent.rs".to_string()]);
        let rejections = ToolRejections {
            read_storm: Some(stormed),
            ..Default::default()
        };

        // The stormed file is blocked, and the message names that file.
        let blocked = rejections
            .message_for(&call(
                "read",
                r#"{"path":"src/tool/agent.rs","offset":480}"#,
            ))
            .expect("stormed target should be rejected");
        assert!(blocked.contains("src/tool/agent.rs"), "message: {blocked}");
        assert!(blocked.contains("symbol_search"), "message: {blocked}");
        assert!(blocked.contains("read_symbol"), "message: {blocked}");
        assert!(
            blocked.contains("RuntimeEventDrainDeps"),
            "message should include a concrete navigation example: {blocked}"
        );

        // A sibling read of a *different* file in the same batch passes through
        // instead of inheriting the stormed file's message.
        assert_eq!(
            rejections.message_for(&call("read", r#"{"path":"src/other.rs"}"#)),
            None
        );
        // Non-read tools and non-diff git ops are never storm-limited.
        assert_eq!(
            rejections.message_for(&call("git", r#"{"op":"status"}"#)),
            None
        );
        assert_eq!(
            rejections.message_for(&call("plan_add_finding", "{}")),
            None
        );
        assert_eq!(
            rejections.message_for(&call("agent", r#"{"agent":"review","prompt":"x"}"#)),
            None
        );
    }

    #[test]
    fn mutation_paths_reads_single_write_path() {
        let paths = mutation_paths(&call("write", r#"{"path":"src/lib.rs","content":"x"}"#));
        assert_eq!(paths, vec!["src/lib.rs"]);
    }

    #[test]
    fn bash_keeps_inspection_output_but_compacts_builds() {
        // Inspection commands whose stdout is the evidence: keep it whole even
        // when it is huge.
        for command in [
            "git status --porcelain",
            "git diff --stat",
            "git show HEAD",
            "git log --oneline -5",
            "cargo test --locked",
            "python3 - <<'PY'\nprint(1)\nPY",
            "sqlite3 db.sqlite \"SELECT 1\"",
            "jq '.foo' file.json",
            "grep needle src",
        ] {
            assert!(
                bash_command_should_keep_output(command),
                "should keep output for: {command}"
            );
        }

        // Build noise and non-inspection git ops carry no keep-whole override:
        // they stay verbatim while small and are excerpted only above the
        // size threshold.
        for command in [
            "cargo build",
            "git commit -m wip",
            "git push",
            "npm run build",
        ] {
            assert!(
                !bash_command_should_keep_output(command),
                "no keep-whole override expected for: {command}"
            );
        }
    }

    fn command_output(truncation: Option<crate::tool::OutputTruncationContext>) -> ToolOutput {
        ToolOutput::Command {
            rendered: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
            truncation,
        }
    }

    /// `rustc --version` was stubbed to "output compacted" and
    /// the model spent eight turns smuggling the version string through xxd,
    /// /tmp, and a scratch file. Small successful output must reach the model
    /// verbatim no matter what the command was.
    #[test]
    fn small_successful_command_output_stays_verbatim() {
        let rendered = "rustc 1.96.1 (31fca3adb 2026-06-26)\n\n[Command summary]\ncommand: rustc --version\nexit_code: 0\n";
        let compacted = compact_successful_command_model_content(
            &call("bash", r#"{"command":"rustc --version"}"#),
            &command_output(None),
            rendered,
            true,
        );
        assert_eq!(compacted, None, "small output must never be stubbed");

        // Small build output too — no command-name amputation.
        let compacted = compact_successful_command_model_content(
            &call("bash", r#"{"command":"cargo build"}"#),
            &command_output(None),
            rendered,
            true,
        );
        assert_eq!(compacted, None);
    }

    #[test]
    fn huge_successful_command_output_is_excerpted_head_and_tail() {
        let mut body = String::new();
        for line_number in 0..2_000 {
            body.push_str(&format!("build log line {line_number}\n"));
        }
        let rendered = format!(
            "{body}\n[Command summary]\ncommand: cargo build\nexit_code: 0\nlast_output:\nbuild log line 1999"
        );
        let compacted = compact_successful_command_model_content(
            &call("bash", r#"{"command":"cargo build"}"#),
            &command_output(None),
            &rendered,
            true,
        )
        .expect("output above the size threshold is excerpted");

        assert!(compacted.starts_with("build log line 0"), "{compacted}");
        assert!(compacted.contains("output chars elided"), "{compacted}");
        assert!(compacted.contains("build log line 1999"), "{compacted}");
        assert!(
            compacted.contains("[Command summary]"),
            "summary footer must survive verbatim: {compacted}"
        );
        assert!(
            !compacted.contains("transcript card"),
            "must never point the model at a UI-only surface: {compacted}"
        );
        assert!(
            compacted.chars().count() < rendered.chars().count() / 2,
            "excerpting should reclaim most of the output"
        );

        // The keep-whole override still protects huge inspection payloads.
        let kept = compact_successful_command_model_content(
            &call("bash", r#"{"command":"grep -r needle src"}"#),
            &command_output(None),
            &rendered,
            true,
        );
        assert_eq!(
            kept, None,
            "inspection output is kept whole regardless of size"
        );
    }

    /// Harness for the watcher tests: a runner whose provider future never
    /// resolves, plus one tracked run parked on it forever with its own,
    /// never-cancelled token (standing in for a detached subagent whose token
    /// is not a child of the parent run's).
    async fn parked_subagent_run() -> (
        crate::tool::SubagentRunner,
        Arc<crate::subagent::SubagentRegistry>,
        String,
    ) {
        let factory: crate::tool::SubagentProviderFactory =
            Arc::new(|_agent, _chain| Box::pin(std::future::pending()));
        let subagents = Arc::new(crate::subagent::SubagentRegistry::new());
        let runner = crate::tool::SubagentRunner::new(
            factory,
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            crate::tool::ReadTracker::new(),
            Arc::new(crate::tool::ProjectInfoRuntime::new(None)),
            subagents.clone(),
            std::env::temp_dir(),
        );
        tokio::spawn({
            let runner = runner.clone();
            async move {
                runner
                    .run_self_review(
                        "instructions",
                        "wait",
                        Arc::new(ToolRegistry::new()),
                        CancellationToken::new(),
                        None,
                    )
                    .await
            }
        });
        // One yield lets the spawned run register itself and park on the
        // never-resolving provider future.
        tokio::task::yield_now().await;
        let subtask_id = subagents
            .list()
            .into_iter()
            .next()
            .expect("run should be registered")
            .id;
        (runner, subagents, subtask_id.to_string())
    }

    #[tokio::test]
    async fn cancel_watcher_drop_sweeps_subagents_when_cancel_races_run_exit() {
        // Regression: the parent run can observe cancellation and exit before
        // the watcher task is ever scheduled. Dropping the watcher used to
        // just abort that task, silently skipping the sweep — a subagent whose
        // token is not a child of the parent's then kept running forever after
        // "Run interrupted".
        let (runner, subagents, subtask_id) = parked_subagent_run().await;

        let token = CancellationToken::new();
        let watcher = SubagentCancelWatcher::spawn(Some(runner), token.clone());
        // No await between spawn, cancel, and drop: on the current-thread test
        // runtime the watcher task cannot have been polled, so only the Drop
        // sweep can stop the parked run.
        token.cancel();
        drop(watcher);

        assert_eq!(
            subagents
                .snapshot(&subtask_id)
                .expect("run should be registered")
                .status,
            crate::subagent::SubagentStatus::Cancelled,
            "dropping a cancelled watcher must sweep running subagents"
        );
    }

    #[tokio::test]
    async fn cancel_watcher_drop_without_cancellation_leaves_subagents_running() {
        // A run that finishes without being cancelled must NOT kill running
        // subagents — detached ones are designed to outlive the turn and
        // report through the background wake path.
        let (runner, subagents, subtask_id) = parked_subagent_run().await;

        let token = CancellationToken::new();
        let watcher = SubagentCancelWatcher::spawn(Some(runner), token.clone());
        drop(watcher);

        assert_eq!(
            subagents
                .snapshot(&subtask_id)
                .expect("run should be registered")
                .status,
            crate::subagent::SubagentStatus::Running,
            "an uncancelled run exit must not stop running subagents"
        );
    }

    #[test]
    fn only_known_slow_verification_commands_auto_background() {
        assert!(known_slow_verification("cargo test --locked"));
        assert!(known_slow_verification(
            "cargo clippy --all-targets --all-features -- -D warnings"
        ));
        assert!(known_slow_verification("cargo build --release --locked"));
        assert!(!known_slow_verification("cargo check --locked"));
        assert!(!known_slow_verification("cargo clippy -p small"));
        assert!(!known_slow_verification("cargo build"));
    }
}
