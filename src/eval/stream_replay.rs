//! Offline normalized-provider playback. Tools still execute through the real agent.
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use async_openai::types::chat::{ChatCompletionRequestMessage, ChatCompletionTool};
use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use super::stream_fixture::*;
use crate::output::SharedSink;
use crate::provider::*;

#[derive(Debug)]
pub(super) struct ReplayState {
    attempts: VecDeque<StreamAttempt>,
    consumed: usize,
    failure: Option<String>,
}
impl ReplayState {
    pub fn new(attempts: Vec<StreamAttempt>) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            attempts: attempts.into(),
            consumed: 0,
            failure: None,
        }))
    }
    pub fn finish(&self) -> Result<()> {
        if let Some(failure) = &self.failure {
            bail!("{failure}");
        }
        if !self.attempts.is_empty() {
            bail!("replay has {} unused attempts", self.attempts.len());
        }
        Ok(())
    }
}

pub(super) async fn load(path: &Path) -> Result<(StreamHeader, Vec<StreamAttempt>)> {
    let file = tokio::fs::File::open(path)
        .await
        .context("open provider stream fixture")?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() || metadata.len() > MAX_FIXTURE_BYTES as u64 {
        bail!("provider fixture must be a bounded regular file");
    }
    let mut bytes = Vec::new();
    file.take(MAX_FIXTURE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_FIXTURE_BYTES {
        bail!("provider fixture exceeds byte limit");
    }
    let text = std::str::from_utf8(&bytes).context("fixture must be UTF-8")?;
    let mut lines = text.lines();
    let first = lines.next().context("missing stream header")?;
    if first.len() > MAX_ATTEMPT_BYTES {
        bail!("provider header exceeds byte limit");
    }
    let header: StreamRecord = serde_json::from_str(first).context("invalid stream header")?;
    let StreamRecord::Header(header) = header else {
        bail!("fixture must start with header");
    };
    if header.version != STREAM_FIXTURE_VERSION {
        bail!("unsupported stream fixture version {}", header.version);
    }
    ensure_portable(&serde_json::to_value(&header)?)?;
    header.preparation.validate_portable()?;
    if header.context_tokens == 0 {
        bail!("invalid fixture context window");
    }
    anyhow::ensure!(
        !header.platform.trim().is_empty(),
        "missing fixture platform"
    );
    let mut attempts = Vec::new();
    let mut complete = false;
    for line in lines {
        if complete {
            bail!("records after stream completeness marker");
        }
        if line.len() > MAX_ATTEMPT_BYTES {
            bail!("provider attempt exceeds byte limit");
        }
        let record: StreamRecord =
            serde_json::from_str(line).context("malformed provider stream record")?;
        ensure_portable(&serde_json::to_value(&record)?)?;
        match record {
            StreamRecord::Attempt(attempt) if attempt.index == attempts.len() => {
                if let RecordedOutcome::Failure {
                    class,
                    status,
                    code,
                    ..
                } = &attempt.outcome
                    && matches!(class, FailureClass::Http)
                    && (status.is_none() || !valid_code(code))
                {
                    bail!("invalid portable HTTP failure");
                }
                attempts.push(attempt);
            }
            StreamRecord::Complete { attempts: count } if count == attempts.len() => {
                complete = true
            }
            _ => bail!("invalid provider stream record ordering"),
        }
    }
    if !complete {
        bail!("incomplete provider stream fixture");
    }
    Ok((header, attempts))
}

pub(super) struct ReplayProvider {
    preparation: crate::provider::opencode::PortableChatPreparation,
    normalizer: Mutex<Normalizer>,
    pub state: Arc<Mutex<ReplayState>>,
}
impl ReplayProvider {
    pub fn with_state(
        header: &StreamHeader,
        normalizer: Normalizer,
        state: Arc<Mutex<ReplayState>>,
    ) -> Self {
        Self {
            preparation: header.preparation.clone(),
            normalizer: Mutex::new(normalizer),
            state,
        }
    }
}

#[async_trait]
impl Provider for ReplayProvider {
    fn prepare_request_for_estimation(
        &self,
        m: &[ChatCompletionRequestMessage],
        t: &[ChatCompletionTool],
        o: ProviderRequestOptions,
        c: &ProviderRequestContext,
    ) -> ProviderResult<PreparedProviderRequest> {
        let mut prepared = self.preparation.prepare(m, t, o, c)?;
        self.normalizer
            .lock()
            .map_err(|_| ProviderFailure::configuration("replay lock poisoned"))?
            .request_value(&mut prepared.body);
        Ok(prepared)
    }
    fn set_tool_call_identity_store(&mut self, store: ToolCallIdentityStore) {
        self.preparation.tool_call_identities = store;
    }
    fn set_conversation_cache_key(&mut self, key: &str) {
        self.preparation.prompt_cache_key = key.into();
    }
    fn project_state_cache_strategy(&self) -> ProjectStateCacheStrategy {
        self.preparation.cache_strategy()
    }
    fn reasoning(&self) -> ReasoningSelection {
        ReasoningSelection::Off
    }
    async fn chat_stream(
        &self,
        m: &[ChatCompletionRequestMessage],
        t: &[ChatCompletionTool],
        cancel: CancellationToken,
        sink: SharedSink,
    ) -> ProviderResult<StreamedResponse> {
        self.chat_stream_with_context(m, t, Default::default(), &Default::default(), cancel, sink)
            .await
    }
    async fn chat_stream_with_options(
        &self,
        m: &[ChatCompletionRequestMessage],
        t: &[ChatCompletionTool],
        o: ProviderRequestOptions,
        cancel: CancellationToken,
        sink: SharedSink,
    ) -> ProviderResult<StreamedResponse> {
        self.chat_stream_with_context(m, t, o, &Default::default(), cancel, sink)
            .await
    }
    async fn chat_stream_with_context(
        &self,
        m: &[ChatCompletionRequestMessage],
        t: &[ChatCompletionTool],
        o: ProviderRequestOptions,
        c: &ProviderRequestContext,
        cancel: CancellationToken,
        sink: SharedSink,
    ) -> ProviderResult<StreamedResponse> {
        let attempt = (|| -> Result<StreamAttempt> {
            let prepared = self.preparation.prepare(m, t, o, c)?;
            let request = self
                .normalizer
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .request(
                    m,
                    t,
                    o,
                    c,
                    &self.preparation.tool_call_identities,
                    prepared.local_count_body(),
                )?;
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let index = state.consumed;
            let expected = state
                .attempts
                .front()
                .context("unexpected extra provider invocation")?;
            if let Some(path) = first_difference(
                &serde_json::to_value(&expected.request)?,
                &serde_json::to_value(&request)?,
                "request",
            ) {
                bail!("replay attempt {index} mismatch at {path}");
            }
            state.consumed += 1;
            state.attempts.pop_front().context("missing replay attempt")
        })();
        let attempt = match attempt {
            Ok(attempt) => attempt,
            Err(error) => {
                let failure = error.to_string();
                self.state.lock().unwrap_or_else(|p| p.into_inner()).failure =
                    Some(failure.clone());
                return Err(ProviderFailure::configuration(failure));
            }
        };
        for event in attempt.events {
            let normalizer = self.normalizer.lock().unwrap_or_else(|p| p.into_inner());
            match event {
                StreamEvent::Assistant(text) => sink.assistant_delta(&normalizer.rebase(&text)),
                StreamEvent::Reasoning(text) => sink.reasoning_delta(&normalizer.rebase(&text)),
                StreamEvent::Done => sink.assistant_done(),
            }
        }
        match attempt.outcome {
            RecordedOutcome::Response {
                content,
                tool_calls,
                terminal,
                usage,
                reasoning_chars,
            } => {
                if terminal.is_interrupted() {
                    cancel.cancelled().await;
                }
                let mut calls = tool_calls
                    .iter()
                    .map(|call| ToolCall {
                        id: call.wire_id.clone(),
                        name: call.name.clone(),
                        arguments: self
                            .normalizer
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .rebase_arguments(&call.arguments),
                    })
                    .collect::<Vec<_>>();
                self.preparation
                    .tool_call_identities
                    .assign_calls(&self.preparation.tool_call_scope, &mut calls);
                {
                    let mut normalizer = self.normalizer.lock().unwrap_or_else(|p| p.into_inner());
                    for (record, call) in tool_calls.iter().zip(&calls) {
                        if normalizer.call(&call.id) != record.id {
                            self.state.lock().unwrap_or_else(|p| p.into_inner()).failure =
                                Some("replay tool identity ordering mismatch".into());
                            return Err(ProviderFailure::configuration(
                                "replay tool identity ordering mismatch",
                            ));
                        }
                    }
                }
                Ok(StreamedResponse {
                    content: self
                        .normalizer
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .rebase(&content),
                    tool_calls: calls,
                    terminal,
                    reasoning_chars,
                    usage: usage.map(|u| TokenUsage {
                        prompt_tokens: u.prompt_tokens,
                        completion_tokens: u.completion_tokens,
                        input_cache: u.input_cache.map(|[r, c, t]| InputCacheUsage::new(r, c, t)),
                    }),
                })
            }
            RecordedOutcome::Failure {
                class,
                message,
                status,
                code,
                retry_after_secs,
            } => Err(match class {
                FailureClass::Http => ProviderFailure::http_with_details(
                    status.unwrap_or(400),
                    message,
                    parse_code(&code),
                    None,
                    None,
                    retry_after_secs,
                    None,
                ),
                FailureClass::Transport => ProviderFailure::transport(message),
                FailureClass::Decode => ProviderFailure::stream_decode(message),
                FailureClass::Configuration => ProviderFailure::configuration(message),
            }),
        }
    }
    async fn list_models(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

fn valid_code(code: &str) -> bool {
    matches!(
        code,
        "Authentication"
            | "Permission"
            | "NotFound"
            | "InvalidRequest"
            | "RateLimit"
            | "Quota"
            | "Overloaded"
            | "Server"
            | "Unknown"
    )
}

fn parse_code(code: &str) -> ProviderErrorCode {
    match code {
        "Authentication" => ProviderErrorCode::Authentication,
        "Permission" => ProviderErrorCode::Permission,
        "NotFound" => ProviderErrorCode::NotFound,
        "InvalidRequest" => ProviderErrorCode::InvalidRequest,
        "RateLimit" => ProviderErrorCode::RateLimit,
        "Quota" => ProviderErrorCode::Quota,
        "Overloaded" => ProviderErrorCode::Overloaded,
        "Server" => ProviderErrorCode::Server,
        _ => ProviderErrorCode::Unknown,
    }
}
