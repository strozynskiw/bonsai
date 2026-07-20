//! Request-preview wire sections for the `/preview` request inspector.
//!
//! Providers serialize their outgoing request body to JSON and hand it here to
//! be split into a labelled, token-estimated tree ([`ProviderWireSection`]) the
//! TUI renders. This is presentation-only: it never touches the wire, only the
//! already-built body. Lifted out of `provider/mod.rs` so the hub keeps just the
//! shared types and traits.

use std::sync::Mutex;

use serde_json::Value;

/// Prompt-cache disposition of a wire section, derived from the request body
/// the provider actually built (never guessed from config alone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireCacheHint {
    /// Inside the region a cache breakpoint covers — expected to be read back
    /// from the provider's prompt cache while it stays byte-stable.
    CachedPrefix,
    /// This section carries the `cache_control` marker itself.
    Breakpoint,
    /// Past the last breakpoint in its container — rewritten every turn.
    Volatile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderWireSection {
    pub id: String,
    pub label: String,
    pub provider_path: String,
    pub token_estimate: usize,
    pub chars: usize,
    pub bytes: usize,
    pub preview: String,
    pub source_context_node_id: Option<String>,
    /// None when the provider does not annotate caching for this section.
    pub cache: Option<WireCacheHint>,
    pub children: Vec<ProviderWireSection>,
}

impl ProviderWireSection {
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        provider_path: impl Into<String>,
        preview: impl Into<String>,
        source_context_node_id: Option<String>,
        children: Vec<ProviderWireSection>,
    ) -> Self {
        let preview = preview.into();
        Self {
            id: id.into(),
            label: label.into(),
            provider_path: provider_path.into(),
            token_estimate: wire_token_estimate(&preview),
            chars: preview.chars().count(),
            bytes: preview.len(),
            preview,
            source_context_node_id,
            cache: None,
            children,
        }
    }

    pub fn from_value(
        id: impl Into<String>,
        label: impl Into<String>,
        provider_path: impl Into<String>,
        value: &Value,
        source_context_node_id: Option<String>,
    ) -> Self {
        let provider_path = provider_path.into();
        let kind = if is_system_text_path(&provider_path) {
            WireValueKind::SystemText
        } else {
            WireValueKind::Generic
        };
        Self::from_value_with_kind(
            id,
            label,
            provider_path,
            value,
            source_context_node_id,
            kind,
        )
    }

    fn from_value_with_kind(
        id: impl Into<String>,
        label: impl Into<String>,
        provider_path: impl Into<String>,
        value: &Value,
        source_context_node_id: Option<String>,
        kind: WireValueKind,
    ) -> Self {
        let id = id.into();
        let provider_path = provider_path.into();
        let preview = match value {
            Value::String(text) => text.clone(),
            _ => serde_json::to_string_pretty(value).unwrap_or_else(|_err| value.to_string()),
        };
        let children = match (kind, value) {
            (WireValueKind::SystemText, Value::String(text)) => {
                system_text_wire_children(&id, &provider_path, text)
            }
            _ => wire_children_from_value(&id, &provider_path, value),
        };
        Self::new(
            id,
            label,
            provider_path,
            preview,
            source_context_node_id,
            children,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireValueKind {
    Generic,
    SystemText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WireField {
    pub id: &'static str,
    pub label: &'static str,
    pub key: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequestPreview {
    pub method: &'static str,
    pub endpoint: String,
    pub body: Value,
    pub wire_sections: Vec<ProviderWireSection>,
}

/// Diagnostics captured from the JSON body a provider actually submitted.
///
/// Keeping the serialized bytes lets the agent calculate body size, hashes,
/// and adjacent-turn prefix reuse without rebuilding a second preview after
/// the request completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRequestDiagnostics {
    pub(crate) serialized_body: Vec<u8>,
    pub(crate) cache_mechanism: Option<String>,
    pub(crate) cache_route_key: Option<String>,
    pub(crate) preview: ProviderRequestPreview,
}

impl ProviderRequestDiagnostics {
    pub(crate) fn capture(
        preview: ProviderRequestPreview,
        slot: &Mutex<Option<Self>>,
    ) -> serde_json::Result<Vec<u8>> {
        let serialized_body = serde_json::to_vec(&preview.body)?;
        let diagnostics = Self {
            serialized_body: serialized_body.clone(),
            cache_mechanism: cache_mechanism_for_body(&preview.body),
            cache_route_key: preview
                .body
                .get("prompt_cache_key")
                .and_then(Value::as_str)
                .map(str::to_string),
            preview,
        };
        if let Ok(mut slot) = slot.lock() {
            *slot = Some(diagnostics);
        }
        Ok(serialized_body)
    }

    pub(crate) fn take(slot: &Mutex<Option<Self>>) -> Option<Self> {
        slot.lock().ok()?.take()
    }
}

impl ProviderRequestPreview {
    pub fn with_wire_sections(
        method: &'static str,
        endpoint: impl Into<String>,
        body: Value,
        wire_sections: Vec<ProviderWireSection>,
    ) -> Self {
        Self {
            method,
            endpoint: endpoint.into(),
            body,
            wire_sections,
        }
    }

    #[cfg(test)]
    pub(crate) fn cache_mechanism(&self) -> Option<String> {
        cache_mechanism_for_body(&self.body)
    }
}

fn cache_mechanism_for_body(body: &Value) -> Option<String> {
    let mut mechanisms = Vec::new();
    if body.get("prompt_cache_key").is_some() {
        mechanisms.push("prompt_cache_key");
    }
    if value_has_key(body, "prompt_cache_breakpoint") {
        mechanisms.push("explicit_breakpoints");
    }
    if value_has_cache_control(body) {
        mechanisms.push("cache_control");
    }
    (!mechanisms.is_empty()).then(|| mechanisms.join("+"))
}

fn value_has_cache_control(value: &Value) -> bool {
    value_has_key(value, "cache_control")
}

fn value_has_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key(key) || object.values().any(|value| value_has_key(value, key))
        }
        Value::Array(values) => values.iter().any(|value| value_has_key(value, key)),
        _ => false,
    }
}

