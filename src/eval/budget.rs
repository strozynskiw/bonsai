use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::agent::{ExecutionLaneKind, UsageTurnReport, UsageTurnStatus};

use super::{UsageReport, millis_u64};

/// Optional efficiency limits declared at suite or task scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EvalBudgets {
    pub max_logical_turns: Option<usize>,
    pub max_provider_attempts: Option<usize>,
    pub max_duration_ms: Option<u64>,
    pub max_prompt_tokens: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    pub max_uncached_prompt_tokens: Option<u64>,
    pub max_cost_micros: Option<u64>,
    pub max_completion_tokens_per_turn: Option<u64>,
    pub max_missing_usage_turns: Option<usize>,
    pub min_post_warmup_cache_percent: Option<u64>,
    pub cache_warmup_turns: Option<usize>,
    pub max_reads_per_unchanged_path: Option<usize>,
}

impl EvalBudgets {
    pub(crate) fn overlay(self, task: Self) -> Self {
        Self {
            max_logical_turns: task.max_logical_turns.or(self.max_logical_turns),
            max_provider_attempts: task.max_provider_attempts.or(self.max_provider_attempts),
            max_duration_ms: task.max_duration_ms.or(self.max_duration_ms),
            max_prompt_tokens: task.max_prompt_tokens.or(self.max_prompt_tokens),
            max_completion_tokens: task.max_completion_tokens.or(self.max_completion_tokens),
            max_uncached_prompt_tokens: task
                .max_uncached_prompt_tokens
                .or(self.max_uncached_prompt_tokens),
            max_cost_micros: task.max_cost_micros.or(self.max_cost_micros),
            max_completion_tokens_per_turn: task
                .max_completion_tokens_per_turn
                .or(self.max_completion_tokens_per_turn),
            max_missing_usage_turns: task
                .max_missing_usage_turns
                .or(self.max_missing_usage_turns),
            min_post_warmup_cache_percent: task
                .min_post_warmup_cache_percent
                .or(self.min_post_warmup_cache_percent),
            cache_warmup_turns: task.cache_warmup_turns.or(self.cache_warmup_turns),
            max_reads_per_unchanged_path: task
                .max_reads_per_unchanged_path
                .or(self.max_reads_per_unchanged_path),
        }
    }
}

/// Observed task metrics used by the budget evaluator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EvalMetricsReport {
    pub logical_turns: usize,
    pub provider_attempts: usize,
    pub duration_ms: u64,
    pub uncached_prompt_tokens: u64,
    pub max_completion_tokens_per_turn: u64,
    pub missing_usage_turns: usize,
    pub post_warmup_cache_percent: Option<u64>,
    pub max_reads_per_unchanged_path: usize,
    pub inspection_executed: usize,
    pub inspection_reused: usize,
    pub inspection_rejected: usize,
    pub inspection_returned_chars: usize,
    pub inspection_avoided_chars: usize,
    pub read_paths: Vec<EvalReadPathReport>,
}

/// Per-path read attempts and final admission outcomes. Raw argument shapes
/// make range variation visible in live-eval reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EvalReadPathReport {
    pub path: String,
    pub attempts: usize,
    pub executed: usize,
    pub reused: usize,
    pub rejected: usize,
    pub failed: usize,
    pub arguments: Vec<String>,
}

impl EvalMetricsReport {
    pub(crate) fn from_run(
        turns: &[UsageTurnReport],
        logical_turns: usize,
        provider_attempts: usize,
        elapsed: std::time::Duration,
        max_reads_per_unchanged_path: usize,
        read_paths: Vec<EvalReadPathReport>,
        warmup_turns: usize,
    ) -> Self {
        let uncached_prompt_tokens = turns
            .iter()
            .map(|turn| {
                turn.cache_measured_input_tokens
                    .or(turn.prompt_tokens)
                    .unwrap_or(0)
                    .saturating_sub(turn.cache_read_input_tokens.unwrap_or(0))
            })
            .sum();
        Self {
            logical_turns,
            provider_attempts,
            duration_ms: millis_u64(elapsed),
            uncached_prompt_tokens,
            max_completion_tokens_per_turn: turns
                .iter()
                .filter_map(|turn| turn.completion_tokens)
                .max()
                .unwrap_or(0),
            missing_usage_turns: turns
                .iter()
                .filter(|turn| turn.status == UsageTurnStatus::Missing)
                .count(),
            post_warmup_cache_percent: post_warmup_cache_percent(turns, warmup_turns),
            max_reads_per_unchanged_path,
            inspection_executed: turns.iter().map(|turn| turn.inspection_executed).sum(),
            inspection_reused: turns.iter().map(|turn| turn.inspection_reused).sum(),
            inspection_rejected: turns.iter().map(|turn| turn.inspection_rejected).sum(),
            inspection_returned_chars: turns
                .iter()
                .map(|turn| turn.inspection_returned_chars)
                .sum(),
            inspection_avoided_chars: turns.iter().map(|turn| turn.inspection_avoided_chars).sum(),
            read_paths,
        }
    }
}

/// Declared limits, observed metrics, and all violations for one task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EvalBudgetReport {
    pub declared: EvalBudgets,
    pub metrics: EvalMetricsReport,
    pub violations: Vec<String>,
}

