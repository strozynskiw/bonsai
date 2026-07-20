//! Shared request-shaping helpers for the message-based providers.
//!
//! Anthropic and Codex both translate the OpenAI-style request
//! (`ChatCompletionRequestMessage` / `ChatCompletionTool`) into their own JSON,
//! and the *extraction* half of that work is identical — only the output JSON
//! shape differs. These helpers own the extraction; each provider maps the
//! normalized values to its protocol shape.

use std::borrow::Cow;
use std::collections::HashMap;

use anyhow::{Context, Result};
use async_openai::types::chat::{
    ChatCompletionRequestDeveloperMessage, ChatCompletionRequestDeveloperMessageContent,
    ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent, ChatCompletionTool,
};
use serde_json::{Value, json};

/// Flatten an OpenAI message `content` field (string or array of parts) to its
/// plain text, joining array text parts with newlines. Returns `None` when
/// there is no text.
pub(crate) fn content_to_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

/// A protocol-neutral piece of user content. Each provider renders these into
/// its own JSON (`text`/`image` for Anthropic, `input_text`/`input_image` for
/// Codex).
pub(crate) enum ContentPart {
    Text(String),
    ImageUrl(String),
}

/// Normalize an OpenAI message `content` field into ordered [`ContentPart`]s.
pub(crate) fn content_parts(content: Option<&Value>) -> Vec<ContentPart> {
    match content {
        Some(Value::String(text)) => vec![ContentPart::Text(text.clone())],
        Some(Value::Array(items)) => items.iter().filter_map(content_part).collect(),
        _ => Vec::new(),
    }
}

/// Provider-wire placement for Bonsai's immutable project-state snapshots.
///
/// IMPORTANT CACHE COMPATIBILITY CONTRACT: select this at the transport
/// boundary. Codex Responses needs the complete append-only history for its
/// automatic latest-message breakpoint. Chat Completions and Anthropic retain
/// their previously proven layouts so Qwen, GLM, Claude, and compatible
/// gateways see the same cache shape they did before snapshots became durable.
/// Do not replace these policies with one universal message rewrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectStateWireLayout {
    /// Preserve every snapshot at its historical position (Codex Responses).
    AppendOnly,
    /// Keep only the latest snapshot as the final unnamed user message
    /// (OpenAI Chat Completions and compatible gateways).
    LatestAtEnd,
    /// Fold only the latest snapshot back into the system volatile tail
    /// (Anthropic Messages, whose explicit cache breakpoint splits that tail).
    SystemTail,
}

/// Shape durable project-state history for a provider's proven cache layout.
///
/// The append-only path borrows without cloning. Compatibility layouts own a
/// rewritten wire-only copy; persisted conversation history is never mutated.
pub(crate) fn messages_for_project_state_layout<'a>(
    messages: &'a [ChatCompletionRequestMessage],
    layout: ProjectStateWireLayout,
) -> Cow<'a, [ChatCompletionRequestMessage]> {
    match layout {
        ProjectStateWireLayout::AppendOnly => Cow::Borrowed(messages),
        ProjectStateWireLayout::LatestAtEnd => {
            Cow::Owned(messages_with_latest_project_state_at_end(messages))
        }
        ProjectStateWireLayout::SystemTail => {
            Cow::Owned(messages_with_latest_project_state_in_system(messages))
        }
    }
}

/// Demote every non-leading `system` message to a named `user` message.
///
/// Strict OpenAI-compatible chat templates `raise_exception` unless every
/// system message sits at index 0 — Qwen3's Jinja template does exactly this
/// (`{% if ... %}{{- raise_exception('System message must be at the beginning')`).
/// Bonsai injects trusted harness notes as mid-conversation system messages
/// (e.g. the self-review nudge armed before a coding task finishes), which such
/// templates reject with an HTTP 400. Anthropic sidesteps this by hoisting all
/// system content into its top-level `system` field; the OpenAI-compatible
/// transport has no such field, so it must fold non-leading system turns into
/// the user stream instead.
///
/// The leading system prompt is left untouched, so the cache prefix stays
/// byte-stable, and the `Harness note:` envelope plus the carried `name`
/// (`bonsai_harness`) preserve provenance for the model. Returns `Borrowed`
/// when there is nothing to demote, keeping the common wire body byte-identical.
pub(crate) fn demote_non_leading_system_messages(
    messages: &[ChatCompletionRequestMessage],
) -> Cow<'_, [ChatCompletionRequestMessage]> {
    let has_non_leading_system = messages
        .iter()
        .skip(1)
        .any(|message| matches!(message, ChatCompletionRequestMessage::System(_)));
    if !has_non_leading_system {
        return Cow::Borrowed(messages);
    }
    let demoted = messages
        .iter()
        .enumerate()
        .map(|(index, message)| match message {
            ChatCompletionRequestMessage::System(system) if index > 0 => {
                // Reuse the untyped flattener so both string and array system
                // content round-trip to plain user text.
                let text = serde_json::to_value(&system.content)
                    .ok()
                    .and_then(|value| content_to_text(Some(&value)))
                    .unwrap_or_default();
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: text.into(),
                    name: system.name.clone(),
                })
            }
            other => other.clone(),
        })
        .collect();
    Cow::Owned(demoted)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VolatileSystemSplit {
    pub(crate) stable: String,
    pub(crate) volatile: String,
}

