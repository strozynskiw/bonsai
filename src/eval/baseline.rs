use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{EvalReport, TaskReport};

const BASELINE_SCHEMA_VERSION: u32 = 1;

/// Metrics captured for one eval run or stored as its comparison reference.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct EvalBaselineMetrics {
    pub(crate) score_percent: f64,
    pub(crate) total_tokens: u64,
    pub(crate) cost_micros: Option<u64>,
    pub(crate) duration_ms: u64,
    pub(crate) cache_reuse_percent: Option<u64>,
    pub(crate) repair_turns: u64,
}

impl EvalBaselineMetrics {
    pub(crate) fn from_report(report: &EvalReport) -> Self {
        Self {
            score_percent: report.score.percent,
            total_tokens: report.usage.total_tokens,
            cost_micros: report.cost_micros,
            duration_ms: report.duration_ms,
            cache_reuse_percent: report.cache_reuse_percent,
            repair_turns: report.repair_turns,
        }
    }
}

/// Signed changes from the stored reference; positive values mean the run used
/// more of a metric (or scored higher).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct EvalBaselineDeltas {
    pub(crate) score_percent: f64,
    pub(crate) total_tokens: i64,
    pub(crate) cost_micros: Option<i64>,
    pub(crate) duration_ms: i64,
    pub(crate) cache_reuse_percent: Option<i64>,
    pub(crate) repair_turns: i64,
}

/// Exact profile identity selected from a baseline file.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub(crate) struct EvalBaselineProfileKey {
    pub(crate) suite: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) effort: String,
}

/// Regression tolerances applied to one exact baseline profile.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct EvalBaselinePolicy {
    pub(crate) allowed_score_drop_percent: f64,
    pub(crate) allowed_total_token_growth_percent: Option<u64>,
    pub(crate) allowed_cost_growth_percent: Option<u64>,
    pub(crate) allowed_duration_growth_percent: Option<u64>,
    pub(crate) allowed_cache_reuse_drop_points: Option<u64>,
    pub(crate) allowed_repair_turn_increase: Option<u64>,
}

/// Baseline comparison embedded beside the run score and efficiency metrics.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct EvalBaselineComparison {
    pub(crate) source: String,
    pub(crate) schema_version: u32,
    pub(crate) profile: EvalBaselineProfileKey,
    pub(crate) reference: EvalBaselineMetrics,
    pub(crate) actual: EvalBaselineMetrics,
    pub(crate) deltas: EvalBaselineDeltas,
    pub(crate) policy: EvalBaselinePolicy,
    pub(crate) minimum_score_percent: f64,
    pub(crate) passed: bool,
    pub(crate) violations: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalBaselineFile {
    schema_version: u32,
    profiles: Vec<EvalBaselineProfile>,
}

/// A parsed and validated baseline kept in memory for preflight selection and
/// post-run comparison.
#[derive(Debug)]
pub(crate) struct LoadedEvalBaseline {
    source: String,
    file: EvalBaselineFile,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvalBaselineProfile {
    suite: String,
    provider: String,
    model: String,
    effort: String,
    score_percent: f64,
    #[serde(default)]
    allowed_score_drop_percent: f64,
    allowed_total_token_growth_percent: Option<u64>,
    allowed_cost_growth_percent: Option<u64>,
    allowed_duration_growth_percent: Option<u64>,
    allowed_cache_reuse_drop_points: Option<u64>,
    allowed_repair_turn_increase: Option<u64>,
    total_tokens: u64,
    cost_micros: Option<u64>,
    duration_ms: u64,
    cache_reuse_percent: Option<u64>,
    repair_turns: u64,
}

impl EvalBaselineProfile {
    fn key(&self) -> EvalBaselineProfileKey {
        EvalBaselineProfileKey {
            suite: self.suite.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
        }
    }

    fn metrics(&self) -> EvalBaselineMetrics {
        EvalBaselineMetrics {
            score_percent: self.score_percent,
            total_tokens: self.total_tokens,
            cost_micros: self.cost_micros,
            duration_ms: self.duration_ms,
            cache_reuse_percent: self.cache_reuse_percent,
            repair_turns: self.repair_turns,
        }
    }

