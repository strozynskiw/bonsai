use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::timeout;

use super::*;

const DEFAULT_GRADER_TIMEOUT_SECS: u64 = 30;

/// A single grader declaration parsed from a suite's `[[tasks.graders]]` table.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub(crate) enum GraderSpec {
    /// Run a shell command in the worktree; passes on exit code 0.
    TestPass {
        command: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// Assert facts about a file in the worktree (existence/content/exact match).
    FileState {
        path: String,
        #[serde(default)]
        exists: Option<bool>,
        #[serde(default)]
        contains: Vec<String>,
        #[serde(default)]
        not_contains: Vec<String>,
        #[serde(default)]
        exact_file: Option<String>,
    },
    /// Assert substrings present/absent in the assistant's final output.
    Assertion {
        #[serde(default)]
        contains: Vec<String>,
        #[serde(default)]
        not_contains: Vec<String>,
    },
}

impl GraderSpec {
    /// Validate this grader's declaration at suite-load time.
    ///
    /// # Errors
    /// Returns an error if the grader is malformed (empty command, unsafe path,
    /// missing `exact_file`, or a check-less assertion).
    pub(crate) fn validate(&self, suite_base_dir: &Path, task_id: &str) -> Result<()> {
        match self {
            Self::TestPass { command, .. } => {
                if command.trim().is_empty() {
                    anyhow::bail!("Task '{task_id}' has a test-pass grader with an empty command");
                }
            }
            Self::FileState {
                path, exact_file, ..
            } => {
                SafeRelativePath::parse(path, "file-state path")
                    .with_context(|| format!("Invalid file-state path in task '{task_id}'"))?;
                if let Some(exact_file) = exact_file {
                    let expected =
                        SafeRelativePath::parse(exact_file, "exact_file")?.join(suite_base_dir);
                    if !expected.is_file() {
                        anyhow::bail!(
                            "Task '{task_id}' exact_file does not exist: {}",
                            expected.display()
                        );
                    }
                }
            }
            Self::Assertion {
                contains,
                not_contains,
            } => {
                if contains.is_empty() && not_contains.is_empty() {
                    anyhow::bail!("Task '{task_id}' assertion grader has no checks");
                }
            }
        }
        Ok(())
    }

    /// Execute this grader against a finished task, producing a [`GraderResult`].
    pub(crate) async fn grade(
        &self,
        worktree: &Path,
        suite_base_dir: &Path,
        assistant_output: &str,
    ) -> GraderResult {
        let started = Instant::now();
        let (grader_type, passed, details) = match self {
            Self::TestPass {
                command,
                timeout_secs,
            } => {
                let timeout_secs = timeout_secs.unwrap_or(DEFAULT_GRADER_TIMEOUT_SECS).max(1);
                let result = run_grader_command(worktree, command, timeout_secs).await;
                ("test-pass", result.passed, result.details)
            }
            Self::FileState {
                path,
                exists,
                contains,
                not_contains,
                exact_file,
            } => {
                let result = grade_file_state(
                    worktree,
                    suite_base_dir,
                    path,
                    *exists,
                    contains,
                    not_contains,
                    exact_file.as_deref(),
                );
                ("file-state", result.passed, result.details)
            }
            Self::Assertion {
                contains,
                not_contains,
            } => {
                let result = grade_assertion(assistant_output, contains, not_contains);
                ("assertion", result.passed, result.details)
            }
        };
        GraderResult {
            grader_type: grader_type.to_string(),
            passed,
            details,
            duration_ms: millis_u64(started.elapsed()),
        }
    }
}

/// Serialized outcome of running one grader against a finished task.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GraderResult {
    /// Grader discriminant (`test-pass` / `file-state` / `assertion`).
    #[serde(rename = "type")]
    pub(crate) grader_type: String,
    /// Whether the grader passed.
    pub(crate) passed: bool,
    /// Human-readable explanation of the outcome.
    pub(crate) details: String,
    /// Wall-clock grading duration in milliseconds.
    pub(crate) duration_ms: u64,
}

