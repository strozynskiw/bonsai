use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Typed reason a foreground run stopped after consuming an explicit budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RunBudgetExhaustion {
    #[error(
        "Agent stopped after {limit} model/tool iterations because its max-turn budget was exhausted"
    )]
    MaxTurns { limit: usize },
    #[error("session exhausted its {limit}-turn budget after {used} persisted provider turns")]
    SessionTurns { limit: usize, used: usize },
    #[error(
        "session exhausted its {limit_seconds}s active-time budget after {used_seconds}s of foreground execution"
    )]
    SessionTime {
        limit_seconds: u64,
        used_seconds: u64,
    },
    #[error("run exhausted its {limit_seconds}s wall-time budget")]
    RunTime { limit_seconds: u64 },
    #[error("provider generation stalled with no output for {limit_seconds}s")]
    GenerationTime { limit_seconds: u64 },
    #[error("provider generation exhausted its {limit_chars}-character output budget")]
    ProviderOutput { limit_chars: usize },
    #[error(
        "session exhausted its {limit_chars}-character provider-output budget after {used_chars} characters"
    )]
    SessionOutput {
        limit_chars: usize,
        used_chars: usize,
    },
    #[error(
        "session exhausted its {limit_micros}-micro-USD cost budget after {used_micros} micro-USD of exactly priced usage"
    )]
    SessionCost { limit_micros: u64, used_micros: u64 },
    #[error("provider stopped because its generation budget was exhausted")]
    ProviderGeneration,
    #[error("provider rate limit remained active after {attempts} attempts")]
    ProviderRateLimit { attempts: usize },
    #[error("provider account quota or credit limit was exhausted")]
    ProviderQuota,
}

impl RunBudgetExhaustion {
    pub(crate) fn to_json(self) -> Result<String> {
        serde_json::to_string(&self).context("Failed to serialize run budget exhaustion")
    }

    pub(crate) fn from_json(value: &str) -> Result<Self> {
        serde_json::from_str(value).context("Failed to parse run budget exhaustion")
    }
}

/// Optional limits applied to foreground runs and their resumable session.
///
/// Every field is opt-in. `max_session_*` limits consume the persisted usage
/// ledger across resumes; the other fields reset for each foreground run or
/// provider/tool call. An empty value preserves Bonsai's normal runtime policy
/// and is also the persisted default for new and existing users.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct RunBudget {
    pub(crate) max_turns: Option<usize>,
    pub(crate) max_run_seconds: Option<u64>,
    pub(crate) max_generation_seconds: Option<u64>,
    pub(crate) max_output_chars: Option<usize>,
    pub(crate) max_tool_seconds: Option<u64>,
    pub(crate) max_session_turns: Option<usize>,
    pub(crate) max_session_output_chars: Option<usize>,
    pub(crate) max_session_active_seconds: Option<u64>,
    pub(crate) max_session_cost_micros: Option<u64>,
}

/// Persisted usage consumed by the cumulative session budget axes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct SessionBudgetUsage {
    pub(crate) turns: usize,
    pub(crate) turn_limit: Option<usize>,
    pub(crate) output_chars: usize,
    pub(crate) output_char_limit: Option<usize>,
    pub(crate) active_run_ms: u64,
    pub(crate) active_limit_seconds: Option<u64>,
    /// Exact cumulative cost, absent once any provider turn could not be priced.
    pub(crate) exact_cost_micros: Option<u64>,
    pub(crate) cost_limit_micros: Option<u64>,
}

impl SessionBudgetUsage {
    pub(crate) const fn with_active_run_ms(mut self, active_run_ms: u64) -> Self {
        self.active_run_ms = active_run_ms;
        self
    }
}

