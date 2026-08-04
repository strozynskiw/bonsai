use super::*;
use crate::output::ToolExecutionStatus;
use crate::verification::{
    VerificationBinding, VerificationCheck, VerificationCheckRecord, VerificationCheckStatus,
    VerificationKind, VerificationProfile, VerificationReasoningEscalation, VerificationRunRecord,
    VerificationRunStatus, VerificationTerminalReason, VerificationWorkflow, VerifyAfterEdit,
    normalize_verification_command,
};

const MAX_VERIFICATION_REPAIR_ATTEMPTS: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
enum VerificationRecoveryEvent {
    Repair {
        command: String,
        signature: String,
        attempt: u32,
        reasoning_escalation: Option<VerificationReasoningEscalation>,
    },
    FlakyRerun {
        command: String,
        signature: String,
    },
    UnstablePass {
        command: String,
    },
}

impl VerificationRecoveryEvent {
    fn harness_note(&self) -> String {
        match self {
            Self::Repair {
                command,
                signature,
                attempt,
                reasoning_escalation,
            } => {
                let escalation = reasoning_escalation
                    .as_ref()
                    .map_or_else(String::new, |event| {
                        format!(
                            "\n- Request-local reasoning: {} -> {} (saved model setting unchanged)",
                            event.from, event.to
                        )
                    });
                format!(
                    "Typed verification recovery event: recoverable_failure\n\
                 - Command: {command}\n\
                 - Failure signature: {signature}\n\
                 - Repair attempt: {attempt}/{MAX_VERIFICATION_REPAIR_ATTEMPTS}{escalation}\n\
                 Make one focused repair using the diagnostic in the preceding tool result, then rerun the exact configured command. Do not broaden the task or repeat unchanged inspections."
                )
            }
            Self::FlakyRerun { command, signature } => format!(
                "Typed verification recovery event: suspected_flaky\n\
                 - Command: {command}\n\
                 - Changed failure signature: {signature}\n\
                 The workspace did not change between failures. Rerun this exact command once without editing. A pass will be recorded as unstable; another failure will stop the run."
            ),
            Self::UnstablePass { command } => format!(
                "Typed verification recovery event: unstable_pass\n\
                 - Command: {command}\n\
                 The check passed on a no-change rerun after failing. Report it as unstable, continue only with remaining configured checks, and do not claim stable verification."
            ),
        }
    }
}

impl Agent {
    pub(super) fn begin_verification_observation_window(&mut self) {
        self.verification.observed_verification_run_indices.clear();
    }

    pub(super) async fn capture_pending_verification_bindings(
        &mut self,
        calls: &[ToolCall],
    ) -> HashMap<String, String> {
        let mut suppressed = HashMap::new();
        for call in calls {
            if call.name != "bash" {
                continue;
            }
            let Some(command) = bash_command(&call.arguments) else {
                continue;
            };
            if self.verification_kind_for_command(&command).is_none() {
                continue;
            }
            let fallback_cwd = bash_command_cwd(&self.project_root, &call.arguments);
            let command_cwd = match serde_json::from_str(&call.arguments) {
                Ok(arguments) => match self.tool_registry.get("bash") {
                    Some(tool) => tool.execution_cwd(&arguments).await.unwrap_or(fallback_cwd),
                    None => fallback_cwd,
                },
                Err(_) => fallback_cwd,
            };
            let binding =
                capture_verification_workspace_binding(&self.project_root, &command_cwd, &command)
                    .await;
            self.verification
                .pending_verification_bindings
                .insert(call.id.clone(), binding.clone());
            if self.has_deterministic_failure(&command, &binding) {
                self.verification
                    .suppressed_verification_calls
                    .insert(call.id.clone());
                suppressed.insert(
                    call.id.clone(),
                    format!(
                        "Verification blocked: {command:?} already produced the same deterministic failure for unchanged inputs."
                    ),
                );
            }
        }
        suppressed
    }

    /// Reset to a focused coding context and arm typed verification evidence.
    pub(crate) async fn begin_verification_run(
        &mut self,
        kind: VerificationKind,
        checks: &[VerificationCheck],
        prompt: &str,
    ) {
        self.begin_verification_observation_window();
        self.begin_focused_coding_run(prompt).await;
        self.arm_verification_record(kind, checks);
    }

    fn arm_verification_record(&mut self, kind: VerificationKind, checks: &[VerificationCheck]) {
        if let Some(active) = self.verification.active_verification.take()
            && let Some(record) = self
                .verification
                .verification_runs
                .get_mut(active.record_index)
        {
            record.status = VerificationRunStatus::Interrupted;
            record.finished_at_ms = Some(crate::util::time::now_ms());
        }
        let record_index = self.verification.verification_runs.len();
        self.verification
            .verification_runs
            .push(VerificationRunRecord::running(kind, checks));
        self.verification.active_verification = Some(ActiveVerificationRun {
            record_index,
            last_check_snapshot: None,
            last_failure_signature: None,
            flaky_rerun_used: false,
            unstable_observed: false,
            reasoning_override: None,
            pending_blocker: None,
        });
    }

    pub(super) fn reset_after_edit_verification(&mut self) {
        self.verification.after_edit_verification_pending = false;
        self.verification.after_edit_verification_injected = false;
    }

    pub(super) fn note_typed_verification_worthy_mutation(&mut self, paths: Vec<String>) {
        self.mark_latest_verification_stale(&paths);
        self.self_review.note_typed_mutation(paths);
        self.verification.after_edit_verification_pending = true;
    }

    pub(super) fn note_bash_window_verification_worthy_mutation(&mut self, paths: Vec<String>) {
        self.mark_latest_verification_stale(&paths);
        self.self_review.note_bash_window_mutation(paths);
        self.verification.after_edit_verification_pending = true;
    }

    fn mark_latest_verification_stale(&mut self, paths: &[String]) {
        if let Some(record_index) = self
            .verification
            .active_verification
            .as_ref()
            .map(|active| active.record_index)
        {
            let invalidated = self
                .verification
                .verification_runs
                .get_mut(record_index)
                .map(|record| {
                    let mut invalidated = Vec::new();
                    for check in &mut record.checks {
                        if check.status == VerificationCheckStatus::Passed {
                            check.status = VerificationCheckStatus::Pending;
                            check.delivered_binding = None;
                            invalidated.push(check.name.clone());
                        }
                    }
                    invalidated
                })
                .unwrap_or_default();
            if !invalidated.is_empty() {
                self.push_harness_note(&format!(
                    "The active verification gate was invalidated by a workspace mutation. After the focused repair succeeds, rerun these previously passing checks against the final workspace: {}.",
                    invalidated.join(", ")
                ));
            }
            return;
        }
        let mut invalidated = false;
        for record in &mut self.verification.verification_runs {
            if !matches!(
                record.status,
                VerificationRunStatus::Passed | VerificationRunStatus::Unstable
            ) {
                continue;
            }
            invalidated = true;
            record.status = VerificationRunStatus::Stale;
            record.observed_final_workspace = Some(false);
            for path in paths {
                if !record.workspace_changes_after_last_check.contains(path) {
                    record.workspace_changes_after_last_check.push(path.clone());
                }
            }
        }
        if invalidated {
            self.verification.after_edit_verification_injected = false;
        }
    }

