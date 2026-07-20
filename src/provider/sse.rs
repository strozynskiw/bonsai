//! Shared HTTP + Server-Sent-Events transport for the streaming providers.
//!
//! Anthropic, Codex, and the OpenAI-compatible provider all spoke the same
//! low-level dialect — send a cancellable request, map non-2xx to
//! [`ProviderFailure::Http`], then frame an SSE byte stream on `\n\n` boundaries
//! — with subtly divergent copies. This module is the single source of truth.
//!
//! The two protocol families differ only in framing: Anthropic uses
//! `event:` + `data:` pairs, while OpenAI/Codex send bare `data:` lines with a
//! `[DONE]` sentinel. A single `on_event(event_name: Option<&str>, data: &str)`
//! callback covers both — Anthropic receives `Some(name)`, the others `None`.

use futures::StreamExt;
use serde_json::Value;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;

use crate::provider::{ProviderFailure, ProviderResult};

pub(crate) const STREAM_CHUNK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Map a non-2xx response to [`ProviderFailure::Http`], reading the status, the
/// `retry-after` header, and a truncated body.
pub(crate) async fn error_from_response(response: reqwest::Response) -> ProviderFailure {
    let status = response.status();
    let retry_after_secs = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let text = response.text().await.unwrap_or_default();
    ProviderFailure::Http {
        status: status.as_u16(),
        message: format!("{:.500}", text),
        retry_after_secs,
    }
}

/// Parse one SSE `data:` payload as JSON. Returns `Ok(None)` for keepalives,
/// `[DONE]` variants, and other non-JSON sentinels (callers skip them);
/// `Ok(Some(value))` for a parsed frame; and `Err` only when a frame that is
/// meant to be a JSON object (`{…}`) fails to parse — genuine corruption or
/// truncation, surfaced as a retryable `stream_decode` error.
pub(crate) fn parse_frame(data: &str) -> ProviderResult<Option<Value>> {
    match serde_json::from_str(data) {
        Ok(value) => Ok(Some(value)),
        Err(err) if data.trim_start().starts_with('{') => Err(ProviderFailure::stream_decode(
            format!("malformed stream frame: {err}"),
        )),
        Err(_) => Ok(None),
    }
}

/// Build a retryable-classified error from an in-stream error frame: truncate
/// `detail` to 500 chars, keep the structured `type`/`code`, and wrap via
/// [`ProviderFailure::from_stream_error`] so transient overload/rate-limit classes
/// are retried instead of failing the turn as a plain error.
pub(crate) fn stream_error(
    error_type: Option<&str>,
    error_code: Option<&str>,
    prefix: &str,
    detail: &str,
) -> ProviderFailure {
    ProviderFailure::from_stream_error(error_type, error_code, format!("{prefix}: {detail:.500}"))
}

/// [`stream_error`] from a located in-stream error *object*: extract the
/// `type`/`code`/`message` fields every transport shares. Where the error
/// object sits in the frame differs per provider, so callers locate it (an
/// absent object degrades to `Value::Null` → all fields `None`). Prefer the
/// structured `message` over serializing `fallback`: `serde_json::Value`'s
/// `Display` ignores precision, so the 500-char truncation in [`stream_error`]
/// only applies once the detail is already a `String`. This is the block that
/// decides whether a mid-stream `overloaded`/`rate_limit` frame is retried —
/// keep the extraction shared so no transport drifts into swallowing one.
pub(crate) fn stream_error_from_object(
    error: &Value,
    prefix: &str,
    fallback: &Value,
) -> ProviderFailure {
    let error_type = error.get("type").and_then(Value::as_str);
    let error_code = error.get("code").and_then(Value::as_str);
    let detail = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string());
    stream_error(error_type, error_code, prefix, &detail)
}

/// Send a request, honouring cancellation and a start-up timeout. Returns
/// `Ok(None)` when the token fires before the response begins — callers map
/// that to an interrupted [`StreamedResponse`](crate::provider::StreamedResponse).
pub(crate) async fn send_cancellable(
    builder: reqwest::RequestBuilder,
    token: &CancellationToken,
) -> ProviderResult<Option<reqwest::Response>> {
    send_cancellable_with_timeout(builder, token, STREAM_CHUNK_TIMEOUT).await
}

async fn send_cancellable_with_timeout(
    builder: reqwest::RequestBuilder,
    token: &CancellationToken,
    request_timeout: Duration,
) -> ProviderResult<Option<reqwest::Response>> {
    let send = timeout(request_timeout, builder.send());
    tokio::select! {
        _ = token.cancelled() => Ok(None),
        result = send => match result {
            Err(_) => Err(ProviderFailure::transport(
                "timed out waiting for streaming request to start",
            )),
            Ok(Err(error)) => Err(request_start_failure(error)),
            Ok(Ok(response)) => Ok(Some(response)),
        }
    }
}

