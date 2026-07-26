use super::*;

pub(super) async fn run(
    agent: &mut Agent,
    cancellation_token: CancellationToken,
    sink: SharedSink,
    queued_messages: Option<&mut mpsc::UnboundedReceiver<QueuedUserMessageCommand>>,
) -> Result<AgentRunResult> {
    TurnCoordinator::new(agent, cancellation_token, sink, queued_messages)
        .run()
        .await
}

/// Owns the mutable state and policy decisions for one agent run.
///
/// `Agent` remains the long-lived dependency container and public façade. The
/// coordinator is deliberately short-lived: one instance advances one run
/// through model and tool turns until it produces a terminal outcome.
struct TurnCoordinator<'agent, 'receiver> {
    agent: &'agent mut Agent,
    cancellation_token: CancellationToken,
    sink: SharedSink,
    queued_messages: Option<&'receiver mut mpsc::UnboundedReceiver<QueuedUserMessageCommand>>,
    pending_queued_messages: VecDeque<QueuedUserMessage>,
    cancelled_queued_message_ids: HashSet<u64>,
    tool_schema: ToolSchemaPayload,
    tool_registry: Arc<ToolRegistry>,
    state: TurnState,
}

#[derive(Default)]
struct TurnState {
    turns_started: usize,
    policies: TurnPolicies,
}

impl TurnState {
    fn begin_turn(&mut self) {
        self.turns_started = self.turns_started.saturating_add(1);
    }
}

#[derive(Default)]
struct TurnPolicies {
    planning_research: PlanningResearchGuard,
    repeated_inspection: RepeatedInspectionGuard,
    read_storm: ReadStormGuard,
    repeated_failure: RepeatedFailureGuard,
    single_call_streak: SingleCallStreakGuard,
    serial_delegation: SerialDelegationGuard,
    implementation_stall: ImplementationStallGuard,
    empty_response_nudges: usize,
}

impl TurnPolicies {
    fn reset_for_user_steering(&mut self) {
        self.planning_research.reset();
        self.repeated_inspection.reset();
        self.read_storm.reset();
        self.repeated_failure.reset();
        self.single_call_streak = SingleCallStreakGuard::default();
        self.serial_delegation = SerialDelegationGuard::default();
        self.implementation_stall.reset();
        self.empty_response_nudges = 0;
    }

    fn observe_tool_execution(&mut self, execution: &ToolExecutionOutcome) {
        self.repeated_failure
            .observe(&execution.tool_observations, execution.reset_loop_guards);
        if execution.reset_loop_guards {
            self.planning_research.reset();
            self.repeated_inspection.reset();
            self.read_storm.reset();
            self.single_call_streak = SingleCallStreakGuard::default();
            self.serial_delegation = SerialDelegationGuard::default();
            self.implementation_stall.reset();
            self.empty_response_nudges = 0;
        }
    }
}

#[derive(Debug)]
enum TurnOutcome {
    Continue,
    Finished(AgentRunResult),
}

impl<'agent, 'receiver> TurnCoordinator<'agent, 'receiver> {
    fn new(
        agent: &'agent mut Agent,
        cancellation_token: CancellationToken,
        sink: SharedSink,
        queued_messages: Option<&'receiver mut mpsc::UnboundedReceiver<QueuedUserMessageCommand>>,
    ) -> Self {
        let tool_schema = agent.active_tool_schema();
        let tool_registry = agent.tool_registry.clone();
        Self {
            agent,
            cancellation_token,
            sink,
            queued_messages,
            pending_queued_messages: VecDeque::new(),
            cancelled_queued_message_ids: HashSet::new(),
            tool_schema,
            tool_registry,
            state: TurnState::default(),
        }
    }

    async fn run(mut self) -> Result<AgentRunResult> {
        self.snapshot_run_start_dirty_paths().await;
        let _subagent_cancel_watcher = SubagentCancelWatcher::spawn(
            self.agent.subagent_runner.clone(),
            self.cancellation_token.clone(),
        );
        let max_iterations = self.agent.budget.max_iterations;

        for _ in 0..max_iterations {
            self.state.begin_turn();
            match self.advance_turn().await? {
                TurnOutcome::Continue => {}
                TurnOutcome::Finished(result) => return Ok(result),
            }
        }

        debug_assert_eq!(self.state.turns_started, max_iterations);
        Err(anyhow::Error::new(
            crate::run_budget::RunBudgetExhaustion::MaxTurns {
                limit: max_iterations,
            },
        ))
    }

