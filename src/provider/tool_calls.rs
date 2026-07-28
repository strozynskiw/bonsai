use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

use crate::provider::ToolCall;

/// Some OpenAI-compatible backends (Gemini's compat endpoint, several Qwen
/// gateways) stream tool calls with no `id` at all. Downstream, tool-call ids
/// are the join key everywhere — the TUI matches `ToolFinished` to its card by
/// id, persistence keys `tool_calls` rows by it, and reasoning replay maps are
/// keyed by the first call id — so letting `""` escape makes every parallel
/// call collide on the first empty-id card in the transcript: the first tool
/// swallows all results and every later tool spins as "running" forever.
/// Synthesize a process-unique id instead; the provider never sent one, so the
/// only contract is self-consistency between the assistant echo and the tool
/// result, which both read this value from the stored `ToolCall`. Deliberate
/// mixed-model seam — do not "simplify" back to passing the wire id through.
fn synthesize_missing_call_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("bonsai-call-{seq}")
}

fn ensure_call_id(id: String) -> String {
    if id.is_empty() {
        synthesize_missing_call_id()
    } else {
        id
    }
}

#[derive(Debug, Default)]
pub(crate) struct ChatToolCallAccumulator {
    // Keyed by the delta `index` and ordered by it: `into_tool_calls` must yield
    // parallel tool calls in their wire order deterministically (a `HashMap`
    // would iterate in arbitrary order).
    calls: BTreeMap<usize, (String, String, String, Option<Value>)>,
}

#[derive(Debug)]
pub(crate) struct ChatToolCall {
    pub(crate) call: ToolCall,
    pub(crate) extra_content: Option<Value>,
}

impl ChatToolCallAccumulator {
    pub(crate) fn push_delta(&mut self, tool_call: &Value) {
        let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        let entry = self
            .calls
            .entry(index)
            .or_insert_with(|| (String::new(), String::new(), String::new(), None));

        if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
            entry.0 = id.to_string();
        }
        if let Some(function) = tool_call.get("function").and_then(Value::as_object) {
            if let Some(name) = function.get("name").and_then(Value::as_str) {
                entry.1.push_str(name);
            }
            if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                entry.2.push_str(arguments);
            }
        }
        if let Some(extra_content) = tool_call
            .get("extra_content")
            .filter(|value| !value.is_null())
        {
            entry.3 = Some(extra_content.clone());
        }
    }

    #[cfg(test)]
    pub(crate) fn into_tool_calls(self) -> Vec<ToolCall> {
        self.into_tool_calls_with_extra_content()
            .into_iter()
            .map(|tool_call| tool_call.call)
            .collect()
    }

    pub(crate) fn into_tool_calls_with_extra_content(self) -> Vec<ChatToolCall> {
        // Kimi K2 mints deterministic `name:index` ids and can hand two
        // parallel calls in one message the same id (observed live:
        // `project_info:1` twice). Duplicates break the same downstream id
        // joins as empty ids above, and Moonshot 400s any replayed history
        // containing them ("tool call id ... is duplicated"), wedging the
        // session. Keep the first occurrence, suffix later ones.
        let mut seen = std::collections::HashSet::new();
        self.calls
            .into_iter()
            .map(|(_, (id, name, arguments, extra_content))| {
                let mut id = ensure_call_id(id);
                if !seen.insert(id.clone()) {
                    let mut suffix = 2usize;
                    id = loop {
                        let candidate = format!("{id}-dup{suffix}");
                        if seen.insert(candidate.clone()) {
                            break candidate;
                        }
                        suffix += 1;
                    };
                }
                ChatToolCall {
                    call: ToolCall {
                        id,
                        name,
                        arguments,
                    },
                    extra_content,
                }
            })
            .collect()
    }
}

