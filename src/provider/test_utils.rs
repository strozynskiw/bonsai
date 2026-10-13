use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageArgs, ChatCompletionTool,
    FunctionCall,
};
use serde_json::json;
use std::sync::Mutex;

use crate::session::ProviderSession;

/// Stream capture for provider regressions that compare channels and history.
#[derive(Debug, Default)]
pub(crate) struct RecordingSink {
    assistant: Mutex<Vec<String>>,
    reasoning: Mutex<Vec<String>>,
}

impl RecordingSink {
    /// Return the visible text deltas in emission order.
    pub(crate) fn assistant_deltas(&self) -> Vec<String> {
        self.assistant.lock().unwrap().clone()
    }

    /// Return reasoning deltas in emission order.
    pub(crate) fn reasoning_deltas(&self) -> Vec<String> {
        self.reasoning.lock().unwrap().clone()
    }
}

impl crate::output::OutputSink for RecordingSink {
    fn assistant_delta(&self, text: &str) {
        self.assistant.lock().unwrap().push(text.to_string());
    }

    fn reasoning_delta(&self, text: &str) {
        self.reasoning.lock().unwrap().push(text.to_string());
    }
}

/// Build a plain assistant reply for request replay checks.
pub(crate) fn assistant_message(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(
        ChatCompletionRequestAssistantMessageArgs::default()
            .content(text)
            .build()
            .unwrap(),
    )
}

pub(crate) fn assert_tool_call_identity(
    provider: &dyn crate::provider::Provider,
    call: &crate::provider::ToolCall,
    expected_wire_id: &str,
) {
    assert_ne!(call.id, expected_wire_id);
    let uuid = uuid::Uuid::parse_str(call.id.strip_prefix("call_").unwrap()).unwrap();
    assert_eq!(uuid.get_version_num(), 7);
    let history = [
        tool_call_message(&call.id, &call.name, &call.arguments),
        tool_result_message(&call.id, "result"),
    ];
    let body = provider.preview_request(&history, &[]).unwrap().body;
    let input = body.get("input").and_then(serde_json::Value::as_array);
    let wire_call = body
        .pointer("/messages/0/tool_calls/0/id")
        .or_else(|| {
            body.pointer("/messages/0/content")
                .and_then(serde_json::Value::as_array)
                .and_then(|parts| parts.iter().find(|part| part["type"] == "tool_use"))
                .and_then(|part| part.get("id"))
        })
        .or_else(|| {
            input
                .and_then(|input| input.iter().find(|item| item["type"] == "function_call"))
                .and_then(|item| item.get("call_id"))
        });
    assert_eq!(
        wire_call.and_then(serde_json::Value::as_str),
        Some(expected_wire_id),
        "{body}"
    );
    let wire_result = body
        .pointer("/messages/1/tool_call_id")
        .or_else(|| body.pointer("/messages/1/content/0/tool_use_id"))
        .or_else(|| {
            input
                .and_then(|input| {
                    input
                        .iter()
                        .find(|item| item["type"] == "function_call_output")
                })
                .and_then(|item| item.get("call_id"))
        });
    assert_eq!(
        wire_result.and_then(serde_json::Value::as_str),
        Some(expected_wire_id),
        "{body}"
    );
}

pub(crate) fn provider_session(api_key: &str, base_url: &str, model: &str) -> ProviderSession {
    ProviderSession::new(api_key.to_string(), base_url.to_string(), model.to_string())
}

/// A session with an explicit base URL and no model — the common shape for
/// endpoint-provider tests that drive a wiremock server.
pub(crate) fn provider_session_with_base(api_key: &str, base_url: &str) -> ProviderSession {
    ProviderSession::new(api_key.to_string(), base_url.to_string(), String::new())
}

pub(crate) fn user_message(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(
        ChatCompletionRequestUserMessageArgs::default()
            .content(text)
            .build()
            .unwrap(),
    )
}

pub(crate) fn named_user_message(name: &str, text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content: text.to_string().into(),
        name: Some(name.to_string()),
    })
}

pub(crate) fn system_message(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::System(
        ChatCompletionRequestSystemMessageArgs::default()
            .content(text)
            .build()
            .unwrap(),
    )
}

pub(crate) fn tool_call_message(
    id: &str,
    name: &str,
    arguments: &str,
) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(
        ChatCompletionRequestAssistantMessageArgs::default()
            .content("")
            .tool_calls(vec![ChatCompletionMessageToolCalls::Function(
                ChatCompletionMessageToolCall {
                    id: id.to_string(),
                    function: FunctionCall {
                        name: name.to_string(),
                        arguments: arguments.to_string(),
                    },
                },
            )])
            .build()
            .unwrap(),
    )
}

pub(crate) fn tool_result_message(call_id: &str, output: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Tool(
        ChatCompletionRequestToolMessageArgs::default()
            .content(output)
            .tool_call_id(call_id)
            .build()
            .unwrap(),
    )
}

pub(crate) fn sample_tool() -> ChatCompletionTool {
    serde_json::from_value::<ChatCompletionTool>(json!({
        "type": "function",
        "function": {
            "name": "read",
            "description": "Read a file",
            "parameters": {"type": "object"}
        }
    }))
    .unwrap()
}
