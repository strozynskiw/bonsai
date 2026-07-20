use super::*;

pub(crate) fn canonical_context_control_id(node_id: &str) -> String {
    if let Some(index) = message_index_from_context_id(node_id) {
        return ContextNodeId::message(index).into_string();
    }
    if let Some(tool_id) = canonical_tool_control_id(node_id) {
        return tool_id;
    }
    node_id.to_string()
}

pub(crate) fn message_index_from_context_id(node_id: &str) -> Option<usize> {
    let rest = node_id.strip_prefix("msg-")?;
    let digits = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Whether `node_id` names a tool node (`tool-<call_id>` or one of its
/// children). Mirrors the format produced by [`ContextNodeId::tool`].
pub(crate) fn is_tool_context_id(node_id: &str) -> bool {
    node_id.starts_with("tool-")
}

/// The tool call id embedded in a `tool-<call_id>` node id, if `node_id` is a
/// (non-child) tool node. The counterpart of [`message_index_from_context_id`].
pub(crate) fn tool_call_id_from_context_id(node_id: &str) -> Option<&str> {
    node_id.strip_prefix("tool-")
}

fn canonical_tool_control_id(node_id: &str) -> Option<String> {
    let rest = node_id.strip_prefix("tool-")?;
    let rest = [
        "-input",
        "-output",
        "-status",
        "-stdout",
        "-stderr",
        "-truncation",
        "-diff",
        "-image",
        "-framing",
    ]
    .into_iter()
    .find_map(|suffix| rest.strip_suffix(suffix))
    .unwrap_or(rest);
    (!rest.is_empty()).then(|| ContextNodeId::tool(rest).into_string())
}
