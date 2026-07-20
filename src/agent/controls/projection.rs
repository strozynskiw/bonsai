//! Shared context projection: applies user controls once, records why each
//! source row is selected or omitted, and derives the final wire payload.

use super::*;

#[derive(Debug, Clone)]
pub(in crate::agent) struct ContextProjection {
    control_messages: Vec<ChatCompletionRequestMessage>,
    wire_messages: Vec<ChatCompletionRequestMessage>,
    source_message_tokens: Vec<usize>,
    estimate: Option<PromptEstimate>,
    items: Vec<ContextProjectionItem>,
}

impl ContextProjection {
    pub(in crate::agent) fn control_messages(&self) -> &[ChatCompletionRequestMessage] {
        &self.control_messages
    }

    pub(in crate::agent) fn wire_messages(&self) -> &[ChatCompletionRequestMessage] {
        &self.wire_messages
    }

    pub(in crate::agent) fn source_message_tokens(&self) -> &[usize] {
        &self.source_message_tokens
    }

    pub(in crate::agent) fn estimate(&self) -> Option<&PromptEstimate> {
        self.estimate.as_ref()
    }

    #[cfg(test)]
    pub(in crate::agent) fn items(&self) -> &[ContextProjectionItem] {
        &self.items
    }

    pub(in crate::agent) fn message_inclusions(&self) -> HashMap<usize, ContextInclusion> {
        self.items
            .iter()
            .filter_map(|item| {
                if item.selection != ContextProjectionSelection::UserDropped {
                    return None;
                }
                item.source
                    .message_index()
                    .map(|index| (index, ContextInclusion::NotSent))
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::agent) struct ContextProjectionItem {
    pub(in crate::agent) id: String,
    pub(in crate::agent) source: ContextProjectionSource,
    pub(in crate::agent) source_index: usize,
    pub(in crate::agent) role: ContextRole,
    pub(in crate::agent) lifecycle: ContextProjectionLifecycle,
    pub(in crate::agent) cache_class: ContextProjectionCacheClass,
    pub(in crate::agent) trust_class: ContextProjectionTrustClass,
    pub(in crate::agent) selection: ContextProjectionSelection,
    pub(in crate::agent) transform: ContextProjectionTransform,
    pub(in crate::agent) tokens: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum ContextProjectionSource {
    Message(usize),
    ToolSchema,
}

impl ContextProjectionSource {
    const fn message_index(self) -> Option<usize> {
        match self {
            Self::Message(index) => Some(index),
            Self::ToolSchema => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum ContextProjectionLifecycle {
    Mandatory,
    Pinned,
    Recent,
    Evictable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum ContextProjectionCacheClass {
    StablePrefix,
    VolatileTail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum ContextProjectionTrustClass {
    TrustedInstruction,
    UserIntent,
    UntrustedData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum ContextProjectionSelection {
    Selected,
    UserDropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum ContextProjectionTransform {
    None,
    ControlStubbed,
    SmolSummarized,
    SmolCapped,
}

impl Agent {
    pub(in crate::agent) fn context_projection_for_controls(
        &self,
        messages: &[ChatCompletionRequestMessage],
        message_ids: &[String],
        controls: &HashMap<String, ContextControlState>,
    ) -> ContextProjection {
        self.build_context_projection(messages, message_ids, controls, None)
    }

    pub(in crate::agent) fn context_projection_with_estimate_for_controls(
        &self,
        messages: &[ChatCompletionRequestMessage],
        message_ids: &[String],
        controls: &HashMap<String, ContextControlState>,
        tool_schema: &ToolSchemaPayload,
    ) -> ContextProjection {
        self.build_context_projection(messages, message_ids, controls, Some(tool_schema))
    }

    fn build_context_projection(
        &self,
        messages: &[ChatCompletionRequestMessage],
        message_ids: &[String],
        controls: &HashMap<String, ContextControlState>,
        tool_schema: Option<&ToolSchemaPayload>,
    ) -> ContextProjection {
        let mut control_messages = Vec::with_capacity(messages.len());
        let mut control_message_item_indices = Vec::with_capacity(messages.len());
        let mut source_message_tokens = vec![0; messages.len()];
        let mut items = Vec::with_capacity(messages.len());

        for (index, message) in messages.iter().enumerate() {
            let message_id = message_ids
                .get(index)
                .cloned()
                .unwrap_or_else(|| ContextNodeId::message(index).into_string());
            let (role, _text) = describe_message_full(message);
            let pinned = Self::message_has_control_in(
                controls,
                index,
                message,
                Some(&message_id),
                |state| state.pinned,
            );
            let dropped = Self::message_has_control_in(
                controls,
                index,
                message,
                Some(&message_id),
                |state| state.drop_next_turn,
            );
            let mut transform = ContextProjectionTransform::None;
            let selected_message = if dropped {
                None
            } else if let Some(stubbed) = self.stubbed_message_with_controls(message, controls) {
                transform = ContextProjectionTransform::ControlStubbed;
                Some(stubbed)
            } else {
                Some(message.clone())
            };
            let selection = if dropped {
                ContextProjectionSelection::UserDropped
            } else {
                ContextProjectionSelection::Selected
            };
            let item_index = items.len();
            if let Some(selected_message) = selected_message {
                control_message_item_indices.push(item_index);
                control_messages.push(selected_message);
            }
            items.push(ContextProjectionItem {
                id: message_id,
                source: ContextProjectionSource::Message(index),
                source_index: index,
                role,
                lifecycle: projection_lifecycle(index, role, pinned),
                cache_class: projection_cache_class(index, role),
                trust_class: projection_trust_class(message, role),
                selection,
                transform,
                tokens: 0,
            });
        }

        let estimate = tool_schema.map(|tool_schema| {
            let (payload_tokens, estimate) = self
                .prompt_estimator
                .estimate_messages_for_report_with_tool_schema_tokens(
                    &control_messages,
                    tool_schema.report_tool_schema_tokens(),
                );
            for (token_index, item_index) in
                control_message_item_indices.iter().copied().enumerate()
            {
                let tokens = payload_tokens.get(token_index).copied().unwrap_or(0);
                if let Some(item) = items.get_mut(item_index) {
                    item.tokens = tokens;
                    if let Some(source_index) = item.source.message_index()
                        && let Some(source_tokens) = source_message_tokens.get_mut(source_index)
                    {
                        *source_tokens = tokens;
                    }
                }
            }
            estimate
        });
        if let Some(estimate) = estimate.as_ref()
            && estimate.tool_schema_tokens > 0
        {
            items.push(ContextProjectionItem {
                id: "tool-schemas".to_string(),
                source: ContextProjectionSource::ToolSchema,
                source_index: messages.len(),
                role: ContextRole::ToolSchema,
                lifecycle: ContextProjectionLifecycle::Mandatory,
                cache_class: ContextProjectionCacheClass::StablePrefix,
                trust_class: ContextProjectionTrustClass::TrustedInstruction,
                selection: ContextProjectionSelection::Selected,
                transform: ContextProjectionTransform::None,
                tokens: estimate.tool_schema_tokens,
            });
        }

        let wire_messages = if self.smol_applies_to_active_persona() {
            self.smol_outgoing_messages(control_messages.clone())
        } else {
            control_messages.clone()
        };
        if self.smol_applies_to_active_persona() {
            mark_smol_transforms(&mut items, &control_messages, &wire_messages);
        }

        ContextProjection {
            control_messages,
            wire_messages,
            source_message_tokens,
            estimate,
            items,
        }
    }
}

fn projection_lifecycle(
    index: usize,
    role: ContextRole,
    pinned: bool,
) -> ContextProjectionLifecycle {
    if index == 0 || role == ContextRole::System {
        ContextProjectionLifecycle::Mandatory
    } else if pinned {
        ContextProjectionLifecycle::Pinned
    } else if role == ContextRole::Tool {
        ContextProjectionLifecycle::Evictable
    } else {
        ContextProjectionLifecycle::Recent
    }
}

fn projection_cache_class(index: usize, role: ContextRole) -> ContextProjectionCacheClass {
    if index == 0 || role == ContextRole::System {
        ContextProjectionCacheClass::StablePrefix
    } else {
        ContextProjectionCacheClass::VolatileTail
    }
}

fn projection_trust_class(
    message: &ChatCompletionRequestMessage,
    role: ContextRole,
) -> ContextProjectionTrustClass {
    match role {
        ContextRole::System | ContextRole::ToolSchema => {
            ContextProjectionTrustClass::TrustedInstruction
        }
        ContextRole::User => match message {
            ChatCompletionRequestMessage::User(user) if user.name.is_none() => {
                ContextProjectionTrustClass::UserIntent
            }
            _ => ContextProjectionTrustClass::UntrustedData,
        },
        ContextRole::Assistant | ContextRole::Tool => ContextProjectionTrustClass::UntrustedData,
    }
}

fn mark_smol_transforms(
    items: &mut [ContextProjectionItem],
    control_messages: &[ChatCompletionRequestMessage],
    wire_messages: &[ChatCompletionRequestMessage],
) {
    let wire_json = wire_messages
        .iter()
        .filter_map(|message| serde_json::to_string(message).ok())
        .collect::<Vec<_>>();
    let wire_tool_call_ids = wire_messages
        .iter()
        .filter_map(tool_message_call_id)
        .collect::<HashSet<_>>();

    for (control_index, message) in control_messages.iter().enumerate() {
        let Some(item) = items
            .iter_mut()
            .filter(|item| item.selection == ContextProjectionSelection::Selected)
            .nth(control_index)
        else {
            continue;
        };
        let Ok(encoded) = serde_json::to_string(message) else {
            continue;
        };
        if wire_json.iter().any(|wire| wire == &encoded) {
            continue;
        }
        item.transform = if tool_message_call_id(message)
            .as_ref()
            .is_some_and(|call_id| wire_tool_call_ids.contains(call_id))
        {
            ContextProjectionTransform::SmolCapped
        } else {
            ContextProjectionTransform::SmolSummarized
        };
    }
}

#[cfg(test)]
mod trust_tests {
    use super::*;

    #[test]
    fn user_role_trust_uses_structured_provenance_name() {
        let human =
            crate::agent::messages::provenance_message(MessageProvenance::Human, "human request");
        assert_eq!(
            projection_trust_class(&human, ContextRole::User),
            ContextProjectionTrustClass::UserIntent
        );

        for provenance in [
            MessageProvenance::Peer,
            MessageProvenance::Background,
            MessageProvenance::UntrustedData,
        ] {
            let message = crate::agent::messages::provenance_message(provenance, "external data");
            assert_eq!(
                projection_trust_class(&message, ContextRole::User),
                ContextProjectionTrustClass::UntrustedData,
                "{provenance:?}"
            );
        }
    }
}
