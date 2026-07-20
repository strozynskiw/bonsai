use serde_json::Value;

use crate::provider::{InputCacheUsage, TokenUsage, token_count_u32};

pub(crate) fn chat_completions_usage_from_value(value: &Value) -> TokenUsage {
    let prompt_tokens = value
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let input_cache = openai_input_cache_usage(value.get("prompt_tokens_details"), prompt_tokens)
        .or_else(|| {
            value
                .get("cached_tokens")
                .and_then(Value::as_u64)
                .map(|read_tokens| InputCacheUsage::new(read_tokens, 0, prompt_tokens))
        })
        // DeepSeek's direct API reports automatic prefix-cache reuse as a
        // top-level `prompt_cache_hit_tokens` (its complement is
        // `prompt_cache_miss_tokens`), NOT the standard
        // `prompt_tokens_details.cached_tokens`. Without this fallback DeepSeek
        // reads as 0% cache reuse and every input token is billed at the
        // 50x-more-expensive miss rate. Automatic caching has no explicit write
        // charge, so write_tokens stays 0.
        .or_else(|| {
            value
                .get("prompt_cache_hit_tokens")
                .and_then(Value::as_u64)
                .map(|read_tokens| InputCacheUsage::new(read_tokens, 0, prompt_tokens))
        });
    TokenUsage {
        prompt_tokens: token_count_u32(Some(prompt_tokens)),
        completion_tokens: token_count_u32(value.get("completion_tokens").and_then(Value::as_u64)),
        input_cache,
    }
}

pub(crate) fn responses_usage_from_value(value: &Value) -> TokenUsage {
    let input_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let input_cache = openai_input_cache_usage(value.get("input_tokens_details"), input_tokens);
    TokenUsage {
        prompt_tokens: token_count_u32(Some(input_tokens)),
        completion_tokens: token_count_u32(value.get("output_tokens").and_then(Value::as_u64)),
        input_cache,
    }
}

fn openai_input_cache_usage(
    details: Option<&Value>,
    total_input_tokens: u64,
) -> Option<InputCacheUsage> {
    let details = details?;
    let read_tokens = details.get("cached_tokens").and_then(Value::as_u64);
    let write_tokens = details.get("cache_write_tokens").and_then(Value::as_u64);
    (read_tokens.is_some() || write_tokens.is_some()).then(|| {
        InputCacheUsage::new(
            read_tokens.unwrap_or(0),
            write_tokens.unwrap_or(0),
            total_input_tokens,
        )
    })
}

/// Accumulates token usage across Anthropic stream events: `input_tokens`
/// arrives in `message_start`, `output_tokens` accrues in `message_delta`.
#[derive(Debug, Default)]
pub(crate) struct AnthropicUsageAccumulator {
    input: Option<u32>,
    output: Option<u32>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
}

impl AnthropicUsageAccumulator {
    pub(crate) fn set_input_tokens(&mut self, value: u64) {
        self.input = Some(token_count_u32(Some(value)));
    }

    pub(crate) fn set_output_tokens(&mut self, value: u64) {
        self.output = Some(token_count_u32(Some(value)));
    }

    pub(crate) fn set_cache_read_tokens(&mut self, value: u64) {
        self.cache_read = Some(value);
    }

    pub(crate) fn set_cache_creation_tokens(&mut self, value: u64) {
        self.cache_creation = Some(value);
    }

