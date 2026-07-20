use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
use tokio::sync::{Barrier, Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::background::BackgroundTaskRegistry;
use crate::output::{OutputSink, SharedSink, StdoutSink, ToolCallStart};
use crate::provider::{
    EstimateConfidence, InputCacheUsage, ModelPricing, PromptEstimate, PromptEstimator, Provider,
    ProviderFailure, StreamedResponse, TokenCounterKind,
};
use crate::tool::test_utils::TestFixture;
use crate::tool::{
    OutputTruncationContext, ReadCoverage, ReadEvidence, ReadWindow, Tool, ToolOutput, ToolRegistry,
};

use super::{
    Agent, AgentMode, AgentRunResult, CompactionRequest, CompactionSummarySource,
    ContextControlAction, ContextInclusion, ContextNode, ContextNodeKind, ContextPreviewInput,
    ContextPreviewUserInput, ContextReport, ContextRewriteKind, ContextRole, ContextStubReason,
    PLANNING_RESEARCH_TURN_LIMIT, PreflightPerfCapture, QueuedUserMessage,
    QueuedUserMessageCommand, ReadAdmission, ReviewScope, ToolContextDetail, ToolContextResult,
    ToolImageContext, compact_tool_arguments_for_context, diff_context_preview, system_message,
};

use super::controls::projection::{ContextProjectionSelection, ContextProjectionTransform};
use super::{
    MENTION_DIRECTORY_ENTRY_CAP, MENTION_FILE_CAP_BYTES, expand_mentions_for_context,
    expand_mentions_for_context_with_evidence, tool_call_batches, tool_call_batches_with_yolo,
};

mod git;
mod inspection_reuse;
mod mocks;
mod read_efficiency_replay;
mod read_persistence;
mod verification;

use git::*;
use mocks::*;

fn find_context_node<'a>(
    nodes: &'a [ContextNode],
    kind: ContextNodeKind,
    label: &str,
) -> Option<&'a ContextNode> {
    for node in nodes {
        if node.kind == kind && node.label.contains(label) {
            return Some(node);
        }
        if let Some(found) = find_context_node(&node.children, kind, label) {
            return Some(found);
        }
    }
    None
}

fn top_context_node(nodes: &[ContextNode], kind: ContextNodeKind) -> Option<&ContextNode> {
    nodes.iter().find(|node| node.kind == kind)
}

fn counted_ledger_tokens(report: &ContextReport) -> usize {
    report.ledger.iter().map(ContextNode::counted_tokens).sum()
}

fn assistant_tool_call_message(
    id: &str,
    name: &str,
    arguments: &str,
) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(
        async_openai::types::chat::ChatCompletionRequestAssistantMessageArgs::default()
            .tool_calls(vec![
                async_openai::types::chat::ChatCompletionMessageToolCalls::Function(
                    async_openai::types::chat::ChatCompletionMessageToolCall {
                        id: id.to_string(),
                        function: async_openai::types::chat::FunctionCall {
                            name: name.to_string(),
                            arguments: arguments.to_string(),
                        },
                    },
                ),
            ])
            .build()
            .expect("assistant tool-call message should build"),
    )
}

fn tool_result_message(id: &str, content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Tool(
        async_openai::types::chat::ChatCompletionRequestToolMessageArgs::default()
            .tool_call_id(id)
            .content(content)
            .build()
            .expect("tool result message should build"),
    )
}

fn image_user_message() -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(
            async_openai::types::chat::ChatCompletionRequestUserMessageArgs::default()
                .content(
                    async_openai::types::chat::ChatCompletionRequestUserMessageContent::Array(
                        vec![
                            async_openai::types::chat::ChatCompletionRequestUserMessageContentPart::Text(
                                async_openai::types::chat::ChatCompletionRequestMessageContentPartText {
                                    text: "inspect this image".to_string(),
                                },
                            ),
                            async_openai::types::chat::ChatCompletionRequestUserMessageContentPart::ImageUrl(
                                async_openai::types::chat::ChatCompletionRequestMessageContentPartImage {
                                    image_url: async_openai::types::chat::ImageUrl {
                                        url: "data:image/png;base64,AAAA".to_string(),
                                        detail: None,
                                    },
                                },
                            ),
                        ],
                    ),
                )
                .build()
                .expect("image user message should build"),
        )
}

/// Build a `ChatCompletionRequestMessage::User` for tests. Mirrors
/// the builder used by `Agent::run` so the message survives the
/// `to_value().get("content")` round-trip above.
fn test_user_message(content: &str) -> ChatCompletionRequestMessage {
    super::user_text_message(content)
}

fn test_system_message(content: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::System(
        async_openai::types::chat::ChatCompletionRequestSystemMessageArgs::default()
            .content(content.to_string())
            .build()
            .expect("system message builder should succeed with static content"),
    )
}

/// Extract the text content of a `ChatCompletionRequestMessage::User`
/// by serialising to JSON and reading the `content` field. Mirrors
/// `system_content` but for user messages. Returns an empty string
/// if the message is the wrong variant or the content is not a
/// plain string.
fn user_message_content(message: &ChatCompletionRequestMessage) -> String {
    message_content(message)
}

fn user_messages_in(messages: &[ChatCompletionRequestMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| {
            matches!(message, ChatCompletionRequestMessage::User(_))
                && !super::is_project_state_message(message)
        })
        .map(message_content)
        .collect()
}

fn project_state_messages_in(messages: &[ChatCompletionRequestMessage]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| super::is_project_state_message(message))
        .map(message_content)
        .collect()
}

fn message_content(message: &ChatCompletionRequestMessage) -> String {
    let value = serde_json::to_value(message).unwrap_or(serde_json::Value::Null);
    value
        .get("content")
        .and_then(|c| match c {
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

mod background;
mod batching;
mod compaction;
mod context;
mod episodes;
mod hook_recovery;
mod mcp_injection;
mod memory_recall;
mod mentions;
mod persona_tools;
mod prompt_review;
mod read_dedup;
mod run_loop;
mod stall_guard;
mod tool_execution;
mod web_injection;