    async fn snapshot_run_start_dirty_paths(&mut self) {
        // Snapshot before the first volatile refresh so the git block can
        // distinguish baseline WIP from this run's own edits. A new run gets a
        // new baseline; eval/headless prompts remain deterministic when the
        // volatile context is disabled.
        if !self.agent.refresh_volatile {
            return;
        }
        let root = self.agent.project_root.clone();
        self.agent.run_start_dirty_paths =
            tokio::task::spawn_blocking(move || crate::context::dirty_worktree_paths(&root))
                .await
                .ok();
    }

    async fn advance_turn(&mut self) -> Result<TurnOutcome> {
        if self.cancellation_token.is_cancelled() {
            self.agent.cancel_running_subagents();
            return Ok(TurnOutcome::Finished(AgentRunResult::Interrupted(
                String::new(),
            )));
        }

        // Keep the volatile git state honest across round-trips: writes and
        // concurrent edits land mid-run and must be visible on the next turn.
        let volatile_changed = self.agent.refresh_volatile_project_state().await;
        if self.agent.refresh_background_context(&self.sink).await || volatile_changed {
            self.agent.emit_context_updated(&self.sink);
        }
        let user_steered = self
            .agent
            .drain_queued_messages(
                &mut self.queued_messages,
                &mut self.pending_queued_messages,
                &mut self.cancelled_queued_message_ids,
                &self.sink,
            )
            .await?;
        if user_steered {
            self.state.policies.reset_for_user_steering();
            self.agent.set_planning_advisory(None);
        }

        self.agent.caches.last_perf_report = None;
        let deferred_sink = DeferredAssistantSink::new(self.sink.clone());
        let model_sink: SharedSink = deferred_sink.clone();
        let response = match self
            .agent
            .call_model(
                &self.tool_schema,
                &model_sink,
                self.cancellation_token.clone(),
            )
            .await?
        {
            ModelCallOutcome::Interrupted => {
                return Ok(TurnOutcome::Finished(AgentRunResult::Interrupted(
                    String::new(),
                )));
            }
            ModelCallOutcome::Response(response) => response,
        };

        if response.is_interrupted() {
            self.agent.log_response(&response, true);
            if !response.content.is_empty() {
                deferred_sink.flush();
                self.agent
                    .push_message(assistant_text_message(&response.content));
                self.agent.emit_context_updated(&self.sink);
            }
            return Ok(TurnOutcome::Finished(AgentRunResult::Interrupted(
                response.content,
            )));
        }

        if response.tool_calls.is_empty() {
            return self
                .finish_or_continue_without_tools(response, &deferred_sink)
                .await;
        }

        deferred_sink.flush();
        // A model action starts a fresh empty-response nudge budget.
        self.state.policies.empty_response_nudges = 0;
        self.execute_tool_turn(response).await
    }

