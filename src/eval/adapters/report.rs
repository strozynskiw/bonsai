use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{
    ADAPTER_SCHEMA_VERSION, AdapterRequest, AdapterTerminalState, ExtractedPatch, LaunchResult,
};

const MAX_FINAL_OUTPUT_CHARS: usize = 16_000;
const MAX_REASON_CHARS: usize = 4_000;
const MAX_STDERR_CHARS: usize = 8_000;

/// Redacted diagnostics stored separately from official benchmark predictions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdapterSidecar {
    pub(crate) schema_version: u32,
    pub(crate) request_key: String,
    pub(crate) benchmark: SidecarBenchmark,
    pub(crate) task_id: String,
    pub(crate) bonsai: SidecarBonsai,
    pub(crate) terminal_state: AdapterTerminalState,
    pub(crate) process: SidecarProcess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<super::HeadlessUsageProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) repair_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) final_output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) patch: Option<SidecarPatch>,
    pub(crate) artifacts: SidecarArtifacts,
}

impl AdapterSidecar {
    pub(crate) fn from_launch(
        request: &AdapterRequest,
        request_key: String,
        launch: LaunchResult,
        patch: Option<&ExtractedPatch>,
        prediction_file: Option<&Path>,
        sidecar_file: &Path,
    ) -> Self {
        let terminal_reason = launch
            .terminal_reason
            .as_deref()
            .map(|value| redacted_bounded(value, MAX_REASON_CHARS));
        let stderr_excerpt = (!launch.stderr.trim().is_empty())
            .then(|| redacted_bounded(&launch.stderr, MAX_STDERR_CHARS));
        let usage = launch.headless.as_ref().map(|output| output.usage);
        let repair_turns = launch
            .headless
            .as_ref()
            .and_then(|output| output.verification)
            .map(|verification| verification.repair_attempts);
        let session_id = launch.headless.as_ref().map(|output| output.session_id);
        let final_output = launch
            .headless
            .as_ref()
            .map(|output| redacted_bounded(&output.output, MAX_FINAL_OUTPUT_CHARS));
        Self {
            schema_version: ADAPTER_SCHEMA_VERSION,
            request_key,
            benchmark: SidecarBenchmark {
                kind: request.benchmark.kind,
                dataset: request.benchmark.dataset.clone(),
                dataset_version: request.benchmark.dataset_version.clone(),
                harness_commit: request.benchmark.harness_commit.clone(),
                contract_commit: request.benchmark.contract_commit.clone(),
            },
            task_id: request.task.id.clone(),
            bonsai: SidecarBonsai {
                declared_revision: request.runner.bonsai_revision.clone(),
                binary_version: launch.binary_version,
                provider: launch
                    .headless
                    .as_ref()
                    .map(|output| output.provider.clone())
                    .unwrap_or_else(|| request.runner.provider.clone()),
                model: launch
                    .headless
                    .as_ref()
                    .map(|output| output.model.clone())
                    .unwrap_or_else(|| request.runner.model.clone()),
                reasoning_effort: request.runner.reasoning_effort.clone(),
                autonomy: request.runner.autonomy,
                network: request.runner.network,
                budgets: request.runner.budgets,
            },
            terminal_state: launch.terminal_state,
            process: SidecarProcess {
                exit_code: launch.exit_code,
                elapsed_ms: launch.elapsed_ms,
                timed_out: launch.timed_out,
                stdout_truncated: launch.stdout_truncated,
                stderr_truncated: launch.stderr_truncated,
                terminal_reason,
                stderr_excerpt,
            },
            usage,
            repair_turns,
            session_id,
            final_output,
            patch: patch.map(|patch| SidecarPatch {
                digest_algorithm: "blake3".to_string(),
                digest: patch.digest.clone(),
                bytes: patch.bytes,
                empty: patch.body.is_empty(),
            }),
            artifacts: SidecarArtifacts {
                prediction: prediction_file.map(file_name),
                sidecar: file_name(sidecar_file),
            },
        }
    }

