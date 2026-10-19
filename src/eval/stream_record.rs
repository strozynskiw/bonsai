//! Eval-only per-attempt capture. Sink callbacks only buffer; disk writes run off-runtime.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::stream_fixture::*;
use crate::output::{OutputSink, SharedSink};
use crate::provider::*;

#[derive(Debug)]
pub(super) struct CaptureState {
    path: PathBuf,
    normalizer: Normalizer,
    attempts: usize,
    bytes: usize,
    active: bool,
    failure: Option<String>,
    fault: Option<CaptureFault>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum CaptureFault {
    ErrorAfterPrefix(usize),
    WaitForCancellation(usize),
}
impl CaptureFault {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(value) = std::env::var_os("BONSAI_EVAL_STREAM_FAULT") else {
            return Ok(None);
        };
        let value = value.to_str().context("invalid stream fault encoding")?;
        let (kind, attempt) = value
            .split_once(':')
            .context("stream fault must be error-after-prefix:N or wait-for-cancellation:N")?;
        let attempt = attempt
            .parse::<usize>()
            .context("invalid stream fault attempt")?;
        anyhow::ensure!(attempt > 0, "stream fault attempt must be positive");
        Ok(Some(match kind {
            "error-after-prefix" => Self::ErrorAfterPrefix(attempt),
            "wait-for-cancellation" => Self::WaitForCancellation(attempt),
            _ => bail!("unknown stream fault"),
        }))
    }
}

pub(super) type CaptureHandle = Arc<Mutex<CaptureState>>;

pub(super) async fn create_capture(
    path: &Path,
    header: StreamHeader,
    normalizer: Normalizer,
) -> Result<CaptureHandle> {
    let fault = CaptureFault::from_env()?;
    ensure_portable(&serde_json::to_value(&header)?)?;
    let path = path.to_owned();
    let target = path.clone();
    let bytes = encode(&StreamRecord::Header(header))?;
    let len = bytes.len();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        options
            .open(&target)
            .context("create private stream fixture (overwrites refused)")?
            .write_all(&bytes)?;
        Ok(())
    })
    .await??;
    Ok(Arc::new(Mutex::new(CaptureState {
        path,
        normalizer,
        attempts: 0,
        bytes: len,
        active: false,
        failure: None,
        fault,
    })))
}

fn encode(record: &StreamRecord) -> Result<Vec<u8>> {
    if let StreamRecord::Attempt(attempt) = record {
        validate_callback_channels(&attempt.events)?;
    }
    let value = serde_json::to_value(record)?;
    ensure_portable(&value)?;
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_ATTEMPT_BYTES {
        bail!("stream record exceeds byte limit");
    }
    Ok(bytes)
}

fn validate_callback_channels(events: &[StreamEvent]) -> Result<()> {
    let mut assistant = String::new();
    let mut reasoning = String::new();
    for event in events {
        match event {
            StreamEvent::Assistant(text) => assistant.push_str(text),
            StreamEvent::Reasoning(text) => reasoning.push_str(text),
            StreamEvent::Done => {}
        }
    }
    ensure_portable(&serde_json::Value::String(assistant))?;
    ensure_portable(&serde_json::Value::String(reasoning))
}

async fn append(path: PathBuf, bytes: Vec<u8>) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
        file.write_all(&bytes)?;
        file.sync_data()?;
        Ok(())
    })
    .await?
}

pub(super) async fn finish_capture(handle: &CaptureHandle) -> Result<()> {
    let (path, bytes) = {
        let state = handle
            .lock()
            .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?;
        if let Some(failure) = &state.failure {
            bail!("stream capture failed: {failure}");
        }
        if state.active {
            bail!("stream capture has incomplete attempt");
        }
        (
            state.path.clone(),
            encode(&StreamRecord::Complete {
                attempts: state.attempts,
            })?,
        )
    };
    append(path, bytes).await
}