/// Split Bonsai's legacy system prompt at the volatile project-state boundary.
///
/// Resumed sessions written before append-only snapshots can still carry this
/// layout, so provider compatibility shaping must continue to understand it.
pub(crate) fn split_volatile_system_context(system: &str) -> Option<VolatileSystemSplit> {
    let needle = format!("\n\n{}\n", crate::context::VOLATILE_STATE_HEADING);
    let idx = system.rfind(&needle)?;
    let stable = system[..idx].trim_end();
    let volatile = system[idx..].trim_start();
    if stable.is_empty() || volatile.is_empty() {
        return None;
    }
    Some(VolatileSystemSplit {
        stable: stable.to_string(),
        volatile: volatile.to_string(),
    })
}

pub(crate) fn volatile_context_user_text(volatile: &str) -> String {
    format!(
        "{}\n\n{volatile}",
        crate::context::PROJECT_STATE_UPDATE_PREFIX
    )
}

/// Preserve the established Chat Completions cache layout.
///
/// IMPORTANT: Qwen/GLM on OpenCode were verified at roughly 95% warm reuse
/// with one current volatile snapshot after stable history. Durable snapshots
/// therefore collapse only in this wire copy; changing this to append-only for
/// every backend would silently alter a known-good provider contract.
fn messages_with_latest_project_state_at_end(
    messages: &[ChatCompletionRequestMessage],
) -> Vec<ChatCompletionRequestMessage> {
    let (mut filtered, latest_state) = without_project_state_messages(messages);
    let legacy_tail =
        filtered
            .first()
            .and_then(split_first_system_message)
            .map(|(stable, volatile)| {
                filtered[0] = stable;
                volatile
            });

    let current_tail = match latest_state.as_deref() {
        Some(state) => active_project_state_text(state).map(plain_user_message),
        None => legacy_tail,
    };
    if let Some(current_tail) = current_tail {
        filtered.push(current_tail);
    }
    filtered
}

/// Preserve the established Anthropic explicit-breakpoint layout.
fn messages_with_latest_project_state_in_system(
    messages: &[ChatCompletionRequestMessage],
) -> Vec<ChatCompletionRequestMessage> {
    let (mut filtered, latest_state) = without_project_state_messages(messages);
    let Some(latest_state) = latest_state else {
        return filtered;
    };
    let active_state = active_project_state_text(&latest_state);
    let Some(first) = filtered.first().cloned() else {
        return active_state.map(plain_user_message).into_iter().collect();
    };
    let stable_first = split_first_system_message(&first)
        .map(|(stable, _)| stable)
        .unwrap_or(first);
    match active_state.and_then(|state| system_message_with_volatile_tail(&stable_first, state)) {
        Some(system) => filtered[0] = system,
        None if active_state.is_none() => filtered[0] = stable_first,
        None => {
            // A non-text custom system message cannot safely absorb the state;
            // preserve the information as an ordinary user message instead.
            filtered.push(plain_user_message(active_state.unwrap_or_default()));
        }
    }
    filtered
}

fn without_project_state_messages(
    messages: &[ChatCompletionRequestMessage],
) -> (Vec<ChatCompletionRequestMessage>, Option<String>) {
    let mut latest = None;
    let filtered = messages
        .iter()
        .filter_map(|message| {
            if let Some(text) = project_state_message_text(message) {
                latest = Some(text.to_string());
                None
            } else {
                Some(message.clone())
            }
        })
        .collect();
    (filtered, latest)
}

