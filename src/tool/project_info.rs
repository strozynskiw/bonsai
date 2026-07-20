use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::context::{ProjectContextSnapshot, detect_project_ecosystems, project_context_snapshot};
use crate::tool::schema::{closed_object, parse_args};
use crate::tool::{ParallelPolicy, Tool, ToolOutput};

const MAX_PROJECT_INFO_CHARS: usize = 12_000;
const PROJECT_INFO_TRUNCATION_NOTE: &str =
    "Use narrower read, grep, git, or project-specific tools for more detail.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProjectInfoProviderState {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) model: String,
}

impl ProjectInfoProviderState {
    pub(crate) fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            model: model.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ProjectInfoRuntimeSnapshot {
    pub(crate) mode: Option<String>,
    pub(crate) provider: Option<ProjectInfoProviderState>,
    pub(crate) tools: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct ProjectInfoRuntime {
    state: Mutex<ProjectInfoRuntimeSnapshot>,
}

impl ProjectInfoRuntime {
    pub(crate) fn new(provider: Option<ProjectInfoProviderState>) -> Self {
        Self {
            state: Mutex::new(ProjectInfoRuntimeSnapshot {
                provider,
                ..ProjectInfoRuntimeSnapshot::default()
            }),
        }
    }

    pub(crate) fn set_active_tools(&self, mode: &str, tools: Vec<String>) {
        let mut state = self.lock_state();
        state.mode = Some(mode.to_string());
        state.tools = tools;
    }

    pub(crate) fn set_provider(&self, provider: ProjectInfoProviderState) {
        self.lock_state().provider = Some(provider);
    }

    pub(crate) fn provider_state(&self) -> Option<ProjectInfoProviderState> {
        self.lock_state().provider.clone()
    }

    fn snapshot(&self) -> ProjectInfoRuntimeSnapshot {
        self.lock_state().clone()
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ProjectInfoRuntimeSnapshot> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

#[derive(Debug)]
pub struct ProjectInfoTool {
    project_root: PathBuf,
    runtime: Arc<ProjectInfoRuntime>,
}

impl ProjectInfoTool {
    pub(crate) fn new(project_root: PathBuf, runtime: Arc<ProjectInfoRuntime>) -> Self {
        Self {
            project_root,
            runtime,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ProjectInfoArgs {}

#[derive(Debug, Serialize)]
struct ProjectInfoOutput {
    cwd: String,
    mode: Option<String>,
    provider: Option<ProjectInfoProviderState>,
    ecosystems: Vec<String>,
    git: GitInfo,
    steering_files: Vec<SteeringFileInfo>,
    known_commands: Vec<KnownCommand>,
    tools: Vec<String>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncation_note: Option<&'static str>,
}

#[derive(Debug, Serialize)]
struct ProjectInfoFallbackOutput {
    cwd: String,
    mode: Option<String>,
    provider: Option<ProjectInfoProviderState>,
    ecosystems: Vec<String>,
    git: GitInfo,
    steering_files_count: usize,
    known_commands: Vec<KnownCommand>,
    tools_count: usize,
    truncated: bool,
    truncation_note: &'static str,
}

#[derive(Debug, Serialize)]
struct GitInfo {
    repository: bool,
    branch: Option<String>,
    uncommitted_changes: Option<usize>,
    last_commit: Option<String>,
}

#[derive(Debug, Serialize)]
struct SteeringFileInfo {
    name: String,
    path: String,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct KnownCommand {
    kind: &'static str,
    command: &'static str,
}

#[async_trait]
impl Tool for ProjectInfoTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::ReadOnly
    }

    fn name(&self) -> &str {
        "project_info"
    }

    fn description(&self) -> &str {
        "Return compact read-only project orientation: cwd, ecosystems, git summary, steering files, available tools, and likely commands."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        closed_object([], &[])
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        ParallelPolicy::AlwaysSafe
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let _args: ProjectInfoArgs = parse_args("project_info", args)?;
        let context = project_context_snapshot(&self.project_root);
        let runtime = self.runtime.snapshot();
        let ecosystems = detect_project_ecosystems(&self.project_root)
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let git = GitInfo::collect(&self.project_root).await;
        let output = ProjectInfoOutput {
            cwd: self.project_root.display().to_string(),
            mode: runtime.mode,
            provider: runtime.provider,
            ecosystems: ecosystems.clone(),
            git,
            steering_files: steering_files(&context),
            known_commands: known_commands(&self.project_root, &ecosystems),
            tools: runtime.tools,
            truncated: false,
            truncation_note: None,
        };
        Ok(ToolOutput::Text(render_project_info(output)?))
    }
}

fn render_project_info(mut output: ProjectInfoOutput) -> Result<String> {
    let rendered = serde_json::to_string_pretty(&output)?;
    if rendered.chars().count() <= MAX_PROJECT_INFO_CHARS {
        return Ok(rendered);
    }

    let steering_files_count = output.steering_files.len();
    let tools_count = output.tools.len();
    output.truncated = true;
    output.truncation_note = Some(PROJECT_INFO_TRUNCATION_NOTE);
    output.steering_files.truncate(32);
    output.tools.truncate(128);

    let rendered = serde_json::to_string_pretty(&output)?;
    if rendered.chars().count() <= MAX_PROJECT_INFO_CHARS {
        return Ok(rendered);
    }

    let fallback = ProjectInfoFallbackOutput {
        cwd: output.cwd,
        mode: output.mode,
        provider: output.provider,
        ecosystems: output.ecosystems,
        git: output.git,
        steering_files_count,
        known_commands: output.known_commands,
        tools_count,
        truncated: true,
        truncation_note: PROJECT_INFO_TRUNCATION_NOTE,
    };
    Ok(serde_json::to_string_pretty(&fallback)?)
}

impl GitInfo {
    async fn collect(root: &Path) -> Self {
        let branch = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await;
        let Some(branch) = branch else {
            return Self {
                repository: false,
                branch: None,
                uncommitted_changes: None,
                last_commit: None,
            };
        };
        let uncommitted_changes = git(
            root,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=normal",
                "--no-renames",
            ],
        )
        .await
        .map(|status| {
            status
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
        });
        let last_commit = git(root, &["log", "-1", "--pretty=%s"])
            .await
            .filter(|line| !line.is_empty());
        Self {
            repository: true,
            branch: Some(branch),
            uncommitted_changes,
            last_commit,
        }
    }
}

fn steering_files(context: &ProjectContextSnapshot) -> Vec<SteeringFileInfo> {
    context
        .steering_files
        .iter()
        .map(|file| SteeringFileInfo {
            name: file.name.clone(),
            path: file.directory.join(&file.name).display().to_string(),
            truncated: file.truncated,
        })
        .collect()
}

fn known_commands(root: &Path, ecosystems: &[String]) -> Vec<KnownCommand> {
    let mut commands = Vec::new();
    if ecosystems.iter().any(|kind| kind == "Rust") {
        commands.push(KnownCommand {
            kind: "format",
            command: "cargo fmt --all",
        });
        commands.push(KnownCommand {
            kind: "lint",
            command: "cargo clippy --all-targets --all-features -- -D warnings",
        });
        commands.push(KnownCommand {
            kind: "test",
            command: if root.join("Cargo.lock").exists() {
                "cargo test --locked"
            } else {
                "cargo test"
            },
        });
        commands.push(KnownCommand {
            kind: "build",
            command: if root.join("Cargo.lock").exists() {
                "cargo build --release --locked"
            } else {
                "cargo build --release"
            },
        });
    }
    if ecosystems.iter().any(|kind| kind == "Node/JavaScript") {
        commands.push(KnownCommand {
            kind: "install",
            command: "npm install",
        });
        commands.push(KnownCommand {
            kind: "test",
            command: "npm test",
        });
        commands.push(KnownCommand {
            kind: "lint",
            command: "npm run lint",
        });
        commands.push(KnownCommand {
            kind: "build",
            command: "npm run build",
        });
    }
    if ecosystems.iter().any(|kind| kind == "Python") {
        commands.push(KnownCommand {
            kind: "test",
            command: "pytest",
        });
        commands.push(KnownCommand {
            kind: "lint",
            command: "ruff check .",
        });
    }
    if ecosystems.iter().any(|kind| kind == "Go") {
        commands.push(KnownCommand {
            kind: "format",
            command: "gofmt -w .",
        });
        commands.push(KnownCommand {
            kind: "test",
            command: "go test ./...",
        });
    }
    commands
}

async fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .await
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reports_project_orientation_without_steering_body() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname='demo'\n").unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "secret rules").unwrap();
        let runtime = Arc::new(ProjectInfoRuntime::new(Some(
            ProjectInfoProviderState::new("codex", "Codex", "gpt-5"),
        )));
        runtime.set_active_tools(
            "coding",
            vec!["project_info".to_string(), "read".to_string()],
        );
        let tool = ProjectInfoTool::new(dir.path().to_path_buf(), runtime);

        let result = tool.execute(serde_json::json!({})).await.unwrap();
        let ToolOutput::Text(text) = result else {
            panic!("project_info should return text");
        };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(value["mode"], "coding");
        assert_eq!(value["provider"]["id"], "codex");
        assert_eq!(value["provider"]["model"], "gpt-5");
        assert_eq!(value["ecosystems"], serde_json::json!(["Rust"]));
        assert_eq!(value["tools"], serde_json::json!(["project_info", "read"]));
        assert_eq!(value["known_commands"][2]["command"], "cargo test --locked");
        assert_eq!(value["steering_files"][0]["name"], "AGENTS.md");
        assert!(
            !text.contains("secret rules"),
            "project_info should list steering files without embedding instructions"
        );
    }

    #[tokio::test]
    async fn capped_project_info_remains_valid_json() {
        let dir = tempfile::TempDir::new().unwrap();
        let runtime = Arc::new(ProjectInfoRuntime::default());
        runtime.set_active_tools(
            "coding",
            (0..2_000)
                .map(|i| format!("tool-{i}-{}", "x".repeat(400)))
                .collect(),
        );
        let tool = ProjectInfoTool::new(dir.path().to_path_buf(), runtime);

        let result = tool.execute(serde_json::json!({})).await.unwrap();
        let ToolOutput::Text(text) = result else {
            panic!("project_info should return text");
        };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert!(
            text.chars().count() <= MAX_PROJECT_INFO_CHARS,
            "project_info should stay within the output cap"
        );
        assert_eq!(value["truncated"], true);
        assert_eq!(value["tools_count"], 2_000);
    }

    #[test]
    fn project_info_is_always_safe_and_closed_schema() {
        let runtime = Arc::new(ProjectInfoRuntime::default());
        let tool = ProjectInfoTool::new(PathBuf::from("."), runtime);

        assert_eq!(tool.parallel_policy(), ParallelPolicy::AlwaysSafe);
        assert_eq!(tool.parameters_schema()["additionalProperties"], false);
    }
}
