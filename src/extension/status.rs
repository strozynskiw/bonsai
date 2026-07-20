//! Shared, runtime-mutable status store for every configured extension. The
//! `/mcp` and `/hooks` surfaces render these entries directly. Session toggles
//! and connection failures update them live; bootstrap seeds config state.

use std::collections::BTreeMap;
use std::sync::RwLock;

use crate::config::{ConfigSource, DeclaredCapabilities};

use super::ExtensionId;

/// Why an extension is `Disabled`. Distinct from `Failed`/`Degraded`: a
/// disabled extension was never attempted (or was explicitly turned off),
/// while `Failed`/`Degraded` describe an attempt that didn't fully succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DisableReason {
    /// `enabled = false` in config. An MCP server filters this case out
    /// before a status entry exists at all (`McpHub::connect_and_discover`),
    /// so it never appears in `/mcp` rather than appearing as disabled; a
    /// hook keeps its status entry instead, so `/hooks` can show it.
    Config,
    /// Turned off for this session via a `/mcp`/`/hooks` toggle.
    Session,
    /// The user denied the one-time trust prompt for a project-config hook.
    PermissionDenied,
}

/// One extension's current lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExtensionState {
    Enabled,
    Disabled {
        reason: DisableReason,
    },
    /// Load/spawn/handshake/validation failed — visible, never fatal to the
    /// agent loop.
    Failed {
        error: String,
    },
    /// Working, with a caveat (e.g. a tool skipped on a wire-name collision).
    Degraded {
        warning: String,
    },
}

impl ExtensionState {
    /// A one-line human label for `/mcp`/`/hooks` listings.
    pub(crate) fn label(&self) -> String {
        match self {
            ExtensionState::Enabled => "enabled".to_string(),
            ExtensionState::Disabled { reason } => format!("disabled ({reason:?})"),
            ExtensionState::Failed { error } => format!("failed: {error}"),
            ExtensionState::Degraded { warning } => format!("degraded: {warning}"),
        }
    }
}

/// One discovered MCP tool, retained so `/mcp tools <server>` can list the
/// callable surface after bootstrap discards the rmcp tool list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredTool {
    /// Remote (un-namespaced) tool name, e.g. `create_issue`.
    pub name: String,
    /// First line of the remote description; empty when the server omits one.
    pub description: String,
}

/// A display-ready snapshot of one extension for `/mcp`/`/hooks`/`/config`.
#[derive(Debug, Clone)]
pub(crate) struct ExtensionStatus {
    pub id: ExtensionId,
    pub source: ConfigSource,
    pub capabilities: DeclaredCapabilities,
    pub state: ExtensionState,
    /// Free-form detail line, e.g. `"14 tools"`, `"PostFileWrite · shell"`.
    pub detail: String,
    /// Discovered tool inventory — MCP servers only; always empty for hooks.
    pub tools: Vec<DiscoveredTool>,
}

/// Shared status store, cheap to clone-share via `Arc` and safe to update
/// from any task (a connection failure, a session toggle) without a mutable
/// borrow of the owner.
#[derive(Default)]
pub(crate) struct ExtensionRegistry {
    entries: RwLock<BTreeMap<ExtensionId, ExtensionStatus>>,
}

impl ExtensionRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Insert or replace an extension's status entry.
    pub(crate) fn upsert(&self, status: ExtensionStatus) {
        let Ok(mut entries) = self.entries.write() else {
            return;
        };
        entries.insert(status.id.clone(), status);
    }

    /// Update just the state of an already-registered extension. A no-op if
    /// `id` was never registered.
    pub(crate) fn set_state(&self, id: &ExtensionId, state: ExtensionState) {
        let Ok(mut entries) = self.entries.write() else {
            return;
        };
        if let Some(status) = entries.get_mut(id) {
            status.state = state;
        }
    }

    /// Drop one tool from an extension's discovered inventory — used when a
    /// wire-name collision skips that tool at registration, so `/mcp tools`
    /// never lists a tool that isn't actually callable. A no-op if the entry
    /// or the named tool is absent.
    pub(crate) fn remove_discovered_tool(&self, id: &ExtensionId, remote_name: &str) {
        let Ok(mut entries) = self.entries.write() else {
            return;
        };
        if let Some(status) = entries.get_mut(id) {
            status.tools.retain(|tool| tool.name != remote_name);
        }
    }

    /// All entries, in `ExtensionId` order — byte-stable for `/mcp`/`/hooks`
    /// listing order across runs.
    pub(crate) fn snapshot(&self) -> Vec<ExtensionStatus> {
        self.entries
            .read()
            .map(|entries| entries.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether `id` is registered and currently `Enabled`. `false` for an
    /// unknown id, matching the fail-closed posture the gate relies on.
    pub(crate) fn is_enabled(&self, id: &ExtensionId) -> bool {
        self.entries
            .read()
            .ok()
            .and_then(|entries| {
                entries
                    .get(id)
                    .map(|status| status.state == ExtensionState::Enabled)
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(id: ExtensionId, state: ExtensionState) -> ExtensionStatus {
        ExtensionStatus {
            id,
            source: ConfigSource::Project,
            capabilities: DeclaredCapabilities::default(),
            state,
            detail: String::new(),
            tools: Vec::new(),
        }
    }

    #[test]
    fn upsert_and_snapshot_round_trip() {
        let registry = ExtensionRegistry::new();
        let id = ExtensionId::McpServer("github".to_string());
        registry.upsert(status(id.clone(), ExtensionState::Enabled));

        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, id);
        assert!(registry.is_enabled(&id));
    }

    #[test]
    fn set_state_updates_an_existing_entry_only() {
        let registry = ExtensionRegistry::new();
        let id = ExtensionId::McpServer("github".to_string());
        registry.upsert(status(id.clone(), ExtensionState::Enabled));

        registry.set_state(
            &id,
            ExtensionState::Degraded {
                warning: "tool name collision".to_string(),
            },
        );
        assert!(!registry.is_enabled(&id));

        // Setting the state of a never-registered id is a harmless no-op.
        let unknown = ExtensionId::McpServer("unknown".to_string());
        registry.set_state(&unknown, ExtensionState::Enabled);
        assert_eq!(registry.snapshot().len(), 1);
    }

    #[test]
    fn unknown_id_is_not_enabled() {
        let registry = ExtensionRegistry::new();
        assert!(!registry.is_enabled(&ExtensionId::McpServer("ghost".to_string())));
    }
}
