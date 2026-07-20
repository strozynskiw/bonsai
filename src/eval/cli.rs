use std::fmt;
use std::path::PathBuf;

use serde::Serialize;

const DEFAULT_SUITE_PATH: &str = "eval/suites/m2_10.toml";
const DEFAULT_OUT_DIR: &str = "target/eval";

/// Provider backend an eval run targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EvalMode {
    /// Deterministic in-process mock provider driven by each task's mock script.
    Mock,
    /// A real, configured provider selected from the user's catalog.
    Live,
}

impl EvalMode {
    /// Parse a `--mode` CLI value, returning `None` for anything unrecognized.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "mock" => Some(Self::Mock),
            "live" => Some(Self::Live),
            _ => None,
        }
    }
}

impl fmt::Display for EvalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mock => "mock",
            Self::Live => "live",
        })
    }
}

/// Fully parsed configuration for a single `bonsai eval` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EvalCliConfig {
    /// Path to the suite TOML file to run.
    pub suite: PathBuf,
    /// Provider backend (mock or live).
    pub mode: EvalMode,
    /// Optional live-provider override (only valid in live mode).
    pub provider: Option<String>,
    /// Optional live-model override, resolved with the normal `/model` rules.
    pub model: Option<String>,
    /// Optional live reasoning override for the selected model.
    pub effort: Option<crate::provider::ReasoningSelection>,
    /// Optional single-task filter; runs the whole suite when `None`.
    pub task: Option<String>,
    /// Optional seed override; falls back to the suite's declared seed.
    pub seed: Option<u64>,
    /// Directory the per-run output folder is created under.
    pub out_dir: PathBuf,
    /// Optional versioned baseline; selecting it enables regression gating.
    pub baseline: Option<PathBuf>,
    /// When set, pretty-prints the JSON report to stdout.
    pub json: bool,
    /// Return a non-zero process status when any task or budget fails.
    pub fail_on_task_failure: bool,
}

impl Default for EvalCliConfig {
    fn default() -> Self {
        Self {
            suite: PathBuf::from(DEFAULT_SUITE_PATH),
            mode: EvalMode::Mock,
            provider: None,
            model: None,
            effort: None,
            task: None,
            seed: None,
            out_dir: PathBuf::from(DEFAULT_OUT_DIR),
            baseline: None,
            json: false,
            fail_on_task_failure: false,
        }
    }
}
