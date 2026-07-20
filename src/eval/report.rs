use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::agent::UsageTotals;

use super::*;

/// Top-level eval report serialized to `report.json` (and summarized to
/// `summary.md`).
#[derive(Debug, Serialize)]
pub(crate) struct EvalReport {
    pub(crate) run_id: String,
    pub(crate) suite: SuiteReport,
    pub(crate) mode: EvalMode,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) reasoning: String,
    pub(crate) seed: u64,
    pub(crate) score: ScoreReport,
    pub(crate) tasks: Vec<TaskReport>,
    pub(crate) usage: UsageReport,
    pub(crate) cost_micros: Option<u64>,
    pub(crate) tokens_per_dollar: Option<f64>,
    pub(crate) duration_ms: u64,
    pub(crate) cache_reuse_percent: Option<u64>,
    pub(crate) repair_turns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) baseline: Option<EvalBaselineComparison>,
    pub(crate) output_dir: String,
}

/// Identifying metadata for the suite a report was produced from.
#[derive(Debug, Serialize)]
pub(crate) struct SuiteReport {
    pub(crate) id: String,
    pub(crate) path: String,
}

/// Terminal state of a single task run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TaskStatus {
    /// The agent finished its run normally.
    #[default]
    Completed,
    /// The run was interrupted (e.g. cancellation).
    Interrupted,
    /// The run aborted with an error.
    Error,
}

impl TaskStatus {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Error => "error",
        }
    }
}

/// Per-task entry in an [`EvalReport`].
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TaskReport {
    pub(crate) id: String,
    pub(crate) prompt: String,
    pub(crate) status: TaskStatus,
    pub(crate) expected_status: TaskStatus,
    pub(crate) passed: bool,
    pub(crate) score: ScoreReport,
    pub(crate) output: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) run_error: Option<String>,
    pub(crate) usage: UsageReport,
    pub(crate) usage_turns: Vec<crate::agent::UsageTurnReport>,
    pub(crate) budget: EvalBudgetReport,
    pub(crate) cost_micros: Option<u64>,
    pub(crate) tokens_per_dollar: Option<f64>,
    pub(crate) duration_ms: u64,
    pub(crate) repair_turns: u64,
    pub(crate) compactions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) min_compactions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_compactions: Option<usize>,
    pub(crate) episode_evictions: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) min_episode_evictions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) shared_workspace: Option<SharedWorkspaceReport>,
    pub(crate) worktree_path: String,
    pub(crate) graders: Vec<GraderResult>,
}

/// Cross-session mutation attribution observed for a shared-workspace task.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SharedWorkspaceReport {
    pub(crate) primary_session_id: i64,
    pub(crate) peer_session_id: i64,
    pub(crate) primary_changed_files: Vec<String>,
    pub(crate) peer_changed_files: Vec<String>,
    pub(crate) expected_primary_changed_files: Vec<String>,
    pub(crate) expected_peer_changed_files: Vec<String>,
    pub(crate) passed: bool,
}

/// Passed/total tally with a precomputed percentage.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct ScoreReport {
    pub(crate) passed: usize,
    pub(crate) total: usize,
    pub(crate) percent: f64,
}

impl ScoreReport {
    /// Build a score from a `passed`/`total` tally, computing the percentage
    /// (zero when `total` is zero).
    pub(crate) fn new(passed: usize, total: usize) -> Self {
        let percent = if total == 0 {
            0.0
        } else {
            (passed as f64 / total as f64) * 100.0
        };
        Self {
            passed,
            total,
            percent,
        }
    }

    /// Aggregate suite-level score from per-task pass/fail flags.
    pub(crate) fn from_tasks(tasks: &[TaskReport]) -> Self {
        Self::new(tasks.iter().filter(|task| task.passed).count(), tasks.len())
    }

    /// Aggregate per-task score from individual grader results.
    pub(crate) fn from_grader_results(graders: &[GraderResult]) -> Self {
        Self::new(
            graders.iter().filter(|grader| grader.passed).count(),
            graders.len(),
        )
    }
}

/// Token and cost totals for a task or whole run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct UsageReport {
    pub(crate) prompt_tokens: u64,
    pub(crate) completion_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cost_micros: Option<u64>,
}

impl UsageReport {
    /// Build a usage report from an agent's accumulated [`UsageTotals`].
    pub(crate) fn from_totals(totals: UsageTotals) -> Self {
        let total_tokens = totals
            .prompt_tokens
            .saturating_add(totals.completion_tokens);
        Self {
            prompt_tokens: totals.prompt_tokens,
            completion_tokens: totals.completion_tokens,
            total_tokens,
            cost_micros: totals.cost_micros,
        }
    }