impl EvalBudgetReport {
    pub(crate) fn evaluate(
        declared: EvalBudgets,
        metrics: EvalMetricsReport,
        usage: UsageReport,
    ) -> Self {
        let mut violations = Vec::new();
        check_max(
            &mut violations,
            "logical turns",
            metrics.logical_turns as u64,
            declared.max_logical_turns.map(|value| value as u64),
        );
        check_max(
            &mut violations,
            "provider attempts",
            metrics.provider_attempts as u64,
            declared.max_provider_attempts.map(|value| value as u64),
        );
        check_max(
            &mut violations,
            "duration (ms)",
            metrics.duration_ms,
            declared.max_duration_ms,
        );
        check_max(
            &mut violations,
            "prompt tokens",
            usage.prompt_tokens,
            declared.max_prompt_tokens,
        );
        check_max(
            &mut violations,
            "completion tokens",
            usage.completion_tokens,
            declared.max_completion_tokens,
        );
        check_max(
            &mut violations,
            "uncached prompt tokens",
            metrics.uncached_prompt_tokens,
            declared.max_uncached_prompt_tokens,
        );
        check_max(
            &mut violations,
            "completion tokens in one turn",
            metrics.max_completion_tokens_per_turn,
            declared.max_completion_tokens_per_turn,
        );
        check_max(
            &mut violations,
            "missing usage turns",
            metrics.missing_usage_turns as u64,
            declared.max_missing_usage_turns.map(|value| value as u64),
        );
        check_max(
            &mut violations,
            "reads per unchanged path",
            metrics.max_reads_per_unchanged_path as u64,
            declared
                .max_reads_per_unchanged_path
                .map(|value| value as u64),
        );

        if let Some(maximum) = declared.max_cost_micros {
            match usage.cost_micros {
                Some(actual) => check_max(&mut violations, "cost (micros)", actual, Some(maximum)),
                None => violations.push(format!(
                    "cost is unavailable, so maximum {maximum} micros cannot be verified"
                )),
            }
        }
        if let Some(minimum) = declared.min_post_warmup_cache_percent {
            match metrics.post_warmup_cache_percent {
                Some(actual) if actual < minimum => violations.push(format!(
                    "post-warmup cache was {actual}%, below minimum {minimum}%"
                )),
                Some(_) => {}
                None => violations.push(format!(
                    "post-warmup cache is unavailable, so minimum {minimum}% cannot be verified"
                )),
            }
        }
        Self {
            declared,
            metrics,
            violations,
        }
    }

    pub(crate) fn passed(&self) -> bool {
        self.violations.is_empty()
    }
}

fn check_max(violations: &mut Vec<String>, label: &str, actual: u64, maximum: Option<u64>) {
    if let Some(maximum) = maximum
        && actual > maximum
    {
        violations.push(format!("{label} was {actual}, above maximum {maximum}"));
    }
}

fn post_warmup_cache_percent(turns: &[UsageTurnReport], warmup_turns: usize) -> Option<u64> {
    let mut lane_seen = HashMap::<(ExecutionLaneKind, &str), usize>::new();
    let mut read_tokens = 0u64;
    let mut measured_tokens = 0u64;
    for turn in turns {
        let seen = lane_seen
            .entry((turn.lane_kind, turn.lane_id.as_str()))
            .or_default();
        *seen += 1;
        if *seen <= warmup_turns {
            continue;
        }
        let Some(measured) = turn.cache_measured_input_tokens else {
            continue;
        };
        read_tokens = read_tokens.saturating_add(turn.cache_read_input_tokens.unwrap_or(0));
        measured_tokens = measured_tokens.saturating_add(measured);
    }
    read_tokens.saturating_mul(100).checked_div(measured_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_budgets_overlay_suite_defaults() {
        let suite = EvalBudgets {
            max_logical_turns: Some(10),
            max_prompt_tokens: Some(1_000),
            ..EvalBudgets::default()
        };
        let task = EvalBudgets {
            max_logical_turns: Some(4),
            ..EvalBudgets::default()
        };

        let merged = suite.overlay(task);

        assert_eq!(merged.max_logical_turns, Some(4));
        assert_eq!(merged.max_prompt_tokens, Some(1_000));
    }

    #[test]
    fn budget_report_collects_every_violation() {
        let declared = EvalBudgets {
            max_logical_turns: Some(2),
            max_prompt_tokens: Some(50),
            max_cost_micros: Some(10),
            ..EvalBudgets::default()
        };
        let metrics = EvalMetricsReport {
            logical_turns: 3,
            provider_attempts: 3,
            duration_ms: 1,
            uncached_prompt_tokens: 60,
            max_completion_tokens_per_turn: 5,
            missing_usage_turns: 0,
            post_warmup_cache_percent: None,
            max_reads_per_unchanged_path: 1,
            inspection_executed: 0,
            inspection_reused: 0,
            inspection_rejected: 0,
            inspection_returned_chars: 0,
            inspection_avoided_chars: 0,
            read_paths: Vec::new(),
        };
        let usage = UsageReport {
            prompt_tokens: 60,
            completion_tokens: 5,
            total_tokens: 65,
            cost_micros: None,
        };

        let report = EvalBudgetReport::evaluate(declared, metrics, usage);

        assert_eq!(report.violations.len(), 3);
    }
}