    pub(crate) fn internal_error(
        request: &AdapterRequest,
        request_key: String,
        reason: &str,
        sidecar_file: &Path,
    ) -> Self {
        Self {
            schema_version: ADAPTER_SCHEMA_VERSION,
            request_key,
            benchmark: SidecarBenchmark {
                kind: request.benchmark.kind,
                dataset: request.benchmark.dataset.clone(),
                dataset_version: request.benchmark.dataset_version.clone(),
                harness_commit: request.benchmark.harness_commit.clone(),
                contract_commit: request.benchmark.contract_commit.clone(),
            },
            task_id: request.task.id.clone(),
            bonsai: SidecarBonsai {
                declared_revision: request.runner.bonsai_revision.clone(),
                binary_version: None,
                provider: request.runner.provider.clone(),
                model: request.runner.model.clone(),
                reasoning_effort: request.runner.reasoning_effort.clone(),
                autonomy: request.runner.autonomy,
                network: request.runner.network,
                budgets: request.runner.budgets,
            },
            terminal_state: AdapterTerminalState::InternalError,
            process: SidecarProcess {
                exit_code: None,
                elapsed_ms: 0,
                timed_out: false,
                stdout_truncated: false,
                stderr_truncated: false,
                terminal_reason: Some(redacted_bounded(reason, MAX_REASON_CHARS)),
                stderr_excerpt: None,
            },
            usage: None,
            repair_turns: None,
            session_id: None,
            final_output: None,
            patch: None,
            artifacts: SidecarArtifacts {
                prediction: None,
                sidecar: file_name(sidecar_file),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SidecarBenchmark {
    pub(crate) kind: super::BenchmarkKind,
    pub(crate) dataset: String,
    pub(crate) dataset_version: String,
    pub(crate) harness_commit: String,
    pub(crate) contract_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SidecarBonsai {
    pub(crate) declared_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) binary_version: Option<String>,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: String,
    pub(crate) autonomy: super::BenchmarkAutonomy,
    pub(crate) network: super::NetworkPolicy,
    pub(crate) budgets: super::BenchmarkBudgets,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SidecarProcess {
    pub(crate) exit_code: Option<i32>,
    pub(crate) elapsed_ms: u64,
    pub(crate) timed_out: bool,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) terminal_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stderr_excerpt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SidecarPatch {
    pub(crate) digest_algorithm: String,
    pub(crate) digest: String,
    pub(crate) bytes: usize,
    pub(crate) empty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SidecarArtifacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prediction: Option<String>,
    pub(crate) sidecar: String,
}

pub(crate) fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut body = serde_json::to_vec_pretty(value).context("Failed to serialize JSON artifact")?;
    body.push(b'\n');
    write_atomic(path, &body)
}

pub(crate) fn write_json_lines_atomic<T: Serialize>(path: &Path, values: &[T]) -> Result<()> {
    let mut body = Vec::new();
    for value in values {
        serde_json::to_writer(&mut body, value).context("Failed to serialize JSONL artifact")?;
        body.push(b'\n');
    }
    write_atomic(path, &body)
}

fn write_atomic(path: &Path, body: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Artifact path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create artifact directory {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "Failed to create temporary artifact in {}",
            parent.display()
        )
    })?;
    temporary
        .write_all(body)
        .with_context(|| format!("Failed to write temporary artifact for {}", path.display()))?;
    temporary
        .as_file()
        .sync_all()
        .with_context(|| format!("Failed to sync temporary artifact for {}", path.display()))?;
    temporary.persist(path).map_err(|error| {
        anyhow::anyhow!(
            "Failed to atomically persist {}: {}",
            path.display(),
            error.error
        )
    })?;
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| PathBuf::from(path).display().to_string())
}

fn redacted_bounded(value: &str, max_chars: usize) -> String {
    let redacted = crate::redact::redact(value);
    if redacted.chars().count() <= max_chars {
        return redacted.into_owned();
    }
    let mut bounded = redacted.chars().take(max_chars).collect::<String>();
    bounded.push_str("\n[truncated]");
    bounded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_and_bounds_apply_before_artifact_serialization() {
        let secret = format!("failure sk-{} tail", "a".repeat(40));
        let rendered = redacted_bounded(&secret, 200);
        assert!(!rendered.contains(&secret));
        assert!(rendered.contains("[REDACTED:OpenAI API key]"));

        let bounded = redacted_bounded(&"x".repeat(100), 10);
        assert_eq!(bounded, "xxxxxxxxxx\n[truncated]");
    }

    #[test]
    fn atomic_json_write_replaces_complete_artifact() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("artifact.json");
        write_json_atomic(&path, &serde_json::json!({"value": 1})).unwrap();
        write_json_atomic(&path, &serde_json::json!({"value": 2})).unwrap();
        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(value["value"], 2);
    }
}
