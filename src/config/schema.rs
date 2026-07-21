//! Serde types for one config *file* — a single layer (global or project)
//! before merging. Every field is optional/defaulted: a file states only what
//! it overrides. See [`super`] for the layered, merged view.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

pub(crate) fn default_true() -> bool {
    true
}

fn default_mcp_timeout_secs() -> u64 {
    30
}

fn default_hook_timeout_secs() -> u64 {
    30
}

/// One parsed config file. `bonsai` never deserializes this directly with
/// serde end-to-end — [`super::parse_config_file`] walks the raw TOML table so
/// one bad entry (an unknown MCP transport, a malformed hook) can be skipped
/// with a diagnostic instead of failing the whole file — but the shape here is
/// the authoritative schema each section is deserialized against.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct ConfigFile {
    #[serde(default)]
    pub sandbox: SandboxSection,
    #[serde(default)]
    pub mcp: McpSection,
    #[serde(default)]
    pub hooks: Vec<HookDef>,
    #[serde(default)]
    pub verification: VerificationSection,
    #[serde(default)]
    pub update: UpdateSection,
}

/// `[update]` — native self-update policy. `mode` stays `Option` so the merge
/// can tell an explicit project-layer setting from an omitted one; `pin` is
/// kept as the raw string here and validated as semver at parse time.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct UpdateSection {
    #[serde(default)]
    pub mode: Option<UpdateMode>,
    #[serde(default)]
    pub pin: Option<String>,
}

/// How far the startup self-update may go. `Auto` (the default) silently
/// stages verified releases; `Notify` only surfaces that one exists; `Off`
/// disables the startup check entirely (`bonsai update` still works).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UpdateMode {
    #[default]
    Auto,
    Notify,
    Off,
}

/// `[verification]` — explicit ordered overrides for detected project checks.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct VerificationSection {
    #[serde(default)]
    pub test: Option<Vec<String>>,
    #[serde(default)]
    pub build: Option<Vec<String>>,
    #[serde(default)]
    pub after_edit: Option<crate::verification::VerifyAfterEdit>,
}

/// `[sandbox]` — absorbs the `BONSAI_SANDBOX_WRITABLE_ROOTS` /
/// `BONSAI_SANDBOX_NETWORK` env-only stand-ins (`src/sandbox/policy.rs`) into
/// config, without removing the env vars: env still wins per the precedence
/// table.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct SandboxSection {
    #[serde(default)]
    pub writable_roots: Vec<PathBuf>,
    #[serde(default)]
    pub deny_network: Option<bool>,
}

/// `[mcp]` — currently just the server table; `servers` is populated entry by
/// entry by the resilient parser, never via a single derive on this struct.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct McpSection {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// One `[mcp.servers.<name>]` entry.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct McpServerConfig {
    #[serde(flatten)]
    pub transport: McpTransportConfig,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tool allowlist (remote names). Omitted/empty ⇒ all discovered tools.
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(flatten)]
    pub declared: DeclaredCapabilities,
    #[serde(default = "default_mcp_timeout_secs")]
    pub timeout_secs: u64,
}

/// How a server is reached. `${VAR}` references in `env`/`headers` are left
/// unexpanded here — expansion happens where the value is actually used (the
/// MCP client), not at config-load time.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub(crate) enum McpTransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        /// Optional pre-registered public client id or HTTPS Client ID
        /// Metadata Document URL for authorization servers without dynamic
        /// client registration.
        #[serde(default)]
        oauth_client_id: Option<String>,
        /// Explicit OAuth scopes. Empty lets protected-resource and
        /// authorization-server metadata select the advertised scopes.
        #[serde(default)]
        oauth_scopes: Vec<String>,
    },
}

/// Capabilities the *user* declares for an extension — never self-declared by
/// a remote MCP server. `risk_tier()`/`parallel_policy()` (the policy that
/// turns this data into a permission posture) land with the extension runtime
///; this is deliberately data-only.
#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct DeclaredCapabilities {
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub batching: BatchingPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Capability {
    Read,
    Write,
    Network,
    Shell,
    /// May delete, publish, rewrite history, or cause another irreversible
    /// effect. This maps to the autonomy ladder's always-prompt floor.
    Irreversible,
    UntrustedOutput,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BatchingPolicy {
    #[default]
    Serialized,
    PathScoped,
}

/// One `[[hooks]]` entry — the config shape only; the execution engine
/// (shell/http/llm_prompt runners, the lifecycle events) lives in
/// `crate::hooks`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HookDef {
    pub name: String,
    pub event: HookEvent,
    #[serde(default)]
    pub matcher: HookMatcher,
    pub action: HookAction,
    #[serde(default = "default_hook_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub blocking: bool,
    #[serde(default)]
    pub on_failure: FailureBehavior,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum HookEvent {
    SessionStart,
    SessionEnd,
    PreToolUse,
    PostToolUse,
    PreFileWrite,
    PostFileWrite,
    PreBash,
    PostBash,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct HookMatcher {
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum HookAction {
    Shell {
        command: String,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
    LlmPrompt {
        prompt: String,
    },
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureBehavior {
    #[default]
    Warn,
    Block,
}
