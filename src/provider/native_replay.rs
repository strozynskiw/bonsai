//! Native response payloads are protocol data, addressed by immutable provenance.
//!
//! Neither answer text nor context-control row IDs identify a model response:
//! text can repeat, and row IDs are reassigned on restoration. Request context
//! deliberately has no serialization implementation so provenance cannot leak
//! into model-visible messages.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{ProviderFailure, ProviderResult};

/// Immutable identity reserved for one provider response attempt.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ResponseId(String);

impl ResponseId {
    /// Allocate independently of visible text, UI IDs, and session boundaries.
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7().to_string())
    }
}

impl Default for ResponseId {
    fn default() -> Self {
        Self::new()
    }
}

impl<'de> Deserialize<'de> for ResponseId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let id = uuid::Uuid::parse_str(&text).map_err(|error| {
            serde::de::Error::custom(format!("invalid native response UUID: {error}"))
        })?;
        Ok(Self(id.to_string()))
    }
}

/// Provenance aligned with assistant rows in the original request.
///
/// `None` means provenance is unavailable, including legacy stored history.
/// A present vector must cover every assistant row; individual missing entries
/// remain missing and never select replay data by text or tool-call IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderRequestContext {
    pub assistant_response_ids: Option<Vec<Option<ResponseId>>>,
    pub response_id: Option<ResponseId>,
}

impl ProviderRequestContext {
    /// Validate alignment after transforms that preserve assistant row order.
    ///
    /// # Errors
    /// Returns a configuration failure if provenance would address wrong rows.
    pub fn validate_assistant_count(&self, count: usize) -> ProviderResult<()> {
        if let Some(ids) = &self.assistant_response_ids
            && ids.len() != count
        {
            return Err(ProviderFailure::configuration(format!(
                "native response provenance has {} assistant entries for {count} rows",
                ids.len()
            )));
        }
        Ok(())
    }
}

/// Known native fields from one completed Chat Completions response.
///
/// Outer field names are typed; nested signed/encrypted payloads remain opaque
/// JSON. `Some(empty)` retains a provider's explicit empty native channel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatNativeReplay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_evidence: Option<NativeBudgetEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_parts: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_details: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_extra_content: BTreeMap<String, Value>,
    /// Original function values belonging to opaque per-call replay. Runtime
    /// normalization must not silently change the call paired with its data.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_functions: BTreeMap<String, NativeToolFunction>,
}

/// Exact provider-returned function strings, separate from runtime validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeToolFunction {
    pub name: String,
    pub arguments: String,
}

/// Ordered assistant content from a completed Anthropic response. Native
/// thinking, signatures, redacted data, and unknown block fields stay opaque.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicNativeReplay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_evidence: Option<NativeBudgetEvidence>,
    /// Absent for an unsigned tool turn that uses typed-message reconstruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_blocks: Option<Vec<Value>>,
    /// The actual request configuration must survive rebuilds during tool use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_settings: Option<AnthropicReplaySettings>,
}

/// Thinking configuration submitted for one Anthropic assistant/tool turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicReplaySettings {
    pub reasoning: super::ReasoningSelection,
    pub thinking: Option<Value>,
    pub output_config: Option<Value>,
}

/// Ordered supported Responses output items, retained independently of display
/// text. Opaque item IDs, encrypted data, phases, and function strings are data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexNativeReplay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_evidence: Option<NativeBudgetEvidence>,
    pub output_items: Vec<Value>,
}

/// Evidence for a conservative replay allowance, independent of ciphertext
/// length. Missing measurements remain missing; explicit zero is preserved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeBudgetEvidence {
    pub output_tokens: Option<u64>,
    /// The original request's actual output-token ceiling, when one was sent.
    pub output_ceiling_tokens: Option<u64>,
}

/// Native cost belonging to one selected original assistant row. A response
/// with several blocks/items contributes one opaque allowance per occurrence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NativeBudgetAttribution {
    pub assistant_slot: usize,
    pub reasoning_text: String,
    pub opaque: bool,
    pub opaque_allowance_tokens: Option<u64>,
}