    fn policy(&self) -> EvalBaselinePolicy {
        EvalBaselinePolicy {
            allowed_score_drop_percent: self.allowed_score_drop_percent,
            allowed_total_token_growth_percent: self.allowed_total_token_growth_percent,
            allowed_cost_growth_percent: self.allowed_cost_growth_percent,
            allowed_duration_growth_percent: self.allowed_duration_growth_percent,
            allowed_cache_reuse_drop_points: self.allowed_cache_reuse_drop_points,
            allowed_repair_turn_increase: self.allowed_repair_turn_increase,
        }
    }
}

/// Load and validate a versioned eval baseline.
///
/// # Errors
/// Returns an error for unreadable or invalid baseline files.
pub(crate) fn load_eval_baseline(path: &Path) -> Result<LoadedEvalBaseline> {
    let body = fs::read_to_string(path)
        .with_context(|| format!("Failed to read eval baseline {:?}", path))?;
    let file: EvalBaselineFile = toml::from_str(&body)
        .with_context(|| format!("Failed to parse eval baseline {:?}", path))?;
    validate_baseline(&file, path)?;
    Ok(LoadedEvalBaseline {
        source: path.display().to_string(),
        file,
    })
}

impl LoadedEvalBaseline {
    /// Require an exact profile before the potentially expensive eval starts.
    ///
    /// # Errors
    /// Returns an error when no profile matches the full run identity.
    pub(crate) fn require_profile(
        &self,
        suite: &str,
        provider: &str,
        model: &str,
        effort: &str,
    ) -> Result<()> {
        self.profile(&EvalBaselineProfileKey {
            suite: suite.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            effort: effort.to_string(),
        })?;
        Ok(())
    }

    /// Compare a completed report with its exact profile.
    ///
    /// # Errors
    /// Returns an error when no profile matches the report identity.
    pub(crate) fn compare(&self, report: &EvalReport) -> Result<EvalBaselineComparison> {
        let actual_key = EvalBaselineProfileKey {
            suite: report.suite.id.clone(),
            provider: report.provider.clone(),
            model: report.model.clone(),
            effort: report.reasoning.clone(),
        };
        let profile = self.profile(&actual_key)?;
        Ok(self.compare_profile(actual_key, profile, report))
    }

    fn profile(&self, key: &EvalBaselineProfileKey) -> Result<&EvalBaselineProfile> {
        self.file
            .profiles
            .iter()
            .find(|profile| profile.key() == *key)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Eval baseline '{}' has no profile for suite='{}', provider='{}', model='{}', effort='{}'",
                    self.source,
                    key.suite,
                    key.provider,
                    key.model,
                    key.effort
                )
            })
    }

    fn compare_profile(
        &self,
        actual_key: EvalBaselineProfileKey,
        profile: &EvalBaselineProfile,
        report: &EvalReport,
    ) -> EvalBaselineComparison {
        let reference = profile.metrics();
        let actual = EvalBaselineMetrics::from_report(report);
        let policy = profile.policy();
        let minimum_score_percent =
            (reference.score_percent - policy.allowed_score_drop_percent).max(0.0);
        let mut violations = Vec::new();
        if actual.score_percent < minimum_score_percent {
            violations.push(format!(
                "score was {:.1}%, below baseline floor {:.1}% (reference {:.1}%, allowed drop {:.1} points)",
                actual.score_percent,
                minimum_score_percent,
                reference.score_percent,
                policy.allowed_score_drop_percent
            ));
        }
        if let Some(allowed_growth) = policy.allowed_total_token_growth_percent {
            push_growth_violation(
                "total tokens",
                actual.total_tokens,
                reference.total_tokens,
                allowed_growth,
                &mut violations,
            );
        }
        if let Some(allowed_growth) = policy.allowed_cost_growth_percent {
            push_optional_growth_violation(
                "cost",
                actual.cost_micros,
                reference.cost_micros,
                allowed_growth,
                &mut violations,
            );
        }
        if let Some(allowed_growth) = policy.allowed_duration_growth_percent {
            push_growth_violation(
                "duration",
                actual.duration_ms,
                reference.duration_ms,
                allowed_growth,
                &mut violations,
            );
        }
        if let Some(allowed_drop) = policy.allowed_cache_reuse_drop_points {
            push_cache_reuse_violation(
                actual.cache_reuse_percent,
                reference.cache_reuse_percent,
                allowed_drop,
                &mut violations,
            );
        }
        if let Some(allowed_increase) = policy.allowed_repair_turn_increase {
            let increase = actual.repair_turns.saturating_sub(reference.repair_turns);
            if increase > allowed_increase {
                violations.push(format!(
                    "repair turns increased from {} to {}, exceeding the allowed increase of {}",
                    reference.repair_turns, actual.repair_turns, allowed_increase
                ));
            }
        }
        let deltas = EvalBaselineDeltas {
            score_percent: actual.score_percent - reference.score_percent,
            total_tokens: signed_delta(actual.total_tokens, reference.total_tokens),
            cost_micros: optional_delta(actual.cost_micros, reference.cost_micros),
            duration_ms: signed_delta(actual.duration_ms, reference.duration_ms),
            cache_reuse_percent: optional_delta(
                actual.cache_reuse_percent,
                reference.cache_reuse_percent,
            ),
            repair_turns: signed_delta(actual.repair_turns, reference.repair_turns),
        };

        EvalBaselineComparison {
            source: self.source.clone(),
            schema_version: self.file.schema_version,
            profile: actual_key,
            reference,
            actual,
            deltas,
            policy,
            minimum_score_percent,
            passed: violations.is_empty(),
            violations,
        }
    }
}

