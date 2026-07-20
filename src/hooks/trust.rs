//! Load-time trust for project-config hooks: gating happens once per config
//! (when [`super::HookEngine`] is built), not per fire — a `PostFileWrite`
//! formatter hook that reprompted on every write would be unusable.
//!
//! Global-config hooks are pre-trusted (the user wrote their own home file).
//! Project-config hooks with a `shell`/`http` action need a one-time
//! approval, persisted as a `hook`-kind permission rule
//! (`PermissionManager::load_hooks`) keyed by a content hash of the complete
//! effect declaration, so changing the command, matcher, failure behavior, or
//! declared capabilities re-prompts. `llm_prompt` hooks skip this gate: they
//! only ever reach the already-authorized provider, never a new process or
//! network endpoint.

use std::sync::Arc;

use crate::config::{ConfigSource, HookAction, HookDef};
use crate::interaction::{
    InteractionOutcome, InteractionRequest, InteractionService, InteractionStatus,
    PermissionDecision,
};
use crate::permissions::{Permission, PermissionManager};

pub(super) enum TrustOutcome {
    Trusted,
    Denied,
}

/// A stable identity for one hook's complete effect declaration:
/// `hook.<name>:<hash>`.
/// Deliberately blake3 rather than sha256 — bonsai already depends on blake3
/// for the MCP wire-name hash (`src/extension/mod.rs`), and this needs the
/// same property (a stable content fingerprint), not interoperability with an
/// external format, so a second hash crate buys nothing.
pub(super) fn trust_pattern(def: &HookDef) -> String {
    let action = match &def.action {
        HookAction::Shell { command } => format!("shell:{command}"),
        HookAction::Http { url, headers } => format!("http:{url}:{headers:?}"),
        HookAction::LlmPrompt { prompt } => format!("llm_prompt:{prompt}"),
    };
    let content = format!(
        "v2\nname={}\nevent={}\naction={action}\nmatcher={:?}\ntimeout_secs={}\nblocking={}\non_failure={:?}\ncapabilities={:?}",
        def.name,
        def.event.wire_name(),
        def.matcher,
        def.timeout_secs,
        def.blocking,
        def.on_failure,
        def.capabilities,
    );
    let hash = blake3::hash(content.as_bytes());
    format!("hook.{}:{}", def.name, &hash.to_hex()[..16])
}

fn action_kind_label(action: &HookAction) -> &'static str {
    match action {
        HookAction::Shell { .. } => "shell",
        HookAction::Http { .. } => "http",
        HookAction::LlmPrompt { .. } => "llm_prompt",
    }
}

fn action_preview(action: &HookAction) -> String {
    match action {
        HookAction::Shell { command } => command.clone(),
        HookAction::Http { url, .. } => url.clone(),
        HookAction::LlmPrompt { prompt } => prompt.clone(),
    }
}

/// Whether authorizing this hook definition would need to prompt — i.e.
/// [`authorize`] would reach its `interaction.request(...)` call rather than
/// resolving from an already-persisted rule. Pure and synchronous, so
/// [`super::HookEngine::build`] can decide *before* calling `authorize`
/// whether it's safe to await inline (no prompt: `Global` source, a
/// non-shell/http action, or an already-persisted allow/deny rule) or must
/// defer to a background task (a prompt needs a live, already-running
/// interactive session to answer — see `build`'s doc comment for why).
pub(super) fn needs_interactive_prompt(
    def: &HookDef,
    source: ConfigSource,
    pattern: &str,
    permissions: &PermissionManager,
) -> bool {
    if matches!(source, ConfigSource::Global) {
        return false;
    }
    if !matches!(
        def.action,
        HookAction::Shell { .. } | HookAction::Http { .. }
    ) {
        return false;
    }
    matches!(permissions.check_one(pattern), Permission::Ask)
}

/// Authorize one hook definition from `source`. Fail-closed: a noninteractive
/// session (headless/eval) or a cancelled prompt denies rather than trusting
/// by default.
pub(super) async fn authorize(
    def: &HookDef,
    source: ConfigSource,
    pattern: &str,
    permissions: &PermissionManager,
    interaction: &Arc<InteractionService>,
) -> TrustOutcome {
    if matches!(source, ConfigSource::Global) {
        return TrustOutcome::Trusted;
    }
    if !matches!(
        def.action,
        HookAction::Shell { .. } | HookAction::Http { .. }
    ) {
        return TrustOutcome::Trusted;
    }
    match permissions.check_one(pattern) {
        Permission::Allow => return TrustOutcome::Trusted,
        Permission::Deny => return TrustOutcome::Denied,
        Permission::Ask => {}
    }

    let name = def.name.clone();
    let event = def.event.wire_name().to_string();
    let action_kind = action_kind_label(&def.action).to_string();
    let action_preview = action_preview(&def.action);
    let build = move |request_id| InteractionRequest::HookTrust {
        request_id,
        name,
        event,
        action_kind,
        action_preview,
    };
    match interaction.request(build).await {
        Ok(InteractionOutcome::Permission(
            PermissionDecision::AllowOnce | PermissionDecision::AllowMatching,
        )) => TrustOutcome::Trusted,
        Ok(InteractionOutcome::Permission(PermissionDecision::AllowForProject)) => {
            if let Err(err) = permissions.allow_for_project(pattern).await {
                tracing::warn!(hook = %def.name, %err, "failed to persist project hook-trust rule");
            }
            TrustOutcome::Trusted
        }
        Ok(InteractionOutcome::Permission(PermissionDecision::Deny)) => TrustOutcome::Denied,
        Err(InteractionStatus::Noninteractive) | Err(InteractionStatus::Cancelled) | Ok(_) => {
            TrustOutcome::Denied
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Capability, FailureBehavior, HookEvent, HookMatcher};

    fn hook() -> HookDef {
        HookDef {
            name: "guard".to_string(),
            event: HookEvent::PreFileWrite,
            matcher: HookMatcher::default(),
            action: HookAction::Shell {
                command: "cargo fmt".to_string(),
            },
            timeout_secs: 30,
            blocking: true,
            on_failure: FailureBehavior::Warn,
            capabilities: vec![Capability::Write],
            enabled: true,
        }
    }

    #[test]
    fn trust_hash_changes_with_the_complete_effect_declaration() {
        let base = hook();
        let base_pattern = trust_pattern(&base);

        let mut changed_failure = base.clone();
        changed_failure.on_failure = FailureBehavior::Block;
        assert_ne!(base_pattern, trust_pattern(&changed_failure));

        let mut changed_capability = base.clone();
        changed_capability
            .capabilities
            .push(Capability::Irreversible);
        assert_ne!(base_pattern, trust_pattern(&changed_capability));

        let mut changed_matcher = base;
        changed_matcher.matcher.path = Some(".env*".to_string());
        assert_ne!(base_pattern, trust_pattern(&changed_matcher));
    }
}