impl NativeReplayPayload {
    fn budget_attribution(&self, assistant_slot: usize) -> NativeBudgetAttribution {
        let mut texts = Vec::new();
        let mut opaque = false;
        let evidence = match self {
            Self::OpenAiChat(native) => {
                texts.extend(native.reasoning_content.iter().map(String::as_str));
                texts.extend(native.legacy_reasoning.iter().map(String::as_str));
                for detail in native.reasoning_details.iter().flatten() {
                    for key in ["text", "summary"] {
                        if let Some(value) = detail.get(key) {
                            collect_native_text(value, &mut texts);
                        }
                    }
                    opaque |= has_opaque_value(detail, "data")
                        || has_opaque_value(detail, "signature")
                        || !matches!(
                            detail["type"].as_str(),
                            Some("reasoning.text" | "reasoning.summary")
                        );
                }
                for part in native.content_parts.iter().flatten() {
                    if matches!(part["type"].as_str(), Some("thinking" | "reasoning")) {
                        for key in ["thinking", "text"] {
                            if let Some(value) = part.get(key) {
                                collect_native_text(value, &mut texts);
                            }
                        }
                        opaque |= has_opaque_value(part, "signature");
                    }
                }
                opaque |= native
                    .tool_extra_content
                    .values()
                    .any(|value| !value.is_null());
                native.budget_evidence
            }
            Self::AnthropicMessages(native) => {
                for block in native.content_blocks.iter().flatten() {
                    match block["type"].as_str() {
                        Some("thinking") => {
                            texts.extend(block.get("thinking").and_then(Value::as_str));
                            opaque |= has_opaque_value(block, "signature");
                        }
                        Some("redacted_thinking") => opaque = true,
                        _ => {}
                    }
                }
                native.budget_evidence
            }
            Self::CodexResponses(native) => {
                for item in &native.output_items {
                    if item["type"] == "reasoning" {
                        opaque |= has_opaque_value(item, "encrypted_content");
                        for part in ["summary", "content"]
                            .into_iter()
                            .flat_map(|key| item.get(key).and_then(Value::as_array))
                            .flatten()
                        {
                            texts.extend(part.get("text").and_then(Value::as_str));
                        }
                    }
                }
                native.budget_evidence
            }
        };
        NativeBudgetAttribution {
            assistant_slot,
            reasoning_text: texts.join("\n"),
            opaque,
            opaque_allowance_tokens: evidence
                .and_then(|evidence| evidence.output_tokens.or(evidence.output_ceiling_tokens)),
        }
    }
}

fn has_opaque_value(value: &Value, key: &str) -> bool {
    value.get(key).is_some_and(|value| !value.is_null())
}

// Only known text-bearing fields contribute plaintext. Provider identifiers,
// signatures, and arbitrary nested metadata must never become token text.
fn collect_native_text<'a>(value: &'a Value, texts: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => texts.push(text),
        Value::Array(parts) => {
            for part in parts {
                collect_native_text(part, texts);
            }
        }
        Value::Object(part) => {
            for key in ["text", "thinking", "summary"] {
                if let Some(value) = part.get(key) {
                    collect_native_text(value, texts);
                }
            }
        }
        _ => {}
    }
}

/// Protocol-discriminated replay payload; providers cannot attach another
/// transport's native fields accidentally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", content = "payload", rename_all = "kebab-case")]
pub enum NativeReplayPayload {
    OpenAiChat(ChatNativeReplay),
    AnthropicMessages(AnthropicNativeReplay),
    CodexResponses(CodexNativeReplay),
}

/// Persistable native data belonging to one response and provider wire scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeReplayRecord {
    pub response_id: ResponseId,
    pub provider_scope: String,
    pub payload: NativeReplayPayload,
}

/// Session-owned native responses, independent of context retention/provider
/// instances. Live and omitted messages refer to this store by response UUID.
#[derive(Debug, Clone, Default)]
pub struct NativeReplayStore {
    records: Arc<Mutex<HashMap<(ResponseId, String), NativeReplayPayload>>>,
}

