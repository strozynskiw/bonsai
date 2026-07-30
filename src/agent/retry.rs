use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const MIN_STREAMED_CHAR_BUDGET: usize = 64_000;
const MAX_STREAMED_CHAR_BUDGET: usize = 240_000;

/// Timing policy between retryable provider attempts.
///
/// Production runs preserve provider-requested delays and exponential backoff.
/// Deterministic evals use [`Self::Immediate`] so injected failures cover the
/// recovery path without making the suite wait on wall-clock timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryBackoff {
    Exponential,
    Immediate,
}

impl RetryBackoff {
    fn delay(self, provider_error: &ProviderFailure, attempt: usize) -> Duration {
        match self {
            Self::Exponential => Duration::from_secs(
                provider_error
                    .retry_after_secs()
                    .unwrap_or(1 << attempt)
                    .max(1),
            ),
            Self::Immediate => Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GenerationBudget {
    /// Optional inactivity deadline: the longest a single attempt may stream
    /// **without producing any output** before it is abandoned. Reset by every
    /// streamed delta, so a still-working generation is never cut off. `None`
    /// (the default) means no deadline — the stall guard and retry limits catch
    /// a genuinely stuck model instead.
    pub(super) max_duration: Option<Duration>,
    pub(super) max_streamed_chars: usize,
}

impl GenerationBudget {
    pub(super) fn for_context(
        context_budget_tokens: usize,
        max_duration_override: Option<Duration>,
        max_streamed_chars_override: Option<usize>,
    ) -> Self {
        // Off by default: an unset generation budget means no deadline at all.
        // A stuck or looping model is caught by the stall guard and retry
        // limits, not by a wall-clock cap that would also cut off a slow but
        // still-streaming generation. `None` here = unbounded; a value opts into
        // an inactivity deadline (see `GenerationBudget::max_duration`).
        let max_duration = max_duration_override.or_else(|| {
            std::env::var("BONSAI_MAX_GENERATION_SECONDS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .filter(|seconds| *seconds > 0)
                .map(Duration::from_secs)
        });
        let default_chars = context_budget_tokens
            .saturating_mul(2)
            .clamp(MIN_STREAMED_CHAR_BUDGET, MAX_STREAMED_CHAR_BUDGET);
        let max_streamed_chars = max_streamed_chars_override.unwrap_or_else(|| {
            std::env::var("BONSAI_MAX_STREAMED_CHARS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .filter(|chars| *chars > 0)
                .unwrap_or(default_chars)
        });
        Self {
            max_duration,
            max_streamed_chars,
        }
    }
}

impl Default for GenerationBudget {
    fn default() -> Self {
        Self::for_context(
            crate::provider::DEFAULT_CONTEXT_WINDOW_TOKENS as usize,
            None,
            None,
        )
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct AttemptOutput {
    assistant_chars: usize,
    reasoning_chars: usize,
}

pub(super) struct RetriedStream {
    pub(super) response: StreamedResponse,
    pub(super) attempts: Vec<ProviderAttemptReport>,
    pub(super) budget_exhaustion: Option<crate::run_budget::RunBudgetExhaustion>,
}

#[derive(Debug, thiserror::Error)]
#[error("{source:#}")]
pub(super) struct ProviderRetryError {
    #[source]
    source: anyhow::Error,
    attempts: Vec<ProviderAttemptReport>,
}

impl ProviderRetryError {
    fn new(source: anyhow::Error, attempts: Vec<ProviderAttemptReport>) -> Self {
        Self { source, attempts }
    }

    pub(super) fn into_parts(self) -> (anyhow::Error, Vec<ProviderAttemptReport>) {
        (self.source, self.attempts)
    }
}

/// Streams one provider attempt through to the surfaces while metering it.
/// Deltas are forwarded immediately — thinking and text must render while the
/// model is generating, not replay after the stream ends. A retryable stream
/// that dies after emitting output is withdrawn with `attempt_discarded`
/// (paired with the `attempt_started` floor) instead of being buffered until
/// success: buffering here previously suppressed all live streaming for the
/// entire generation.
struct AttemptSink {
    inner: SharedSink,
    assistant_chars: AtomicUsize,
    reasoning_chars: AtomicUsize,
    streamed_chars: AtomicUsize,
    budget_exceeded: AtomicBool,
    max_streamed_chars: usize,
    cancellation: CancellationToken,
    /// Anchors the inactivity deadline. `tokio::time::Instant` so paused-time
    /// tests can exercise the timeout deterministically.
    base: tokio::time::Instant,
    /// Milliseconds since `base` at the last streamed delta. The generation
    /// deadline measures idle time from here, so a stream that keeps producing
    /// output keeps pushing the deadline out.
    last_progress_ms: AtomicU64,
}

impl AttemptSink {
    fn new(
        inner: SharedSink,
        budget: GenerationBudget,
        cancellation: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            assistant_chars: AtomicUsize::new(0),
            reasoning_chars: AtomicUsize::new(0),
            streamed_chars: AtomicUsize::new(0),
            budget_exceeded: AtomicBool::new(false),
            max_streamed_chars: budget.max_streamed_chars,
            cancellation,
            base: tokio::time::Instant::now(),
            last_progress_ms: AtomicU64::new(0),
        })
    }

    /// Record that the provider just streamed output, resetting the inactivity
    /// deadline. Called on every content-bearing delta.
    fn note_progress(&self) {
        let elapsed = u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_progress_ms.store(elapsed, Ordering::Relaxed);
    }

    /// Time since the last streamed delta (or since the attempt began, if the
    /// provider has produced nothing yet). The generation deadline fires once
    /// this reaches `max_duration`.
    fn idle_duration(&self) -> Duration {
        let last = self.last_progress_ms.load(Ordering::Relaxed);
        self.base
            .elapsed()
            .saturating_sub(Duration::from_millis(last))
    }

    fn admitted_stream_prefix<'a>(&self, channel: &AtomicUsize, text: &'a str) -> &'a str {
        let offered_chars = text.chars().count();
        let previous = self
            .streamed_chars
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                Some(
                    used.saturating_add(offered_chars)
                        .min(self.max_streamed_chars),
                )
            })
            .unwrap_or_else(|used| used);
        let admitted_chars = offered_chars.min(self.max_streamed_chars.saturating_sub(previous));
        channel.fetch_add(admitted_chars, Ordering::Relaxed);
        if admitted_chars < offered_chars {
            self.budget_exceeded.store(true, Ordering::Release);
            self.cancellation.cancel();
        }
        if admitted_chars == offered_chars {
            return text;
        }
        let end = text
            .char_indices()
            .nth(admitted_chars)
            .map_or(text.len(), |(index, _)| index);
        &text[..end]
    }

    fn budget_exceeded(&self) -> bool {
        self.budget_exceeded.load(Ordering::Acquire)
    }

    fn output(&self) -> AttemptOutput {
        AttemptOutput {
            assistant_chars: self.assistant_chars.load(Ordering::Relaxed),
            reasoning_chars: self.reasoning_chars.load(Ordering::Relaxed),
        }
    }

    /// This attempt's streamed output will not become the turn's response.
    /// Tell the surfaces to withdraw what they already showed live; must run
    /// before any retry status line so the retraction lands first.
    fn discard(&self) -> AttemptOutput {
        let output = self.output();
        if output.assistant_chars + output.reasoning_chars > 0 {
            self.inner.attempt_discarded();
        }
        output
    }
}

impl crate::output::OutputSink for AttemptSink {
    fn assistant_delta(&self, text: &str) {
        self.note_progress();
        let admitted = self.admitted_stream_prefix(&self.assistant_chars, text);
        if !admitted.is_empty() {
            self.inner.assistant_delta(admitted);
        }
    }

