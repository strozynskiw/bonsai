//! Project verification profiles and focused verification-workflow prompts.

use std::path::Path;

use crate::config::VerificationConfig;
use crate::provider::ReasoningSelection;

/// When Bonsai may propose or start verification after an edit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerifyAfterEdit {
    #[default]
    Off,
    Ask,
    On,
}

impl VerifyAfterEdit {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ask => "ask",
            Self::On => "on",
        }
    }
}

/// Which configured verification lane a user requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationKind {
    Test,
    Build,
}

impl VerificationKind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Test => "test",
            Self::Build => "build",
        }
    }

    pub(crate) fn from_slash_command(input: &str) -> Option<Self> {
        match input.trim() {
            "/test" => Some(Self::Test),
            "/build" => Some(Self::Build),
            _ => None,
        }
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "test" => Some(Self::Test),
            "build" => Some(Self::Build),
            _ => None,
        }
    }
}

/// One ordered command in a resolved verification profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationCheck {
    pub(crate) name: String,
    pub(crate) command: String,
}

/// Resolved project checks after explicit config overrides and manifest detection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct VerificationProfile {
    pub(crate) tests: Vec<VerificationCheck>,
    pub(crate) builds: Vec<VerificationCheck>,
    pub(crate) after_edit: VerifyAfterEdit,
}

/// A resolved slash-command workflow ready to run through the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationWorkflow {
    pub(crate) kind: VerificationKind,
    pub(crate) checks: Vec<VerificationCheck>,
    pub(crate) prompt: String,
}

/// Persisted state of one configured check in a verification run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationCheckStatus {
    Pending,
    Passed,
    Failed,
    TimedOut,
}

impl VerificationCheckStatus {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::TimedOut => "timed_out",
        }
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "pending" => Some(Self::Pending),
            "passed" => Some(Self::Passed),
            "failed" => Some(Self::Failed),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }
}

/// Persisted terminal state of a verification workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VerificationRunStatus {
    Running,
    Passed,
    Failed,
    Blocked,
    Unstable,
    Stale,
    Incomplete,
    Interrupted,
}

impl VerificationRunStatus {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Unstable => "unstable",
            Self::Stale => "stale",
            Self::Incomplete => "incomplete",
            Self::Interrupted => "interrupted",
        }
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "running" => Some(Self::Running),
            "passed" => Some(Self::Passed),
            "failed" => Some(Self::Failed),
            "blocked" => Some(Self::Blocked),
            "unstable" => Some(Self::Unstable),
            "stale" => Some(Self::Stale),
            "incomplete" => Some(Self::Incomplete),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }
}

/// Evidence for one configured command, derived from the Bash tool result.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct VerificationCheckRecord {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) status: VerificationCheckStatus,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) completed_at_ms: Option<i64>,
    pub(crate) attempt_count: u32,
    pub(crate) last_failure_signature: Option<String>,
}

/// Evidence that a failed repair caused one request-local reasoning increase.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct VerificationReasoningEscalation {
    pub(crate) from: ReasoningSelection,
    pub(crate) to: ReasoningSelection,
    pub(crate) repair_attempt: u32,
    pub(crate) failure_signature: String,
    pub(crate) occurred_at_ms: i64,
}

/// Durable evidence for a `/test` or `/build` workflow, or for a directly run
/// Bash command that exactly matches a configured verification check.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct VerificationRunRecord {
    pub(crate) kind: VerificationKind,
    pub(crate) status: VerificationRunStatus,
    pub(crate) checks: Vec<VerificationCheckRecord>,
    pub(crate) started_at_ms: i64,
    pub(crate) finished_at_ms: Option<i64>,
    pub(crate) observed_final_workspace: Option<bool>,
    pub(crate) workspace_changes_after_last_check: Vec<String>,
    pub(crate) repair_attempts: u32,
    pub(crate) reasoning_escalations: Vec<VerificationReasoningEscalation>,
    pub(crate) terminal_reason: Option<String>,
}

impl VerificationRunRecord {
    pub(crate) fn running(kind: VerificationKind, checks: &[VerificationCheck]) -> Self {
        Self {
            kind,
            status: VerificationRunStatus::Running,
            checks: checks
                .iter()
                .map(|check| VerificationCheckRecord {
                    name: check.name.clone(),
                    command: check.command.clone(),
                    status: VerificationCheckStatus::Pending,
                    tool_call_id: None,
                    exit_code: None,
                    completed_at_ms: None,
                    attempt_count: 0,
                    last_failure_signature: None,
                })
                .collect(),
            started_at_ms: crate::util::time::now_ms(),
            finished_at_ms: None,
            observed_final_workspace: None,
            workspace_changes_after_last_check: Vec::new(),
            repair_attempts: 0,
            reasoning_escalations: Vec::new(),
            terminal_reason: None,
        }
    }
}

impl VerificationProfile {
    pub(crate) fn resolve(project_root: &Path, config: &VerificationConfig) -> Self {
        let detected = Self::detect(project_root);
        Self {
            tests: configured_checks(config.test.as_deref(), detected.tests, "test"),
            builds: configured_checks(config.build.as_deref(), detected.builds, "build"),
            after_edit: config.after_edit,
        }
    }

