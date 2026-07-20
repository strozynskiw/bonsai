use super::*;

impl Agent {
    pub(crate) fn can_retry_last_turn(&self) -> bool {
        self.last_retryable_turn
    }

    /// Arm a new attempt of the latest human turn without adding the user
    /// prompt to context a second time.
    ///
    /// # Errors
    ///
    /// Returns an error when this context has no human turn to retry.
    pub(crate) async fn begin_retry_last_turn(&mut self) -> Result<()> {
        if !self.last_retryable_turn {
            bail!("No user turn is available to retry in this conversation.");
        }
        if let Some(bus) = &self.peer_bus {
            bus.begin_turn(crate::peer::TurnOrigin::Human);
        }
        self.set_planning_advisory(None);
        self.arm_self_review_for_coding_task("retry the latest user request")
            .await;
        self.push_harness_note(
            "Retry the latest human turn. The previous attempt was unsuccessful or unsatisfactory. Keep the original user request as the source of truth, continue from the current workspace and evidence, avoid repeating completed work, and produce a fresh final answer.",
        );
        Ok(())
    }

    /// Reset the per-conversation transient state shared by [`Self::clear`],
    /// [`Self::implement_plan_from_with_context`], and [`Self::review_pending_changes`].
    ///
    /// Deliberately does **not** touch `messages` (each caller installs its own
    /// system/user messages), the read tracker, the todo store, or the session
    /// usage meter — those are reset selectively by each caller, so the reset
    /// order relative to message installation does not matter.
    fn reset_transient_state(&mut self) {
        self.last_background_status_report = None;
        self.last_terminal_status_report = None;
        self.pending_peer_delivery_receipts.clear();
        self.read_coverage_advisory.clear();
        self.subagent_status_advisory.clear();
        self.tool_context_details.clear();
        self.inspection_events.clear();
        self.mention_read_evidence.clear();
        self.delegated_read_evidence.clear();
        self.delegated_overlap_advised.clear();
        self.context_controls.clear();
        self.summary_sources.clear();
        self.compaction_events.clear();
        self.pending_context_rewrite = PendingContextRewrite::default();
        self.last_prompt_estimate = None;
        self.last_sent_prompt_estimate = None;
        self.last_perf_report = None;
        self.previous_request_body = None;
        self.run_start_dirty_paths = None;
        self.rebuild_project_system_context();
    }

    pub async fn clear(&mut self) {
        // Episode hard boundary: must run before the message buffer is
        // replaced, or the active span's end could never be resolved.
        self.close_active_episode_for_hard_boundary();
        self.reset_context_messages(self.system_message_for_mode(self.mode));
        self.read_tracker.clear().await;
        if let Some(store) = &self.todo_store {
            store.lock().await.clear();
        }
        // Memory recall dedup resets only here (`/new`, `/clear`) — the
        // transient-state reset below also runs for `/start`/`/review`, which
        // continue the same conversation and must keep the dedup set.
        if let Some(memory) = &self.memory {
            memory.clear_recalled();
        }
        self.reset_transient_state();
        self.usage = SessionUsage::default();
        // Loaded skill bodies are gone with the conversation; start fresh.
        self.loaded_skills.clear();
    }

    #[cfg(test)]
    pub async fn restore_text_history(&mut self, messages: &[(String, String)]) -> Result<()> {
        self.restore_text_history_inner(messages).await;
        Ok(())
    }

    pub(crate) async fn restore_persisted_text_history(&mut self, messages: &[(String, String)]) {
        self.restore_text_history_inner(messages).await;
    }

    async fn restore_text_history_inner(&mut self, messages: &[(String, String)]) {
        self.clear().await;
        for (role, content) in messages {
            match role.as_str() {
                "user" => {
                    self.push_user_message_raw(content);
                    self.last_retryable_turn = true;
                }
                "assistant" => {
                    self.push_message(assistant_text_message(content));
                }
                other => {
                    tracing::warn!(
                        role = other,
                        "restore_text_history: dropping message with unsupported role"
                    );
                }
            }
        }
        self.rebuild_recalled_memory_from_messages();
    }

    #[cfg(test)]
    pub fn context_messages(&self) -> &[ChatCompletionRequestMessage] {
        &self.messages
    }

    #[cfg(test)]
    pub async fn restore_context_messages(
        &mut self,
        messages: Vec<ChatCompletionRequestMessage>,
    ) -> Result<()> {
        self.restore_context_messages_with_ids_inner_async(messages, Vec::new())
            .await;
        Ok(())
    }

