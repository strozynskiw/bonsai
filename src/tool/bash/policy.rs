//! The bash authorization boundary: classify a command's risk, run it through
//! the permission/autonomy gate, prompt the user when the rules say `Ask`, and
//! gate an approved sandbox escape. This is the enforcement floor for command
//! execution — every foreground run passes `authorize_command` (and, when it
//! opts out of the sandbox, `authorize_escape`) before the process spawns.

use std::path::Path;

use anyhow::Result;

use super::BashTool;
use super::command::CommandAnalysis;
use super::session::EscapeApproval;
use crate::interaction::{
    EscalationDecision, InteractionOutcome, InteractionStatus, PermissionDecision,
};
use crate::permissions::{Permission, PermissionMatch, PermissionMatchSource};
use crate::tool::risk::{ApprovalLevel, classify_bash};
use crate::tool::{
    ActionEffect, ActionPlan, AuthorizationVerdict, ToolExecutionContext, authorization_verdict,
};

/// Bash's name for the central authorization outcome.
pub(super) type GateVerdict = AuthorizationVerdict;

/// Decide how the permission/risk gate treats a command, purely from the
/// permission lookup, the risk analysis, and the autonomy level — no I/O, so
/// the policy matrix is unit-testable.
///
/// Permission rules select which actions are eligible for approval; the active
/// autonomy level remains the final decision maker. In particular, a persisted
/// Only a remembered user `Allow` can bypass the autonomy ceiling; built-in
/// allows still obey it, and the destructive floor remains.
pub(super) fn permission_gate(
    permission: PermissionMatch,
    analysis: &CommandAnalysis,
    level: ApprovalLevel,
) -> GateVerdict {
    authorization_verdict(permission, classify_bash(analysis), level)
}

/// A hook execution warning is visible but never fatal. Routed to the caller's
/// sink when a `ToolExecutionContext` is present (the normal run-loop path);
/// falls back to a trace log for the bare `execute()` path (tests, and any
/// direct call with no UI to notify).
pub(super) fn emit_hook_warnings(context: Option<&ToolExecutionContext>, warnings: &[String]) {
    for warning in warnings {
        match context {
            Some(context) => context.sink().status(&format!("[hooks] {warning}")),
            None => tracing::warn!(%warning, "bash hook warning"),
        }
    }
}

impl BashTool {
    /// The permission/risk gate: refuse a denied command, run an approved or
    /// under-ceiling one, prompt for the rest. The verdict itself is the pure
    /// [`permission_gate`]; this wrapper owns the yolo bypass, the threshold
    /// log line, and the interactive prompt.
    pub(super) async fn authorize_command(
        &self,
        command: &str,
        analysis: &CommandAnalysis,
        origin: Option<&str>,
    ) -> Result<()> {
        let tier = classify_bash(analysis);
        let plan =
            ActionPlan::new("bash", command, [ActionEffect::CodeExecution]).with_risk_tier(tier);
        let permission = self
            .permissions
            .check_all_detailed(analysis.permission_commands());
        let configured_level = self.yolo_mode.level();
        if self.yolo_mode.is_enabled() {
            self.authorization_ledger
                .record(&plan, permission, configured_level, "allow", "yolo bypass")
                .await;
            return Ok(());
        }
        let level = if configured_level == ApprovalLevel::AutoAccept && !self.sandbox.is_active() {
            // AutoAccept's high-risk tier is only meaningful with an actual OS
            // confinement boundary. Without one, retain Balanced's routine
            // build/verify loop but force high-risk effects back through a
            // prompt instead of running them unconfined.
            tracing::warn!(
                command = analysis.permission_command(),
                "sandbox unavailable; lowering auto-accept command ceiling to balanced"
            );
            ApprovalLevel::Balanced
        } else {
            configured_level
        };
        match permission_gate(permission, analysis, level) {
            GateVerdict::Deny => {
                self.authorization_ledger
                    .record(&plan, permission, level, "deny", "matching deny rule")
                    .await;
                anyhow::bail!("Command not allowed by permission rules: {command}")
            }
            GateVerdict::AutoApproved {
                tier,
                matched_allow_rule,
            } => {
                tracing::info!(
                    command = analysis.permission_command(),
                    tier = tier.label(),
                    reason = if matched_allow_rule {
                        "matching allow rule"
                    } else {
                        "autonomy threshold"
                    },
                    "bash command auto-approved"
                );
                self.authorization_ledger
                    .record(
                        &plan,
                        permission,
                        level,
                        "allow",
                        if matched_allow_rule {
                            "matching allow rule"
                        } else {
                            "autonomy ceiling"
                        },
                    )
                    .await;
                Ok(())
            }
            GateVerdict::NeedsPrompt => {
                self.prompt_for_permission(command, analysis, origin, &plan, permission, level)
                    .await
            }
        }
    }

