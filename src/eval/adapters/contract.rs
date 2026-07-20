use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub(crate) const ADAPTER_SCHEMA_VERSION: u32 = 1;
pub(crate) const SWE_BENCH_HARNESS_COMMIT: &str = "f7bbbb2ccdf479001d6467c9e34af59e44a840f9";
pub(crate) const SWE_BENCH_PREDICTION_SCHEMA_COMMIT: &str =
    "b679692b8b7e274a6c89fd0842f25b02da4b9256";
pub(crate) const HARBOR_HARNESS_COMMIT: &str = "2d3f78d55a703df2f76c005d7df44a5ce2d8adf5";
pub(crate) const TERMINAL_BENCH_2_DATASET_COMMIT: &str = "2fd12b88aafdd04a52c298e3940bcb189f9766d6";

const MAX_INSTRUCTION_BYTES: usize = 1_000_000;
const MAX_TASK_ID_BYTES: usize = 200;
const MAX_TURNS: usize = 10_000;
const MAX_DURATION_SECONDS: u64 = 7 * 24 * 60 * 60;
const MAX_OUTPUT_CHARS: usize = 64 * 1024 * 1024;
const MAX_PATCH_BYTES: usize = 16 * 1024 * 1024;

/// One versioned request for an externally-owned benchmark harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AdapterRequest {
    pub(crate) schema_version: u32,
    pub(crate) benchmark: BenchmarkPin,
    pub(crate) task: BenchmarkTask,
    pub(crate) runner: BenchmarkRunner,
}

impl AdapterRequest {
    /// Validate a request before any process or workspace action is attempted.
    ///
    /// # Errors
    ///
    /// Returns an error for unknown schemas or upstream pins, unsafe task ids,
    /// missing benchmark fields, or unbounded runner settings.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema_version != ADAPTER_SCHEMA_VERSION {
            anyhow::bail!(
                "Benchmark adapter request uses schema version {}; supported version is {}",
                self.schema_version,
                ADAPTER_SCHEMA_VERSION
            );
        }
        self.benchmark.validate()?;
        self.task.validate(self.benchmark.kind)?;
        self.runner.validate()?;
        Ok(())
    }

    pub(crate) fn resolve_paths(&mut self, base_dir: &Path) {
        if self.task.workspace.is_relative() {
            self.task.workspace = base_dir.join(&self.task.workspace);
        }
        if self.runner.bonsai_binary.is_relative() {
            self.runner.bonsai_binary = base_dir.join(&self.runner.bonsai_binary);
        }
    }

    pub(crate) fn request_key(&self) -> Result<String> {
        #[derive(Serialize)]
        struct KeyMaterial<'a> {
            schema_version: u32,
            benchmark: &'a BenchmarkPin,
            task_id: &'a str,
            base_commit: Option<&'a str>,
            instruction_digest: String,
            bonsai_revision: &'a str,
            provider: &'a str,
            model: &'a str,
            reasoning_effort: &'a str,
            autonomy: BenchmarkAutonomy,
            network: NetworkPolicy,
            budgets: BenchmarkBudgets,
        }

        let material = KeyMaterial {
            schema_version: self.schema_version,
            benchmark: &self.benchmark,
            task_id: &self.task.id,
            base_commit: self.task.base_commit.as_deref(),
            instruction_digest: blake3::hash(self.task.instruction.as_bytes())
                .to_hex()
                .to_string(),
            bonsai_revision: &self.runner.bonsai_revision,
            provider: &self.runner.provider,
            model: &self.runner.model,
            reasoning_effort: &self.runner.reasoning_effort,
            autonomy: self.runner.autonomy,
            network: self.runner.network,
            budgets: self.runner.budgets,
        };
        let encoded = serde_json::to_vec(&material)
            .context("Failed to serialize benchmark request identity")?;
        Ok(blake3::hash(&encoded).to_hex().to_string())
    }

    pub(crate) fn model_name_or_path(&self) -> String {
        format!(
            "bonsai/{}/{}/{}/{}",
            self.runner.bonsai_revision,
            self.runner.provider,
            self.runner.model,
            self.runner.reasoning_effort
        )
    }
}

