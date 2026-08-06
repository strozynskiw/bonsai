//! Runtime-owned completion sequencing for coding tasks.

use super::verification::bash_command;
use super::*;
use crate::verification::VerificationRunStatus;

/// The next runtime-owned action at a terminal candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FinalizationStep {
    Review,
    FinishRepairs,
    FinalGate,
    FinishFinalGate,
    Complete,
}

/// One coding task advances through this sequence exactly once. Workspace
/// mutation after completion invalidates the final gate but does not silently
/// schedule a second automatic review.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum FinalizationPhase {
    #[default]
    Dormant,
    TargetedChecks,
    RepairingReview,
    FinalGatePending,
    FinalGateRunning,
    Green,
    CompleteWithoutGreenGate,
}

#[derive(Debug, Default)]
pub(super) struct FinalizationState {
    phase: FinalizationPhase,
}

impl FinalizationState {
    pub(super) fn begin_task(&mut self) {
        self.phase = FinalizationPhase::TargetedChecks;
    }

    pub(super) const fn step(&self) -> FinalizationStep {
        match self.phase {
            FinalizationPhase::TargetedChecks => FinalizationStep::Review,
            FinalizationPhase::RepairingReview => FinalizationStep::FinishRepairs,
            FinalizationPhase::FinalGatePending => FinalizationStep::FinalGate,
            FinalizationPhase::FinalGateRunning => FinalizationStep::FinishFinalGate,
            FinalizationPhase::Dormant
            | FinalizationPhase::Green
            | FinalizationPhase::CompleteWithoutGreenGate => FinalizationStep::Complete,
        }
    }

    pub(super) fn resolve_review(&mut self, injected_repair_turn: bool) {
        if self.phase != FinalizationPhase::TargetedChecks {
            return;
        }
        self.phase = if injected_repair_turn {
            FinalizationPhase::RepairingReview
        } else {
            FinalizationPhase::FinalGatePending
        };
    }

    pub(super) fn finish_review_repairs(&mut self) {
        if self.phase == FinalizationPhase::RepairingReview {
            self.phase = FinalizationPhase::FinalGatePending;
        }
    }

    pub(super) fn resolve_final_gate(&mut self, injected_gate_turn: bool) {
        if self.phase != FinalizationPhase::FinalGatePending {
            return;
        }
        self.phase = if injected_gate_turn {
            FinalizationPhase::FinalGateRunning
        } else {
            FinalizationPhase::CompleteWithoutGreenGate
        };
    }

    pub(super) fn finish_final_gate(&mut self, status: VerificationRunStatus) {
        if self.phase != FinalizationPhase::FinalGateRunning {
            return;
        }
        self.phase = match status {
            VerificationRunStatus::Passed | VerificationRunStatus::Unstable => {
                FinalizationPhase::Green
            }
            VerificationRunStatus::Stale => FinalizationPhase::FinalGatePending,
            VerificationRunStatus::Running => FinalizationPhase::FinalGateRunning,
            VerificationRunStatus::Failed
            | VerificationRunStatus::Blocked
            | VerificationRunStatus::Incomplete
            | VerificationRunStatus::Interrupted => FinalizationPhase::CompleteWithoutGreenGate,
        };
    }

    pub(super) fn note_workspace_change(&mut self) {
        if matches!(
            self.phase,
            FinalizationPhase::Green | FinalizationPhase::CompleteWithoutGreenGate
        ) {
            self.phase = FinalizationPhase::FinalGatePending;
        }
    }

    pub(super) fn is_green(&self) -> bool {
        self.phase == FinalizationPhase::Green
    }

    fn automatic_review_is_complete(&self) -> bool {
        matches!(
            self.phase,
            FinalizationPhase::RepairingReview
                | FinalizationPhase::FinalGatePending
                | FinalizationPhase::FinalGateRunning
                | FinalizationPhase::Green
                | FinalizationPhase::CompleteWithoutGreenGate
        )
    }
}

impl Agent {
    pub(super) fn finalization_step(&self) -> FinalizationStep {
        self.finalization.step()
    }

    pub(super) fn resolve_finalization_review(&mut self, injected_repair_turn: bool) {
        self.finalization.resolve_review(injected_repair_turn);
    }

    pub(super) fn finish_finalization_review_repairs(&mut self) {
        self.finalization.finish_review_repairs();
    }

    pub(super) fn resolve_finalization_gate(&mut self, injected_gate_turn: bool) {
        self.finalization.resolve_final_gate(injected_gate_turn);
    }

    pub(super) fn note_external_finalization_workspace_change(&mut self) {
        if !self.finalization.is_green() {
            return;
        }
        self.mark_latest_verification_stale(&[]);
        self.verification.after_edit_verification_pending = true;
        self.finalization.note_workspace_change();
    }

