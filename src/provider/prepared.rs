//! The pure provider request boundary used by counting and transmission.

use serde_json::{Map, Value};

use super::{NativeBudgetAttribution, Protocol, ReasoningSelection};

/// Prepared protocol data. Provenance and budget annotations are never added
/// to the submitted JSON body; preparation does not authorize or issue IO.
#[derive(Debug)]
pub struct PreparedProviderRequest {
    pub protocol: Option<Protocol>,
    pub body: Value,
    pub reasoning: ReasoningSelection,
    pub native_budget: Vec<NativeBudgetAttribution>,
    pub count_endpoint: Option<String>,
    pub count_auth_header: Option<String>,
}

impl PreparedProviderRequest {
    /// Identify the actual prompt and native allowances for safe count reuse.
    /// No opaque payload or credential is exposed by the resulting digest.
    ///
    /// # Errors
    /// Returns JSON serialization failures.
    pub(crate) fn estimate_fingerprint(&self) -> Result<blake3::Hash, serde_json::Error> {
        let mut hasher = blake3::Hasher::new();
        serde_json::to_writer(
            &mut hasher,
            &(self.protocol, &self.body, &self.native_budget),
        )?;
        Ok(hasher.finalize())
    }

    /// Image inputs cannot be exactly counted from their encoded wire bytes.
    pub fn has_images(&self) -> bool {
        ["messages", "input"]
            .into_iter()
            .filter_map(|key| self.body.get(key).and_then(Value::as_array))
            .flatten()
            .filter_map(|message| message.get("content").and_then(Value::as_array))
            .flatten()
            .any(|part| {
                matches!(
                    part["type"].as_str(),
                    Some("image" | "image_url" | "input_image")
                )
            })
    }
    /// Fields accepted by Anthropic's counting API, copied from the same body
    /// used for streaming. Signed/redacted native blocks remain unchanged.
    pub fn anthropic_count_body(&self) -> Value {
        let mut body = Map::new();
        for key in [
            "model",
            "system",
            "messages",
            "tools",
            "thinking",
            "output_config",
            "tool_choice",
            "cache_control",
        ] {
            if let Some(value) = self.body.get(key) {
                body.insert(key.into(), value.clone());
            }
        }
        Value::Object(body)
    }

    /// Local counting keeps prompt data and removes opaque payload bytes at
    /// protocol-defined positions. Their allowance comes from native evidence.
    pub fn local_count_body(&self) -> Value {
        let mut body = Map::new();
        for key in [
            "instructions",
            "system",
            "messages",
            "input",
            "tools",
            "additional_tools",
            "thinking",
            "output_config",
            "tool_choice",
            "response_format",
        ] {
            if let Some(value) = self.body.get(key) {
                body.insert(key.into(), value.clone());
            }
        }
        let mut body = Value::Object(body);
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                if self.protocol == Some(Protocol::OpenAiChat) {
                    if let Some(details) = message
                        .get_mut("reasoning_details")
                        .and_then(Value::as_array_mut)
                    {
                        for detail in details {
                            if let Some(object) = detail.as_object_mut() {
                                object.retain(|key, _| {
                                    matches!(key.as_str(), "type" | "text" | "summary" | "format")
                                });
                            }
                        }
                    }
                    if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut)
                    {
                        for call in calls {
                            if call.get("extra_content").is_some() {
                                call["extra_content"] = Value::Null;
                            }
                        }
                    }
                }
                if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                    for part in parts {
                        match part["type"].as_str() {
                            Some("thinking" | "reasoning") => {
                                redact_fields(part, &["signature", "encrypted_content"])
                            }
                            Some("redacted_thinking") => redact_fields(part, &["data"]),
                            Some("image") => {
                                if let Some(source) = part.get_mut("source") {
                                    redact_fields(source, &["data", "url"]);
                                }
                            }
                            Some("image_url") => {
                                if let Some(image) = part.get_mut("image_url") {
                                    redact_fields(image, &["url"]);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
            for item in items {
                if item["type"] == "reasoning" {
                    redact_fields(item, &["encrypted_content"]);
                }
                if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
                    for part in parts {
                        if part["type"] == "input_image" {
                            redact_fields(part, &["image_url"]);
                        }
                    }
                }
            }
        }
        body
    }
}

fn redact_fields(value: &mut Value, fields: &[&str]) {
    if let Some(object) = value.as_object_mut() {
        for field in fields {
            if let Some(value) = object.get_mut(*field) {
                *value = Value::Null;
            }
        }
    }
}