pub(crate) fn responses_tool_call_from_done_event(
    event: &Value,
    argument_deltas: &HashMap<String, String>,
) -> Option<ToolCall> {
    let item = event.get("item")?;
    if item.get("type").and_then(Value::as_str) != Some("function_call") {
        return None;
    }

    let item_id = item.get("id").and_then(Value::as_str);
    let id = ensure_call_id(
        item.get("call_id")
            .and_then(Value::as_str)
            .or(item_id)
            .unwrap_or_default()
            .to_string(),
    );
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| item_id.and_then(|id| argument_deltas.get(id).cloned()))
        .unwrap_or_else(|| "{}".to_string());

    Some(ToolCall {
        id,
        name,
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_tool_call_accumulator_joins_chunked_function_fields() {
        let mut accumulator = ChatToolCallAccumulator::default();
        accumulator.push_delta(&serde_json::json!({
            "index": 0,
            "id": "call_1",
            "function": {"name": "re", "arguments": "{\"path\":"}
        }));
        accumulator.push_delta(&serde_json::json!({
            "index": 0,
            "function": {"name": "ad", "arguments": "\"Cargo.toml\"}"}
        }));

        let calls = accumulator.into_tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "read");
        assert_eq!(calls[0].arguments, "{\"path\":\"Cargo.toml\"}");
    }

    #[test]
    fn chat_tool_call_accumulator_preserves_provider_extra_content() {
        let mut accumulator = ChatToolCallAccumulator::default();
        accumulator.push_delta(&serde_json::json!({
            "index": 0,
            "id": "call_1",
            "function": {"name": "read", "arguments": "{}"},
            "extra_content": {
                "google": {"thought_signature": "encrypted-signature"}
            }
        }));

        let calls = accumulator.into_tool_calls_with_extra_content();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call.id, "call_1");
        assert_eq!(
            calls[0].extra_content,
            Some(serde_json::json!({
                "google": {"thought_signature": "encrypted-signature"}
            }))
        );
    }

    #[test]
    fn chat_tool_call_accumulator_orders_parallel_calls_by_index() {
        let mut accumulator = ChatToolCallAccumulator::default();
        // Deltas can arrive out of index order; the result must be index-ordered.
        for index in [2_u64, 0, 1] {
            accumulator.push_delta(&serde_json::json!({
                "index": index,
                "id": format!("call_{index}"),
                "function": {"name": "f", "arguments": "{}"}
            }));
        }

        let calls = accumulator.into_tool_calls();
        let ids: Vec<&str> = calls.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["call_0", "call_1", "call_2"]);
    }

    #[test]
    fn accumulator_synthesizes_unique_ids_when_the_wire_omits_them() {
        // Gemini's OpenAI-compat endpoint and some Qwen gateways send tool-call
        // deltas without any `id`. Empty ids collide downstream (the TUI joins
        // ToolFinished to its card by id), so each call must get a distinct
        // synthesized id — including across separate accumulators, because the
        // transcript matches ids across the whole session, not per turn.
        let mut first_turn = ChatToolCallAccumulator::default();
        for index in [0_u64, 1] {
            first_turn.push_delta(&serde_json::json!({
                "index": index,
                "function": {"name": "read", "arguments": "{}"}
            }));
        }
        let mut second_turn = ChatToolCallAccumulator::default();
        second_turn.push_delta(&serde_json::json!({
            "index": 0,
            "function": {"name": "grep", "arguments": "{}"}
        }));

        let mut ids: Vec<String> = first_turn
            .into_tool_calls()
            .into_iter()
            .chain(second_turn.into_tool_calls())
            .map(|call| call.id)
            .collect();
        assert!(ids.iter().all(|id| !id.is_empty()));
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3, "ids must be unique: {ids:?}");
    }

    #[test]
    fn accumulator_uniquifies_duplicate_wire_ids_within_one_message() {
        // Kimi K2 mints `name:index` ids and can hand two parallel calls in
        // one message the same id; the first keeps the wire id (its result
        // joins normally), later ones get a deterministic suffix.
        let mut accumulator = ChatToolCallAccumulator::default();
        for index in [0_u64, 1, 2] {
            accumulator.push_delta(&serde_json::json!({
                "index": index,
                "id": "project_info:1",
                "function": {"name": "project_info", "arguments": "{}"}
            }));
        }

        let ids: Vec<String> = accumulator
            .into_tool_calls()
            .into_iter()
            .map(|call| call.id)
            .collect();
        assert_eq!(
            ids,
            [
                "project_info:1",
                "project_info:1-dup2",
                "project_info:1-dup3"
            ]
        );
    }

    #[test]
    fn responses_tool_call_synthesizes_id_when_done_event_omits_both_ids() {
        let call = responses_tool_call_from_done_event(
            &serde_json::json!({
                "item": {
                    "type": "function_call",
                    "name": "read",
                    "arguments": "{}"
                }
            }),
            &HashMap::new(),
        )
        .expect("tool call should be parsed");
        assert!(!call.id.is_empty());
    }

    #[test]
    fn responses_tool_call_uses_accumulated_arguments_when_done_event_omits_them() {
        let mut arguments = HashMap::new();
        arguments.insert(
            "item_1".to_string(),
            "{\"path\":\"Cargo.toml\"}".to_string(),
        );

        let call = responses_tool_call_from_done_event(
            &serde_json::json!({
                "item": {
                    "type": "function_call",
                    "id": "item_1",
                    "call_id": "call_1",
                    "name": "read"
                }
            }),
            &arguments,
        )
        .expect("tool call should be parsed");

        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments, "{\"path\":\"Cargo.toml\"}");
    }
}