fn project_state_message_text(message: &ChatCompletionRequestMessage) -> Option<&str> {
    let ChatCompletionRequestMessage::User(user) = message else {
        return None;
    };
    if user.name.as_deref() != Some(crate::context::PROJECT_STATE_MESSAGE_NAME) {
        return None;
    }
    match &user.content {
        ChatCompletionRequestUserMessageContent::Text(text) => Some(text),
        ChatCompletionRequestUserMessageContent::Array(_) => None,
    }
}

/// Strip the snapshot envelope, accepting the legacy prefix too — resumed
/// sessions persisted before the `Harness note:` envelope still carry it.
fn strip_project_state_prefix(message: &str) -> Option<&str> {
    message
        .strip_prefix(crate::context::PROJECT_STATE_UPDATE_PREFIX)
        .or_else(|| message.strip_prefix(crate::context::LEGACY_PROJECT_STATE_UPDATE_PREFIX))
}

fn active_project_state_text(message: &str) -> Option<&str> {
    let state = strip_project_state_prefix(message)?.trim_start();
    (state != crate::context::PROJECT_STATE_CLEARED_BODY).then_some(message)
}

fn project_state_body(message: &str) -> Option<&str> {
    let state = strip_project_state_prefix(message)?.trim_start();
    (state != crate::context::PROJECT_STATE_CLEARED_BODY).then_some(state)
}

fn split_first_system_message(
    message: &ChatCompletionRequestMessage,
) -> Option<(ChatCompletionRequestMessage, ChatCompletionRequestMessage)> {
    match message {
        ChatCompletionRequestMessage::System(system) => {
            let text = system_message_text(&system.content)?;
            let split = split_volatile_system_context(text)?;
            let stable = ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                content: ChatCompletionRequestSystemMessageContent::Text(split.stable),
                name: system.name.clone(),
            });
            Some((
                stable,
                plain_user_message(&volatile_context_user_text(&split.volatile)),
            ))
        }
        ChatCompletionRequestMessage::Developer(developer) => {
            let text = developer_message_text(&developer.content)?;
            let split = split_volatile_system_context(text)?;
            let stable =
                ChatCompletionRequestMessage::Developer(ChatCompletionRequestDeveloperMessage {
                    content: ChatCompletionRequestDeveloperMessageContent::Text(split.stable),
                    name: developer.name.clone(),
                });
            Some((
                stable,
                plain_user_message(&volatile_context_user_text(&split.volatile)),
            ))
        }
        _ => None,
    }
}

fn system_message_with_volatile_tail(
    message: &ChatCompletionRequestMessage,
    state_message: &str,
) -> Option<ChatCompletionRequestMessage> {
    let volatile = project_state_body(state_message)?;
    match message {
        ChatCompletionRequestMessage::System(system) => Some(ChatCompletionRequestMessage::System(
            ChatCompletionRequestSystemMessage {
                content: ChatCompletionRequestSystemMessageContent::Text(format!(
                    "{}\n\n{volatile}",
                    system_message_text(&system.content)?
                )),
                name: system.name.clone(),
            },
        )),
        ChatCompletionRequestMessage::Developer(developer) => Some(
            ChatCompletionRequestMessage::Developer(ChatCompletionRequestDeveloperMessage {
                content: ChatCompletionRequestDeveloperMessageContent::Text(format!(
                    "{}\n\n{volatile}",
                    developer_message_text(&developer.content)?
                )),
                name: developer.name.clone(),
            }),
        ),
        _ => None,
    }
}

fn plain_user_message(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content: ChatCompletionRequestUserMessageContent::Text(text.to_string()),
        name: None,
    })
}

fn system_message_text(content: &ChatCompletionRequestSystemMessageContent) -> Option<&str> {
    match content {
        ChatCompletionRequestSystemMessageContent::Text(text) => Some(text.as_str()),
        ChatCompletionRequestSystemMessageContent::Array(_) => None,
    }
}

fn developer_message_text(content: &ChatCompletionRequestDeveloperMessageContent) -> Option<&str> {
    match content {
        ChatCompletionRequestDeveloperMessageContent::Text(text) => Some(text.as_str()),
        ChatCompletionRequestDeveloperMessageContent::Array(_) => None,
    }
}