fn validate_baseline(baseline: &EvalBaselineFile, path: &Path) -> Result<()> {
    if baseline.schema_version != BASELINE_SCHEMA_VERSION {
        anyhow::bail!(
            "Eval baseline {:?} uses schema version {}; supported version is {}",
            path,
            baseline.schema_version,
            BASELINE_SCHEMA_VERSION
        );
    }
    if baseline.profiles.is_empty() {
        anyhow::bail!("Eval baseline {:?} must contain at least one profile", path);
    }

    let mut keys = HashSet::new();
    for (index, profile) in baseline.profiles.iter().enumerate() {
        let profile_number = index + 1;
        for (name, value) in [
            ("suite", profile.suite.as_str()),
            ("provider", profile.provider.as_str()),
            ("model", profile.model.as_str()),
            ("effort", profile.effort.as_str()),
        ] {
            if value.trim().is_empty() {
                anyhow::bail!(
                    "Eval baseline {:?} profile {profile_number} has an empty {name}",
                    path
                );
            }
        }
        validate_percent(path, profile_number, "score_percent", profile.score_percent)?;
        validate_percent(
            path,
            profile_number,
            "allowed_score_drop_percent",
            profile.allowed_score_drop_percent,
        )?;
        for (name, value) in [
            (
                "allowed_total_token_growth_percent",
                profile.allowed_total_token_growth_percent,
            ),
            (
                "allowed_cost_growth_percent",
                profile.allowed_cost_growth_percent,
            ),
            (
                "allowed_duration_growth_percent",
                profile.allowed_duration_growth_percent,
            ),
            (
                "allowed_cache_reuse_drop_points",
                profile.allowed_cache_reuse_drop_points,
            ),
        ] {
            if value.is_some_and(|percent| percent > 100) {
                anyhow::bail!(
                    "Eval baseline {:?} profile {profile_number} {name} must be between 0 and 100",
                    path
                );
            }
        }
        if profile
            .cache_reuse_percent
            .is_some_and(|value| value > 1000)
        {
            anyhow::bail!(
                "Eval baseline {:?} profile {profile_number} cache_reuse_percent must be between 0 and 1000",
                path
            );
        }
        if profile.allowed_cost_growth_percent.is_some() && profile.cost_micros.is_none() {
            anyhow::bail!(
                "Eval baseline {:?} profile {profile_number} configures a cost gate without a cost_micros reference",
                path
            );
        }
        if profile.allowed_cache_reuse_drop_points.is_some()
            && profile.cache_reuse_percent.is_none()
        {
            anyhow::bail!(
                "Eval baseline {:?} profile {profile_number} configures a cache-reuse gate without a cache_reuse_percent reference",
                path
            );
        }
        if profile
            .allowed_repair_turn_increase
            .is_some_and(|allowed| profile.repair_turns.checked_add(allowed).is_none())
        {
            anyhow::bail!(
                "Eval baseline {:?} profile {profile_number} allowed_repair_turn_increase overflows the repair-turn limit",
                path
            );
        }
        let key = profile.key();
        if !keys.insert(key) {
            anyhow::bail!(
                "Eval baseline {:?} contains duplicate profile {profile_number}",
                path
            );
        }
    }
    Ok(())
}