impl RunBudget {
    /// Validate values loaded from persistent storage.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured limit is zero.
    pub(crate) fn validate(self) -> Result<Self> {
        validate_positive("max_turns", self.max_turns)?;
        validate_positive("max_run_seconds", self.max_run_seconds)?;
        validate_positive("max_generation_seconds", self.max_generation_seconds)?;
        validate_positive("max_output_chars", self.max_output_chars)?;
        validate_positive("max_tool_seconds", self.max_tool_seconds)?;
        validate_positive("max_session_turns", self.max_session_turns)?;
        validate_positive("max_session_output_chars", self.max_session_output_chars)?;
        validate_positive(
            "max_session_active_seconds",
            self.max_session_active_seconds,
        )?;
        validate_positive("max_session_cost_micros", self.max_session_cost_micros)?;
        Ok(self)
    }

    pub(crate) fn max_run_duration(self) -> Option<Duration> {
        self.max_run_seconds.map(Duration::from_secs)
    }

    pub(crate) fn max_generation_duration(self) -> Option<Duration> {
        self.max_generation_seconds.map(Duration::from_secs)
    }

    pub(crate) fn max_tool_duration(self) -> Option<Duration> {
        self.max_tool_seconds.map(Duration::from_secs)
    }

    pub(crate) fn max_session_active_duration(self) -> Option<Duration> {
        self.max_session_active_seconds.map(Duration::from_secs)
    }

    pub(crate) fn to_json(self) -> Result<String> {
        serde_json::to_string(&self).context("Failed to serialize run budget preference")
    }

    pub(crate) fn from_json(value: &str) -> Result<Self> {
        serde_json::from_str::<Self>(value)
            .context("Failed to parse run budget preference")?
            .validate()
    }
}

/// Select the earliest wall-clock deadline shared by interactive and
/// noninteractive foreground runs.
///
/// `explicit_timeout` is a headless process deadline rather than a budget, so
/// it carries no [`RunBudgetExhaustion`]. Saved per-run and cumulative session
/// limits carry their typed exhaustion reason on both surfaces.
pub(crate) fn select_runtime_timeout(
    explicit_timeout: Option<Duration>,
    max_run_duration: Option<Duration>,
    max_session_active_duration: Option<Duration>,
    active_run_ms: u64,
) -> Option<(Duration, Option<RunBudgetExhaustion>)> {
    let mut selected = explicit_timeout.map(|duration| (duration, None));
    if let Some(duration) = max_run_duration {
        let candidate = (
            duration,
            Some(RunBudgetExhaustion::RunTime {
                limit_seconds: duration.as_secs().max(1),
            }),
        );
        if selected.is_none_or(|current| candidate.0 <= current.0) {
            selected = Some(candidate);
        }
    }
    if let Some(limit) = max_session_active_duration {
        let limit_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
        let candidate = (
            Duration::from_millis(limit_ms.saturating_sub(active_run_ms)),
            Some(session_time_exhaustion(limit, active_run_ms)),
        );
        if selected.is_none_or(|current| candidate.0 <= current.0) {
            selected = Some(candidate);
        }
    }
    selected
}

/// Select a budget-backed deadline for the TUI foreground task controller.
pub(crate) fn select_budget_timeout(
    max_run_duration: Option<Duration>,
    max_session_active_duration: Option<Duration>,
    active_run_ms: u64,
) -> Option<(Duration, RunBudgetExhaustion)> {
    select_runtime_timeout(
        None,
        max_run_duration,
        max_session_active_duration,
        active_run_ms,
    )
    .and_then(|(duration, exhaustion)| exhaustion.map(|reason| (duration, reason)))
}

fn session_time_exhaustion(limit: Duration, active_run_ms: u64) -> RunBudgetExhaustion {
    let limit_seconds = limit.as_secs().max(1);
    RunBudgetExhaustion::SessionTime {
        limit_seconds,
        used_seconds: active_run_ms.div_ceil(1_000).max(limit_seconds),
    }
}

