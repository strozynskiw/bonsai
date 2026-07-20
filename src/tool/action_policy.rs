//! Typed authorization for effects that do not have a shell command to classify.
//!
//! Tools build an [`ActionPlan`] after parsing and validating their arguments,
//! then call [`ActionPolicy::authorize`] before the first side effect. This keeps
//! the autonomy ladder load-bearing for file, LSP, and future tool mutations.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use crate::interaction::{
    InteractionOutcome, InteractionRequest, InteractionService, InteractionStatus,
    PermissionDecision,
};
use crate::output::SharedSink;
use crate::permissions::{Permission, PermissionManager, PermissionMatch, PermissionMatchSource};
use crate::sandbox::CommandSandbox;
use crate::storage::{NewAuthorizationDecision, Storage};
use crate::tool::{ApprovalLevel, RiskTier, SharedActiveSessionId};
use crate::yolo::YoloMode;

/// A concrete effect a tool has planned but not yet performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActionEffect {
    /// Reads data without changing state.
    Read,
    /// Writes a single in-project file without an irreversible shape.
    WorkspaceWrite,
    /// Deletes a file or directory entry.
    Delete,
    /// Replaces existing file content with an empty result.
    Truncate,
    /// Changes more than one project path in a single action.
    MultiFileWrite,
    /// Changes a path that stores repository control data or credentials.
    ProtectedPath,
    /// Writes outside the project containment boundary.
    ExternalWrite,
    /// Evaluates code or executes an untrusted program.
    CodeExecution,
    /// Connects to a remote endpoint.
    Network,
    /// Rewrites history, permissions, or another non-recoverable resource.
    Irreversible,
    /// Mutates only Bonsai-owned session, plan, memory, or UI state.
    InternalState,
}

impl ActionEffect {
    fn risk_tier(self) -> RiskTier {
        match self {
            Self::Read => RiskTier::ReadOnly,
            Self::WorkspaceWrite => RiskTier::Medium,
            Self::MultiFileWrite | Self::ExternalWrite | Self::CodeExecution | Self::Network => {
                RiskTier::High
            }
            Self::Delete | Self::Truncate | Self::ProtectedPath | Self::Irreversible => {
                RiskTier::Destructive
            }
            Self::InternalState => RiskTier::Low,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::WorkspaceWrite => "workspace-write",
            Self::Delete => "delete",
            Self::Truncate => "truncate",
            Self::MultiFileWrite => "multi-file-write",
            Self::ProtectedPath => "protected-path",
            Self::ExternalWrite => "external-write",
            Self::CodeExecution => "code-execution",
            Self::Network => "network",
            Self::Irreversible => "irreversible",
            Self::InternalState => "internal-state",
        }
    }
}

/// A fully parsed, side-effect-free plan submitted to the authorization gate.
#[derive(Debug, Clone)]
pub(crate) struct ActionPlan {
    rule_key: String,
    subject: String,
    effects: Vec<ActionEffect>,
    risk_tier: Option<RiskTier>,
}

/// A file-mutation target with separate operator-facing and authorization
/// identities. The display path preserves the model's alias for prompts; the
/// authorized path is the resolver-produced destination that policy evaluates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MutationTarget {
    display_path: String,
    authorized_path: PathBuf,
}

impl MutationTarget {
    pub(crate) fn new(display_path: &str, authorized_path: &Path) -> Self {
        Self {
            display_path: display_path.to_string(),
            authorized_path: authorized_path.to_path_buf(),
        }
    }

    fn display_path(&self) -> &str {
        &self.display_path
    }

    fn authorized_path(&self) -> &Path {
        &self.authorized_path
    }
}

impl ActionPlan {
    /// Create a plan for a tool whose stable permission namespace is `rule_key`.
    pub(crate) fn new(
        rule_key: impl Into<String>,
        subject: impl Into<String>,
        effects: impl IntoIterator<Item = ActionEffect>,
    ) -> Self {
        let mut effects: Vec<ActionEffect> = effects.into_iter().collect();
        if effects.is_empty() {
            effects.push(ActionEffect::Read);
        }
        Self {
            rule_key: rule_key.into(),
            subject: subject.into(),
            effects,
            risk_tier: None,
        }
    }

