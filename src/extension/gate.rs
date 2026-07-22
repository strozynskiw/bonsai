//! The generic extension permission gate. Placement: inside the
//! extension tool adapter (e.g. `McpTool::execute`), not a branch in
//! `run_loop::execute_single_tool_call` — dispatch has no permission/
//! interaction handles, and bash/WebFetch already establish self-gating at
//! the tool's choke point. Since every extension tool is constructed through
//! one adapter, this is still the single generic gate the roadmap asks for.

use std::sync::Arc;

use crate::config::{Capability, DeclaredCapabilities};
use crate::interaction::{
    InteractionOutcome, InteractionRequest, InteractionService, InteractionStatus,
    PermissionDecision,
};
use crate::permissions::{PermissionManager, PermissionMatch};
use crate::tool::{
    ActionEffect, ActionPlan, ApprovalLevel, AuthorizationLedger, AuthorizationVerdict, RiskTier,
    authorization_verdict,
};
use crate::yolo::YoloMode;

use super::capabilities::capability_labels;

/// Extension-facing name for the shared authorization outcome.
pub(crate) type GateVerdict = AuthorizationVerdict;

/// Pure, unit-testable — the extension analog of bash's `permission_gate`.
pub(crate) fn extension_gate(
    permission: PermissionMatch,
    tier: RiskTier,
    level: ApprovalLevel,
) -> GateVerdict {
    authorization_verdict(permission, tier, level)
}

/// Fail-closed authorization for namespaced extension tool calls (MCP tools
/// and hooks, which reuse the same rule `kind` under their own `hook.<name>`
/// pattern namespace). One instance is shared across every extension
/// tool, mirroring `WebFetchTool`'s domain gate.
pub(crate) struct ExtensionGate {
    permissions: PermissionManager,
    interaction: Arc<InteractionService>,
    yolo: YoloMode,
    authorization_ledger: AuthorizationLedger,
}

#[derive(Debug, Clone, Copy)]
struct PendingAuthorization<'a> {
    plan: &'a ActionPlan,
    permission: PermissionMatch,
    level: ApprovalLevel,
}

impl ExtensionGate {
    pub(crate) fn new(
        permissions: PermissionManager,
        interaction: Arc<InteractionService>,
        yolo: YoloMode,
        authorization_ledger: AuthorizationLedger,
    ) -> Self {
        Self {
            permissions,
            interaction,
            yolo,
            authorization_ledger,
        }
    }

    /// Authorize one call to the extension tool identified by `display_id`
    /// (e.g. `"mcp.github.create_issue"`). `server` names the owning
    /// extension for the prompt; `args_preview` is a short, pre-redacted
    /// first-line preview of the call's arguments.
    pub(crate) async fn authorize(
        &self,
        display_id: &str,
        grant_key: &str,
        server: &str,
        capabilities: &DeclaredCapabilities,
        args_preview: &str,
    ) -> anyhow::Result<()> {
        let permission = self.permissions.check_one_detailed(grant_key);
        let tier = capabilities.risk_tier();
        let level = self.yolo.level();
        let plan = ActionPlan::new(
            "mcp",
            format!("{display_id}: {args_preview}"),
            capability_effects(capabilities),
        )
        .with_risk_tier(tier);
        match extension_gate(permission, tier, level) {
            GateVerdict::Deny => {
                self.authorization_ledger
                    .record(&plan, permission, level, "deny", "matching deny rule")
                    .await;
                anyhow::bail!("Extension tool '{display_id}' is blocked by a permission rule.")
            }
            GateVerdict::AutoApproved {
                matched_allow_rule, ..
            } => {
                self.authorization_ledger
                    .record(
                        &plan,
                        permission,
                        level,
                        "allow",
                        if matched_allow_rule {
                            "allow rule and autonomy ceiling"
                        } else {
                            "autonomy ceiling"
                        },
                    )
                    .await;
                Ok(())
            }
            GateVerdict::NeedsPrompt => {
                self.prompt(
                    display_id,
                    grant_key,
                    server,
                    capabilities,
                    args_preview,
                    PendingAuthorization {
                        plan: &plan,
                        permission,
                        level,
                    },
                )
                .await
            }
        }
    }