/// Exact externally-owned benchmark and schema versions supported by the adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BenchmarkPin {
    pub(crate) kind: BenchmarkKind,
    pub(crate) dataset: String,
    pub(crate) dataset_version: String,
    pub(crate) harness_commit: String,
    pub(crate) contract_commit: String,
}

impl BenchmarkPin {
    fn validate(&self) -> Result<()> {
        require_text("benchmark.dataset", &self.dataset)?;
        require_text("benchmark.dataset_version", &self.dataset_version)?;
        match self.kind {
            BenchmarkKind::SweBenchVerified => {
                require_pin(
                    "SWE-bench harness",
                    &self.harness_commit,
                    SWE_BENCH_HARNESS_COMMIT,
                )?;
                require_pin(
                    "SWE-bench prediction schema",
                    &self.contract_commit,
                    SWE_BENCH_PREDICTION_SCHEMA_COMMIT,
                )?;
            }
            BenchmarkKind::TerminalBench2 => {
                require_pin(
                    "Harbor harness",
                    &self.harness_commit,
                    HARBOR_HARNESS_COMMIT,
                )?;
                require_pin(
                    "Terminal-Bench 2 dataset",
                    &self.contract_commit,
                    TERMINAL_BENCH_2_DATASET_COMMIT,
                )?;
            }
        }
        Ok(())
    }
}

/// Externally-owned benchmark family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BenchmarkKind {
    SweBenchVerified,
    TerminalBench2,
}

/// Prepared task workspace and instruction supplied by the official harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BenchmarkTask {
    pub(crate) id: String,
    pub(crate) workspace: PathBuf,
    pub(crate) instruction: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) base_commit: Option<String>,
}

impl BenchmarkTask {
    fn validate(&self, kind: BenchmarkKind) -> Result<()> {
        validate_task_id(&self.id)?;
        if self.workspace.as_os_str().is_empty() {
            anyhow::bail!("task.workspace is required");
        }
        if self.instruction.trim().is_empty() {
            anyhow::bail!("task.instruction is required");
        }
        if self.instruction.len() > MAX_INSTRUCTION_BYTES {
            anyhow::bail!(
                "task.instruction is {} bytes; maximum is {}",
                self.instruction.len(),
                MAX_INSTRUCTION_BYTES
            );
        }
        match (kind, self.base_commit.as_deref()) {
            (BenchmarkKind::SweBenchVerified, Some(value)) if !value.trim().is_empty() => {
                validate_git_object_id(value)?;
            }
            (BenchmarkKind::SweBenchVerified, _) => {
                anyhow::bail!("SWE-bench tasks require task.base_commit")
            }
            (BenchmarkKind::TerminalBench2, Some(_)) => {
                anyhow::bail!("Terminal-Bench 2 tasks must not set task.base_commit")
            }
            (BenchmarkKind::TerminalBench2, None) => {}
        }
        Ok(())
    }
}

/// Explicit Bonsai binary, model, policy, and budget settings for one run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BenchmarkRunner {
    pub(crate) bonsai_binary: PathBuf,
    pub(crate) bonsai_revision: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) reasoning_effort: String,
    pub(crate) autonomy: BenchmarkAutonomy,
    pub(crate) network: NetworkPolicy,
    pub(crate) budgets: BenchmarkBudgets,
}

impl BenchmarkRunner {
    fn validate(&self) -> Result<()> {
        if self.bonsai_binary.as_os_str().is_empty() {
            anyhow::bail!("runner.bonsai_binary is required");
        }
        require_text("runner.bonsai_revision", &self.bonsai_revision)?;
        require_text("runner.provider", &self.provider)?;
        require_text("runner.model", &self.model)?;
        let reasoning = crate::provider::ReasoningSelection::parse(&self.reasoning_effort)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "runner.reasoning_effort '{}' is not supported",
                    self.reasoning_effort
                )
            })?;
        if matches!(
            reasoning,
            crate::provider::ReasoningSelection::BudgetTokens(_)
        ) {
            anyhow::bail!(
                "runner.reasoning_effort must be a portable effort label, not a token budget"
            );
        }
        self.budgets.validate()
    }
}

