//! Advisory recovery inside existing hard limits; never authorizes tools or marks success.
use super::ToolCallObservation;
use crate::output::ToolExecutionStatus;
use std::collections::HashSet;

const MAX_PENDING_FAILURES: usize = 16;

// The shared repetition signature is `tool:normalized-arguments`. Recovery cares
// about the check, not its timeout, title, output limits or other call metadata.
fn recovery_identity(signature: &str) -> Option<String> {
    let arguments = signature.strip_prefix("bash:")?;
    let arguments: serde_json::Value = serde_json::from_str(arguments).ok()?;
    let command = arguments.get("command")?.as_str()?;
    let workdir = arguments
        .get("workdir")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.is_empty())
        .unwrap_or(".");
    Some(serde_json::json!([command, workdir]).to_string())
}

#[derive(Debug, Default)]
pub(super) struct RecoveryAdvisory {
    pending_failures: HashSet<String>,
    unresolved_turns: usize,
    recovery_sent: bool,
    budget_warning_sent: bool,
    final_warning_sent: bool,
    delivery_checkpoints_sent: u8,
}

impl RecoveryAdvisory {
    /// Reserve time for delivery without extending limits or authorizing effects.
    /// The coordinator calls this only for the parent coding lane.
    pub(super) fn delivery_note(
        &mut self,
        turn: usize,
        limit: usize,
        workspace_mutated: bool,
    ) -> Option<String> {
        // Short runs already receive the final budget warnings. Long runs need
        // feedback while there is still time to implement and verify a slice.
        if limit < 32 {
            return None;
        }
        let stage = if turn > limit - limit / 4 {
            3
        } else if turn > limit / 2 {
            2
        } else if turn > (limit / 4).min(20) {
            1
        } else {
            return None;
        };
        if stage <= self.delivery_checkpoints_sent {
            return None;
        }
        self.delivery_checkpoints_sent = stage;
        let remaining = limit.saturating_sub(turn).saturating_add(1);
        let progress = if workspace_mutated {
            "Workspace changes were observed, but that does not prove the requested deliverable is complete. Compare the current artifact with the required outcomes and verify the highest-risk missing behavior next."
        } else {
            "No workspace changes have been observed. If the user requested an implementation, turn the evidence already gathered into the smallest runnable end-to-end slice now; expand it with focused checks instead of fully reverse-engineering every edge case first. If an edit is not yet safe, name the exact blocking uncertainty and run only the experiment that decides it."
        };
        Some(format!(
            "Delivery checkpoint {stage}/3: {remaining} model turns remain, including this one. {progress} Novel probe output is information, not a finished artifact. Reserve time for implementation, verification, and final delivery; do not spend the whole budget on exploration or raise the limit. For research, review, explanation, or monitoring requests, preserve read-only intent: synthesize sufficient evidence rather than making edits. This advisory grants no permissions, does not require success, and never justifies weakening tests or claiming unverified work."
        ))
    }

    pub(super) fn observe(&mut self, observations: &[ToolCallObservation]) {
        for observation in observations {
            if observation.tool_name != "bash" {
                continue;
            }
            let Some(identity) = recovery_identity(&observation.signature) else {
                continue;
            };
            match observation.status {
                ToolExecutionStatus::Failed
                    if self.pending_failures.len() < MAX_PENDING_FAILURES =>
                {
                    self.pending_failures.insert(identity);
                }
                ToolExecutionStatus::Succeeded => {
                    self.pending_failures.remove(&identity);
                }
                _ => {}
            }
        }
        if self.pending_failures.is_empty() {
            self.unresolved_turns = 0;
            self.recovery_sent = false;
        } else {
            // Novel reads, wrapper commands and edits do not prove a failed check resolved.
            self.unresolved_turns = self.unresolved_turns.saturating_add(1);
        }
    }