    async fn prompt(
        &self,
        display_id: &str,
        grant_key: &str,
        server: &str,
        capabilities: &DeclaredCapabilities,
        args_preview: &str,
        authorization: PendingAuthorization<'_>,
    ) -> anyhow::Result<()> {
        let PendingAuthorization {
            plan,
            permission,
            level,
        } = authorization;
        let id = display_id.to_string();
        let server = server.to_string();
        let capability_list = capability_labels(&capabilities.capabilities);
        let preview = args_preview.to_string();
        let build = move |request_id| InteractionRequest::ExtensionTool {
            request_id,
            id,
            server,
            capabilities: capability_list,
            args_preview: preview,
        };
        match self.interaction.request(build).await {
            Ok(InteractionOutcome::Permission(PermissionDecision::AllowOnce)) => {
                self.authorization_ledger
                    .record(plan, permission, level, "allow", "user allowed once")
                    .await;
                Ok(())
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::AllowMatching)) => {
                self.permissions.allow_for_session(grant_key);
                self.authorization_ledger
                    .record(plan, permission, level, "allow", "user allowed for session")
                    .await;
                Ok(())
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::AllowForProject)) => {
                if let Err(err) = self.permissions.allow_for_project(grant_key).await {
                    tracing::warn!(display_id, %err, "failed to persist project mcp rule");
                }
                self.authorization_ledger
                    .record(plan, permission, level, "allow", "user allowed for project")
                    .await;
                Ok(())
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::DenyForProject)) => {
                if let Err(err) = self.permissions.deny_for_project(grant_key).await {
                    tracing::warn!(display_id, %err, "failed to persist project mcp deny rule");
                }
                self.authorization_ledger
                    .record(plan, permission, level, "deny", "user denied for project")
                    .await;
                anyhow::bail!("Permission denied by user for '{display_id}'.")
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::Deny)) => {
                self.authorization_ledger
                    .record(plan, permission, level, "deny", "user denied")
                    .await;
                anyhow::bail!("Permission denied by user for '{display_id}'.")
            }
            Err(InteractionStatus::Noninteractive) => {
                self.authorization_ledger
                    .record(plan, permission, level, "deny", "prompt unavailable")
                    .await;
                anyhow::bail!(
                    "Extension tool '{display_id}' is not allowed and prompting is unavailable in \
                     noninteractive mode. Approve it in the TUI (allow for project), or run with a \
                     higher --autonomy level."
                )
            }
            Err(_) => {
                self.authorization_ledger
                    .record(plan, permission, level, "deny", "prompt cancelled")
                    .await;
                anyhow::bail!("Permission request cancelled for '{display_id}'.")
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
                    "Unexpected response to the extension tool permission request for '{display_id}'."
                )
            }
        }
    }
}

fn capability_effects(capabilities: &DeclaredCapabilities) -> Vec<ActionEffect> {
    if capabilities.capabilities.is_empty() {
        return vec![ActionEffect::ExternalWrite];
    }
    capabilities
        .capabilities
        .iter()
        .map(|capability| match capability {
            Capability::Read | Capability::UntrustedOutput => ActionEffect::Read,
            Capability::Write => ActionEffect::ExternalWrite,
            Capability::Network => ActionEffect::Network,
            Capability::Shell => ActionEffect::CodeExecution,
            Capability::Irreversible => ActionEffect::Irreversible,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{Permission, PermissionMatchSource};

    fn permission_match(permission: Permission, source: PermissionMatchSource) -> PermissionMatch {
        PermissionMatch { permission, source }
    }

    #[test]
    fn deny_rule_wins() {
        assert_eq!(
            extension_gate(
                permission_match(Permission::Deny, PermissionMatchSource::HardDeny),
                RiskTier::Low,
                ApprovalLevel::Yolo,
            ),
            GateVerdict::Deny
        );
    }

    #[test]
    fn project_allow_rule_bypasses_the_active_level() {
        assert_eq!(
            extension_gate(
                permission_match(Permission::Allow, PermissionMatchSource::Project),
                RiskTier::High,
                ApprovalLevel::Ask,
            ),
            GateVerdict::AutoApproved {
                tier: RiskTier::High,
                matched_allow_rule: true,
            }
        );
    }

    #[test]
    fn undeclared_capabilities_need_a_prompt_at_balanced() {
        // High > Balanced's Medium ceiling.
        assert_eq!(
            extension_gate(
                permission_match(Permission::Ask, PermissionMatchSource::Fallback),
                RiskTier::High,
                ApprovalLevel::Balanced,
            ),
            GateVerdict::NeedsPrompt
        );
    }

    #[test]
    fn declared_read_auto_approves_at_balanced_but_not_conservative() {
        assert_eq!(
            extension_gate(
                permission_match(Permission::Ask, PermissionMatchSource::Fallback),
                RiskTier::Low,
                ApprovalLevel::Balanced,
            ),
            GateVerdict::AutoApproved {
                tier: RiskTier::Low,
                matched_allow_rule: false,
            }
        );
        assert_eq!(
            extension_gate(
                permission_match(Permission::Ask, PermissionMatchSource::Fallback),
                RiskTier::Low,
                ApprovalLevel::Conservative,
            ),
            GateVerdict::NeedsPrompt
        );
    }

    #[test]
    fn conservative_never_auto_approves_above_read_only() {
        for tier in [RiskTier::Low, RiskTier::Medium, RiskTier::High] {
            assert_eq!(
                extension_gate(
                    permission_match(Permission::Ask, PermissionMatchSource::Fallback),
                    tier,
                    ApprovalLevel::Conservative,
                ),
                GateVerdict::NeedsPrompt,
                "{tier:?}"
            );
        }
    }
}