    /// Sum per-task usage into a run total; cost is `None` if any task lacks it.
    pub(crate) fn sum(usages: impl Iterator<Item = UsageReport>) -> Self {
        let mut prompt_tokens = 0u64;
        let mut completion_tokens = 0u64;
        let mut cost_micros = Some(0u64);
        for usage in usages {
            prompt_tokens = prompt_tokens.saturating_add(usage.prompt_tokens);
            completion_tokens = completion_tokens.saturating_add(usage.completion_tokens);
            cost_micros = match (cost_micros, usage.cost_micros) {
                (Some(total), Some(cost)) => Some(total.saturating_add(cost)),
                _ => None,
            };
        }
        let total_tokens = prompt_tokens.saturating_add(completion_tokens);
        Self {
            prompt_tokens,
            completion_tokens,
            total_tokens,
            cost_micros,
        }
    }
}

/// Tokens-per-dollar for a usage report, or `None` when cost is absent/zero.
pub(crate) fn tokens_per_dollar(usage: UsageReport) -> Option<f64> {
    let cost_micros = usage.cost_micros?;
    if cost_micros == 0 {
        return None;
    }
    Some((usage.total_tokens as f64) * 1_000_000.0 / cost_micros as f64)
}

/// Write `body` to `path`, attaching a `label` (e.g. `report`/`summary`) to any
/// I/O error. Unifies the former `write_json_file` / `write_summary_file` pair.
///
/// # Errors
/// Returns an error if the file cannot be written.
pub(crate) fn write_file(path: &Path, body: &str, label: &str) -> Result<()> {
    fs::write(path, body).with_context(|| format!("Failed to write eval {label} {:?}", path))
}

/// Render the Markdown `summary.md` body for a report.
pub(crate) fn format_eval_summary(report: &EvalReport) -> String {
    let mut summary = String::new();
    summary.push_str(&format!("# Eval {}\n\n", report.suite.id));
    summary.push_str(&format!("- Run: `{}`\n", report.run_id));
    summary.push_str(&format!("- Mode: `{}`\n", report.mode));
    summary.push_str(&format!("- Provider: `{}`\n", report.provider));
    summary.push_str(&format!("- Model: `{}`\n", report.model));
    summary.push_str(&format!("- Reasoning: `{}`\n", report.reasoning));
    summary.push_str(&format!("- Seed: `{}`\n", report.seed));
    summary.push_str(&format!(
        "- Score: {}/{} ({:.1}%)\n",
        report.score.passed, report.score.total, report.score.percent
    ));
    summary.push_str(&format!(
        "- Tokens: {} total ({} prompt, {} completion)\n",
        report.usage.total_tokens, report.usage.prompt_tokens, report.usage.completion_tokens
    ));
    summary.push_str(&format!("- Cost: {}\n", format_cost(report.cost_micros)));
    summary.push_str(&format!(
        "- Tokens/$: {}\n",
        format_tokens_per_dollar(report.tokens_per_dollar)
    ));
    summary.push_str(&format!("- Duration: {} ms\n", report.duration_ms));
    summary.push_str(&format!(
        "- Cache reuse: {}\n",
        report
            .cache_reuse_percent
            .map(|percent| format!("{percent}%"))
            .unwrap_or_else(|| "n/a".to_string())
    ));
    summary.push_str(&format!("- Repair turns: {}\n", report.repair_turns));
    if let Some(baseline) = &report.baseline {
        summary.push_str(&format!(
            "- Baseline: {} (score floor {:.1}%, delta {:+.1} points; tokens {:+}, cost {}, duration {:+} ms, cache {}, repairs {:+})\n",
            if baseline.passed { "pass" } else { "fail" },
            baseline.minimum_score_percent,
            baseline.deltas.score_percent,
            baseline.deltas.total_tokens,
            baseline
                .deltas
                .cost_micros
                .map(|delta| format!("{delta:+} micros"))
                .unwrap_or_else(|| "n/a".to_string()),
            baseline.deltas.duration_ms,
            baseline
                .deltas
                .cache_reuse_percent
                .map(|delta| format!("{delta:+} points"))
                .unwrap_or_else(|| "n/a".to_string()),
            baseline.deltas.repair_turns,
        ));
        for violation in &baseline.violations {
            summary.push_str(&format!("  - Regression: {violation}\n"));
        }
    }
    summary.push('\n');
    summary.push_str("| Task | Result | Status | Score | Budget | Tokens | Cost | Tokens/$ |\n");
    summary.push_str("| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |\n");
    for task in &report.tasks {
        let status = if task.status == task.expected_status {
            task.status.label().to_string()
        } else {
            format!(
                "{} (expected {})",
                task.status.label(),
                task.expected_status.label()
            )
        };
        summary.push_str(&format!(
            "| `{}` | {} | {} | {}/{} | {} | {} | {} | {} |\n",
            task.id,
            if task.passed { "pass" } else { "fail" },
            status,
            task.score.passed,
            task.score.total,
            if task.budget.passed() { "pass" } else { "fail" },
            task.usage.total_tokens,
            format_cost(task.cost_micros),
            format_tokens_per_dollar(task.tokens_per_dollar)
        ));
    }
    summary
}

