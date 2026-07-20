use std::sync::Arc;

use async_trait::async_trait;

use super::*;
use crate::tool::{Tool, ToolOutput, ToolRegistry};

#[test]
fn mcp_tool_id_and_wire_name_are_dotted_and_double_underscored() {
    assert_eq!(
        mcp_tool_id("github", "create_issue"),
        "mcp.github.create_issue"
    );
    assert_eq!(
        mcp_wire_name("github", "create_issue"),
        "mcp__github__create_issue"
    );
}

#[test]
fn wire_name_clamps_deterministically_on_overflow() {
    let long_server = "a".repeat(40);
    let long_tool = "b".repeat(40);
    let wire = mcp_wire_name(&long_server, &long_tool);
    assert!(wire.len() <= MAX_WIRE_NAME_LEN, "{wire} ({})", wire.len());
    // Deterministic: the same inputs always clamp to the same wire name.
    assert_eq!(wire, mcp_wire_name(&long_server, &long_tool));
    // A different tool name with the same overflowing server must not collide.
    let other = mcp_wire_name(&long_server, &"c".repeat(40));
    assert_ne!(wire, other);
}

#[test]
fn dotted_alias_resolves_two_segment_mcp_ids_only() {
    assert_eq!(
        dotted_alias_to_wire("mcp.github.create_issue").as_deref(),
        Some("mcp__github__create_issue")
    );
    // A bare server id (one segment) names a server, not a tool.
    assert_eq!(dotted_alias_to_wire("mcp.github"), None);
    // Not an mcp id at all.
    assert_eq!(dotted_alias_to_wire("hook.cargo-fmt"), None);
    assert_eq!(dotted_alias_to_wire("read"), None);
}

#[test]
fn dotted_alias_dispatches_through_tool_registry_get() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FakeTool::new("mcp__github__create_issue")));

    let resolved = registry
        .get("mcp.github.create_issue")
        .expect("dotted alias should resolve");
    assert_eq!(resolved.name(), "mcp__github__create_issue");

    // Exact names still win, and an unrelated dotted id resolves to nothing.
    assert!(registry.get("mcp.github.close_issue").is_none());
}

#[test]
fn collision_marks_extension_degraded_and_keeps_builtin() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FakeTool::new("read")));

    let result = register_or_degrade(&mut registry, Arc::new(FakeTool::new("read")));

    assert!(result.is_err());
    // The builtin is untouched: still exactly one "read" tool, registration
    // order unchanged.
    assert_eq!(registry.names().collect::<Vec<_>>(), vec!["read"]);
}

#[test]
fn non_colliding_registration_succeeds_and_preserves_order() {
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FakeTool::new("read")));
    register_or_degrade(
        &mut registry,
        Arc::new(FakeTool::new("mcp__github__create_issue")),
    )
    .expect("no collision");

    assert_eq!(
        registry.names().collect::<Vec<_>>(),
        vec!["read", "mcp__github__create_issue"]
    );
}

struct FakeTool {
    name: &'static str,
}

impl FakeTool {
    fn new(name: &'static str) -> Self {
        Self { name }
    }
}

#[async_trait]
impl Tool for FakeTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "fake tool for extension namespacing tests"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::Text(String::new()))
    }
}