fn content_part(part: &Value) -> Option<ContentPart> {
    match part.get("type").and_then(Value::as_str) {
        Some("text") | Some("input_text") => part
            .get("text")
            .and_then(Value::as_str)
            .map(|text| ContentPart::Text(text.to_string())),
        Some("image_url") => part
            .get("image_url")
            .and_then(|image| image.get("url").or(Some(image)))
            .and_then(Value::as_str)
            .map(|url| ContentPart::ImageUrl(url.to_string())),
        _ => None,
    }
}

/// The function fields extracted from a tool definition, shared by both
/// protocols. `strict` is only used by Codex.
pub(crate) struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub strict: bool,
}

/// Pull the `function` block out of an OpenAI tool definition. Errors if the
/// tool has no function or no name.
/// Extract `(id, name, arguments)` from one OpenAI-style `tool_call` value, with
/// the shared defaults (`""` for a missing id/name, `"{}"` for missing args).
/// Both transports read this same shape before applying their protocol-specific
/// argument encoding.
pub(crate) fn tool_call_parts(tool_call: &Value) -> (&str, &str, &str) {
    let id = tool_call
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let function = tool_call.get("function");
    let name = function
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = function
        .and_then(|function| function.get("arguments"))
        .and_then(Value::as_str)
        .unwrap_or("{}");
    (id, name, arguments)
}

/// The reasoning/thinking items captured for an assistant turn, keyed by the
/// turn's first tool-call id. Both transports replay these ahead of the turn's
/// output; `None` when there is no map, no tool calls, or no match for the id.
pub(crate) fn replay_items_for<'a>(
    items_by_call_id: Option<&'a HashMap<String, Vec<Value>>>,
    tool_calls: Option<&[Value]>,
) -> Option<&'a [Value]> {
    let first_id = tool_calls?.first()?.get("id").and_then(Value::as_str)?;
    items_by_call_id?.get(first_id).map(Vec::as_slice)
}

pub(crate) fn tool_function(tool: &ChatCompletionTool) -> Result<ToolFunction> {
    let value = serde_json::to_value(tool).context("Failed to serialize tool")?;
    let function = value
        .get("function")
        .and_then(Value::as_object)
        .context("Tool is missing function definition")?;
    let name = function
        .get("name")
        .and_then(Value::as_str)
        .context("Tool is missing function name")?
        .to_string();
    let description = function
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let parameters = function
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    let strict = function
        .get("strict")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ToolFunction {
        name,
        description,
        parameters,
        strict,
    })
}

/// Validate that a built provider has the credentials and configuration it
/// needs before issuing a request.
///
/// `require_key` reflects the provider's `AuthRequirement`: providers with an
/// optional API key (e.g. `anthropic-compatible` pointed at a local endpoint)
/// pass `false` so a missing key is not treated as "unauthorized". This mirrors
/// the `ApiKeyPolicy` gate the OpenAI-compatible provider applies in
/// `opencode::OpenAiCompatibleProvider::ensure_authorized`.
pub(crate) fn ensure_authorized(
    provider_id: &str,
    api_key: &str,
    base_url: &str,
    model: &str,
    require_key: bool,
) -> Result<()> {
    if require_key && api_key.trim().is_empty() {
        anyhow::bail!("{provider_id} is not authorized. Run /authorize {provider_id}.");
    }
    if base_url.trim().is_empty() {
        anyhow::bail!("{provider_id} has no base URL configured");
    }
    if model.trim().is_empty() {
        anyhow::bail!("{provider_id} has no model configured");
    }
    Ok(())
}