    /// Prompt the user to approve a command the rules marked `Ask` (and that the
    /// level didn't auto-approve, including a destructive command an allow rule
    /// would otherwise have run). Returns `Ok(())` when the command may proceed
    /// and an error when the user — or noninteractive mode — declines.
    async fn prompt_for_permission(
        &self,
        command: &str,
        analysis: &CommandAnalysis,
        origin: Option<&str>,
        plan: &ActionPlan,
        permission: PermissionMatch,
        level: ApprovalLevel,
    ) -> Result<()> {
        let request_command = command.to_string();
        let origin = origin.map(str::to_string);
        let outcome = self
            .interaction
            .request(
                move |id| crate::interaction::InteractionRequest::Permission {
                    request_id: id,
                    command: request_command,
                    origin,
                },
            )
            .await;
        match outcome {
            Ok(InteractionOutcome::Permission(PermissionDecision::AllowOnce)) => {
                self.authorization_ledger
                    .record(plan, permission, level, "allow", "user allowed once")
                    .await;
                Ok(())
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::AllowMatching)) => {
                self.permissions
                    .allow_for_session(analysis.permission_command());
                self.authorization_ledger
                    .record(plan, permission, level, "allow", "user allowed for session")
                    .await;
                Ok(())
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::AllowForProject)) => {
                if let Err(err) = self
                    .permissions
                    .allow_for_project(analysis.permission_command())
                    .await
                {
                    tracing::warn!(
                        command = analysis.permission_command(),
                        %err,
                        "failed to persist project permission rule"
                    );
                }
                self.authorization_ledger
                    .record(plan, permission, level, "allow", "user allowed for project")
                    .await;
                Ok(())
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::DenyForProject)) => {
                if let Err(err) = self
                    .permissions
                    .deny_for_project(analysis.permission_command())
                    .await
                {
                    tracing::warn!(
                        command = analysis.permission_command(),
                        %err,
                        "failed to persist project deny rule"
                    );
                }
                self.authorization_ledger
                    .record(plan, permission, level, "deny", "user denied for project")
                    .await;
                anyhow::bail!("Permission denied by user for command: {command}");
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::Deny)) => {
                self.authorization_ledger
                    .record(plan, permission, level, "deny", "user denied")
                    .await;
                anyhow::bail!("Permission denied by user for command: {command}");
            }
            Err(InteractionStatus::Noninteractive) => {
                self.authorization_ledger
                    .record(plan, permission, level, "deny", "prompt unavailable")
                    .await;
                anyhow::bail!(
                    "Permission prompt unavailable in noninteractive mode for command: {command}. Use --autonomy <level>, BONSAI_AUTONOMY, or add a project allow rule in the TUI."
                );
            }
            Err(_) => {
                self.authorization_ledger
                    .record(plan, permission, level, "deny", "prompt cancelled")
                    .await;
                anyhow::bail!("Permission denied by user for command: {command}");
            }
            Ok(_) => {
                self.authorization_ledger
                    .record(
                        plan,
                        permission,
                        level,
                        "deny",
                        "unexpected prompt response",
                    )
                    .await;
                anyhow::bail!(
                    "Permission prompt received an unexpected response for command: {command}"
                );
            }
        }
    }

    /// Prompt the user to run `command` (resolved to run in `cwd`) OUTSIDE the
    /// active sandbox. Mirrors [`prompt_for_permission`](Self::prompt_for_permission):
    /// fail-closed, and a `Deny` aborts the command rather than silently running
    /// confined (which the model expects to fail). A prior session grant for the
    /// exact `(cwd, command)` pair skips the prompt and reports
    /// [`EscapeApproval::Cached`].
    pub(super) async fn authorize_escape(
        &self,
        cwd: &Path,
        command: &str,
        origin: Option<&str>,
    ) -> Result<EscapeApproval> {
        let plan = ActionPlan::new(
            "bash.escape",
            format!("{}: {command}", cwd.display()),
            [ActionEffect::ExternalWrite, ActionEffect::Network],
        );
        let permission = PermissionMatch {
            permission: Permission::Ask,
            source: PermissionMatchSource::Fallback,
        };
        let level = self.yolo_mode.level();
        if self.escape_grants.is_granted(cwd, command) {
            self.authorization_ledger
                .record(
                    &plan,
                    permission,
                    level,
                    "allow",
                    "cached session sandbox escape",
                )
                .await;
            return Ok(EscapeApproval::Cached);
        }
        let request_command = command.to_string();
        let origin = origin.map(str::to_string);
        let outcome = self
            .interaction
            .request(
                move |id| crate::interaction::InteractionRequest::SandboxEscalation {
                    request_id: id,
                    command: request_command,
                    origin,
                },
            )
            .await;
        match outcome {
            Ok(InteractionOutcome::SandboxEscalation(EscalationDecision::AllowOnce)) => {
                self.authorization_ledger
                    .record(&plan, permission, level, "allow", "user allowed once")
                    .await;
                Ok(EscapeApproval::Once)
            }
            Ok(InteractionOutcome::SandboxEscalation(EscalationDecision::AllowForSession)) => {
                self.escape_grants.grant(cwd, command);
                self.authorization_ledger
                    .record(
                        &plan,
                        permission,
                        level,
                        "allow",
                        "user allowed for session",
                    )
                    .await;
                Ok(EscapeApproval::Session)
            }
            Ok(InteractionOutcome::SandboxEscalation(EscalationDecision::Deny)) => {
                self.authorization_ledger
                    .record(&plan, permission, level, "deny", "user denied")
                    .await;
                anyhow::bail!("Sandbox escape denied by user for command: {command}");
            }
            Err(InteractionStatus::Noninteractive) => {
                self.authorization_ledger
                    .record(&plan, permission, level, "deny", "prompt unavailable")
                    .await;
                anyhow::bail!(
                    "Sandbox escape prompt unavailable in noninteractive mode for command: {command}"
                );
            }
            Err(_) => {
                self.authorization_ledger
                    .record(&plan, permission, level, "deny", "prompt cancelled")
                    .await;
                anyhow::bail!("Sandbox escape denied (prompt cancelled) for command: {command}");
            }
            Ok(_) => {
                self.authorization_ledger
                    .record(
                        &plan,
                        permission,
                        level,
                        "deny",
                        "unexpected prompt response",
                    )
                    .await;
                anyhow::bail!(
                    "Sandbox escape prompt received an unexpected response for command: {command}"
                );
            }
        }
    }
}
