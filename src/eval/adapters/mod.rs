//! Offline benchmark contracts and thin launch/export adapters.

mod contract;
mod harbor;
mod launcher;
mod report;
mod swe_bench;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

pub(crate) use contract::*;
pub(crate) use harbor::*;
pub(crate) use launcher::*;
pub(crate) use report::*;
pub(crate) use swe_bench::*;

const DEFAULT_ADAPTER_OUT_DIR: &str = "target/eval/adapters";

/// Parsed `bonsai eval adapter ...` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdapterCliCommand {
    Run(AdapterRunConfig),
    ImportHarbor(AdapterImportConfig),
}

/// Run one or more versioned benchmark requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdapterRunConfig {
    pub(crate) request_path: PathBuf,
    pub(crate) out_dir: PathBuf,
    pub(crate) force: bool,
    pub(crate) json: bool,
}

impl AdapterRunConfig {
    pub(crate) fn new(request_path: PathBuf) -> Self {
        Self {
            request_path,
            out_dir: PathBuf::from(DEFAULT_ADAPTER_OUT_DIR),
            force: false,
            json: false,
        }
    }
}

/// Import a pinned Harbor `TrialResult` envelope into Bonsai's normalized form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdapterImportConfig {
    pub(crate) result_path: PathBuf,
    pub(crate) out_path: PathBuf,
    pub(crate) json: bool,
}

/// Summary returned to the top-level CLI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AdapterRunOutcome {
    pub(crate) total: usize,
    pub(crate) completed: usize,
    pub(crate) failed: usize,
    pub(crate) reused: usize,
    pub(crate) output_dir: String,
    pub(crate) tasks: Vec<AdapterTaskOutcome>,
}

impl AdapterRunOutcome {
    pub(crate) const fn should_fail_process(&self) -> bool {
        self.failed > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AdapterTaskOutcome {
    pub(crate) task_id: String,
    pub(crate) request_key: String,
    pub(crate) terminal_state: AdapterTerminalState,
    pub(crate) reused: bool,
    pub(crate) sidecar: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prediction: Option<String>,
}

pub(crate) async fn execute_adapter(command: AdapterCliCommand) -> Result<AdapterRunOutcome> {
    match command {
        AdapterCliCommand::Run(config) => run_adapter(config).await,
        AdapterCliCommand::ImportHarbor(config) => import_harbor(config),
    }
}

async fn run_adapter(config: AdapterRunConfig) -> Result<AdapterRunOutcome> {
    let requests = load_requests(&config.request_path)?;
    reject_duplicate_swe_instance_ids(&requests)?;
    let includes_swe = requests
        .iter()
        .any(|request| request.benchmark.kind == BenchmarkKind::SweBenchVerified);
    fs::create_dir_all(&config.out_dir).with_context(|| {
        format!(
            "Failed to create benchmark adapter output directory {}",
            config.out_dir.display()
        )
    })?;

    let mut tasks = Vec::with_capacity(requests.len());
    let mut predictions = Vec::new();
    for request in requests {
        let (outcome, prediction) = run_one(&config, request).await?;
        if let Some(prediction) = prediction {
            predictions.push(prediction);
        }
        tasks.push(outcome);
    }
    if includes_swe {
        write_json_lines_atomic(&config.out_dir.join("predictions.jsonl"), &predictions)?;
    }

    let completed = tasks
        .iter()
        .filter(|task| task.terminal_state.is_success())
        .count();
    let reused = tasks.iter().filter(|task| task.reused).count();
    let outcome = AdapterRunOutcome {
        total: tasks.len(),
        completed,
        failed: tasks.len().saturating_sub(completed),
        reused,
        output_dir: config.out_dir.display().to_string(),
        tasks,
    };
    print_outcome(&outcome, config.json)?;
    Ok(outcome)
}

async fn run_one(
    config: &AdapterRunConfig,
    request: AdapterRequest,
) -> Result<(AdapterTaskOutcome, Option<SweBenchPrediction>)> {
    let request_key = request.request_key()?;
    let task_dir = config.out_dir.join(format!(
        "{}-{}",
        request.task.id,
        request_key.get(..12).unwrap_or(&request_key)
    ));
    let sidecar_path = task_dir.join("bonsai-sidecar.json");
    let prediction_path = (request.benchmark.kind == BenchmarkKind::SweBenchVerified)
        .then(|| task_dir.join("prediction.jsonl"));
    if !config.force
        && let Some(reused) = reusable_result(
            &request,
            &request_key,
            &sidecar_path,
            prediction_path.as_deref(),
        )?
    {
        return Ok(reused);
    }
    if let Some(path) = prediction_path.as_ref()
        && let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error)
            .with_context(|| format!("Failed to remove stale prediction {}", path.display()));
    }