    /// Build a file mutation plan and add multi-file/protected-path effects
    /// from the complete set of paths before authorization.
    pub(crate) fn file_mutation<'target>(
        tool_name: &str,
        targets: impl IntoIterator<Item = &'target MutationTarget>,
        effects: impl IntoIterator<Item = ActionEffect>,
    ) -> Self {
        let mut effects: Vec<ActionEffect> = effects.into_iter().collect();
        if !effects.contains(&ActionEffect::WorkspaceWrite) {
            effects.push(ActionEffect::WorkspaceWrite);
        }

        let mut target_count = 0usize;
        let mut first_display_path = None;
        let mut has_protected_target = false;
        for target in targets {
            target_count = target_count.saturating_add(1);
            first_display_path.get_or_insert_with(|| target.display_path().to_string());
            has_protected_target |= is_protected_path(target.authorized_path());
        }
        if target_count > 1 {
            effects.push(ActionEffect::MultiFileWrite);
        }
        if has_protected_target {
            effects.push(ActionEffect::ProtectedPath);
        }

        let listed_paths = match (first_display_path, target_count) {
            (None, _) => "workspace".to_string(),
            (Some(path), 1) => path,
            (Some(path), count) => format!("{path} (+{} more)", count.saturating_sub(1)),
        };
        let labels = effects
            .iter()
            .map(|effect| effect.label())
            .collect::<Vec<_>>()
            .join(", ");
        Self::new(
            format!("mutation.{tool_name}"),
            format!("{tool_name}: {listed_paths} [{labels}]"),
            effects,
        )
    }

    pub(crate) fn tier(&self) -> RiskTier {
        if let Some(tier) = self.risk_tier {
            return tier;
        }
        self.effects
            .iter()
            .map(|effect| effect.risk_tier())
            .max()
            .unwrap_or(RiskTier::Destructive)
    }

    fn rule_key(&self) -> &str {
        &self.rule_key
    }

    fn subject(&self) -> &str {
        &self.subject
    }

    pub(crate) fn with_risk_tier(mut self, risk_tier: RiskTier) -> Self {
        self.risk_tier = Some(risk_tier);
        self
    }

    fn effect_labels(&self) -> Vec<&'static str> {
        self.effects.iter().map(|effect| effect.label()).collect()
    }
}

#[derive(Clone)]
pub(crate) struct AuthorizationCallContext {
    tool_call_id: String,
    sink: SharedSink,
}

impl AuthorizationCallContext {
    pub(crate) fn new(tool_call_id: String, sink: SharedSink) -> Self {
        Self { tool_call_id, sink }
    }
}

tokio::task_local! {
    static AUTHORIZATION_CALL_CONTEXT: AuthorizationCallContext;
}

pub(crate) async fn with_authorization_call_context<F>(
    context: AuthorizationCallContext,
    future: F,
) -> F::Output
where
    F: Future,
{
    AUTHORIZATION_CALL_CONTEXT.scope(context, future).await
}

/// The id of the tool call currently executing on this task, when running
/// inside [`with_authorization_call_context`] (i.e. a real tool dispatch).
/// `None` on internal lanes that call tool code directly, e.g. self-review.
pub(crate) fn current_tool_call_id() -> Option<String> {
    AUTHORIZATION_CALL_CONTEXT
        .try_with(|context| context.tool_call_id.clone())
        .ok()
}

#[derive(Clone)]
pub(crate) struct AuthorizationLedger {
    storage: Option<Storage>,
    active_session_id: Option<SharedActiveSessionId>,
    sandbox: CommandSandbox,
    yolo_mode: YoloMode,
}

impl AuthorizationLedger {
    pub(crate) fn new(
        storage: Storage,
        active_session_id: SharedActiveSessionId,
        sandbox: CommandSandbox,
        yolo_mode: YoloMode,
    ) -> Self {
        Self {
            storage: Some(storage),
            active_session_id: Some(active_session_id),
            sandbox,
            yolo_mode,
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            storage: None,
            active_session_id: None,
            sandbox: CommandSandbox::disabled(),
            yolo_mode: YoloMode::new(),
        }
    }

    pub(crate) async fn record_internal(&self, tool_name: &str) {
        let plan = ActionPlan::new("internal", tool_name, [ActionEffect::InternalState]);
        self.record(
            &plan,
            PermissionMatch {
                permission: Permission::Allow,
                source: PermissionMatchSource::BuiltInDefault,
            },
            self.yolo_mode.level(),
            "allow",
            "built-in internal-state contract",
        )
        .await;
    }