fn validate_percent(path: &Path, profile: usize, name: &str, value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        anyhow::bail!(
            "Eval baseline {:?} profile {profile} {name} must be between 0 and 100",
            path
        );
    }
    Ok(())
}

fn signed_delta(actual: u64, reference: u64) -> i64 {
    let delta = i128::from(actual) - i128::from(reference);
    delta.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn optional_delta(actual: Option<u64>, reference: Option<u64>) -> Option<i64> {
    Some(signed_delta(actual?, reference?))
}

fn push_growth_violation(
    metric: &str,
    actual: u64,
    reference: u64,
    allowed_growth_percent: u64,
    violations: &mut Vec<String>,
) {
    if growth_exceeds(actual, reference, allowed_growth_percent) {
        violations.push(format!(
            "{metric} grew from {reference} to {actual}, exceeding the allowed {allowed_growth_percent}% growth"
        ));
    }
}

fn push_optional_growth_violation(
    metric: &str,
    actual: Option<u64>,
    reference: Option<u64>,
    allowed_growth_percent: u64,
    violations: &mut Vec<String>,
) {
    match (actual, reference) {
        (Some(actual), Some(reference)) => push_growth_violation(
            metric,
            actual,
            reference,
            allowed_growth_percent,
            violations,
        ),
        (None, _) => violations.push(format!(
            "{metric} gate could not be evaluated because the run did not report {metric}"
        )),
        (_, None) => violations.push(format!(
            "{metric} gate could not be evaluated because the baseline has no {metric} reference"
        )),
    }
}

fn push_cache_reuse_violation(
    actual: Option<u64>,
    reference: Option<u64>,
    allowed_drop_points: u64,
    violations: &mut Vec<String>,
) {
    match (actual, reference) {
        (Some(actual), Some(reference)) => {
            let drop = reference.saturating_sub(actual);
            if drop > allowed_drop_points.saturating_mul(10) {
                violations.push(format!(
                    "cache reuse dropped from {}.{}% to {}.{}%, exceeding the allowed {allowed_drop_points}-point drop",
                    reference / 10, reference % 10,
                    actual / 10, actual % 10,
                ));
            }
        }
        (None, _) => violations.push(
            "cache-reuse gate could not be evaluated because the run did not report cache reuse"
                .to_string(),
        ),
        (_, None) => violations.push(
            "cache-reuse gate could not be evaluated because the baseline has no cache-reuse reference"
                .to_string(),
        ),
    }
}

fn growth_exceeds(actual: u64, reference: u64, allowed_growth_percent: u64) -> bool {
    if actual <= reference {
        return false;
    }
    let growth = u128::from(actual - reference) * 100;
    let allowed_growth = u128::from(reference) * u128::from(allowed_growth_percent);
    growth > allowed_growth
}

/// Aggregate cache reads across all measured provider turns in a suite.
pub(crate) fn aggregate_cache_reuse_percent(tasks: &[TaskReport]) -> Option<u64> {
    let (read_tokens, measured_tokens) = tasks
        .iter()
        .flat_map(|task| &task.usage_turns)
        .filter_map(|turn| {
            turn.cache_measured_input_tokens
                .map(|measured| (turn.cache_read_input_tokens.unwrap_or(0), measured))
        })
        .fold(
            (0u64, 0u64),
            |(reads, measured), (turn_reads, turn_measured)| {
                (
                    reads.saturating_add(turn_reads),
                    measured.saturating_add(turn_measured),
                )
            },
        );
    read_tokens
        .saturating_mul(1000)
        .checked_div(measured_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(score_percent: f64) -> EvalReport {
        EvalReport {
            run_id: "run".to_string(),
            suite: super::super::SuiteReport {
                id: "suite".to_string(),
                path: "suite.toml".to_string(),
                repetitions: 1,
            },
            mode: super::super::EvalMode::Mock,
            provider: "provider".to_string(),
            model: "model".to_string(),
            reasoning: "default".to_string(),
            seed: 1,
            score: super::super::ScoreReport {
                passed: 3,
                total: 4,
                percent: score_percent,
            },
            tasks: Vec::new(),
            usage: super::super::UsageReport {
                prompt_tokens: 70,
                completion_tokens: 30,
                total_tokens: 100,
                cost_micros: Some(40),
            },
            cost_micros: Some(40),
            tokens_per_dollar: Some(2_500_000.0),
            duration_ms: 25,
            cache_reuse_percent: Some(600),
            repair_turns: 2,
            baseline: None,
            output_dir: "out".to_string(),
        }
    }

    #[test]
    fn signed_delta_saturates() {
        assert_eq!(signed_delta(5, 8), -3);
        assert_eq!(signed_delta(u64::MAX, 0), i64::MAX);
    }

    #[test]
    fn baseline_validation_rejects_duplicate_profiles() {
        let body = r#"
schema_version = 1

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
total_tokens = 10
duration_ms = 20
repair_turns = 0

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
total_tokens = 10
duration_ms = 20
repair_turns = 0
"#;
        let baseline: EvalBaselineFile = toml::from_str(body).unwrap();

        let error = validate_baseline(&baseline, Path::new("baseline.toml")).unwrap_err();

        assert!(error.to_string().contains("duplicate profile 2"));
    }

    #[test]
    fn baseline_comparison_flags_correctness_regression_and_reports_deltas() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            temp.path(),
            r#"
schema_version = 1

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
allowed_score_drop_percent = 5.0
total_tokens = 90
cost_micros = 30
duration_ms = 20
cache_reuse_percent = 650
repair_turns = 1
"#,
        )
        .unwrap();

        let baseline = load_eval_baseline(temp.path()).unwrap();
        let comparison = baseline.compare(&report(75.0)).unwrap();

        assert!(!comparison.passed);
        assert_eq!(comparison.minimum_score_percent, 95.0);
        assert_eq!(comparison.deltas.total_tokens, 10);
        assert_eq!(comparison.deltas.cost_micros, Some(10));
        assert_eq!(comparison.deltas.duration_ms, 5);
        assert_eq!(comparison.deltas.cache_reuse_percent, Some(-50));
        assert_eq!(comparison.deltas.repair_turns, 1);
        assert_eq!(comparison.violations.len(), 1);
    }

    #[test]
    fn configured_efficiency_tolerances_flag_material_regressions() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            temp.path(),
            r#"
schema_version = 1

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
allowed_total_token_growth_percent = 5
allowed_cost_growth_percent = 10
allowed_duration_growth_percent = 20
allowed_cache_reuse_drop_points = 5
allowed_repair_turn_increase = 1
total_tokens = 100
cost_micros = 40
duration_ms = 25
cache_reuse_percent = 600
repair_turns = 2
"#,
        )
        .unwrap();
        let baseline = load_eval_baseline(temp.path()).unwrap();
        let mut actual = report(100.0);
        actual.usage.total_tokens = 106;
        actual.cost_micros = Some(45);
        actual.duration_ms = 31;
        actual.cache_reuse_percent = Some(540);
        actual.repair_turns = 4;

        let comparison = baseline.compare(&actual).unwrap();

        assert_eq!(
            comparison.violations,
            vec![
                "total tokens grew from 100 to 106, exceeding the allowed 5% growth",
                "cost grew from 40 to 45, exceeding the allowed 10% growth",
                "duration grew from 25 to 31, exceeding the allowed 20% growth",
                "cache reuse dropped from 60.0% to 54.0%, exceeding the allowed 5-point drop",
                "repair turns increased from 2 to 4, exceeding the allowed increase of 1",
            ]
        );
    }

    #[test]
    fn configured_efficiency_tolerances_include_the_boundary() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            temp.path(),
            r#"
