//! Builders for the chat messages the agent installs into its history.
//!
//! Every constructor here is infallible: the underlying async-openai builders
//! only fail when a required field is left unset, and these helpers always set
//! `content`. Constructing the message structs directly removes the
//! `.expect("…owned content")` clutter that previously surrounded every call
//! site without introducing a spurious `Result`.

use async_openai::types::chat::{
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestSystemMessage,
    ChatCompletionRequestUserMessage,
};

use super::*;

/// Append the dynamic project-context block to a persona prompt when present.
fn with_project_context(prompt: &str, context: &str) -> String {
    let mut content = prompt.to_string();
    if !context.trim().is_empty() {
        content.push_str("\n\n# Project context\n\n");
        content.push_str(context);
    }
    content
}

pub(super) fn with_operator_suffix(prompt: &str, suffix: Option<&str>) -> String {
    let Some(suffix) = suffix.map(str::trim).filter(|suffix| !suffix.is_empty()) else {
        return prompt.to_string();
    };
    format!("{prompt}\n\n# Additional operator instructions\n\n{suffix}")
}

fn system_message_from_content(content: String) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
        content: content.into(),
        name: None,
    })
}

/// The full system message for a persona prompt: the prompt plus the dynamic
/// project-context block when one is present.
pub(super) fn system_message_from_prompt(
    prompt: &str,
    context: &str,
) -> ChatCompletionRequestMessage {
    system_message_from_content(with_project_context(prompt, context))
}

/// The full system message for a mode: the static persona prompt plus the
/// dynamic project-context block when one is present.
#[cfg(test)]
pub(super) fn system_message(mode: AgentMode, context: &str) -> ChatCompletionRequestMessage {
    system_message_from_prompt(persona::persona_for(mode).system_prompt(), context)
}

pub(super) fn system_message_with_suffix(
    mode: AgentMode,
    context: &str,
    suffix: Option<&str>,
) -> ChatCompletionRequestMessage {
    let prompt = with_operator_suffix(persona::persona_for(mode).system_prompt(), suffix);
    system_message_from_prompt(&prompt, context)
}

/// A trusted context note loaded at runtime, such as an on-demand skill body.
pub(super) fn trusted_context_message(text: &str) -> ChatCompletionRequestMessage {
    system_message_from_content(text.to_string())
}

const SCOPED_STEERING_MESSAGE_NAME: &str = "bonsai_scoped_steering";

/// A trusted path-scoped steering update with durable provenance. The name is
/// checked when restoring a persisted conversation so user-authored content can
/// never impersonate instruction coverage.
pub(super) fn scoped_steering_message(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
        content: text.to_string().into(),
        name: Some(SCOPED_STEERING_MESSAGE_NAME.to_string()),
    })
}

pub(super) fn is_scoped_steering_message(message: &ChatCompletionRequestMessage) -> bool {
    matches!(
        message,
        ChatCompletionRequestMessage::System(system)
            if system.name.as_deref() == Some(SCOPED_STEERING_MESSAGE_NAME)
    )
}

/// Build an injected context message with role- and name-level provenance.
/// Peer/external content remains user-role data even though its source is
/// structured; only trusted harness/background notes receive system authority.
pub(super) fn provenance_message(
    provenance: MessageProvenance,
    text: &str,
) -> ChatCompletionRequestMessage {
    if provenance.is_trusted_system() {
        return ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
            content: text.to_string().into(),
            name: provenance.wire_name().map(str::to_string),
        });
    }
    ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content: text.to_string().into(),
        name: provenance.wire_name().map(str::to_string),
    })
}

/// A cache-safe snapshot of volatile project state.
///
/// IMPORTANT: these messages are deliberately user-role, named, and
/// append-only. Do not promote them into the system message or relocate the
/// newest snapshot during provider request shaping: GPT prompt caching records
/// message-boundary prefixes, and either change destroys the cache lineage for
/// every assistant/tool message that followed the prior snapshot.
pub(super) fn project_state_message(text: &str) -> ChatCompletionRequestMessage {
    provenance_message(MessageProvenance::ProjectState, text)
}

