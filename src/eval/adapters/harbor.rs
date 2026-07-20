use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{AdapterTerminalState, HARBOR_HARNESS_COMMIT};

pub(crate) const HARBOR_RESULT_SCHEMA_VERSION: u32 = 1;

/// Bonsai-owned envelope around a pinned Harbor `TrialResult` export.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HarborResultEnvelope {
    pub(crate) schema_version: u32,
    pub(crate) harbor_commit: String,
    pub(crate) trial_result: HarborTrialResult,
}

/// Stable subset of Harbor's `TrialResult`; unknown upstream fields are
/// intentionally ignored only after the enclosing commit pin is verified.
#[derive(Debug, Deserialize)]
pub(crate) struct HarborTrialResult {
    pub(crate) task_name: String,
    #[serde(default)]
    pub(crate) agent_result: Option<HarborAgentContext>,
    #[serde(default)]
    pub(crate) verifier_result: Option<HarborVerifierResult>,
    #[serde(default)]
    pub(crate) exception_info: Option<HarborExceptionInfo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HarborAgentContext {
    #[serde(default)]
    pub(crate) metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HarborVerifierResult {
    #[serde(default)]
    pub(crate) rewards: Option<BTreeMap<String, f64>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct HarborExceptionInfo {
    pub(crate) exception_type: String,
    pub(crate) exception_message: String,
}

/// Normalized result imported from a pinned Harbor trial.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct HarborImportedResult {
    pub(crate) task_id: String,
    pub(crate) terminal_state: AdapterTerminalState,
    pub(crate) score: Option<f64>,
    pub(crate) verifier_passed: Option<bool>,
    pub(crate) terminal_reason: Option<String>,
}

/// Validate and normalize one Harbor result without invoking its verifier.
pub(crate) fn import_harbor_result(body: &str) -> Result<HarborImportedResult> {
    let envelope: HarborResultEnvelope = serde_json::from_str(body)?;
    if envelope.schema_version != HARBOR_RESULT_SCHEMA_VERSION {
        anyhow::bail!(
            "Harbor result envelope uses schema version {}; supported version is {}",
            envelope.schema_version,
            HARBOR_RESULT_SCHEMA_VERSION
        );
    }
    if envelope.harbor_commit != HARBOR_HARNESS_COMMIT {
        anyhow::bail!(
            "Unsupported Harbor result commit '{}'; pinned commit is '{}'",
            envelope.harbor_commit,
            HARBOR_HARNESS_COMMIT
        );
    }
    let trial = envelope.trial_result;
    if let Some(exception) = trial.exception_info {
        let state = exception_state(&exception.exception_type, &exception.exception_message);
        return Ok(HarborImportedResult {
            task_id: trial.task_name,
            terminal_state: state,
            score: None,
            verifier_passed: None,
            terminal_reason: Some(crate::redact::redact(&exception.exception_message).into_owned()),
        });
    }
    if let Some(state) = trial
        .agent_result
        .as_ref()
        .and_then(agent_terminal_state)
        .filter(|state| *state != AdapterTerminalState::Completed)
    {
        return Ok(HarborImportedResult {
            task_id: trial.task_name,
            terminal_state: state,
            score: None,
            verifier_passed: None,
            terminal_reason: Some("Bonsai reported a non-completed terminal state".to_string()),
        });
    }
    let rewards = trial
        .verifier_result
        .and_then(|result| result.rewards)
        .unwrap_or_default();
    if rewards.is_empty() {
        return Ok(HarborImportedResult {
            task_id: trial.task_name,
            terminal_state: AdapterTerminalState::InternalError,
            score: None,
            verifier_passed: None,
            terminal_reason: Some("Harbor result did not contain verifier rewards".to_string()),
        });
    }
    let score = rewards.values().sum::<f64>() / rewards.len() as f64;
    let terminal_state = if score > 0.0 {
        AdapterTerminalState::Completed
    } else {
        AdapterTerminalState::VerifierFailed
    };
    Ok(HarborImportedResult {
        task_id: trial.task_name,
        terminal_state,
        score: Some(score),
        verifier_passed: Some(score > 0.0),
        terminal_reason: None,
    })
}

fn agent_terminal_state(context: &HarborAgentContext) -> Option<AdapterTerminalState> {
    let value = context
        .metadata
        .as_ref()?
        .get("bonsai_terminal_state")?
        .as_str()?;
    match value {
        "completed" => Some(AdapterTerminalState::Completed),
        "budget_exhausted" => Some(AdapterTerminalState::BudgetExhausted),
        "cancelled" => Some(AdapterTerminalState::Cancelled),
        "timed_out" => Some(AdapterTerminalState::TimedOut),
        "terminated" => Some(AdapterTerminalState::Terminated),
        "auth_config_failure" => Some(AdapterTerminalState::AuthConfigFailure),
        "agent_failure" => Some(AdapterTerminalState::AgentFailure),
        "verifier_failed" => Some(AdapterTerminalState::VerifierFailed),
        "patch_rejected" => Some(AdapterTerminalState::PatchRejected),
        "internal_error" => Some(AdapterTerminalState::InternalError),
        _ => None,
    }
}

fn exception_state(exception_type: &str, message: &str) -> AdapterTerminalState {
    let value = format!("{exception_type} {message}").to_ascii_lowercase();
    if value.contains("timeout") || value.contains("timed out") {
        AdapterTerminalState::TimedOut
    } else if value.contains("cancel") || value.contains("interrupt") {
        AdapterTerminalState::Cancelled
    } else if value.contains("auth") || value.contains("not logged in") {
        AdapterTerminalState::AuthConfigFailure
    } else if value.contains("budget") || value.contains("usage limit") {
        AdapterTerminalState::BudgetExhausted
    } else {
        AdapterTerminalState::InternalError
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(trial: serde_json::Value) -> String {
        serde_json::json!({
            "schema_version": HARBOR_RESULT_SCHEMA_VERSION,
            "harbor_commit": HARBOR_HARNESS_COMMIT,
            "trial_result": trial,
        })
        .to_string()
    }

    #[test]
    fn mocked_lifecycle_distinguishes_pass_failure_timeout_and_budget() {
        let passed = import_harbor_result(&envelope(serde_json::json!({
            "task_name": "pass",
            "agent_result": {"metadata": {"bonsai_terminal_state": "completed"}},
            "verifier_result": {"rewards": {"reward": 1}},
        })))
        .unwrap();
        assert_eq!(passed.terminal_state, AdapterTerminalState::Completed);
        assert_eq!(passed.verifier_passed, Some(true));

        let failed = import_harbor_result(&envelope(serde_json::json!({
            "task_name": "fail",
            "verifier_result": {"rewards": {"reward": 0}},
        })))
        .unwrap();
        assert_eq!(failed.terminal_state, AdapterTerminalState::VerifierFailed);
        assert_eq!(failed.verifier_passed, Some(false));

        let timed_out = import_harbor_result(&envelope(serde_json::json!({
            "task_name": "timeout",
            "exception_info": {
                "exception_type": "TimeoutError",
                "exception_message": "agent timed out"
            }
        })))
        .unwrap();
        assert_eq!(timed_out.terminal_state, AdapterTerminalState::TimedOut);

        let budget = import_harbor_result(&envelope(serde_json::json!({
            "task_name": "budget",
            "agent_result": {"metadata": {"bonsai_terminal_state": "budget_exhausted"}}
        })))
        .unwrap();
        assert_eq!(budget.terminal_state, AdapterTerminalState::BudgetExhausted);
    }

    #[test]
    fn unknown_harbor_commit_is_rejected_before_result_use() {
        let body = serde_json::json!({
            "schema_version": HARBOR_RESULT_SCHEMA_VERSION,
            "harbor_commit": "new-head",
            "trial_result": {"task_name": "task"},
        })
        .to_string();
        assert!(
            import_harbor_result(&body)
                .unwrap_err()
                .to_string()
                .contains("Unsupported Harbor result commit")
        );
    }
}