    /// Inject one configured post-edit verification workflow before completion.
    /// Returns `true` when the run loop should continue for the verification turn.
    pub(super) async fn maybe_verify_after_edit(
        &mut self,
        sink: &SharedSink,
        cancellation_token: CancellationToken,
    ) -> bool {
        if !self.verification.after_edit_verification_pending
            || self.verification.after_edit_verification_injected
            || self.verification.active_verification.is_some()
            || self.mode != AgentMode::Coding
        {
            return false;
        }
        self.verification.after_edit_verification_pending = false;
        self.verification.after_edit_verification_injected = true;
        let policy = self.config.verification.after_edit;
        let stale_kind = self
            .verification
            .verification_runs
            .last()
            .filter(|run| run.status == VerificationRunStatus::Stale)
            .map(|run| run.kind);
        if policy == VerifyAfterEdit::Off && stale_kind.is_none() {
            self.record_skipped_profile(
                VerificationKind::Test,
                &[],
                VerificationTerminalReason::PolicyDisabled,
            );
            return false;
        }

        let profile = VerificationProfile::resolve(&self.project_root, &self.config.verification);
        let kind = if let Some(kind) = stale_kind {
            kind
        } else if !profile.tests.is_empty() {
            VerificationKind::Test
        } else if !profile.builds.is_empty() {
            VerificationKind::Build
        } else {
            sink.status("Post-edit verification skipped: no checks are configured or detected.");
            self.record_skipped_profile(
                VerificationKind::Test,
                &[],
                VerificationTerminalReason::Irrelevant,
            );
            return false;
        };
        let workflow = VerificationWorkflow {
            kind,
            checks: profile.checks(kind).to_vec(),
            prompt: match profile.workflow_prompt(kind) {
                Ok(prompt) => prompt,
                Err(message) => {
                    sink.status(&format!("Post-edit verification skipped: {message}"));
                    self.record_skipped_profile(
                        kind,
                        profile.checks(kind),
                        VerificationTerminalReason::EnvironmentBlocked,
                    );
                    return false;
                }
            },
        };

        if policy == VerifyAfterEdit::Ask
            && stale_kind.is_none()
            && let Err(reason) = self
                .confirm_after_edit_verification(kind, cancellation_token)
                .await
        {
            self.record_skipped_profile(kind, &workflow.checks, reason);
            return false;
        }

        let label = if stale_kind.is_some() {
            "Stale verification repair"
        } else {
            "Post-edit verification"
        };
        sink.status(&format!(
            "{label}: running the configured {} profile…",
            kind.label()
        ));
        self.arm_verification_record(kind, &workflow.checks);
        self.push_harness_note(&format!(
            "Post-edit verification gate. The coding work is complete; now execute this bounded verification-only workflow before finishing.\n\n{}",
            workflow.prompt
        ));
        true
    }

    async fn confirm_after_edit_verification(
        &self,
        kind: VerificationKind,
        cancellation_token: CancellationToken,
    ) -> Result<(), VerificationTerminalReason> {
        let Some(interaction) = self.interaction.as_ref() else {
            return Err(VerificationTerminalReason::EnvironmentBlocked);
        };
        let outcome = interaction
            .request_with_cancellation(
                |request_id| InteractionRequest::Question {
                    request_id,
                    prompt: format!(
                        "Run the configured {} profile before finishing?",
                        kind.label()
                    ),
                    header: Some("Verify changes".to_string()),
                    options: vec![
                        QuestionOption {
                            label: "Verify".to_string(),
                            description: "Run the configured checks now".to_string(),
                            preselected: false,
                        },
                        QuestionOption {
                            label: "Finish".to_string(),
                            description: "Skip verification for this turn".to_string(),
                            preselected: false,
                        },
                    ],
                    multiple: false,
                    origin: None,
                },
                cancellation_token,
            )
            .await;
        match outcome {
            Ok(InteractionOutcome::Question(Some(InteractionAnswer::Choices(choices))))
                if choices.first() == Some(&0) =>
            {
                Ok(())
            }
            Ok(InteractionOutcome::Question(Some(_))) => {
                Err(VerificationTerminalReason::UserSkipped)
            }
            Ok(_) => Err(VerificationTerminalReason::UserSkipped),
            Err(crate::interaction::InteractionStatus::Noninteractive) => {
                Err(VerificationTerminalReason::EnvironmentBlocked)
            }
            Err(crate::interaction::InteractionStatus::Cancelled) => {
                Err(VerificationTerminalReason::Cancelled)
            }
        }
    }

    pub(crate) fn verification_runs(&self) -> &[VerificationRunRecord] {
        &self.verification.verification_runs
    }

    fn verification_kind_for_command(&self, command: &str) -> Option<VerificationKind> {
        let normalized = normalize_verification_command(command);
        let active_kind = self
            .verification
            .active_verification
            .as_ref()
            .and_then(|active| self.verification.verification_runs.get(active.record_index))
            .and_then(|run| {
                run.checks
                    .iter()
                    .any(|check| normalize_verification_command(&check.command) == normalized)
                    .then_some(run.kind)
            });
        active_kind
            .or_else(|| {
                self.config
                    .verification
                    .test
                    .as_deref()
                    .is_some_and(|checks| {
                        checks
                            .iter()
                            .any(|check| normalize_verification_command(check) == normalized)
                    })
                    .then_some(VerificationKind::Test)
            })
            .or_else(|| {
                self.config
                    .verification
                    .build
                    .as_deref()
                    .is_some_and(|checks| {
                        checks
                            .iter()
                            .any(|check| normalize_verification_command(check) == normalized)
                    })
                    .then_some(VerificationKind::Build)
            })
            .or_else(|| observed_verification_kind(command))
    }