    async fn restore_context_messages_with_ids_inner_async(
        &mut self,
        messages: Vec<ChatCompletionRequestMessage>,
        ids: Vec<String>,
    ) {
        self.clear().await;
        if messages.is_empty() {
            return;
        }
        self.restore_context_messages_with_ids_inner(messages, ids);
        self.normalize_project_state_cache_strategy(ProjectStateNormalization::RestoredHistory);
        self.rebuild_recalled_memory_from_messages();
    }

    #[cfg(test)]
    pub(crate) async fn restore_context_messages_with_ids(
        &mut self,
        messages: Vec<ChatCompletionRequestMessage>,
        ids: Vec<String>,
    ) -> Result<()> {
        self.restore_context_messages_with_ids_inner_async(messages, ids)
            .await;
        Ok(())
    }

    pub(crate) async fn restore_persisted_context_messages_with_ids(
        &mut self,
        messages: Vec<ChatCompletionRequestMessage>,
        ids: Vec<String>,
    ) {
        self.restore_context_messages_with_ids_inner_async(messages, ids)
            .await;
    }

    pub fn restore_usage_totals(
        &mut self,
        prompt_tokens: u64,
        completion_tokens: u64,
        cost_micros: Option<u64>,
        no_cache_cost_micros: Option<u64>,
        input_cache: Option<InputCacheUsage>,
    ) {
        self.usage.restore(
            prompt_tokens,
            completion_tokens,
            cost_micros,
            no_cache_cost_micros,
            input_cache,
        );
    }

    pub(crate) fn restore_usage_turns(&mut self, turns: Vec<UsageTurn>) {
        self.usage.restore_turns(turns);
    }

    pub fn usage_totals(&self) -> UsageTotals {
        self.usage.totals()
    }

    pub(crate) fn session_budget_usage(&self) -> crate::run_budget::SessionBudgetUsage {
        crate::run_budget::SessionBudgetUsage {
            turns: self.usage.session_turn_count(),
            turn_limit: self.max_session_turns,
            output_chars: self.usage.session_output_chars(),
            output_char_limit: self.max_session_output_chars,
            active_run_ms: 0,
            active_limit_seconds: self.max_session_active_seconds,
            exact_cost_micros: self.usage.exact_session_cost_micros(),
            cost_limit_micros: self.max_session_cost_micros,
        }
    }

    pub(crate) fn usage_turns(&self) -> &[UsageTurn] {
        &self.usage.usage_turns
    }

    pub(super) fn absorb_usage_totals(&mut self, totals: UsageTotals) {
        self.usage.absorb_totals(totals);
    }

    pub(super) fn absorb_usage_turns(&mut self, turns: &[UsageTurn]) {
        self.usage.absorb_turns(turns);
    }

    pub(crate) fn set_execution_lane(&mut self, lane: ExecutionLane) {
        if self.execution_lane != lane {
            self.execution_lane = lane;
            self.previous_request_body = None;
        }
    }

    pub(super) fn session_input_cache(&self) -> Option<InputCacheUsage> {
        self.usage.input_cache()
    }

    pub fn set_todo_store(&mut self, todo_store: SharedTodoStore) {
        self.todo_store = Some(todo_store);
    }

    pub fn set_background_tasks(&mut self, background_tasks: Arc<BackgroundTaskRegistry>) {
        self.background_tasks = background_tasks;
        self.last_background_status_report = None;
    }

    pub(crate) fn set_terminals(&mut self, terminals: Arc<crate::terminal::TerminalRegistry>) {
        self.terminals = terminals;
        self.last_terminal_status_report = None;
    }

    /// Wire the inter-agent communication bus (peers P2). `None` (the default)
    /// disables peer-message injection — evals and tests never see it.
    pub fn set_peer_bus(&mut self, peer_bus: Arc<crate::peer::PeerBus>) {
        self.peer_bus = Some(peer_bus);
    }

    /// Wire persistent memory after construction — the builder mirror
    /// for test agents assembled with `Agent::new` (TUI/headless use `.memory()`).
    #[cfg(test)]
    pub(crate) fn set_memory(&mut self, memory: Arc<crate::memory::MemoryService>) {
        self.memory = Some(memory);
    }

    pub async fn background_tasks_report(&self) -> String {
        let (background, terminals) =
            tokio::join!(self.background_tasks.list(), self.terminals.list());
        let mut report = crate::background::format_task_list(&background);
        if !terminals.is_empty() {
            report.push_str("\n\n");
            report.push_str(&crate::terminal::format_terminal_list(&terminals));
        }
        report
    }

    pub async fn stop_background_task(&self, task_id: &str) -> Result<String> {
        if task_id.starts_with("pty-") {
            Ok(self.terminals.stop(task_id).await?.detail())
        } else {
            Ok(self.background_tasks.stop(task_id).await?.detail())
        }
    }

