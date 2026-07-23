use super::*;
use crate::output::ToolExecutionStatus;
use crate::verification::{
    VerificationCheck, VerificationCheckRecord, VerificationCheckStatus, VerificationKind,
    VerificationProfile, VerificationReasoningEscalation, VerificationRunRecord,
    VerificationRunStatus, VerificationWorkflow, VerifyAfterEdit,
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
    /// Reset to a focused coding context and arm typed verification evidence.
    pub(crate) async fn begin_verification_run(
        &mut self,
        kind: VerificationKind,
        checks: &[VerificationCheck],
        prompt: &str,
    ) {
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
        if self.verification.active_verification.is_some() {
            return;
        }
        let Some(record) = self.verification.verification_runs.last_mut() else {
            return;
        };
        if !matches!(
            record.status,
            VerificationRunStatus::Passed | VerificationRunStatus::Unstable
        ) {
            return;
        }
        record.status = VerificationRunStatus::Stale;
        record.observed_final_workspace = Some(false);
        for path in paths {
            if !record.workspace_changes_after_last_check.contains(path) {
                record.workspace_changes_after_last_check.push(path.clone());
            }
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
        if policy == VerifyAfterEdit::Off {
            return false;
        }

        let profile = VerificationProfile::resolve(&self.project_root, &self.config.verification);
        let kind = if !profile.tests.is_empty() {
            VerificationKind::Test
        } else if !profile.builds.is_empty() {
            VerificationKind::Build
        } else {
            sink.status("Post-edit verification skipped: no checks are configured or detected.");
            return false;
        };
        let workflow = VerificationWorkflow {
            kind,
            checks: profile.checks(kind).to_vec(),
            prompt: match profile.workflow_prompt(kind) {
                Ok(prompt) => prompt,
                Err(message) => {
                    sink.status(&format!("Post-edit verification skipped: {message}"));
                    return false;
                }
            },
        };

        if policy == VerifyAfterEdit::Ask
            && !self
                .confirm_after_edit_verification(kind, cancellation_token)
                .await
        {
            return false;
        }

        sink.status(&format!(
            "Post-edit verification: running the configured {} profile…",
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
    ) -> bool {
        let Some(interaction) = self.interaction.as_ref() else {
            return false;
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
        matches!(
            outcome,
            Ok(InteractionOutcome::Question(Some(InteractionAnswer::Choices(choices))))
                if choices.first() == Some(&0)
        )
    }

    pub(crate) fn verification_runs(&self) -> &[VerificationRunRecord] {
        &self.verification.verification_runs
    }

    pub(crate) fn restore_verification_runs(&mut self, mut runs: Vec<VerificationRunRecord>) {
        let now = crate::util::time::now_ms();
        for run in &mut runs {
            if run.status == VerificationRunStatus::Running {
                run.status = VerificationRunStatus::Interrupted;
                run.finished_at_ms = Some(now);
            }
        }
        self.verification.verification_runs = runs;
        self.verification.active_verification = None;
    }

    pub(super) async fn record_verification_tool_result(
        &mut self,
        tool_call: &ToolCall,
        result: &ToolOutput,
        status: ToolExecutionStatus,
    ) {
        if tool_call.name != "bash"
            || matches!(
                status,
                ToolExecutionStatus::Skipped | ToolExecutionStatus::Interrupted
            )
        {
            return;
        }
        let Some(command) = bash_command(&tool_call.arguments) else {
            return;
        };
        let explicit = self
            .verification
            .active_verification
            .as_ref()
            .and_then(|active| self.verification.verification_runs.get(active.record_index))
            .is_some_and(|record| record.checks.iter().any(|check| check.command == command));
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
            self.record_observed_verification(&command, tool_call, result, status);
            return;
        }
        let Some(active) = self.verification.active_verification.as_ref() else {
            return;
        };
        let record_index = active.record_index;
        let Some(record) = self.verification.verification_runs.get(record_index) else {
            return;
        };
        let Some(check_index) = record.checks.iter().position(|check| {
            check.status != VerificationCheckStatus::Passed && check.command == command
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
        let workspace_changed = match (active.last_check_snapshot.as_ref(), snapshot.as_ref()) {
            (Some(previous), Some(current)) => !previous.changed_paths(current).is_empty(),
            _ => false,
        };
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
            check.completed_at_ms = Some(crate::util::time::now_ms());
            check.attempt_count = check.attempt_count.saturating_add(1);

            if check_status == VerificationCheckStatus::Passed {
                if active.last_failure_signature.take().is_some() && !workspace_changed {
                    active.unstable_observed = true;
                    recovery_event = Some(VerificationRecoveryEvent::UnstablePass {
                        command: command.clone(),
                    });
                }
                active.flaky_rerun_used = false;
            } else if let Some(signature) = failure_signature {
                check.last_failure_signature = Some(signature.clone());
                let previous_signature = active.last_failure_signature.replace(signature.clone());
                match previous_signature {
                    None if record.repair_attempts >= MAX_VERIFICATION_REPAIR_ATTEMPTS => {
                        let reason = format!(
                            "Verification blocked: {command:?} failed after the run exhausted its {MAX_VERIFICATION_REPAIR_ATTEMPTS} focused repair attempts."
                        );
                        record.terminal_reason = Some(reason.clone());
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
                        active.pending_blocker = Some(reason);
                    }
                    Some(_) if record.repair_attempts >= MAX_VERIFICATION_REPAIR_ATTEMPTS => {
                        let reason = format!(
                            "Verification blocked: {command:?} still failed after {MAX_VERIFICATION_REPAIR_ATTEMPTS} focused repair attempts."
                        );
                        record.terminal_reason = Some(reason.clone());
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
    ) {
        let configured_kind = self
            .config
            .verification
            .test
            .as_deref()
            .is_some_and(|checks| checks.iter().any(|check| check.trim() == command))
            .then_some(VerificationKind::Test)
            .or_else(|| {
                self.config
                    .verification
                    .build
                    .as_deref()
                    .is_some_and(|checks| checks.iter().any(|check| check.trim() == command))
                    .then_some(VerificationKind::Build)
            });
        let Some(kind) = configured_kind.or_else(|| observed_verification_kind(command)) else {
            return;
        };
        let profile = VerificationProfile::resolve(&self.project_root, &self.config.verification);
        let check_name = profile
            .checks(kind)
            .iter()
            .find(|check| check.command == command)
            .map(|check| check.name.clone())
            .unwrap_or_else(|| format!("Observed {} command", kind.label()));

        let (check_status, exit_code) = verification_check_outcome(result, status);
        let now = crate::util::time::now_ms();
        let failure_signature = (check_status != VerificationCheckStatus::Passed)
            .then(|| verification_failure_signature(command, result, check_status));
        self.verification
            .verification_runs
            .push(VerificationRunRecord {
                kind,
                status: if check_status == VerificationCheckStatus::Passed {
                    VerificationRunStatus::Passed
                } else {
                    VerificationRunStatus::Failed
                },
                checks: vec![VerificationCheckRecord {
                    name: check_name,
                    command: command.to_string(),
                    status: check_status,
                    tool_call_id: Some(tool_call.id.clone()),
                    exit_code,
                    completed_at_ms: Some(now),
                    attempt_count: 1,
                    last_failure_signature: failure_signature,
                }],
                started_at_ms: now,
                finished_at_ms: Some(now),
                observed_final_workspace: Some(true),
                workspace_changes_after_last_check: Vec::new(),
                repair_attempts: 0,
                reasoning_escalations: Vec::new(),
                terminal_reason: None,
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

        let final_snapshot = if let Some(last) = active.last_check_snapshot.as_ref() {
            capture_worktree_snapshot_including(&self.project_root, &last.paths()).await
        } else {
            None
        };
        let (observed_final_workspace, changes) =
            match (active.last_check_snapshot.as_ref(), final_snapshot.as_ref()) {
                (Some(last), Some(final_snapshot)) => {
                    let changes = last.changed_paths(final_snapshot);
                    (Some(changes.is_empty()), changes)
                }
                _ => (None, Vec::new()),
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
        record.status = if active.pending_blocker.is_some() {
            VerificationRunStatus::Blocked
        } else if any_failed {
            VerificationRunStatus::Failed
        } else if matches!(result, Ok(AgentRunResult::Interrupted(_))) {
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