    pub(crate) async fn record_delegation(&self, tool_name: &str) {
        let plan = ActionPlan::new(
            "delegation",
            tool_name,
            [ActionEffect::Network, ActionEffect::CodeExecution],
        );
        self.record(
            &plan,
            PermissionMatch {
                permission: Permission::Allow,
                source: PermissionMatchSource::BuiltInDefault,
            },
            self.yolo_mode.level(),
            "allow",
            "delegated tools retain their own effect gates",
        )
        .await;
    }

    pub(crate) async fn record(
        &self,
        plan: &ActionPlan,
        permission: PermissionMatch,
        autonomy: ApprovalLevel,
        decision: &'static str,
        reason: &'static str,
    ) {
        let effects = plan.effect_labels();
        let subject = compact_redacted_subject(plan.subject());
        let sandbox_posture = self.sandbox_posture();
        // The support lifecycle log's effect line: every authorization
        // decision, whether or not a session is persisting them below.
        tracing::info!(
            target: "bonsai::effect",
            surface = plan.rule_key(),
            subject = %subject,
            effects = %effects.join(","),
            tier = plan.tier().label(),
            autonomy = autonomy.label(),
            sandbox = %sandbox_posture,
            decision,
            reason,
            "authorization decision"
        );
        let context = AUTHORIZATION_CALL_CONTEXT.try_with(Clone::clone).ok();
        if let Some(context) = &context {
            context.sink.tool_output(
                &context.tool_call_id,
                &format!(
                    "[authorization] {decision} · {} · {} · {} · autonomy={} · sandbox={} · {reason}",
                    plan.tier().label(),
                    effects.join(","),
                    permission.source.label(),
                    autonomy.label(),
                    sandbox_posture,
                ),
            );
        }

        let (Some(storage), Some(active_session_id)) = (&self.storage, &self.active_session_id)
        else {
            return;
        };
        let Some(session_id) = *active_session_id.lock().await else {
            return;
        };
        let decision_record = NewAuthorizationDecision {
            tool_call_id: context
                .as_ref()
                .map(|context| context.tool_call_id.as_str()),
            surface: plan.rule_key(),
            subject: &subject,
            effects: &effects,
            risk_tier: plan.tier().label(),
            rule_source: permission.source.label(),
            autonomy_level: autonomy.label(),
            sandbox_posture: &sandbox_posture,
            decision,
            reason,
        };
        if let Err(error) = storage
            .record_authorization_decision(session_id, decision_record)
            .await
        {
            tracing::warn!(%error, "failed to persist authorization decision");
        }
    }

    fn sandbox_posture(&self) -> String {
        let state = if self.sandbox.is_active() {
            "active"
        } else if self.sandbox.is_enabled() {
            "unavailable"
        } else {
            "off"
        };
        let network = if self.sandbox.deny_network() {
            "deny"
        } else {
            "allow"
        };
        format!(
            "{}:{state},network:{network}",
            self.sandbox.backend().label()
        )
    }
}

fn compact_redacted_subject(subject: &str) -> String {
    const MAX_CHARS: usize = 240;
    let redacted = crate::redact::redact(subject);
    let normalized = redacted
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    let mut compact = normalized.chars().take(MAX_CHARS).collect::<String>();
    if normalized.chars().count() > MAX_CHARS {
        compact.push('…');
    }
    compact
}

/// The pure authorization outcome shared by Bash and typed action plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthorizationVerdict {
    /// A deny rule matched, so no prompt may override it.
    Deny,
    /// The effect fits under the active autonomy ceiling.
    AutoApproved {
        tier: RiskTier,
        matched_allow_rule: bool,
    },
    /// The user must approve this plan interactively.
    NeedsPrompt,
}

