use async_openai::types::chat::{ChatCompletionMessageToolCalls, ChatCompletionRequestMessage};
use serde_json::{Value, json};

const EMPTY_TOOL_ARGUMENTS_JSON: &str = "{}";

pub(crate) fn normalize_tool_call_arguments_json(arguments: &str) -> String {
    if tool_call_arguments_are_object(arguments) {
        arguments.to_string()
    } else {
        EMPTY_TOOL_ARGUMENTS_JSON.to_string()
    }
}

pub(crate) fn tool_call_arguments_value(arguments: &str) -> Value {
    if tool_call_arguments_are_object(arguments) {
        serde_json::from_str(arguments).unwrap_or_else(|_err| json!({}))
    } else {
        json!({})
    }
}

pub(crate) fn normalize_chat_message_tool_call_arguments(
    message: &mut ChatCompletionRequestMessage,
) {
    let ChatCompletionRequestMessage::Assistant(assistant) = message else {
        return;
    };
    let Some(tool_calls) = assistant.tool_calls.as_mut() else {
        return;
    };
    for tool_call in tool_calls {
        if let ChatCompletionMessageToolCalls::Function(function_call) = tool_call {
            function_call.function.arguments =
                normalize_tool_call_arguments_json(&function_call.function.arguments);
        }
    }
}

fn tool_call_arguments_are_object(arguments: &str) -> bool {
    serde_json::from_str::<Value>(arguments).is_ok_and(|value| value.is_object())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_malformed_tool_call_arguments_normalize_to_empty_object() {
        assert_eq!(normalize_tool_call_arguments_json(""), "{}");
        assert_eq!(normalize_tool_call_arguments_json("   "), "{}");
        assert_eq!(normalize_tool_call_arguments_json("{"), "{}");
        assert_eq!(normalize_tool_call_arguments_json("\"text\""), "{}");
        assert_eq!(normalize_tool_call_arguments_json("[]"), "{}");
    }

    #[test]
    fn object_tool_call_arguments_are_preserved() {
        assert_eq!(
            normalize_tool_call_arguments_json(r#"{"path":"src/main.rs"}"#),
            r#"{"path":"src/main.rs"}"#
        );
    }
}
