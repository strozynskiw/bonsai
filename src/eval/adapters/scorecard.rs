//! Offline qualification scorecards. Private artifacts are never serialized here.
use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{BenchmarkKind, BenchmarkPin, SidecarBonsai};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExternalScorecard {
    pub(crate) schema_version: u32,
    pub(crate) fixture: bool,
    pub(crate) reviewed: bool,
    pub(crate) manifest_digest: String,
    pub(crate) binary_digest: String,
    pub(crate) profile: SidecarBonsai,
    pub(crate) suites: Vec<SuiteScore>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SuiteScore {
    pub(crate) pin: BenchmarkPin,
    pub(crate) expected_trials: Vec<String>,
    pub(crate) trials: Vec<TrialScore>,
    #[serde(default)]
    pub(crate) counters: Option<Counts>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrialScore {
    pub(crate) id: String,
    pub(crate) outcome: TrialOutcome,
    pub(crate) reward: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrialOutcome {
    Pass,
    Fail,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Counts {
    pub(crate) pass: usize,
    pub(crate) fail: usize,
    pub(crate) error: usize,
    pub(crate) missing: usize,
}

impl SuiteScore {
    pub(crate) fn counts(&self) -> Result<Counts> {
        self.pin.validate()?;
        let expected: HashSet<_> = self.expected_trials.iter().collect();
        anyhow::ensure!(
            !expected.is_empty() && expected.len() == self.expected_trials.len(),
            "Empty or duplicate expected trials"
        );
        let mut seen = HashSet::new();
        let mut counts = Counts {
            pass: 0,
            fail: 0,
            error: 0,
            missing: 0,
        };
        for trial in &self.trials {
            anyhow::ensure!(
                expected.contains(&trial.id) && seen.insert(&trial.id),
                "Foreign or duplicate trial"
            );
            if self.pin.kind == BenchmarkKind::TerminalBench2
                && trial.outcome != TrialOutcome::Error
            {
                anyhow::ensure!(
                    trial.reward
                        == Some(if trial.outcome == TrialOutcome::Pass {
                            1.0
                        } else {
                            0.0
                        }),
                    "Terminal-Bench requires binary verifier reward"
                );
            }
            match trial.outcome {
                TrialOutcome::Pass => counts.pass += 1,
                TrialOutcome::Fail => counts.fail += 1,
                TrialOutcome::Error => counts.error += 1,
            }
        }
        counts.missing = expected.len() - seen.len();
        Ok(counts)
    }

    fn score(&self) -> Result<f64> {
        let counts = self.counts()?;
        anyhow::ensure!(
            counts.error == 0 && counts.missing == 0,
            "Incomplete qualification"
        );
        Ok(counts.pass as f64 / self.expected_trials.len() as f64 * 100.0)
    }
}

impl ExternalScorecard {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.schema_version == 1, "Unsupported scorecard schema");
        anyhow::ensure!(
            !self.fixture && self.reviewed,
            "Qualification requires reviewed real evidence"
        );
        for digest in [&self.manifest_digest, &self.binary_digest] {
            anyhow::ensure!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "Missing evidence digest"
            );
        }
        anyhow::ensure!(
            self.suites.len() == 2
                && self
                    .suites
                    .iter()
                    .any(|s| s.pin.kind == BenchmarkKind::SweBenchVerified)
                && self
                    .suites
                    .iter()
                    .any(|s| s.pin.kind == BenchmarkKind::TerminalBench2),
            "Both benchmark suites are required"
        );
        for suite in &self.suites {
            anyhow::ensure!(
                suite.counters.as_ref() == Some(&suite.counts()?),
                "Missing or inconsistent counters"
            );
            suite.score()?;
        }
        Ok(())
    }
}

/// Compare reviewed external evidence without invoking a harness or model.
/// # Errors
/// Rejects missing, incomplete, unreviewed or incompatible evidence and regressions.
pub(crate) fn compare_external(candidate: &Path, baseline: &Path) -> Result<()> {
    let load = |path: &Path| -> Result<ExternalScorecard> {
        serde_json::from_slice(
            &std::fs::read(path)
                .with_context(|| format!("Missing scorecard {}", path.display()))?,
        )
        .context("Invalid scorecard")
    };
    compare(&load(candidate)?, &load(baseline)?)
}

/// Validate and publish only the typed allowlist; never copy raw artifacts.
/// # Errors
/// Rejects malformed evidence, inconsistent trials, and unknown private fields.
pub(crate) fn import_scorecard(input: &Path, output: &Path) -> Result<()> {
    let body = std::fs::read(input)?;
    let mut card: ExternalScorecard = match serde_json::from_slice::<ExternalScorecard>(&body) {
        Ok(card) => {
            anyhow::ensure!(card.fixture, "Real imports require source evidence bundle");
            card
        }
        Err(_) => {
            let bundle: ImportBundle = serde_json::from_slice(&body)?;
            let mut card = bundle.scorecard;
            if !card.fixture {
                let provenance = bundle
                    .swe_provenance
                    .as_ref()
                    .context("Missing verified SWE provenance")?;
                anyhow::ensure!(bundle.swe_report.is_some(), "Missing SWE report");
                anyhow::ensure!(
                    provenance.binary_blake3 == card.binary_digest
                        && provenance.revision == card.profile.declared_revision,
                    "SWE candidate mismatch"
                );
                let suite = card
                    .suites
                    .iter()
                    .find(|s| s.pin.kind == BenchmarkKind::SweBenchVerified)
                    .context("Missing SWE suite")?;
                anyhow::ensure!(
                    provenance.dataset_revision == suite.pin.dataset_version
                        && provenance.harness_commit == suite.pin.harness_commit,
                    "SWE dataset/harness mismatch"
                );
                let mut ids = HashSet::new();
                for sidecar in &provenance.sidecars {
                    anyhow::ensure!(
                        ids.insert(&sidecar.task_id)
                            && suite.expected_trials.contains(&sidecar.task_id),
                        "Duplicate or foreign SWE sidecar"
                    );
                    anyhow::ensure!(
                        sidecar.bonsai == card.profile
                            && sidecar.benchmark.harness_commit == suite.pin.harness_commit
                            && sidecar.benchmark.dataset_version == suite.pin.dataset_version,
                        "SWE sidecar profile/pin mismatch"
                    );
                }
                anyhow::ensure!(
                    ids.len() == suite.expected_trials.len(),
                    "Missing SWE sidecars"
                );
            }
            if let Some(report) = bundle.swe_report {
                let suite = card
                    .suites
                    .iter_mut()
                    .find(|s| s.pin.kind == BenchmarkKind::SweBenchVerified)
                    .context("Missing SWE suite")?;
                anyhow::ensure!(
                    suite.trials.is_empty(),
                    "Cannot mix supplied and imported SWE trials"
                );
                suite.trials = import_swe(&report, &suite.expected_trials)?;
            }
            let suite = card
                .suites
                .iter_mut()
                .find(|s| s.pin.kind == BenchmarkKind::TerminalBench2)
                .context("Missing Terminal-Bench suite")?;
            anyhow::ensure!(
                suite.trials.is_empty(),
                "Cannot mix supplied and imported Harbor trials"
            );
            let mut evidence = HashSet::new();
            for trial in bundle.harbor_trials {
                anyhow::ensure!(
                    evidence.insert(blake3::hash(
                        serde_json::to_string(&trial.result)?.as_bytes()
                    )),
                    "Duplicate Harbor evidence"
                );
                let envelope: super::HarborResultEnvelope =
                    serde_json::from_value(trial.result.clone())?;
                anyhow::ensure!(
                    envelope.trial_result.task_name == trial.task_id
                        && trial.id == format!("{}-run-{}", trial.task_id, trial.repetition)
                        && trial.repetition > 0,
                    "Harbor task mismatch"
                );
                if !card.fixture {
                    let metadata = envelope
                        .trial_result
                        .agent_result
                        .as_ref()
                        .and_then(|agent| agent.metadata.as_ref())
                        .context("Missing Harbor provenance")?;
                    let actual: SidecarBonsai = serde_json::from_value(
                        metadata
                            .get("bonsai_profile")
                            .context("Missing Harbor execution profile")?
                            .clone(),
                    )?;
                    anyhow::ensure!(actual == card.profile, "Harbor profile mismatch");
                    anyhow::ensure!(
                        metadata
                            .get("bonsai_binary_digest")
                            .and_then(serde_json::Value::as_str)
                            == Some(card.binary_digest.as_str()),
                        "Harbor binary mismatch"
                    );
                    anyhow::ensure!(
                        metadata
                            .get("bonsai_trial_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(trial.id.as_str()),
                        "Harbor trial identity mismatch"
                    );
                }
                let imported = super::import_harbor_result(&serde_json::to_string(&trial.result)?)?;
                suite.trials.push(TrialScore {
                    id: trial.id,
                    outcome: match imported.verifier_passed {
                        Some(true) => TrialOutcome::Pass,
                        Some(false) => TrialOutcome::Fail,
                        None => TrialOutcome::Error,
                    },
                    reward: imported.score,
                });
            }
            card
        }
    };
    // Import never attests human review or promotes a baseline.
    card.reviewed = false;
    anyhow::ensure!(card.schema_version == 1, "Unsupported scorecard schema");
    for suite in &mut card.suites {
        suite.counters = Some(suite.counts()?);
    }
    reject_secrets(&serde_json::to_value(&card)?)?;
    super::write_json_atomic(output, &card)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportBundle {
    scorecard: ExternalScorecard,
    harbor_trials: Vec<HarborTrialImport>,
    #[serde(default)]
    swe_report: Option<SweReport>,
    #[serde(default)]
    swe_provenance: Option<SweProvenance>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SweProvenance {
    binary_blake3: String,
    revision: String,
    dataset_revision: String,
    harness_commit: String,
    sidecars: Vec<super::AdapterSidecar>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HarborTrialImport {
    id: String,
    task_id: String,
    repetition: usize,
    result: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct SweReport {
    schema_version: u32,
    total_instances: usize,
    submitted_ids: Vec<String>,
    completed_ids: Vec<String>,
    resolved_ids: Vec<String>,
    unresolved_ids: Vec<String>,
    error_ids: Vec<String>,
    incomplete_ids: Vec<String>,
    empty_patch_ids: Vec<String>,
}

fn import_swe(report: &SweReport, expected: &[String]) -> Result<Vec<TrialScore>> {
    anyhow::ensure!(
        report.schema_version == 2 && report.total_instances == expected.len(),
        "SWE schema or denominator mismatch"
    );
    let expected: HashSet<_> = expected.iter().collect();
    let group = |ids: &[String]| -> Result<HashSet<String>> {
        let set: HashSet<_> = ids.iter().cloned().collect();
        anyhow::ensure!(
            set.len() == ids.len() && set.iter().all(|id| expected.contains(id)),
            "Foreign or duplicate SWE IDs"
        );
        Ok(set)
    };
    let submitted = group(&report.submitted_ids)?;
    let completed = group(&report.completed_ids)?;
    let resolved = group(&report.resolved_ids)?;
    let unresolved = group(&report.unresolved_ids)?;
    let errors = group(&report.error_ids)?;
    let empty = group(&report.empty_patch_ids)?;
    let incomplete = group(&report.incomplete_ids)?;
    anyhow::ensure!(
        incomplete
            == expected
                .iter()
                .filter(|id| !submitted.contains(**id))
                .map(|id| (*id).clone())
                .collect(),
        "SWE incomplete IDs mismatch"
    );
    let classifications = [&resolved, &unresolved, &errors, &empty];
    for (index, set) in classifications.iter().enumerate() {
        anyhow::ensure!(
            set.is_subset(&submitted)
                && classifications[index + 1..]
                    .iter()
                    .all(|other| set.is_disjoint(other)),
            "Contradictory SWE outcomes"
        );
    }
    anyhow::ensure!(
        resolved.is_subset(&completed)
            && unresolved.is_subset(&completed)
            && completed
                .iter()
                .all(|id| resolved.contains(id) || unresolved.contains(id) || errors.contains(id)),
        "SWE completed classification mismatch"
    );
    Ok(submitted
        .into_iter()
        .filter_map(|id| {
            let outcome = if resolved.contains(&id) {
                TrialOutcome::Pass
            } else if unresolved.contains(&id) || empty.contains(&id) {
                TrialOutcome::Fail
            } else if errors.contains(&id) {
                TrialOutcome::Error
            } else {
                return None;
            };
            Some(TrialScore {
                id,
                outcome,
                reward: None,
            })
        })
        .collect())
}

fn reject_secrets(value: &serde_json::Value) -> Result<()> {
    match value {
        serde_json::Value::String(text) => anyhow::ensure!(
            crate::redact::redact(text) == text.as_str(),
            "Scorecard contains credential patterns"
        ),
        serde_json::Value::Array(values) => {
            for value in values {
                reject_secrets(value)?;
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                reject_secrets(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn compare(candidate: &ExternalScorecard, baseline: &ExternalScorecard) -> Result<()> {
    candidate.validate()?;
    baseline.validate()?;
    anyhow::ensure!(
        candidate.manifest_digest == baseline.manifest_digest,
        "Manifest mismatch"
    );
    let mut profile = candidate.profile.clone();
    profile.declared_revision = baseline.profile.declared_revision.clone();
    profile.binary_version = baseline.profile.binary_version.clone();
    anyhow::ensure!(profile == baseline.profile, "Execution profile mismatch");
    for actual in &candidate.suites {
        let reference = baseline
            .suites
            .iter()
            .find(|s| s.pin.kind == actual.pin.kind)
            .context("Missing baseline suite")?;
        anyhow::ensure!(
            actual.pin == reference.pin && actual.expected_trials == reference.expected_trials,
            "Suite manifest mismatch"
        );
        let minimum = super::super::baseline::minimum_score(reference.score()?, 0.0);
        anyhow::ensure!(actual.score()? >= minimum, "External benchmark regression");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn card() -> ExternalScorecard {
        let request: super::super::AdapterRequest = serde_json::from_str(include_str!(
            "../../../eval/fixtures/adapters/swe-request-v1.json"
        ))
        .unwrap();
        let profile = SidecarBonsai {
            declared_revision: "test".into(),
            binary_version: Some("test".into()),
            provider: request.runner.provider,
            model: request.runner.model,
            reasoning_effort: request.runner.reasoning_effort,
            autonomy: request.runner.autonomy,
            network: request.runner.network,
            budgets: request.runner.budgets,
        };
        let mut terminal = request.benchmark.clone();
        terminal.kind = BenchmarkKind::TerminalBench2;
        terminal.harness_commit = super::super::HARBOR_HARNESS_COMMIT.into();
        terminal.contract_commit = super::super::TERMINAL_BENCH_2_DATASET_COMMIT.into();
        ExternalScorecard {
            schema_version: 1,
            fixture: false,
            reviewed: true,
            manifest_digest: "a".repeat(64),
            binary_digest: "b".repeat(64),
            profile,
            suites: [request.benchmark, terminal]
                .into_iter()
                .map(|pin| SuiteScore {
                    counters: Some(Counts {
                        pass: 1,
                        fail: 0,
                        error: 0,
                        missing: 0,
                    }),
                    pin,
                    expected_trials: vec!["task-run-1".into()],
                    trials: vec![TrialScore {
                        id: "task-run-1".into(),
                        outcome: TrialOutcome::Pass,
                        reward: Some(1.0),
                    }],
                })
                .collect(),
        }
    }

    #[test]
    fn gate_requires_complete_reviewed_matching_evidence() {
        let reference = card();
        assert!(compare(&reference, &reference).is_ok());
        let mut failed = reference.clone();
        failed.suites[0].trials[0].outcome = TrialOutcome::Fail;
        failed.suites[0].counters = Some(failed.suites[0].counts().unwrap());
        assert!(compare(&failed, &reference).is_err());
        assert!(compare(&reference, &failed).is_ok());
        for kind in 0..6 {
            let mut invalid = reference.clone();
            match kind {
                0 => invalid.fixture = true,
                1 => invalid.reviewed = false,
                2 => invalid.suites[1].trials.clear(),
                3 => invalid.suites.truncate(1),
                4 => invalid.profile.budgets.max_turns += 1,
                _ => invalid.manifest_digest = "c".repeat(64),
            }
            assert!(compare(&invalid, &reference).is_err());
        }
    }

    #[test]
    fn publication_rejects_private_fields_and_credentials() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("private.json");
        let output = work.path().join("public.json");
        let mut value = serde_json::to_value(card()).unwrap();
        value["transcript"] = serde_json::json!("private patch content");
        std::fs::write(&input, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(import_scorecard(&input, &output).is_err());
        assert!(!output.exists());
        let mut secret = card();
        secret.profile.model = "sk-abcdefghijklmnopqrstuvwxyz1234567890".into();
        std::fs::write(&input, serde_json::to_vec(&secret).unwrap()).unwrap();
        assert!(import_scorecard(&input, &output).is_err());
        let mut fixture = card();
        fixture.fixture = true;
        std::fs::write(&input, serde_json::to_vec(&fixture).unwrap()).unwrap();
        import_scorecard(&input, &output).unwrap();
        assert!(
            !std::fs::read_to_string(&output)
                .unwrap()
                .contains("transcript")
        );
        assert!(compare_external(&input, &work.path().join("absent.json")).is_err());
    }

    #[test]
    fn offline_harness_bundle_preserves_rewards_and_never_promotes_fixture() {
        let work = tempfile::tempdir().unwrap();
        let input = work.path().join("bundle.json");
        let output = work.path().join("scorecard.json");
        let mut scorecard = card();
        scorecard.fixture = true;
        scorecard.suites[0].expected_trials = vec!["swe-task".into()];
        for suite in &mut scorecard.suites {
            suite.trials.clear();
        }
        let bundle = serde_json::json!({
            "scorecard": scorecard,
            "swe_report": {
                "schema_version": 2, "total_instances": 1,
                "submitted_ids": ["swe-task"], "completed_ids": ["swe-task"],
                "resolved_ids": ["swe-task"], "unresolved_ids": [],
                "error_ids": [], "incomplete_ids": [], "empty_patch_ids": [],
            },
            "harbor_trials": [{ "id": "task-run-1", "task_id": "task", "repetition": 1,
                "result": { "schema_version": 1, "harbor_commit": super::super::HARBOR_HARNESS_COMMIT,
                    "trial_result": { "task_name": "task", "verifier_result": {"rewards": {"reward": 1}} }
                }
            }]
        });
        std::fs::write(&input, serde_json::to_vec(&bundle).unwrap()).unwrap();
        import_scorecard(&input, &output).unwrap();
        let imported: ExternalScorecard =
            serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(imported.suites[1].trials[0].reward, Some(1.0));
        assert!(!imported.reviewed);
        assert_eq!(imported.suites[0].counters.as_ref().unwrap().pass, 1);
        assert!(compare_external(&output, &output).is_err());
        let mut real = imported.clone();
        real.fixture = false;
        std::fs::write(&input, serde_json::to_vec(&real).unwrap()).unwrap();
        assert!(import_scorecard(&input, &output).is_err());
        for separator in ["\t", "\n"] {
            assert!(
                reject_secrets(&serde_json::json!(format!(
                    "Bearer{separator}abcdefghijklmnop1234567890"
                )))
                .is_err()
            );
        }
    }

    #[test]
    fn counts_reject_duplicates_and_account_for_missing_trials() {
        let request: super::super::AdapterRequest = serde_json::from_str(include_str!(
            "../../../eval/fixtures/adapters/swe-request-v1.json"
        ))
        .unwrap();
        let mut suite = SuiteScore {
            counters: None,
            pin: request.benchmark,
            expected_trials: vec!["a".into(), "b".into()],
            trials: vec![TrialScore {
                id: "a".into(),
                outcome: TrialOutcome::Fail,
                reward: None,
            }],
        };
        assert_eq!(
            suite.counts().unwrap(),
            Counts {
                pass: 0,
                fail: 1,
                error: 0,
                missing: 1
            }
        );
        assert!(suite.score().is_err());
        suite.trials.push(suite.trials[0].clone());
        assert!(suite.counts().is_err());
    }
}
