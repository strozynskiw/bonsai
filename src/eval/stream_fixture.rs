//! Versioned, allowlisted provider-stream artifacts. No wire payloads or credentials.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::provider::{ProviderRequestContext, ProviderRequestOptions, ToolCallIdentityStore};

pub(super) const STREAM_FIXTURE_VERSION: u32 = 1;
pub(super) const MAX_FIXTURE_BYTES: usize = 32 * 1024 * 1024;
pub(super) const MAX_ATTEMPT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StreamHeader {
    pub version: u32,
    pub suite: String,
    pub task: String,
    pub seed: u64,
    pub provider: String,
    pub model: String,
    pub context_tokens: usize,
    pub platform: String,
    pub preparation: crate::provider::opencode::PortableChatPreparation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LogicalRequest {
    pub messages: Value,
    pub tools: Value,
    pub options: Value,
    pub context: Value,
    pub identities: Value,
    pub local_count: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "channel",
    content = "text",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum StreamEvent {
    Assistant(String),
    Reasoning(String),
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordedToolCall {
    pub id: String,
    pub wire_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordedUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub input_cache: Option<[u64; 3]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum RecordedOutcome {
    Response {
        content: String,
        tool_calls: Vec<RecordedToolCall>,
        terminal: crate::provider::StreamTerminal,
        usage: Option<RecordedUsage>,
        reasoning_chars: usize,
    },
    Failure {
        class: FailureClass,
        message: String,
        status: Option<u16>,
        code: String,
        retry_after_secs: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FailureClass {
    Http,
    Transport,
    Decode,
    Configuration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StreamAttempt {
    pub index: usize,
    pub request: LogicalRequest,
    pub events: Vec<StreamEvent>,
    pub outcome: RecordedOutcome,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "record",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(super) enum StreamRecord {
    Header(StreamHeader),
    Attempt(StreamAttempt),
    Complete { attempts: usize },
}

/// Only declared roots and allocated identities are portable. No UUID/path regexes.
#[derive(Debug, Clone, Default)]
pub(super) struct Normalizer {
    roots: Vec<(String, String)>,
    calls: BTreeMap<String, String>,
    responses: BTreeMap<String, String>,
}

impl Normalizer {
    pub fn new(workspace: &Path, output: &Path) -> Result<Self> {
        let mut roots = vec![
            (
                workspace.to_str().context("non-UTF8 workspace")?.to_owned(),
                "@EVAL_WORKSPACE@".into(),
            ),
            (
                output.to_str().context("non-UTF8 output")?.to_owned(),
                "@EVAL_OUTPUT@".into(),
            ),
        ];
        roots.sort_by_key(|(root, _)| std::cmp::Reverse(root.len()));
        Ok(Self {
            roots,
            ..Self::default()
        })
    }

    pub fn call(&mut self, id: &str) -> String {
        let next = self.calls.len();
        self.calls
            .entry(id.to_owned())
            .or_insert_with(|| format!("@CALL_{next}@"))
            .clone()
    }

    fn response(&mut self, id: &crate::provider::ResponseId) -> Result<String> {
        let id = serde_json::to_value(id)?
            .as_str()
            .context("invalid response ID")?
            .to_owned();
        let next = self.responses.len();
        Ok(self
            .responses
            .entry(id)
            .or_insert_with(|| format!("@RESPONSE_{next}@"))
            .clone())
    }

    pub fn text(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for (root, marker) in &self.roots {
            text = replace_delimited(&text, root, marker, true);
        }
        for (id, marker) in &self.calls {
            text = replace_delimited(&text, id, marker, false);
        }
        text
    }

    pub fn rebase(&self, text: &str) -> String {
        self.roots
            .iter()
            .fold(text.to_owned(), |text, (root, marker)| {
                text.replace(marker, root)
            })
    }

    pub fn rebase_arguments(&self, text: &str) -> String {
        fn visit(normalizer: &Normalizer, value: &mut Value) {
            match value {
                Value::String(text) => *text = normalizer.rebase(text),
                Value::Array(values) => {
                    values.iter_mut().for_each(|value| visit(normalizer, value))
                }
                Value::Object(values) => values
                    .values_mut()
                    .for_each(|value| visit(normalizer, value)),
                _ => {}
            }
        }
        if let Ok(mut value) = serde_json::from_str::<Value>(text)
            && (value.is_object() || value.is_array())
        {
            visit(self, &mut value);
            value.to_string()
        } else {
            self.rebase(text)
        }
    }

    pub fn value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.text(text),
            Value::Array(values) => values.iter_mut().for_each(|value| self.value(value)),
            Value::Object(values) => values.values_mut().for_each(|value| self.value(value)),
            _ => {}
        }
    }

    pub fn arguments(&self, text: &str) -> String {
        if let Ok(mut value) = serde_json::from_str::<Value>(text)
            && (value.is_object() || value.is_array())
        {
            self.value(&mut value);
            value.to_string()
        } else {
            self.text(text)
        }
    }

    /// Parse only the protocol's tool-argument envelopes, never source text.
    pub fn request_value(&self, value: &mut Value) {
        let messages = if value.is_array() {
            value.as_array_mut()
        } else {
            value.get_mut("messages").and_then(Value::as_array_mut)
        };
        if let Some(messages) = messages {
            for message in messages {
                if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                    for call in calls {
                        if let Some(Value::String(arguments)) =
                            call.pointer_mut("/function/arguments")
                        {
                            *arguments = self.arguments(arguments);
                        }
                    }
                }
            }
        }
        self.value(value);
    }

    pub fn request(
        &mut self,
        messages: &[async_openai::types::chat::ChatCompletionRequestMessage],
        tools: &[async_openai::types::chat::ChatCompletionTool],
        options: ProviderRequestOptions,
        context: &ProviderRequestContext,
        identities: &ToolCallIdentityStore,
        local_count: Value,
    ) -> Result<LogicalRequest> {
        let mut messages = serde_json::to_value(messages)?;
        // Bind IDs in conversational order, not UUID/snapshot sort order.
        for message in messages.as_array().context("messages not array")? {
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    self.call(call["id"].as_str().context("tool call missing ID")?);
                }
            }
        }
        let assistant_ids = context
            .assistant_response_ids
            .as_ref()
            .map(|ids| {
                ids.iter()
                    .map(|id| id.as_ref().map(|id| self.response(id)).transpose())
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?;
        let response_id = context
            .response_id
            .as_ref()
            .map(|id| self.response(id))
            .transpose()?;
        let records = identities.snapshot();
        let mut relationships = Vec::new();
        for (canonical, label) in &self.calls {
            if let Some(record) = records
                .iter()
                .find(|record| &record.canonical_id == canonical)
            {
                relationships.push(json!({"id": label, "wire_id": record.wire_id,
                    "sources": record.reuse_source_call_ids.iter().map(|id| self.calls.get(id).cloned().unwrap_or_else(|| id.clone())).collect::<Vec<_>>() }));
            }
        }
        relationships.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        self.request_value(&mut messages);
        let mut tools = serde_json::to_value(tools)?;
        self.value(&mut tools);
        let mut local_count = local_count;
        self.request_value(&mut local_count);
        let request = LogicalRequest {
            messages,
            tools,
            local_count,
            options: json!({"reasoning": options.reasoning.map(|value| value.to_string())}),
            context: json!({"assistant_response_ids": assistant_ids, "response_id": response_id}),
            identities: Value::Array(relationships),
        };
        ensure_portable(&serde_json::to_value(&request)?)?;
        Ok(request)
    }
}

fn replace_delimited(text: &str, needle: &str, replacement: &str, root: bool) -> String {
    if needle.is_empty() {
        return text.to_owned();
    }
    let mut result = String::with_capacity(text.len());
    let mut start = 0;
    for (index, _) in text.match_indices(needle) {
        let end = index + needle.len();
        let before = text[..index].chars().next_back();
        let after = text[end..].chars().next();
        let delimiter = |c: char| !c.is_alphanumeric() && !matches!(c, '_' | '-' | '.' | '/' | '@');
        if before.is_none_or(delimiter) && after.is_none_or(|c| delimiter(c) || (root && c == '/'))
        {
            result.push_str(&text[start..index]);
            result.push_str(replacement);
            start = end;
        }
    }
    result.push_str(&text[start..]);
    result
}

/// Fail closed if sanitization would alter replay semantics. Walk JSON strings too.
pub(super) fn ensure_portable(value: &Value) -> Result<()> {
    match value {
        Value::String(text) => {
            if crate::redact::redact(text).as_ref() != text {
                bail!("fixture contains credential-shaped text");
            }
            if let Ok(nested) = serde_json::from_str::<Value>(text)
                && (nested.is_object() || nested.is_array())
            {
                ensure_portable(&nested)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                let key = key.to_ascii_lowercase().replace('-', "_");
                if matches!(
                    key.as_str(),
                    "authorization"
                        | "api_key"
                        | "apikey"
                        | "password"
                        | "secret"
                        | "access_token"
                        | "refresh_token"
                        | "signature"
                        | "encrypted_content"
                ) {
                    bail!("fixture contains excluded credential/native field");
                }
                if matches!(
                    key.as_str(),
                    "image_url" | "input_image" | "reasoning_details" | "reasoning_content"
                ) {
                    bail!("fixture contains unsupported image/native history");
                }
                ensure_portable(value)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                ensure_portable(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Return a structural location only; never echo mismatched confidential data.
pub(super) fn first_difference(left: &Value, right: &Value, path: &str) -> Option<String> {
    if left == right {
        return None;
    }
    match (left, right) {
        (Value::Object(a), Value::Object(b)) if a.keys().eq(b.keys()) => {
            a.iter().enumerate().find_map(|(index, (key, value))| {
                first_difference(value, &b[key], &format!("{path}.field{index}"))
            })
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => a
            .iter()
            .zip(b)
            .enumerate()
            .find_map(|(index, (a, b))| first_difference(a, b, &format!("{path}[{index}]"))),
        _ => Some(path.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_declared_roots_and_id_boundaries_change() {
        let mut normalizer =
            Normalizer::new(Path::new("/tmp/work"), Path::new("/tmp/out")).unwrap();
        normalizer.call("call_123");
        assert_eq!(
            normalizer.text("/tmp/work/a call_123 /tmp/work-other /other/path xcall_123"),
            "@EVAL_WORKSPACE@/a @CALL_0@ /tmp/work-other /other/path xcall_123"
        );
        assert_eq!(normalizer.rebase("@EVAL_WORKSPACE@/a"), "/tmp/work/a");
    }

    #[test]
    fn recursively_rejects_credentials_and_native_payloads() {
        for value in [
            json!({"args":"{\"password\":\"innocent-looking\"}"}),
            json!({"nested":[{"authorization":"hidden"}]}),
            json!(format!("sk-{}", "x".repeat(40))),
            json!({"signature":"opaque"}),
        ] {
            assert!(ensure_portable(&value).is_err());
        }
        assert!(ensure_portable(&json!({"arguments":"{\"path\":\"src/main.rs\"}"})).is_ok());
    }

    #[test]
    fn differences_never_include_values() {
        assert_eq!(
            first_difference(
                &json!({"tools":["private"]}),
                &json!({"tools":["other"]}),
                "$"
            ),
            Some("$.field0[0]".into())
        );
    }
}