fn request_start_failure(error: reqwest::Error) -> ProviderFailure {
    let message = format!("failed to send streaming request: {error}");
    if error.is_builder() {
        ProviderFailure::configuration(message)
    } else if error.is_decode() {
        ProviderFailure::stream_decode(message)
    } else {
        // DNS, TCP connection, proxy, TLS, and pre-header request failures are
        // transport failures. Reqwest deliberately groups several of those
        // below `is_connect`, so retry policy belongs on this typed boundary
        // rather than on string matching at the caller.
        ProviderFailure::transport(message)
    }
}

/// Drive an SSE byte stream to completion, invoking `on_event` for each
/// `data:` frame. Handles chunk buffering, blank-line framing (LF `\n\n` or
/// strict CRLF `\r\n\r\n`), partial UTF-8 across chunk boundaries, CRLF line
/// endings, empty keep-alive frames, the `[DONE]` sentinel, per-chunk timeouts,
/// and cancellation. Returns whether the stream was interrupted by cancellation.
pub(crate) async fn drive_sse<F>(
    response: reqwest::Response,
    token: CancellationToken,
    mut on_event: F,
) -> ProviderResult<bool>
where
    F: FnMut(Option<&str>, &str) -> ProviderResult<()>,
{
    let mut stream = response.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();
    let mut interrupted = token.is_cancelled();

    while !token.is_cancelled() {
        let chunk = tokio::select! {
            _ = token.cancelled() => {
                interrupted = true;
                break;
            }
            chunk = timeout(STREAM_CHUNK_TIMEOUT, stream.next()) => match chunk {
                Ok(next) => next,
                // A stall mid-stream is transient; classify it as a retryable
                // transport failure rather than a bare `anyhow` error the retry
                // loop can't downcast.
                Err(_elapsed) => {
                    return Err(ProviderFailure::transport(
                        "Timed out waiting for streaming response chunk",
                    ));
                }
            },
        };

        let Some(chunk) = chunk else { break };
        // A body read error (connection reset, HTTP/2 stream reset) is transient:
        // map it to a retryable transport error so `chat_stream_with_retry` backs
        // off instead of failing the turn on the first drop.
        let chunk =
            chunk.map_err(|err| ProviderFailure::transport(format!("Stream read error: {err}")))?;
        buffer.extend_from_slice(&chunk);
        drain_frames(&mut buffer, &mut on_event)?;
    }

    // Flush a final frame the server left unterminated at EOF (e.g. a trailing
    // `data:{…}` or `[DONE]` with no closing blank line) instead of silently
    // dropping it: append a separator and drain. A *complete* final frame is
    // recovered; a *truncated* one fails to parse in `on_event`, which now
    // errors (see the transport callbacks) so the truncation surfaces as a
    // retryable failure rather than a successful empty/partial turn. Whitespace-
    // only residue (a stray trailing newline) is left alone.
    if !interrupted && buffer.iter().any(|&byte| !byte.is_ascii_whitespace()) {
        buffer.extend_from_slice(b"\n\n");
        drain_frames(&mut buffer, &mut on_event)?;
    }

    Ok(interrupted)
}

/// Drain every complete frame from `buffer`, leaving any partial trailing frame
/// in place. Frames are separated by a blank line, which per the SSE spec may be
/// either `\n\n` (LF) or `\r\n\r\n` (CRLF); both are recognized. Pure and
/// synchronous so it can be unit tested without a live stream.
fn drain_frames<F>(buffer: &mut Vec<u8>, on_event: &mut F) -> ProviderResult<()>
where
    F: FnMut(Option<&str>, &str) -> ProviderResult<()>,
{
    while let Some((pos, separator_len)) = find_frame_boundary(buffer) {
        if pos == 0 {
            // Empty keep-alive frame.
            buffer.drain(..separator_len);
            continue;
        }

        let frame = String::from_utf8_lossy(&buffer[..pos]).into_owned();
        buffer.drain(..pos + separator_len);

        let mut event_name: Option<String> = None;
        // Per the SSE spec, multiple `data:` lines in one event are concatenated
        // with `\n` and delivered as a single payload at the event boundary — not
        // one callback per line. Buffer them and dispatch the joined payload once
        //; a provider legally splitting a JSON object across two `data:`
        // lines then parses correctly instead of failing on a `{` fragment.
        let mut data_lines: Vec<&str> = Vec::new();
        for line in frame.lines() {
            let trimmed = line.strip_suffix('\r').unwrap_or(line);
            if let Some(value) = trimmed.strip_prefix("event:") {
                // The space after the colon is optional per the SSE spec, and
                // real servers omit it: OpenCode Go's qwen*-plus models stream
                // `event:message_start`, while its qwen*-max models stream
                // `event: message_start`. Matching only the spaced form left
                // every frame unnamed, and the Anthropic transport drops
                // unnamed events — so the whole stream was discarded and the
                // turn died on "stream ended before message_stop". Keep this
                // symmetric with the `data:` arm below.
                event_name = Some(value.trim().to_string());
            } else if let Some(data) = trimmed.strip_prefix("data:") {
                // Accept both `data: {...}` (one space, per spec) and a
                // spaceless `data:{...}`; strip at most one leading space.
                data_lines.push(data.strip_prefix(' ').unwrap_or(data));
            }
        }
        if !data_lines.is_empty() {
            let payload = data_lines.join("\n");
            if payload != "[DONE]" {
                on_event(event_name.as_deref(), &payload)?;
            }
        }
    }
    Ok(())
}

