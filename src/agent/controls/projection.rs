//! Shared context projection: applies user controls once, records why each
//! source row is selected or omitted, and derives the final wire payload.

use super::*;

#[derive(Debug, Clone)]
pub(in crate::agent) struct ContextProjection {
    control_messages: Vec<ChatCompletionRequestMessage>,
    wire_messages: Vec<ChatCompletionRequestMessage>,
    wire_request_context: crate::provider::ProviderRequestContext,
    source_message_tokens: Vec<usize>,
    estimate: Option<PromptEstimate>,
    estimate_error: Option<String>,
    estimate_fingerprint: Option<blake3::Hash>,
    native_message_budgets: HashMap<usize, crate::context_view::NativeMessageBudget>,
    items: Vec<ContextProjectionItem>,
}

impl ContextProjection {
    pub(in crate::agent) fn into_prepared_model_request(self) -> PreparedModelRequest {
        PreparedModelRequest {
            messages: self.wire_messages,
            context: self.wire_request_context,
            options: Default::default(),
            estimate_fingerprint: self.estimate_fingerprint,
        }
    }
    pub(in crate::agent) fn control_messages(&self) -> &[ChatCompletionRequestMessage] {
        &self.control_messages
    }

    #[cfg(test)]
    pub(in crate::agent) fn wire_messages(&self) -> &[ChatCompletionRequestMessage] {
        &self.wire_messages
    }

    pub(in crate::agent) fn source_message_tokens(&self) -> &[usize] {
        &self.source_message_tokens
    }

    pub(in crate::agent) fn estimate(&self) -> Option<&PromptEstimate> {
        self.estimate.as_ref()
    }

    pub(in crate::agent) fn estimate_error(&self) -> Option<&str> {
        self.estimate_error.as_deref()
    }

    pub(in crate::agent) fn estimate_fingerprint(&self) -> Option<blake3::Hash> {
        self.estimate_fingerprint
    }

    pub(in crate::agent) fn native_message_budgets(
        &self,
    ) -> &HashMap<usize, crate::context_view::NativeMessageBudget> {
        &self.native_message_budgets
    }

    pub(in crate::agent) fn volatile_terminal_tokens(&self) -> usize {
        self.items
            .iter()
            .find(|item| item.source == ContextProjectionSource::VolatileTerminal)
            .map_or(0, |item| item.tokens)
    }

    #[cfg(test)]
    pub(in crate::agent) fn items(&self) -> &[ContextProjectionItem] {
        &self.items
    }