pub(crate) fn wire_sections_from_body(
    body: &Value,
    fields: &[WireField],
    include_parameters: bool,
) -> Vec<ProviderWireSection> {
    let Some(object) = body.as_object() else {
        return Vec::new();
    };
    let mut sections = Vec::new();
    for (key, value) in object {
        let field = fields.iter().find(|field| field.key == key);
        if field.is_none() && !include_parameters {
            continue;
        }
        let id = field
            .map(|field| field.id.to_string())
            .unwrap_or_else(|| format!("wire-{}", sanitize_wire_id(key)));
        let label = field
            .map(|field| field.label.to_string())
            .unwrap_or_else(|| wire_field_label(key));
        sections.push(ProviderWireSection::from_value(
            id,
            label,
            format!("$.{key}"),
            value,
            None,
        ));
    }
    sections
}

/// Annotate wire sections with their prompt-cache disposition by locating the
/// `cache_control` markers in the serialized body. Truthful by construction —
/// it reads the markers the provider's `request_body` actually emitted and is
/// a no-op when the body carries none.
///
/// Only containers that hold a marker are annotated (parameters like `model`
/// stay unmarked): the marker-carrying node reads `Breakpoint`, siblings
/// before it `CachedPrefix`, siblings after it `Volatile` (e.g. the volatile
/// system tail that follows the byte-stable head's marker).
pub(crate) fn annotate_cache_control_sections(sections: &mut [ProviderWireSection], body: &Value) {
    let mut breakpoints = std::collections::HashSet::new();
    collect_cache_control_paths(body, "$", &mut breakpoints);
    if breakpoints.is_empty() {
        return;
    }
    for section in sections {
        annotate_cache_section(section, &breakpoints);
    }
}

fn collect_cache_control_paths(
    value: &Value,
    path: &str,
    out: &mut std::collections::HashSet<String>,
) {
    match value {
        Value::Object(map) => {
            if map.contains_key("cache_control") {
                out.insert(path.to_string());
            }
            for (key, child) in map {
                collect_cache_control_paths(child, &format!("{path}.{key}"), out);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                collect_cache_control_paths(item, &format!("{path}[{index}]"), out);
            }
        }
        _ => {}
    }
}

