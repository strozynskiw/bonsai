//! Wire payloads for the shell and HTTP hook actions: one JSON object on
//! stdin/POST body, and a permissive response shape covering the three
//! optional-control fields a hook may return (`decision`+`reason`,
//! `decision`+`tool_args`, or `context`). Fields the caller didn't set are
//! simply absent rather than a parse error, so a hook that only wants to add
//! context doesn't have to echo back a null `decision`.

use serde::{Deserialize, Serialize};

use super::HookContext;

#[derive(Debug, Serialize)]
pub(super) struct HookInput<'a> {
    pub(super) event: &'static str,
    pub(super) session_id: &'a str,
    pub(super) cwd: &'a str,
    pub(super) tool_name: Option<&'a str>,
    pub(super) file_path: Option<&'a str>,
    pub(super) tool_args: Option<&'a serde_json::Value>,
    pub(super) command: Option<&'a str>,
    pub(super) exit_code: Option<i32>,
    pub(super) output_excerpt: Option<&'a str>,
    pub(super) diff: Option<&'a str>,
}

impl<'a> HookInput<'a> {
    pub(super) fn new(
        event_name: &'static str,
        session_id: &'a str,
        cwd: &'a str,
        ctx: &HookContext<'a>,
    ) -> Self {
        Self {
            event: event_name,
            session_id,
            cwd,
            tool_name: ctx.tool_name,
            file_path: ctx.file_path.and_then(|path| path.to_str()),
            tool_args: ctx.tool_args,
            command: ctx.command,
            exit_code: ctx.exit_code,
            output_excerpt: ctx.output_excerpt,
            diff: ctx.diff,
        }
    }
}

/// A hook's advanced-control response, parsed leniently: any subset of these
/// keys may be present. `decision: "block"` needs `reason`; `decision:
/// "modify"` needs `tool_args`; either can be combined with `context`. A
/// response missing every key degrades to [`super::HookDecision::Continue`].
/// Malformed non-empty JSON is returned to the engine as a failure so the
/// hook's configured `on_failure` policy decides whether to continue or veto.
#[derive(Debug, Default, Deserialize)]
pub(super) struct HookResponse {
    #[serde(default)]
    pub(super) decision: Option<String>,
    #[serde(default)]
    pub(super) reason: Option<String>,
    #[serde(default)]
    pub(super) tool_args: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) context: Option<String>,
}

impl HookResponse {
    /// Parse `body` as a [`HookResponse`]; `None` for empty/blank output (the
    /// common case — most hooks say nothing and rely on the exit code alone).
    pub(super) fn parse(body: &str) -> Option<Result<Self, serde_json::Error>> {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(serde_json::from_str(trimmed))
    }

    pub(super) fn into_decision(self) -> super::HookDecision {
        let Self {
            decision,
            reason,
            tool_args,
            context,
        } = self;
        match (decision.as_deref(), tool_args) {
            (Some("block"), _) => super::HookDecision::Block {
                reason: reason.unwrap_or_else(|| "blocked by hook".to_string()),
            },
            (Some("modify"), Some(args)) => super::HookDecision::ModifyArgs { args },
            _ => match context {
                Some(text) if !text.is_empty() => super::HookDecision::AddContext { text },
                _ => super::HookDecision::Continue,
            },
        }
    }
}

/// The LLM-prompt action's required reply shape — no optional fields, since an
/// LLM judge always renders a decision (unlike shell/HTTP, which may run
/// side-effect-only and return nothing).
#[derive(Debug, Deserialize)]
pub(super) struct LlmHookResponse {
    pub(super) decision: LlmDecision,
    #[serde(default)]
    pub(super) reason: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum LlmDecision {
    Allow,
    Block,
}