    pub(in crate::agent) fn message_inclusions(&self) -> HashMap<usize, ContextInclusion> {
        self.items
            .iter()
            .filter_map(|item| {
                if !matches!(
                    item.selection,
                    ContextProjectionSelection::UserDropped
                        | ContextProjectionSelection::VolatileSuperseded
                ) {
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
    VolatileTerminal,
    ToolSchema,
}

impl ContextProjectionSource {
    const fn message_index(self) -> Option<usize> {
        match self {
            Self::Message(index) => Some(index),
            Self::VolatileTerminal | Self::ToolSchema => None,
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
    VolatileSuperseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::agent) enum ContextProjectionTransform {
    None,
    ControlStubbed,
    VolatileReplaced,
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
        let mut control_messages = Vec::with_capacity(messages.len().saturating_add(1));
        let mut control_message_item_indices = Vec::with_capacity(messages.len().saturating_add(1));
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
            let volatile_superseded = is_volatile_terminal_message(message);
            let mut transform = ContextProjectionTransform::None;
            let selected_message = if volatile_superseded {
                transform = ContextProjectionTransform::VolatileReplaced;
                None
            } else if dropped {
                None
            } else if let Some(stubbed) = self.stubbed_message_with_controls(message, controls) {
                transform = ContextProjectionTransform::ControlStubbed;
                Some(stubbed)
            } else {
                Some(message.clone())
            };
            let selection = if volatile_superseded {
                ContextProjectionSelection::VolatileSuperseded
            } else if dropped {
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

        if let Some(message) = self.volatile_terminal_context_message() {
            let (role, _text) = describe_message_full(&message);
            let item_index = items.len();
            control_message_item_indices.push(item_index);
            control_messages.push(message);
            items.push(ContextProjectionItem {
                id: "terminal-live".to_string(),
                source: ContextProjectionSource::VolatileTerminal,
                source_index: messages.len(),
                role,
                lifecycle: ContextProjectionLifecycle::Recent,
                cache_class: ContextProjectionCacheClass::VolatileTail,
                trust_class: ContextProjectionTrustClass::UntrustedData,
                selection: ContextProjectionSelection::Selected,
                transform: ContextProjectionTransform::None,
                tokens: 0,
            });
        }

        let mut estimate = tool_schema.map(|tool_schema| {
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

        let (wire_messages, wire_source_indices) = if self.smol_applies_to_active_persona() {
            self.smol_outgoing_messages_with_sources(control_messages.clone())
        } else {
            (
                control_messages.clone(),
                (0..control_messages.len()).map(Some).collect(),
            )
        };
        let assistant_response_ids = wire_messages
            .iter()
            .zip(wire_source_indices.iter().copied())
            .filter_map(|(message, source_index)| {
                if !matches!(message, ChatCompletionRequestMessage::Assistant(_)) {
                    return None;
                }
                let id = source_index
                    .and_then(|index| control_message_item_indices.get(index))
                    .and_then(|&index| items.get(index))
                    .filter(|item| item.transform == ContextProjectionTransform::None)
                    .and_then(|item| item.source.message_index())
                    .and_then(|index| message_ids.get(index))
                    .and_then(|id| self.native_response_bindings.get(id))
                    .cloned();
                Some(id)
            })
            .collect();
        let wire_request_context = crate::provider::ProviderRequestContext {
            assistant_response_ids: Some(assistant_response_ids),
            response_id: None,
        };
        let mut estimate_error = None;
        let mut estimate_fingerprint = None;
        let mut native_message_budgets = HashMap::new();
        if let Some(tool_schema) = tool_schema {
            let options = self
                .verification_reasoning_for_request()
                .map(crate::provider::ProviderRequestOptions::with_reasoning)
                .unwrap_or_default();
            let prepared = self.provider.prepare_request_for_estimation(
                &wire_messages,
                tool_schema.tools(),
                options,
                &wire_request_context,
            );
            match prepared.and_then(|prepared| {
                estimate_fingerprint = Some(prepared.estimate_fingerprint().map_err(|error| {
                    crate::provider::ProviderFailure::configuration(format!(
                        "Failed to identify prepared prompt: {error}"
                    ))
                })?);
                let counted = self
                    .prompt_estimator
                    .estimate_prepared_for_report(
                        &prepared,
                        tool_schema.report_tool_schema_tokens(),
                        self.budget.context_budget_tokens,
                    )
                    .map_err(|error| {
                        crate::provider::ProviderFailure::configuration(format!("{error:#}"))
                    })?;
                let native_by_slot = prepared
                    .native_budget
                    .iter()
                    .map(|native| (native.assistant_slot, native))
                    .collect::<HashMap<_, _>>();
                // SMOL can remove old assistant rows; use its source sidecar,
                // rather than the ordinal of untransformed control rows.
                let mut assistant_slot = 0usize;
                for (message, source) in wire_messages.iter().zip(wire_source_indices) {
                    if !matches!(message, ChatCompletionRequestMessage::Assistant(_)) {
                        continue;
                    }
                    if let Some(native) = native_by_slot.get(&assistant_slot)
                        && let Some(item_index) = source
                            .and_then(|source| control_message_item_indices.get(source))
                            .copied()
                    {
                        let allowance = if native.opaque {
                            native
                                .opaque_allowance_tokens
                                .map(|tokens| {
                                    usize::try_from(tokens)
                                        .unwrap_or(self.budget.context_budget_tokens)
                                        .min(self.budget.context_budget_tokens)
                                })
                                .unwrap_or(self.budget.context_budget_tokens)
                        } else {
                            0
                        };
                        let text_tokens = if native.reasoning_text.is_empty() {
                            0
                        } else {
                            self.prompt_estimator
                                .estimate_text_for_report(&native.reasoning_text)
                        };
                        let extra = text_tokens.saturating_add(allowance);
                        items[item_index].tokens = items[item_index].tokens.saturating_add(extra);
                        if let Some(index) = items[item_index].source.message_index() {
                            source_message_tokens[index] =
                                source_message_tokens[index].saturating_add(extra);
                            native_message_budgets.insert(
                                index,
                                crate::context_view::NativeMessageBudget {
                                    reasoning_tokens: text_tokens,
                                    opaque_reserve_tokens: allowance,
                                    opaque_cost_unknown: native.opaque
                                        && native.opaque_allowance_tokens.is_none(),
                                },
                            );
                        }
                    }
                    assistant_slot += 1;
                }
                Ok(counted)
            }) {
                Ok(counted) => estimate = Some(counted),
                Err(error) => {
                    estimate_error = Some(error.to_string());
                    let mut unavailable = PromptEstimate::heuristic(
                        self.budget.context_budget_tokens,
                        tool_schema.report_tool_schema_tokens(),
                        TokenCounterKind::Heuristic,
                    );
                    unavailable.native_cost_unknown = true;
                    estimate = Some(unavailable);
                }
            }
        }
        if self.smol_applies_to_active_persona() {
            mark_smol_transforms(&mut items, &control_messages, &wire_messages);
        }

        ContextProjection {
            control_messages,
            wire_messages,
            wire_request_context,
            source_message_tokens,
            estimate,
            estimate_error,
            estimate_fingerprint,
            native_message_budgets,
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
            MessageProvenance::Terminal,
            MessageProvenance::ToolImage,
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