/// Non-interactive authorization ceiling passed explicitly to Bonsai.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BenchmarkAutonomy {
    Ask,
    Conservative,
    Balanced,
    AutoAccept,
    Yolo,
}

impl BenchmarkAutonomy {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Conservative => "conservative",
            Self::Balanced => "balanced",
            Self::AutoAccept => "auto-accept",
            Self::Yolo => "yolo",
        }
    }
}

/// Requested sandbox network posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NetworkPolicy {
    Deny,
    Allow,
}

impl NetworkPolicy {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Allow => "allow",
        }
    }
}

/// Every bounded resource accepted by the headless benchmark launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BenchmarkBudgets {
    pub(crate) max_turns: usize,
    pub(crate) max_generation_seconds: u64,
    pub(crate) max_output_chars: usize,
    pub(crate) max_tool_seconds: u64,
    pub(crate) timeout_seconds: u64,
    pub(crate) max_patch_bytes: usize,
}

impl BenchmarkBudgets {
    fn validate(self) -> Result<()> {
        require_bounded("max_turns", self.max_turns, MAX_TURNS)?;
        require_bounded("max_output_chars", self.max_output_chars, MAX_OUTPUT_CHARS)?;
        require_bounded("max_patch_bytes", self.max_patch_bytes, MAX_PATCH_BYTES)?;
        for (name, value) in [
            ("max_generation_seconds", self.max_generation_seconds),
            ("max_tool_seconds", self.max_tool_seconds),
            ("timeout_seconds", self.timeout_seconds),
        ] {
            if value == 0 || value > MAX_DURATION_SECONDS {
                anyhow::bail!("runner.budgets.{name} must be between 1 and {MAX_DURATION_SECONDS}");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AdapterRequestFile {
    One(Box<AdapterRequest>),
    Many(Vec<AdapterRequest>),
}

/// Load one request or a request array, resolve relative paths, and validate
/// every contract before any child process starts.
pub(crate) fn load_requests(path: &Path) -> Result<Vec<AdapterRequest>> {
    let body = fs::read_to_string(path)
        .with_context(|| format!("Failed to read benchmark request {}", path.display()))?;
    let parsed: AdapterRequestFile = serde_json::from_str(&body)
        .with_context(|| format!("Failed to parse benchmark request {}", path.display()))?;
    let mut requests = match parsed {
        AdapterRequestFile::One(request) => vec![*request],
        AdapterRequestFile::Many(requests) => requests,
    };
    if requests.is_empty() {
        anyhow::bail!("Benchmark request array must not be empty");
    }
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut keys = HashSet::new();
    for request in &mut requests {
        request.resolve_paths(base_dir);
        request.validate()?;
        let key = request.request_key()?;
        if !keys.insert(key) {
            anyhow::bail!("Benchmark request file contains a duplicate task/configuration");
        }
    }
    Ok(requests)
}

fn require_pin(name: &str, actual: &str, expected: &str) -> Result<()> {
    if actual != expected {
        anyhow::bail!("Unsupported {name} commit '{actual}'; pinned commit is '{expected}'");
    }
    Ok(())
}

fn require_text(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        anyhow::bail!("{name} is required");
    }
    Ok(())
}

fn require_bounded(name: &str, value: usize, maximum: usize) -> Result<()> {
    if value == 0 || value > maximum {
        anyhow::bail!("runner.budgets.{name} must be between 1 and {maximum}");
    }
    Ok(())
}

fn validate_task_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_TASK_ID_BYTES
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        anyhow::bail!(
            "task.id must be 1-{MAX_TASK_ID_BYTES} ASCII letters, digits, '.', '_', or '-'"
        );
    }
    Ok(())
}

fn validate_git_object_id(value: &str) -> Result<()> {
    if !(7..=64).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("task.base_commit must be a 7-64 character hexadecimal Git object id");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: BenchmarkKind) -> AdapterRequest {
        let (dataset, version, harness, contract, base_commit) = match kind {
            BenchmarkKind::SweBenchVerified => (
                "swe-bench_verified",
                "test",
                SWE_BENCH_HARNESS_COMMIT,
                SWE_BENCH_PREDICTION_SCHEMA_COMMIT,
                Some("0123456789abcdef".to_string()),
            ),
            BenchmarkKind::TerminalBench2 => (
                "terminal-bench",
                "2.0",
                HARBOR_HARNESS_COMMIT,
                TERMINAL_BENCH_2_DATASET_COMMIT,
                None,
            ),
        };
        AdapterRequest {
            schema_version: ADAPTER_SCHEMA_VERSION,
            benchmark: BenchmarkPin {
                kind,
                dataset: dataset.to_string(),
                dataset_version: version.to_string(),
                harness_commit: harness.to_string(),
                contract_commit: contract.to_string(),
            },
            task: BenchmarkTask {
                id: "owner__repo-123".to_string(),
                workspace: PathBuf::from("workspace"),
                instruction: "Fix the bug.".to_string(),
                base_commit,
            },
            runner: BenchmarkRunner {
                bonsai_binary: PathBuf::from("bonsai"),
                bonsai_revision: "deadbeef".to_string(),
                provider: "openai".to_string(),
                model: "gpt-test".to_string(),
                reasoning_effort: "high".to_string(),
                autonomy: BenchmarkAutonomy::AutoAccept,
                network: NetworkPolicy::Deny,
                budgets: BenchmarkBudgets {
                    max_turns: 64,
                    max_generation_seconds: 300,
                    max_output_chars: 200_000,
                    max_tool_seconds: 120,
                    timeout_seconds: 1_800,
                    max_patch_bytes: 1_000_000,
                },
            },
        }
    }