/// Run every grader for a task in declaration order, collecting their results.
pub(crate) async fn grade_task(
    graders: &[GraderSpec],
    worktree: &Path,
    suite_base_dir: &Path,
    assistant_output: &str,
) -> Vec<GraderResult> {
    let mut results = Vec::with_capacity(graders.len());
    for grader in graders {
        results.push(
            grader
                .grade(worktree, suite_base_dir, assistant_output)
                .await,
        );
    }
    results
}

/// Validate a task id is usable as a worktree directory name.
///
/// # Errors
/// Returns an error if the id is blank, reserved (`.`/`..`), or contains
/// characters outside `[A-Za-z0-9._-]`.
pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.trim().is_empty() {
        anyhow::bail!("Task id is required");
    }
    if matches!(id, "." | "..") {
        anyhow::bail!("Task id '{id}' is reserved and cannot be used as a worktree directory");
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        anyhow::bail!("Task id '{id}' may only contain ASCII letters, numbers, '.', '_' and '-'");
    }
    Ok(())
}

/// Pass/fail verdict plus an explanatory detail string, produced by the
/// individual grading helpers before being folded into a [`GraderResult`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct GradeOutcome {
    passed: bool,
    details: String,
}

impl GradeOutcome {
    fn pass(details: impl Into<String>) -> Self {
        Self {
            passed: true,
            details: details.into(),
        }
    }

    fn fail(details: impl Into<String>) -> Self {
        Self {
            passed: false,
            details: details.into(),
        }
    }
}

/// Where a [`check_needles`] scan is running, used to format failure details.
enum NeedleContext<'a> {
    /// A worktree file at the given relative path.
    File(&'a str),
    /// The assistant's final output.
    AssistantOutput,
}

impl NeedleContext<'_> {
    fn missing(&self, needle: &str) -> String {
        match self {
            Self::File(path) => format!("missing expected text in {path}: {needle:?}"),
            Self::AssistantOutput => {
                format!("assistant output missing expected text: {needle:?}")
            }
        }
    }

    fn forbidden(&self, needle: &str) -> String {
        match self {
            Self::File(path) => format!("found forbidden text in {path}: {needle:?}"),
            Self::AssistantOutput => {
                format!("assistant output included forbidden text: {needle:?}")
            }
        }
    }
}

/// Check that every `contains` needle is present in `haystack` and every
/// `not_contains` needle is absent, returning the first failing outcome (or
/// `None` when all checks pass).
fn check_needles(
    haystack: &str,
    contains: &[String],
    not_contains: &[String],
    context: NeedleContext<'_>,
) -> Option<GradeOutcome> {
    for needle in contains {
        if !haystack.contains(needle) {
            return Some(GradeOutcome::fail(context.missing(needle)));
        }
    }
    for needle in not_contains {
        if haystack.contains(needle) {
            return Some(GradeOutcome::fail(context.forbidden(needle)));
        }
    }
    None
}

async fn run_grader_command(worktree: &Path, command: &str, timeout_secs: u64) -> GradeOutcome {
    let mut command = shell_command(command);
    command
        .current_dir(worktree)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return GradeOutcome::fail(format!("failed to execute command: {err}")),
    };
    let result = timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;
    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let code = output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let passed = output.status.success();
            let details = if passed {
                "command exited 0".to_string()
            } else {
                truncate_detail(&format!(
                    "command exited {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                ))
            };
            GradeOutcome { passed, details }
        }
        Ok(Err(err)) => GradeOutcome::fail(format!("failed to wait for command: {err}")),
        Err(_) => GradeOutcome::fail(format!("command timed out after {timeout_secs}s")),
    }
}

fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", command]);
        cmd
    }

    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = Command::new(shell);
        cmd.arg("-c").arg(command);
        cmd
    }
}