    let mut launch = match launch(&request).await {
        Ok(launch) => launch,
        Err(error) => {
            let sidecar = AdapterSidecar::internal_error(
                &request,
                request_key.clone(),
                &format!("{error:#}"),
                &sidecar_path,
            );
            write_json_atomic(&sidecar_path, &sidecar)?;
            return Ok((
                task_outcome(
                    &request,
                    request_key,
                    AdapterTerminalState::InternalError,
                    false,
                    &sidecar_path,
                    None,
                ),
                None,
            ));
        }
    };

    let mut patch = None;
    let mut prediction = None;
    if request.benchmark.kind == BenchmarkKind::SweBenchVerified {
        let base_commit = request
            .task
            .base_commit
            .as_deref()
            .context("Validated SWE-bench request lost its base commit")?;
        match extract_patch(
            &request.task.workspace,
            base_commit,
            request.runner.budgets.max_patch_bytes,
        ) {
            Ok(extracted) => {
                let value = SweBenchPrediction {
                    instance_id: request.task.id.clone(),
                    model_patch: extracted.body.clone(),
                    model_name_or_path: request.model_name_or_path(),
                };
                if let Some(path) = prediction_path.as_ref() {
                    write_json_lines_atomic(path, std::slice::from_ref(&value))?;
                }
                patch = Some(extracted);
                prediction = Some(value);
            }
            Err(error) => {
                launch.terminal_state = AdapterTerminalState::PatchRejected;
                launch.terminal_reason = Some(format!("{error:#}"));
            }
        }
    }

    let terminal_state = launch.terminal_state;
    let sidecar = AdapterSidecar::from_launch(
        &request,
        request_key.clone(),
        launch,
        patch.as_ref(),
        prediction_path.as_deref().filter(|_| prediction.is_some()),
        &sidecar_path,
    );
    write_json_atomic(&sidecar_path, &sidecar)?;
    Ok((
        task_outcome(
            &request,
            request_key,
            terminal_state,
            false,
            &sidecar_path,
            prediction_path.as_deref().filter(|_| prediction.is_some()),
        ),
        prediction,
    ))
}

fn reusable_result(
    request: &AdapterRequest,
    request_key: &str,
    sidecar_path: &Path,
    prediction_path: Option<&Path>,
) -> Result<Option<(AdapterTaskOutcome, Option<SweBenchPrediction>)>> {
    let body = match fs::read_to_string(sidecar_path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Failed to read adapter sidecar {}", sidecar_path.display())
            });
        }
    };
    let sidecar: AdapterSidecar = serde_json::from_str(&body)
        .with_context(|| format!("Failed to parse adapter sidecar {}", sidecar_path.display()))?;
    if sidecar.schema_version != ADAPTER_SCHEMA_VERSION
        || sidecar.request_key != request_key
        || sidecar.terminal_state != AdapterTerminalState::Completed
    {
        return Ok(None);
    }
    let prediction = match prediction_path {
        Some(path) => match read_one_prediction(path)? {
            Some(prediction) if prediction.instance_id == request.task.id => Some(prediction),
            _ => return Ok(None),
        },
        None => None,
    };
    Ok(Some((
        task_outcome(
            request,
            request_key.to_string(),
            sidecar.terminal_state,
            true,
            sidecar_path,
            prediction_path,
        ),
        prediction,
    )))
}

fn read_one_prediction(path: &Path) -> Result<Option<SweBenchPrediction>> {
    let body = match fs::read_to_string(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read prediction {}", path.display()));
        }
    };
    let mut lines = body.lines().filter(|line| !line.trim().is_empty());
    let Some(line) = lines.next() else {
        return Ok(None);
    };
    if lines.next().is_some() {
        anyhow::bail!(
            "Per-task prediction {} contains multiple records",
            path.display()
        );
    }
    Ok(Some(serde_json::from_str(line).with_context(|| {
        format!("Failed to parse prediction {}", path.display())
    })?))
}

fn reject_duplicate_swe_instance_ids(requests: &[AdapterRequest]) -> Result<()> {
    let mut ids = HashSet::new();
    for request in requests
        .iter()
        .filter(|request| request.benchmark.kind == BenchmarkKind::SweBenchVerified)
    {
        if !ids.insert(&request.task.id) {
            anyhow::bail!(
                "SWE-bench batch contains duplicate instance id '{}'",
                request.task.id
            );
        }
    }
    Ok(())
}

fn task_outcome(
    request: &AdapterRequest,
    request_key: String,
    terminal_state: AdapterTerminalState,
    reused: bool,
    sidecar_path: &Path,
    prediction_path: Option<&Path>,
) -> AdapterTaskOutcome {
    AdapterTaskOutcome {
        task_id: request.task.id.clone(),
        request_key,
        terminal_state,
        reused,
        sidecar: sidecar_path.display().to_string(),
        prediction: prediction_path.map(|path| path.display().to_string()),
    }
}

