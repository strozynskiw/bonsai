//! Separate durable Bonsai identities from provider protocol correlation IDs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ToolCall;

/// Original protocol ID belonging to one canonical Bonsai tool call.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolCallIdentity {
    pub canonical_id: String,
    pub provider_scope: String,
    pub wire_id: String,
    /// Dependencies of a Bonsai-generated reuse pointer, never arbitrary output.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reuse_source_call_ids: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct RequestToolCallIds {
    calls: Vec<String>,
    results: Vec<String>,
}

impl RequestToolCallIds {
    fn capture(body: &Value) -> Self {
        let mut ids = Self::default();
        if let Some(messages) = body.get("messages").and_then(Value::as_array) {
            for message in messages {
                match message.get("role").and_then(Value::as_str) {
                    Some("assistant") => {
                        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                            ids.calls.extend(
                                calls
                                    .iter()
                                    .filter_map(|call| call.get("id").and_then(Value::as_str))
                                    .map(str::to_string),
                            );
                        }
                    }
                    Some("tool") => {
                        if let Some(id) = message.get("tool_call_id").and_then(Value::as_str) {
                            ids.results.push(id.to_string());
                        }
                    }
                    _ => {}
                }
                if let Some(parts) = message.get("content").and_then(Value::as_array) {
                    for part in parts {
                        let target = match part.get("type").and_then(Value::as_str) {
                            Some("tool_use") => (&mut ids.calls, "id"),
                            Some("tool_result") => (&mut ids.results, "tool_use_id"),
                            _ => continue,
                        };
                        if let Some(id) = part.get(target.1).and_then(Value::as_str) {
                            target.0.push(id.to_string());
                        }
                    }
                }
            }
        }
        if let Some(input) = body.get("input").and_then(Value::as_array) {
            for item in input {
                let target = match item.get("type").and_then(Value::as_str) {
                    Some("function_call") => &mut ids.calls,
                    Some("function_call_output") => &mut ids.results,
                    _ => continue,
                };
                if let Some(id) = item.get("call_id").and_then(Value::as_str) {
                    target.push(id.to_string());
                }
            }
        }
        ids
    }
}

/// Session-owned identity mapping, shared with each active provider.
///
/// Calls receive UUID identities independently of working-context retention.
/// Requests to their original provider restore its exact correlation IDs.
#[derive(Debug, Clone, Default)]
pub struct ToolCallIdentityStore {
    identities: Arc<Mutex<HashMap<String, ToolCallIdentity>>>,
}