    /// Begin an "implement plan" session.
    ///
    /// The user has finished planning and is asking the agent to execute
    /// the plan on the canvas. We:
    ///
    /// 1. Reset the conversation so the agent doesn't get distracted
    ///    by the planning history. The system prompt is rebuilt with
    ///    the project context for the current cwd (matching the
    ///    initial `run`/refresh behaviour).
    /// 2. Seed the todo list with the plan's tasks when present
    ///    (preserving order, promoting the first open task to
    ///    `in_progress`). The model now has a real checklist to work
    ///    through instead of having to re-derive one from the prompt.
    /// 3. Ship a single user message describing what to do. The
    ///    rendered plan is appended so the model sees the same view
    ///    the user is looking at, even if it ignored earlier turns
    ///    where the plan was built.
    /// 4. Return `true` if the plan had any content; the TUI uses this
    ///    to decide whether to dispatch the implementation run.
    ///
    /// `phase` selects which phase of a phased plan to implement (`None` for a
    /// flat plan, which seeds the whole checklist). For a phase, only that
    /// phase's todos are seeded and the instruction scopes the agent to it,
    /// while the full plan markdown is still appended for context.
    #[cfg(test)]
    pub async fn implement_plan_from(
        &mut self,
        plan: &crate::plan::PlanDoc,
        phase: Option<usize>,
    ) -> bool {
        self.implement_plan_from_with_context(plan, phase, PlanContextMode::Clean)
            .await
    }

    pub async fn implement_plan_from_with_context(
        &mut self,
        plan: &crate::plan::PlanDoc,
        phase: Option<usize>,
        context_mode: PlanContextMode,
    ) -> bool {
        let _ = self.set_mode(AgentMode::Coding);
        let phase_name = phase.and_then(|idx| plan.phases.get(idx).map(|p| p.name.clone()));
        let tasks = match phase {
            Some(idx) => plan.phase_todo_items(idx),
            None => plan.tasks_as_todo_items(),
        };
        let has_tasks = !tasks.is_empty();
        let has_plan = !plan.is_empty();
        let todo_block = implementation_todo_block(&tasks);

        if context_mode == PlanContextMode::Clean {
            self.close_active_episode_for_hard_boundary();
            // Build the system message the same way the initial run() does,
            // so the model gets fresh project context for the cwd.
            self.reset_context_messages(self.system_message_for_mode(self.mode));
            self.read_tracker.clear().await;
        }
        self.reset_transient_state();

        if let Some(store) = &self.todo_store {
            let mut store = store.lock().await;
            if has_tasks {
                store.set_todos(tasks);
            } else {
                store.clear();
            }
        }

        if has_plan {
            // Compose the user message. We always include the plan
            // markdown (cheap, gives the model a canonical view) and
            // append a brief instruction so the model knows the user
            // is asking it to execute, not re-plan.
            let intro = match &phase_name {
                Some(name) => format!(
                    "The plan on the canvas is ready. Implement only this phase — {name} — then stop; the remaining phases run afterward."
                ),
                None => "The plan on the canvas is ready. Please implement it.".to_string(),
            };
            // Structured review findings ride as their own predictable section,
            // built from `plan.findings` rather than the plan prose — so a
            // finding can never be silently dropped on handoff. Emitted only
            // when unresolved findings exist.
            let findings_section = {
                let block = plan.findings_handoff_block();
                if block.is_empty() {
                    String::new()
                } else {
                    format!("\n\n{block}")
                }
            };
            let body = format!(
                "{intro}\n\n{todo_block}\n\nUse todowrite explicitly: before editing files, call todowrite with the implementation todo list above, preserving the statuses and keeping at most one item in_progress. Keep todowrite updated as you complete work. If a Review findings section is present, include those fixes in todowrite progress and the final summary; do not mutate the plan canvas to mark them done.{findings_section}\n\nPlan markdown:\n\n{}",
                plan.to_markdown_for_handoff()
            );
            self.arm_self_review_for_coding_task(&body).await;
            self.push_message(user_text_message(&body));
            // The implementation episode opens on the seeded handoff message.
            self.open_episode_for_seeded_message(&intro);
        }

        has_plan
    }

    /// Begin a focused coding session seeded by a generated workflow prompt.
    ///
    /// This resets conversation context like `/start` so command workflows do not
    /// inherit unrelated chat history, then installs `prompt` as the sole user
    /// message for the next run.
    pub(crate) async fn begin_focused_coding_run(&mut self, prompt: &str) {
        let _ = self.set_mode(AgentMode::Coding);
        self.reset_for_hard_boundary().await;
        self.arm_self_review_for_coding_task(prompt).await;
        self.push_message(user_text_message(prompt));
    }