/// Append-only trusted runtime-policy note. A newer note explicitly supersedes
/// older ones without changing the stable system-message prefix.
pub(super) fn execution_policy_message(content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
        content: content.to_string().into(),
        name: Some("bonsai_execution_policy".to_string()),
    })
}

pub(super) fn is_project_state_message(message: &ChatCompletionRequestMessage) -> bool {
    matches!(
        message,
        ChatCompletionRequestMessage::User(user)
            if user.name.as_deref() == MessageProvenance::ProjectState.wire_name()
    )
}

pub(super) fn is_volatile_terminal_message(message: &ChatCompletionRequestMessage) -> bool {
    matches!(
        message,
        ChatCompletionRequestMessage::User(user)
            if user.name.as_deref() == MessageProvenance::Terminal.wire_name()
                || user.name.as_deref() == MessageProvenance::Background.wire_name()
                    && try_message_content_string(message).is_some_and(|content| {
                        content.contains("source=\"running interactive terminal status\"")
                    })
    )
}

/// Like [`system_message_from_prompt`], but appends a transition section so the
/// model drops the old persona when switching personas mid-conversation.
pub(super) fn system_message_from_prompt_with_transition(
    prompt: &str,
    context: &str,
    from_name: &str,
    to_name: &str,
) -> ChatCompletionRequestMessage {
    use std::fmt::Write as _;
    let mut content = with_project_context(prompt, context);
    let _ = write!(
        content,
        "\n\n# Mode transition\n\n\
         The conversation history above is from your previous {from_name} mode session. \
         You are now in {to_name} mode with a different tool set and persona. \
         Do not reference or continue the {from_name} workflow.",
    );
    system_message_from_content(content)
}

/// Place a freshly-built system message at index 0, pushing when the buffer is
/// empty. Centralizes the "push vs replace slot 0" choice that several call
/// sites previously open-coded.
pub(super) fn set_system_message(
    messages: &mut Vec<ChatCompletionRequestMessage>,
    message: ChatCompletionRequestMessage,
) {
    if messages.is_empty() {
        messages.push(message);
    } else {
        messages[0] = message;
    }
}

/// A `User` message wrapping owned text.
pub(super) fn user_text_message(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content: text.into(),
        name: None,
    })
}

/// A `User` message carrying text plus one or more attached images, each as a
/// base64 data URI. Mirrors [`image_user_message`](super::run_loop_executor)
/// but for a real user turn with pasted images.
pub(super) fn user_message_with_images(
    text: &str,
    images: &[ImageAttachment],
) -> ChatCompletionRequestMessage {
    let mut parts: Vec<ChatCompletionRequestUserMessageContentPart> =
        Vec::with_capacity(1 + images.len());
    parts.push(ChatCompletionRequestUserMessageContentPart::Text(
        ChatCompletionRequestMessageContentPartText {
            text: text.to_string(),
        },
    ));
    for image in images {
        let data_uri = format!("data:{};base64,{}", image.mime, image.base64);
        parts.push(ChatCompletionRequestUserMessageContentPart::ImageUrl(
            ChatCompletionRequestMessageContentPartImage {
                image_url: ImageUrl {
                    url: data_uri,
                    detail: None,
                },
            },
        ));
    }
    ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content: ChatCompletionRequestUserMessageContent::Array(parts),
        name: None,
    })
}

/// An `Assistant` message wrapping owned text.
pub(super) fn assistant_text_message(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(ChatCompletionRequestAssistantMessage {
        content: Some(text.into()),
        ..Default::default()
    })
}

#[cfg(test)]
mod provenance_tests {
    use super::*;

    #[test]
    fn provenance_controls_role_and_structured_name() {
        let harness = serde_json::to_value(provenance_message(
            MessageProvenance::Harness,
            "Harness note: act now",
        ))
        .unwrap();
        assert_eq!(harness["role"], "system");
        assert_eq!(harness["name"], "bonsai_harness");

        let peer = serde_json::to_value(provenance_message(
            MessageProvenance::Peer,
            "untrusted peer data",
        ))
        .unwrap();
        assert_eq!(peer["role"], "user");
        assert_eq!(peer["name"], "bonsai_peer");
    }
}