/// `#…` fragments are presentation-only text splits with no body node; they
/// share their parent's JSON path (and thus its disposition).
fn wire_base_path(provider_path: &str) -> &str {
    provider_path
        .split_once('#')
        .map_or(provider_path, |(base, _fragment)| base)
}

fn path_contains_breakpoint(path: &str, breakpoints: &std::collections::HashSet<String>) -> bool {
    breakpoints
        .iter()
        .any(|b| b.starts_with(&format!("{path}.")) || b.starts_with(&format!("{path}[")))
}

fn annotate_cache_section(
    section: &mut ProviderWireSection,
    breakpoints: &std::collections::HashSet<String>,
) {
    let path = wire_base_path(&section.provider_path).to_string();
    if breakpoints.contains(&path) {
        set_cache_hint_recursive(section, WireCacheHint::CachedPrefix);
        section.cache = Some(WireCacheHint::Breakpoint);
        return;
    }
    if !path_contains_breakpoint(&path, breakpoints) {
        return;
    }
    section.cache = Some(WireCacheHint::CachedPrefix);
    // Children past the last marker-bearing sibling fall out of the cached
    // region. Only array items get the position rule — object keys serialize
    // in map order that carries no cache semantics, and `#` fragments share
    // the container's path — so both kinds only recurse or stay unmarked.
    let last_marked = section.children.iter().rposition(|child| {
        let child_path = wire_base_path(&child.provider_path);
        child_path != path
            && (breakpoints.contains(child_path)
                || path_contains_breakpoint(child_path, breakpoints))
    });
    match last_marked {
        Some(last) => {
            for (index, child) in section.children.iter_mut().enumerate() {
                if index == last {
                    annotate_cache_section(child, breakpoints);
                } else if wire_base_path(&child.provider_path).ends_with(']') {
                    let hint = if index < last {
                        WireCacheHint::CachedPrefix
                    } else {
                        WireCacheHint::Volatile
                    };
                    set_cache_hint_recursive(child, hint);
                }
            }
        }
        None => {
            for child in &mut section.children {
                set_cache_hint_recursive(child, WireCacheHint::CachedPrefix);
            }
        }
    }
}

fn set_cache_hint_recursive(section: &mut ProviderWireSection, hint: WireCacheHint) {
    section.cache = Some(hint);
    for child in &mut section.children {
        set_cache_hint_recursive(child, hint);
    }
}

fn wire_field_label(key: &str) -> String {
    let label = key
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if label.is_empty() {
        key.to_string()
    } else {
        label
    }
}

fn wire_token_estimate(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.chars().count().saturating_div(4).max(1)
    }
}

fn wire_children_from_value(
    parent_id: &str,
    parent_path: &str,
    value: &Value,
) -> Vec<ProviderWireSection> {
    match value {
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let id = format!("{parent_id}-{index}");
                let path = format!("{parent_path}[{index}]");
                ProviderWireSection::from_value(
                    id,
                    wire_array_item_label(parent_path, index, item),
                    path,
                    item,
                    None,
                )
            })
            .collect(),
        Value::Object(map) => {
            let is_system_message = map
                .get("role")
                .and_then(Value::as_str)
                .is_some_and(|role| matches!(role, "system" | "developer"));
            map.iter()
                .map(|(key, item)| {
                    let kind = if is_system_message && key == "content" {
                        WireValueKind::SystemText
                    } else {
                        WireValueKind::Generic
                    };
                    let id = format!("{parent_id}-{}", sanitize_wire_id(key));
                    let path = format!("{parent_path}.{key}");
                    ProviderWireSection::from_value_with_kind(
                        id,
                        key.clone(),
                        path,
                        item,
                        None,
                        kind,
                    )
                })
                .collect()
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => Vec::new(),
    }
}

fn is_system_text_path(provider_path: &str) -> bool {
    matches!(provider_path, "$.system" | "$.instructions")
}