struct CaptureSink {
    downstream: SharedSink,
    events: Mutex<(Vec<StreamEvent>, usize, bool)>,
}
impl CaptureSink {
    fn push(&self, event: StreamEvent, len: usize) {
        if let Ok(mut state) = self.events.lock() {
            state.1 = state.1.saturating_add(len);
            if state.1 > MAX_ATTEMPT_BYTES || state.0.len() >= 65536 {
                state.2 = true;
            } else {
                state.0.push(event);
            }
        }
    }
}
impl OutputSink for CaptureSink {
    fn assistant_delta(&self, text: &str) {
        if text.len() <= MAX_ATTEMPT_BYTES {
            self.push(StreamEvent::Assistant(text.into()), text.len());
        } else if let Ok(mut state) = self.events.lock() {
            state.2 = true;
        }
        self.downstream.assistant_delta(text);
    }
    fn reasoning_delta(&self, text: &str) {
        if text.len() <= MAX_ATTEMPT_BYTES {
            self.push(StreamEvent::Reasoning(text.into()), text.len());
        } else if let Ok(mut state) = self.events.lock() {
            state.2 = true;
        }
        self.downstream.reasoning_delta(text);
    }
    fn assistant_done(&self) {
        self.push(StreamEvent::Done, 0);
        self.downstream.assistant_done();
    }
    fn thinking(&self, text: &str) {
        self.downstream.thinking(text);
    }
    fn status(&self, text: &str) {
        self.downstream.status(text);
    }
}

pub(super) struct RecordingProvider {
    inner: Box<dyn Provider>,
    handle: CaptureHandle,
    identities: ToolCallIdentityStore,
    native: NativeReplayStore,
}
impl RecordingProvider {
    pub fn new(inner: Box<dyn Provider>, handle: CaptureHandle) -> Self {
        Self {
            inner,
            handle,
            identities: Default::default(),
            native: Default::default(),
        }
    }

    async fn capture(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: ProviderRequestOptions,
        context: &ProviderRequestContext,
        token: CancellationToken,
        sink: SharedSink,
    ) -> Result<StreamedResponse, ProviderFailure> {
        let prepared = match self
            .inner
            .prepare_request_for_estimation(messages, tools, options, context)
        {
            Ok(prepared) if !prepared.has_images() && prepared.native_budget.is_empty() => prepared,
            _ => {
                if let Ok(mut state) = self.handle.lock() {
                    state.failure = Some("unsupported capture request".into());
                }
                return Err(ProviderFailure::configuration(
                    "unsupported capture request",
                ));
            }
        };
        let (index, request, fault) = {
            let mut state = self
                .handle
                .lock()
                .map_err(|_| ProviderFailure::configuration("capture lock poisoned"))?;
            if state.active || state.failure.is_some() {
                return Err(ProviderFailure::configuration(
                    "incomplete or failed stream capture",
                ));
            }
            state.active = true;
            let request = state
                .normalizer
                .request(
                    messages,
                    tools,
                    options,
                    context,
                    &self.identities,
                    prepared.local_count_body(),
                )
                .map_err(|_| {
                    ProviderFailure::configuration(
                        "request is not portable or contains excluded data",
                    )
                })?;
            (state.attempts, request, state.fault)
        };
        let proxy = Arc::new(CaptureSink {
            downstream: sink,
            events: Mutex::new((Vec::new(), 0, false)),
        });
        let result = match fault {
            Some(CaptureFault::ErrorAfterPrefix(attempt)) if attempt == index + 1 => {
                proxy.assistant_delta("[controlled fault prefix]");
                Err(ProviderFailure::http(
                    503,
                    "controlled eval stream fault",
                    Some(0),
                ))
            }
            Some(CaptureFault::WaitForCancellation(attempt)) if attempt == index + 1 => {
                proxy.assistant_delta("[controlled fault prefix]");
                token.cancelled().await;
                Ok(StreamedResponse {
                    content: "[controlled fault prefix]".into(),
                    terminal: StreamTerminal::Interrupted,
                    ..Default::default()
                })
            }
            _ => {
                self.inner
                    .chat_stream_with_context(
                        messages,
                        tools,
                        options,
                        context,
                        token,
                        proxy.clone(),
                    )
                    .await
            }
        };
        let record = (|| -> Result<(PathBuf, Vec<u8>)> {
            anyhow::ensure!(
                self.native.snapshot().is_empty(),
                "capture encountered unsupported native response history"
            );
            let mut state = self
                .handle
                .lock()
                .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?;
            let mut events = proxy
                .events
                .lock()
                .map_err(|_| anyhow::anyhow!("capture sink poisoned"))?;
            if events.2 {
                bail!("stream callbacks exceeded limit");
            }
            let outcome = outcome(&result, &mut state.normalizer, &self.identities)?;
            let mut callbacks = std::mem::take(&mut events.0);
            for event in &mut callbacks {
                match event {
                    StreamEvent::Assistant(text) | StreamEvent::Reasoning(text) => {
                        *text = state.normalizer.text(text)
                    }
                    StreamEvent::Done => {}
                }
            }
            let bytes = encode(&StreamRecord::Attempt(StreamAttempt {
                index,
                request,
                events: callbacks,
                outcome,
            }))?;
            state.bytes = state.bytes.saturating_add(bytes.len());
            if state.bytes > MAX_FIXTURE_BYTES {
                bail!("fixture exceeds byte limit");
            }
            Ok((state.path.clone(), bytes))
        })();
        let persisted = match record {
            Ok((path, bytes)) => append(path, bytes).await,
            Err(error) => Err(error),
        };
        let mut state = self
            .handle
            .lock()
            .map_err(|_| ProviderFailure::configuration("capture lock poisoned"))?;
        if persisted.is_err() {
            state.failure = Some("artifact validation or write failed".into());
            return Err(ProviderFailure::configuration(
                "stream artifact validation or write failed",
            ));
        }
        state.attempts += 1;
        state.active = false;
        result
    }
}