    /// Apply every usage field present on one stream event's `usage` object,
    /// present-fields-win. Called from both `message_start` and
    /// `message_delta`: real Anthropic sends input/cache on start and
    /// cumulative output on delta, while some Anthropic-compatible providers
    /// (e.g. MiniMax) send zeros on start and the final breakdown on delta.
    /// One shared parse guarantees a new field class (say, another cache tier)
    /// is picked up by both events at once instead of under-counting on the
    /// providers that report it on the other event.
    pub(crate) fn apply_event_usage(&mut self, usage: &Value) {
        if let Some(value) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.set_input_tokens(value);
        }
        if let Some(value) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.set_output_tokens(value);
        }
        if let Some(value) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.set_cache_read_tokens(value);
        }
        if let Some(value) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.set_cache_creation_tokens(value);
        }
    }

    pub(crate) fn into_usage(self) -> Option<TokenUsage> {
        (self.input.is_some()
            || self.output.is_some()
            || self.cache_read.is_some()
            || self.cache_creation.is_some())
        .then_some({
            let cache_read = self.cache_read.unwrap_or(0);
            let cache_creation = self.cache_creation.unwrap_or(0);
            // Anthropic reports `input_tokens` as the *non-cached* input only.
            // `prompt_tokens` follows the crate-wide convention of being the
            // full input including cache (see `TokenUsage`), so fold the cache
            // counts in; the per-class breakdown stays in `input_cache`.
            let non_cached_input = u64::from(self.input.unwrap_or(0));
            let total_input = non_cached_input
                .saturating_add(cache_read)
                .saturating_add(cache_creation);
            let input_cache = (self.cache_read.is_some() || self.cache_creation.is_some())
                .then(|| InputCacheUsage::new(cache_read, cache_creation, total_input));
            TokenUsage {
                prompt_tokens: token_count_u32(Some(total_input)),
                completion_tokens: self.output.unwrap_or(0),
                input_cache,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_usage_captures_input_cache_tokens() {
        let usage = chat_completions_usage_from_value(&serde_json::json!({
            "prompt_tokens": 8_000,
            "completion_tokens": 250,
            "prompt_tokens_details": {
                "cached_tokens": 2_000,
                "cache_write_tokens": 750
            }
        }));

        assert_eq!(usage.prompt_tokens, 8_000);
        assert_eq!(usage.completion_tokens, 250);
        assert_eq!(
            usage.input_cache,
            Some(InputCacheUsage::new(2_000, 750, 8_000))
        );
    }

    #[test]
    fn chat_usage_without_cache_tokens_leaves_cache_unknown() {
        let usage = chat_completions_usage_from_value(&serde_json::json!({
            "prompt_tokens": 8_000,
            "completion_tokens": 250
        }));

        assert_eq!(usage.input_cache, None);
    }

    #[test]
    fn chat_usage_captures_kimi_top_level_cached_tokens() {
        let usage = chat_completions_usage_from_value(&serde_json::json!({
            "prompt_tokens": 8_000,
            "completion_tokens": 250,
            "cached_tokens": 2_000
        }));

        assert_eq!(
            usage.input_cache,
            Some(InputCacheUsage::new(2_000, 0, 8_000))
        );
    }

    #[test]
    fn chat_usage_captures_deepseek_prompt_cache_hit_tokens() {
        // DeepSeek direct API: top-level prompt_cache_hit_tokens (+ its
        // complement prompt_cache_miss_tokens), no prompt_tokens_details.
        let usage = chat_completions_usage_from_value(&serde_json::json!({
            "prompt_tokens": 10_000,
            "completion_tokens": 200,
            "prompt_cache_hit_tokens": 9_000,
            "prompt_cache_miss_tokens": 1_000
        }));

        assert_eq!(
            usage.input_cache,
            Some(InputCacheUsage::new(9_000, 0, 10_000))
        );
    }

    #[test]
    fn responses_usage_captures_input_cache_tokens() {
        let usage = responses_usage_from_value(&serde_json::json!({
            "input_tokens": 10_000,
            "output_tokens": 500,
            "input_tokens_details": {
                "cached_tokens": 2_500,
                "cache_write_tokens": 1_250
            }
        }));

        assert_eq!(usage.prompt_tokens, 10_000);
        assert_eq!(usage.completion_tokens, 500);
        assert_eq!(
            usage.input_cache,
            Some(InputCacheUsage::new(2_500, 1_250, 10_000))
        );
    }

    #[test]
    fn responses_usage_without_cache_tokens_leaves_cache_unknown() {
        let usage = responses_usage_from_value(&serde_json::json!({
            "input_tokens": 10_000,
            "output_tokens": 500
        }));

        assert_eq!(usage.input_cache, None);
    }

    #[test]
    fn anthropic_usage_captures_cache_tokens() {
        let mut usage = AnthropicUsageAccumulator::default();
        usage.set_input_tokens(42);
        usage.set_output_tokens(99);
        usage.set_cache_read_tokens(30);
        usage.set_cache_creation_tokens(10);

        let usage = usage.into_usage().expect("usage should be captured");
        // prompt_tokens is cache-inclusive (42 fresh + 30 read + 10 creation).
        assert_eq!(usage.prompt_tokens, 82);
        assert_eq!(usage.completion_tokens, 99);
        assert_eq!(usage.input_cache, Some(InputCacheUsage::new(30, 10, 82)));
    }

    #[test]
    fn anthropic_usage_without_cache_fields_leaves_cache_unknown() {
        let mut usage = AnthropicUsageAccumulator::default();
        usage.set_input_tokens(42);
        usage.set_output_tokens(1);

        let usage = usage.into_usage().expect("usage should be captured");
        assert_eq!(usage.input_cache, None);
    }
}
