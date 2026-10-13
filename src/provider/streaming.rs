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
        return tokio::select! {
            biased;
            _ = token.cancelled() => Ok(None),
            error = sse::error_from_response(response) => Err(error),
        };
    }

    Ok(Some(response))
}

/// Shared stream tail for the transports. Flush any text a [`ThinkTagSplitter`]
/// held back at a possible `<think>`/tag boundary — pushing the final visible
/// tail into `content` and to the sink, and any final reasoning tail to the sink
/// — then emit `assistant_done` when the assistant produced any text. `splitter`
/// is `None` for transports that don't parse think tags (Codex). Return the
/// reasoning suffix so transports can preserve its exact bytes in native replay.
pub(crate) fn finish_text_stream(
    splitter: Option<&mut ThinkTagSplitter>,
    content: &mut String,
    sink: &SharedSink,
) -> String {
    let mut reasoning_tail = String::new();
    if let Some(splitter) = splitter {
        let tail = splitter.finish();
        if !tail.visible.is_empty() {
            content.push_str(&tail.visible);
            sink.assistant_delta(&tail.visible);
        }
        if !tail.reasoning.is_empty() {
            sink.reasoning_delta(&tail.reasoning);
            reasoning_tail = tail.reasoning;
        }
    }

    if !content.is_empty() {
        sink.assistant_done();
    }
    reasoning_tail
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_http_error_body() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (headers_tx, headers_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 32\r\n\r\n")
                .await
                .unwrap();
            let _ = headers_tx.send(());
            std::future::pending::<()>().await;
        });
        let token = CancellationToken::new();
        let run_token = token.clone();
        let run = tokio::spawn(async move {
            send_json_stream(
                reqwest::Client::new().get(format!("http://{address}")),
                &run_token,
            )
            .await
        });
        headers_rx.await.unwrap();
        // Let the response headers resolve and enter the body-read branch.
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        token.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        server.abort();
        assert!(result.is_none());
    }
}