fn outcome(
    result: &Result<StreamedResponse, ProviderFailure>,
    normalizer: &mut Normalizer,
    identities: &ToolCallIdentityStore,
) -> Result<RecordedOutcome> {
    Ok(match result {
        Ok(response) => {
            let identities = identities.snapshot();
            let mut calls = Vec::new();
            for call in &response.tool_calls {
                let identity = identities
                    .iter()
                    .find(|identity| identity.canonical_id == call.id)
                    .context("provider did not assign tool identity")?;
                let id = normalizer.call(&call.id);
                let arguments = normalizer.arguments(&call.arguments);
                calls.push(RecordedToolCall {
                    id,
                    wire_id: identity.wire_id.clone(),
                    name: call.name.clone(),
                    arguments,
                });
            }
            RecordedOutcome::Response {
                content: normalizer.text(&response.content),
                tool_calls: calls,
                terminal: response.terminal.clone(),
                reasoning_chars: response.reasoning_chars,
                usage: response.usage.map(|usage| RecordedUsage {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    input_cache: usage.input_cache.map(|cache| {
                        [
                            cache.read_tokens,
                            cache.creation_tokens,
                            cache.total_input_tokens,
                        ]
                    }),
                }),
            }
        }
        Err(error) => {
            let (class, message, status, code, retry_after_secs) = match error {
                ProviderFailure::Http {
                    status,
                    message,
                    code,
                    retry_after_secs,
                    ..
                } => (
                    FailureClass::Http,
                    message,
                    Some(*status),
                    format!("{code:?}"),
                    *retry_after_secs,
                ),
                ProviderFailure::Transport { message, .. } => {
                    (FailureClass::Transport, message, None, String::new(), None)
                }
                ProviderFailure::Decode { message, .. } => {
                    (FailureClass::Decode, message, None, String::new(), None)
                }
                ProviderFailure::Configuration { message } => (
                    FailureClass::Configuration,
                    message,
                    None,
                    String::new(),
                    None,
                ),
            };
            RecordedOutcome::Failure {
                class,
                message: normalizer.text(&crate::redact::redact(message)),
                status,
                code,
                retry_after_secs,
            }
        }
    })
}

