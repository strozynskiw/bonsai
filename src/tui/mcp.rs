use crate::config::{BatchingPolicy, ConfigSource};
use crate::extension::ExtensionId;
use crate::extension::capabilities::capability_label;
use crate::extension::status::{DisableReason, DiscoveredTool, ExtensionRegistry, ExtensionState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpServerStateKind {
    Enabled,
    Disabled,
    Failed,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpToolRow {
    pub(crate) name: String,
    pub(crate) description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpServerRow {
    pub(crate) name: String,
    pub(crate) state: McpServerStateKind,
    pub(crate) state_label: String,
    pub(crate) state_detail: Option<String>,
    pub(crate) source: String,
    pub(crate) capabilities: Vec<String>,
    pub(crate) batching: String,
    pub(crate) risk: String,
    pub(crate) detail: String,
    pub(crate) tools: Vec<McpToolRow>,
}

pub(crate) fn mcp_server_rows(extensions: &ExtensionRegistry) -> Vec<McpServerRow> {
    extensions
        .snapshot()
        .into_iter()
        .filter_map(|status| {
            let ExtensionId::McpServer(name) = status.id else {
                return None;
            };
            let (state, state_label, state_detail) = state_fields(&status.state);
            let capabilities = if status.capabilities.capabilities.is_empty() {
                vec!["undeclared".to_string()]
            } else {
                status
                    .capabilities
                    .capabilities
                    .iter()
                    .map(|capability| capability_label(*capability).to_string())
                    .collect()
            };
            let batching = batching_label(status.capabilities.batching).to_string();
            let risk = status.capabilities.risk_tier().label().to_string();
            Some(McpServerRow {
                name,
                state,
                state_label,
                state_detail,
                source: source_label(status.source).to_string(),
                capabilities,
                batching,
                risk,
                detail: status.detail,
                tools: status.tools.iter().map(tool_row).collect(),
            })
        })
        .collect()
}

fn tool_row(tool: &DiscoveredTool) -> McpToolRow {
    McpToolRow {
        name: tool.name.clone(),
        description: tool.description.clone(),
    }
}

fn state_fields(state: &ExtensionState) -> (McpServerStateKind, String, Option<String>) {
    match state {
        ExtensionState::Enabled => (McpServerStateKind::Enabled, "enabled".to_string(), None),
        ExtensionState::Disabled { reason } => (
            McpServerStateKind::Disabled,
            "disabled".to_string(),
            Some(disable_reason_label(reason).to_string()),
        ),
        ExtensionState::Failed { error } => (
            McpServerStateKind::Failed,
            "failed".to_string(),
            Some(error.clone()),
        ),
        ExtensionState::Degraded { warning } => (
            McpServerStateKind::Degraded,
            "degraded".to_string(),
            Some(warning.clone()),
        ),
    }
}

fn disable_reason_label(reason: &DisableReason) -> &'static str {
    match reason {
        DisableReason::Config => "config",
        DisableReason::Session => "session",
        DisableReason::PermissionDenied => "permission denied",
    }
}

fn batching_label(policy: BatchingPolicy) -> &'static str {
    match policy {
        BatchingPolicy::Serialized => "serialized",
        BatchingPolicy::PathScoped => "path scoped",
    }
}

fn source_label(source: ConfigSource) -> &'static str {
    source.label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Capability, DeclaredCapabilities};
    use crate::extension::status::ExtensionStatus;

    #[test]
    fn server_rows_include_tools_and_policy_metadata() {
        let extensions = ExtensionRegistry::new();
        extensions.upsert(ExtensionStatus {
            id: ExtensionId::Hook("format".to_string()),
            source: ConfigSource::Project,
            capabilities: DeclaredCapabilities::default(),
            state: ExtensionState::Enabled,
            detail: String::new(),
            tools: Vec::new(),
        });
        extensions.upsert(ExtensionStatus {
            id: ExtensionId::McpServer("demo".to_string()),
            source: ConfigSource::Project,
            capabilities: DeclaredCapabilities {
                capabilities: vec![Capability::Read, Capability::Write],
                batching: BatchingPolicy::PathScoped,
            },
            state: ExtensionState::Degraded {
                warning: "one tool skipped".to_string(),
            },
            detail: "1 tool(s)".to_string(),
            tools: vec![DiscoveredTool {
                name: "read_note".to_string(),
                description: "Read the demo note".to_string(),
            }],
        });

        let rows = mcp_server_rows(&extensions);

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.name, "demo");
        assert_eq!(row.state, McpServerStateKind::Degraded);
        assert_eq!(row.state_label, "degraded");
        assert_eq!(row.state_detail.as_deref(), Some("one tool skipped"));
        assert_eq!(
            row.capabilities,
            vec!["read".to_string(), "write".to_string()]
        );
        assert_eq!(row.batching, "path scoped");
        assert_eq!(row.risk, "high");
        assert_eq!(row.tools[0].name, "read_note");
    }
}
