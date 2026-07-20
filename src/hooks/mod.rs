//! The hooks engine: `SessionStart/End`, `Pre/PostToolUse`,
//! `Pre/PostFileWrite`, `Pre/PostBash` — shell/HTTP/LLM-prompt actions fired
//! at each lifecycle event, with blocking + conditional (tool/path glob)
//! config, declared failure behavior, and load-time trust gating for
//! project-config hooks (`src/hooks/trust.rs`).
//!
//! [`HookEngine::fire`] never returns `Err`: every failure mode (a bad
//! matcher, a spawn error, a timeout, unparseable JSON) maps through the
//! hook's own `on_failure` policy instead, so a broken hook degrades to a
//! visible warning rather than crashing the agent loop it's wired into
//! (`src/agent/run_loop.rs`, `src/tool/bash.rs`, `src/tool/file_mutation.rs`).

mod actions;
mod protocol;
mod trust;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use glob::Pattern;

pub(crate) use crate::config::HookEvent;
use crate::config::{Config, ConfigSource, FailureBehavior, HookAction, HookDef};
use crate::extension::ExtensionId;
use crate::extension::status::{DisableReason, ExtensionRegistry, ExtensionState, ExtensionStatus};
use crate::interaction::InteractionService;
use crate::permissions::{Permission, PermissionManager, PermissionMatch, PermissionMatchSource};
use crate::provider::Provider;
use crate::sandbox::CommandSandbox;
use crate::tool::{ActionEffect, ActionPlan, AuthorizationLedger};
use crate::yolo::YoloMode;

impl HookEvent {
    /// The wire-protocol event name (`HookInput.event`, `BONSAI_HOOK_EVENT`).
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PreFileWrite => "PreFileWrite",
            Self::PostFileWrite => "PostFileWrite",
            Self::PreBash => "PreBash",
            Self::PostBash => "PostBash",
        }
    }

    /// Whether this event can veto/modify the action it precedes. `SessionEnd`
    /// and the `Post*` events are always non-veto regardless of a hook's own
    /// `blocking` flag — there is nothing left to block.
    fn is_pre(self) -> bool {
        matches!(self, Self::PreToolUse | Self::PreFileWrite | Self::PreBash)
    }
}