impl NativeReplayStore {
    fn lock(&self) -> MutexGuard<'_, HashMap<(ResponseId, String), NativeReplayPayload>> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Record one immutable response; a duplicate is a caller identity error.
    ///
    /// # Errors
    /// Rejects overwrites, including identical payloads, to expose ID reuse.
    pub fn insert(&self, record: NativeReplayRecord) -> ProviderResult<()> {
        let mut records = self.lock();
        let key = (record.response_id, record.provider_scope);
        match records.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(record.payload);
                Ok(())
            }
            std::collections::hash_map::Entry::Occupied(_) => Err(ProviderFailure::configuration(
                "native response identity was reused",
            )),
        }
    }

    /// Snapshot in stable order for atomic session persistence and signatures.
    pub fn snapshot(&self) -> Vec<NativeReplayRecord> {
        let mut records = self
            .lock()
            .iter()
            .map(
                |((response_id, provider_scope), payload)| NativeReplayRecord {
                    response_id: response_id.clone(),
                    provider_scope: provider_scope.clone(),
                    payload: payload.clone(),
                },
            )
            .collect::<Vec<_>>();
        records.sort_unstable_by(|left, right| {
            (&left.response_id, &left.provider_scope)
                .cmp(&(&right.response_id, &right.provider_scope))
        });
        records
    }

    /// Restore a complete session snapshot without partially replacing state.
    ///
    /// # Errors
    /// Rejects duplicate identities in a corrupt snapshot.
    pub fn restore(&self, snapshot: &[NativeReplayRecord]) -> ProviderResult<()> {
        let mut records = HashMap::with_capacity(snapshot.len());
        for record in snapshot {
            if records
                .insert(
                    (record.response_id.clone(), record.provider_scope.clone()),
                    record.payload.clone(),
                )
                .is_some()
            {
                return Err(ProviderFailure::configuration(
                    "duplicate native response identity in session snapshot",
                ));
            }
        }
        *self.lock() = records;
        Ok(())
    }

    /// Start a new durable session after staging the outgoing snapshot.
    pub fn clear(&self) {
        self.lock().clear();
    }

    /// Select immutable protocol data by response UUID and exact wire scope.
    /// The owned copy becomes part of the next request; absent provenance never
    /// selects data by visible text or tool correlator.
    pub(crate) fn payload_for(
        &self,
        scope: &str,
        response_id: &ResponseId,
    ) -> Option<NativeReplayPayload> {
        self.lock()
            .get(&(response_id.clone(), scope.to_string()))
            .cloned()
    }

    /// Select settings without cloning the response's potentially large blocks.
    pub(crate) fn anthropic_settings_for(
        &self,
        scope: &str,
        response_id: &ResponseId,
    ) -> Option<AnthropicReplaySettings> {
        match self.lock().get(&(response_id.clone(), scope.to_string())) {
            Some(NativeReplayPayload::AnthropicMessages(native)) => native.request_settings.clone(),
            _ => None,
        }
    }

    /// Attribute only records selected by this request's UUIDs and exact scope.
    pub(crate) fn selected_budgets(
        &self,
        scope: &str,
        context: &ProviderRequestContext,
        protocol: super::Protocol,
    ) -> Vec<NativeBudgetAttribution> {
        let records = self.lock();
        context
            .assistant_response_ids
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .enumerate()
            .filter_map(|(slot, id)| {
                let payload = records.get(&(id.as_ref()?.clone(), scope.to_string()))?;
                if !matches!(
                    (protocol, payload),
                    (
                        super::Protocol::OpenAiChat,
                        NativeReplayPayload::OpenAiChat(_)
                    ) | (
                        super::Protocol::AnthropicMessages,
                        NativeReplayPayload::AnthropicMessages(_)
                    ) | (
                        super::Protocol::CodexResponses,
                        NativeReplayPayload::CodexResponses(_)
                    )
                ) {
                    return None;
                }
                Some(payload.budget_attribution(slot))
            })
            .collect()
    }

    /// Attach known native fields before restoring wire tool correlators.
    ///
    /// # Errors
    /// Rejects sidecar/message misalignment. Missing identities or records do
    /// not attach any data and never fall back to visible-content matching.
    pub fn project_chat_request(
        &self,
        scope: &str,
        context: &ProviderRequestContext,
        body: &mut Value,
    ) -> ProviderResult<()> {
        let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
            context.validate_assistant_count(0)?;
            return Ok(());
        };
        let count = messages
            .iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
            .count();
        context.validate_assistant_count(count)?;
        let Some(ids) = &context.assistant_response_ids else {
            return Ok(());
        };
        let records = self.lock();
        for (message, id) in messages
            .iter_mut()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
            .zip(ids)
        {
            let Some(id) = id else { continue };
            let Some(NativeReplayPayload::OpenAiChat(payload)) =
                records.get(&(id.clone(), scope.to_string()))
            else {
                continue;
            };
            if let Some(reasoning) = &payload.reasoning_content {
                message["reasoning_content"] = Value::String(reasoning.clone());
            }
            if let Some(parts) = &payload.content_parts {
                message["content"] = Value::Array(parts.clone());
            }
            if let Some(details) = &payload.reasoning_details {
                message["reasoning_details"] = Value::Array(details.clone());
            }
            if let Some(reasoning) = &payload.legacy_reasoning {
                message["reasoning"] = Value::String(reasoning.clone());
            }
            if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for call in calls {
                    if let Some(extra) = call
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| payload.tool_extra_content.get(id))
                    {
                        call["extra_content"] = extra.clone();
                    }
                    if let Some(original) = call
                        .get("id")
                        .and_then(Value::as_str)
                        .and_then(|id| payload.tool_functions.get(id))
                        && let Some(function) =
                            call.get_mut("function").and_then(Value::as_object_mut)
                    {
                        function.insert("name".into(), Value::String(original.name.clone()));
                        function.insert(
                            "arguments".into(),
                            Value::String(original.arguments.clone()),
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(id: ResponseId, scope: &str, reasoning: &str) -> NativeReplayRecord {
        NativeReplayRecord {
            response_id: id,
            provider_scope: scope.into(),
            payload: NativeReplayPayload::OpenAiChat(ChatNativeReplay {
                reasoning_content: Some(reasoning.into()),
                ..ChatNativeReplay::default()
            }),
        }
    }

    #[test]
    fn native_budget_attributes_structured_thinking_without_opaque_metadata() {
        let native = NativeReplayPayload::OpenAiChat(ChatNativeReplay {
            content_parts: Some(vec![json!({
                "type":"thinking",
                "thinking":[
                    {"type":"text","text":"first thought","signature":"do not count"},
                    {"type":"text","text":"second thought","data":"do not count"}
                ],
                "signature":null
            })]),
            reasoning_details: Some(vec![json!({
                "type":"reasoning.summary",
                "summary":[{"text":"nested summary","data":"do not count"}],
                "data":null,"signature":null
            })]),
            ..ChatNativeReplay::default()
        });
        let attribution = native.budget_attribution(3);
        assert_eq!(attribution.assistant_slot, 3);
        assert_eq!(
            attribution.reasoning_text,
            "nested summary\nfirst thought\nsecond thought"
        );
        assert!(!attribution.opaque);
        assert_eq!(attribution.opaque_allowance_tokens, None);
    }

    #[test]
    fn native_budget_preserves_zero_and_counts_multiple_opaque_blocks_once() {
        for (evidence, expected) in [
            (
                NativeBudgetEvidence {
                    output_tokens: Some(0),
                    output_ceiling_tokens: Some(100),
                },
                Some(0),
            ),
            (
                NativeBudgetEvidence {
                    output_tokens: None,
                    output_ceiling_tokens: Some(100),
                },
                Some(100),
            ),
            (NativeBudgetEvidence::default(), None),
        ] {
            let native = NativeReplayPayload::OpenAiChat(ChatNativeReplay {
                budget_evidence: Some(evidence),
                reasoning_details: Some(vec![
                    json!({"type":"reasoning.text","text":"visible","data":null,"signature":""}),
                    json!({"type":"reasoning.encrypted","data":"ciphertext"}),
                ]),
                ..ChatNativeReplay::default()
            });
            let restored: NativeReplayPayload =
                serde_json::from_value(serde_json::to_value(&native).unwrap()).unwrap();
            let attribution = restored.budget_attribution(0);
            assert!(attribution.opaque);
            assert_eq!(attribution.opaque_allowance_tokens, expected);
            assert_eq!(attribution.reasoning_text, "visible");
        }
    }

    #[test]
    fn native_budget_uses_only_selected_identity_scope_and_transport() {
        let store = NativeReplayStore::default();
        let id = ResponseId::new();
        store
            .insert(record(id.clone(), "source", "selected thought"))
            .unwrap();
        let context = ProviderRequestContext {
            assistant_response_ids: Some(vec![Some(id.clone()), None, Some(id)]),
            response_id: None,
        };
        assert!(
            store
                .selected_budgets("other", &context, super::super::Protocol::OpenAiChat)
                .is_empty()
        );
        assert!(
            store
                .selected_budgets(
                    "source",
                    &context,
                    super::super::Protocol::AnthropicMessages
                )
                .is_empty()
        );
        let budgets =
            store.selected_budgets("source", &context, super::super::Protocol::OpenAiChat);
        assert_eq!(
            budgets
                .iter()
                .map(|budget| budget.assistant_slot)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert!(
            budgets
                .iter()
                .all(|budget| budget.reasoning_text == "selected thought")
        );
    }

    #[test]
    fn retained_repeated_answers_select_only_their_immutable_provenance() {
        let store = NativeReplayStore::default();
        let old = ResponseId::new();
        let recent = ResponseId::new();
        store
            .insert(record(old.clone(), "scope", "old trace"))
            .unwrap();
        store
            .insert(record(recent.clone(), "scope", "recent trace"))
            .unwrap();
        for (id, trace) in [(old.clone(), "old trace"), (recent.clone(), "recent trace")] {
            let mut body = json!({"messages":[{"role":"assistant","content":"same answer"}]});
            store
                .project_chat_request(
                    "scope",
                    &ProviderRequestContext {
                        assistant_response_ids: Some(vec![Some(id)]),
                        response_id: None,
                    },
                    &mut body,
                )
                .unwrap();
            assert_eq!(body["messages"][0]["reasoning_content"], trace);
        }
        let mut body = json!({"messages":[
            {"role":"assistant","content":"same answer"},
            {"role":"user","content":"next"},
            {"role":"assistant","content":"same answer"},
            {"role":"assistant","content":"same answer"}
        ]});
        store
            .project_chat_request(
                "scope",
                &ProviderRequestContext {
                    assistant_response_ids: Some(vec![Some(recent), None, Some(old)]),
                    response_id: None,
                },
                &mut body,
            )
            .unwrap();
        assert_eq!(body["messages"][0]["reasoning_content"], "recent trace");
        assert!(body["messages"][2].get("reasoning_content").is_none());
        assert_eq!(body["messages"][3]["reasoning_content"], "old trace");
    }

    #[test]
    fn legacy_missing_wrong_scope_and_misaligned_provenance_never_guess() {
        let store = NativeReplayStore::default();
        let id = ResponseId::new();
        store
            .insert(record(id.clone(), "source", "private trace"))
            .unwrap();
        let original = json!({"messages":[{"role":"assistant","content":"same answer"}]});
        for context in [
            ProviderRequestContext::default(),
            ProviderRequestContext {
                assistant_response_ids: Some(vec![Some(id)]),
                response_id: None,
            },
        ] {
            let mut body = original.clone();
            store
                .project_chat_request("other", &context, &mut body)
                .unwrap();
            assert_eq!(body, original);
        }
        let mut body = original.clone();
        let error = store
            .project_chat_request(
                "source",
                &ProviderRequestContext {
                    assistant_response_ids: Some(vec![]),
                    response_id: None,
                },
                &mut body,
            )
            .unwrap_err();
        assert!(error.to_string().contains("0 assistant entries for 1 rows"));
        assert_eq!(body, original);
    }

    #[test]
    fn snapshot_rebuild_preserves_opaque_fields_and_present_empty_channels() {
        let id = ResponseId::new();
        let opaque =
            json!({"type":"reasoning.encrypted","data":"signed==","unknown":{"id":"opaque"}});
        let extra = json!({"google":{"thought_signature":"exact=="}});
        let store = NativeReplayStore::default();
        store
            .insert(NativeReplayRecord {
                response_id: id.clone(),
                provider_scope: "scope".into(),
                payload: NativeReplayPayload::OpenAiChat(ChatNativeReplay {
                    reasoning_content: Some(String::new()),
                    reasoning_details: Some(vec![opaque.clone()]),
                    legacy_reasoning: Some("exact legacy field".into()),
                    tool_extra_content: BTreeMap::from([("canonical".into(), extra.clone())]),
                    ..ChatNativeReplay::default()
                }),
            })
            .unwrap();
        let bytes = serde_json::to_vec(&store.snapshot()).unwrap();
        let restored = NativeReplayStore::default();
        restored
            .restore(&serde_json::from_slice::<Vec<NativeReplayRecord>>(&bytes).unwrap())
            .unwrap();
        let mut body = json!({"messages":[{"role":"assistant","content":"same answer","tool_calls":[{"id":"canonical","function":{"arguments":"opaque"}}]}]});
        restored
            .project_chat_request(
                "scope",
                &ProviderRequestContext {
                    assistant_response_ids: Some(vec![Some(id)]),
                    response_id: None,
                },
                &mut body,
            )
            .unwrap();
        assert_eq!(body["messages"][0]["reasoning_content"], "");
        assert_eq!(body["messages"][0]["reasoning_details"], json!([opaque]));
        assert_eq!(body["messages"][0]["reasoning"], "exact legacy field");
        assert_eq!(body["messages"][0]["tool_calls"][0]["extra_content"], extra);
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["arguments"],
            "opaque"
        );
        assert_eq!(restored.snapshot(), store.snapshot());
    }

    #[test]
    fn identity_reuse_and_corrupt_restore_cannot_overwrite_an_existing_trace() {
        let store = NativeReplayStore::default();
        let original = record(ResponseId::new(), "scope", "original");
        store.insert(original.clone()).unwrap();
        let duplicate = record(original.response_id.clone(), "scope", "replacement");
        assert!(store.insert(duplicate.clone()).is_err());
        assert!(store.restore(&[original.clone(), duplicate]).is_err());
        assert_eq!(store.snapshot(), vec![original]);
        assert!(serde_json::from_value::<ResponseId>(json!("msg-0")).is_err());
        assert!(serde_json::from_value::<ChatNativeReplay>(json!({"role":"system"})).is_err());
        store.clear();
        assert!(store.snapshot().is_empty());
    }

    #[test]
    fn opaque_call_replay_retains_original_function_strings_after_snapshot_restore() {
        let id = ResponseId::new();
        let store = NativeReplayStore::default();
        let original = NativeToolFunction {
            name: "read".into(),
            arguments: "{malformed π".into(),
        };
        store
            .insert(NativeReplayRecord {
                response_id: id.clone(),
                provider_scope: "scope".into(),
                payload: NativeReplayPayload::OpenAiChat(ChatNativeReplay {
                    tool_extra_content: BTreeMap::from([(
                        "canonical".into(),
                        json!({"signature":"exact=="}),
                    )]),
                    tool_functions: BTreeMap::from([("canonical".into(), original.clone())]),
                    ..Default::default()
                }),
            })
            .unwrap();
        let restored = NativeReplayStore::default();
        restored
            .restore(
                &serde_json::from_slice::<Vec<NativeReplayRecord>>(
                    &serde_json::to_vec(&store.snapshot()).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        let normalized = json!({"messages":[{"role":"assistant","tool_calls":[{
            "id":"canonical","function":{"name":"read","arguments":"{}"}
        }]}]});
        let mut body = normalized.clone();
        restored
            .project_chat_request(
                "scope",
                &ProviderRequestContext {
                    assistant_response_ids: Some(vec![Some(id)]),
                    response_id: None,
                },
                &mut body,
            )
            .unwrap();
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["function"]["arguments"],
            original.arguments
        );
        assert_eq!(
            body["messages"][0]["tool_calls"][0]["extra_content"],
            json!({"signature":"exact=="})
        );
        let bytes = serde_json::to_vec(&body).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), body);
        let mut missing = normalized.clone();
        restored
            .project_chat_request("scope", &ProviderRequestContext::default(), &mut missing)
            .unwrap();
        assert_eq!(missing, normalized);
    }
}
