use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::Result;
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::output::SharedSink;
use crate::provider::{InputCacheUsage, Provider, StreamedResponse, TokenUsage, ToolCall};

use super::*;

/// Synthetic provider id reported for mock-mode eval runs.
pub(crate) const MOCK_PROVIDER_ID: &str = "mock-eval";
/// Synthetic model id reported for mock-mode eval runs.
pub(crate) const MOCK_MODEL: &str = "mock-eval-v0";

/// Default assistant message a mock task emits once its scripted tool calls are
/// exhausted.
pub(crate) fn default_final_response() -> String {
    "Done.".to_string()
}

/// A single scripted file write performed by a mock task.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MockWrite {
    /// Worktree-relative destination path.
    pub(crate) path: String,
    /// File contents to write.
    pub(crate) content: String,
}

/// One raw tool call emitted by a scripted mock turn. `arguments` stays a
/// string so release scenarios can exercise malformed JSON exactly as a model
/// produced it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MockToolCall {
    #[serde(default)]
    pub(crate) id: Option<String>,
    pub(crate) name: String,
    pub(crate) arguments: String,
}

/// Tool calls emitted together in one model response.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MockToolTurn {
    pub(crate) calls: Vec<MockToolCall>,
}

/// Deterministic in-process [`Provider`] that replays a [`MockScript`] of reads,
/// writes, and a final response across successive agent turns.
#[derive(Debug, Clone)]
pub(crate) struct MockEvalProvider {
    script: MockScript,
    seed: u64,
    /// Logical script step (reads → writes → final); advances only on the
    /// non-error path so transient errors don't consume the script.
    step: Arc<AtomicUsize>,
    /// Total provider calls, used to inject the leading transient errors.
    calls: Arc<AtomicUsize>,
    /// Previous ordinary request body used by opt-in prompt-cache simulation.
    previous_request: Arc<Mutex<Option<String>>>,
    /// Makes a scripted cancellation wait a one-shot pause across resume.
    cancellation_wait_consumed: Arc<AtomicBool>,
}

