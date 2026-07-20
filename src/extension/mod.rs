//! Runtime boundaries shared by configured extensions: namespaced identity,
//! declared-capability risk policy, live status, and the permission gate used
//! by extension tools. Extension definitions originate in
//! `.bonsai/config.toml`; this module owns their runtime identity and policy.
//!
//! Dotted IDs (`mcp.github`, `hook.cargo-fmt`) are for humans — UI, `/config`,
//! `/mcp`, `/hooks`, permission-rule patterns. Wire names (`mcp__github__…`)
//! are for the provider: tool-name charsets are not portable (Anthropic
//! requires `^[a-zA-Z0-9_-]{1,128}$`; several OpenAI-compatible backends
//! reject dots).
//!
//! MCP servers and hooks are the currently supported extension families. A
//! new family must add its namespace only when its runtime, trust boundary,
//! and permission behavior are implemented together.

pub(crate) mod capabilities;
pub(crate) mod gate;
pub(crate) mod status;

#[cfg(test)]
mod tests;

/// Namespaced identity for one configured extension.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ExtensionId {
    McpServer(String),
    Hook(String),
}

/// Canonical dotted tool id for one MCP server's tool — UI, permission rules,
/// config allowlists.
pub(crate) fn mcp_tool_id(server: &str, tool: &str) -> String {
    format!("mcp.{server}.{tool}")
}

/// A provider-wire-safe registry name for one MCP server's tool
/// ([`crate::tool::Tool::name`]). Clamped to 64 chars with a stable hash
/// suffix on overflow, so a long server/tool name pair still yields a
/// deterministic, collision-resistant wire name within common provider
/// charset limits.
const MAX_WIRE_NAME_LEN: usize = 64;

pub(crate) fn mcp_wire_name(server: &str, tool: &str) -> String {
    let name = format!("mcp__{server}__{tool}");
    if name.len() <= MAX_WIRE_NAME_LEN {
        return name;
    }
    let hash = blake3::hash(name.as_bytes());
    let suffix = format!("_{}", &hash.to_hex()[..8]);
    let keep = MAX_WIRE_NAME_LEN.saturating_sub(suffix.len());
    format!("{}{suffix}", truncate_to_char_boundary(&name, keep))
}

pub(crate) fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// A dotted display id with an `mcp.` prefix (`mcp.<server>.<tool>`) resolved
/// to its wire name, for [`crate::tool::ToolRegistry::get`]'s generic alias
/// arm — so a model (or a permission rule) using the human-readable id still
/// dispatches. `None` for anything not shaped like a two-segment mcp tool id
/// (a bare `mcp.<server>` names a *server*, not a tool, and never dispatches).
pub(crate) fn dotted_alias_to_wire(name: &str) -> Option<String> {
    let rest = name.strip_prefix("mcp.")?;
    let (server, tool) = rest.split_once('.')?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some(mcp_wire_name(server, tool))
}

/// Register `tool` into `registry` under its wire name, unless that name is
/// already taken — by a builtin or an earlier extension. The generic
/// collision guard every extension-tool registration path shares: the
/// builtin (or first registrant) always wins, and a loser degrades visibly
/// rather than silently shadowing or panicking.
pub(crate) fn register_or_degrade(
    registry: &mut crate::tool::ToolRegistry,
    tool: std::sync::Arc<dyn crate::tool::Tool>,
) -> Result<(), String> {
    let wire_name = tool.name().to_string();
    if registry.get(&wire_name).is_some() {
        return Err(format!(
            "wire name '{wire_name}' is already registered; this tool was skipped"
        ));
    }
    registry.register(tool);
    Ok(())
}