impl ToolCallIdentityStore {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, ToolCallIdentity>> {
        self.identities
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn assign_calls(&self, scope: &str, calls: &mut [ToolCall]) {
        let mut identities = self.lock();
        for call in calls {
            let canonical_id = format!("call_{}", uuid::Uuid::now_v7().simple());
            let wire_id = std::mem::replace(&mut call.id, canonical_id.clone());
            identities.insert(
                canonical_id.clone(),
                ToolCallIdentity {
                    wire_id: if wire_id.is_empty() {
                        canonical_id.clone()
                    } else {
                        wire_id
                    },
                    canonical_id,
                    provider_scope: scope.to_string(),
                    reuse_source_call_ids: Vec::new(),
                },
            );
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<ToolCallIdentity> {
        let mut identities = self.lock().values().cloned().collect::<Vec<_>>();
        identities.sort_unstable_by(|left, right| left.canonical_id.cmp(&right.canonical_id));
        identities
    }

    pub(crate) fn restore(&self, records: &[ToolCallIdentity]) {
        let mut identities = self.lock();
        identities.clear();
        identities.extend(
            records
                .iter()
                .map(|record| (record.canonical_id.clone(), record.clone())),
        );
    }

    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    pub(crate) fn set_reuse_sources(&self, call_id: &str, sources: Vec<String>) {
        if let Some(identity) = self.lock().get_mut(call_id) {
            identity.reuse_source_call_ids = sources;
        }
    }

    /// Project only protocol identity fields; arguments and opaque replay
    /// payloads must remain byte-for-byte unchanged.
    pub(crate) fn project_request(&self, scope: &str, body: &mut Value) -> RequestToolCallIds {
        let ids = RequestToolCallIds::capture(body);
        let identities = self.lock();
        let project = |value: &mut Value| {
            let Some(id) = value.as_str() else {
                return;
            };
            if let Some(identity) = identities
                .get(id)
                .filter(|identity| identity.provider_scope == scope)
            {
                *value = Value::String(identity.wire_id.clone());
            }
        };
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                match message.get("role").and_then(Value::as_str) {
                    Some("assistant") => {
                        if let Some(calls) =
                            message.get_mut("tool_calls").and_then(Value::as_array_mut)
                        {
                            for call in calls {
                                if let Some(id) = call.get_mut("id") {
                                    project(id);
                                }
                            }
                        }
                    }
                    Some("tool") => {
                        if let Some(id) = message.get_mut("tool_call_id") {
                            project(id);
                        }
                    }
                    _ => {}
                }
                if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
                    for block in content {
                        let key = match block.get("type").and_then(Value::as_str) {
                            Some("tool_use") => "id",
                            Some("tool_result") => "tool_use_id",
                            _ => continue,
                        };
                        if let Some(id) = block.get_mut(key) {
                            project(id);
                        }
                    }
                }
            }
        }
        if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
            for item in input {
                if matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("function_call" | "function_call_output")
                ) && let Some(id) = item.get_mut("call_id")
                {
                    project(id);
                }
            }
        }
        ids
    }

    /// Translate references in typed reuse pointers to the IDs the model sees,
    /// after any compatible-wire deduplication. File bytes and opaque metadata
    /// are not rewritten; only outputs explicitly marked by the agent qualify.
    pub(crate) fn project_reuse_references(&self, body: &mut Value, ids: &RequestToolCallIds) {
        let wire_ids = RequestToolCallIds::capture(body);
        let mut counts = HashMap::new();
        for wire in &wire_ids.calls {
            *counts.entry(wire.as_str()).or_insert(0usize) += 1;
        }
        let labels = wire_ids
            .calls
            .iter()
            .enumerate()
            .map(|(index, wire)| {
                if counts[wire.as_str()] > 1 {
                    std::borrow::Cow::Owned(format!("{wire} (earlier tool call #{})", index + 1))
                } else {
                    std::borrow::Cow::Borrowed(wire.as_str())
                }
            })
            .collect::<Vec<_>>();
        let projected = ids
            .calls
            .iter()
            .zip(&labels)
            .map(|(id, wire)| (id.as_str(), wire.as_ref()))
            .collect::<HashMap<_, _>>();
        let identities = self.lock();
        let mut results = ids.results.iter();
        let mut rewrite = |value: Option<&mut Value>| {
            let Some(id) = results.next() else {
                return;
            };
            let Some(identity) = identities
                .get(id)
                .filter(|identity| !identity.reuse_source_call_ids.is_empty())
            else {
                return;
            };
            let Some(value) = value else {
                return;
            };
            let rewrite_text = |text: &mut Value| {
                if let Some(content) = text.as_str() {
                    *text = Value::String(project_pointer_references(
                        content,
                        &identity.reuse_source_call_ids,
                        &projected,
                    ));
                }
            };
            if value.is_string() {
                rewrite_text(value);
            } else if let Some(parts) = value.as_array_mut() {
                for part in parts {
                    if part.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = part.get_mut("text")
                    {
                        rewrite_text(text);
                    }
                }
            }
        };
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                if message.get("role").and_then(Value::as_str) == Some("tool")
                    && message.get("tool_call_id").is_some_and(Value::is_string)
                {
                    rewrite(message.get_mut("content"));
                }
                if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                    for part in parts {
                        if part.get("type").and_then(Value::as_str) == Some("tool_result")
                            && part.get("tool_use_id").is_some_and(Value::is_string)
                        {
                            rewrite(part.get_mut("content"));
                        }
                    }
                }
            }
        }
        if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
            for item in input {
                if item.get("type").and_then(Value::as_str) == Some("function_call_output")
                    && item.get("call_id").is_some_and(Value::is_string)
                {
                    rewrite(item.get_mut("output"));
                }
            }
        }
    }
}

