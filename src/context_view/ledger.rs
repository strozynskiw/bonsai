use std::collections::HashMap;

use async_openai::types::chat::ChatCompletionRequestMessage;

use crate::provider::PromptEstimator;

use super::*;

#[derive(Debug, Default)]
pub(crate) struct ToolLedgerCalls {
    calls: Vec<ToolLedgerCall>,
    by_id: HashMap<String, usize>,
    output_count: usize,
}

impl ToolLedgerCalls {
    pub(crate) fn output_count(&self) -> usize {
        self.output_count
    }

    pub(crate) fn record_input(
        &mut self,
        call: AssistantToolCallContext,
        row_metadata: ContextTokenMetadata,
        estimator: &PromptEstimator,
        source: ContextSourceRef,
    ) {
        let index = self.ensure_call(
            &call.call_id,
            &call.name,
            row_metadata,
            ContextInclusion::Included,
        );
        let input_text = pretty_tool_arguments(&call.arguments);
        let input_tokens = estimator.estimate_text_for_report(&input_text);
        let ledger_call = &mut self.calls[index];
        if ledger_call.name.as_deref() == Some("tool") {
            ledger_call.name = Some(call.name.clone());
        }
        ledger_call.name_fallback = call.name;
        ledger_call.arguments = Some(input_text);
        ledger_call.input_tokens = input_tokens;
        ledger_call.input_source = Some(source);
    }

    pub(crate) fn record_output(
        &mut self,
        message: &ChatCompletionRequestMessage,
        tokens: usize,
        row_metadata: ContextTokenMetadata,
        estimator: &PromptEstimator,
        details: &HashMap<String, ToolContextDetail>,
        source: ContextSourceRef,
    ) {
        self.output_count = self.output_count.saturating_add(1);
        let call_id = tool_message_call_id(message)
            .unwrap_or_else(|| format!("tool-result-{}", self.output_count));
        let detail = details.get(&call_id);
        let name = detail
            .map(tool_detail_ledger_name)
            .unwrap_or_else(|| "tool result".to_string());
        let index = self.ensure_call(&call_id, &name, row_metadata, ContextInclusion::Included);
        let output_text =
            message_content_text(&serde_json::to_value(message).unwrap_or(serde_json::Value::Null));
        if let Some(detail) = detail {
            debug_assert_eq!(detail.call_id, call_id);
        }
        let output_children = tool_result_children(
            ContextNodeId::tool(&call_id).as_str(),
            &call_id,
            &output_text,
            detail,
            row_metadata,
            estimator,
            source.clone(),
        );
        let call = &mut self.calls[index];
        if let Some(detail) = detail {
            call.name = Some(tool_detail_ledger_name(detail));
        }
        if call.arguments.is_none()
            && let Some(detail) = detail
        {
            let input_text = pretty_tool_arguments(&detail.arguments);
            call.input_tokens = estimator.estimate_text_for_report(&input_text);
            call.arguments = Some(input_text);
        }
        call.output_tokens = call.output_tokens.saturating_add(tokens);
        call.output_text = Some(output_text);
        call.output_children.extend(output_children);
    }

    pub(crate) fn push_orphan_result(&mut self, node: ContextNode) {
        let row_metadata = ContextTokenMetadata {
            source: node.source,
            confidence: node.confidence,
        };
        self.calls.push(ToolLedgerCall {
            call_id: node.id.as_str().to_string(),
            name: Some(node.label.clone()),
            name_fallback: node.label.clone(),
            arguments: None,
            input_tokens: 0,
            input_source: None,
            output_tokens: node.tokens,
            output_text: Some(node.preview.clone()),
            inclusion: node.inclusion,
            output_children: vec![node],
            row_metadata,
        });
    }

    pub(crate) fn into_nodes(self) -> Vec<ContextNode> {
        self.calls
            .into_iter()
            .map(ToolLedgerCall::into_node)
            .collect()
    }

    fn ensure_call(
        &mut self,
        call_id: &str,
        name: &str,
        row_metadata: ContextTokenMetadata,
        inclusion: ContextInclusion,
    ) -> usize {
        if let Some(index) = self.by_id.get(call_id).copied() {
            if self.calls[index].name.as_deref() == Some("tool") {
                self.calls[index].name = Some(name.to_string());
            }
            return index;
        }
        let index = self.calls.len();
        self.calls.push(ToolLedgerCall {
            call_id: call_id.to_string(),
            name: Some(name.to_string()),
            name_fallback: name.to_string(),
            arguments: None,
            input_tokens: 0,
            input_source: None,
            output_tokens: 0,
            output_text: None,
            inclusion,
            output_children: Vec::new(),
            row_metadata,
        });
        self.by_id.insert(call_id.to_string(), index);
        index
    }
}

fn tool_detail_ledger_name(detail: &ToolContextDetail) -> String {
    let Some(evidence) = detail.read_evidence.as_ref() else {
        return detail.name.clone();
    };
    format!("{} · {}", detail.name, evidence.freshness().label())
}

#[derive(Debug)]
struct ToolLedgerCall {
    call_id: String,
    name: Option<String>,
    name_fallback: String,
    arguments: Option<String>,
    input_tokens: usize,
    input_source: Option<ContextSourceRef>,
    output_tokens: usize,
    output_text: Option<String>,
    inclusion: ContextInclusion,
    output_children: Vec<ContextNode>,
    row_metadata: ContextTokenMetadata,
}

impl ToolLedgerCall {
    fn into_node(self) -> ContextNode {
        let label_name = self.name.unwrap_or(self.name_fallback);
        let mut children = Vec::new();
        if let Some(arguments) = &self.arguments {
            children.push(
                ContextNode::leaf(
                    ContextNodeId::tool_child(&self.call_id, "input"),
                    ContextNodeKind::ToolInput,
                    ContextInclusion::Included,
                    Some(ContextRole::Tool),
                    "Input JSON",
                    self.input_tokens,
                    arguments,
                    self.row_metadata,
                )
                .with_sources(self.input_source.clone().into_iter().chain(
                    std::iter::once(ContextSourceRef::new(
                        ContextSourceKind::ToolInput,
                        self.call_id.clone(),
                        "assistant tool call",
                    )),
                )),
            );
        }
        children.extend(self.output_children);
        let sources = child_sources(&children).into_iter().chain(std::iter::once(
            ContextSourceRef::new(
                ContextSourceKind::ToolCall,
                self.call_id.clone(),
                "tool call",
            )
            .with_detail(label_name.clone()),
        ));
        let text = match (&self.arguments, &self.output_text) {
            (Some(arguments), Some(output)) => format!("{arguments}\n\n{output}"),
            (Some(arguments), None) => arguments.clone(),
            (None, Some(output)) => output.clone(),
            (None, None) => String::new(),
        };
        message_node_with_framing(
            ContextNode::parent(
                ContextNodeId::tool(&self.call_id),
                ContextNodeKind::ToolCall,
                self.inclusion,
                Some(ContextRole::Tool),
                format!("{label_name} · {}", self.call_id),
                self.input_tokens.saturating_add(self.output_tokens),
                &text,
                self.row_metadata,
                children,
            )
            .with_sources(sources),
        )
    }
}