    fn has_deterministic_failure(&self, command: &str, binding: &VerificationBinding) -> bool {
        if !matches!(binding, VerificationBinding::Bound { .. }) {
            return false;
        }
        let normalized = normalize_verification_command(command);
        self.verification.verification_runs.iter().any(|run| {
            run.checks.iter().any(|check| {
                normalize_verification_command(&check.command) == normalized
                    && check.binding.as_ref() == Some(binding)
                    && check.terminal_reason_kind
                        == Some(VerificationTerminalReason::RepeatedDeterministicFailure)
            })
        })
    }

    /// Rebind completion-relevant checks to the workspace about to be delivered.
    pub(crate) async fn revalidate_verification_for_delivery(&mut self, baseline: usize) {
        let run_indices = self
            .verification
            .verification_runs
            .iter()
            .enumerate()
            .skip(baseline)
            .filter_map(|(index, run)| {
                matches!(
                    run.status,
                    VerificationRunStatus::Passed
                        | VerificationRunStatus::Unstable
                        | VerificationRunStatus::Failed
                        | VerificationRunStatus::Blocked
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        for run_index in run_indices {
            let checks = self.verification.verification_runs[run_index]
                .checks
                .iter()
                .map(|check| (check.command.clone(), check.binding.clone()))
                .collect::<Vec<_>>();
            let mut delivered_bindings = Vec::with_capacity(checks.len());
            let mut stale = false;
            let mut blocked_reason = None;
            for (command, recorded_binding) in checks {
                let command_cwd = recorded_binding
                    .as_ref()
                    .and_then(binding_command_cwd)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.project_root.clone());
                let current = capture_verification_workspace_binding(
                    &self.project_root,
                    &command_cwd,
                    &command,
                )
                .await;
                stale |= !bindings_are_fresh(recorded_binding.as_ref(), &current);
                if let VerificationBinding::Blocked { reason } = &current {
                    blocked_reason.get_or_insert(*reason);
                }
                delivered_bindings.push(current);
            }
            let run = &mut self.verification.verification_runs[run_index];
            let was_successful = matches!(
                run.status,
                VerificationRunStatus::Passed | VerificationRunStatus::Unstable
            );
            for (check, delivered) in run.checks.iter_mut().zip(delivered_bindings) {
                check.delivered_binding = Some(delivered);
            }
            run.delivered_workspace_binding = run
                .checks
                .last()
                .and_then(|check| check.delivered_binding.clone());
            if let Some(reason) = blocked_reason {
                run.status = VerificationRunStatus::Blocked;
                run.observed_final_workspace = None;
                run.terminal_reason_kind = Some(reason);
            } else if stale && was_successful {
                run.status = VerificationRunStatus::Stale;
                run.observed_final_workspace = Some(false);
            } else {
                run.observed_final_workspace = Some(!stale);
            }
        }
    }

    pub(crate) fn restore_verification_runs(&mut self, mut runs: Vec<VerificationRunRecord>) {
        let now = crate::util::time::now_ms();
        for run in &mut runs {
            if run.status == VerificationRunStatus::Running {
                run.status = VerificationRunStatus::Interrupted;
                run.finished_at_ms = Some(now);
                run.terminal_reason_kind = Some(VerificationTerminalReason::Interrupted);
                for check in &mut run.checks {
                    if check.status == VerificationCheckStatus::Pending {
                        check.completed_at_ms = Some(now);
                        check.terminal_reason_kind = Some(VerificationTerminalReason::Interrupted);
                    }
                }
            }
        }
        self.verification = VerificationState {
            verification_runs: runs,
            ..VerificationState::default()
        };
    }

    pub(super) async fn record_background_verification_results(
        &mut self,
        tasks: &[crate::background::BackgroundTaskSnapshot],
    ) {
        for task in tasks {
            let Some(capture) = self
                .verification
                .background_verification_bindings
                .remove(&task.id)
            else {
                continue;
            };
            let tool_call_id = task
                .tool_call_id
                .clone()
                .unwrap_or_else(|| format!("background:{}", task.id));
            let arguments = serde_json::json!({
                "command": capture.command.clone(),
                "workdir": task.cwd,
            })
            .to_string();
            let tool_call = ToolCall {
                id: tool_call_id.clone(),
                name: "bash".to_string(),
                arguments,
            };
            let result = ToolOutput::Command {
                rendered: task.completion_report(),
                stdout: task.tail.clone(),
                stderr: String::new(),
                exit_code: task.exit_code,
                timed_out: task.timed_out,
                truncation: None,
            };
            let status = match task.status {
                crate::background::BackgroundTaskStatus::Succeeded => {
                    ToolExecutionStatus::Succeeded
                }
                crate::background::BackgroundTaskStatus::Failed
                | crate::background::BackgroundTaskStatus::TimedOut => ToolExecutionStatus::Failed,
                crate::background::BackgroundTaskStatus::Stopped => {
                    ToolExecutionStatus::Interrupted
                }
                crate::background::BackgroundTaskStatus::Running => {
                    self.verification
                        .background_verification_bindings
                        .insert(task.id.clone(), capture);
                    continue;
                }
            };
            self.verification
                .pending_verification_bindings
                .insert(tool_call_id, capture.binding);
            let active_record_index = self
                .verification
                .active_verification
                .as_ref()
                .map(|active| active.record_index);
            let belongs_to_active =
                capture.record_index.is_some() && capture.record_index == active_record_index;
            if active_record_index.is_none() || belongs_to_active {
                self.record_verification_tool_result(&tool_call, &result, status)
                    .await;
                continue;
            }

            // A detached command may finish during a later, unrelated
            // verification workflow. Record it as its own observed check
            // without letting the shared command text consume that workflow's
            // pending check.
            let unrelated_active = self.verification.active_verification.take();
            self.record_verification_tool_result(&tool_call, &result, status)
                .await;
            self.verification.active_verification = unrelated_active;
        }
    }

    pub(super) async fn record_verification_tool_result(
        &mut self,
        tool_call: &ToolCall,
        result: &ToolOutput,
        status: ToolExecutionStatus,
    ) {
        if tool_call.name != "bash" {
            return;
        }
        let Some(command) = bash_command(&tool_call.arguments) else {
            return;
        };
        if self.verification_kind_for_command(&command).is_none() {
            self.verification
                .pending_verification_bindings
                .remove(&tool_call.id);
            self.verification
                .suppressed_verification_calls
                .remove(&tool_call.id);
            return;
        }
        let command_cwd = bash_command_cwd(&self.project_root, &tool_call.arguments);
        let pending_binding = self
            .verification
            .pending_verification_bindings
            .remove(&tool_call.id);
        if status == ToolExecutionStatus::Started {
            let ToolOutput::BackgroundTaskStarted { task_id, .. } = result else {
                return;
            };
            if let Some(task) = self.background_tasks.snapshot(task_id).await {
                let binding = pending_binding
                    .filter(|binding| binding_matches_background_start(binding, &task.cwd))
                    .unwrap_or(VerificationBinding::Blocked {
                        reason: VerificationTerminalReason::EnvironmentBlocked,
                    });
                let record_index = self
                    .verification
                    .active_verification
                    .as_ref()
                    .filter(|active| {
                        self.verification
                            .verification_runs
                            .get(active.record_index)
                            .is_some_and(|record| {
                                let command = normalize_verification_command(&command);
                                record.checks.iter().any(|check| {
                                    normalize_verification_command(&check.command) == command
                                })
                            })
                    })
                    .map(|active| active.record_index);
                if let Some(record_index) = record_index
                    && let Some(check) = self
                        .verification
                        .verification_runs
                        .get_mut(record_index)
                        .and_then(|record| {
                            let command = normalize_verification_command(&command);
                            record.checks.iter_mut().find(|check| {
                                normalize_verification_command(&check.command) == command
                            })
                        })
                {
                    check.tool_call_id = Some(tool_call.id.clone());
                    check.binding = Some(binding.clone());
                }
                self.verification.background_verification_bindings.insert(
                    task_id.clone(),
                    BackgroundVerificationCapture {
                        binding,
                        record_index,
                        command: command.clone(),
                    },
                );
            } else {
                let binding = match pending_binding {
                    Some(binding) => binding,
                    None => {
                        capture_verification_workspace_binding(
                            &self.project_root,
                            &command_cwd,
                            &command,
                        )
                        .await
                    }
                };
                self.record_nonexecuted_verification(
                    &command,
                    Some(&tool_call.id),
                    VerificationTerminalReason::Delegated,
                    binding,
                    "Verification was delegated to an interactive process without a capturable terminal result.",
                );
            }
            return;
        }
        let binding = match pending_binding {
            Some(binding) => binding,
            None => {
                capture_verification_workspace_binding(&self.project_root, &command_cwd, &command)
                    .await
            }
        };
        let deterministic_failure_suppressed = self
            .verification
            .suppressed_verification_calls
            .remove(&tool_call.id);
        if matches!(
            status,
            ToolExecutionStatus::Skipped | ToolExecutionStatus::Interrupted
        ) {
            let reason = if deterministic_failure_suppressed {
                VerificationTerminalReason::RepeatedDeterministicFailure
            } else if status == ToolExecutionStatus::Interrupted {
                VerificationTerminalReason::Interrupted
            } else {
                verification_nonexecution_reason(result, status)
            };
            let message = if deterministic_failure_suppressed {
                format!(
                    "Verification blocked: {command:?} was not executed because the same workspace already produced a confirmed deterministic failure."
                )
            } else {
                format!(
                    "Verification did not execute {command:?}: {}.",
                    reason.label()
                )
            };
            self.record_nonexecuted_verification(
                &command,
                Some(&tool_call.id),
                reason,
                binding,
                &message,
            );
            return;
        }
        if !matches!(result, ToolOutput::Command { .. }) {
            let reason = verification_nonexecution_reason(result, status);
            let message = format!(
                "Verification did not execute {command:?}: {}.",
                reason.label()
            );
            self.record_nonexecuted_verification(
                &command,
                Some(&tool_call.id),
                reason,
                binding,
                &message,
            );
            return;
        }
        let explicit = self
            .verification
            .active_verification
            .as_ref()
            .and_then(|active| self.verification.verification_runs.get(active.record_index))
            .is_some_and(|record| {
                let command = normalize_verification_command(&command);
                record
                    .checks
                    .iter()
                    .any(|check| normalize_verification_command(&check.command) == command)
            });
        let passed = matches!(
            result,
            ToolOutput::Command {
                exit_code: Some(0),
                timed_out: false,
                ..
            }
        ) && status.is_success();
        self.self_review
            .note_check_result(&command, passed, explicit);
        if self.verification.active_verification.is_none() {
            self.record_observed_verification(&command, tool_call, result, status, binding);
            return;
        }
        let Some(active) = self.verification.active_verification.as_ref() else {
            return;
        };
        let record_index = active.record_index;
        let Some(record) = self.verification.verification_runs.get(record_index) else {
            return;
        };
        let normalized = normalize_verification_command(&command);
        let Some(check_index) = record.checks.iter().position(|check| {
            check.status != VerificationCheckStatus::Passed
                && normalize_verification_command(&check.command) == normalized
        }) else {
            return;
        };

        let (check_status, exit_code) = verification_check_outcome(result, status);
        let retained_paths = active
            .last_check_snapshot
            .as_ref()
            .map(WorktreeSnapshot::paths)
            .unwrap_or_default();
        let snapshot =
            capture_worktree_snapshot_including(&self.project_root, &retained_paths).await;
        let workspace_changed = record.checks[check_index]
            .binding
            .as_ref()
            .is_some_and(|previous| previous != &binding)
            || match (active.last_check_snapshot.as_ref(), snapshot.as_ref()) {
                (Some(previous), Some(current)) => !previous.changed_paths(current).is_empty(),
                _ => false,
            };
        let binding_blocker = binding_terminal_reason(&binding);
        let failure_signature = (check_status != VerificationCheckStatus::Passed)
            .then(|| verification_failure_signature(&command, result, check_status));

        let base_reasoning = self.provider.reasoning();
        let available_reasoning_escalation = self.provider.reasoning_escalation();
        let Some(mut active) = self.verification.active_verification.take() else {
            return;
        };
        active.last_check_snapshot = snapshot;
        let mut recovery_event = None;
        if let Some(record) = self.verification.verification_runs.get_mut(record_index)
            && let Some(check) = record.checks.get_mut(check_index)
        {
            check.status = check_status;
            check.tool_call_id = Some(tool_call.id.clone());
            check.exit_code = exit_code;
            let now = crate::util::time::now_ms();
            check.completed_at_ms = Some(now);
            check.attempt_count = check.attempt_count.saturating_add(1);
            check.attempt_timestamps_ms.push(now);
            check.binding = Some(binding);

            if let Some(reason) = binding_blocker {
                let message = format!(
                    "Verification evidence for {command:?} could not be bound to the workspace: {}.",
                    reason.label()
                );
                check.terminal_reason_kind = Some(reason);
                record.terminal_reason = Some(message.clone());
                record.terminal_reason_kind = Some(reason);
                active.pending_blocker = Some(message);
            } else if check_status == VerificationCheckStatus::Passed {
                if active.last_failure_signature.take().is_some() && !workspace_changed {
                    active.unstable_observed = true;
                    recovery_event = Some(VerificationRecoveryEvent::UnstablePass {
                        command: command.clone(),
                    });
                }
                active.flaky_rerun_used = false;
            } else if let Some(signature) = failure_signature {
                check.last_failure_signature = Some(signature.clone());
                check.failure_signatures.push(signature.clone());
                let previous_signature = active.last_failure_signature.replace(signature.clone());
                match previous_signature {
                    None if record.repair_attempts >= MAX_VERIFICATION_REPAIR_ATTEMPTS => {
                        let reason = format!(
                            "Verification blocked: {command:?} failed after the run exhausted its {MAX_VERIFICATION_REPAIR_ATTEMPTS} focused repair attempts."
                        );
                        record.terminal_reason = Some(reason.clone());
                        record.terminal_reason_kind =
                            Some(VerificationTerminalReason::RepairBudgetExhausted);
                        check.terminal_reason_kind =
                            Some(VerificationTerminalReason::RepairBudgetExhausted);
                        active.pending_blocker = Some(reason);
                    }
                    None => {
                        record.repair_attempts = record.repair_attempts.saturating_add(1);
                        recovery_event = Some(VerificationRecoveryEvent::Repair {
                            command: command.clone(),
                            signature,
                            attempt: record.repair_attempts,
                            reasoning_escalation: None,
                        });
                    }
                    Some(previous) if !workspace_changed && previous == signature => {
                        let reason = format!(
                            "Verification blocked: {command:?} repeated the same deterministic failure ({signature}) without a workspace change."
                        );
                        record.terminal_reason = Some(reason.clone());
                        record.terminal_reason_kind =
                            Some(VerificationTerminalReason::RepeatedDeterministicFailure);
                        check.terminal_reason_kind =
                            Some(VerificationTerminalReason::RepeatedDeterministicFailure);
                        active.pending_blocker = Some(reason);
                    }
                    Some(_) if !workspace_changed && !active.flaky_rerun_used => {
                        active.flaky_rerun_used = true;
                        recovery_event = Some(VerificationRecoveryEvent::FlakyRerun {
                            command: command.clone(),
                            signature,
                        });
                    }
                    Some(_) if !workspace_changed => {
                        let reason = format!(
                            "Verification blocked: {command:?} remained unstable after its one bounded no-change rerun."
                        );
                        record.terminal_reason = Some(reason.clone());
                        record.terminal_reason_kind =
                            Some(VerificationTerminalReason::UnstableFailure);
                        check.terminal_reason_kind =
                            Some(VerificationTerminalReason::UnstableFailure);
                        active.pending_blocker = Some(reason);
                    }
                    Some(_) if record.repair_attempts >= MAX_VERIFICATION_REPAIR_ATTEMPTS => {
                        let reason = format!(
                            "Verification blocked: {command:?} still failed after {MAX_VERIFICATION_REPAIR_ATTEMPTS} focused repair attempts."
                        );
                        record.terminal_reason = Some(reason.clone());
                        record.terminal_reason_kind =
                            Some(VerificationTerminalReason::RepairBudgetExhausted);
                        check.terminal_reason_kind =
                            Some(VerificationTerminalReason::RepairBudgetExhausted);
                        active.pending_blocker = Some(reason);
                    }
                    Some(_) => {
                        record.repair_attempts = record.repair_attempts.saturating_add(1);
                        active.flaky_rerun_used = false;
                        let reasoning_escalation = maybe_escalate_verification_reasoning(
                            &mut active,
                            record,
                            base_reasoning,
                            available_reasoning_escalation,
                            &signature,
                        );
                        recovery_event = Some(VerificationRecoveryEvent::Repair {
                            command: command.clone(),
                            signature,
                            attempt: record.repair_attempts,
                            reasoning_escalation,
                        });
                    }
                }
            }
        }
        self.verification.active_verification = Some(active);
        if let Some(event) = recovery_event {
            self.push_harness_note(&event.harness_note());
        }
    }

    fn record_observed_verification(
        &mut self,
        command: &str,
        tool_call: &ToolCall,
        result: &ToolOutput,
        status: ToolExecutionStatus,
        binding: VerificationBinding,
    ) {
        let Some(kind) = self.verification_kind_for_command(command) else {
            return;
        };
        let normalized = normalize_verification_command(command);
        let profile = VerificationProfile::resolve(&self.project_root, &self.config.verification);
        let check_name = profile
            .checks(kind)
            .iter()
            .find(|check| normalize_verification_command(&check.command) == normalized)
            .map(|check| check.name.clone())
            .unwrap_or_else(|| format!("Observed {} command", kind.label()));

        let (check_status, exit_code) = verification_check_outcome(result, status);
        let binding_blocker = binding_terminal_reason(&binding);
        let now = crate::util::time::now_ms();
        let failure_signature = (check_status != VerificationCheckStatus::Passed)
            .then(|| verification_failure_signature(command, result, check_status));
        let failure_signatures = failure_signature.iter().cloned().collect();
        let existing_index = self
            .verification
            .observed_verification_run_indices
            .get(&normalized)
            .copied()
            .filter(|index| {
                self.verification
                    .verification_runs
                    .get(*index)
                    .is_some_and(|run| {
                        run.checks.len() == 1
                            && normalize_verification_command(&run.checks[0].command) == normalized
                            && run.checks[0].binding.as_ref() == Some(&binding)
                    })
            });
        if let Some(existing) =
            existing_index.and_then(|index| self.verification.verification_runs.get_mut(index))
        {
            let check = &mut existing.checks[0];
            let previous_status = check.status;
            let previous_signature = check.last_failure_signature.clone();
            check.status = check_status;
            check.tool_call_id = Some(tool_call.id.clone());
            check.exit_code = exit_code;
            check.completed_at_ms = Some(now);
            check.attempt_count = check.attempt_count.saturating_add(1);
            check.attempt_timestamps_ms.push(now);
            check.delivered_binding = None;
            if let Some(signature) = failure_signature.as_ref() {
                check.last_failure_signature = Some(signature.clone());
                check.failure_signatures.push(signature.clone());
            }
            let repeated_failure = matches!(
                previous_status,
                VerificationCheckStatus::Failed | VerificationCheckStatus::TimedOut
            ) && failure_signature.is_some()
                && previous_signature.as_ref() == failure_signature.as_ref();
            existing.status = if let Some(reason) = binding_blocker {
                check.terminal_reason_kind = Some(reason);
                existing.terminal_reason_kind = Some(reason);
                VerificationRunStatus::Blocked
            } else if repeated_failure {
                check.terminal_reason_kind =
                    Some(VerificationTerminalReason::RepeatedDeterministicFailure);
                existing.terminal_reason_kind =
                    Some(VerificationTerminalReason::RepeatedDeterministicFailure);
                existing.terminal_reason = Some(format!(
                    "Verification blocked: {command:?} repeated the same deterministic failure without an input change."
                ));
                VerificationRunStatus::Blocked
            } else if check_status == VerificationCheckStatus::Passed
                && matches!(
                    previous_status,
                    VerificationCheckStatus::Failed | VerificationCheckStatus::TimedOut
                )
            {
                existing.terminal_reason = Some(
                    "The check passed only after a no-change rerun and is unstable.".to_string(),
                );
                VerificationRunStatus::Unstable
            } else if check_status == VerificationCheckStatus::Passed {
                VerificationRunStatus::Passed
            } else {
                VerificationRunStatus::Failed
            };
            existing.finished_at_ms = Some(now);
            existing.observed_final_workspace = binding_blocker.is_none().then_some(true);
            existing.delivered_workspace_binding = None;
            return;
        }
        let terminal_reason_kind = binding_blocker;
        let run_status = if terminal_reason_kind.is_some() {
            VerificationRunStatus::Blocked
        } else if check_status == VerificationCheckStatus::Passed {
            VerificationRunStatus::Passed
        } else {
            VerificationRunStatus::Failed
        };
        let record_index = self.verification.verification_runs.len();
        self.verification
            .verification_runs
            .push(VerificationRunRecord {
                kind,
                status: run_status,
                checks: vec![VerificationCheckRecord {
                    name: check_name,
                    command: command.to_string(),
                    status: check_status,
                    tool_call_id: Some(tool_call.id.clone()),
                    exit_code,
                    completed_at_ms: Some(now),
                    attempt_count: 1,
                    last_failure_signature: failure_signature,
                    binding: Some(binding),
                    delivered_binding: None,
                    attempt_timestamps_ms: vec![now],
                    failure_signatures,
                    terminal_reason_kind,
                }],
                started_at_ms: now,
                finished_at_ms: Some(now),
                observed_final_workspace: terminal_reason_kind.is_none().then_some(true),
                workspace_changes_after_last_check: Vec::new(),
                repair_attempts: 0,
                reasoning_escalations: Vec::new(),
                terminal_reason: None,
                terminal_reason_kind,
                delivered_workspace_binding: None,
            });
        self.verification
            .observed_verification_run_indices
            .insert(normalized, record_index);
    }

    fn record_skipped_profile(
        &mut self,
        kind: VerificationKind,
        checks: &[VerificationCheck],
        reason: VerificationTerminalReason,
    ) {
        let now = crate::util::time::now_ms();
        let mut record = VerificationRunRecord::running(kind, checks);
        record.status = verification_status_for_terminal_reason(reason);
        record.started_at_ms = now;
        record.finished_at_ms = Some(now);
        record.terminal_reason_kind = Some(reason);
        for check in &mut record.checks {
            check.terminal_reason_kind = Some(reason);
        }
        self.verification.verification_runs.push(record);
    }

    fn record_nonexecuted_verification(
        &mut self,
        command: &str,
        tool_call_id: Option<&str>,
        reason: VerificationTerminalReason,
        binding: VerificationBinding,
        message: &str,
    ) {
        let now = crate::util::time::now_ms();
        let normalized = normalize_verification_command(command);
        let active_index = self
            .verification
            .active_verification
            .as_ref()
            .map(|active| active.record_index)
            .filter(|index| {
                self.verification
                    .verification_runs
                    .get(*index)
                    .is_some_and(|record| {
                        record.checks.iter().any(|check| {
                            normalize_verification_command(&check.command) == normalized
                        })
                    })
            });
        if let Some(index) = active_index {
            if let Some(record) = self.verification.verification_runs.get_mut(index) {
                record.status = verification_status_for_terminal_reason(reason);
                record.finished_at_ms = Some(now);
                record.observed_final_workspace = None;
                record.terminal_reason = Some(message.to_string());
                record.terminal_reason_kind = Some(reason);
                if let Some(check) = record
                    .checks
                    .iter_mut()
                    .find(|check| normalize_verification_command(&check.command) == normalized)
                {
                    check.tool_call_id = tool_call_id.map(str::to_string);
                    check.completed_at_ms = Some(now);
                    check.binding = Some(binding);
                    check.terminal_reason_kind = Some(reason);
                }
            }
            if let Some(active) = self.verification.active_verification.as_mut() {
                active.pending_blocker = Some(message.to_string());
            }
            return;
        }

        let kind = self
            .verification_kind_for_command(command)
            .unwrap_or(VerificationKind::Test);
        self.verification
            .verification_runs
            .push(VerificationRunRecord {
                kind,
                status: verification_status_for_terminal_reason(reason),
                checks: vec![VerificationCheckRecord {
                    name: format!("Non-executed {} command", kind.label()),
                    command: command.to_string(),
                    status: VerificationCheckStatus::Pending,
                    tool_call_id: tool_call_id.map(str::to_string),
                    exit_code: None,
                    completed_at_ms: Some(now),
                    attempt_count: 0,
                    last_failure_signature: None,
                    binding: Some(binding),
                    delivered_binding: None,
                    attempt_timestamps_ms: Vec::new(),
                    failure_signatures: Vec::new(),
                    terminal_reason_kind: Some(reason),
                }],
                started_at_ms: now,
                finished_at_ms: Some(now),
                observed_final_workspace: None,
                workspace_changes_after_last_check: Vec::new(),
                repair_attempts: 0,
                reasoning_escalations: Vec::new(),
                terminal_reason: Some(message.to_string()),
                terminal_reason_kind: Some(reason),
                delivered_workspace_binding: None,
            });
    }

    pub(super) fn verification_blocker(&self) -> Option<&str> {
        self.verification
            .active_verification
            .as_ref()
            .and_then(|active| active.pending_blocker.as_deref())
    }

    pub(super) async fn finish_verification_run(&mut self, result: &Result<AgentRunResult>) {
        if matches!(result, Ok(AgentRunResult::Waiting(_))) {
            return;
        }
        let Some(active) = self.verification.active_verification.take() else {
            return;
        };

        let checks = self
            .verification
            .verification_runs
            .get(active.record_index)
            .map(|record| {
                record
                    .checks
                    .iter()
                    .map(|check| (check.command.clone(), check.binding.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut delivered_bindings = Vec::with_capacity(checks.len());
        let mut bindings_fresh = !checks.is_empty();
        let mut delivery_blocker = None;
        for (command, recorded_binding) in checks {
            let command_cwd = recorded_binding
                .as_ref()
                .and_then(binding_command_cwd)
                .map(PathBuf::from)
                .unwrap_or_else(|| self.project_root.clone());
            let delivered =
                capture_verification_workspace_binding(&self.project_root, &command_cwd, &command)
                    .await;
            bindings_fresh &= bindings_are_fresh(recorded_binding.as_ref(), &delivered);
            if let VerificationBinding::Blocked { reason } = &delivered {
                delivery_blocker.get_or_insert(*reason);
            }
            delivered_bindings.push(delivered);
        }

        let final_snapshot = if let Some(last) = active.last_check_snapshot.as_ref() {
            capture_worktree_snapshot_including(&self.project_root, &last.paths()).await
        } else {
            None
        };
        let (snapshot_observed_final_workspace, changes) =
            match (active.last_check_snapshot.as_ref(), final_snapshot.as_ref()) {
                (Some(last), Some(final_snapshot)) => {
                    let changes = last.changed_paths(final_snapshot);
                    (Some(changes.is_empty()), changes)
                }
                _ => (None, Vec::new()),
            };
        let observed_final_workspace = if delivery_blocker.is_some() {
            None
        } else if delivered_bindings.is_empty() {
            snapshot_observed_final_workspace
        } else {
            Some(bindings_fresh && snapshot_observed_final_workspace.unwrap_or(true))
        };

        let Some(record) = self
            .verification
            .verification_runs
            .get_mut(active.record_index)
        else {
            return;
        };
        record.finished_at_ms = Some(crate::util::time::now_ms());
        record.observed_final_workspace = observed_final_workspace;
        record.workspace_changes_after_last_check = changes;
        for (check, delivered) in record.checks.iter_mut().zip(delivered_bindings) {
            check.delivered_binding = Some(delivered);
        }
        record.delivered_workspace_binding = record
            .checks
            .last()
            .and_then(|check| check.delivered_binding.clone());
        if let Some(reason) = delivery_blocker {
            record.terminal_reason_kind = Some(reason);
        }
        let any_failed = record.checks.iter().any(|check| {
            matches!(
                check.status,
                VerificationCheckStatus::Failed | VerificationCheckStatus::TimedOut
            )
        });
        let all_passed = !record.checks.is_empty()
            && record
                .checks
                .iter()
                .all(|check| check.status == VerificationCheckStatus::Passed);
        record.status = if delivery_blocker.is_some() {
            VerificationRunStatus::Blocked
        } else if active.pending_blocker.is_some() {
            record
                .terminal_reason_kind
                .map(verification_status_for_terminal_reason)
                .unwrap_or(VerificationRunStatus::Blocked)
        } else if any_failed {
            VerificationRunStatus::Failed
        } else if matches!(result, Ok(AgentRunResult::Interrupted(_))) {
            record.terminal_reason_kind = Some(VerificationTerminalReason::Interrupted);
            VerificationRunStatus::Interrupted
        } else if all_passed && active.unstable_observed {
            if record.terminal_reason.is_none() {
                record.terminal_reason =
                    Some("At least one check passed only after a no-change rerun.".to_string());
            }
            VerificationRunStatus::Unstable
        } else if all_passed && observed_final_workspace == Some(true) {
            VerificationRunStatus::Passed
        } else if all_passed && observed_final_workspace == Some(false) {
            VerificationRunStatus::Stale
        } else {
            VerificationRunStatus::Incomplete
        };
        match record.status {
            VerificationRunStatus::Passed | VerificationRunStatus::Unstable => {
                self.verification.after_edit_verification_pending = false;
            }
            VerificationRunStatus::Stale => {
                self.verification.after_edit_verification_pending = true;
                self.verification.after_edit_verification_injected = false;
            }
            _ => {}
        }
    }
}

fn verification_status_for_terminal_reason(
    reason: VerificationTerminalReason,
) -> VerificationRunStatus {
    match reason {
        VerificationTerminalReason::EnvironmentBlocked
        | VerificationTerminalReason::RepeatedDeterministicFailure
        | VerificationTerminalReason::RepairBudgetExhausted
        | VerificationTerminalReason::UnstableFailure => VerificationRunStatus::Blocked,
        VerificationTerminalReason::Cancelled | VerificationTerminalReason::Interrupted => {
            VerificationRunStatus::Interrupted
        }
        VerificationTerminalReason::Irrelevant
        | VerificationTerminalReason::PolicyDisabled
        | VerificationTerminalReason::UserSkipped
        | VerificationTerminalReason::Delegated => VerificationRunStatus::Incomplete,
    }
}

fn binding_terminal_reason(binding: &VerificationBinding) -> Option<VerificationTerminalReason> {
    match binding {
        VerificationBinding::Bound { .. } => None,
        VerificationBinding::Blocked { reason } => Some(*reason),
    }
}

fn verification_nonexecution_reason(
    result: &ToolOutput,
    status: ToolExecutionStatus,
) -> VerificationTerminalReason {
    if status == ToolExecutionStatus::Interrupted {
        return VerificationTerminalReason::Interrupted;
    }
    let message = result.rendered_summary().to_ascii_lowercase();
    if message.contains("prompt cancelled") || message.contains("request cancelled") {
        VerificationTerminalReason::Cancelled
    } else if message.contains("permission denied by user")
        || message.contains("denied for command")
    {
        VerificationTerminalReason::UserSkipped
    } else if status == ToolExecutionStatus::Skipped {
        VerificationTerminalReason::Interrupted
    } else {
        VerificationTerminalReason::EnvironmentBlocked
    }
}

fn observed_verification_kind(command: &str) -> Option<VerificationKind> {
    let mut saw_build = false;
    let analysis = crate::tool::analyze_bash_command(command);
    for segment in analysis.permission_commands() {
        let words = segment
            .split_whitespace()
            .map(|word| word.trim_matches(['(', ')', '\'', '"']))
            .filter(|word| !word.is_empty())
            .collect::<Vec<_>>();
        let mut index = usize::from(words.first() == Some(&"env"));
        while words
            .get(index)
            .is_some_and(|word| word.contains('=') && !word.starts_with('-'))
        {
            index += 1;
        }
        let Some(program) = words
            .get(index)
            .and_then(|program| program.rsplit('/').next())
        else {
            continue;
        };
        let args = &words[index + 1..];
        let kind = match (program, args) {
            ("cargo", ["test" | "nextest", ..])
            | ("go", ["test", ..])
            | ("pytest" | "py.test", _)
            | ("python" | "python3", ["-m", "pytest" | "unittest" | "doctest", ..])
            | ("npm", ["test", ..])
            | ("npm", ["run", "test", ..])
            | ("pnpm" | "yarn" | "bun", ["test", ..])
            | ("deno", ["test", ..]) => VerificationKind::Test,
            ("cargo", ["build" | "check" | "clippy", ..])
            | ("go", ["build" | "vet", ..])
            | ("npm" | "pnpm" | "yarn" | "bun", ["run", "build" | "lint", ..])
            | ("deno", ["check" | "lint", ..])
            | ("python" | "python3", ["-m", "build" | "compileall", ..]) => VerificationKind::Build,
            _ => continue,
        };
        if kind == VerificationKind::Test {
            return Some(kind);
        }
        saw_build = true;
    }
    saw_build.then_some(VerificationKind::Build)
}

fn verification_check_outcome(
    result: &ToolOutput,
    status: ToolExecutionStatus,
) -> (VerificationCheckStatus, Option<i32>) {
    match result {
        ToolOutput::Command {
            exit_code,
            timed_out,
            ..
        } if *timed_out => (VerificationCheckStatus::TimedOut, *exit_code),
        ToolOutput::Command { exit_code, .. } if *exit_code == Some(0) && status.is_success() => {
            (VerificationCheckStatus::Passed, *exit_code)
        }
        ToolOutput::Command { exit_code, .. } => (VerificationCheckStatus::Failed, *exit_code),
        _ if status.is_success() => (VerificationCheckStatus::Passed, None),
        _ => (VerificationCheckStatus::Failed, None),
    }
}

fn maybe_escalate_verification_reasoning(
    active: &mut ActiveVerificationRun,
    record: &mut VerificationRunRecord,
    from: ReasoningSelection,
    to: Option<ReasoningSelection>,
    failure_signature: &str,
) -> Option<VerificationReasoningEscalation> {
    if active.reasoning_override.is_some() {
        return None;
    }
    let to = to?;
    let escalation = VerificationReasoningEscalation {
        from,
        to,
        repair_attempt: record.repair_attempts,
        failure_signature: failure_signature.to_string(),
        occurred_at_ms: crate::util::time::now_ms(),
    };
    active.reasoning_override = Some(to);
    record.reasoning_escalations.push(escalation.clone());
    Some(escalation)
}

fn verification_failure_signature(
    command: &str,
    result: &ToolOutput,
    status: VerificationCheckStatus,
) -> String {
    let diagnostic = match result {
        ToolOutput::Command { rendered, .. } => rendered.as_str(),
        ToolOutput::Text(text) => text.as_str(),
        _ => result.rendered_summary(),
    };
    let normalized = normalize_failure_diagnostic(diagnostic);
    let input = format!("{command}\0{}\0{normalized}", status.label());
    blake3::hash(input.as_bytes()).to_hex()[..16].to_string()
}

fn normalize_failure_diagnostic(diagnostic: &str) -> String {
    let mut normalized = String::with_capacity(diagnostic.len().min(8_000));
    let mut in_digits = false;
    let mut in_whitespace = false;
    for ch in diagnostic.chars().take(8_000) {
        if ch.is_ascii_digit() {
            if !in_digits {
                normalized.push('#');
            }
            in_digits = true;
            in_whitespace = false;
        } else if ch.is_whitespace() {
            if !in_whitespace && !normalized.is_empty() {
                normalized.push(' ');
            }
            in_digits = false;
            in_whitespace = true;
        } else {
            normalized.push(ch.to_ascii_lowercase());
            in_digits = false;
            in_whitespace = false;
        }
    }
    normalized.trim().to_string()
}

fn bash_command(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    value
        .get("command")?
        .as_str()
        .map(str::trim)
        .map(str::to_string)
}

fn bash_command_cwd(project_root: &Path, arguments: &str) -> PathBuf {
    let workdir = serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|value| value.get("workdir")?.as_str().map(str::to_owned));
    workdir.map_or_else(
        || project_root.to_path_buf(),
        |workdir| {
            let path = PathBuf::from(workdir);
            if path.is_absolute() {
                path
            } else {
                project_root.join(path)
            }
        },
    )
}

fn binding_command_cwd(binding: &VerificationBinding) -> Option<&str> {
    match binding {
        VerificationBinding::Bound { identity, .. } => Some(&identity.command_cwd),
        VerificationBinding::Blocked { .. } => None,
    }
}

fn binding_matches_background_start(binding: &VerificationBinding, started_cwd: &Path) -> bool {
    match binding {
        VerificationBinding::Bound { identity, .. } => {
            Path::new(&identity.command_cwd) == started_cwd
        }
        VerificationBinding::Blocked { .. } => true,
    }
}

fn bindings_are_fresh(
    recorded: Option<&VerificationBinding>,
    delivered: &VerificationBinding,
) -> bool {
    matches!(
        (recorded, delivered),
        (
            Some(VerificationBinding::Bound { digest: recorded, .. }),
            VerificationBinding::Bound { digest: delivered, .. }
        ) if recorded == delivered
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_command_requires_the_structured_command_field() {
        assert_eq!(
            bash_command(r#"{"command":" cargo test "}"#),
            Some("cargo test".to_string())
        );
        assert_eq!(bash_command(r#"{"cmd":"cargo test"}"#), None);
        assert_eq!(bash_command("not-json"), None);
    }

    #[test]
    fn compound_commands_are_classified_as_observed_verification() {
        assert_eq!(
            observed_verification_kind(
                "cargo fmt --all -- --check && cargo test --locked settings_ -- --nocapture"
            ),
            Some(VerificationKind::Test)
        );
        assert_eq!(
            observed_verification_kind("RUSTFLAGS=-Dwarnings cargo clippy --all-targets"),
            Some(VerificationKind::Build)
        );
        assert_eq!(observed_verification_kind("echo 'cargo test'"), None);
        assert_eq!(
            observed_verification_kind("echo 'skip && cargo test'"),
            None
        );
    }

    #[test]
    fn failure_signature_ignores_volatile_numbers_but_not_diagnostics() {
        let first = ToolOutput::Text("failed at line 41 after 1.2s: expected alpha".to_string());
        let second = ToolOutput::Text("failed at line 92 after 8.7s: expected alpha".to_string());
        let changed = ToolOutput::Text("failed at line 92 after 8.7s: expected beta".to_string());

        assert_eq!(
            verification_failure_signature("cargo test", &first, VerificationCheckStatus::Failed,),
            verification_failure_signature("cargo test", &second, VerificationCheckStatus::Failed,)
        );
        assert_ne!(
            verification_failure_signature("cargo test", &second, VerificationCheckStatus::Failed,),
            verification_failure_signature("cargo test", &changed, VerificationCheckStatus::Failed,)
        );
    }
}