fn system_text_wire_children(
    parent_id: &str,
    parent_path: &str,
    text: &str,
) -> Vec<ProviderWireSection> {
    const PROJECT_CONTEXT_DELIMITER: &str = "\n\n# Project context\n\n";
    if let Some((persona, project_context)) = text.split_once(PROJECT_CONTEXT_DELIMITER) {
        let mut children = Vec::new();
        if !persona.trim().is_empty() {
            let persona_id = wire_child_id(parent_id, 0, "Persona");
            let persona_path = wire_child_path(parent_path, 0, "Persona");
            children.push(wire_text_part(
                parent_id,
                parent_path,
                0,
                "Persona",
                persona,
                prompt_block_wire_children(&persona_id, &persona_path, persona),
            ));
        }
        if !project_context.trim().is_empty() {
            let project_id = wire_child_id(parent_id, 1, "Project context");
            let project_path = wire_child_path(parent_path, 1, "Project context");
            children.push(wire_text_part(
                parent_id,
                parent_path,
                1,
                "Project context",
                project_context,
                markdown_heading_wire_children(&project_id, &project_path, project_context, 2),
            ));
        }
        return children;
    }

    let markdown_children = markdown_heading_wire_children(parent_id, parent_path, text, 2);
    if !markdown_children.is_empty() {
        return markdown_children;
    }
    prompt_block_wire_children(parent_id, parent_path, text)
}

fn prompt_block_wire_children(
    parent_id: &str,
    parent_path: &str,
    text: &str,
) -> Vec<ProviderWireSection> {
    let parts = text
        .split("\n\n")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.len() <= 1 {
        return Vec::new();
    }
    parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            wire_text_part(
                parent_id,
                parent_path,
                index,
                prompt_block_label(index, part),
                part,
                Vec::new(),
            )
        })
        .collect()
}

fn prompt_block_label(index: usize, text: &str) -> String {
    let first_line = text.lines().next().unwrap_or_default().trim();
    if let Some(label) = first_line.strip_suffix(':')
        && !label.trim().is_empty()
    {
        return label.trim().to_string();
    }
    if index == 0 {
        "Overview".to_string()
    } else {
        format!("Part {}", index + 1)
    }
}

fn markdown_heading_wire_children(
    parent_id: &str,
    parent_path: &str,
    text: &str,
    level: usize,
) -> Vec<ProviderWireSection> {
    if level > 3 {
        return Vec::new();
    }
    let marker = format!("{} ", "#".repeat(level));
    let mut sections = Vec::new();
    let mut current_label: Option<String> = None;
    let mut current_lines = Vec::new();
    let mut preamble_lines = Vec::new();

    for line in text.lines() {
        if let Some(label) = line.strip_prefix(&marker) {
            if let Some(label) = current_label.take() {
                push_markdown_text_part(
                    &mut sections,
                    parent_id,
                    parent_path,
                    label,
                    std::mem::take(&mut current_lines),
                    level,
                );
            } else if preamble_lines
                .iter()
                .any(|line: &String| !line.trim().is_empty())
            {
                push_markdown_text_part(
                    &mut sections,
                    parent_id,
                    parent_path,
                    "Overview".to_string(),
                    std::mem::take(&mut preamble_lines),
                    level,
                );
            }
            current_label = Some(label.trim().to_string());
            current_lines.push(line.to_string());
        } else if current_label.is_some() {
            current_lines.push(line.to_string());
        } else {
            preamble_lines.push(line.to_string());
        }
    }

    if let Some(label) = current_label {
        push_markdown_text_part(
            &mut sections,
            parent_id,
            parent_path,
            label,
            current_lines,
            level,
        );
    }

    sections
}

fn push_markdown_text_part(
    sections: &mut Vec<ProviderWireSection>,
    parent_id: &str,
    parent_path: &str,
    label: String,
    lines: Vec<String>,
    level: usize,
) {
    let text = lines.join("\n");
    let next_level = level + 1;
    let index = sections.len();
    let children = markdown_heading_wire_children(
        &wire_child_id(parent_id, index, &label),
        &wire_child_path(parent_path, index, &label),
        &text,
        next_level,
    );
    sections.push(wire_text_part(
        parent_id,
        parent_path,
        index,
        label,
        &text,
        children,
    ));
}