fn project_pointer_references(
    content: &str,
    sources: &[String],
    projected: &HashMap<&str, &str>,
) -> String {
    if !content.starts_with("[reused previous read]")
        && !content.starts_with("[partially reused read]")
        && !content.starts_with("[reused previous recall]")
    {
        return content.to_string();
    }
    let first_line_end = content.find('\n').map_or(content.len(), |index| index + 1);
    let header_end = if content[first_line_end..].starts_with("source_calls: ") {
        first_line_end
            + content[first_line_end..]
                .find('\n')
                .map_or(content.len() - first_line_end, |index| index + 1)
    } else {
        first_line_end
    };
    let (header, tail) = content.split_at(header_end);
    let marker = if header.starts_with("[partially reused read]") {
        " from tool(s) "
    } else if header.starts_with("[reused previous recall]") {
        "(call "
    } else if header.contains(" — unchanged since tool ") {
        " — unchanged since tool "
    } else {
        " across tool(s) "
    };
    let reference_start = header
        .rfind(marker)
        .map(|index| index + marker.len())
        .or_else(|| {
            header[first_line_end..]
                .starts_with("source_calls: ")
                .then_some(first_line_end + "source_calls: ".len())
        });
    let mut text = match reference_start {
        Some(start) => {
            let mut text = header[..start].to_string();
            text.push_str(&replace_reference_ids(&header[start..], sources, projected));
            text
        }
        None => header.to_string(),
    };
    text.push_str(tail);
    text
}

fn replace_reference_ids(
    text: &str,
    sources: &[String],
    projected: &HashMap<&str, &str>,
) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((offset, source, wire)) = sources
        .iter()
        .filter_map(|source| {
            if source.is_empty() {
                return None;
            }
            let wire = *projected.get(source.as_str())?;
            if wire == source {
                return None;
            }
            rest.find(source.as_str())
                .map(|offset| (offset, source.as_str(), wire))
        })
        .min_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| right.1.len().cmp(&left.1.len()))
        })
    {
        output.push_str(&rest[..offset]);
        output.push_str(wire);
        rest = &rest[offset + source.len()..];
    }
    output.push_str(rest);
    output
}