    pub(super) fn finalization_rejections(
        &self,
        tool_calls: &[ToolCall],
    ) -> HashMap<String, String> {
        tool_calls
            .iter()
            .filter_map(|call| {
                let verification_command = call.name == "bash"
                    && bash_command(&call.arguments).is_some_and(|command| {
                        self.verification_kind_for_command(&command).is_some()
                    });
                finalization_rejection(&self.finalization, call, verification_command)
                    .map(|rejection| (call.id.clone(), rejection.message().to_string()))
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalizationRejection {
    AdditionalReview,
    VerificationAfterGreenGate,
}

impl FinalizationRejection {
    const fn message(self) -> &'static str {
        match self {
            Self::AdditionalReview => {
                "The task's automatic review pass is already complete. Do not launch another reviewer; repair the existing findings, run only necessary focused checks, then proceed to the one final gate."
            }
            Self::VerificationAfterGreenGate => {
                "The final gate already passed for the unchanged workspace. Do not run another verification pass; finish now. A later workspace mutation will invalidate the gate and re-enable the necessary checks."
            }
        }
    }
}

fn finalization_rejection(
    state: &FinalizationState,
    call: &ToolCall,
    verification_command: bool,
) -> Option<FinalizationRejection> {
    if state.automatic_review_is_complete() && review_agent_call(call) {
        Some(FinalizationRejection::AdditionalReview)
    } else if state.is_green() && verification_command {
        Some(FinalizationRejection::VerificationAfterGreenGate)
    } else {
        None
    }
}

fn review_agent_call(call: &ToolCall) -> bool {
    if call.name != "agent" {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&call.arguments)
        .ok()
        .and_then(|arguments| {
            arguments
                .get("agent")
                .and_then(serde_json::Value::as_str)
                .and_then(crate::subagent::BuiltinSubagentId::parse)
        })
        .is_some_and(|agent| {
            matches!(
                agent,
                crate::subagent::BuiltinSubagentId::Review
                    | crate::subagent::BuiltinSubagentId::SecurityReview
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_machine_orders_review_repairs_and_final_gate() {
        let mut state = FinalizationState::default();
        state.begin_task();
        assert_eq!(state.step(), FinalizationStep::Review);

        state.resolve_review(true);
        assert_eq!(state.step(), FinalizationStep::FinishRepairs);

        state.finish_review_repairs();
        assert_eq!(state.step(), FinalizationStep::FinalGate);

        state.resolve_final_gate(true);
        assert_eq!(state.step(), FinalizationStep::FinishFinalGate);

        state.finish_final_gate(VerificationRunStatus::Passed);
        assert_eq!(state.step(), FinalizationStep::Complete);
        assert!(state.is_green());
    }

    #[test]
    fn workspace_change_invalidates_a_green_gate_without_repeating_review() {
        let mut state = FinalizationState {
            phase: FinalizationPhase::Green,
        };

        state.note_workspace_change();

        assert_eq!(state.step(), FinalizationStep::FinalGate);
    }

    #[test]
    fn explicit_new_task_reopens_review_after_a_green_gate() {
        let mut state = FinalizationState {
            phase: FinalizationPhase::Green,
        };

        state.begin_task();

        assert_eq!(state.step(), FinalizationStep::Review);
        assert!(!state.is_green());
    }

    #[test]
    fn review_agent_detection_is_limited_to_reviewers() {
        let review = ToolCall {
            id: "review".to_string(),
            name: "agent".to_string(),
            arguments: r#"{"agent":"review","prompt":"check"}"#.to_string(),
        };
        let explore = ToolCall {
            id: "explore".to_string(),
            name: "agent".to_string(),
            arguments: r#"{"agent":"explore","prompt":"locate"}"#.to_string(),
        };

        assert!(review_agent_call(&review));
        assert!(!review_agent_call(&explore));
    }

    #[test]
    fn green_gate_rejects_review_and_verification_but_not_other_tools() {
        let state = FinalizationState {
            phase: FinalizationPhase::Green,
        };
        let review = ToolCall {
            id: "review".to_string(),
            name: "agent".to_string(),
            arguments: r#"{"agent":"security-review","prompt":"check"}"#.to_string(),
        };
        let bash = ToolCall {
            id: "test".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"cargo test"}"#.to_string(),
        };
        let read = ToolCall {
            id: "read".to_string(),
            name: "read".to_string(),
            arguments: r#"{"path":"src/lib.rs"}"#.to_string(),
        };

        assert_eq!(
            finalization_rejection(&state, &review, false),
            Some(FinalizationRejection::AdditionalReview)
        );
        assert_eq!(
            finalization_rejection(&state, &bash, true),
            Some(FinalizationRejection::VerificationAfterGreenGate)
        );
        assert_eq!(finalization_rejection(&state, &read, false), None);
    }

    #[test]
    fn repair_phase_rejects_serial_review_but_allows_focused_verification() {
        let mut state = FinalizationState::default();
        state.begin_task();
        state.resolve_review(true);
        let review = ToolCall {
            id: "review".to_string(),
            name: "agent".to_string(),
            arguments: r#"{"agent":"review","prompt":"review again"}"#.to_string(),
        };
        let bash = ToolCall {
            id: "test".to_string(),
            name: "bash".to_string(),
            arguments: r#"{"command":"cargo test focused"}"#.to_string(),
        };

        assert_eq!(
            finalization_rejection(&state, &review, false),
            Some(FinalizationRejection::AdditionalReview)
        );
        assert_eq!(finalization_rejection(&state, &bash, true), None);
    }
}