impl MockEvalProvider {
    /// Build a mock provider that replays `script`, deriving synthetic token
    /// usage from `seed`.
    pub(crate) fn new(script: MockScript, seed: u64) -> Self {
        Self {
            script,
            seed,
            step: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
            previous_request: Arc::new(Mutex::new(None)),
            cancellation_wait_consumed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn next_step(&self) -> usize {
        self.step.fetch_add(1, Ordering::Relaxed)
    }

    fn usage_for(
        &self,
        step: usize,
        tool_count: usize,
        has_final: bool,
        request: &str,
    ) -> TokenUsage {
        let seed_offset = u32::try_from(self.seed % 17).unwrap_or(0);
        let step_offset = u32::try_from(step).unwrap_or(u32::MAX).saturating_mul(11);
        let tool_offset = u32::try_from(tool_count)
            .unwrap_or(u32::MAX)
            .saturating_mul(3);
        let simulated_cache = self.script.simulate_prompt_cache.then(|| {
            let prompt_tokens = u32::try_from(request.len().saturating_add(3) / 4)
                .unwrap_or(u32::MAX)
                .max(1);
            let mut previous = self
                .previous_request
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let read_tokens = previous
                .as_deref()
                .map(|prior| common_prefix_len(prior.as_bytes(), request.as_bytes()) / 4)
                .and_then(|tokens| u64::try_from(tokens).ok())
                .unwrap_or(0)
                .min(u64::from(prompt_tokens));
            *previous = Some(request.to_string());
            (
                prompt_tokens,
                InputCacheUsage::new(read_tokens, 0, u64::from(prompt_tokens)),
            )
        });
        TokenUsage {
            prompt_tokens: simulated_cache.map_or_else(
                || {
                    100u32
                        .saturating_add(seed_offset)
                        .saturating_add(step_offset)
                        .saturating_add(tool_offset)
                },
                |(prompt_tokens, _)| prompt_tokens,
            ),
            completion_tokens: if has_final {
                24u32.saturating_add(seed_offset)
            } else {
                8u32.saturating_add(tool_offset)
            },
            input_cache: simulated_cache.map(|(_, cache)| cache),
        }
    }
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

#[async_trait]
impl Provider for MockEvalProvider {
    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> crate::provider::ProviderResult<StreamedResponse> {
        let request = serde_json::to_string(&(messages, tools)).unwrap_or_default();
        for forbidden in &self.script.forbidden_request_substrings {
            if request.contains(forbidden) {
                return Err(crate::provider::ProviderFailure::configuration(format!(
                    "mock provider request contained forbidden text: {forbidden}",
                )));
            }
        }
        if let Some(expected) = self.script.expected_user_message_count {
            let actual = messages
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        ChatCompletionRequestMessage::User(user) if user.name.is_none()
                    )
                })
                .count();
            if actual != expected {
                return Err(crate::provider::ProviderFailure::configuration(format!(
                    "mock expected {expected} human user messages, provider request carried {actual}",
                )));
            }
        }
        let is_compaction_summary = tools.is_empty()
            && (request.contains("You summarize prior chat history for context compaction")
                || request.contains(
                    "You maintain a rolling summary of prior chat history for context compaction",
                ));
        if is_compaction_summary {
            return Ok(StreamedResponse {
                content: "# Compacted Context Summary\n\n## Current goal\n- Complete the scripted eval.\n\n## Decisions\n- Preserve the latest task state.\n\n## Constraints\n- None.\n\n## Files touched\n- See visible context.\n\n## Tool findings\n- See visible context.\n\n## Open tasks\n- Continue the script.\n\n## Risks\n- None."
                    .to_string(),
                ..StreamedResponse::default()
            });
        }
        // The hidden episode-card call (episode eviction). Like the hidden
        // compaction summary above, it consumes no script step and no
        // transient-error budget, and never counts as the final request.
        let is_episode_card = tools.is_empty()
            && request.contains("You write a compact completion card for an archived task episode");
        if is_episode_card {
            return Ok(StreamedResponse {
                content: "## Episode card\n- Goal: complete the archived phase.\n- Outcome: the phase finished; artifacts are on disk.\n- Decisions: none recorded.\n- Files touched: see card.\n- Gotchas: none."
                    .to_string(),
                ..StreamedResponse::default()
            });
        }
        // Inject the scripted leading transient errors. They are retryable, so
        // the agent's `chat_stream_with_retry` should recover and the task still
        // complete — exercising the retry path end to end.
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        if call < self.script.transient_errors {
            return Err(crate::provider::ProviderFailure::stream_decode(
                "mock transient stream error",
            ));
        }

        let scripted_call = call.saturating_sub(self.script.transient_errors);
        if scripted_call < self.script.truncated_responses {
            return Ok(StreamedResponse {
                terminal: crate::provider::StreamTerminal::Incomplete(
                    crate::provider::FinishReason::Length,
                ),
                ..StreamedResponse::default()
            });
        }

        let cancellation_step = self
            .script
            .wait_for_cancellation
            .then_some(0)
            .or(self.script.wait_for_cancellation_after_tool_turns);
        if cancellation_step == Some(self.step.load(Ordering::Relaxed))
            && !self
                .cancellation_wait_consumed
                .swap(true, Ordering::Relaxed)
        {
            cancellation_token.cancelled().await;
            return Ok(StreamedResponse::interrupted());
        }

        let step = self.next_step();
        let tool_calls = if !self.script.tool_turns.is_empty() {
            self.script
                .tool_turns
                .get(step)
                .map(|turn| {
                    turn.calls
                        .iter()
                        .enumerate()
                        .map(|(index, call)| ToolCall {
                            id: call
                                .id
                                .clone()
                                .unwrap_or_else(|| format!("mock-tool-{step}-{index}")),
                            name: call.name.clone(),
                            arguments: call.arguments.clone(),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        } else {
            match step {
                0 if !self.script.read.is_empty() => self
                    .script
                    .read
                    .iter()
                    .enumerate()
                    .map(|(index, path)| ToolCall {
                        id: format!("mock-read-{index}"),
                        name: "read".to_string(),
                        arguments: json!({ "path": path }).to_string(),
                    })
                    .collect::<Vec<_>>(),
                0 | 1 if !self.script.write.is_empty() => self
                    .script
                    .write
                    .iter()
                    .enumerate()
                    .map(|(index, write)| ToolCall {
                        id: format!("mock-write-{index}"),
                        name: "write".to_string(),
                        arguments: json!({
                            "path": write.path,
                            "content": write.content,
                        })
                        .to_string(),
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            }
        };

        let tool_count = tool_calls.len();
        if tool_calls.is_empty() {
            for required in &self.script.final_request_contains {
                if !request.contains(required) {
                    return Err(crate::provider::ProviderFailure::configuration(format!(
                        "mock final provider request did not contain: {required}",
                    )));
                }
            }
            // Complement of final_request_contains, checked only here — the
            // first ordinary request after the script has no tool calls.
            // Hidden card/summary calls and transient retries returned above
            // and never qualify as final.
            for forbidden in &self.script.final_request_forbidden_substrings {
                if request.contains(forbidden) {
                    return Err(crate::provider::ProviderFailure::configuration(format!(
                        "mock final provider request contained forbidden text: {forbidden}",
                    )));
                }
            }
            sink.assistant_delta(&self.script.final_response);
            sink.assistant_done();
            Ok(StreamedResponse {
                content: self.script.final_response.clone(),
                usage: Some(self.usage_for(step, 0, true, &request)),
                ..StreamedResponse::default()
            })
        } else {
            let content = if self.script.tool_turn_content_chars == 0 {
                String::new()
            } else {
                format!(
                    "Context-pressure turn {step}: {}",
                    "x".repeat(self.script.tool_turn_content_chars)
                )
            };
            Ok(StreamedResponse {
                content,
                tool_calls,
                usage: Some(self.usage_for(step, tool_count, false, &request)),
                ..StreamedResponse::default()
            })
        }
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(vec![MOCK_MODEL.to_string()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::StdoutSink;

    fn script(transient_errors: usize) -> MockScript {
        MockScript {
            read: Vec::new(),
            write: Vec::new(),
            final_response: "Done".to_string(),
            transient_errors,
            truncated_responses: 0,
            expected_user_message_count: None,
            forbidden_request_substrings: Vec::new(),
            final_request_contains: Vec::new(),
            final_request_forbidden_substrings: Vec::new(),
            tool_turns: Vec::new(),
            tool_turn_content_chars: 0,
            simulate_prompt_cache: false,
            wait_for_cancellation: false,
            wait_for_cancellation_after_tool_turns: None,
        }
    }

    #[tokio::test]
    async fn transient_errors_are_retryable_then_the_script_runs() {
        let provider = MockEvalProvider::new(script(2), 7);
        let sink: SharedSink = Arc::new(StdoutSink);

        // The first two calls fail with a *retryable* provider error, so the
        // agent's `chat_stream_with_retry` would back off and retry...
        for _ in 0..2 {
            let err = provider
                .chat_stream(&[], &[], CancellationToken::new(), sink.clone())
                .await
                .expect_err("a scripted transient error should fail the call");
            assert!(err.is_retryable());
        }

        // ...then the script proceeds and the final response is delivered, so
        // the task still completes despite the transient failures.
        let response = provider
            .chat_stream(&[], &[], CancellationToken::new(), sink.clone())
            .await
            .expect("after the transient errors, the script runs");
        assert_eq!(response.content, "Done");
    }

    fn user_message(text: &str) -> ChatCompletionRequestMessage {
        ChatCompletionRequestMessage::User(
            async_openai::types::chat::ChatCompletionRequestUserMessageArgs::default()
                .content(text.to_string())
                .build()
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn final_request_forbidden_text_is_checked_only_on_the_final_request() {
        let mut scripted = script(0);
        scripted.tool_turns = vec![MockToolTurn {
            calls: vec![MockToolCall {
                id: None,
                name: "read".to_string(),
                arguments: "{\"path\":\"README.md\"}".to_string(),
            }],
        }];
        scripted.final_request_forbidden_substrings = vec!["EPHEMERAL_BULK".to_string()];
        let provider = MockEvalProvider::new(scripted, 7);
        let sink: SharedSink = Arc::new(StdoutSink);

        // The bulk may appear on an ordinary (non-final) request...
        provider
            .chat_stream(
                &[user_message("EPHEMERAL_BULK payload")],
                &[],
                CancellationToken::new(),
                sink.clone(),
            )
            .await
            .expect("forbidden-final text is allowed before the final request");
        // ...but the final request must have shed it.
        let err = provider
            .chat_stream(
                &[user_message("EPHEMERAL_BULK payload")],
                &[],
                CancellationToken::new(),
                sink.clone(),
            )
            .await
            .expect_err("the final request still carrying the text must fail");
        assert!(err.to_string().contains("EPHEMERAL_BULK"), "{err}");
    }

    #[tokio::test]
    async fn hidden_episode_card_call_answers_without_consuming_script_steps() {
        let mut scripted = script(0);
        scripted.tool_turns = vec![MockToolTurn {
            calls: vec![MockToolCall {
                id: None,
                name: "read".to_string(),
                arguments: "{\"path\":\"README.md\"}".to_string(),
            }],
        }];
        let provider = MockEvalProvider::new(scripted, 7);
        let sink: SharedSink = Arc::new(StdoutSink);

        let card = provider
            .chat_stream(
                &[user_message(
                    "You write a compact completion card for an archived task episode from a coding session.",
                )],
                &[],
                CancellationToken::new(),
                sink.clone(),
            )
            .await
            .expect("the hidden card call is answered");
        assert!(
            card.content.starts_with("## Episode card"),
            "{}",
            card.content
        );
        assert!(card.tool_calls.is_empty());

        // The card call consumed neither a step nor the error budget: the next
        // ordinary call still gets scripted tool turn 0.
        let next = provider
            .chat_stream(&[], &[], CancellationToken::new(), sink.clone())
            .await
            .unwrap();
        assert_eq!(next.tool_calls.len(), 1);
        assert_eq!(next.tool_calls[0].name, "read");
    }
}