    async fn finish_or_continue_without_tools(
        &mut self,
        response: StreamedResponse,
        deferred_sink: &DeferredAssistantSink,
    ) -> Result<TurnOutcome> {
        self.agent.log_response(&response, false);
        self.agent.log_turn_line(
            &[],
            None,
            self.state
                .policies
                .implementation_stall
                .turns_without_progress,
        );
        self.agent.set_planning_advisory(None);
        if response.content.trim().is_empty() {
            deferred_sink.discard();
            if self.state.policies.empty_response_nudges >= EMPTY_RESPONSE_NUDGE_LIMIT {
                bail!(
                    "Model produced {} consecutive empty turns (no tool calls, no answer text). \
                     The provider may be truncating very long reasoning — try a lower reasoning \
                     effort for this model, or re-run the task.",
                    self.state.policies.empty_response_nudges + 1
                );
            }
            self.state.policies.empty_response_nudges += 1;
            tracing::warn!(
                target: "bonsai::guard",
                guard = "empty_response",
                action = "nudge",
                nudges = self.state.policies.empty_response_nudges,
                "guard nudged model"
            );
            self.sink
                .status("Empty model turn (no output, no tool calls) — nudging it to act.");
            self.agent
                .push_harness_note(&empty_response_nudge_message());
            self.agent.emit_context_updated(&self.sink);
            return Ok(TurnOutcome::Continue);
        }
        if self.agent.refresh_background_context(&self.sink).await {
            deferred_sink.flush();
            self.agent
                .push_message(assistant_text_message(&response.content));
            self.agent.emit_context_updated(&self.sink);
            return Ok(TurnOutcome::Continue);
        }
        if self
            .agent
            .maybe_self_review(&self.sink, self.cancellation_token.clone())
            .await
        {
            deferred_sink.discard();
            self.agent.emit_context_updated(&self.sink);
            return Ok(TurnOutcome::Continue);
        }
        // Verification must observe any repair prompted by self-review.
        if self.cancellation_token.is_cancelled() {
            deferred_sink.discard();
            return Ok(TurnOutcome::Finished(AgentRunResult::Interrupted(
                String::new(),
            )));
        }
        if self
            .agent
            .maybe_verify_after_edit(&self.sink, self.cancellation_token.clone())
            .await
        {
            deferred_sink.discard();
            self.agent.emit_context_updated(&self.sink);
            return Ok(TurnOutcome::Continue);
        }
        if self.cancellation_token.is_cancelled() {
            deferred_sink.discard();
            return Ok(TurnOutcome::Finished(AgentRunResult::Interrupted(
                String::new(),
            )));
        }

        deferred_sink.flush();
        self.agent
            .push_message(assistant_text_message(&response.content));
        self.agent.finalize_pending_self_review(&response.content);
        self.agent.emit_context_updated(&self.sink);
        Ok(TurnOutcome::Finished(AgentRunResult::Completed(
            response.content,
        )))
    }