schema_version = 1

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
allowed_total_token_growth_percent = 5
allowed_cost_growth_percent = 10
allowed_duration_growth_percent = 20
allowed_cache_reuse_drop_points = 5
allowed_repair_turn_increase = 1
total_tokens = 100
cost_micros = 40
duration_ms = 25
cache_reuse_percent = 600
repair_turns = 2
"#,
        )
        .unwrap();
        let baseline = load_eval_baseline(temp.path()).unwrap();
        let mut actual = report(100.0);
        actual.usage.total_tokens = 105;
        actual.cost_micros = Some(44);
        actual.duration_ms = 30;
        actual.cache_reuse_percent = Some(550);
        actual.repair_turns = 3;

        let comparison = baseline.compare(&actual).unwrap();

        assert!(comparison.passed, "violations: {:?}", comparison.violations);
    }

    #[test]
    fn comparison_serializes_the_applied_efficiency_policy() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            temp.path(),
            r#"
schema_version = 1

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
allowed_total_token_growth_percent = 5
total_tokens = 100
duration_ms = 25
repair_turns = 2
"#,
        )
        .unwrap();
        let baseline = load_eval_baseline(temp.path()).unwrap();
        let comparison = baseline.compare(&report(100.0)).unwrap();

        let serialized = serde_json::to_value(comparison).unwrap();

        assert_eq!(
            serialized["policy"]["allowed_total_token_growth_percent"],
            5
        );
    }

    #[test]
    fn configured_optional_gates_fail_when_the_run_omits_measurements() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            temp.path(),
            r#"