fn format_cost(cost_micros: Option<u64>) -> String {
    let Some(cost_micros) = cost_micros else {
        return "n/a".to_string();
    };
    let dollars = cost_micros / 1_000_000;
    let micros = cost_micros % 1_000_000;
    format!("${dollars}.{micros:06}")
}

fn format_tokens_per_dollar(tokens_per_dollar: Option<f64>) -> String {
    tokens_per_dollar
        .map(|tokens_per_dollar| format!("{tokens_per_dollar:.0}"))
        .unwrap_or_else(|| "n/a".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_report(id: &str, passed: bool) -> TaskReport {
        let metrics = EvalMetricsReport {
            logical_turns: 1,
            provider_attempts: 1,
            duration_ms: 1,
            uncached_prompt_tokens: 1,
            max_completion_tokens_per_turn: 1,
            missing_usage_turns: 0,
            post_warmup_cache_percent: None,
            max_reads_per_unchanged_path: 0,
            inspection_executed: 0,
            inspection_reused: 0,
            inspection_rejected: 0,
            inspection_returned_chars: 0,
            inspection_avoided_chars: 0,
            read_paths: Vec::new(),
        };
        TaskReport {
            id: id.to_string(),
            prompt: String::new(),
            status: TaskStatus::Completed,
            expected_status: TaskStatus::Completed,
            passed,
            score: ScoreReport::new(1, 1),
            output: String::new(),
            run_error: None,
            usage: UsageReport {
                prompt_tokens: 1,
                completion_tokens: 1,
                total_tokens: 2,
                cost_micros: Some(1),
            },
            usage_turns: Vec::new(),
            budget: EvalBudgetReport::evaluate(
                EvalBudgets::default(),
                metrics,
                UsageReport {
                    prompt_tokens: 1,
                    completion_tokens: 1,
                    total_tokens: 2,
                    cost_micros: Some(1),
                },
            ),
            cost_micros: Some(1),
            tokens_per_dollar: Some(2_000_000.0),
            duration_ms: 1,
            repair_turns: 0,
            compactions: 0,
            min_compactions: None,
            max_compactions: None,
            episode_evictions: 0,
            min_episode_evictions: None,
            shared_workspace: None,
            worktree_path: String::new(),
            graders: Vec::new(),
        }
    }

    #[test]
    fn score_aggregation_counts_passing_tasks() {
        let tasks = vec![
            task_report("a", true),
            task_report("b", false),
            task_report("c", true),
        ];

        assert_eq!(ScoreReport::from_tasks(&tasks), ScoreReport::new(2, 3));
    }

    #[test]
    fn eval_summary_includes_score_tokens_and_na_cost() {
        let mut task = task_report("t01", true);
        task.usage = UsageReport {
            prompt_tokens: 7,
            completion_tokens: 3,
            total_tokens: 10,
            cost_micros: None,
        };
        task.cost_micros = None;
        task.tokens_per_dollar = None;
        let report = EvalReport {
            run_id: "run-1".to_string(),
            suite: SuiteReport {
                id: "suite".to_string(),
                path: "suite.toml".to_string(),
            },
            mode: EvalMode::Mock,
            provider: MOCK_PROVIDER_ID.to_string(),
            model: MOCK_MODEL.to_string(),
            reasoning: "default".to_string(),
            seed: 1,
            score: ScoreReport::new(1, 1),
            tasks: vec![task],
            usage: UsageReport {
                prompt_tokens: 70,
                completion_tokens: 30,
                total_tokens: 100,
                cost_micros: None,
            },
            cost_micros: None,
            tokens_per_dollar: None,
            duration_ms: 12,
            cache_reuse_percent: None,
            repair_turns: 0,
            baseline: None,
            output_dir: "target/eval/run-1".to_string(),
        };

        let summary = format_eval_summary(&report);

        assert!(summary.contains("- Score: 1/1 (100.0%)"));
        assert!(summary.contains("- Tokens: 100 total (70 prompt, 30 completion)"));
        assert!(summary.contains("- Cost: n/a"));
        assert!(summary.contains("- Cache reuse: n/a"));
        assert!(summary.contains("- Repair turns: 0"));
        assert!(summary.contains("| `t01` | pass | completed | 1/1 | pass | 10 | n/a | n/a |"));
    }
}