    pub(crate) fn checks(&self, kind: VerificationKind) -> &[VerificationCheck] {
        match kind {
            VerificationKind::Test => &self.tests,
            VerificationKind::Build => &self.builds,
        }
    }

    pub(crate) fn workflow_prompt(&self, kind: VerificationKind) -> Result<String, String> {
        let checks = self.checks(kind);
        if checks.is_empty() {
            return Err(format!(
                "No {} verification checks were detected or configured. Add `[verification].{}` commands to .bonsai/config.toml.",
                kind.label(),
                kind.label(),
            ));
        }

        let ordered = checks
            .iter()
            .enumerate()
            .map(|(index, check)| format!("{}. {}: {:?}", index + 1, check.name, check.command))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(format!(
            "Run the configured {} verification profile against the current workspace.\n\nOrdered checks:\n{}\n\nRules:\n- Run each command once, in order, through the bash tool so normal permissions, sandboxing, hooks, cancellation, and evidence capture apply.\n- Before a failure, do not edit files, install dependencies, or change configuration.\n- A failed check produces a typed recovery event. Follow only that event: make its bounded focused repair or perform its one no-change flaky rerun, then rerun the exact failed command.\n- Do not advance to later checks until the failed check passes. Bonsai stops repeated deterministic failures and exhausted repair budgets.\n- Record the final git status after the checks so the report states whether they observed the current workspace.\n- Finish with a concise pass/fail report listing every executed and skipped check; never describe an unstable pass as stable.",
            kind.label(),
            ordered,
        ))
    }

    fn detect(project_root: &Path) -> Self {
        let mut profile = Self::default();
        if project_root.join("Cargo.toml").is_file() {
            let locked = project_root.join("Cargo.lock").is_file();
            let locked_arg = if locked { " --locked" } else { "" };
            profile.tests.push(VerificationCheck {
                name: "Rust tests".to_string(),
                command: format!("cargo test{locked_arg}"),
            });
            profile.builds.push(VerificationCheck {
                name: "Rust build".to_string(),
                command: format!("cargo build{locked_arg}"),
            });
        }
        if let Some(scripts) = package_scripts(project_root) {
            let runner = node_package_runner(project_root);
            if scripts.contains("test") {
                profile.tests.push(VerificationCheck {
                    name: "JavaScript tests".to_string(),
                    command: runner.script_command("test"),
                });
            }
            if scripts.contains("build") {
                profile.builds.push(VerificationCheck {
                    name: "JavaScript build".to_string(),
                    command: runner.script_command("build"),
                });
            }
        }
        if project_root.join("go.mod").is_file() {
            profile.tests.push(VerificationCheck {
                name: "Go tests".to_string(),
                command: "go test ./...".to_string(),
            });
            profile.builds.push(VerificationCheck {
                name: "Go build".to_string(),
                command: "go build ./...".to_string(),
            });
        }
        if let Some(project) = python_project(project_root) {
            if project.has_pytest {
                profile.tests.push(VerificationCheck {
                    name: "Python tests".to_string(),
                    command: "python -m pytest".to_string(),
                });
            }
            if project.has_build_system {
                profile.builds.push(VerificationCheck {
                    name: "Python build".to_string(),
                    command: "python -m build".to_string(),
                });
            }
        }
        profile
    }
}

pub(crate) fn resolve_slash_command(
    input: &str,
    project_root: &Path,
    config: &VerificationConfig,
) -> Result<Option<VerificationWorkflow>, String> {
    let Some(kind) = VerificationKind::from_slash_command(input) else {
        return Ok(None);
    };
    let profile = VerificationProfile::resolve(project_root, config);
    let prompt = profile.workflow_prompt(kind)?;
    Ok(Some(VerificationWorkflow {
        kind,
        checks: profile.checks(kind).to_vec(),
        prompt,
    }))
}