fn grade_file_state(
    worktree: &Path,
    suite_base_dir: &Path,
    path: &str,
    exists: Option<bool>,
    contains: &[String],
    not_contains: &[String],
    exact_file: Option<&str>,
) -> GradeOutcome {
    let target = match SafeRelativePath::parse(path, "file-state path") {
        Ok(relative) => relative.join(worktree),
        Err(err) => return GradeOutcome::fail(err.to_string()),
    };
    let should_exist = exists.unwrap_or_else(|| {
        !contains.is_empty() || !not_contains.is_empty() || exact_file.is_some()
    });
    if !target.exists() {
        return GradeOutcome {
            passed: !should_exist,
            details: if should_exist {
                format!("missing expected path: {path}")
            } else {
                format!("path is missing as expected: {path}")
            },
        };
    }
    if !should_exist {
        return GradeOutcome::fail(format!("path exists but should be missing: {path}"));
    }
    if contains.is_empty() && not_contains.is_empty() && exact_file.is_none() {
        return GradeOutcome::pass(format!("path exists: {path}"));
    }

    let content = match fs::read_to_string(&target) {
        Ok(content) => content,
        Err(err) => return GradeOutcome::fail(format!("failed to read {path}: {err}")),
    };
    if let Some(outcome) =
        check_needles(&content, contains, not_contains, NeedleContext::File(path))
    {
        return outcome;
    }
    if let Some(exact_file) = exact_file {
        let expected_path = match SafeRelativePath::parse(exact_file, "exact_file") {
            Ok(relative) => relative.join(suite_base_dir),
            Err(err) => return GradeOutcome::fail(err.to_string()),
        };
        let expected = match fs::read_to_string(&expected_path) {
            Ok(content) => content,
            Err(err) => {
                return GradeOutcome::fail(format!(
                    "failed to read expected file {}: {err}",
                    exact_file
                ));
            }
        };
        if content != expected {
            return GradeOutcome::fail(format!("{path} does not match expected file {exact_file}"));
        }
    }
    GradeOutcome::pass(format!("file-state checks passed for {path}"))
}

fn grade_assertion(
    assistant_output: &str,
    contains: &[String],
    not_contains: &[String],
) -> GradeOutcome {
    check_needles(
        assistant_output,
        contains,
        not_contains,
        NeedleContext::AssistantOutput,
    )
    .unwrap_or_else(|| GradeOutcome::pass("assistant output assertions passed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn reserved_task_ids_are_rejected() {
        assert!(validate_id("task.ok").is_ok());
        assert!(validate_id(".").is_err());
        assert!(validate_id("..").is_err());
    }

    #[tokio::test]
    async fn test_pass_grader_reports_pass_fail_and_timeout() {
        let temp = tempfile::TempDir::new().unwrap();

        let pass = run_grader_command(temp.path(), "true", 5).await;
        assert!(pass.passed);

        let fail = run_grader_command(temp.path(), "false", 5).await;
        assert!(!fail.passed);
        assert!(fail.details.contains("command exited"));

        let timeout = run_grader_command(temp.path(), "sleep 2", 1).await;
        assert!(!timeout.passed);
        assert!(timeout.details.contains("timed out"));
    }

    #[tokio::test]
    async fn timed_out_grader_command_is_killed() {
        let temp = tempfile::TempDir::new().unwrap();
        let command = "sleep 2; echo leaked > leaked.txt";

        let timeout = run_grader_command(temp.path(), command, 1).await;
        assert!(!timeout.passed);
        assert!(timeout.details.contains("timed out"));

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!temp.path().join("leaked.txt").exists());
    }

    #[test]
    fn file_state_grader_checks_content_and_missing_paths() {
        let temp = tempfile::TempDir::new().unwrap();
        write_file(&temp.path().join("README.md"), "hello bonsai\n");
        write_file(&temp.path().join("expected.txt"), "hello bonsai\n");

        let pass = grade_file_state(
            temp.path(),
            temp.path(),
            "README.md",
            Some(true),
            &[String::from("bonsai")],
            &[String::from("codex")],
            Some("expected.txt"),
        );
        assert!(pass.passed);

        let fail = grade_file_state(
            temp.path(),
            temp.path(),
            "README.md",
            Some(true),
            &[String::from("missing")],
            &[],
            None,
        );
        assert!(!fail.passed);

        let missing = grade_file_state(
            temp.path(),
            temp.path(),
            "absent.txt",
            Some(false),
            &[],
            &[],
            None,
        );
        assert!(missing.passed);
    }

    #[test]
    fn assertion_grader_checks_final_output() {
        let pass = grade_assertion(
            "done without panic",
            &[String::from("done")],
            &[String::from("error")],
        );
        assert!(pass.passed);

        let fail = grade_assertion(
            "done with error",
            &[String::from("done")],
            &[String::from("error")],
        );
        assert!(!fail.passed);
    }
}