#[async_trait]
impl Provider for RecordingProvider {
    fn eval_stream_preparation(
        &self,
    ) -> Result<Option<crate::provider::opencode::PortableChatPreparation>> {
        self.inner.eval_stream_preparation()
    }
    fn prepare_request_for_estimation(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: ProviderRequestOptions,
        context: &ProviderRequestContext,
    ) -> ProviderResult<PreparedProviderRequest> {
        let mut prepared = self
            .inner
            .prepare_request_for_estimation(messages, tools, options, context)?;
        let state = self
            .handle
            .lock()
            .map_err(|_| ProviderFailure::configuration("capture lock poisoned"))?;
        state.normalizer.request_value(&mut prepared.body);
        prepared.count_endpoint = None;
        prepared.count_auth_header = None;
        Ok(prepared)
    }
    fn reasoning_for_request(
        &self,
        messages: &[ChatCompletionRequestMessage],
        options: ProviderRequestOptions,
        context: &ProviderRequestContext,
    ) -> ProviderResult<ReasoningSelection> {
        self.inner.reasoning_for_request(messages, options, context)
    }
    fn set_tool_call_identity_store(&mut self, store: ToolCallIdentityStore) {
        self.identities = store.clone();
        self.inner.set_tool_call_identity_store(store);
    }
    fn set_native_replay_store(&mut self, store: NativeReplayStore) {
        self.native = store.clone();
        self.inner.set_native_replay_store(store);
    }
    fn set_conversation_cache_key(&mut self, key: &str) {
        self.inner.set_conversation_cache_key(key);
    }
    fn project_state_cache_strategy(&self) -> ProjectStateCacheStrategy {
        self.inner.project_state_cache_strategy()
    }
    fn reasoning(&self) -> ReasoningSelection {
        self.inner.reasoning()
    }
    fn reasoning_escalation(&self) -> Option<ReasoningSelection> {
        self.inner.reasoning_escalation()
    }
    fn take_last_request_diagnostics(&self) -> Option<ProviderRequestDiagnostics> {
        self.inner.take_last_request_diagnostics()
    }
    async fn chat_stream(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        token: CancellationToken,
        sink: SharedSink,
    ) -> ProviderResult<StreamedResponse> {
        self.capture(
            messages,
            tools,
            Default::default(),
            &Default::default(),
            token,
            sink,
        )
        .await
    }
    async fn chat_stream_with_options(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: ProviderRequestOptions,
        token: CancellationToken,
        sink: SharedSink,
    ) -> ProviderResult<StreamedResponse> {
        self.capture(messages, tools, options, &Default::default(), token, sink)
            .await
    }
    async fn chat_stream_with_context(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        options: ProviderRequestOptions,
        context: &ProviderRequestContext,
        token: CancellationToken,
        sink: SharedSink,
    ) -> ProviderResult<StreamedResponse> {
        self.capture(messages, tools, options, context, token, sink)
            .await
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        self.inner.list_models().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn split_credentials_fail_for_failed_assistant_and_successful_reasoning() {
        for reasoning in [false, true] {
            let events = [
                "sk-".to_owned(),
                "ABCdef0123".repeat(2),
                "ABCdef0123".repeat(2),
            ]
            .into_iter()
            .map(|part| {
                if reasoning {
                    StreamEvent::Reasoning(part)
                } else {
                    StreamEvent::Assistant(part)
                }
            })
            .collect::<Vec<_>>();
            for event in &events {
                ensure_portable(&serde_json::to_value(event).unwrap()).unwrap();
            }
            let outcome = if reasoning {
                RecordedOutcome::Response {
                    content: "done".into(),
                    tool_calls: vec![],
                    terminal: StreamTerminal::Completed(FinishReason::Stop),
                    usage: None,
                    reasoning_chars: 43,
                }
            } else {
                RecordedOutcome::Failure {
                    class: FailureClass::Transport,
                    message: "connection lost".into(),
                    status: None,
                    code: String::new(),
                    retry_after_secs: None,
                }
            };
            let record = StreamRecord::Attempt(StreamAttempt {
                index: 0,
                request: LogicalRequest {
                    messages: json!([]),
                    tools: json!([]),
                    options: json!({}),
                    context: json!({}),
                    identities: json!([]),
                    local_count: json!({}),
                },
                events,
                outcome,
            });
            assert!(encode(&record).is_err());
        }
    }
}