fn import_harbor(config: AdapterImportConfig) -> Result<AdapterRunOutcome> {
    let body = fs::read_to_string(&config.result_path).with_context(|| {
        format!(
            "Failed to read Harbor result envelope {}",
            config.result_path.display()
        )
    })?;
    let imported = import_harbor_result(&body)?;
    write_json_atomic(&config.out_path, &imported)?;
    if config.json {
        println!("{}", serde_json::to_string_pretty(&imported)?);
    } else {
        println!(
            "Harbor {}: {:?}{}",
            imported.task_id,
            imported.terminal_state,
            imported
                .score
                .map(|score| format!(" · score {score:.3}"))
                .unwrap_or_default()
        );
        println!("Imported result: {}", config.out_path.display());
    }
    let failed = usize::from(!imported.terminal_state.is_success());
    Ok(AdapterRunOutcome {
        total: 1,
        completed: 1_usize.saturating_sub(failed),
        failed,
        reused: 0,
        output_dir: config
            .out_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .display()
            .to_string(),
        tasks: vec![AdapterTaskOutcome {
            task_id: imported.task_id,
            request_key: String::new(),
            terminal_state: imported.terminal_state,
            reused: false,
            sidecar: config.out_path.display().to_string(),
            prediction: None,
        }],
    })
}

fn print_outcome(outcome: &AdapterRunOutcome, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(outcome)?);
    } else {
        println!(
            "Benchmark adapters: {}/{} completed, {} failed, {} reused",
            outcome.completed, outcome.total, outcome.failed, outcome.reused
        );
        println!("Artifacts: {}", outcome.output_dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn duplicate_swe_instance_ids_are_rejected_but_harbor_ids_are_independent() {
        let fixture = include_str!("../../../eval/fixtures/adapters/swe-request-v1.json");
        let request: AdapterRequest = serde_json::from_str(fixture).unwrap();
        let error = reject_duplicate_swe_instance_ids(&[request.clone(), request])
            .unwrap_err()
            .to_string();
        assert!(error.contains("duplicate instance id"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_swe_prediction_is_reused_without_relaunching() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        fn git(workspace: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .args(args)
                .current_dir(workspace)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        }

        let workspace = tempfile::TempDir::new().unwrap();
        git(workspace.path(), &["init", "-q"]);
        git(
            workspace.path(),
            &["config", "user.email", "eval@example.test"],
        );
        git(workspace.path(), &["config", "user.name", "Eval"]);
        fs::write(workspace.path().join("tracked.txt"), "before\n").unwrap();
        git(workspace.path(), &["add", "tracked.txt"]);
        git(workspace.path(), &["commit", "-qm", "base"]);
        let base = git(workspace.path(), &["rev-parse", "HEAD"]);

        let binary_dir = tempfile::TempDir::new().unwrap();
        let binary = binary_dir.path().join("fake-bonsai");
        fs::write(
            &binary,
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo "bonsai fixture"; exit 0; fi
cat >/dev/null
printf 'after\n' > tracked.txt
printf 'new\n' > added.txt
printf '%s\n' '{"status":"completed","output":"done","provider":"provider","model":"model","session_id":7,"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12,"cost_micros":3},"budget_exhaustion":null,"verification":{"repair_attempts":0},"completion_report":{}}'
"#,
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();

        let fixture = include_str!("../../../eval/fixtures/adapters/swe-request-v1.json");
        let mut request: AdapterRequest = serde_json::from_str(fixture).unwrap();
        request.task.id = "owner__repo-1".to_string();
        request.task.workspace = workspace.path().to_path_buf();
        request.task.base_commit = Some(base);
        request.runner.bonsai_binary = binary.clone();
        request.runner.bonsai_revision = "fixture".to_string();
        request.runner.provider = "provider".to_string();
        request.runner.model = "model".to_string();
        request.validate().unwrap();

        let request_dir = tempfile::TempDir::new().unwrap();
        let request_path = request_dir.path().join("request.json");
        fs::write(&request_path, serde_json::to_vec_pretty(&request).unwrap()).unwrap();
        let output_dir = tempfile::TempDir::new().unwrap();
        let config = AdapterRunConfig {
            request_path,
            out_dir: output_dir.path().to_path_buf(),
            force: false,
            json: false,
        };

        let first = run_adapter(config.clone()).await.unwrap();
        assert_eq!(first.completed, 1);
        assert_eq!(first.reused, 0);
        fs::remove_file(&binary).unwrap();

        let second = run_adapter(config.clone()).await.unwrap();
        assert_eq!(second.completed, 1);
        assert_eq!(second.reused, 1);
        let prediction_path = PathBuf::from(second.tasks[0].prediction.as_ref().unwrap());
        let predictions = fs::read_to_string(output_dir.path().join("predictions.jsonl")).unwrap();
        assert_eq!(predictions.lines().count(), 1);
        assert!(predictions.contains("tracked.txt"));
        assert!(predictions.contains("added.txt"));

        let mut forced = config;
        forced.force = true;
        let failed = run_adapter(forced).await.unwrap();
        assert_eq!(failed.failed, 1);
        assert!(!prediction_path.exists());
        assert!(
            fs::read_to_string(output_dir.path().join("predictions.jsonl"))
                .unwrap()
                .is_empty()
        );
    }
}