fn wire_text_part(
    parent_id: &str,
    parent_path: &str,
    index: usize,
    label: impl Into<String>,
    text: &str,
    children: Vec<ProviderWireSection>,
) -> ProviderWireSection {
    let label = label.into();
    ProviderWireSection::new(
        wire_child_id(parent_id, index, &label),
        label.clone(),
        wire_child_path(parent_path, index, &label),
        text.to_string(),
        None,
        children,
    )
}

fn wire_child_id(parent_id: &str, index: usize, label: &str) -> String {
    let slug = sanitize_wire_id(label);
    if slug.is_empty() {
        format!("{parent_id}-part-{index}")
    } else {
        format!("{parent_id}-{index}-{slug}")
    }
}

fn wire_child_path(parent_path: &str, index: usize, label: &str) -> String {
    let slug = sanitize_wire_id(label);
    if slug.is_empty() {
        format!("{parent_path}#part-{index}")
    } else {
        format!("{parent_path}#{index}-{slug}")
    }
}

fn wire_array_item_label(parent_path: &str, index: usize, value: &Value) -> String {
    let ordinal = index + 1;
    if parent_path.ends_with(".messages") {
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("message");
        format!("{role} message {ordinal}")
    } else if parent_path.ends_with(".tools") {
        let name = value
            .pointer("/function/name")
            .or_else(|| value.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("tool");
        format!("tool {ordinal}: {name}")
    } else if parent_path.ends_with(".input") {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("input");
        let role = value.get("role").and_then(Value::as_str);
        match role {
            Some(role) => format!("{kind} {ordinal}: {role}"),
            None => format!("{kind} {ordinal}"),
        }
    } else {
        format!("item {ordinal}")
    }
}

fn sanitize_wire_id(value: &str) -> String {
    let mut id = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch.to_ascii_lowercase());
        } else {
            id.push('-');
        }
    }
    id.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn system_text_sample() -> String {
        [
            "You are a coding agent.",
            "Style:\n- Direct.",
            "Work:\n- Read first.",
            "# Project context\n\n## Environment\n- cwd: /repo\n\n## Project instructions\nFollow these steering files.\n\n### AGENTS.md (/repo)\nUse tests.\n\n## Volatile state\n- git: dirty",
        ]
        .join("\n\n")
    }

    fn child_labels(section: &ProviderWireSection) -> Vec<&str> {
        section
            .children
            .iter()
            .map(|child| child.label.as_str())
            .collect()
    }

    #[test]
    fn cache_mechanism_reports_explicit_prompt_breakpoints() {
        let preview = ProviderRequestPreview::with_wire_sections(
            "POST",
            "/responses",
            json!({
                "prompt_cache_key": "lane-1",
                "input": [{
                    "type": "message",
                    "content": [{
                        "type": "input_text",
                        "text": "stable",
                        "prompt_cache_breakpoint": {"mode": "explicit"}
                    }]
                }]
            }),
            Vec::new(),
        );

        assert_eq!(
            preview.cache_mechanism().as_deref(),
            Some("prompt_cache_key+explicit_breakpoints")
        );
    }

    #[test]
    fn request_diagnostics_capture_serialized_body_and_drain_once() {
        let slot = Mutex::new(None);
        let body = json!({
            "prompt_cache_key": "lane-1",
            "messages": [{"role": "user", "content": "hello"}],
        });

        let preview = ProviderRequestPreview::with_wire_sections(
            "POST",
            "/responses",
            body.clone(),
            Vec::new(),
        );
        let sent = ProviderRequestDiagnostics::capture(preview, &slot).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&sent).unwrap(), body);

        let captured = ProviderRequestDiagnostics::take(&slot).unwrap();
        assert_eq!(captured.serialized_body, sent);
        assert_eq!(
            captured.cache_mechanism.as_deref(),
            Some("prompt_cache_key")
        );
        assert_eq!(captured.cache_route_key.as_deref(), Some("lane-1"));
        assert_eq!(captured.preview.body, body);
        assert_eq!(ProviderRequestDiagnostics::take(&slot), None);
    }

    #[test]
    fn provider_wire_section_splits_top_level_system_text() {
        let text = system_text_sample();
        let section = ProviderWireSection::from_value(
            "wire-system",
            "System",
            "$.system",
            &Value::String(text),
            None,
        );

        assert_eq!(child_labels(&section), vec!["Persona", "Project context"]);
        assert_eq!(
            child_labels(&section.children[0]),
            vec!["Overview", "Style", "Work"]
        );
        assert_eq!(
            child_labels(&section.children[1]),
            vec!["Environment", "Project instructions", "Volatile state"]
        );
        assert_eq!(
            child_labels(&section.children[1].children[1]),
            vec!["Overview", "AGENTS.md (/repo)"]
        );
    }

    #[test]
    fn annotate_cache_control_marks_breakpoints_prefix_and_volatile_tail() {
        let body = json!({
            "model": "claude",
            "tools": [
                {"name": "read"},
                {"name": "edit", "cache_control": {"type": "ephemeral"}},
            ],
            "system": [
                {"type": "text", "text": "stable", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "volatile"},
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "one"},
                    {"type": "text", "text": "two", "cache_control": {"type": "ephemeral"}},
                ]},
            ],
        });
        let mut sections = wire_sections_from_body(
            &body,
            &[
                WireField {
                    id: "wire-tools",
                    label: "Tools",
                    key: "tools",
                },
                WireField {
                    id: "wire-system",
                    label: "System",
                    key: "system",
                },
                WireField {
                    id: "wire-messages",
                    label: "Messages",
                    key: "messages",
                },
            ],
            true,
        );
        annotate_cache_control_sections(&mut sections, &body);

        let by_id = |id: &str| {
            sections
                .iter()
                .find(|section| section.id == id)
                .unwrap_or_else(|| panic!("{id} section"))
        };
        // Parameters carry no cache semantics.
        assert_eq!(by_id("wire-model").cache, None);

        let tools = by_id("wire-tools");
        assert_eq!(tools.cache, Some(WireCacheHint::CachedPrefix));
        assert_eq!(tools.children[0].cache, Some(WireCacheHint::CachedPrefix));
        assert_eq!(tools.children[1].cache, Some(WireCacheHint::Breakpoint));

        let system = by_id("wire-system");
        assert_eq!(system.cache, Some(WireCacheHint::CachedPrefix));
        assert_eq!(system.children[0].cache, Some(WireCacheHint::Breakpoint));
        assert_eq!(system.children[1].cache, Some(WireCacheHint::Volatile));

        let messages = by_id("wire-messages");
        assert_eq!(messages.cache, Some(WireCacheHint::CachedPrefix));
        let message = &messages.children[0];
        assert_eq!(message.cache, Some(WireCacheHint::CachedPrefix));
        let content = message
            .children
            .iter()
            .find(|child| child.label == "content")
            .expect("content child");
        assert_eq!(content.cache, Some(WireCacheHint::CachedPrefix));
        assert_eq!(content.children[0].cache, Some(WireCacheHint::CachedPrefix));
        assert_eq!(content.children[1].cache, Some(WireCacheHint::Breakpoint));
        // Object-key siblings (role) carry no positional cache meaning.
        let role = message
            .children
            .iter()
            .find(|child| child.label == "role")
            .expect("role child");
        assert_eq!(role.cache, None);
    }

    #[test]
    fn annotate_cache_control_is_a_noop_without_markers() {
        let body = json!({
            "model": "gpt",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let mut sections = wire_sections_from_body(
            &body,
            &[WireField {
                id: "wire-messages",
                label: "Messages",
                key: "messages",
            }],
            true,
        );
        annotate_cache_control_sections(&mut sections, &body);
        fn all_none(section: &ProviderWireSection) -> bool {
            section.cache.is_none() && section.children.iter().all(all_none)
        }
        assert!(sections.iter().all(all_none));
    }

    #[test]
    fn provider_wire_section_splits_chat_system_message_content() {
        let value = json!({
            "role": "system",
            "content": system_text_sample(),
        });
        let section = ProviderWireSection::from_value(
            "wire-messages-0",
            "system message 1",
            "$.messages[0]",
            &value,
            None,
        );
        let content = section
            .children
            .iter()
            .find(|child| child.label == "content")
            .expect("system message content should be present");

        assert_eq!(child_labels(content), vec!["Persona", "Project context"]);
    }
}