/// What one lifecycle event can tell a hook about the action it surrounds.
/// Every field is optional: `SessionStart`/`SessionEnd` set none of them,
/// `PreBash`/`PostBash` set `command` (and `PostBash` sets `exit_code`/
/// `output_excerpt`), and so on according to the event contract.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct HookContext<'a> {
    pub(crate) tool_name: Option<&'a str>,
    pub(crate) tool_args: Option<&'a serde_json::Value>,
    pub(crate) file_path: Option<&'a Path>,
    pub(crate) command: Option<&'a str>,
    pub(crate) exit_code: Option<i32>,
    /// Already redacted by the caller — hooks never see raw credentials.
    pub(crate) output_excerpt: Option<&'a str>,
    /// Deterministic, redacted, and visibly bounded proposed/committed diff.
    pub(crate) diff: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub(crate) enum HookDecision {
    Continue,
    /// Pre-events only.
    Block {
        reason: String,
    },
    /// `PreToolUse` only.
    ModifyArgs {
        args: serde_json::Value,
    },
    /// `SessionStart` / post-events: injected as untrusted data, never a
    /// system message.
    AddContext {
        text: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct HookOutcome {
    pub(crate) decision: HookDecision,
    pub(crate) warnings: Vec<String>,
}

impl HookOutcome {
    fn continue_with_no_warnings() -> Self {
        Self {
            decision: HookDecision::Continue,
            warnings: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct PreparedHook {
    def: HookDef,
    source: ConfigSource,
    tool_matcher: Option<Pattern>,
    path_matcher: Option<Pattern>,
}

fn matches(hook: &PreparedHook, ctx: &HookContext<'_>) -> bool {
    if let Some(pattern) = &hook.tool_matcher {
        match ctx.tool_name {
            Some(name) if pattern.matches(name) => {}
            _ => return false,
        }
    }
    if let Some(pattern) = &hook.path_matcher {
        match ctx.file_path.and_then(Path::to_str) {
            Some(path) if pattern.matches(path) => {}
            _ => return false,
        }
    }
    true
}

pub(crate) struct HookEngine {
    /// Mutable at runtime: a hook awaiting interactive trust approval is
    /// spliced in later by a background task (see [`Self::build`]), well
    /// after this list is first returned to the caller.
    hooks: Arc<RwLock<Vec<PreparedHook>>>,
    extensions: Arc<ExtensionRegistry>,
    /// One-shot provider for `llm_prompt` actions. `None`
    /// disables that action kind (evals/headless-without-catalog); such hooks
    /// fail with `on_failure` rather than panicking.
    llm: Option<Arc<dyn Provider>>,
    project_root: PathBuf,
    session_id: String,
    sandbox: CommandSandbox,
    authorization_ledger: AuthorizationLedger,
    yolo_mode: YoloMode,
}

/// Runtime services that define how hook actions are authorized and confined.
///
/// Keeping these values together prevents a hook engine from accidentally
/// mixing a sandbox from one runtime with another runtime's authorization
/// ledger or approval mode.
#[derive(Clone)]
pub(crate) struct HookExecutionPolicy {
    sandbox: CommandSandbox,
    authorization_ledger: AuthorizationLedger,
    yolo_mode: YoloMode,
}

impl HookExecutionPolicy {
    pub(crate) fn new(
        sandbox: CommandSandbox,
        authorization_ledger: AuthorizationLedger,
        yolo_mode: YoloMode,
    ) -> Self {
        Self {
            sandbox,
            authorization_ledger,
            yolo_mode,
        }
    }
}

impl fmt::Debug for HookExecutionPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HookExecutionPolicy")
            .field("sandbox", &self.sandbox)
            .field("authorization_ledger", &"configured")
            .field("yolo_mode", &self.yolo_mode)
            .finish()
    }
}

/// Cohesive construction context for a live hook engine.
#[derive(Clone)]
pub(crate) struct HookRuntimeDeps {
    project_root: PathBuf,
    permissions: PermissionManager,
    interaction: Arc<InteractionService>,
    extensions: Arc<ExtensionRegistry>,
    llm: Option<Arc<dyn Provider>>,
    execution_policy: HookExecutionPolicy,
}

impl HookRuntimeDeps {
    pub(crate) fn new(
        project_root: PathBuf,
        permissions: PermissionManager,
        interaction: Arc<InteractionService>,
        extensions: Arc<ExtensionRegistry>,
        execution_policy: HookExecutionPolicy,
    ) -> Self {
        Self {
            project_root,
            permissions,
            interaction,
            extensions,
            llm: None,
            execution_policy,
        }
    }

    pub(crate) fn with_llm(mut self, llm: Arc<dyn Provider>) -> Self {
        self.llm = Some(llm);
        self
    }
}

impl fmt::Debug for HookRuntimeDeps {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HookRuntimeDeps")
            .field("project_root", &self.project_root)
            .field("permissions", &"configured")
            .field("interaction", &"configured")
            .field("extensions", &"configured")
            .field("llm", &self.llm.is_some())
            .field("execution_policy", &self.execution_policy)
            .finish()
    }
}

impl HookEngine {
    /// No-op engine: evals and any surface that never loads config. `fire`
    /// always returns `Continue` with no warnings and no hook ever runs.
    pub(crate) fn disabled() -> Self {
        Self {
            hooks: Arc::new(RwLock::new(Vec::new())),
            extensions: Arc::new(ExtensionRegistry::new()),
            llm: None,
            project_root: PathBuf::new(),
            session_id: String::new(),
            sandbox: CommandSandbox::disabled(),
            authorization_ledger: AuthorizationLedger::disabled(),
            yolo_mode: YoloMode::new(),
        }
    }

    /// Compile matchers and run the load-time trust gate for every configured
    /// hook. A hook that fails to compile or is disabled in config
    /// contributes an `extensions` status entry and is excluded from firing —
    /// never a startup failure.
    ///
    /// Trust that resolves without a prompt (`Global` source, an
    /// `llm_prompt` action, or an already-persisted allow/deny rule) is
    /// awaited inline. Trust that *would* need to prompt is deliberately
    /// **not** awaited here: nothing drains `interaction`'s channel until the
    /// caller's event loop is live, which is after this function returns —
    /// awaiting inline would deadlock startup waiting for a modal nothing can
    /// show yet. That case is resolved in a background task instead, which
    /// splices the hook into the live, `RwLock`-guarded list once the user
    /// answers from inside the now-running session.
    #[cfg(test)]
    pub(crate) async fn build(
        config: &Config,
        project_root: PathBuf,
        permissions: PermissionManager,
        interaction: Arc<InteractionService>,
        extensions: Arc<ExtensionRegistry>,
        llm: Option<Arc<dyn Provider>>,
    ) -> Self {
        Self::build_with_sandbox(
            config,
            project_root,
            permissions,
            interaction,
            extensions,
            llm,
            CommandSandbox::disabled(),
        )
        .await
    }

    /// Test convenience for constructing an engine whose shell actions use
    /// the supplied command sandbox. Production entry points provide a full
    /// [`HookRuntimeDeps`] so sandbox and authorization state stay coupled.
    #[cfg(test)]
    pub(crate) async fn build_with_sandbox(
        config: &Config,
        project_root: PathBuf,
        permissions: PermissionManager,
        interaction: Arc<InteractionService>,
        extensions: Arc<ExtensionRegistry>,
        llm: Option<Arc<dyn Provider>>,
        sandbox: CommandSandbox,
    ) -> Self {
        let deps = HookRuntimeDeps::new(
            project_root,
            permissions,
            interaction,
            extensions,
            HookExecutionPolicy::new(sandbox, AuthorizationLedger::disabled(), YoloMode::new()),
        );
        let deps = match llm {
            Some(llm) => deps.with_llm(llm),
            None => deps,
        };
        Self::build_with_runtime(config, deps).await
    }

    pub(crate) async fn build_with_runtime(config: &Config, deps: HookRuntimeDeps) -> Self {
        let HookRuntimeDeps {
            project_root,
            permissions,
            interaction,
            extensions,
            llm,
            execution_policy:
                HookExecutionPolicy {
                    sandbox,
                    authorization_ledger,
                    yolo_mode,
                },
        } = deps;
        let hooks: Arc<RwLock<Vec<PreparedHook>>> = Arc::new(RwLock::new(Vec::new()));
        for (def, source) in &config.hooks {
            let extension_id = ExtensionId::Hook(def.name.clone());
            if !def.enabled {
                extensions.upsert(ExtensionStatus {
                    id: extension_id,
                    source: *source,
                    capabilities: crate::config::DeclaredCapabilities::default(),
                    state: ExtensionState::Disabled {
                        reason: DisableReason::Config,
                    },
                    detail: format!(
                        "{} · {}",
                        def.event.wire_name(),
                        action_kind_label(&def.action)
                    ),
                    tools: Vec::new(),
                });
                continue;
            }

            let tool_matcher = match def.matcher.tool.as_deref().map(Pattern::new) {
                Some(Ok(pattern)) => Some(pattern),
                Some(Err(err)) => {
                    extensions.upsert(failed_status(
                        extension_id,
                        *source,
                        def,
                        format!("invalid tool matcher: {err}"),
                    ));
                    continue;
                }
                None => None,
            };
            let path_matcher = match def.matcher.path.as_deref().map(Pattern::new) {
                Some(Ok(pattern)) => Some(pattern),
                Some(Err(err)) => {
                    extensions.upsert(failed_status(
                        extension_id,
                        *source,
                        def,
                        format!("invalid path matcher: {err}"),
                    ));
                    continue;
                }
                None => None,
            };

            let pattern = trust::trust_pattern(def);
            if trust::needs_interactive_prompt(def, *source, &pattern, &permissions) {
                extensions.upsert(ExtensionStatus {
                    id: extension_id.clone(),
                    source: *source,
                    capabilities: crate::config::DeclaredCapabilities::default(),
                    state: ExtensionState::Disabled {
                        reason: DisableReason::PermissionDenied,
                    },
                    detail: format!(
                        "{} · {} (awaiting trust approval)",
                        def.event.wire_name(),
                        action_kind_label(&def.action)
                    ),
                    tools: Vec::new(),
                });
                let hooks = hooks.clone();
                let extensions = extensions.clone();
                let permissions = permissions.clone();
                let interaction = interaction.clone();
                let def = def.clone();
                let source = *source;
                let pattern = pattern.clone();
                tokio::spawn(async move {
                    let detail = format!(
                        "{} · {}",
                        def.event.wire_name(),
                        action_kind_label(&def.action)
                    );
                    match trust::authorize(&def, source, &pattern, &permissions, &interaction).await
                    {
                        trust::TrustOutcome::Trusted => {
                            extensions.upsert(ExtensionStatus {
                                id: extension_id,
                                source,
                                capabilities: crate::config::DeclaredCapabilities::default(),
                                state: ExtensionState::Enabled,
                                detail,
                                tools: Vec::new(),
                            });
                            hooks
                                .write()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .push(PreparedHook {
                                    def,
                                    source,
                                    tool_matcher,
                                    path_matcher,
                                });
                        }
                        trust::TrustOutcome::Denied => {
                            extensions.upsert(ExtensionStatus {
                                id: extension_id,
                                source,
                                capabilities: crate::config::DeclaredCapabilities::default(),
                                state: ExtensionState::Disabled {
                                    reason: DisableReason::PermissionDenied,
                                },
                                detail,
                                tools: Vec::new(),
                            });
                        }
                    }
                });
                continue;
            }

            match trust::authorize(def, *source, &pattern, &permissions, &interaction).await {
                trust::TrustOutcome::Denied => {
                    extensions.upsert(ExtensionStatus {
                        id: extension_id,
                        source: *source,
                        capabilities: crate::config::DeclaredCapabilities::default(),
                        state: ExtensionState::Disabled {
                            reason: DisableReason::PermissionDenied,
                        },
                        detail: format!(
                            "{} · {}",
                            def.event.wire_name(),
                            action_kind_label(&def.action)
                        ),
                        tools: Vec::new(),
                    });
                    continue;
                }
                trust::TrustOutcome::Trusted => {}
            }

            extensions.upsert(ExtensionStatus {
                id: extension_id,
                source: *source,
                capabilities: crate::config::DeclaredCapabilities::default(),
                state: ExtensionState::Enabled,
                detail: format!(
                    "{} · {}",
                    def.event.wire_name(),
                    action_kind_label(&def.action)
                ),
                tools: Vec::new(),
            });
            hooks
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(PreparedHook {
                    def: def.clone(),
                    source: *source,
                    tool_matcher,
                    path_matcher,
                });
        }

        Self {
            hooks,
            extensions,
            llm,
            project_root,
            session_id: generate_session_id(),
            sandbox,
            authorization_ledger,
            yolo_mode,
        }
    }

    /// Fire every enabled hook matching `event`'s tool/path matcher, in
    /// config order (global then project — the order `config.hooks` is
    /// already merged in). Blocking pre-event hooks run sequentially so
    /// "first `Block` wins, else last `ModifyArgs`" is well-defined; every
    /// other matching hook (non-blocking pre-hooks and all post-hooks, which
    /// never veto) runs concurrently and can only contribute `AddContext`.
    pub(crate) async fn fire(&self, event: HookEvent, ctx: HookContext<'_>) -> HookOutcome {
        // Cloned out from behind the lock before any `.await` below — a
        // background trust-approval task (`Self::build_with_runtime`) may need to take
        // the write lock at any time, and a `std::sync::RwLock` must never
        // be held across an await point.
        let matching: Vec<PreparedHook> = {
            let hooks = self
                .hooks
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            hooks
                .iter()
                .filter(|hook| {
                    hook.def.event == event
                        && matches(hook, &ctx)
                        && self
                            .extensions
                            .is_enabled(&ExtensionId::Hook(hook.def.name.clone()))
                })
                .cloned()
                .collect()
        };
        if matching.is_empty() {
            return HookOutcome::continue_with_no_warnings();
        }

        let cwd = self.project_root.to_string_lossy().to_string();
        let mut warnings = Vec::new();
        let mut block = None;
        let mut modify = None;
        let mut context_parts = Vec::new();

        let (vetoing, side_effect): (Vec<PreparedHook>, Vec<PreparedHook>) = matching
            .into_iter()
            .partition(|hook| event.is_pre() && hook.def.blocking);

        for hook in &vetoing {
            let (decision, warning) = self.run_and_interpret(hook, event, &cwd, &ctx).await;
            if let Some(warning) = warning {
                warnings.push(warning);
            }
            match decision {
                HookDecision::Block { reason } => {
                    block = Some(reason);
                    break;
                }
                HookDecision::ModifyArgs { args } => modify = Some(args),
                HookDecision::AddContext { text } => context_parts.push(text),
                HookDecision::Continue => {}
            }
        }

        let side_effect_results = futures::future::join_all(
            side_effect
                .iter()
                .map(|hook| self.run_and_interpret(hook, event, &cwd, &ctx)),
        )
        .await;
        for (decision, warning) in side_effect_results {
            if let Some(warning) = warning {
                warnings.push(warning);
            }
            if let HookDecision::AddContext { text } = decision {
                context_parts.push(text);
            }
        }

        let decision = if let Some(reason) = block {
            HookDecision::Block { reason }
        } else if let Some(args) = modify {
            HookDecision::ModifyArgs { args }
        } else if !context_parts.is_empty() {
            HookDecision::AddContext {
                text: context_parts.join("\n\n"),
            }
        } else {
            HookDecision::Continue
        };

        HookOutcome { decision, warnings }
    }

    async fn run_and_interpret(
        &self,
        hook: &PreparedHook,
        event: HookEvent,
        cwd: &str,
        ctx: &HookContext<'_>,
    ) -> (HookDecision, Option<String>) {
        let can_veto = event.is_pre() && hook.def.blocking;
        let input = protocol::HookInput::new(event.wire_name(), &self.session_id, cwd, ctx);
        let timeout = std::time::Duration::from_secs(hook.def.timeout_secs.max(1));
        let plan = ActionPlan::new(
            "hook",
            format!("hook.{}: {}", hook.def.name, event.wire_name()),
            hook_effects(&hook.def.action),
        );
        let sandbox_blocks_http = matches!(&hook.def.action, HookAction::Http { .. })
            && self.sandbox.is_active()
            && self.sandbox.deny_network();
        self.authorization_ledger
            .record(
                &plan,
                PermissionMatch {
                    permission: Permission::Allow,
                    source: match hook.source {
                        ConfigSource::Global => PermissionMatchSource::Global,
                        ConfigSource::Project => PermissionMatchSource::Project,
                        ConfigSource::Env => PermissionMatchSource::EnvConfig,
                    },
                },
                self.yolo_mode.level(),
                if sandbox_blocks_http { "deny" } else { "allow" },
                if sandbox_blocks_http {
                    "sandbox network denied"
                } else {
                    "trusted hook definition"
                },
            )
            .await;
        let raw = if sandbox_blocks_http {
            actions::RawOutcome::Failed {
                error: "HTTP hook blocked because the active command sandbox denies network; use `/sandbox net off` to allow it."
                    .to_string(),
            }
        } else {
            match &hook.def.action {
                HookAction::Shell { command } => {
                    actions::run_shell(command, &self.project_root, &self.sandbox, &input, timeout)
                        .await
                }
                HookAction::Http { url, headers } => {
                    actions::run_http(url, headers, &self.sandbox, &input, timeout).await
                }
                HookAction::LlmPrompt { prompt } => match &self.llm {
                    Some(provider) => {
                        actions::run_llm_prompt(provider.as_ref(), prompt, ctx, timeout).await
                    }
                    None => actions::RawOutcome::Failed {
                        error: "no provider configured for llm_prompt hooks".to_string(),
                    },
                },
            }
        };

        match raw {
            actions::RawOutcome::Continue => (HookDecision::Continue, None),
            actions::RawOutcome::Block { reason } => {
                if can_veto {
                    (HookDecision::Block { reason }, None)
                } else {
                    (
                        HookDecision::Continue,
                        Some(format!(
                            "hook '{}' requested a block but cannot veto {} (blocking={}); ignored",
                            hook.def.name,
                            event.wire_name(),
                            hook.def.blocking
                        )),
                    )
                }
            }
            actions::RawOutcome::Modify { args } => {
                if can_veto && matches!(event, HookEvent::PreToolUse) {
                    (HookDecision::ModifyArgs { args }, None)
                } else {
                    (
                        HookDecision::Continue,
                        Some(format!(
                            "hook '{}' requested a tool_args rewrite outside a blocking PreToolUse; ignored",
                            hook.def.name
                        )),
                    )
                }
            }
            actions::RawOutcome::Context { text } => (HookDecision::AddContext { text }, None),
            actions::RawOutcome::Failed { error } => {
                // Deliberately does NOT call `extensions.set_state(Failed)`
                // here: `is_enabled()` in `fire`'s matching filter requires
                // the state to be exactly `Enabled`, so marking a hook
                // `Failed` after one runtime failure would permanently
                // exclude it from firing again for the rest of the session —
                // silently turning a fail-closed (`on_failure: block`) hook
                // fail-open after its first transient error. A per-call
                // failure is surfaced via the warning below only; `Failed` is
                // reserved for load-time problems (`Self::build`).
                let warning = format!("hook '{}' failed: {error}", hook.def.name);
                match hook.def.on_failure {
                    FailureBehavior::Warn => (HookDecision::Continue, Some(warning)),
                    FailureBehavior::Block if can_veto => (
                        HookDecision::Block {
                            reason: format!("hook '{}' failed", hook.def.name),
                        },
                        Some(warning),
                    ),
                    FailureBehavior::Block => (HookDecision::Continue, Some(warning)),
                }
            }
        }
    }

    /// Literal tool names matched by any blocking `PreToolUse` hook, for the
    /// batcher's serialization override (`src/agent/batching.rs`). Only
    /// literal matchers contribute: a glob like `mcp.github.*` can't be
    /// expanded into concrete names without the tool registry, which the
    /// engine doesn't hold — a hook that needs to serialize a whole namespace
    /// of tools should list them individually today.
    pub(crate) fn serialized_tool_names(&self) -> HashSet<String> {
        self.hooks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|hook| hook.def.event == HookEvent::PreToolUse && hook.def.blocking)
            .filter_map(|hook| hook.def.matcher.tool.clone())
            .filter(|pattern| !pattern.contains(['*', '?', '[']))
            .collect()
    }

    /// Names of hooks that loaded successfully (compiled matchers, passed
    /// trust), for `/hooks`'s usage errors — mirrors `McpHub::server_names`.
    pub(crate) fn hook_names(&self) -> Vec<String> {
        self.hooks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|hook| hook.def.name.clone())
            .collect()
    }

    /// Session-only enable/disable (`/hooks enable|disable <name>`). Flips
    /// the shared `ExtensionRegistry` state; `fire` checks it on every call,
    /// so a disabled hook stops matching immediately. Persisting the toggle
    /// means editing `enabled = false` in config — `/hooks` prints that hint,
    /// this only flips the in-memory state. Returns `false` for an unknown
    /// (never-loaded, or still awaiting trust approval) hook name.
    pub(crate) fn set_enabled(&self, name: &str, enabled: bool) -> bool {
        let known = self
            .hooks
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|hook| hook.def.name == name);
        if !known {
            return false;
        }
        let state = if enabled {
            ExtensionState::Enabled
        } else {
            ExtensionState::Disabled {
                reason: DisableReason::Session,
            }
        };
        self.extensions
            .set_state(&ExtensionId::Hook(name.to_string()), state);
        true
    }

    /// Fire `name` once with a synthetic payload (`/hooks test <name>`),
    /// bypassing the event/matcher filter in [`Self::fire`] — this is an
    /// on-demand smoke test, not a real lifecycle event. `None` for an
    /// unknown hook name.
    pub(crate) async fn test_fire(&self, name: &str) -> Option<HookOutcome> {
        // Cloned out from behind the lock before the `.await` below, same as
        // `fire` — a `std::sync::RwLock` guard must not live across an await.
        let hook = {
            let hooks = self
                .hooks
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            hooks.iter().find(|hook| hook.def.name == name).cloned()
        }?;
        let cwd = self.project_root.to_string_lossy().to_string();
        let synthetic = HookContext {
            tool_name: Some("test"),
            command: Some("echo test"),
            output_excerpt: Some("test"),
            exit_code: Some(0),
            ..Default::default()
        };
        let (decision, warning) = self
            .run_and_interpret(&hook, hook.def.event, &cwd, &synthetic)
            .await;
        Some(HookOutcome {
            decision,
            warnings: warning.into_iter().collect(),
        })
    }
}