    fn assistant_done(&self) {
        self.inner.assistant_done();
    }

    fn reasoning_delta(&self, text: &str) {
        self.note_progress();
        let admitted = self.admitted_stream_prefix(&self.reasoning_chars, text);
        if !admitted.is_empty() {
            self.inner.reasoning_delta(admitted);
        }
    }

    fn thinking(&self, text: &str) {
        self.note_progress();
        self.inner.thinking(text);
    }

    fn transient_status(&self, text: &str) {
        self.inner.transient_status(text);
    }

    fn status(&self, text: &str) {
        self.inner.status(text);
    }

    fn error(&self, text: &str) {
        self.inner.error(text);
    }
}

impl Agent {
    pub(super) fn session_budget_exhaustion(
        &self,
    ) -> Option<crate::run_budget::RunBudgetExhaustion> {
        if let Some(limit) = self.budget.max_session_turns {
            let used = self.usage.session_turn_count();
            if used >= limit {
                return Some(crate::run_budget::RunBudgetExhaustion::SessionTurns { limit, used });
            }
        }
        if let Some(limit_chars) = self.budget.max_session_output_chars {
            let used_chars = self.usage.session_output_chars();
            if used_chars >= limit_chars {
                return Some(crate::run_budget::RunBudgetExhaustion::SessionOutput {
                    limit_chars,
                    used_chars,
                });
            }
        }
        if let (Some(limit_micros), Some(used_micros)) = (
            self.budget.max_session_cost_micros,
            self.usage.exact_session_cost_micros(),
        ) && used_micros >= limit_micros
        {
            return Some(crate::run_budget::RunBudgetExhaustion::SessionCost {
                limit_micros,
                used_micros,
            });
        }
        None
    }

    pub(super) fn generation_budget(&self) -> GenerationBudget {
        GenerationBudget::for_context(
            self.budget.context_budget_tokens,
            self.budget.max_generation_duration,
            self.budget.max_streamed_chars,
        )
    }

