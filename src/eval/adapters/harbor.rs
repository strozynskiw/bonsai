use std::collections::BTreeMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::{AdapterTerminalState, BenchmarkKind, BenchmarkPin, HARBOR_HARNESS_COMMIT};

pub(crate) const HARBOR_RESULT_SCHEMA_VERSION: u32 = 1;

/// Bonsai-owned envelope around a pinned Harbor `TrialResult` export.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HarborResultEnvelope {
    pub(crate) schema_version: u32,
    pub(crate) harbor_commit: String,
    #[serde(default)]
    pub(crate) benchmark: Option<BenchmarkPin>,
    #[serde(default)]
    pub(crate) agent: Option<String>,
    #[serde(default)]
    pub(crate) manifest_digest: Option<String>,
    #[serde(default)]
    pub(crate) artifact_digest: Option<String>,
    #[serde(default)]
    pub(crate) comparison_manifest: Option<serde_json::Value>,
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
    pub(crate) benchmark: BenchmarkKind,
    pub(crate) terminal_state: AdapterTerminalState,
    pub(crate) score: Option<f64>,
    pub(crate) verifier_passed: Option<bool>,
    pub(crate) terminal_reason: Option<String>,
}

/// Validate and normalize one Harbor result without invoking its verifier.
pub(crate) fn import_harbor_result(body: &str) -> Result<HarborImportedResult> {
    let envelope: HarborResultEnvelope = serde_json::from_str(body)?;
    let benchmark = match envelope.schema_version {
        1 => {
            anyhow::ensure!(
                envelope.benchmark.is_none()
                    && envelope.agent.is_none()
                    && envelope.manifest_digest.is_none()
                    && envelope.artifact_digest.is_none()
                    && envelope.comparison_manifest.is_none(),
                "Historical TB2 envelope cannot carry TB4 fields"
            );
            BenchmarkKind::TerminalBench2
        }
        2 => {
            let pin = envelope
                .benchmark
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("TB4 requires benchmark pin"))?;
            pin.validate()?;
            anyhow::ensure!(
                pin.kind == BenchmarkKind::TerminalBench4
                    && envelope.harbor_commit == pin.harness_commit,
                "TB4 benchmark/harness mismatch"
            );
            // This importer feeds Bonsai qualification; paired DSH evidence is
            // deliberately handled by the separate comparison report path.
            anyhow::ensure!(
                envelope.agent.as_deref() == Some("bonsai"),
                "Only Bonsai evidence may enter qualification"
            );
            for digest in [&envelope.manifest_digest, &envelope.artifact_digest] {
                let value = digest.as_deref().and_then(|s| s.strip_prefix("sha256:"));
                anyhow::ensure!(
                    value
                        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
                    "TB4 requires manifest and artifact digests"
                );
            }
            let manifest = envelope
                .comparison_manifest
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("TB4 requires frozen manifest bytes"))?;
            use sha2::{Digest, Sha256};
            let bytes = serde_json::to_vec(manifest)?;
            let actual = format!(
                "sha256:{}",
                Sha256::digest(&bytes)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            anyhow::ensure!(
                envelope.manifest_digest.as_deref() == Some(actual.as_str()),
                "TB4 changed manifest digest"
            );
            anyhow::ensure!(
                manifest["dataset"] == "terminal-bench/terminal-bench@4.0.0"
                    && manifest["harbor_commit"] == pin.harness_commit,
                "TB4 manifest benchmark mismatch"
            );
            anyhow::ensure!(
                manifest["artifacts"]["bonsai"]["artifact_digest"].as_str()
                    == envelope.artifact_digest.as_deref(),
                "TB4 manifest artifact mismatch"
            );
            super::comparison::validate_manifest(manifest, &envelope.trial_result.task_name)?;
            let metadata = envelope
                .trial_result
                .agent_result
                .as_ref()
                .and_then(|result| result.metadata.as_ref())
                .ok_or_else(|| anyhow::anyhow!("TB4 execution metadata required"))?;
            for (recorded, expected) in [
                (
                    "artifact_digest",
                    &manifest["artifacts"]["bonsai"]["artifact_digest"],
                ),
                (
                    "profile_digest",
                    &manifest["profiles"]["bonsai"]["overlay_digest"],
                ),
                ("model", &manifest["model"]),
                (
                    "task_content_digest",
                    &manifest["tasks"]
                        .as_array()
                        .and_then(|tasks| {
                            tasks
                                .iter()
                                .find(|task| task["id"] == envelope.trial_result.task_name)
                        })
                        .ok_or_else(|| anyhow::anyhow!("TB4 task missing"))?["content_digest"],
                ),
            ] {
                anyhow::ensure!(
                    metadata.get(recorded) == Some(expected),
                    "TB4 execution provenance mismatch: {recorded}"
                );
            }
            BenchmarkKind::TerminalBench4
        }
        _ => {
            anyhow::bail!(
                "Harbor result envelope uses schema version {}; supported version is {}",
                envelope.schema_version,
                HARBOR_RESULT_SCHEMA_VERSION
            );
        }
    };
    if benchmark == BenchmarkKind::TerminalBench2 && envelope.harbor_commit != HARBOR_HARNESS_COMMIT
    {
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
            benchmark,
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
            benchmark,
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
            benchmark,
            terminal_state: AdapterTerminalState::InternalError,
            score: None,
            verifier_passed: None,
            terminal_reason: Some("Harbor result did not contain verifier rewards".to_string()),
        });
    }
    anyhow::ensure!(rewards.len() == 1, "Terminal-Bench expects one reward");
    let score = *rewards
        .get("reward")
        .ok_or_else(|| anyhow::anyhow!("Missing Terminal-Bench reward"))?;
    anyhow::ensure!(
        score == 0.0 || score == 1.0,
        "Terminal-Bench verifier reward must be binary"
    );
    let terminal_state = if score == 1.0 {
        AdapterTerminalState::Completed
    } else {
        AdapterTerminalState::VerifierFailed
    };
    Ok(HarborImportedResult {
        task_id: trial.task_name,
        benchmark,
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
    fn tb4_import_is_separate_and_rejects_dsh_qualification() {
        let mut body = serde_json::json!({
            "schema_version": 2,
            "harbor_commit": super::super::HARBOR_TB4_HARNESS_COMMIT,
            "benchmark": {
                "kind": "terminal_bench4", "dataset": super::super::TERMINAL_BENCH_4_DATASET,
                "dataset_version": "4.0.0", "harness_commit": super::super::HARBOR_TB4_HARNESS_COMMIT,
                "contract_commit": super::super::HARBOR_TB4_HARNESS_COMMIT
            },
            "agent": "bonsai", "manifest_digest": format!("sha256:{}", "a".repeat(64)),
            "artifact_digest": format!("sha256:{}", "b".repeat(64)),
            "trial_result": {"task_name": "task", "verifier_result": {"rewards": {"reward": 1}}}
        });
        let content = format!("sha256:{}", "b".repeat(64));
        let artifact = serde_json::json!({"revision": "fixture", "source_digest": content, "dirty": true, "artifact_digest": content, "version": "fixture"});
        let mut dsh = artifact.clone();
        dsh["revision"] = serde_json::json!("687f282e8625c6b8d11ff58211a90524be2e4317");
        dsh["dirty"] = serde_json::json!(false);
        let tasks: Vec<_> = (0..13).map(|index| serde_json::json!({"id": if index == 3 { "task".to_string() } else { format!("task-{index}") }, "content_digest": content, "resource_digest": content, "image_digest": content, "phase": if index < 3 { "smoke" } else { "pilot" }})).collect();
        let profile = serde_json::json!({"overlay_digest": content, "native_limits": "fixture"});
        let model = serde_json::json!({"id": "fixture", "endpoint": "https://provider.invalid", "reasoning": "high", "alias_limitation": "fixture", "fallback": false});
        let manifest = serde_json::json!({
            "schema_version": 1, "dataset": "terminal-bench/terminal-bench@4.0.0", "harbor_commit": super::super::HARBOR_TB4_HARNESS_COMMIT,
            "dataset_digest": content, "dependencies_digest": content,
            "node": {"version": "fixture", "artifact_digest": content},
            "artifacts": {"bonsai": artifact, "dsh": dsh}, "profiles": {"bonsai": profile, "dsh": profile}, "model": model,
            "resources": {"cpu": 1, "ram_mb": 1, "disk_mb": 1, "timeout_seconds": 1, "network": "fixture", "working_directory": "/app", "confinement": "container"},
            "selection": {"seed": "fixture", "rule": "sha256-seed-task-v1", "inventory_digest": content, "repetitions": 3},
            "tasks": tasks, "exclusions": []
        });
        body["trial_result"]["agent_result"] = serde_json::json!({"metadata": {"artifact_digest": content, "profile_digest": content, "model": model, "task_content_digest": content}});
        use sha2::{Digest, Sha256};
        body["manifest_digest"] = serde_json::json!(format!(
            "sha256:{}",
            Sha256::digest(serde_json::to_vec(&manifest).unwrap())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        body["comparison_manifest"] = manifest;
        assert_eq!(
            import_harbor_result(&body.to_string()).unwrap().benchmark,
            BenchmarkKind::TerminalBench4
        );
        let mut invalid = body.clone();
        invalid["trial_result"]["task_name"] = serde_json::json!("not-selected");
        assert!(import_harbor_result(&invalid.to_string()).is_err());
        invalid = body.clone();
        invalid["trial_result"]["agent_result"]["metadata"]["profile_digest"] =
            serde_json::json!("wrong");
        assert!(import_harbor_result(&invalid.to_string()).is_err());
        invalid = body.clone();
        invalid["comparison_manifest"]["resources"] = serde_json::Value::Null;
        assert!(import_harbor_result(&invalid.to_string()).is_err());
        assert!(
            super::super::comparison::validate_manifest(&invalid["comparison_manifest"], "task")
                .is_err()
        );
        let minimal = serde_json::json!({"dataset": "terminal-bench/terminal-bench@4.0.0", "harbor_commit": super::super::HARBOR_TB4_HARNESS_COMMIT, "artifacts": {"bonsai": {"artifact_digest": content}}});
        assert!(super::super::comparison::validate_manifest(&minimal, "task").is_err());
        body["agent"] = serde_json::json!("dsh");
        assert!(import_harbor_result(&body.to_string()).is_err());
        body["agent"] = serde_json::json!("bonsai");
        body["artifact_digest"] = serde_json::Value::Null;
        assert!(import_harbor_result(&body.to_string()).is_err());
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