/// Find the first SSE event separator in `buffer`, returning `(frame_end,
/// separator_len)` where `frame_end` is the byte index where the frame content
/// stops (the start of the separator) and `separator_len` is the separator's
/// length in bytes. Recognizes both `\n\n` (2 bytes) and `\r\n\r\n` (4 bytes);
/// whichever starts first wins, so an LF stream and a strict-CRLF stream both
/// frame correctly.
fn find_frame_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    buffer.iter().enumerate().find_map(|(i, &byte)| {
        if byte == b'\n' && buffer.get(i + 1) == Some(&b'\n') {
            Some((i, 2))
        } else if byte == b'\r' && buffer[i..].starts_with(b"\r\n\r\n") {
            Some((i, 4))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_request_configuration_is_not_retryable() {
        let error = send_cancellable(
            reqwest::Client::new().get("://invalid-url"),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ProviderFailure::Configuration { .. }));
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn connection_failure_is_a_retryable_transport_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);

        let error = send_cancellable_with_timeout(
            reqwest::Client::new().get(format!("http://{address}")),
            &CancellationToken::new(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ProviderFailure::Transport { .. }));
        assert!(error.is_retryable());
    }

    #[tokio::test]
    async fn dns_failure_is_a_retryable_transport_failure() {
        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        let error = send_cancellable_with_timeout(
            client.get("http://bonsai-provider-test.invalid"),
            &CancellationToken::new(),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ProviderFailure::Transport { .. }));
        assert!(error.is_retryable());
    }

    #[tokio::test]
    async fn request_start_timeout_is_a_retryable_transport_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });

        let error = send_cancellable_with_timeout(
            reqwest::Client::new().get(format!("http://{address}")),
            &CancellationToken::new(),
            Duration::from_millis(25),
        )
        .await
        .unwrap_err();
        server.abort();

        assert!(matches!(error, ProviderFailure::Transport { .. }));
        assert!(error.is_retryable());
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn tls_handshake_failure_is_a_retryable_transport_failure() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();

        let error = send_cancellable_with_timeout(
            client.get(format!("https://{address}")),
            &CancellationToken::new(),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        server.await.unwrap();

        assert!(matches!(error, ProviderFailure::Transport { .. }));
        assert!(error.is_retryable());
    }

    /// Collect (event, data) pairs a frame stream yields.
    fn collect(chunks: &[&[u8]]) -> Vec<(Option<String>, String)> {
        let mut buffer: Vec<u8> = Vec::new();
        let mut events: Vec<(Option<String>, String)> = Vec::new();
        let mut sink = |event: Option<&str>, data: &str| {
            events.push((event.map(ToString::to_string), data.to_string()));
            Ok(())
        };
        for chunk in chunks {
            buffer.extend_from_slice(chunk);
            drain_frames(&mut buffer, &mut sink).unwrap();
        }
        events
    }

    #[test]
    fn parses_anthropic_event_data_pairs() {
        let events = collect(&[b"event: content_block_delta\ndata: {\"x\":1}\n\n"]);
        assert_eq!(
            events,
            vec![(
                Some("content_block_delta".to_string()),
                "{\"x\":1}".to_string()
            )]
        );
    }

    /// OpenCode Go's qwen*-plus models stream `event:message_start` with no
    /// space, which the Anthropic transport must still route by name — it drops
    /// unnamed events, so a missed name silently discards the entire stream.
    #[test]
    fn parses_spaceless_event_and_data_fields() {
        let events = collect(&[b"event:message_stop\ndata:{\"type\":\"message_stop\"}\n\n"]);
        assert_eq!(
            events,
            vec![(
                Some("message_stop".to_string()),
                "{\"type\":\"message_stop\"}".to_string()
            )],
            "the space after `event:` is optional per the SSE spec",
        );
    }

    #[test]
    fn parses_bare_data_lines_without_event() {
        let events = collect(&[b"data: {\"a\":1}\n\ndata: {\"b\":2}\n\n"]);
        assert_eq!(
            events,
            vec![
                (None, "{\"a\":1}".to_string()),
                (None, "{\"b\":2}".to_string()),
            ]
        );
    }

    #[test]
    fn skips_done_sentinel() {
        let events = collect(&[b"data: {\"a\":1}\n\ndata: [DONE]\n\n"]);
        assert_eq!(events, vec![(None, "{\"a\":1}".to_string())]);
    }

    #[test]
    fn folds_multiple_data_lines_into_one_payload() {
        // Per the SSE spec, multiple `data:` lines within one event concatenate
        // with `\n` and dispatch once — a provider legally splitting a JSON
        // object across two `data:` lines then parses instead of failing on a
        // `{` fragment.
        let events = collect(&[b"data: {\"a\":1,\ndata: \"b\":2}\n\n"]);
        assert_eq!(events, vec![(None, "{\"a\":1,\n\"b\":2}".to_string())]);
    }

    #[test]
    fn splits_frames_across_chunk_boundaries() {
        // A single frame delivered in two chunks must still parse once whole.
        let events = collect(&[b"data: {\"hel", b"lo\":1}\n\n"]);
        assert_eq!(events, vec![(None, "{\"hello\":1}".to_string())]);
    }

    #[test]
    fn strips_crlf_line_endings() {
        // A server using CRLF line endings leaves a trailing `\r` on each line.
        // The codex path historically did not strip it (leaving `\r` in the JSON
        // payload); the shared parser strips it for every provider. Here the
        // frame separator is `\r\n\n` (CRLF line ending plus a bare LF).
        let events = collect(&[b"event: ping\r\ndata: {\"a\":1}\r\n\n"]);
        assert_eq!(
            events,
            vec![(Some("ping".to_string()), "{\"a\":1}".to_string())]
        );
    }

    #[test]
    fn frames_strict_crlf_event_separators() {
        // A spec-compliant server may separate events with `\r\n\r\n` (no bare
        // `\n\n` anywhere). The framer must still split — otherwise the buffer
        // grows unbounded and no events are ever delivered.
        let events = collect(&[b"event: ping\r\ndata: {\"a\":1}\r\n\r\ndata: {\"b\":2}\r\n\r\n"]);
        assert_eq!(
            events,
            vec![
                (Some("ping".to_string()), "{\"a\":1}".to_string()),
                (None, "{\"b\":2}".to_string()),
            ]
        );
    }

    #[test]
    fn frames_strict_crlf_across_chunk_boundaries() {
        // The `\r\n\r\n` separator may be split across read chunks.
        let events = collect(&[b"data: {\"a\":1}\r\n", b"\r\ndata: {\"b\":2}\r\n\r\n"]);
        assert_eq!(
            events,
            vec![
                (None, "{\"a\":1}".to_string()),
                (None, "{\"b\":2}".to_string()),
            ]
        );
    }

    #[test]
    fn skips_empty_keep_alive_frames() {
        let events = collect(&[b"\n\ndata: {\"a\":1}\n\n"]);
        assert_eq!(events, vec![(None, "{\"a\":1}".to_string())]);
    }

    #[test]
    fn skips_strict_crlf_keep_alive_frames() {
        let events = collect(&[b"\r\n\r\ndata: {\"a\":1}\r\n\r\n"]);
        assert_eq!(events, vec![(None, "{\"a\":1}".to_string())]);
    }

    #[test]
    fn leaves_partial_trailing_frame_buffered() {
        let mut buffer: Vec<u8> = Vec::new();
        let mut events: Vec<String> = Vec::new();
        let mut sink = |_event: Option<&str>, data: &str| {
            events.push(data.to_string());
            Ok(())
        };
        buffer.extend_from_slice(b"data: {\"a\":1}\n\ndata: partial");
        drain_frames(&mut buffer, &mut sink).unwrap();
        assert_eq!(events, vec!["{\"a\":1}".to_string()]);
        // The partial frame stays in the buffer for the next chunk.
        assert_eq!(buffer, b"data: partial");
    }

    #[test]
    fn propagates_callback_errors() {
        let mut buffer: Vec<u8> = Vec::new();
        buffer.extend_from_slice(b"data: boom\n\n");
        let mut sink =
            |_event: Option<&str>, _data: &str| Err(ProviderFailure::stream_decode("stop"));
        let result = drain_frames(&mut buffer, &mut sink);
        assert!(result.is_err());
    }
}
