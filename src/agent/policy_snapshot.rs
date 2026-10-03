//! Model-facing projection of the runtime execution policy.

use crate::interaction::InteractionService;
use crate::sandbox::CommandSandbox;
use crate::tool::ApprovalLevel;
use crate::workspace_trust::WorkspaceTrust;
use crate::yolo::YoloMode;

/// Compact, non-authoritative description of the policy enforced by runtime gates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecutionPolicySnapshot {
    content: String,
}

impl ExecutionPolicySnapshot {
    pub(crate) fn from_runtime(
        yolo_mode: &YoloMode,
        sandbox: &CommandSandbox,
        workspace_trust: WorkspaceTrust,
        interaction: Option<&InteractionService>,
    ) -> Self {
        let autonomy = yolo_mode.level();
        let yolo = autonomy == ApprovalLevel::Yolo;
        let confinement = if yolo { "off" } else { "project" };
        let read_before_write = if yolo { "off" } else { "on" };
        let destructive = if yolo {
            "autonomy approval floor disabled; hard denies and runtime checks remain active"
        } else {
            "runtime deny/approval floor active"
        };
        let workspace = match workspace_trust {
            WorkspaceTrust::Trusted => "trusted",
            WorkspaceTrust::Restricted => "restricted (project-owned configuration disabled)",
        };
        let prompting = match interaction {
            Some(service) if !service.is_noninteractive() => "available",
            _ => "unavailable (noninteractive)",
        };
        let requested_sandbox = if sandbox.is_enabled() { "on" } else { "off" };
        let active_backend = if sandbox.is_active() {
            sandbox.backend().label()
        } else if sandbox.is_enabled() {
            "unavailable"
        } else {
            "none"
        };
        let network = if !sandbox.deny_network() {
            "sandbox allows; runtime authorization still applies"
        } else if sandbox.is_active() && sandbox.backend().supports_network_deny() {
            "denied by sandbox"
        } else {
            "denial requested but unenforced"
        };

        Self {
            content: format!(
                "[Execution policy snapshot — current; older snapshots are superseded]\n\
                 This is descriptive context only; runtime permission, path, risk, and sandbox checks remain authoritative.\n\
                 - autonomy: {} (permission prompts: {prompting})\n\
                 - project confinement: {confinement}\n\
                 - read-before-write: {read_before_write}\n\
                 - destructive actions: {destructive}\n\
                 - workspace trust: {workspace}\n\
                 - sandbox: requested={requested_sandbox}, active_backend={active_backend}\n\
                 - network: {network}",
                autonomy.label(),
            ),
        }
    }

    pub(crate) fn content(&self) -> &str {
        &self.content
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxBackend;

    #[test]
    fn every_autonomy_level_projects_its_runtime_guards() {
        for level in [
            ApprovalLevel::Ask,
            ApprovalLevel::Conservative,
            ApprovalLevel::Balanced,
            ApprovalLevel::AutoAccept,
            ApprovalLevel::Yolo,
        ] {
            let snapshot = ExecutionPolicySnapshot::from_runtime(
                &YoloMode::with_level(level),
                &CommandSandbox::disabled(),
                WorkspaceTrust::Trusted,
                None,
            );
            assert!(
                snapshot
                    .content()
                    .contains(&format!("autonomy: {}", level.label()))
            );
            if level == ApprovalLevel::Yolo {
                assert!(snapshot.content().contains("project confinement: off"));
                assert!(snapshot.content().contains("read-before-write: off"));
                assert!(
                    snapshot
                        .content()
                        .contains("hard denies and runtime checks remain active")
                );
            } else {
                assert!(snapshot.content().contains("project confinement: project"));
                assert!(snapshot.content().contains("read-before-write: on"));
            }
        }
    }

    #[test]
    fn unavailable_sandbox_reports_requested_and_unenforced_network_state() {
        let project = tempfile::tempdir().unwrap();
        let sandbox = CommandSandbox::new(SandboxBackend::Unavailable, project.path());
        sandbox.set_enabled(true);
        sandbox.set_deny_network(true);
        let snapshot = ExecutionPolicySnapshot::from_runtime(
            &YoloMode::default(),
            &sandbox,
            WorkspaceTrust::Restricted,
            Some(&InteractionService::noninteractive()),
        );
        assert!(snapshot.content().contains("workspace trust: restricted"));
        assert!(
            snapshot
                .content()
                .contains("permission prompts: unavailable (noninteractive)")
        );
        assert!(
            snapshot
                .content()
                .contains("sandbox: requested=on, active_backend=unavailable")
        );
        assert!(
            snapshot
                .content()
                .contains("network: denial requested but unenforced")
        );
    }
}