/// Combine a permission lookup, a typed risk tier, and the active autonomy
/// level. An explicit matching `Allow` remembers the user's decision across
/// sessions; only the destructive floor remains non-bypassable outside Yolo.
pub(crate) fn authorization_verdict(
    permission: PermissionMatch,
    tier: RiskTier,
    level: ApprovalLevel,
) -> AuthorizationVerdict {
    match permission.permission {
        Permission::Deny => AuthorizationVerdict::Deny,
        Permission::Allow
            if matches!(
                permission.source,
                PermissionMatchSource::Session
                    | PermissionMatchSource::Project
                    | PermissionMatchSource::Global
            ) && (tier != RiskTier::Destructive || level == ApprovalLevel::Yolo) =>
        {
            AuthorizationVerdict::AutoApproved {
                tier,
                matched_allow_rule: true,
            }
        }
        Permission::Allow | Permission::Ask if level.auto_approves(tier) => {
            AuthorizationVerdict::AutoApproved {
                tier,
                matched_allow_rule: false,
            }
        }
        Permission::Allow | Permission::Ask => AuthorizationVerdict::NeedsPrompt,
    }
}

/// Runtime gate used by non-shell tools after they have built an [`ActionPlan`].
#[derive(Clone)]
pub(crate) struct ActionPolicy {
    permissions: PermissionManager,
    interaction: Arc<InteractionService>,
    yolo_mode: YoloMode,
    ledger: AuthorizationLedger,
}

impl ActionPolicy {
    #[cfg(test)]
    pub(crate) fn new(
        permissions: PermissionManager,
        interaction: Arc<InteractionService>,
        yolo_mode: YoloMode,
    ) -> Self {
        Self::with_ledger(
            permissions,
            interaction,
            yolo_mode,
            AuthorizationLedger::disabled(),
        )
    }

    pub(crate) fn with_ledger(
        permissions: PermissionManager,
        interaction: Arc<InteractionService>,
        yolo_mode: YoloMode,
        ledger: AuthorizationLedger,
    ) -> Self {
        Self {
            permissions,
            interaction,
            yolo_mode,
            ledger,
        }
    }

