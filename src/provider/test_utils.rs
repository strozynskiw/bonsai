use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
    ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestToolMessageArgs,
    ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageArgs, ChatCompletionTool,
    FunctionCall,
};
use serde_json::json;

use crate::session::ProviderSession;

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