fn hook_effects(action: &HookAction) -> Vec<ActionEffect> {
    match action {
        HookAction::Shell { .. } => vec![ActionEffect::CodeExecution],
        HookAction::Http { .. } => vec![ActionEffect::Network],
        HookAction::LlmPrompt { .. } => vec![ActionEffect::Network],
    }
}

/// Cap a `PostToolUse`/`PostBash` hook's `output_excerpt` so a hook payload
/// (stdin JSON, an HTTP POST body) never carries a full multi-megabyte tool
/// result.
const HOOK_OUTPUT_EXCERPT_CHARS: usize = 4000;

pub(crate) fn truncate_excerpt(text: &str) -> String {
    if text.chars().count() <= HOOK_OUTPUT_EXCERPT_CHARS {
        return text.to_string();
    }
    let truncated: String = text.chars().take(HOOK_OUTPUT_EXCERPT_CHARS).collect();
    format!("{truncated}\n… [truncated]")
}

fn action_kind_label(action: &HookAction) -> &'static str {
    match action {
        HookAction::Shell { .. } => "shell",
        HookAction::Http { .. } => "http",
        HookAction::LlmPrompt { .. } => "llm_prompt",
    }
}

fn failed_status(
    id: ExtensionId,
    source: ConfigSource,
    def: &HookDef,
    error: String,
) -> ExtensionStatus {
    ExtensionStatus {
        id,
        source,
        capabilities: crate::config::DeclaredCapabilities::default(),
        state: ExtensionState::Failed { error },
        detail: format!(
            "{} · {}",
            def.event.wire_name(),
            action_kind_label(&def.action)
        ),
        tools: Vec::new(),
    }
}

/// A per-engine-instance correlation id for `HookInput.session_id`/
/// `BONSAI_SESSION_ID`. Not the persisted storage session id (unavailable
/// this early in headless/eval, and not worth threading through every call
/// site just for a log-correlation field) — just stable for the lifetime of
/// one `HookEngine`.
fn generate_session_id() -> String {
    let seed = format!("{}-{:?}", std::process::id(), std::time::SystemTime::now());
    blake3::hash(seed.as_bytes()).to_hex()[..16].to_string()
}