    /// Clear conversation context, read tracking, and transient run state at a
    /// hard boundary (fresh session, review, focused run). Rebuilds the system
    /// message for the current mode so the model gets fresh project context;
    /// the caller must set the target mode first.
    async fn reset_for_hard_boundary(&mut self) {
        self.close_active_episode_for_hard_boundary();
        self.reset_context_messages(self.system_message_for_mode(self.mode));
        self.read_tracker.clear().await;
        self.reset_transient_state();
    }

    /// Enter Review mode over a freshly captured diff, seeding the next run with
    /// a prompt built by `build_prompt`. Returns false when there is nothing to
    /// review. Shared by the standard and security review entry points.
    async fn seed_review(
        &mut self,
        scope: ReviewScope,
        build_prompt: impl FnOnce(ReviewScope, &CapturedDiff) -> String,
    ) -> bool {
        let diff: CapturedDiff = match capture_diff(&self.project_root, scope).await {
            Some(diff) => diff,
            None => return false,
        };
        let _ = self.set_mode(AgentMode::Review);
        self.reset_for_hard_boundary().await;
        let body = build_prompt(scope, &diff);
        self.push_message(user_text_message(&body));
        true
    }

    pub async fn review_pending_changes(&mut self, scope: ReviewScope) -> bool {
        self.seed_review(scope, review_prompt).await
    }

    /// Begin a focused security audit over a captured diff using the enforced
    /// read-only review registry.
    pub async fn security_review_pending_changes(&mut self, scope: ReviewScope) -> bool {
        self.seed_review(scope, security_review_prompt).await
    }

    pub fn set_provider(
        &mut self,
        mut provider: Box<dyn Provider>,
        context_budget_tokens: usize,
        prompt_estimator: PromptEstimator,
        identity: ActiveModelIdentity,
    ) {
        provider.set_conversation_cache_key(&self.conversation_cache_key);
        self.provider = provider;
        // IMPORTANT CACHE STRATEGY BOUNDARY: switching providers may switch
        // how volatile state is represented. Normalize immediately so Codex's
        // append-only snapshots never leak into Qwen/GLM/Anthropic requests,
        // and their mutable system tail never rewrites a Codex cache lineage.
        self.normalize_project_state_cache_strategy(ProjectStateNormalization::ProviderSwitch);
        self.active_model_identity = Some(identity);
        self.set_context_budget_tokens(context_budget_tokens);
        self.prompt_estimator = prompt_estimator;
        self.clear_tool_schema_cache();
        self.last_prompt_estimate = None;
        self.last_sent_prompt_estimate = None;
        self.last_perf_report = None;
        self.previous_request_body = None;
        self.cached_models.clear();
    }

    /// Install a one-shot backup used only by delegated subagents.
    pub(crate) fn set_provider_fallback(&mut self, fallback: crate::tool::SubagentProviderConfig) {
        self.provider_fallback = Some(fallback);
    }

    /// Switch permanently to the configured backup for the rest of this run.
    /// Returns its display label when a backup was available.
    pub(super) fn activate_provider_fallback(&mut self) -> Option<String> {
        let mut fallback = self.provider_fallback.take()?;
        self.provider_fallback = fallback.backup.take().map(|backup| *backup);
        let label = fallback.model.clone();
        match fallback.active_model_identity {
            Some(identity) => self.set_provider(
                fallback.provider,
                fallback.context_budget_tokens,
                fallback.prompt_estimator,
                identity,
            ),
            None => {
                self.provider = fallback.provider;
                self.normalize_project_state_cache_strategy(
                    ProjectStateNormalization::ProviderSwitch,
                );
                self.active_model_identity = None;
                self.set_context_budget_tokens(fallback.context_budget_tokens);
                self.set_prompt_estimator(fallback.prompt_estimator);
            }
        }
        Some(label)
    }

    /// Apply the durable cache-routing key owned by the persisted session.
    /// Empty keys are ignored so corrupt legacy state cannot collapse unrelated
    /// conversations onto one provider route.
    pub(crate) fn set_conversation_cache_key(&mut self, key: &str) {
        if key.trim().is_empty() {
            return;
        }
        self.provider.set_conversation_cache_key(key);
        self.conversation_cache_key.clear();
        self.conversation_cache_key.push_str(key);
    }

    pub fn set_prompt_estimator(&mut self, prompt_estimator: PromptEstimator) {
        self.prompt_estimator = prompt_estimator;
        self.clear_tool_schema_cache();
        self.last_prompt_estimate = None;
        self.last_sent_prompt_estimate = None;
        self.last_perf_report = None;
        self.previous_request_body = None;
    }

    pub fn set_context_budget_tokens(&mut self, context_budget_tokens: usize) {
        self.context_budget_tokens = context_budget_tokens.max(1);
        self.refresh_effective_smol_profile();
    }
}