schema_version = 1

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
allowed_cost_growth_percent = 10
allowed_cache_reuse_drop_points = 5
total_tokens = 100
cost_micros = 40
duration_ms = 25
cache_reuse_percent = 600
repair_turns = 2
"#,
        )
        .unwrap();
        let baseline = load_eval_baseline(temp.path()).unwrap();
        let mut actual = report(100.0);
        actual.cost_micros = None;
        actual.cache_reuse_percent = None;

        let comparison = baseline.compare(&actual).unwrap();

        assert_eq!(
            comparison.violations,
            vec![
                "cost gate could not be evaluated because the run did not report cost",
                "cache-reuse gate could not be evaluated because the run did not report cache reuse",
            ]
        );
    }

    #[test]
    fn baseline_validation_rejects_cost_gate_without_reference() {
        let body = r#"
schema_version = 1

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
allowed_cost_growth_percent = 10
total_tokens = 100
duration_ms = 25
repair_turns = 2
"#;
        let baseline: EvalBaselineFile = toml::from_str(body).unwrap();

        let error = validate_baseline(&baseline, Path::new("baseline.toml")).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("without a cost_micros reference")
        );
    }

    #[test]
    fn baseline_validation_rejects_cache_gate_without_reference() {
        let body = r#"
schema_version = 1

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
allowed_cache_reuse_drop_points = 5
total_tokens = 100
duration_ms = 25
repair_turns = 2
"#;
        let baseline: EvalBaselineFile = toml::from_str(body).unwrap();

        let error = validate_baseline(&baseline, Path::new("baseline.toml")).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("without a cache_reuse_percent reference")
        );
    }

    #[test]
    fn baseline_validation_rejects_efficiency_percent_above_one_hundred() {
        let body = r#"
schema_version = 1

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
allowed_total_token_growth_percent = 101
total_tokens = 100
duration_ms = 25
repair_turns = 2
"#;
        let baseline: EvalBaselineFile = toml::from_str(body).unwrap();

        let error = validate_baseline(&baseline, Path::new("baseline.toml")).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("allowed_total_token_growth_percent must be between 0 and 100")
        );
    }

    #[test]
    fn baseline_validation_rejects_repair_turn_limit_overflow() {
        let body = r#"
schema_version = 1

[[profiles]]
suite = "suite"
provider = "provider"
model = "model"
effort = "default"
score_percent = 100.0
total_tokens = 100
duration_ms = 25
repair_turns = 2
"#;
        let mut baseline: EvalBaselineFile = toml::from_str(body).unwrap();
        baseline.profiles[0].repair_turns = u64::MAX;
        baseline.profiles[0].allowed_repair_turn_increase = Some(1);

        let error = validate_baseline(&baseline, Path::new("baseline.toml")).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("overflows the repair-turn limit")
        );
    }

    #[test]
    fn growth_comparison_uses_wide_arithmetic_at_u64_max() {
        assert!(growth_exceeds(u64::MAX, u64::MAX / 2, 100));
    }
}