#[derive(Debug, Clone, Copy)]
enum NodePackageRunner {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl NodePackageRunner {
    fn script_command(self, script: &str) -> String {
        match (self, script) {
            (Self::Npm, "test") => "npm test".to_string(),
            (Self::Npm, script) => format!("npm run {script}"),
            (Self::Pnpm, script) => format!("pnpm {script}"),
            (Self::Yarn, script) => format!("yarn {script}"),
            (Self::Bun, script) => format!("bun run {script}"),
        }
    }
}

fn node_package_runner(project_root: &Path) -> NodePackageRunner {
    if project_root.join("pnpm-lock.yaml").is_file() {
        NodePackageRunner::Pnpm
    } else if project_root.join("yarn.lock").is_file() {
        NodePackageRunner::Yarn
    } else if project_root.join("bun.lock").is_file() || project_root.join("bun.lockb").is_file() {
        NodePackageRunner::Bun
    } else {
        NodePackageRunner::Npm
    }
}

fn package_scripts(project_root: &Path) -> Option<std::collections::BTreeSet<String>> {
    let content = std::fs::read_to_string(project_root.join("package.json")).ok()?;
    let document = serde_json::from_str::<serde_json::Value>(&content).ok()?;
    Some(
        document
            .get("scripts")?
            .as_object()?
            .keys()
            .cloned()
            .collect(),
    )
}

#[derive(Debug, Clone, Copy)]
struct PythonProject {
    has_pytest: bool,
    has_build_system: bool,
}

fn python_project(project_root: &Path) -> Option<PythonProject> {
    let pyproject_path = project_root.join("pyproject.toml");
    let document = std::fs::read_to_string(&pyproject_path)
        .ok()
        .and_then(|content| toml::from_str::<toml::Value>(&content).ok());
    let has_python_marker = pyproject_path.is_file()
        || project_root.join("pytest.ini").is_file()
        || project_root.join("setup.cfg").is_file()
        || project_root.join("setup.py").is_file()
        || project_root.join("requirements.txt").is_file();
    if !has_python_marker {
        return None;
    }
    let has_pytest = document
        .as_ref()
        .and_then(|document| document.get("tool"))
        .and_then(|tool| tool.get("pytest"))
        .is_some()
        || project_root.join("pytest.ini").is_file()
        || project_root.join("tests").is_dir();
    Some(PythonProject {
        has_pytest,
        has_build_system: document
            .as_ref()
            .and_then(|document| document.get("build-system"))
            .is_some(),
    })
}

fn configured_checks(
    configured: Option<&[String]>,
    detected: Vec<VerificationCheck>,
    kind: &str,
) -> Vec<VerificationCheck> {
    let Some(configured) = configured else {
        return detected;
    };
    configured
        .iter()
        .map(|command| command.trim())
        .filter(|command| !command.is_empty())
        .enumerate()
        .map(|(index, command)| VerificationCheck {
            name: format!("Configured {kind} {}", index + 1),
            command: command.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_locked_rust_profiles() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        std::fs::write(root.path().join("Cargo.lock"), "").unwrap();

        let profile = VerificationProfile::resolve(root.path(), &VerificationConfig::default());

        assert_eq!(profile.tests[0].command, "cargo test --locked");
        assert_eq!(profile.builds[0].command, "cargo build --locked");
        assert_eq!(profile.after_edit, VerifyAfterEdit::Off);
    }

    #[test]
    fn explicit_order_replaces_detection_and_blank_list_disables_lane() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        let config = VerificationConfig {
            test: Some(vec![
                "cargo test unit".into(),
                "cargo test integration".into(),
            ]),
            build: Some(Vec::new()),
            after_edit: VerifyAfterEdit::On,
        };

        let profile = VerificationProfile::resolve(root.path(), &config);

        assert_eq!(
            profile
                .tests
                .iter()
                .map(|check| check.command.as_str())
                .collect::<Vec<_>>(),
            ["cargo test unit", "cargo test integration"]
        );
        assert!(profile.builds.is_empty());
        assert_eq!(profile.after_edit, VerifyAfterEdit::On);
    }

    #[test]
    fn workflow_prompt_is_bounded_and_names_skipped_checks() {
        let profile = VerificationProfile {
            tests: vec![VerificationCheck {
                name: "unit".into(),
                command: "cargo test --locked".into(),
            }],
            ..VerificationProfile::default()
        };

        let prompt = profile.workflow_prompt(VerificationKind::Test).unwrap();

        assert!(prompt.contains("Run each command once"));
        assert!(prompt.contains("typed recovery event"));
        assert!(prompt.contains("stops repeated deterministic failures"));
        assert!(prompt.contains("executed and skipped check"));
    }

    #[test]
    fn detects_node_go_and_python_in_stable_order() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"test":"vitest","build":"tsc"}}"#,
        )
        .unwrap();
        std::fs::write(root.path().join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(root.path().join("go.mod"), "module example.test/x\n").unwrap();
        std::fs::create_dir(root.path().join("tests")).unwrap();
        std::fs::write(
            root.path().join("pyproject.toml"),
            "[build-system]\nrequires=[]\n",
        )
        .unwrap();

        let profile = VerificationProfile::resolve(root.path(), &VerificationConfig::default());

        assert_eq!(
            profile
                .tests
                .iter()
                .map(|check| check.command.as_str())
                .collect::<Vec<_>>(),
            ["pnpm test", "go test ./...", "python -m pytest"]
        );
        assert_eq!(
            profile
                .builds
                .iter()
                .map(|check| check.command.as_str())
                .collect::<Vec<_>>(),
            ["pnpm build", "go build ./...", "python -m build"]
        );
    }

    #[test]
    fn slash_expansion_is_exact_and_preserves_other_prompts() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname='x'\n").unwrap();

        assert!(
            resolve_slash_command("/test", root.path(), &VerificationConfig::default())
                .unwrap()
                .is_some()
        );
        assert!(
            resolve_slash_command("/test unit", root.path(), &VerificationConfig::default())
                .unwrap()
                .is_none()
        );
    }
}