    #[test]
    fn supported_pins_validate_and_unknown_schema_fails() {
        let mut request = request(BenchmarkKind::SweBenchVerified);
        request.validate().unwrap();
        request.schema_version += 1;
        assert!(
            request
                .validate()
                .unwrap_err()
                .to_string()
                .contains("schema version")
        );
    }

    #[test]
    fn upstream_commit_changes_fail_before_launch() {
        let mut request = request(BenchmarkKind::TerminalBench2);
        request.benchmark.harness_commit = "new-upstream-head".to_string();
        let error = request.validate().unwrap_err().to_string();
        assert!(
            error.contains("Unsupported Harbor harness commit"),
            "{error}"
        );
    }

    #[test]
    fn request_key_ignores_machine_paths_but_tracks_configuration() {
        let mut first = request(BenchmarkKind::SweBenchVerified);
        let mut second = first.clone();
        second.task.workspace = PathBuf::from("/another/workspace");
        second.runner.bonsai_binary = PathBuf::from("/another/bonsai");
        assert_eq!(first.request_key().unwrap(), second.request_key().unwrap());

        first.runner.model = "different".to_string();
        assert_ne!(first.request_key().unwrap(), second.request_key().unwrap());
    }

    #[test]
    fn task_ids_and_budget_values_are_bounded() {
        let mut request = request(BenchmarkKind::SweBenchVerified);
        request.task.id = "../escape".to_string();
        assert!(request.validate().is_err());
        request.task.id = "safe".to_string();
        request.runner.budgets.max_turns = 0;
        assert!(request.validate().is_err());
    }

    #[test]
    fn invalid_swe_base_commit_fails_contract_validation() {
        let mut request = request(BenchmarkKind::SweBenchVerified);
        request.task.base_commit = Some("HEAD~1".to_string());
        let error = request.validate().unwrap_err().to_string();
        assert!(error.contains("task.base_commit"), "{error}");
    }
}
