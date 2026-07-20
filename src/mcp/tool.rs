//! `McpTool` — the extension tool adapter. Every MCP tool call flows through
//! the generic [`ExtensionGate`] here, exactly once, and every result (success
//! or `is_error`) comes back framed [`ToolOutput::untrusted_context`]: a
//! remote MCP server is third-party code, and its output is data, not
//! instructions (the prompt-injection gate).

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use rmcp::model::{CallToolResult, ContentBlock, Tool as McpToolDefinition};

use crate::config::DeclaredCapabilities;
use crate::extension::gate::ExtensionGate;
use crate::extension::{mcp_tool_id, mcp_wire_name};
use crate::tool::{ParallelPolicy, Tool, ToolOutput};

use super::McpServerHandle;

pub(crate) struct McpTool {
    wire_name: String,
    display_id: String,
    server: String,
    remote_name: String,
    description: String,
    schema: serde_json::Value,
    declared: DeclaredCapabilities,
    /// Stable permission key including the declared capabilities and schema,
    /// so an approval cannot silently survive a later privilege/schema change.
    grant_key: String,
    handle: Arc<McpServerHandle>,
    gate: Arc<ExtensionGate>,
}

impl McpTool {
    /// The owning server's config name (not the dotted display id), for
    /// [`super::register_mcp_tools`]'s collision-degrade path.
    pub(crate) fn server_name(&self) -> &str {
        &self.server
    }

    /// The remote (un-namespaced) tool name, for pruning the discovered-tool
    /// inventory when a wire-name collision skips this tool.
    pub(crate) fn remote_name(&self) -> &str {
        &self.remote_name
    }