    pub(super) fn next_note(&mut self, turn: usize, limit: usize) -> Option<String> {
        let remaining = limit.saturating_sub(turn).saturating_add(1);
        if turn > 1 && remaining == 1 && !self.final_warning_sent {
            self.final_warning_sent = true;
            return Some("Final model turn within the existing hard limit. If the task is verified, deliver the result now. Otherwise report the unresolved failure or unverified change honestly; tool calls will not earn another turn. Do not claim success from partial checks.".into());
        }
        let warning_at = (limit / 3).clamp(2, 6);
        if turn > 1 && remaining <= warning_at && !self.budget_warning_sent {
            self.budget_warning_sent = true;
            return Some(format!(
                "Run budget: {remaining} model turns remain, including this one; the limit will not be extended. Prioritize one focused repair, verification of the final artifact, and delivery. Before changing a self-authored assertion, reconcile its expected result with the user's specification. Avoid unrelated probes or cosmetic edits; preserve unresolved failures in the final report."
            ));
        }
        if self.unresolved_turns >= 3 && !self.recovery_sent {
            self.recovery_sent = true;
            return Some("Recovery checkpoint: a failed shell command has not been observed passing again. New reads, different command wrappers or edits alone do not resolve that evidence. Decide whether it exposed an implementation defect, an environment problem, or an incorrect self-authored test by checking the user's contract. Make the smallest justified repair and rerun the relevant check; do not weaken tests merely to obtain a pass. If the failure was an intentional probe, explain why it is irrelevant and continue. This is guidance, not a completion or execution gate.".into());
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(
        signature: &str,
        tool: &str,
        status: ToolExecutionStatus,
    ) -> ToolCallObservation {
        ToolCallObservation {
            signature: if signature.starts_with("bash:") {
                signature.into()
            } else {
                format!("{tool}:{}", serde_json::json!({"command": signature}))
            },
            tool_name: tool.into(),
            status,
            makes_progress: false,
            progress_reason: None,
            semantic_evidence: None,
            leaves_pending_work: false,
        }
    }

    #[test]
    fn successful_retry_ignores_metadata_but_not_workdir_or_command() {
        let mut advisory = RecoveryAdvisory::default();
        advisory.observe(&[observation(
            r#"bash:{"command":"node test.mjs","timeout":10,"title":"check"}"#,
            "bash",
            ToolExecutionStatus::Failed,
        )]);
        for arguments in [
            r#"bash:{"command":"node test.mjs","workdir":"other"}"#,
            r#"bash:{"command":"node other.test.mjs"}"#,
        ] {
            advisory.observe(&[observation(
                arguments,
                "bash",
                ToolExecutionStatus::Succeeded,
            )]);
            assert_eq!(advisory.pending_failures.len(), 1);
        }
        advisory.observe(&[observation(
            r#"bash:{"command":"node test.mjs","workdir":".","timeout":90,"title":"retry","run_in_background":false}"#,
            "bash", ToolExecutionStatus::Succeeded,
        )]);
        assert!(advisory.pending_failures.is_empty());
        assert!(advisory.next_note(4, 100).is_none());
    }

    #[test]
    fn recovery_survives_reads_and_successful_unrelated_probes() {
        let mut advisory = RecoveryAdvisory::default();
        advisory.observe(&[observation(
            "node test.mjs",
            "bash",
            ToolExecutionStatus::Failed,
        )]);
        advisory.observe(&[observation(
            "solution",
            "read",
            ToolExecutionStatus::Succeeded,
        )]);
        advisory.observe(&[observation(
            "grep module",
            "bash",
            ToolExecutionStatus::Succeeded,
        )]);
        assert!(
            advisory
                .next_note(4, 100)
                .unwrap()
                .contains("user's contract")
        );
        assert!(advisory.next_note(5, 100).is_none());
        advisory.observe(&[observation(
            "node test.mjs",
            "bash",
            ToolExecutionStatus::Succeeded,
        )]);
        assert!(advisory.pending_failures.is_empty());
        advisory.observe(&[observation(
            "another check",
            "bash",
            ToolExecutionStatus::Failed,
        )]);
        advisory.observe(&[]);
        advisory.observe(&[]);
        assert!(advisory.next_note(8, 100).is_some());
    }

    #[test]
    fn edits_do_not_erase_failure_and_pending_work_is_not_success() {
        let mut advisory = RecoveryAdvisory::default();
        advisory.observe(&[observation("check", "bash", ToolExecutionStatus::Failed)]);
        advisory.observe(&[observation(
            "repair",
            "edit",
            ToolExecutionStatus::Succeeded,
        )]);
        advisory.observe(&[observation("check", "bash", ToolExecutionStatus::Started)]);
        assert!(advisory.next_note(4, 100).is_some());
        assert_eq!(advisory.pending_failures.len(), 1);
    }

    #[test]
    fn budget_notes_arrive_before_exhaustion_without_extending_limit() {
        let mut advisory = RecoveryAdvisory::default();
        for turn in 1..9 {
            assert!(advisory.next_note(turn, 12).is_none());
        }
        assert!(advisory.next_note(9, 12).unwrap().contains("4 model turns"));
        assert!(advisory.next_note(10, 12).is_none());
        assert!(advisory.next_note(11, 12).is_none());
        assert!(
            advisory
                .next_note(12, 12)
                .unwrap()
                .contains("Final model turn")
        );
        assert!(advisory.next_note(12, 12).is_none());
        assert!(RecoveryAdvisory::default().next_note(1, 1).is_none());
    }

    #[test]
    fn delivery_checkpoints_reserve_budget_and_preserve_task_authority() {
        let mut advisory = RecoveryAdvisory::default();
        assert!(advisory.delivery_note(20, 200, false).is_none());
        let early = advisory.delivery_note(21, 200, false).unwrap();
        assert!(early.contains("180 model turns remain"));
        assert!(early.contains("smallest runnable end-to-end slice"));
        assert!(early.contains("preserve read-only intent"));
        assert!(early.contains("grants no permissions"));
        for turn in 22..=100 {
            assert!(advisory.delivery_note(turn, 200, false).is_none());
        }
        let middle = advisory.delivery_note(101, 200, true).unwrap();
        assert!(middle.contains("100 model turns remain"));
        assert!(middle.contains("does not prove"));
        assert!(!middle.contains("No workspace changes"));
        assert!(advisory.delivery_note(150, 200, true).is_none());
        assert!(
            advisory
                .delivery_note(151, 200, true)
                .unwrap()
                .contains("50 model turns remain")
        );
        assert!(advisory.delivery_note(199, 200, false).is_none());
        assert!(
            RecoveryAdvisory::default()
                .delivery_note(12, 12, false)
                .is_none()
        );
        // Resetting for new user steering re-arms the advisory, not permissions.
        advisory = RecoveryAdvisory::default();
        assert!(advisory.delivery_note(30, 200, false).is_some());
    }

    #[test]
    fn pending_failure_storage_is_bounded() {
        let mut advisory = RecoveryAdvisory::default();
        for index in 0..100 {
            advisory.observe(&[observation(
                &index.to_string(),
                "bash",
                ToolExecutionStatus::Failed,
            )]);
        }
        assert_eq!(advisory.pending_failures.len(), MAX_PENDING_FAILURES);
    }
}