pub(crate) fn tool_call_scope(
    transport: &str,
    provider_id: &str,
    endpoint: &str,
    path: &str,
) -> String {
    let mut hash = blake3::Hasher::new();
    for part in [transport, provider_id, endpoint, path] {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{transport}:{}", hash.finalize().to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reference_projection_does_not_reinterpret_a_projected_id_as_another_source() {
        let sources = vec!["first".to_string(), "second".to_string()];
        let projected = HashMap::from([("first", "second"), ("second", "native")]);
        let pointer = "[partially reused read] f.rs: kept lines 1-2 from tool(s) first, second; newly read lines 3-4 follow.\nsource_calls: first, second\nfile data: first, second";
        let actual = project_pointer_references(pointer, &sources, &projected);
        assert!(actual.contains("tool(s) second, native;"));
        assert!(actual.contains("source_calls: second, native\n"));
        assert!(actual.ends_with("file data: first, second"));
    }

    #[test]
    fn typed_reuse_references_follow_final_wire_ids_without_rewriting_file_bytes() {
        let store = ToolCallIdentityStore::default();
        let mut calls = ["same", "same", "pointer"].map(|id| ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: "{}".into(),
        });
        store.assign_calls("scope", &mut calls);
        let source = &calls[1].id;
        store.set_reuse_sources(&calls[2].id, vec![source.clone()]);
        let file_bytes = format!("\n1: tool {source}\n2: source_calls: {source}\n");
        let pointer = format!(
            "[partially reused read] {source}.rs: kept lines 1-2 from tool(s) {source}; newly read lines 3-4 follow.\nsource_calls: {source}\n{file_bytes}"
        );
        let mut body = json!({"messages":[
            {"role":"assistant","tool_calls":[{"id":calls[0].id}]},
            {"role":"tool","tool_call_id":calls[0].id,"content":file_bytes},
            {"role":"assistant","tool_calls":[{"id":calls[1].id}]},
            {"role":"tool","tool_call_id":calls[1].id,"content":"retained source"},
            {"role":"assistant","tool_calls":[{"id":calls[2].id}]},
            {"role":"tool","tool_call_id":calls[2].id,"content":pointer}
        ]});
        let ids = store.project_request("scope", &mut body);
        // Compatible transports can deduplicate wire correlators afterward.
        body["messages"][2]["tool_calls"][0]["id"] = json!("same-dup2");
        body["messages"][3]["tool_call_id"] = json!("same-dup2");
        store.project_reuse_references(&mut body, &ids);
        assert_eq!(body["messages"][1]["content"], file_bytes);
        let projected = body["messages"][5]["content"].as_str().unwrap();
        assert!(
            projected.starts_with(&format!("[partially reused read] {source}.rs:")),
            "path unchanged"
        );
        assert!(projected.contains("from tool(s) same-dup2;"));
        assert!(projected.contains("\nsource_calls: same-dup2\n"));
        assert!(
            projected.ends_with(&file_bytes),
            "fresh file bytes unchanged"
        );
    }

    #[test]
    fn signed_reused_correlators_keep_reference_positions_unambiguous() {
        let store = ToolCallIdentityStore::default();
        let mut calls = ["signed", "signed", "pointer"].map(|id| ToolCall {
            id: id.into(),
            name: "recall".into(),
            arguments: "{}".into(),
        });
        store.assign_calls("scope", &mut calls);
        store.set_reuse_sources(&calls[2].id, vec![calls[1].id.clone()]);
        let pointer = format!(
            "[reused previous recall] The identical recall page (call {}) is still live.\nsource_calls: {}",
            calls[1].id, calls[1].id
        );
        let mut body = json!({"input":[
            {"type":"function_call","call_id":calls[0].id},
            {"type":"function_call_output","call_id":calls[0].id,"output":"first"},
            {"type":"function_call","call_id":calls[1].id},
            {"type":"function_call_output","call_id":calls[1].id,"output":"second"},
            {"type":"function_call","call_id":calls[2].id},
            {"type":"function_call_output","call_id":calls[2].id,"output":pointer}
        ]});
        let ids = store.project_request("scope", &mut body);
        store.project_reuse_references(&mut body, &ids);
        assert_eq!(body["input"][0]["call_id"], "signed");
        assert_eq!(body["input"][2]["call_id"], "signed");
        let pointer = body["input"][5]["output"].as_str().unwrap();
        assert!(pointer.contains("call signed (earlier tool call #2)"));
        assert!(pointer.contains("source_calls: signed (earlier tool call #2)"));
    }

    #[test]
    fn repeated_wire_ids_get_distinct_durable_ids_and_exact_replay() {
        let store = ToolCallIdentityStore::default();
        let mut calls = (0..2)
            .map(|_| ToolCall {
                id: "same".into(),
                name: "read".into(),
                arguments: "{}".into(),
            })
            .collect::<Vec<_>>();
        store.assign_calls("scope", &mut calls);
        assert_ne!(calls[0].id, calls[1].id);
        let records = store.snapshot();
        store.clear();
        store.restore(&records);
        let mut body = json!({"messages": [
            {"role":"assistant", "tool_calls":[{"id":calls[0].id, "extra_content":{"google":{"thought_signature":"opaque"}}}]},
            {"role":"tool", "tool_call_id":calls[0].id, "content":"result"},
            {"role":"assistant", "tool_calls":[{"id":calls[1].id}]},
            {"role":"tool", "tool_call_id":calls[1].id, "content":"second"}
        ]});
        store.project_request("scope", &mut body);
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "same");
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
            "opaque"
        );
        assert_eq!(body["messages"][1]["tool_call_id"], "same");
        assert_eq!(body["messages"][2]["tool_calls"][0]["id"], "same");
        assert_eq!(body["messages"][3]["tool_call_id"], "same");
    }

    #[test]
    fn identity_projection_is_provider_scoped_and_preserves_legacy_ids() {
        let store = ToolCallIdentityStore::default();
        let mut calls = [ToolCall {
            id: "wire".into(),
            name: "f".into(),
            arguments: "{}".into(),
        }];
        store.assign_calls("first", &mut calls);
        let mut body = json!({"input":[{"type":"function_call", "call_id":calls[0].id, "id":"opaque-item"}, {"type":"function_call_output", "call_id":"legacy"}]});
        let unchanged = body.clone();
        store.project_request("second", &mut body);
        assert_eq!(body, unchanged);
        store.project_request("first", &mut body);
        assert_eq!(body["input"][0]["call_id"], "wire");
        assert_eq!(body["input"][0]["id"], "opaque-item");
        assert_eq!(body["input"][1]["call_id"], "legacy");
    }
}
