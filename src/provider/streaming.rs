use tokio_util::sync::CancellationToken;

use crate::output::SharedSink;
use crate::provider::ProviderResult;
use crate::provider::sse;
use crate::provider::think_tags::ThinkTagSplitter;

pub(crate) async fn send_json_stream(
    builder: reqwest::RequestBuilder,
    token: &CancellationToken,
) -> ProviderResult<Option<reqwest::Response>> {
    let Some(response) = sse::send_cancellable(builder, token).await? else {
        return Ok(None);
    };

    if !response.status().is_success() {
        return Err(sse::error_from_response(response).await);
    }

    Ok(Some(response))
}

/// Shared stream tail for the transports. Flush any text a [`ThinkTagSplitter`]
/// held back at a possible `<think>`/tag boundary — pushing the final visible
/// tail into `content` and to the sink, and any final reasoning tail to the sink
/// — then emit `assistant_done` when the assistant produced any text. `splitter`
/// is `None` for transports that don't parse think tags (Codex).
pub(crate) fn finish_text_stream(
    splitter: Option<&mut ThinkTagSplitter>,
    content: &mut String,
    sink: &SharedSink,
) -> usize {
    let mut reasoning_chars = 0;
    if let Some(splitter) = splitter {
        let tail = splitter.finish();
        if !tail.visible.is_empty() {
            content.push_str(&tail.visible);
            sink.assistant_delta(&tail.visible);
        }
        if !tail.reasoning.is_empty() {
            reasoning_chars = tail.reasoning.chars().count();
            sink.reasoning_delta(&tail.reasoning);
        }
    }

    if !content.is_empty() {
        sink.assistant_done();
    }
    reasoning_chars
}

/// Shared tail of every transport's reasoning-replay persistence: stash one
/// turn's reasoning payload, keyed by the turn's FIRST tool-call id — the same
/// id the rebuilt assistant message carries, which is how the payload finds its
/// way back ahead of that call on the next request. This keying contract is the
/// invariant all three transports must agree on; *what* qualifies as a
/// replayable payload is per-provider policy and stays at the call site
/// (Anthropic: any non-empty thinking blocks; Codex: exactly one reasoning
/// item, more can't replay in an order the Responses API accepts; OpenCode:
/// only `reasoning_content`-echoing backends). `None` payloads and turns
/// without tool calls stash nothing — text-only final turns need no replay.
pub(crate) fn stash_reasoning_for_replay<T>(
    store: &std::sync::Mutex<std::collections::HashMap<String, T>>,
    tool_calls: &[crate::provider::ToolCall],
    payload: Option<T>,
) -> ProviderResult<()> {
    let (Some(payload), Some(first_call)) = (payload, tool_calls.first()) else {
        return Ok(());
    };
    store
        .lock()
        .map_err(|_| {
            crate::provider::ProviderFailure::configuration(
                "reasoning replay state lock is poisoned",
            )
        })?
        .insert(first_call.id.clone(), payload);
    Ok(())
}