    async fn execute_tool_turn(&mut self, response: StreamedResponse) -> Result<TurnOutcome> {
        let policies = &mut self.state.policies;
        let planning_research_action = self
            .agent
            .planning_research_action(&response.tool_calls, &mut policies.planning_research);
        let planning_research_rejection = resolve_guard(
            self.agent,
            "planning_research",
            planning_research_action,
            &response,
        )?;
        let delegated_read_rejections = self.agent.delegated_read_rejections(&response.tool_calls);
        let read_target_versions = self.agent.read_target_versions(&response.tool_calls).await;
        let repeated_inspection_action = if self.agent.persona_planning_budget() {
            None
        } else {
            policies.repeated_inspection.action_for_with_versions(
                &response.tool_calls,
                self.agent.has_repair_advisory(),
                &read_target_versions,
            )
        };
        let repeated_inspection_rejection = resolve_guard(
            self.agent,
            "repeated_inspection",
            repeated_inspection_action,
            &response,
        )?;
        let read_storm_action = if self.agent.persona_planning_budget() {
            None
        } else {
            policies
                .read_storm
                .action_for_with_versions(&response.tool_calls, &read_target_versions)
        };
        let read_storm_rejection =
            resolve_guard(self.agent, "read_storm", read_storm_action, &response)?;
        let repeated_failure_action = policies.repeated_failure.action_for(&response.tool_calls);
        let repeated_failure_rejection = resolve_guard(
            self.agent,
            "repeated_failure",
            repeated_failure_action,
            &response,
        )?;
        let batching_hint = policies.single_call_streak.hint_for(&response.tool_calls);
        let serial_delegation_hint =
            policies
                .serial_delegation
                .hint_for(is_serial_read_only_delegation(
                    &response.tool_calls,
                    &self.tool_registry,
                ));
        // A hook, peer, or editor can change a file after request preflight
        // while the model is responding. Probe again at the reuse decision.
        self.agent.refresh_read_evidence_freshness().await;
        let precomputed_read = self.agent.preexecution_read_reuses(&response.tool_calls);
        let precomputed_read_delta = self.agent.preexecution_read_deltas(&response.tool_calls);
        let auto_background = self
            .agent
            .auto_background_verification_calls(&response.tool_calls);

        self.agent.push_message(build_assistant_message(&response)?);
        self.agent.emit_context_updated(&self.sink);
        self.agent.log_response(&response, false);

        let tool_execution = self
            .agent
            .execute_tool_calls(
                &response.tool_calls,
                &self.tool_registry,
                ToolRejections {
                    precomputed_read,
                    precomputed_read_delta,
                    auto_background,
                    planning_research: planning_research_rejection,
                    delegated_read: delegated_read_rejections,
                    repeated_inspection: repeated_inspection_rejection,
                    read_storm: read_storm_rejection,
                    repeated_failure: repeated_failure_rejection.unwrap_or_default(),
                },
                &self.sink,
                self.cancellation_token.clone(),
                QueuedMessageState {
                    receiver: &mut self.queued_messages,
                    pending: &mut self.pending_queued_messages,
                    cancelled_ids: &mut self.cancelled_queued_message_ids,
                },
            )
            .await?;
        self.state.policies.observe_tool_execution(&tool_execution);
        if tool_execution.reset_loop_guards {
            self.agent.set_planning_advisory(None);
        }
        // A steered turn never counts toward the stall: its queued user
        // message reset the policy state before execution.
        let implementation_stall_nudge =
            if !tool_execution.reset_loop_guards && self.agent.implementation_stall_guarded() {
                self.state
                    .policies
                    .implementation_stall
                    .observe_turn(&tool_execution.tool_observations)
            } else {
                None
            };
        let turn_made_progress = tool_execution
            .tool_observations
            .iter()
            .any(|observation| observation.makes_progress);
        self.agent.log_turn_line(
            &response.tool_calls,
            Some(turn_made_progress),
            self.state
                .policies
                .implementation_stall
                .turns_without_progress,
        );

        if tool_execution.interrupted_mid_tools {
            self.agent.cancel_running_subagents();
            return Ok(TurnOutcome::Finished(AgentRunResult::Interrupted(
                String::new(),
            )));
        }
        if !tool_execution.detached_subagent_ids.is_empty() {
            return Ok(TurnOutcome::Finished(AgentRunResult::Waiting(
                WaitReason::Subagents(tool_execution.detached_subagent_ids),
            )));
        }
        if let Some(reason) = tool_execution.wait_reason {
            return Ok(TurnOutcome::Finished(AgentRunResult::Waiting(reason)));
        }
        if let Some(blocker) = self.agent.verification_blocker() {
            bail!(blocker.to_string());
        }

        // Harness notes are appended after tool results so they remain the
        // final model-visible message without invalidating the prompt prefix.
        if let Some(nudge) = implementation_stall_nudge {
            tracing::warn!(
                target: "bonsai::guard",
                guard = "implementation_stall",
                action = "nudge",
                turns = self
                    .state
                    .policies
                    .implementation_stall
                    .turns_without_progress,
                "guard nudged model"
            );
            if nudge.status == ImplementationStallStatus::Show {
                self.sink.status(IMPLEMENTATION_STALL_STATUS_MESSAGE);
            }
            self.agent.push_harness_note(&nudge.note);
            self.agent.emit_context_updated(&self.sink);
        } else if let Some(hint) = serial_delegation_hint {
            tracing::info!(
                target: "bonsai::guard",
                guard = "serial_delegation",
                action = "nudge",
                "guard nudged model to fan out delegations"
            );
            self.agent.push_harness_note(hint);
            self.agent.emit_context_updated(&self.sink);
        } else if let Some(hint) = batching_hint {
            tracing::info!(
                target: "bonsai::guard",
                guard = "single_call_streak",
                action = "nudge",
                "guard nudged model to batch calls"
            );
            self.agent.push_harness_note(&hint);
            self.agent.emit_context_updated(&self.sink);
        }

        Ok(TurnOutcome::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_state_counts_started_turns() {
        let mut state = TurnState::default();

        state.begin_turn();
        state.begin_turn();

        assert_eq!(state.turns_started, 2);
    }

    #[test]
    fn user_steering_resets_transient_policy_state() {
        let mut policies = TurnPolicies {
            empty_response_nudges: 2,
            ..TurnPolicies::default()
        };
        policies.implementation_stall.turns_without_progress = 5;
        policies.single_call_streak.streak = 4;

        policies.reset_for_user_steering();

        assert_eq!(policies.empty_response_nudges, 0);
        assert_eq!(policies.implementation_stall.turns_without_progress, 0);
        assert_eq!(policies.single_call_streak.streak, 0);
    }
}
