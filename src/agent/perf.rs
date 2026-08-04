//! Runtime performance snapshots for the `/perf` command.

use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use super::*;
use crate::diff::FileDiff;
use crate::provider::TokenCounterKind;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PerfReport {
    pub(crate) preflight: PreflightPerf,
    pub(crate) provider: ProviderPerf,
    pub(crate) prompt: PromptPerf,
    pub(crate) cache: CachePerf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PreflightPerf {
    pub(crate) total_duration: Duration,
    pub(crate) token_count_duration: Duration,
    pub(crate) token_count_calls: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ProviderPerf {
    pub(crate) first_output_duration: Option<Duration>,
    pub(crate) total_duration: Duration,
    pub(crate) attempts: Vec<ProviderAttemptReport>,
    pub(crate) generation_budget: GenerationBudget,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PromptPerf {
    pub(crate) prompt_tokens: usize,
    pub(crate) estimate_source: TokenCounterKind,
    pub(crate) estimate_confidence: EstimateConfidence,
    pub(crate) request_body_bytes: Option<usize>,
    pub(crate) request_body_hash: Option<String>,
    pub(crate) request_preview: Option<crate::provider::ProviderRequestPreview>,
    pub(crate) tool_schema_tokens: usize,
    pub(crate) tool_schema_bytes: usize,
    pub(crate) tool_schema_hash: Option<String>,
    pub(crate) tool_names: Vec<String>,
    pub(crate) tool_count: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CachePerf {
    pub(crate) cacheable_prefix_tokens: usize,
    pub(crate) volatile_tail_tokens: usize,
    pub(crate) cache_mechanism: Option<String>,
    pub(crate) route_fingerprint: Option<String>,
    /// Retained for persisted-schema compatibility. Raw JSON bytes cannot be
    /// converted proportionally into model tokens, so new turns leave this
    /// unset instead of reporting a fabricated token count.
    pub(crate) local_reusable_prefix_tokens: Option<usize>,
    /// Exact common-prefix percentage of consecutive serialized request bodies.
    /// This is a wire-byte diagnostic, not provider-reported cache usage.
    pub(crate) local_reusable_prefix_percent: Option<u64>,
    /// Fingerprint of the byte-stable system prefix (persona + project context,
    /// before the volatile tail). Two adjacent turns with different fingerprints
    /// reveal prompt-cache prefix churn — a known prompt-cache failure mode.
    /// Empty when there is no system prefix to hash.
    pub(crate) prefix_hash: String,
}

#[derive(Debug, Default)]
pub(super) struct PreflightPerfCapture {
    token_count_duration: Duration,
    token_count_calls: usize,
}

impl PreflightPerfCapture {
    pub(super) fn record_token_count(&mut self, duration: Duration) {
        self.token_count_duration = self.token_count_duration.saturating_add(duration);
        self.token_count_calls = self.token_count_calls.saturating_add(1);
    }

    pub(super) fn finish(self, total_duration: Duration) -> PreflightPerf {
        PreflightPerf {
            total_duration,
            token_count_duration: self.token_count_duration,
            token_count_calls: self.token_count_calls,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct FirstOutputTracker {
    first_output_at: StdMutex<Option<Instant>>,
}

impl FirstOutputTracker {
    fn mark(&self, instant: Instant) {
        let Ok(mut first_output_at) = self.first_output_at.lock() else {
            return;
        };
        if first_output_at.is_none() {
            *first_output_at = Some(instant);
        }
    }

    pub(super) fn duration_since(&self, started_at: Instant) -> Option<Duration> {
        self.first_output_at
            .lock()
            .ok()
            .and_then(|first_output_at| {
                first_output_at.map(|instant| instant.saturating_duration_since(started_at))
            })
    }
}

pub(super) struct PerfSink {
    inner: SharedSink,
    first_output: Arc<FirstOutputTracker>,
}

impl PerfSink {
    pub(super) fn shared(inner: SharedSink) -> (SharedSink, Arc<FirstOutputTracker>) {
        let first_output = Arc::new(FirstOutputTracker::default());
        let sink: SharedSink = Arc::new(Self {
            inner,
            first_output: first_output.clone(),
        });
        (sink, first_output)
    }

    fn mark_first_output(&self, text: &str) {
        if !text.is_empty() {
            self.first_output.mark(Instant::now());
        }
    }
}

impl OutputSink for PerfSink {
    fn assistant_delta(&self, text: &str) {
        self.mark_first_output(text);
        self.inner.assistant_delta(text);
    }

    fn assistant_done(&self) {
        self.inner.assistant_done();
    }

    fn reasoning_delta(&self, text: &str) {
        self.mark_first_output(text);
        self.inner.reasoning_delta(text);
    }

    fn attempt_started(&self) {
        self.inner.attempt_started();
    }

    fn attempt_discarded(&self) {
        // `first_output` deliberately keeps the earliest instant even when
        // that attempt is retracted: time-to-first-output measures what the
        // user saw, and they saw the discarded stream.
        self.inner.attempt_discarded();
    }

    fn thinking(&self, text: &str) {
        self.inner.thinking(text);
    }

    fn tool_calls_started(&self, calls: &[ToolCallStart]) {
        self.inner.tool_calls_started(calls);
    }

    fn tool_started(&self, id: &str, name: &str, arguments: &str) {
        self.inner.tool_started(id, name, arguments);
    }

    fn tool_output(&self, id: &str, output: &str) {
        self.inner.tool_output(id, output);
    }

    fn tool_finished(&self, id: &str, result: &str, status: crate::output::ToolExecutionStatus) {
        self.inner.tool_finished(id, result, status);
    }

    fn tool_finished_with_diff(
        &self,
        id: &str,
        result: &str,
        status: crate::output::ToolExecutionStatus,
        diff: FileDiff,
    ) {
        self.inner.tool_finished_with_diff(id, result, status, diff);
    }

    fn delivery_barrier(&self) -> Option<crate::output::OutputDeliveryBarrier> {
        self.inner.delivery_barrier()
    }

    fn workspace_changed(&self, paths: &[String], intent: &str) {
        self.inner.workspace_changed(paths, intent);
    }

    fn queued_user_message_sent(&self, id: u64, text: &str) {
        self.inner.queued_user_message_sent(id, text);
    }

    fn context_updated(&self, report: ContextReport) {
        self.inner.context_updated(report);
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
    pub(super) async fn estimate_prompt_for_preflight(
        &self,
        messages: &[ChatCompletionRequestMessage],
        tools: &[ChatCompletionTool],
        tool_schema_tokens: usize,
        capture: &mut PreflightPerfCapture,
    ) -> PromptEstimate {
        let started_at = Instant::now();
        let estimate = self
            .prompt_estimator
            .estimate_prompt_with_tool_schema_tokens(messages, tools, tool_schema_tokens)
            .await;
        capture.record_token_count(started_at.elapsed());
        estimate
    }

    pub(super) fn build_perf_report(
        &mut self,
        preflight: PreflightPerf,
        provider: ProviderPerf,
        tool_schema: &ToolSchemaPayload,
        request_messages: &[ChatCompletionRequestMessage],
    ) -> PerfReport {
        let estimate = self
            .caches
            .last_sent_prompt_estimate
            .clone()
            .unwrap_or_else(|| PromptEstimate::heuristic(0, 0, TokenCounterKind::Heuristic));
        let (request_body, request_preview, cache_mechanism, cache_route_key) = self
            .provider
            .take_last_request_diagnostics()
            .map_or((None, None, None, None), |diagnostics| {
                (
                    Some(diagnostics.serialized_body),
                    Some(diagnostics.preview),
                    diagnostics.cache_mechanism,
                    diagnostics.cache_route_key,
                )
            });
        let request_body_hash = request_body.as_deref().map(short_hash_bytes);
        let request_body_bytes = request_body.as_ref().map(Vec::len);
        let route_fingerprint = cache_route_key
            .as_deref()
            .map(|key| short_hash_bytes(key.as_bytes()));
        let local_reusable_prefix_percent = request_body
            .as_deref()
            .zip(self.caches.previous_request_body.as_deref())
            .map(|(current, previous)| {
                let reusable_bytes = common_prefix_len(current, previous);
                reusable_bytes
                    .saturating_mul(100)
                    .checked_div(current.len().max(1))
                    .unwrap_or(0) as u64
            });
        // IMPORTANT DIAGNOSTIC INVARIANT: JSON byte position and tokenizer
        // position are not proportional. Do not revive the old
        // `prompt_tokens * common_bytes / body_bytes` estimate; it produced
        // plausible-looking but materially false cache-token totals.
        let local_reusable_prefix_tokens = None;
        self.caches.previous_request_body = request_body;
        let (request_message_tokens, _) = self
            .prompt_estimator
            .estimate_messages_for_report_with_tool_schema_tokens(request_messages, 0);
        let (cacheable_prefix_tokens, volatile_tail_tokens) = prompt_cache_token_totals(
            request_messages,
            &request_message_tokens,
            request_messages.len(),
            system_prompt(self.mode),
            self.project_context.as_ref(),
            &self.prompt_estimator,
            tool_schema.report_tool_schema_tokens(),
        );

        PerfReport {
            preflight,
            provider,
            prompt: PromptPerf {
                prompt_tokens: estimate.input_tokens,
                estimate_source: estimate.source,
                estimate_confidence: estimate.confidence,
                request_body_bytes,
                request_body_hash,
                request_preview,
                tool_schema_tokens: tool_schema.model_tool_schema_tokens(),
                tool_schema_bytes: tool_schema.serialized_bytes_len(),
                tool_schema_hash: tool_schema.serialized_hash().map(str::to_string),
                tool_names: tool_schema.names().to_vec(),
                tool_count: tool_schema.tools().len(),
            },
            cache: CachePerf {
                cacheable_prefix_tokens,
                volatile_tail_tokens,
                cache_mechanism,
                route_fingerprint,
                local_reusable_prefix_tokens,
                local_reusable_prefix_percent,
                prefix_hash: stable_prefix_fingerprint(request_messages),
            },
        }
    }

    pub(crate) fn perf_report_text(&self) -> String {
        let mut report = format_perf_report(
            self.caches.last_perf_report.as_ref(),
            &self.context_report(),
        );
        if let Some((groups, median, singles, maximum)) =
            delegation_width_summary(&self.usage.usage_turns)
        {
            report.push_str(&format!(
                "\ndelegation       {groups} launch groups | median width {median} | width=1 repeated {singles} groups | max {maximum}"
            ));
        }
        if !self.self_review_runs.is_empty() {
            let findings = self
                .self_review_runs
                .iter()
                .map(|run| u64::from(run.findings.total()))
                .sum::<u64>();
            let fixed = self
                .self_review_runs
                .iter()
                .filter(|run| run.disposition == Some(SelfReviewDisposition::Fixed))
                .count();
            let rebutted = self
                .self_review_runs
                .iter()
                .filter(|run| run.disposition == Some(SelfReviewDisposition::Rebutted))
                .count();
            let rebuttal_rate = rebutted.saturating_mul(100) / self.self_review_runs.len();
            let calibration = if rebuttal_rate > 50 {
                " | ALERT: tighten reviewer prompt"
            } else {
                ""
            };
            report.push_str(&format!(
                "\nself-review      {} runs | {findings} findings | {fixed} fixed | {rebutted} rebutted ({rebuttal_rate}%){calibration}",
                self.self_review_runs.len(),
            ));
        }
        report
    }
}

fn delegation_width_summary(turns: &[UsageTurn]) -> Option<(usize, usize, usize, usize)> {
    let mut groups = std::collections::BTreeMap::<&str, std::collections::BTreeSet<&str>>::new();
    for turn in turns {
        let (Some(group), Some(parent_call)) = (
            turn.launch_group_id.as_deref(),
            turn.parent_tool_call_id.as_deref(),
        ) else {
            continue;
        };
        groups.entry(group).or_default().insert(parent_call);
    }
    if groups.is_empty() {
        return None;
    }
    let mut widths = groups
        .values()
        .map(std::collections::BTreeSet::len)
        .collect::<Vec<_>>();
    widths.sort_unstable();
    let median = widths[(widths.len() - 1) / 2];
    let singles = widths.iter().filter(|width| **width == 1).count();
    let maximum = widths.last().copied().unwrap_or(1);
    Some((widths.len(), median, singles, maximum))
}

fn format_perf_report(report: Option<&PerfReport>, context: &ContextReport) -> String {
    let mut lines = Vec::with_capacity(14);
    if let Some(report) = report {
        lines.push("Performance: last model call".to_string());
        lines.push(format!(
            "preflight        {} total | token count {} ({})",
            format_duration(report.preflight.total_duration),
            format_duration(report.preflight.token_count_duration),
            format_count(report.preflight.token_count_calls, "call")
        ));
        lines.push(format!(
            "generation       budget {} | {} streamed chars (~{} tok)",
            report
                .provider
                .generation_budget
                .max_duration
                .map_or_else(|| "off".to_string(), format_duration),
            group_thousands(report.provider.generation_budget.max_streamed_chars),
            group_thousands(
                report
                    .provider
                    .generation_budget
                    .max_streamed_chars
                    .saturating_div(4)
            )
        ));
        lines.push(format!(
            "provider         first output {} | total {} | {} ({} failed)",
            report
                .provider
                .first_output_duration
                .map(format_duration)
                .unwrap_or_else(|| "n/a".to_string()),
            format_duration(report.provider.total_duration),
            format_count(report.provider.attempts.len(), "attempt"),
            report
                .provider
                .attempts
                .iter()
                .filter(|attempt| matches!(attempt.outcome, ProviderAttemptOutcome::Failed))
                .count()
        ));
        lines.push(format!(
            "prompt           {} tok | {} request body{}",
            group_thousands(report.prompt.prompt_tokens),
            report
                .prompt
                .request_body_bytes
                .map(format_bytes)
                .unwrap_or_else(|| "n/a".to_string()),
            report
                .prompt
                .request_body_hash
                .as_deref()
                .map(|hash| format!(" | req {hash}"))
                .unwrap_or_default()
        ));
        lines.push(format!(
            "tool schema      {} tok | {} | {}{}",
            group_thousands(report.prompt.tool_schema_tokens),
            format_bytes(report.prompt.tool_schema_bytes),
            format_count(report.prompt.tool_count, "tool"),
            report
                .prompt
                .tool_schema_hash
                .as_deref()
                .map(|hash| format!(" | schema {hash}"))
                .unwrap_or_default()
        ));
        lines.push(format!(
            "estimator        {} | {}",
            report.prompt.estimate_source.label(),
            report.prompt.estimate_confidence.label()
        ));
        lines.push(format!(
            "cache prefix     {} tok cacheable | {} tok volatile",
            group_thousands(report.cache.cacheable_prefix_tokens),
            group_thousands(report.cache.volatile_tail_tokens)
        ));
        if let Some(turn) = context.usage_turns.last() {
            lines.push(format!(
                "execution lane   {}:{} #{}",
                turn.lane_kind.as_db_str(),
                turn.lane_id,
                turn.lane_seq
            ));
        }
        if report.cache.local_reusable_prefix_percent.is_some()
            || report.cache.route_fingerprint.is_some()
        {
            let reusable = report
                .cache
                .local_reusable_prefix_percent
                .map(|percent| format!("{percent}% wire bytes"))
                .unwrap_or_else(|| "first lane turn".to_string());
            let route = report.cache.route_fingerprint.as_deref().unwrap_or("n/a");
            lines.push(format!("cache reuse      {reusable} | route {route}"));
        }
    } else {
        lines.push("Performance: last model call".to_string());
        lines.push(
            "No performance data yet. Run the agent once, then use /perf or /cost.".to_string(),
        );
    }
    lines.push(String::new());
    lines.push("Usage: session".to_string());
    lines.extend(
        crate::context_view::telemetry::CostTelemetry::from_report(context).usage_lines(context),
    );
    lines.join("\n")
}

fn short_hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_string()
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

fn format_duration(duration: Duration) -> String {
    if duration > Duration::ZERO && duration.as_millis() == 0 {
        return "<1 ms".to_string();
    }
    if duration < Duration::from_secs(1) {
        return format!("{} ms", duration.as_millis());
    }
    format!("{:.1} s", duration.as_secs_f64())
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kib = bytes as f64 / 1024.0;
    if kib < 1024.0 {
        return format!("{kib:.1} KiB");
    }
    format!("{:.1} MiB", kib / 1024.0)
}

fn format_count(count: usize, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{} {singular}s", group_thousands(count))
    }
}

fn group_thousands(value: usize) -> String {
    let raw = value.to_string();
    let mut grouped = String::with_capacity(raw.len() + raw.len() / 3);
    for (index, ch) in raw.chars().rev().enumerate() {
        if index > 0 && index % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped.chars().rev().collect()
}

/// Fingerprint the byte-stable head of the first system message (persona +
/// project context, excluding the volatile tail). Used per turn so prompt-cache
/// prefix churn is visible in `usage_turns` — an adjacent-turn hash change is
/// the churn signal. Returns "" when there is no system prefix.
fn stable_prefix_fingerprint(messages: &[ChatCompletionRequestMessage]) -> String {
    let Some(first) = messages.first() else {
        return String::new();
    };
    let Ok(value) = serde_json::to_value(first) else {
        return String::new();
    };
    if value.get("role").and_then(|role| role.as_str()) != Some("system") {
        return String::new();
    }
    let text = crate::context_view::message_content_text(&value);
    if text.is_empty() {
        return String::new();
    }
    // The stable prefix is everything before the volatile-state heading (the same
    // split the codex transport makes between `instructions` and its trailing
    // volatile input item).
    let heading = format!("\n\n{}\n", crate::context::VOLATILE_STATE_HEADING);
    let stable = match text.rfind(&heading) {
        Some(index) => &text[..index],
        None => text.as_str(),
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(stable.as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_msg(text: &str) -> ChatCompletionRequestMessage {
        use async_openai::types::chat::ChatCompletionRequestSystemMessageArgs;
        ChatCompletionRequestSystemMessageArgs::default()
            .content(text)
            .build()
            .unwrap()
            .into()
    }

    #[test]
    fn stable_prefix_fingerprint_ignores_the_volatile_tail() {
        let heading = crate::context::VOLATILE_STATE_HEADING;
        let base = stable_prefix_fingerprint(&[system_msg(&format!(
            "persona and project context\n\n{heading}\nbranch: main"
        ))]);
        // A different volatile tail must not change the fingerprint.
        let other_tail = stable_prefix_fingerprint(&[system_msg(&format!(
            "persona and project context\n\n{heading}\nbranch: feature; 3 dirty files"
        ))]);
        assert_eq!(
            base, other_tail,
            "the volatile tail must not perturb the hash"
        );
        assert_eq!(base.len(), 16);

        // A changed stable prefix (the churn signal) must change the fingerprint.
        let churned = stable_prefix_fingerprint(&[system_msg(&format!(
            "persona and DIFFERENT project context\n\n{heading}\nbranch: main"
        ))]);
        assert_ne!(
            base, churned,
            "a changed stable prefix must change the hash"
        );

        // No system prefix → empty fingerprint.
        assert!(stable_prefix_fingerprint(&[]).is_empty());
    }

    #[test]
    fn common_prefix_measurement_stops_at_first_wire_change() {
        assert_eq!(common_prefix_len(b"stable|old-tail", b"stable|new-tail"), 7);
        assert_eq!(common_prefix_len(b"same", b"same"), 4);
        assert_eq!(common_prefix_len(b"", b"prior"), 0);
    }

    #[test]
    fn formats_perf_report_with_optional_first_output() {
        let report = PerfReport {
            preflight: PreflightPerf {
                total_duration: Duration::from_millis(18),
                token_count_duration: Duration::from_millis(12),
                token_count_calls: 1,
            },
            provider: ProviderPerf {
                first_output_duration: Some(Duration::from_millis(640)),
                total_duration: Duration::from_millis(2_100),
                attempts: Vec::new(),
                generation_budget: GenerationBudget::for_context(128_000, None, None),
            },
            prompt: PromptPerf {
                prompt_tokens: 42_180,
                estimate_source: TokenCounterKind::Tiktoken,
                estimate_confidence: EstimateConfidence::High,
                request_body_bytes: Some(172_032),
                request_body_hash: Some("request123".to_string()),
                request_preview: None,
                tool_schema_tokens: 7_320,
                tool_schema_bytes: 31_744,
                tool_schema_hash: Some("schema123".to_string()),
                tool_names: vec!["read".to_string(), "bash".to_string()],
                tool_count: 14,
            },
            cache: CachePerf {
                cacheable_prefix_tokens: 18_900,
                volatile_tail_tokens: 23_280,
                cache_mechanism: Some("prompt_cache_key".to_string()),
                route_fingerprint: Some("route123".to_string()),
                local_reusable_prefix_tokens: None,
                local_reusable_prefix_percent: Some(84),
                prefix_hash: String::new(),
            },
        };

        let context = ContextReport::default();
        let text = format_perf_report(Some(&report), &context);

        assert!(text.contains("Performance: last model call"));
        assert!(text.contains("preflight        18 ms total | token count 12 ms (1 call)"));
        assert!(text.contains("provider         first output 640 ms | total 2.1 s"));
        assert!(text.contains("prompt           42,180 tok | 168.0 KiB request body"));
        assert!(text.contains("tool schema      7,320 tok | 31.0 KiB | 14 tools"));
        assert!(text.contains("estimator        tiktoken | high confidence"));
        assert!(text.contains("cache prefix     18,900 tok cacheable | 23,280 tok volatile"));
        assert!(text.contains("Usage: session"));
    }
}