    /// Authorize the whole plan before its caller mutates state.
    pub(crate) async fn authorize(&self, plan: &ActionPlan) -> Result<()> {
        let permission = self.permissions.check_one_detailed(plan.rule_key());
        let level = self.yolo_mode.level();
        if self.yolo_mode.is_enabled() {
            self.ledger
                .record(plan, permission, level, "allow", "yolo bypass")
                .await;
            return Ok(());
        }

        match authorization_verdict(permission, plan.tier(), level) {
            AuthorizationVerdict::Deny => {
                self.ledger
                    .record(plan, permission, level, "deny", "matching deny rule")
                    .await;
                anyhow::bail!(
                    "Action '{}' is blocked by a permission rule.",
                    plan.subject()
                )
            }
            AuthorizationVerdict::AutoApproved {
                matched_allow_rule, ..
            } => {
                self.ledger
                    .record(
                        plan,
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
            AuthorizationVerdict::NeedsPrompt => self.prompt(plan, permission, level).await,
        }
    }

    async fn prompt(
        &self,
        plan: &ActionPlan,
        permission: PermissionMatch,
        level: ApprovalLevel,
    ) -> Result<()> {
        let command = plan.subject().to_string();
        match self
            .interaction
            .request(move |request_id| InteractionRequest::Permission {
                request_id,
                command,
                origin: None,
            })
            .await
        {
            Ok(InteractionOutcome::Permission(PermissionDecision::AllowOnce)) => {
                self.ledger
                    .record(plan, permission, level, "allow", "user allowed once")
                    .await;
                Ok(())
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::AllowMatching)) => {
                self.permissions.allow_for_session(plan.rule_key());
                self.ledger
                    .record(plan, permission, level, "allow", "user allowed for session")
                    .await;
                Ok(())
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::AllowForProject)) => {
                if let Err(error) = self.permissions.allow_for_project(plan.rule_key()).await {
                    tracing::warn!(rule = plan.rule_key(), %error, "failed to persist mutation permission rule");
                }
                self.ledger
                    .record(plan, permission, level, "allow", "user allowed for project")
                    .await;
                Ok(())
            }
            Ok(InteractionOutcome::Permission(PermissionDecision::Deny)) => {
                self.ledger
                    .record(plan, permission, level, "deny", "user denied")
                    .await;
                anyhow::bail!("Permission denied by user for action: {}", plan.subject())
            }
            Err(InteractionStatus::Noninteractive) => {
                self.ledger
                    .record(plan, permission, level, "deny", "prompt unavailable")
                    .await;
                anyhow::bail!(
                    "Permission prompt unavailable in noninteractive mode for action: {}. Use --autonomy <level>, BONSAI_AUTONOMY, or approve the matching mutation rule in the TUI.",
                    plan.subject()
                )
            }
            Err(_) => {
                self.ledger
                    .record(plan, permission, level, "deny", "prompt cancelled")
                    .await;
                anyhow::bail!("Permission denied by user for action: {}", plan.subject())
            }
            Ok(_) => {
                self.ledger
                    .record(
                        plan,
                        permission,
                        level,
                        "deny",
                        "unexpected prompt response",
                    )
                    .await;
                anyhow::bail!(
                    "Unexpected response to the action permission request for {}.",
                    plan.subject()
                )
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn testing() -> Self {
        Self::new(
            PermissionManager::memory_only(),
            Arc::new(InteractionService::noninteractive()),
            // Test constructors exercise path/read/hook behavior independently.
            // Dedicated ActionPolicy tests cover the production verdict matrix.
            YoloMode::with_level(ApprovalLevel::Yolo),
        )
    }
}

fn is_protected_path(path: &Path) -> bool {
    path.components().any(|component| {
        let component = component.as_os_str().to_string_lossy();
        matches!(
            component.as_ref(),
            ".git" | ".bonsai" | ".env" | ".ssh" | ".aws"
        ) || component.starts_with(".env.")
            || component.ends_with(".pem")
            || component.ends_with(".key")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mutation_target(path: &str) -> MutationTarget {
        MutationTarget::new(path, Path::new(path))
    }

    fn permission_match(permission: Permission, source: PermissionMatchSource) -> PermissionMatch {
        PermissionMatch { permission, source }
    }

    #[test]
    fn allow_rules_persistently_approve_non_destructive_actions() {
        assert_eq!(
            authorization_verdict(
                permission_match(Permission::Allow, PermissionMatchSource::Project),
                RiskTier::High,
                ApprovalLevel::Ask,
            ),
            AuthorizationVerdict::AutoApproved {
                tier: RiskTier::High,
                matched_allow_rule: true,
            }
        );
        assert_eq!(
            authorization_verdict(
                permission_match(Permission::Allow, PermissionMatchSource::Project),
                RiskTier::Destructive,
                ApprovalLevel::Ask,
            ),
            AuthorizationVerdict::NeedsPrompt
        );
        assert_eq!(
            authorization_verdict(
                permission_match(Permission::Allow, PermissionMatchSource::BuiltInDefault),
                RiskTier::High,
                ApprovalLevel::Ask,
            ),
            AuthorizationVerdict::NeedsPrompt
        );
        assert_eq!(
            authorization_verdict(
                permission_match(Permission::Allow, PermissionMatchSource::BuiltInDefault),
                RiskTier::ReadOnly,
                ApprovalLevel::Balanced,
            ),
            AuthorizationVerdict::AutoApproved {
                tier: RiskTier::ReadOnly,
                matched_allow_rule: false,
            }
        );
    }

    #[test]
    fn auto_accept_prompts_for_file_removal_patches() {
        let plan = ActionPlan::file_mutation(
            "apply_patch",
            [mutation_target("src/lib.rs")].iter(),
            [ActionEffect::Delete],
        );
        assert_eq!(plan.tier(), RiskTier::Destructive);
        assert_eq!(
            authorization_verdict(
                permission_match(Permission::Ask, PermissionMatchSource::Fallback),
                plan.tier(),
                ApprovalLevel::AutoAccept,
            ),
            AuthorizationVerdict::NeedsPrompt
        );
    }

    #[test]
    fn auto_accept_approves_non_destructive_multi_file_patches() {
        let targets = [
            mutation_target("src/lib.rs"),
            mutation_target("src/main.rs"),
        ];
        let plan = ActionPlan::file_mutation(
            "apply_patch",
            targets.iter(),
            [ActionEffect::WorkspaceWrite],
        );

        assert_eq!(plan.tier(), RiskTier::High);
        assert_eq!(
            authorization_verdict(
                permission_match(Permission::Ask, PermissionMatchSource::Fallback),
                plan.tier(),
                ApprovalLevel::Balanced,
            ),
            AuthorizationVerdict::NeedsPrompt
        );
        assert_eq!(
            authorization_verdict(
                permission_match(Permission::Ask, PermissionMatchSource::Fallback),
                plan.tier(),
                ApprovalLevel::AutoAccept,
            ),
            AuthorizationVerdict::AutoApproved {
                tier: RiskTier::High,
                matched_allow_rule: false,
            }
        );
    }

    #[test]
    fn protected_paths_are_destructive_even_for_single_file_writes() {
        let plan = ActionPlan::file_mutation(
            "write",
            [mutation_target(".env.production")].iter(),
            [ActionEffect::WorkspaceWrite],
        );
        assert_eq!(plan.tier(), RiskTier::Destructive);
    }

    #[test]
    fn protected_target_is_classified_from_resolved_path_but_displays_alias() {
        let target = MutationTarget::new("safe.txt", Path::new(".env"));
        let plan = ActionPlan::file_mutation(
            "write",
            std::iter::once(&target),
            [ActionEffect::WorkspaceWrite],
        );

        assert_eq!(plan.tier(), RiskTier::Destructive);
        assert!(plan.subject().contains("safe.txt"));
        assert!(!plan.subject().contains(".env"));
    }

    #[test]
    fn ledger_subjects_are_redacted_single_line_and_bounded() {
        let secret = format!("ghp_{}", "a".repeat(40));
        let subject = format!("run\n\u{1b}[31m token={secret} {}", "x".repeat(300));
        let compact = compact_redacted_subject(&subject);

        assert!(!compact.contains('\n'));
        assert!(!compact.contains('\u{1b}'));
        assert!(!compact.contains(&secret));
        assert!(compact.contains("[REDACTED:GitHub token]"));
        assert!(compact.chars().count() <= 241);
    }

    #[tokio::test]
    async fn auto_approved_plan_persists_a_redacted_decision() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let session_id = fixture.start_session().await;
        let active_session_id = Arc::new(tokio::sync::Mutex::new(Some(session_id)));
        let yolo = YoloMode::with_level(ApprovalLevel::Balanced);
        let ledger = AuthorizationLedger::new(
            fixture.storage.clone(),
            active_session_id,
            CommandSandbox::disabled(),
            yolo.clone(),
        );
        let policy = ActionPolicy::with_ledger(
            PermissionManager::memory_only(),
            Arc::new(InteractionService::noninteractive()),
            yolo,
            ledger,
        );
        let secret = format!("ghp_{}", "a".repeat(40));
        let plan = ActionPlan::new(
            "mutation.write",
            format!("write src/main.rs token={secret}"),
            [ActionEffect::WorkspaceWrite],
        );

        policy.authorize(&plan).await.unwrap();

        let records = fixture
            .storage
            .recent_authorization_decisions(session_id, 10)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].decision, "allow");
        assert_eq!(records[0].risk_tier, "medium");
        assert_eq!(records[0].rule_source, "fallback");
        assert!(records[0].subject.contains("[REDACTED:GitHub token]"));
        assert!(!records[0].subject.contains(&secret));
    }

    #[tokio::test]
    async fn deny_rule_persists_its_source_and_reason() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let session_id = fixture.start_session().await;
        let active_session_id = Arc::new(tokio::sync::Mutex::new(Some(session_id)));
        let yolo = YoloMode::with_level(ApprovalLevel::Balanced);
        let ledger = AuthorizationLedger::new(
            fixture.storage.clone(),
            active_session_id,
            CommandSandbox::disabled(),
            yolo.clone(),
        );
        let permissions = PermissionManager::memory_only();
        permissions.add_session_rule("mutation.write", Permission::Deny);
        let policy = ActionPolicy::with_ledger(
            permissions,
            Arc::new(InteractionService::noninteractive()),
            yolo,
            ledger,
        );
        let plan = ActionPlan::new(
            "mutation.write",
            "write src/main.rs",
            [ActionEffect::WorkspaceWrite],
        );

        assert!(policy.authorize(&plan).await.is_err());

        let records = fixture
            .storage
            .recent_authorization_decisions(session_id, 10)
            .await
            .unwrap();
        assert_eq!(records[0].decision, "deny");
        assert_eq!(records[0].rule_source, "session");
        assert_eq!(records[0].reason, "matching deny rule");
    }
}