    pub(super) async fn chat_stream_with_retry(
        &mut self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[async_openai::types::chat::ChatCompletionTool],
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> std::result::Result<RetriedStream, ProviderRetryError> {
        self.chat_stream_with_retry_options(
            messages,
            tools,
            crate::provider::ProviderRequestOptions::default(),
            cancellation_token,
            sink,
        )
        .await
    }

    pub(super) async fn chat_stream_with_retry_options(
        &mut self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[async_openai::types::chat::ChatCompletionTool],
        options: crate::provider::ProviderRequestOptions,
        cancellation_token: CancellationToken,
        sink: SharedSink,
    ) -> std::result::Result<RetriedStream, ProviderRetryError> {
        let mut attempt = 0;
        let mut attempts = Vec::new();
        if let Some(exhaustion) = self.session_budget_exhaustion() {
            return Err(ProviderRetryError::new(
                anyhow::Error::new(exhaustion),
                attempts,
            ));
        }
        let base_budget = self.generation_budget();
        let mut session_output_remaining = self.budget.max_session_output_chars.map(|limit| {
            (
                limit,
                limit.saturating_sub(self.usage.session_output_chars()),
            )
        });
        loop {
            let (budget, output_exhaustion) =
                output_budget_for_attempt(base_budget, session_output_remaining);
            let attempt_cancellation = cancellation_token.child_token();
            // Set the retraction floor before the first delta: if this attempt
            // dies after streaming, `discard` withdraws exactly what it
            // streamed and nothing from earlier calls.
            sink.attempt_started();
            let attempt_sink = AttemptSink::new(sink.clone(), budget, attempt_cancellation.clone());
            let provider_sink: SharedSink = attempt_sink.clone();
            let started_at = std::time::Instant::now();
            let provider_call = self.provider.chat_stream_with_options(
                messages,
                tools,
                options,
                attempt_cancellation.clone(),
                provider_sink,
            );
            // Optional inactivity deadline: when a generation budget is set,
            // wake only after the stream has been idle (no streamed delta) for
            // `max_duration`. Each delta advances the sink's idle floor, so a
            // generation that keeps producing output — however long in total —
            // never trips it; only a genuinely stalled stream does. Unset (the
            // default) leaves the call unbounded here; the stall guard and
            // retry limits catch a stuck model instead.
            let response = match budget.max_duration {
                Some(max_duration) => {
                    let idle_deadline = {
                        let attempt_sink = attempt_sink.clone();
                        async move {
                            loop {
                                let idle = attempt_sink.idle_duration();
                                if idle >= max_duration {
                                    break;
                                }
                                sleep(max_duration - idle).await;
                            }
                        }
                    };
                    tokio::select! {
                        response = provider_call => Some(response),
                        _ = idle_deadline => {
                            attempt_cancellation.cancel();
                            None
                        }
                    }
                }
                None => Some(provider_call.await),
            };
            let Some(response) = response else {
                return Ok(budget_exit(
                    &attempt_sink,
                    attempt,
                    started_at,
                    attempts,
                    crate::run_budget::RunBudgetExhaustion::GenerationTime {
                        limit_seconds: budget.max_duration.map_or(0, |d| d.as_secs()),
                    },
                ));
            };
            if attempt_sink.budget_exceeded() {
                return Ok(budget_exit(
                    &attempt_sink,
                    attempt,
                    started_at,
                    attempts,
                    output_exhaustion,
                ));
            }
            match response {
                Ok(response) => {
                    let output = attempt_sink.output();
                    attempts.push(attempt_report(
                        attempt + 1,
                        started_at.elapsed(),
                        &output,
                        &response,
                    ));
                    return Ok(RetriedStream {
                        response,
                        attempts,
                        budget_exhaustion: None,
                    });
                }
                Err(err) => {
                    let output = attempt_sink.discard();
                    // Compute the (owned) backoff while the borrow of `err` is
                    // live, then fall through to retry; any non-retryable error
                    // or exhausted budget returns `err` directly.
                    let retry = match err.is_retryable() {
                        true if attempt < MAX_PROVIDER_RETRIES => Some((
                            self.retry_backoff.delay(&err, attempt),
                            retry_status_label(&err),
                        )),
                        _ => None,
                    };
                    attempts.push(failed_attempt_report(
                        attempt + 1,
                        started_at.elapsed(),
                        &output,
                        Some(&err),
                        retry.map(|(delay, _)| delay),
                    ));
                    if let Some((limit, remaining)) = session_output_remaining.as_mut() {
                        *remaining = remaining.saturating_sub(
                            output
                                .assistant_chars
                                .saturating_add(output.reasoning_chars),
                        );
                        if *remaining == 0 {
                            return Ok(RetriedStream {
                                response: generation_budget_response(),
                                attempts,
                                budget_exhaustion: Some(
                                    crate::run_budget::RunBudgetExhaustion::SessionOutput {
                                        limit_chars: *limit,
                                        used_chars: *limit,
                                    },
                                ),
                            });
                        }
                    }
                    let Some((delay, retry_label)) = retry else {
                        if !cancellation_token.is_cancelled()
                            && let Some(model) = self.activate_provider_fallback()
                        {
                            sink.status(&format!("[provider failover] switching to {model}"));
                            attempt = 0;
                            continue;
                        }
                        if let Some(limit) = err.limit_kind() {
                            let exhaustion = match limit {
                                crate::provider::ProviderLimitKind::RateLimit => {
                                    crate::run_budget::RunBudgetExhaustion::ProviderRateLimit {
                                        attempts: attempt + 1,
                                    }
                                }
                                crate::provider::ProviderLimitKind::Quota => {
                                    crate::run_budget::RunBudgetExhaustion::ProviderQuota
                                }
                            };
                            return Err(ProviderRetryError::new(
                                anyhow::Error::new(exhaustion),
                                attempts,
                            ));
                        }
                        return Err(ProviderRetryError::new(err.into(), attempts));
                    };
                    let retry_status = if delay.is_zero() {
                        format!(
                            "[{retry_label}] retrying immediately (attempt {}/{MAX_PROVIDER_RETRIES})",
                            attempt + 1,
                        )
                    } else {
                        format!(
                            "[{retry_label}] retrying in {}s (attempt {}/{MAX_PROVIDER_RETRIES})",
                            delay.as_secs(),
                            attempt + 1,
                        )
                    };
                    sink.status(&retry_status);
                    tokio::select! {
                        _ = cancellation_token.cancelled() => {
                            return Ok(RetriedStream {
                                response: StreamedResponse::interrupted(),
                                attempts,
                                budget_exhaustion: None,
                            });
                        }
                        _ = sleep(delay) => {}
                    }
                    attempt += 1;
                }
            }
        }
    }
}

fn output_budget_for_attempt(
    mut budget: GenerationBudget,
    session_output_remaining: Option<(usize, usize)>,
) -> (GenerationBudget, crate::run_budget::RunBudgetExhaustion) {
    if let Some((limit_chars, remaining_chars)) = session_output_remaining
        && remaining_chars <= budget.max_streamed_chars
    {
        budget.max_streamed_chars = remaining_chars;
        return (
            budget,
            crate::run_budget::RunBudgetExhaustion::SessionOutput {
                limit_chars,
                used_chars: limit_chars,
            },
        );
    }
    (
        budget,
        crate::run_budget::RunBudgetExhaustion::ProviderOutput {
            limit_chars: budget.max_streamed_chars,
        },
    )
}

fn generation_budget_response() -> StreamedResponse {
    StreamedResponse {
        terminal: crate::provider::StreamTerminal::Incomplete(
            crate::provider::FinishReason::GenerationBudget,
        ),
        ..StreamedResponse::default()
    }
}

/// The generation-budget exit ritual shared by the timeout and output-cap
/// paths: discard the aborted attempt, record it in `attempts`, and return the
/// budget-exhausted stream. Centralized so no exit path can forget the
/// `attempt_report` push. (The `Err`-arm session-output exit is not routed here:
/// it already recorded a `failed_attempt_report` and only differs in the exhaustion.)
fn budget_exit(
    attempt_sink: &AttemptSink,
    attempt: usize,
    started_at: std::time::Instant,
    mut attempts: Vec<ProviderAttemptReport>,
    exhaustion: crate::run_budget::RunBudgetExhaustion,
) -> RetriedStream {
    let output = attempt_sink.discard();
    let response = generation_budget_response();
    attempts.push(attempt_report(
        attempt + 1,
        started_at.elapsed(),
        &output,
        &response,
    ));
    RetriedStream {
        response,
        attempts,
        budget_exhaustion: Some(exhaustion),
    }
}

fn attempt_report(
    attempt: usize,
    latency: Duration,
    output: &AttemptOutput,
    response: &StreamedResponse,
) -> ProviderAttemptReport {
    let outcome = match &response.terminal {
        crate::provider::StreamTerminal::Completed(_) => ProviderAttemptOutcome::Completed,
        crate::provider::StreamTerminal::Incomplete(_) => ProviderAttemptOutcome::Incomplete,
        crate::provider::StreamTerminal::Interrupted => ProviderAttemptOutcome::Interrupted,
    };
    ProviderAttemptReport {
        attempt,
        outcome,
        latency_ms: crate::util::time::duration_to_ms(latency),
        assistant_chars: output.assistant_chars,
        reasoning_chars: output.reasoning_chars,
        finish_reason: response.finish_reason().cloned(),
        error_class: None,
        backoff_ms: None,
        prompt_tokens: response.usage.map(|usage| u64::from(usage.prompt_tokens)),
        completion_tokens: response
            .usage
            .map(|usage| u64::from(usage.completion_tokens)),
        cache_read_input_tokens: response
            .usage
            .and_then(|usage| usage.input_cache)
            .map(|cache| cache.read_tokens),
        cache_creation_input_tokens: response
            .usage
            .and_then(|usage| usage.input_cache)
            .map(|cache| cache.creation_tokens),
        cache_measured_input_tokens: response
            .usage
            .and_then(|usage| usage.input_cache)
            .map(|cache| cache.total_input_tokens),
    }
}

fn failed_attempt_report(
    attempt: usize,
    latency: Duration,
    output: &AttemptOutput,
    error: Option<&ProviderFailure>,
    backoff: Option<Duration>,
) -> ProviderAttemptReport {
    ProviderAttemptReport {
        attempt,
        outcome: ProviderAttemptOutcome::Failed,
        latency_ms: crate::util::time::duration_to_ms(latency),
        assistant_chars: output.assistant_chars,
        reasoning_chars: output.reasoning_chars,
        finish_reason: None,
        error_class: Some(error.map_or("unclassified", retry_status_label).to_string()),
        backoff_ms: backoff.map(crate::util::time::duration_to_ms),
        prompt_tokens: None,
        completion_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        cache_measured_input_tokens: None,
    }
}

fn retry_status_label(error: &ProviderFailure) -> &'static str {
    if error.limit_kind() == Some(crate::provider::ProviderLimitKind::Quota) {
        return "provider quota";
    }
    match error {
        ProviderFailure::Http {
            code: crate::provider::ProviderErrorCode::RateLimit,
            ..
        }
        | ProviderFailure::Http { status: 429, .. } => "rate limit",
        ProviderFailure::Http {
            code: crate::provider::ProviderErrorCode::Overloaded,
            ..
        }
        | ProviderFailure::Http { status: 529, .. } => "provider overloaded",
        ProviderFailure::Http { .. } => "provider error",
        ProviderFailure::Transport { .. } => "transport error",
        ProviderFailure::Decode { .. } => "decode error",
        ProviderFailure::Configuration { .. } => "configuration error",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[derive(Default)]
    struct CapturingSink {
        assistant: Mutex<String>,
        reasoning: Mutex<String>,
        attempt_floor: Mutex<(usize, usize)>,
        statuses: Mutex<Vec<String>>,
    }

    struct StreamingRetryProvider {
        attempts: AtomicUsize,
    }

    struct ConfigurationFailureProvider {
        attempts: Arc<AtomicUsize>,
    }

    struct CountingSuccessProvider {
        attempts: Arc<AtomicUsize>,
        content: &'static str,
    }

    struct StreamingFailureProvider {
        attempts: Arc<AtomicUsize>,
        failure: ProviderFailure,
    }

    struct CancellationProvider {
        attempts: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for StreamingRetryProvider {
        async fn chat_stream(
            &self,
            _messages: &[ChatCompletionRequestMessage],
            _tools: &[async_openai::types::chat::ChatCompletionTool],
            _cancellation_token: CancellationToken,
            sink: SharedSink,
        ) -> crate::provider::ProviderResult<StreamedResponse> {
            let attempt = self.attempts.fetch_add(1, Ordering::Relaxed);
            if attempt == 0 {
                sink.assistant_delta("abc");
                return Err(ProviderFailure::http(429, "retry", None));
            }
            sink.assistant_delta("def");
            Ok(StreamedResponse {
                terminal: crate::provider::StreamTerminal::Interrupted,
                ..StreamedResponse::default()
            })
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl Provider for ConfigurationFailureProvider {
        async fn chat_stream(
            &self,
            _messages: &[ChatCompletionRequestMessage],
            _tools: &[async_openai::types::chat::ChatCompletionTool],
            _cancellation_token: CancellationToken,
            _sink: SharedSink,
        ) -> crate::provider::ProviderResult<StreamedResponse> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            Err(ProviderFailure::configuration("missing credentials"))
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl Provider for CountingSuccessProvider {
        async fn chat_stream(
            &self,
            _messages: &[ChatCompletionRequestMessage],
            _tools: &[async_openai::types::chat::ChatCompletionTool],
            _cancellation_token: CancellationToken,
            sink: SharedSink,
        ) -> crate::provider::ProviderResult<StreamedResponse> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            sink.assistant_delta(self.content);
            Ok(StreamedResponse {
                content: self.content.to_string(),
                terminal: crate::provider::StreamTerminal::Completed(
                    crate::provider::FinishReason::Stop,
                ),
                usage: Some(crate::provider::TokenUsage {
                    prompt_tokens: 3,
                    completion_tokens: 2,
                    input_cache: None,
                }),
                ..StreamedResponse::default()
            })
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl Provider for StreamingFailureProvider {
        async fn chat_stream(
            &self,
            _messages: &[ChatCompletionRequestMessage],
            _tools: &[async_openai::types::chat::ChatCompletionTool],
            _cancellation_token: CancellationToken,
            sink: SharedSink,
        ) -> crate::provider::ProviderResult<StreamedResponse> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            sink.assistant_delta("discarded-primary");
            Err(self.failure.clone())
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    #[async_trait::async_trait]
    impl Provider for CancellationProvider {
        async fn chat_stream(
            &self,
            _messages: &[ChatCompletionRequestMessage],
            _tools: &[async_openai::types::chat::ChatCompletionTool],
            cancellation_token: CancellationToken,
            _sink: SharedSink,
        ) -> crate::provider::ProviderResult<StreamedResponse> {
            self.attempts.fetch_add(1, Ordering::Relaxed);
            cancellation_token.cancelled().await;
            Ok(StreamedResponse::interrupted())
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// Streams `deltas` chunks, sleeping `step` between each. Used to prove a
    /// slow-but-progressing generation is never abandoned when its per-delta
    /// gap stays under the inactivity budget, even if its total runtime does
    /// not.
    struct SlowStreamingProvider {
        deltas: usize,
        step: Duration,
    }

    #[async_trait::async_trait]
    impl Provider for SlowStreamingProvider {
        async fn chat_stream(
            &self,
            _messages: &[ChatCompletionRequestMessage],
            _tools: &[async_openai::types::chat::ChatCompletionTool],
            cancellation_token: CancellationToken,
            sink: SharedSink,
        ) -> crate::provider::ProviderResult<StreamedResponse> {
            for _ in 0..self.deltas {
                sleep(self.step).await;
                if cancellation_token.is_cancelled() {
                    return Ok(StreamedResponse::interrupted());
                }
                sink.assistant_delta("x");
            }
            Ok(StreamedResponse {
                content: "x".repeat(self.deltas),
                terminal: crate::provider::StreamTerminal::Completed(
                    crate::provider::FinishReason::Stop,
                ),
                ..StreamedResponse::default()
            })
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    /// Produces no output and never returns on its own — the inactivity
    /// deadline must abandon it.
    struct StallingProvider;

    #[async_trait::async_trait]
    impl Provider for StallingProvider {
        async fn chat_stream(
            &self,
            _messages: &[ChatCompletionRequestMessage],
            _tools: &[async_openai::types::chat::ChatCompletionTool],
            _cancellation_token: CancellationToken,
            _sink: SharedSink,
        ) -> crate::provider::ProviderResult<StreamedResponse> {
            sleep(Duration::from_secs(3_600)).await;
            Ok(StreamedResponse::interrupted())
        }

        async fn list_models(&self) -> anyhow::Result<Vec<String>> {
            Ok(Vec::new())
        }
    }

    impl crate::output::OutputSink for CapturingSink {
        fn assistant_delta(&self, text: &str) {
            self.assistant.lock().unwrap().push_str(text);
        }

        fn reasoning_delta(&self, text: &str) {
            self.reasoning.lock().unwrap().push_str(text);
        }

        fn attempt_started(&self) {
            *self.attempt_floor.lock().unwrap() = (
                self.assistant.lock().unwrap().len(),
                self.reasoning.lock().unwrap().len(),
            );
        }

        fn attempt_discarded(&self) {
            let (assistant, reasoning) = *self.attempt_floor.lock().unwrap();
            self.assistant.lock().unwrap().truncate(assistant);
            self.reasoning.lock().unwrap().truncate(reasoning);
        }

        fn status(&self, text: &str) {
            self.statuses.lock().unwrap().push(text.to_string());
        }
    }

    fn fallback_config(
        provider: Box<dyn Provider>,
        provider_id: &str,
        model: &str,
    ) -> crate::tool::SubagentProviderConfig {
        crate::tool::SubagentProviderConfig::new(
            provider,
            32_000,
            PromptEstimator::heuristic(),
            crate::subagent::provider_model_label(provider_id, model),
        )
        .with_active_model_identity(provider_id, model)
    }

    #[test]
    fn explicit_generation_limits_override_context_defaults() {
        let budget = GenerationBudget::for_context(128_000, Some(Duration::from_secs(7)), Some(9));

        assert_eq!(budget.max_duration, Some(Duration::from_secs(7)));
        assert_eq!(budget.max_streamed_chars, 9);
    }

    #[test]
    fn generation_time_budget_is_off_by_default() {
        // No override and no env opt-in: there is no generation deadline at all.
        // A stuck model is the stall guard's job, not a wall-clock cap that
        // would also cut off a slow-but-streaming generation.
        let budget = GenerationBudget::for_context(128_000, None, None);

        assert_eq!(budget.max_duration, None);
    }

    #[tokio::test(start_paused = true)]
    async fn generation_deadline_resets_on_streamed_progress() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        // Six deltas 600ms apart run 3.6s in total — far past the 1s budget —
        // but no single gap between deltas reaches 1s, so the generation is
        // never abandoned.
        let provider = Box::new(SlowStreamingProvider {
            deltas: 6,
            step: Duration::from_millis(600),
        });
        let mut agent = Agent::new(
            provider,
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.set_run_budget(crate::run_budget::RunBudget {
            max_generation_seconds: Some(1),
            ..crate::run_budget::RunBudget::default()
        });

        let stream = agent
            .chat_stream_with_retry(
                &[],
                &[],
                CancellationToken::new(),
                Arc::new(CapturingSink::default()),
            )
            .await
            .unwrap();

        assert!(stream.budget_exhaustion.is_none());
        assert_eq!(stream.response.content, "xxxxxx");
    }

    #[tokio::test(start_paused = true)]
    async fn generation_deadline_abandons_a_stalled_stream() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let provider = Box::new(StallingProvider);
        let mut agent = Agent::new(
            provider,
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.set_run_budget(crate::run_budget::RunBudget {
            max_generation_seconds: Some(1),
            ..crate::run_budget::RunBudget::default()
        });

        let stream = agent
            .chat_stream_with_retry(
                &[],
                &[],
                CancellationToken::new(),
                Arc::new(CapturingSink::default()),
            )
            .await
            .unwrap();

        assert_eq!(
            stream.budget_exhaustion,
            Some(crate::run_budget::RunBudgetExhaustion::GenerationTime { limit_seconds: 1 })
        );
    }

    #[test]
    fn attempt_sink_enforces_exact_combined_char_cap_at_utf8_boundary() {
        let captured = Arc::new(CapturingSink::default());
        let cancellation = CancellationToken::new();
        let budget = GenerationBudget {
            max_duration: Some(Duration::from_secs(1)),
            max_streamed_chars: 5,
        };
        let sink = AttemptSink::new(captured.clone(), budget, cancellation.clone());

        sink.reasoning_delta("éé");
        sink.assistant_delta("abcd");

        assert_eq!(captured.reasoning.lock().unwrap().as_str(), "éé");
        assert_eq!(captured.assistant.lock().unwrap().as_str(), "abc");
        assert_eq!(sink.output().reasoning_chars, 2);
        assert_eq!(sink.output().assistant_chars, 3);
        assert!(sink.budget_exceeded());
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn session_output_remaining_tightens_attempt_budget_and_reason() {
        let base = GenerationBudget {
            max_duration: Some(Duration::from_secs(1)),
            max_streamed_chars: 100,
        };

        let (budget, reason) = output_budget_for_attempt(base, Some((1_000, 25)));

        assert_eq!(budget.max_streamed_chars, 25);
        assert_eq!(
            reason,
            crate::run_budget::RunBudgetExhaustion::SessionOutput {
                limit_chars: 1_000,
                used_chars: 1_000,
            }
        );
    }

    #[test]
    fn per_call_output_budget_wins_when_it_is_lower_than_session_remaining() {
        let base = GenerationBudget {
            max_duration: Some(Duration::from_secs(1)),
            max_streamed_chars: 100,
        };

        let (budget, reason) = output_budget_for_attempt(base, Some((1_000, 250)));

        assert_eq!(budget.max_streamed_chars, 100);
        assert_eq!(
            reason,
            crate::run_budget::RunBudgetExhaustion::ProviderOutput { limit_chars: 100 }
        );
    }

    #[tokio::test]
    async fn retry_attempts_share_one_exact_session_output_allowance() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let provider = Box::new(StreamingRetryProvider {
            attempts: AtomicUsize::new(0),
        });
        let mut agent = Agent::new(
            provider,
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.set_retry_backoff(RetryBackoff::Immediate);
        agent.set_run_budget(crate::run_budget::RunBudget {
            max_output_chars: Some(100),
            max_session_output_chars: Some(5),
            ..crate::run_budget::RunBudget::default()
        });

        let error = agent
            .run(
                "test",
                CancellationToken::new(),
                Arc::new(CapturingSink::default()),
            )
            .await
            .unwrap_err();

        assert_eq!(
            error.downcast_ref::<crate::run_budget::RunBudgetExhaustion>(),
            Some(&crate::run_budget::RunBudgetExhaustion::SessionOutput {
                limit_chars: 5,
                used_chars: 5,
            })
        );
        assert_eq!(agent.session_budget_usage().output_chars, 5);
    }

    #[tokio::test]
    async fn configuration_failure_is_not_retried() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider = Box::new(ConfigurationFailureProvider {
            attempts: attempts.clone(),
        });
        let mut agent = Agent::new(
            provider,
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.set_retry_backoff(RetryBackoff::Immediate);

        let error = agent
            .run(
                "test",
                CancellationToken::new(),
                Arc::new(CapturingSink::default()),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("missing credentials"));
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn permanent_failure_switches_immediately_and_backup_is_sticky() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let primary_attempts = Arc::new(AtomicUsize::new(0));
        let backup_attempts = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(
            Box::new(StreamingFailureProvider {
                attempts: primary_attempts.clone(),
                failure: ProviderFailure::configuration("bad primary"),
            }),
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.set_provider_fallback(fallback_config(
            Box::new(CountingSuccessProvider {
                attempts: backup_attempts.clone(),
                content: "backup",
            }),
            "backup-provider",
            "backup-model",
        ));
        let sink = Arc::new(CapturingSink::default());

        for _ in 0..2 {
            agent
                .chat_stream_with_retry(&[], &[], CancellationToken::new(), sink.clone())
                .await
                .expect("backup succeeds");
        }

        assert_eq!(primary_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(backup_attempts.load(Ordering::Relaxed), 2);
        assert_eq!(sink.assistant.lock().unwrap().as_str(), "backupbackup");
        assert_eq!(
            sink.statuses.lock().unwrap().as_slice(),
            ["[provider failover] switching to backup-provider:backup-model"]
        );
    }

    #[tokio::test]
    async fn activating_fallback_preserves_a_nested_backup() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let mut agent = Agent::new(
            Box::new(ConfigurationFailureProvider {
                attempts: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        let last = fallback_config(
            Box::new(ConfigurationFailureProvider {
                attempts: Arc::new(AtomicUsize::new(0)),
            }),
            "last-provider",
            "last-model",
        );
        let middle = fallback_config(
            Box::new(ConfigurationFailureProvider {
                attempts: Arc::new(AtomicUsize::new(0)),
            }),
            "middle-provider",
            "middle-model",
        )
        .with_backup(Some(last));
        agent.set_provider_fallback(middle);

        assert_eq!(
            agent.activate_provider_fallback().as_deref(),
            Some("middle-provider:middle-model")
        );
        assert_eq!(
            agent.activate_provider_fallback().as_deref(),
            Some("last-provider:last-model")
        );
        assert_eq!(agent.activate_provider_fallback(), None);
    }

    #[tokio::test]
    async fn chained_failover_resets_attempts_and_reaches_final_provider() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let primary_attempts = Arc::new(AtomicUsize::new(0));
        let middle_attempts = Arc::new(AtomicUsize::new(0));
        let final_attempts = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(
            Box::new(StreamingFailureProvider {
                attempts: primary_attempts.clone(),
                failure: ProviderFailure::transport("primary reset"),
            }),
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.set_retry_backoff(RetryBackoff::Immediate);
        let final_config = fallback_config(
            Box::new(CountingSuccessProvider {
                attempts: final_attempts.clone(),
                content: "final-success",
            }),
            "final-provider",
            "final-model",
        );
        let middle_config = fallback_config(
            Box::new(StreamingFailureProvider {
                attempts: middle_attempts.clone(),
                failure: ProviderFailure::transport("middle reset"),
            }),
            "middle-provider",
            "middle-model",
        )
        .with_backup(Some(final_config));
        agent.set_provider_fallback(middle_config);
        let sink = Arc::new(CapturingSink::default());

        let response = agent
            .chat_stream_with_retry(&[], &[], CancellationToken::new(), sink.clone())
            .await
            .expect("final fallback succeeds")
            .response;

        assert_eq!(response.content, "final-success");
        assert_eq!(
            primary_attempts.load(Ordering::Relaxed),
            MAX_PROVIDER_RETRIES + 1
        );
        assert_eq!(
            middle_attempts.load(Ordering::Relaxed),
            MAX_PROVIDER_RETRIES + 1,
            "retry counter resets after switching providers"
        );
        assert_eq!(final_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(sink.assistant.lock().unwrap().as_str(), "final-success");
        let statuses = sink.statuses.lock().unwrap();
        let failovers = statuses
            .iter()
            .filter(|status| status.starts_with("[provider failover]"))
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            failovers,
            [
                "[provider failover] switching to middle-provider:middle-model",
                "[provider failover] switching to final-provider:final-model",
            ]
        );
    }

    #[tokio::test]
    async fn transient_primary_exhausts_retries_before_failover() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let primary_attempts = Arc::new(AtomicUsize::new(0));
        let backup_attempts = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(
            Box::new(StreamingFailureProvider {
                attempts: primary_attempts.clone(),
                failure: ProviderFailure::transport("reset"),
            }),
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.set_retry_backoff(RetryBackoff::Immediate);
        agent.set_provider_fallback(fallback_config(
            Box::new(CountingSuccessProvider {
                attempts: backup_attempts.clone(),
                content: "recovered",
            }),
            "backup-provider",
            "backup-model",
        ));
        let sink = Arc::new(CapturingSink::default());

        agent
            .chat_stream_with_retry(&[], &[], CancellationToken::new(), sink.clone())
            .await
            .expect("backup succeeds after retries");

        assert_eq!(
            primary_attempts.load(Ordering::Relaxed),
            MAX_PROVIDER_RETRIES + 1
        );
        assert_eq!(backup_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(sink.assistant.lock().unwrap().as_str(), "recovered");
    }

    #[tokio::test]
    async fn sustained_rate_limit_returns_a_persistable_terminal_reason() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(
            Box::new(StreamingFailureProvider {
                attempts: attempts.clone(),
                failure: ProviderFailure::http(429, "requests per minute exceeded", None),
            }),
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.set_retry_backoff(RetryBackoff::Immediate);

        let error = agent
            .run(
                "test",
                CancellationToken::new(),
                Arc::new(CapturingSink::default()),
            )
            .await
            .unwrap_err();

        assert_eq!(
            error.downcast_ref::<crate::run_budget::RunBudgetExhaustion>(),
            Some(&crate::run_budget::RunBudgetExhaustion::ProviderRateLimit {
                attempts: MAX_PROVIDER_RETRIES + 1,
            })
        );
        assert_eq!(attempts.load(Ordering::Relaxed), MAX_PROVIDER_RETRIES + 1);
    }

    #[tokio::test]
    async fn provider_quota_wall_stops_without_retries() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(
            Box::new(StreamingFailureProvider {
                attempts: attempts.clone(),
                failure: ProviderFailure::http(429, "code=insufficient_quota", None),
            }),
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();

        let error = agent
            .run(
                "test",
                CancellationToken::new(),
                Arc::new(CapturingSink::default()),
            )
            .await
            .unwrap_err();

        assert_eq!(
            error.downcast_ref::<crate::run_budget::RunBudgetExhaustion>(),
            Some(&crate::run_budget::RunBudgetExhaustion::ProviderQuota)
        );
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cancellation_never_activates_backup() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let primary_attempts = Arc::new(AtomicUsize::new(0));
        let backup_attempts = Arc::new(AtomicUsize::new(0));
        let mut agent = Agent::new(
            Box::new(CancellationProvider {
                attempts: primary_attempts.clone(),
            }),
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.set_provider_fallback(fallback_config(
            Box::new(CountingSuccessProvider {
                attempts: backup_attempts.clone(),
                content: "must-not-run",
            }),
            "backup-provider",
            "backup-model",
        ));
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let response = agent
            .chat_stream_with_retry(&[], &[], cancellation, Arc::new(CapturingSink::default()))
            .await
            .expect("cancellation is an interrupted response")
            .response;

        assert!(response.is_interrupted());
        assert_eq!(primary_attempts.load(Ordering::Relaxed), 1);
        assert_eq!(backup_attempts.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn successful_fallback_turn_is_attributed_to_backup_model() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let mut agent = Agent::builder(
            Box::new(ConfigurationFailureProvider {
                attempts: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            fixture.project_root.clone(),
        )
        .active_model_identity(ActiveModelIdentity {
            provider_id: "primary-provider".parse().unwrap(),
            model: "primary-model".to_string(),
        })
        .build()
        .unwrap();
        agent.set_provider_fallback(fallback_config(
            Box::new(CountingSuccessProvider {
                attempts: Arc::new(AtomicUsize::new(0)),
                content: "done",
            }),
            "backup-provider",
            "backup-model",
        ));

        agent
            .run(
                "test",
                CancellationToken::new(),
                Arc::new(CapturingSink::default()),
            )
            .await
            .expect("backup completes run");

        let turn = agent.usage_turns().last().expect("usage turn");
        assert_eq!(turn.provider_id.as_deref(), Some("backup-provider"));
        assert_eq!(turn.model.as_deref(), Some("backup-model"));
    }

    #[tokio::test]
    async fn exact_session_cost_budget_stops_before_provider_call() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider = Box::new(ConfigurationFailureProvider {
            attempts: attempts.clone(),
        });
        let mut agent = Agent::new(
            provider,
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.restore_usage_totals(100, 10, Some(5_000_000), Some(5_000_000), None);
        agent.set_run_budget(crate::run_budget::RunBudget {
            max_session_cost_micros: Some(5_000_000),
            ..crate::run_budget::RunBudget::default()
        });

        let error = agent
            .run(
                "test",
                CancellationToken::new(),
                Arc::new(CapturingSink::default()),
            )
            .await
            .unwrap_err();

        assert_eq!(
            error.downcast_ref::<crate::run_budget::RunBudgetExhaustion>(),
            Some(&crate::run_budget::RunBudgetExhaustion::SessionCost {
                limit_micros: 5_000_000,
                used_micros: 5_000_000,
            })
        );
        assert_eq!(attempts.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn incomplete_session_cost_never_claims_exact_exhaustion() {
        let fixture = crate::tool::test_utils::TestFixture::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let provider = Box::new(ConfigurationFailureProvider {
            attempts: attempts.clone(),
        });
        let mut agent = Agent::new(
            provider,
            Arc::new(ToolRegistry::new()),
            Arc::new(ToolRegistry::new()),
            fixture.read_tracker.clone(),
            String::new(),
            fixture.project_root.clone(),
        )
        .unwrap();
        agent.restore_usage_totals(100, 10, Some(5_000_000), Some(5_000_000), None);
        agent.usage.record(
            None,
            None,
            crate::agent::usage_ledger::UsageTurnDiagnostics::default(),
        );
        agent.set_run_budget(crate::run_budget::RunBudget {
            max_session_cost_micros: Some(5_000_000),
            ..crate::run_budget::RunBudget::default()
        });

        let error = agent
            .run(
                "test",
                CancellationToken::new(),
                Arc::new(CapturingSink::default()),
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("missing credentials"));
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }
}