/// Refresh cumulative active-time evidence after a foreground run finishes.
pub(crate) fn refresh_session_time_exhaustion(
    exhaustion: RunBudgetExhaustion,
    active_run_ms: u64,
) -> RunBudgetExhaustion {
    match exhaustion {
        RunBudgetExhaustion::SessionTime { limit_seconds, .. } => {
            RunBudgetExhaustion::SessionTime {
                limit_seconds,
                used_seconds: active_run_ms.div_ceil(1_000).max(limit_seconds),
            }
        }
        exhaustion => exhaustion,
    }
}

fn validate_positive<T>(name: &str, value: Option<T>) -> Result<()>
where
    T: Copy + PartialEq + From<u8>,
{
    if value == Some(T::from(0)) {
        bail!("run budget `{name}` must be greater than zero");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_leaves_every_budget_unset() {
        assert_eq!(
            RunBudget::default(),
            RunBudget {
                max_turns: None,
                max_run_seconds: None,
                max_generation_seconds: None,
                max_output_chars: None,
                max_tool_seconds: None,
                max_session_turns: None,
                max_session_output_chars: None,
                max_session_active_seconds: None,
                max_session_cost_micros: None,
            }
        );
    }

    #[test]
    fn json_round_trip_preserves_selected_limits() {
        let budget = RunBudget {
            max_turns: Some(50),
            max_run_seconds: Some(900),
            max_generation_seconds: Some(180),
            max_output_chars: Some(64_000),
            max_tool_seconds: Some(120),
            max_session_turns: Some(500),
            max_session_output_chars: Some(1_000_000),
            max_session_active_seconds: Some(7_200),
            max_session_cost_micros: Some(5_000_000),
        };

        assert_eq!(
            RunBudget::from_json(&budget.to_json().unwrap()).unwrap(),
            budget
        );
    }

    #[test]
    fn rejects_zero_limits() {
        let error = RunBudget::from_json(r#"{"max_turns":0}"#).unwrap_err();
        assert!(error.to_string().contains("max_turns"));
        let error = RunBudget::from_json(r#"{"max_session_output_chars":0}"#).unwrap_err();
        assert!(error.to_string().contains("max_session_output_chars"));
        let error = RunBudget::from_json(r#"{"max_session_cost_micros":0}"#).unwrap_err();
        assert!(error.to_string().contains("max_session_cost_micros"));
    }

    #[test]
    fn exhaustion_reason_json_preserves_kind_and_limit() {
        let reason = RunBudgetExhaustion::ProviderOutput {
            limit_chars: 64_000,
        };

        assert_eq!(
            RunBudgetExhaustion::from_json(&reason.to_json().unwrap()).unwrap(),
            reason
        );

        let provider_limit = RunBudgetExhaustion::ProviderRateLimit { attempts: 4 };
        assert_eq!(
            RunBudgetExhaustion::from_json(&provider_limit.to_json().unwrap()).unwrap(),
            provider_limit
        );
    }

    #[test]
    fn legacy_session_budget_usage_defaults_new_cost_fields() {
        let usage: SessionBudgetUsage = serde_json::from_str("{}").unwrap();

        assert_eq!(usage, SessionBudgetUsage::default());
    }

    #[test]
    fn session_deadline_wins_for_both_runtime_timeout_entry_points() {
        let expected = (
            Duration::from_secs(5),
            RunBudgetExhaustion::SessionTime {
                limit_seconds: 60,
                used_seconds: 60,
            },
        );

        assert_eq!(
            select_budget_timeout(
                Some(Duration::from_secs(10)),
                Some(Duration::from_secs(60)),
                55_000,
            ),
            Some(expected)
        );
        assert_eq!(
            select_runtime_timeout(
                Some(Duration::from_secs(30)),
                Some(Duration::from_secs(10)),
                Some(Duration::from_secs(60)),
                55_000,
            ),
            Some((expected.0, Some(expected.1)))
        );
    }

    #[test]
    fn explicit_headless_timeout_remains_a_non_budget_deadline() {
        assert_eq!(
            select_runtime_timeout(Some(Duration::from_secs(2)), None, None, 0),
            Some((Duration::from_secs(2), None))
        );
    }
}