    pub(crate) fn from_discovered_tool(
        server: &str,
        remote_tool: &McpToolDefinition,
        handle: Arc<McpServerHandle>,
        gate: Arc<ExtensionGate>,
    ) -> Self {
        let remote_name = remote_tool.name.to_string();
        let wire_name = mcp_wire_name(server, &remote_name);
        let display_id = mcp_tool_id(server, &remote_name);
        let description = format!(
            "[mcp:{server}] {}",
            remote_tool.description.as_deref().unwrap_or_default()
        );
        let schema = serde_json::Value::Object((*remote_tool.input_schema).clone());
        let declared = handle.config.declared.clone();
        let grant_key = grant_key(&display_id, &declared, &schema);
        Self {
            wire_name,
            display_id,
            server: server.to_string(),
            remote_name,
            description,
            schema,
            declared,
            grant_key,
            handle,
            gate,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::SelfAuthorized
    }

    fn name(&self) -> &str {
        &self.wire_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        self.declared.parallel_policy()
    }

    fn hook_match_name(&self) -> &str {
        &self.display_id
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        if !self.handle.extensions.is_enabled(&self.handle.extension_id) {
            anyhow::bail!(
                "MCP server '{}' is disabled; re-enable it with /mcp enable {}",
                self.server,
                self.server
            );
        }
        self.gate
            .authorize(
                &self.display_id,
                &self.grant_key,
                &self.server,
                &self.declared,
                &args_preview(&args),
            )
            .await?;
        let connection = self.handle.connection().await;
        let Some(connection) = connection else {
            anyhow::bail!("MCP server '{}' is unavailable", self.server);
        };
        let result = connection
            .call_tool(
                &self.remote_name,
                args,
                std::time::Duration::from_secs(self.handle.config.timeout_secs),
            )
            .await?;
        let status = if result.is_error == Some(true) {
            crate::output::ToolExecutionStatus::Failed
        } else {
            crate::output::ToolExecutionStatus::Succeeded
        };
        Ok(ToolOutput::untrusted_context_with_status(
            format!("mcp:{}:{}", self.server, self.remote_name),
            &render_call_result(&result),
            status,
        ))
    }
}

/// Bind a remembered grant to the stable tool id, its declared effects, and
/// the advertised input schema. A config or server schema change consequently
/// produces a new key and re-enters the ordinary permission path.
fn grant_key(
    display_id: &str,
    declared: &DeclaredCapabilities,
    schema: &serde_json::Value,
) -> String {
    let mut capabilities =
        crate::extension::capabilities::capability_labels(&declared.capabilities);
    capabilities.sort_unstable();
    let schema = serde_json::to_string(schema).unwrap_or_default();
    let identity = format!(
        "{display_id}\ncapabilities={}\nbatching={:?}\nschema={schema}",
        capabilities.join(","),
        declared.batching,
    );
    let hash = blake3::hash(identity.as_bytes());
    format!("{display_id}@{}", &hash.to_hex()[..16])
}

/// Join text content blocks; summarize non-text blocks. `is_error` results
/// are still server-authored text — framed the same way as success, never
/// promoted to a hard [`anyhow::Error`], so the untrusted frame always
/// applies (an `Err` here would surface raw, unframed text via the run
/// loop's generic "Error: {e}" path).
fn render_call_result(result: &CallToolResult) -> String {
    let body = if result.content.is_empty() {
        "(no content)".to_string()
    } else {
        result
            .content
            .iter()
            .map(render_content_block)
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    if result.is_error == Some(true) {
        format!("Error: {body}")
    } else {
        body
    }
}

fn render_content_block(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        // Image passthrough is a noted follow-up: ToolOutput::Image bypasses
        // the untrusted frame, so v1 deliberately drops non-text content.
        ContentBlock::Image(_) => "[image content omitted]".to_string(),
        ContentBlock::Audio(_) => "[audio content omitted]".to_string(),
        ContentBlock::Resource(_) => "[embedded resource omitted]".to_string(),
        ContentBlock::ResourceLink(_) => "[resource link omitted]".to_string(),
        // ContentBlock is #[non_exhaustive]; treat future variants like the
        // non-text ones above rather than failing the whole tool result.
        _ => "[unsupported content omitted]".to_string(),
    }
}

/// A short, pre-redacted first-line preview of a tool call's arguments, for
/// the permission prompt and status lines — never the full payload.
fn args_preview(args: &serde_json::Value) -> String {
    const MAX_PREVIEW_CHARS: usize = 200;
    let raw = serde_json::to_string(args).unwrap_or_default();
    let first_line = raw.lines().next().unwrap_or_default();
    let truncated: String = first_line.chars().take(MAX_PREVIEW_CHARS).collect();
    crate::redact::redact(&truncated).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BatchingPolicy, Capability};

    #[test]
    fn args_preview_redacts_and_truncates() {
        let key = format!("sk-ant-{}", "ABCdef0123".repeat(3));
        let preview = args_preview(&serde_json::json!({ "token": key }));
        assert!(!preview.contains(&key), "{preview}");
    }

    #[test]
    fn render_call_result_prefixes_errors_and_joins_text_blocks() {
        let ok = CallToolResult::success(vec![
            ContentBlock::text("hello"),
            ContentBlock::text("world"),
        ]);
        assert_eq!(render_call_result(&ok), "hello\n\nworld");

        let err = CallToolResult::error(vec![ContentBlock::text("boom")]);
        assert_eq!(render_call_result(&err), "Error: boom");
    }

    #[test]
    fn grant_key_changes_with_declared_effects_or_schema() {
        let read = DeclaredCapabilities {
            capabilities: vec![Capability::Read],
            batching: BatchingPolicy::Serialized,
        };
        let write = DeclaredCapabilities {
            capabilities: vec![Capability::Write],
            batching: BatchingPolicy::Serialized,
        };
        let schema = serde_json::json!({"type": "object", "properties": {}});
        let changed_schema = serde_json::json!({
            "type": "object",
            "properties": {"path": {"type": "string"}}
        });
        let base = grant_key("mcp.example.apply", &read, &schema);

        assert_ne!(base, grant_key("mcp.example.apply", &write, &schema));
        assert_ne!(base, grant_key("mcp.example.apply", &read, &changed_schema));
    }
}