/// Join a base URL and an endpoint path into a request URL, collapsing the
/// slash between them so `base/` + `/path` yields exactly one separator. Shared
/// by the OpenAI-compatible, Anthropic-compatible, and Codex providers.
pub(crate) fn endpoint_url(base_url: &str, endpoint_path: &str) -> String {
    format!(
        "{}/{}",
        base_url.trim_end_matches('/'),
        endpoint_path.trim_start_matches('/')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::test_utils::{named_user_message, system_message, user_message};

    #[test]
    fn endpoint_url_trims_trailing_and_leading_slashes() {
        assert_eq!(
            endpoint_url("http://h/", "/v1/messages"),
            "http://h/v1/messages"
        );
        assert_eq!(
            endpoint_url("http://h", "v1/messages"),
            "http://h/v1/messages"
        );
        assert_eq!(endpoint_url("http://h///", "///models"), "http://h/models");
    }

    #[test]
    fn endpoint_url_preserves_internal_slashes() {
        assert_eq!(
            endpoint_url("http://h/api", "v1/chat/completions"),
            "http://h/api/v1/chat/completions"
        );
    }

    #[test]
    fn content_to_text_joins_array_text_parts() {
        let content = json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]);
        assert_eq!(content_to_text(Some(&content)), Some("a\nb".to_string()));
    }

    #[test]
    fn content_to_text_reads_plain_string() {
        let content = json!("hello");
        assert_eq!(content_to_text(Some(&content)), Some("hello".to_string()));
    }

    #[test]
    fn demote_non_leading_system_messages_folds_harness_notes_into_user_stream() {
        let mid_system = ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
            content: "Harness note: Self-review before finishing."
                .to_string()
                .into(),
            name: Some("bonsai_harness".to_string()),
        });
        let messages = vec![
            system_message("system prompt"),
            user_message("do the thing"),
            mid_system,
            user_message("continue"),
        ];

        let shaped = demote_non_leading_system_messages(&messages);

        // The leading system prompt is left in place (cache prefix stays stable).
        assert!(matches!(
            &shaped[0],
            ChatCompletionRequestMessage::System(system)
                if system_message_text(&system.content) == Some("system prompt")
        ));
        // The mid-stream system note became a named user message, provenance intact.
        match &shaped[2] {
            ChatCompletionRequestMessage::User(user) => {
                assert_eq!(user.name.as_deref(), Some("bonsai_harness"));
                let value = serde_json::to_value(&user.content).unwrap();
                assert_eq!(
                    content_to_text(Some(&value)).as_deref(),
                    Some("Harness note: Self-review before finishing.")
                );
            }
            other => panic!("expected a demoted user message, got {other:?}"),
        }
        // Nothing that reaches the wire past index 0 is still a system message.
        assert!(
            shaped
                .iter()
                .skip(1)
                .all(|message| !matches!(message, ChatCompletionRequestMessage::System(_)))
        );
    }

    #[test]
    fn demote_non_leading_system_messages_borrows_when_already_conformant() {
        let messages = vec![
            system_message("system prompt"),
            user_message("hello"),
            named_user_message("bonsai_background", "note"),
        ];
        // No non-leading system message → zero-copy, byte-identical wire body.
        assert!(matches!(
            demote_non_leading_system_messages(&messages),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn content_parts_extracts_text_and_image() {
        let content = json!([
            {"type": "text", "text": "hi"},
            {"type": "image_url", "image_url": {"url": "http://x/y.png"}},
        ]);
        let parts = content_parts(Some(&content));
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], ContentPart::Text(t) if t == "hi"));
        assert!(matches!(&parts[1], ContentPart::ImageUrl(u) if u == "http://x/y.png"));
    }

    #[test]
    fn split_volatile_system_context_keeps_stable_head() {
        let system = format!(
            "stable instructions\n\n{}\n- branch: main",
            crate::context::VOLATILE_STATE_HEADING
        );

        let split = split_volatile_system_context(&system).expect("volatile split");

        assert_eq!(split.stable, "stable instructions");
        assert_eq!(
            split.volatile,
            format!("{}\n- branch: main", crate::context::VOLATILE_STATE_HEADING)
        );
    }

    #[test]
    fn chat_layout_is_byte_equivalent_for_legacy_and_durable_state() {
        let volatile = format!("{}\n- dirty files", crate::context::VOLATILE_STATE_HEADING);
        let legacy = vec![
            system_message(&format!("stable instructions\n\n{volatile}")),
            user_message("hello"),
        ];
        let durable = vec![
            system_message("stable instructions"),
            user_message("hello"),
            named_user_message(
                crate::context::PROJECT_STATE_MESSAGE_NAME,
                &volatile_context_user_text(&volatile),
            ),
        ];

        let legacy_wire =
            messages_for_project_state_layout(&legacy, ProjectStateWireLayout::LatestAtEnd);
        let durable_wire =
            messages_for_project_state_layout(&durable, ProjectStateWireLayout::LatestAtEnd);

        assert_eq!(
            serialize_messages(legacy_wire.as_ref()),
            serialize_messages(durable_wire.as_ref()),
            "IMPORTANT: Qwen/GLM must retain their proven wire cache layout"
        );
    }

    #[test]
    fn chat_layout_discards_historical_snapshots_and_keeps_latest_at_end() {
        let prefix = crate::context::PROJECT_STATE_UPDATE_PREFIX;
        let messages = vec![
            system_message("stable"),
            named_user_message(
                crate::context::PROJECT_STATE_MESSAGE_NAME,
                &format!("{prefix}\n\n## Volatile state\nold"),
            ),
            user_message("work"),
            named_user_message(
                crate::context::PROJECT_STATE_MESSAGE_NAME,
                &format!("{prefix}\n\n## Volatile state\nnew"),
            ),
        ];

        let wire =
            messages_for_project_state_layout(&messages, ProjectStateWireLayout::LatestAtEnd);
        let values = serialize_messages(wire.as_ref());

        assert_eq!(values.len(), 3);
        assert_eq!(values[1]["content"], json!("work"));
        assert_eq!(
            values[2]["content"],
            json!(format!("{prefix}\n\n## Volatile state\nnew"))
        );
        assert!(values[2].get("name").is_none());
    }

    #[test]
    fn anthropic_layout_is_byte_equivalent_for_legacy_and_durable_state() {
        let volatile = format!("{}\n- dirty files", crate::context::VOLATILE_STATE_HEADING);
        let legacy = vec![
            system_message(&format!("stable instructions\n\n{volatile}")),
            user_message("hello"),
        ];
        let durable = vec![
            system_message("stable instructions"),
            user_message("hello"),
            named_user_message(
                crate::context::PROJECT_STATE_MESSAGE_NAME,
                &volatile_context_user_text(&volatile),
            ),
        ];

        let legacy_wire =
            messages_for_project_state_layout(&legacy, ProjectStateWireLayout::SystemTail);
        let durable_wire =
            messages_for_project_state_layout(&durable, ProjectStateWireLayout::SystemTail);

        assert_eq!(
            serialize_messages(legacy_wire.as_ref()),
            serialize_messages(durable_wire.as_ref()),
            "IMPORTANT: Anthropic must retain its explicit-breakpoint input layout"
        );
    }

    #[test]
    fn append_only_layout_borrows_and_preserves_every_message() {
        let messages = vec![
            system_message("stable"),
            named_user_message(
                crate::context::PROJECT_STATE_MESSAGE_NAME,
                // Deliberately the legacy envelope: resumed pre-Harness-note
                // sessions must keep working through every layout.
                "Context update for the request above:\n\n## Volatile state\nfirst",
            ),
            user_message("next"),
        ];

        let wire = messages_for_project_state_layout(&messages, ProjectStateWireLayout::AppendOnly);

        assert!(matches!(wire, Cow::Borrowed(_)));
        assert_eq!(wire.as_ref(), messages.as_slice());
    }

    fn serialize_messages(messages: &[ChatCompletionRequestMessage]) -> Vec<Value> {
        messages
            .iter()
            .map(|message| serde_json::to_value(message).expect("message should serialize"))
            .collect()
    }

    #[test]
    fn tool_function_extracts_fields() {
        let tool = serde_json::from_value::<ChatCompletionTool>(json!({
            "type": "function",
            "function": {
                "name": "read",
                "description": "Read a file",
                "parameters": {"type": "object"},
                "strict": true,
            }
        }))
        .unwrap();
        let func = tool_function(&tool).unwrap();
        assert_eq!(func.name, "read");
        assert_eq!(func.description, "Read a file");
        assert_eq!(func.parameters, json!({"type": "object"}));
        assert!(func.strict);
    }

    #[test]
    fn ensure_authorized_reports_missing_fields() {
        // Required-key provider: a missing key is unauthorized.
        assert!(ensure_authorized("p", "", "url", "model", true).is_err());
        assert!(ensure_authorized("p", "key", "", "model", true).is_err());
        assert!(ensure_authorized("p", "key", "url", "", true).is_err());
        assert!(ensure_authorized("p", "key", "url", "model", true).is_ok());
    }

    #[test]
    fn ensure_authorized_allows_empty_key_when_optional() {
        // Optional-key provider (e.g. anthropic-compatible against a local
        // endpoint): an empty key is fine as long as base URL + model are set.
        assert!(ensure_authorized("p", "", "url", "model", false).is_ok());
        // base URL and model are still required regardless of the key policy.
        assert!(ensure_authorized("p", "", "", "model", false).is_err());
        assert!(ensure_authorized("p", "", "url", "", false).is_err());
    }
}
