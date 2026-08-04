//! Helpers that keep the run-loop's tool-batch body small: argument parsing,
//! per-tool completion reporting, and image-result message construction.

use async_openai::types::chat::ChatCompletionRequestUserMessage;
use futures::future::BoxFuture;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use super::*;
use crate::tool::Tool;
use crate::tool::arg_repair::{RepairNote, repair_arguments, unwrap_double_encoded_arguments};
use crate::tool::schema::compact_schema_hint;
use crate::verification::{
    VerificationBinding, VerificationTerminalReason, VerificationWorkspaceIdentity,
    normalize_verification_command,
};

const TOOL_ARGUMENT_PREVIEW_CHARS: usize = 600;
const TOOL_RESULT_PREVIEW_CHARS: usize = 4_000;
pub(super) const COMMAND_SUMMARY_MARKER: &str = "[Command summary]";

#[derive(Debug, Default)]
pub(super) struct WorktreeSnapshot {
    files: BTreeMap<String, String>,
}

impl WorktreeSnapshot {
    pub(super) fn paths(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }

    pub(super) fn changed_paths(&self, current: &Self) -> Vec<String> {
        self.files
            .keys()
            .chain(current.files.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|path| self.files.get(*path) != current.files.get(*path))
            .cloned()
            .collect()
    }
}

pub(super) async fn capture_worktree_snapshot(root: &Path) -> Option<WorktreeSnapshot> {
    capture_worktree_snapshot_including(root, &[]).await
}

pub(super) async fn capture_worktree_snapshot_including(
    root: &Path,
    retained_paths: &[String],
) -> Option<WorktreeSnapshot> {
    let commands: [&[&str]; 3] = [
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            "-z",
            "HEAD",
            "--",
        ],
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--cached",
            "--name-only",
            "-z",
            "--",
        ],
        &["ls-files", "--others", "--exclude-standard", "-z"],
    ];
    let mut paths = retained_paths.iter().cloned().collect::<BTreeSet<_>>();
    let mut command_succeeded = false;
    for args in commands {
        let output = tokio::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            continue;
        }
        command_succeeded = true;
        for raw in output.stdout.split(|byte| *byte == 0) {
            if !raw.is_empty() {
                paths.insert(String::from_utf8_lossy(raw).into_owned());
            }
        }
    }
    if !command_succeeded {
        return None;
    }

    let mut files = BTreeMap::new();
    for path in paths {
        let fingerprint = fingerprint_path(&root.join(&path)).await;
        files.insert(path, fingerprint);
    }
    Some(WorktreeSnapshot { files })
}

pub(super) fn capture_verification_workspace_binding<'a>(
    root: &'a Path,
    command_cwd: &'a Path,
    command: &'a str,
) -> BoxFuture<'a, VerificationBinding> {
    // Workspace capture has several process and filesystem await points. Box
    // that state machine so it does not inflate every caller all the way up to
    // the long-lived agent turn future.
    Box::pin(async move {
        match capture_verification_workspace_identity(root, command_cwd, command).await {
            Ok(identity) => match identity.digest() {
                Ok(digest) => VerificationBinding::Bound {
                    digest,
                    identity: Box::new(identity),
                },
                Err(_) => VerificationBinding::Blocked {
                    reason: VerificationTerminalReason::EnvironmentBlocked,
                },
            },
            Err(_) => VerificationBinding::Blocked {
                reason: VerificationTerminalReason::EnvironmentBlocked,
            },
        }
    })
}

async fn capture_verification_workspace_identity(
    root: &Path,
    command_cwd: &Path,
    command: &str,
) -> Result<VerificationWorkspaceIdentity, std::io::Error> {
    let project_root = tokio::fs::canonicalize(root).await?;
    let command_cwd = tokio::fs::canonicalize(command_cwd).await?;
    let git_worktree_root = git_stdout(root, &["rev-parse", "--show-toplevel"])
        .await
        .ok()
        .map(|path| PathBuf::from(path.trim()));
    let (
        repository_root,
        worktree_root,
        head_oid,
        index_digest,
        tracked_worktree_digest,
        untracked_inputs,
    ) = if let Some(git_worktree_root) = git_worktree_root {
        let worktree_root = tokio::fs::canonicalize(git_worktree_root).await?;
        let repository_root = git_common_dir(&worktree_root)
            .await
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        let head_oid = git_stdout(&worktree_root, &["rev-parse", "--verify", "HEAD"])
            .await
            .ok()
            .map(|oid| oid.trim().to_string());
        let (index_args, tracked_args): (&[&str], &[&str]) = if head_oid.is_some() {
            (
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--cached",
                    "--binary",
                    "HEAD",
                    "--",
                ],
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--binary",
                    "HEAD",
                    "--",
                ],
            )
        } else {
            (
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--cached",
                    "--binary",
                    "--",
                ],
                &["diff", "--no-ext-diff", "--no-textconv", "--binary", "--"],
            )
        };
        let index_digest = git_digest(&worktree_root, index_args).await?;
        let tracked_worktree_digest = git_digest(&worktree_root, tracked_args).await?;
        let untracked_inputs = capture_git_untracked_inputs(&worktree_root, command).await?;
        (
            repository_root,
            worktree_root,
            head_oid,
            index_digest,
            tracked_worktree_digest,
            untracked_inputs,
        )
    } else {
        let inputs = capture_unversioned_inputs(&project_root, command).await?;
        let input_digest = digest_serializable(&inputs)?;
        (
            None,
            project_root.clone(),
            None,
            blake3::hash(&[]).to_hex().to_string(),
            input_digest,
            inputs,
        )
    };

    let mut environment = BTreeMap::new();
    for name in [
        "CI",
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "CC",
        "CFLAGS",
        "CXX",
        "CXXFLAGS",
        "GOENV",
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "RUSTC_WRAPPER",
        "RUSTUP_TOOLCHAIN",
        "NODE_ENV",
        "PYTHONPATH",
        "VIRTUAL_ENV",
        "GOFLAGS",
        "PATH",
    ] {
        if let Some(value) = std::env::var_os(name) {
            environment.insert(name, value.to_string_lossy().into_owned());
        }
    }
    let command_config = verification_command_config(&project_root, &command_cwd, command).await;
    let command_fingerprint = digest_serializable(&(normalize_command(command), command_config))?;
    let toolchain = verification_toolchain_fingerprint(&command_cwd, command).await;
    let toolchain_environment_fingerprint = digest_serializable(&(environment, toolchain))?;

    Ok(VerificationWorkspaceIdentity {
        repository_root,
        worktree_root: worktree_root.to_string_lossy().into_owned(),
        project_root: project_root.to_string_lossy().into_owned(),
        head_oid,
        index_digest,
        tracked_worktree_digest,
        untracked_inputs,
        command_cwd: command_cwd.to_string_lossy().into_owned(),
        command_fingerprint,
        toolchain_environment_fingerprint,
    })
}

async fn verification_command_config(
    root: &Path,
    command_cwd: &Path,
    command: &str,
) -> BTreeMap<String, String> {
    const INPUTS: &[&str] = &[
        ".bonsai/config.toml",
        ".cargo/config",
        ".cargo/config.toml",
        ".npmrc",
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain",
        "rust-toolchain.toml",
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "bun.lock",
        "bun.lockb",
        "tsconfig.json",
        "pyproject.toml",
        "requirements.txt",
        "requirements-dev.txt",
        "pytest.ini",
        "setup.cfg",
        "tox.ini",
        "uv.lock",
        "poetry.lock",
        "go.mod",
        "go.sum",
        "go.work",
        "go.work.sum",
    ];
    let mut inputs = BTreeMap::new();
    for (scope, base) in [("project", root), ("cwd", command_cwd)] {
        for path in INPUTS {
            if let Ok(content) = tokio::fs::read(base.join(path)).await {
                inputs.insert(
                    format!("{scope}:{path}"),
                    blake3::hash(&content).to_hex().to_string(),
                );
            }
        }
    }
    inputs.insert("command".to_string(), normalize_command(command));
    inputs
}

fn normalize_command(command: &str) -> String {
    normalize_verification_command(command)
}

fn is_relevant_verification_input(path: &str, command: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let file_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let known_config = matches!(
        file_name,
        "Cargo.toml"
            | "Cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "bun.lock"
            | "bun.lockb"
            | "tsconfig.json"
            | "pyproject.toml"
            | "requirements.txt"
            | "requirements-dev.txt"
            | "pytest.ini"
            | "setup.cfg"
            | "tox.ini"
            | "uv.lock"
            | "poetry.lock"
            | "go.mod"
            | "go.sum"
            | "go.work"
            | "go.work.sum"
            | "rust-toolchain"
            | "rust-toolchain.toml"
    );
    known_config
        || normalized.starts_with("src/")
        || normalized.starts_with("test/")
        || normalized.starts_with("tests/")
        || normalized.rsplit_once('.').is_some_and(|(_, extension)| {
            matches!(
                extension,
                "c" | "cc"
                    | "cpp"
                    | "cxx"
                    | "go"
                    | "h"
                    | "hpp"
                    | "java"
                    | "js"
                    | "jsx"
                    | "kt"
                    | "kts"
                    | "proto"
                    | "py"
                    | "rs"
                    | "sh"
                    | "sql"
                    | "swift"
                    | "ts"
                    | "tsx"
            )
        })
        || command_mentions_path(command, &normalized)
}

fn command_mentions_path(command: &str, path: &str) -> bool {
    let path = path.trim_start_matches("./");
    command.split_whitespace().any(|word| {
        let word = word.trim_matches(|character: char| "'\";,&|()".contains(character));
        let word = word
            .split_once('=')
            .map_or(word, |(_, value)| value)
            .trim_start_matches("./");
        word == path
    })
}

async fn capture_git_untracked_inputs(
    worktree_root: &Path,
    command: &str,
) -> Result<BTreeMap<String, String>, std::io::Error> {
    let mut child = tokio::process::Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard", "-z"])
        .current_dir(worktree_root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("Git stdout pipe was unavailable"))?;
    let mut stdout = tokio::io::BufReader::new(stdout);
    let mut inputs = BTreeMap::new();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        let read = stdout.read_until(0, &mut raw).await?;
        if read == 0 {
            break;
        }
        if raw.last() == Some(&0) {
            raw.pop();
        }
        if raw.is_empty() {
            continue;
        }
        let path = String::from_utf8_lossy(&raw).into_owned();
        if is_relevant_verification_input(&path, command) {
            inputs.insert(
                path.clone(),
                fingerprint_path(&worktree_root.join(path)).await,
            );
        }
    }
    let status = child.wait().await?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "Git exited with status {status}"
        )));
    }
    Ok(inputs)
}

async fn capture_unversioned_inputs(
    root: &Path,
    command: &str,
) -> Result<BTreeMap<String, String>, std::io::Error> {
    let walk_root = root.to_path_buf();
    let command = command.to_string();
    let paths = tokio::task::spawn_blocking(move || {
        let mut paths = Vec::new();
        let mut builder = ignore::WalkBuilder::new(&walk_root);
        builder.hidden(false).follow_links(false);
        for entry in builder.build() {
            let entry = entry.map_err(|error| std::io::Error::other(error.to_string()))?;
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() && !file_type.is_symlink() {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&walk_root) else {
                continue;
            };
            if generated_path(relative) {
                continue;
            }
            let path = relative.to_string_lossy().replace('\\', "/");
            if is_relevant_verification_input(&path, &command) {
                paths.push(path);
            }
        }
        Ok::<_, std::io::Error>(paths)
    })
    .await
    .map_err(std::io::Error::other)??;

    let mut inputs = BTreeMap::new();
    for path in paths {
        inputs.insert(path.clone(), fingerprint_path(&root.join(path)).await);
    }
    Ok(inputs)
}

fn generated_path(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component.as_os_str().to_str(),
            Some(".git" | ".venv" | "__pycache__" | "build" | "dist" | "node_modules" | "target")
        )
    })
}

async fn verification_toolchain_fingerprint(
    command_cwd: &Path,
    command: &str,
) -> BTreeMap<String, String> {
    let mut tools = BTreeMap::new();
    for program in verification_command_programs(command) {
        let fingerprint = match resolve_executable(command_cwd, &program).await {
            Some(path) => {
                let canonical = tokio::fs::canonicalize(&path).await.unwrap_or(path);
                format!(
                    "{}:{}",
                    canonical.to_string_lossy(),
                    fingerprint_executable(&canonical).await
                )
            }
            None => "unresolved".to_string(),
        };
        tools.insert(program, fingerprint);
    }
    tools
}

async fn fingerprint_executable(path: &Path) -> String {
    type CacheKey = (PathBuf, u64, String);
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<CacheKey, String>>,
    > = std::sync::OnceLock::new();

    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return fingerprint_path(path).await,
    };
    let key = (
        path.to_path_buf(),
        metadata.len(),
        executable_change_stamp(&metadata),
    );
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    if let Ok(cache) = cache.lock()
        && let Some(fingerprint) = cache.get(&key)
    {
        return fingerprint.clone();
    }

    let fingerprint = fingerprint_path(path).await;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, fingerprint.clone());
    }
    fingerprint
}

#[cfg(unix)]
fn executable_change_stamp(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;

    format!(
        "{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.ctime(),
        metadata.ctime_nsec()
    )
}

#[cfg(not(unix))]
fn executable_change_stamp(metadata: &std::fs::Metadata) -> String {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn verification_command_programs(command: &str) -> BTreeSet<String> {
    let analysis = crate::tool::analyze_bash_command(command);
    analysis
        .permission_commands()
        .iter()
        .filter_map(|segment| command_program(segment))
        .collect()
}

fn command_program(segment: &str) -> Option<String> {
    let words = segment
        .split_whitespace()
        .map(|word| word.trim_matches(['(', ')', '\'', '"']))
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    let mut index = 0;
    while words.get(index).is_some_and(|word| shell_assignment(word)) {
        index += 1;
    }
    if words.get(index) == Some(&"env") {
        index += 1;
        while let Some(word) = words.get(index) {
            if *word == "--" {
                index += 1;
                break;
            }
            if matches!(*word, "-u" | "--unset" | "-C" | "--chdir") {
                index = index.saturating_add(2);
                continue;
            }
            if word.starts_with('-') || shell_assignment(word) {
                index += 1;
                continue;
            }
            break;
        }
    }
    while matches!(words.get(index), Some(&"command" | &"exec")) {
        index += 1;
    }
    words.get(index).map(|program| program.to_string())
}

fn shell_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name
                .chars()
                .all(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

async fn resolve_executable(command_cwd: &Path, program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() || program.contains('/') || program.contains('\\') {
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            command_cwd.join(path)
        };
        return tokio::fs::symlink_metadata(&candidate)
            .await
            .ok()
            .map(|_| candidate);
    }
    let search_path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&search_path) {
        let candidate = directory.join(program);
        if tokio::fs::symlink_metadata(&candidate).await.is_ok() {
            return Some(candidate);
        }
    }
    None
}

async fn fingerprint_path(path: &Path) -> String {
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return "missing".to_string();
        }
        Err(_) => return "unreadable".to_string(),
    };
    if metadata.file_type().is_symlink() {
        let target = tokio::fs::read_link(path)
            .await
            .map(|target| target.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "unreadable".to_string());
        let resolved = match tokio::fs::canonicalize(path).await {
            Ok(resolved) => resolved,
            Err(_) => return format!("link:{target}:missing-target"),
        };
        let resolved_metadata = match tokio::fs::metadata(&resolved).await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return format!("link:{target}:non-regular-target"),
            Err(_) => return format!("link:{target}:unreadable-target"),
        };
        return format!(
            "link:{target}:{}:{}",
            resolved_metadata.len(),
            fingerprint_regular_file(&resolved).await,
        );
    }
    if metadata.is_file() {
        return fingerprint_regular_file(path).await;
    }
    "other".to_string()
}

async fn fingerprint_regular_file(path: &Path) -> String {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return "non-regular-file".to_string(),
        Err(_) => return "unreadable-file".to_string(),
    };
    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(_) => return "unreadable-file".to_string(),
    };
    let mut hasher = blake3::Hasher::new();
    // Keep the read buffer off the async state machine's stack. This helper is
    // nested under the already-large agent turn future, so an inline 64 KiB
    // array can push ordinary debug/test threads over their stack limit.
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        match file.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                hasher.update(&buffer[..read]);
            }
            Err(_) => return "unreadable-file".to_string(),
        }
    }
    format!("file:{}:{}", metadata.len(), hasher.finalize())
}

async fn git_common_dir(root: &Path) -> Result<PathBuf, std::io::Error> {
    let path = git_stdout(root, &["rev-parse", "--git-common-dir"]).await?;
    let path = PathBuf::from(path.trim());
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    tokio::fs::canonicalize(path).await
}

fn digest_serializable(value: &impl serde::Serialize) -> Result<String, std::io::Error> {
    serde_json::to_vec(value)
        .map(|encoded| blake3::hash(&encoded).to_hex().to_string())
        .map_err(std::io::Error::other)
}

async fn git_stdout(root: &Path, args: &[&str]) -> Result<String, std::io::Error> {
    String::from_utf8(git_bytes(root, args).await?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

async fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, std::io::Error> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .await?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

async fn git_digest(root: &Path, args: &[&str]) -> Result<String, std::io::Error> {
    let mut child = tokio::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("Git stdout pipe was unavailable"))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = stdout.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let status = child.wait().await?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "Git exited with status {status}"
        )));
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Parse a tool call's JSON arguments, returning a model-readable error string
/// when they are not a valid JSON object matching the tool's schema. The error
/// names the tool and the required fields so the next turn can self-correct.
///
/// Between parsing and validation sits the [`arg_repair`] pass: schema-invalid
/// shapes that weaker models (DeepSeek-class) predictably emit are repaired in
/// place. The pass is failure-gated, not model-gated — a value that conforms
/// to the schema is never touched, so well-behaved models are unaffected by
/// construction and no model identity needs to reach this layer.
pub(super) fn parse_tool_arguments(
    tool: &dyn Tool,
    name: &str,
    arguments: &str,
) -> Result<serde_json::Value, String> {
    let schema = tool.parameters_schema();
    let mut value = match serde_json::from_str::<Value>(arguments) {
        Ok(value) => value,
        Err(parse_err) => {
            return Err(format_tool_argument_error(
                name,
                &schema,
                arguments,
                "are not valid JSON matching the tool schema",
                &[],
                &[],
                Some(&parse_err.to_string()),
            ));
        }
    };

    // A double-encoded payload (the whole object stringified twice) must
    // unwrap before the object check below would reject it as a string.
    let mut repairs = Vec::new();
    repairs.extend(unwrap_double_encoded_arguments(&mut value));

    if !value.is_object() {
        return Err(format_tool_argument_error(
            name,
            &schema,
            arguments,
            &format!(
                "must be a JSON object matching the tool schema (got {})",
                json_type_name(&value)
            ),
            &[],
            &[],
            None,
        ));
    };

    repairs.extend(repair_arguments(&schema, &mut value));
    repairs.extend(tool.coerce_arguments(&mut value));

    let object = value.as_object().expect("checked to be an object above");
    let missing = missing_required_fields(&schema, object);
    let rejected = rejected_fields(&schema, object);
    if missing.is_empty() && rejected.is_empty() {
        if !repairs.is_empty() {
            tracing::debug!(
                tool = %name,
                repairs = %join_repairs(&repairs),
                "repaired malformed tool-call arguments"
            );
        }
        return Ok(value);
    }

    // Repairs never remove required keys or add keys, so they cannot be the
    // cause of this failure — but naming them (e.g. an unwrapped
    // double-encoded payload) explains why the guidance below talks about an
    // object when the raw payload echoed back was a string.
    Err(format_tool_argument_error_with_repairs(
        name,
        &schema,
        arguments,
        "do not match the tool schema",
        &missing,
        &rejected,
        None,
        &repairs,
    ))
}

fn join_repairs(repairs: &[RepairNote]) -> String {
    repairs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

fn format_tool_argument_error(
    name: &str,
    schema: &Value,
    arguments: &str,
    reason: &str,
    missing: &[String],
    rejected: &[String],
    parse_error: Option<&str>,
) -> String {
    format_tool_argument_error_with_repairs(
        name,
        schema,
        arguments,
        reason,
        missing,
        rejected,
        parse_error,
        &[],
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "one assembly point for every error part"
)]
fn format_tool_argument_error_with_repairs(
    name: &str,
    schema: &Value,
    arguments: &str,
    reason: &str,
    missing: &[String],
    rejected: &[String],
    parse_error: Option<&str>,
    repairs: &[RepairNote],
) -> String {
    let required = required_fields(schema);
    let mut parts = vec![format!("Error: arguments for '{name}' {reason}.")];
    if !required.is_empty() {
        parts.push(format!("Required fields: {}.", required.join(", ")));
    }
    if !missing.is_empty() {
        parts.push(format!("Missing fields: {}.", missing.join(", ")));
    }
    if !rejected.is_empty() {
        parts.push(format!("Rejected fields: {}.", rejected.join(", ")));
    }
    if let Some(hint) = compact_schema_hint(schema) {
        parts.push(format!("Schema: {hint}."));
    }
    parts.push(format!(
        "Got: {}.",
        compact_argument_payload(arguments, TOOL_ARGUMENT_PREVIEW_CHARS)
    ));
    if let Some(parse_error) = parse_error {
        parts.push(format!("Parse error: {parse_error}."));
    }
    if !repairs.is_empty() {
        parts.push(format!(
            "Attempted repairs before validation: {}.",
            join_repairs(repairs)
        ));
    }
    parts.join(" ")
}

fn missing_required_fields(schema: &Value, object: &Map<String, Value>) -> Vec<String> {
    required_fields(schema)
        .into_iter()
        .filter(|field| !object.contains_key(field))
        .collect()
}

fn rejected_fields(schema: &Value, object: &Map<String, Value>) -> Vec<String> {
    if schema.get("additionalProperties").and_then(Value::as_bool) != Some(false) {
        return Vec::new();
    }
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        let mut rejected = object.keys().cloned().collect::<Vec<_>>();
        rejected.sort_unstable();
        return rejected;
    };
    let mut rejected = object
        .keys()
        .filter(|field| !properties.contains_key(*field))
        .cloned()
        .collect::<Vec<_>>();
    rejected.sort_unstable();
    rejected
}

fn required_fields(schema: &Value) -> Vec<String> {
    schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToString::to_string)
        .collect()
}

fn compact_argument_payload(arguments: &str, max_chars: usize) -> String {
    if arguments.is_empty() {
        "<empty>".to_string()
    } else {
        truncate(arguments, max_chars)
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Report a single tool's completion to the sink from inside the concurrent
/// task, so each tool finishes individually with its own execution time instead
/// of all batch members "finishing" together after `join_all` returns.
pub(super) fn report_tool_completion(
    sink: &SharedSink,
    tool_call: &ToolCall,
    result: &ToolOutput,
    status: crate::output::ToolExecutionStatus,
) {
    match result {
        ToolOutput::BackgroundTaskStarted { message, .. }
        | ToolOutput::SubagentStarted { message, .. } => {
            sink.thinking(&format!("Started {}", tool_call.name));
            sink.tool_output(&tool_call.id, &truncate(message, TOOL_RESULT_PREVIEW_CHARS));
        }
        ToolOutput::Edit { summary, diff } => {
            sink.thinking(&format!("Finished {}", tool_call.name));
            sink.tool_finished_with_diff(
                &tool_call.id,
                &truncate(summary, TOOL_RESULT_PREVIEW_CHARS),
                status,
                diff.clone(),
            );
        }
        ToolOutput::Image { description, .. } => {
            sink.thinking(&format!("Finished {}", tool_call.name));
            sink.tool_finished(&tool_call.id, description, status);
        }
        ToolOutput::Command { rendered, .. } => {
            sink.thinking(&format!("Finished {}", tool_call.name));
            sink.tool_finished(
                &tool_call.id,
                &truncate_command_result(rendered, TOOL_RESULT_PREVIEW_CHARS),
                status,
            );
        }
        _ => {
            sink.thinking(&format!("Finished {}", tool_call.name));
            sink.tool_finished(
                &tool_call.id,
                &truncate(result.rendered_summary(), TOOL_RESULT_PREVIEW_CHARS),
                status,
            );
        }
    }
}

fn truncate_command_result(rendered: &str, max_chars: usize) -> String {
    let char_count = rendered.chars().count();
    if char_count <= max_chars {
        return rendered.to_string();
    }
    let Some(footer_start) = command_summary_footer_start(rendered) else {
        return truncate(rendered, max_chars);
    };

    let footer = &rendered[footer_start..];
    let footer_chars = footer.chars().count();
    if footer_chars >= max_chars {
        return footer.to_string();
    }

    let notice = format!("... ({char_count} chars total, command summary preserved below)");
    let fixed_chars = footer_chars
        .saturating_add(notice.chars().count())
        .saturating_add(4);
    if fixed_chars >= max_chars {
        return footer.to_string();
    }

    let preview_chars = max_chars - fixed_chars;
    let preview = rendered[..footer_start]
        .trim_end()
        .chars()
        .take(preview_chars)
        .collect::<String>();
    format!("{preview}\n\n{notice}\n\n{footer}")
}

fn command_summary_footer_start(text: &str) -> Option<usize> {
    let mut search_end = text.len();
    while let Some(index) = text[..search_end].rfind(COMMAND_SUMMARY_MARKER) {
        let summary = text[index + COMMAND_SUMMARY_MARKER.len()..].trim_start_matches(['\r', '\n']);
        if has_command_summary_fields(summary) {
            return Some(index);
        }
        search_end = index;
    }
    None
}

fn has_command_summary_fields(summary: &str) -> bool {
    let mut lines = summary.lines().map(str::trim);
    let Some(command) = lines.next() else {
        return false;
    };
    let Some(exit_code) = lines.next() else {
        return false;
    };
    let mut next = lines.next();

    if next.is_some_and(|line| line.starts_with("signal: ")) {
        next = lines.next();
    }
    let Some(timed_out) = next else {
        return false;
    };
    next = lines.next();
    if next.is_some_and(|line| line.starts_with("timeout_seconds: ")) {
        next = lines.next();
    }
    let Some(duration) = next else {
        return false;
    };
    let Some(stdout_bytes) = lines.next() else {
        return false;
    };
    let Some(stderr_bytes) = lines.next() else {
        return false;
    };
    let Some(combined_output_chars) = lines.next() else {
        return false;
    };
    next = lines.next();
    if next.is_some_and(|line| line.starts_with("saved_output: ")) {
        next = lines.next();
    }
    matches!(
        (
            command.starts_with("command: "),
            exit_code.starts_with("exit_code: "),
            timed_out.starts_with("timed_out: "),
            duration.starts_with("duration: "),
            stdout_bytes.starts_with("stdout_bytes: "),
            stderr_bytes.starts_with("stderr_bytes: "),
            combined_output_chars.starts_with("combined_output_chars: "),
            next,
        ),
        (
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            Some("last_output:")
        )
    )
}

/// Synthetic tool results for a batch dropped (or never started) by mid-batch
/// cancellation. The assistant message already references every tool-call id, so
/// each one still needs a reply or the next request is malformed; the UI's
/// active tools are cleared wholesale on interrupt, so no per-tool finish is
/// emitted here.
pub(super) fn interrupted_tool_results(
    batch: Vec<ToolCall>,
) -> Vec<(ToolCall, ToolOutput, crate::output::ToolExecutionStatus)> {
    batch
        .into_iter()
        .map(|tool_call| {
            (
                tool_call,
                ToolOutput::Text("Error: tool interrupted before completion.".to_string()),
                crate::output::ToolExecutionStatus::Interrupted,
            )
        })
        .collect()
}

pub(super) fn skipped_tool_results(
    calls: Vec<ToolCall>,
    message: &str,
) -> Vec<(ToolCall, ToolOutput, crate::output::ToolExecutionStatus)> {
    calls
        .into_iter()
        .map(|call| {
            (
                call,
                ToolOutput::Text(message.to_string()),
                crate::output::ToolExecutionStatus::Skipped,
            )
        })
        .collect()
}

/// Build the synthetic user message that carries an image tool result back to
/// the model as a base64 data URI.
pub(super) fn image_user_message(
    mime_type: &str,
    base64_data: &str,
) -> ChatCompletionRequestMessage {
    let data_uri = format!("data:{mime_type};base64,{base64_data}");
    ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content: ChatCompletionRequestUserMessageContent::Array(vec![
            ChatCompletionRequestUserMessageContentPart::Text(
                ChatCompletionRequestMessageContentPartText {
                    text: "Here is the image content you requested to view:".to_string(),
                },
            ),
            ChatCompletionRequestUserMessageContentPart::ImageUrl(
                ChatCompletionRequestMessageContentPartImage {
                    image_url: ImageUrl {
                        url: data_uri,
                        detail: None,
                    },
                },
            ),
        ]),
        name: None,
    })
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::output::{OutputSink, SharedSink};
    use crate::tool::ToolOutput;
    use crate::tool::schema::{
        array_property, bounded_integer_property, closed_object, string_property,
    };

    /// Stub tool with a schema rich enough to exercise every repair rule
    /// through the real `parse_tool_arguments` path.
    struct RepairableTool;

    #[async_trait]
    impl Tool for RepairableTool {
        fn name(&self) -> &str {
            "repairable"
        }

        fn description(&self) -> &str {
            "test stub"
        }

        fn parameters_schema(&self) -> Value {
            closed_object(
                [
                    (
                        "files",
                        array_property("Files", string_property("File path")),
                    ),
                    ("count", bounded_integer_property("Count", Some(1), None)),
                    ("note", string_property("Optional note")),
                ],
                &["files"],
            )
        }

        async fn execute(&self, _args: Value) -> anyhow::Result<ToolOutput> {
            unreachable!("parse-only tests never execute the tool")
        }
    }

    #[test]
    fn parse_tool_arguments_valid_args_are_returned_identical() {
        let args = r#"{"files": ["a.rs", "b.rs"], "count": 2}"#;

        let value = parse_tool_arguments(&RepairableTool, "repairable", args)
            .expect("valid arguments should parse");

        let direct = serde_json::from_str::<Value>(args).expect("test input is valid JSON");
        assert_eq!(value, direct, "conformant arguments must be untouched");
    }

    #[test]
    fn parse_tool_arguments_repairs_deepseek_shaped_payload() {
        // The three headline DeepSeek shapes in one call: bare string for an
        // array, quoted integer, and null for an optional field.
        let args = r#"{"files": "src/main.rs", "count": "5", "note": null}"#;

        let value = parse_tool_arguments(&RepairableTool, "repairable", args)
            .expect("repairable arguments should parse after repair");

        assert_eq!(value, json!({"files": ["src/main.rs"], "count": 5}));
    }

    #[test]
    fn parse_tool_arguments_unwraps_double_encoded_payload() {
        let args = r#""{\"files\": [\"a.rs\"]}""#;

        let value = parse_tool_arguments(&RepairableTool, "repairable", args)
            .expect("double-encoded object should unwrap and parse");

        assert_eq!(value, json!({"files": ["a.rs"]}));
    }

    #[test]
    fn parse_tool_arguments_error_mentions_attempted_repairs() {
        // Unwraps to an object, but the required `files` field is absent —
        // the guidance must note the unwrap so "Got: <a string>" makes sense.
        let args = r#""{\"count\": 5}""#;

        let message = parse_tool_arguments(&RepairableTool, "repairable", args)
            .expect_err("missing required field should still fail");

        assert!(message.contains("Missing fields: files"), "{message}");
        assert!(
            message.contains("Attempted repairs before validation:"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn parse_tool_arguments_maps_read_line_range_aliases() {
        // Exact shape observed in the tool-call log: `start_line`/`end_line`
        // (as quoted numbers) imported from the `read_region` convention,
        // rejected by `read`'s closed schema before the per-tool coercion
        // hook existed.
        let fixture = crate::tool::test_utils::TestFixture::new();
        let tool =
            crate::tool::ReadTool::new(fixture.project_root.clone(), fixture.read_tracker.clone());
        let args = r#"{"end_line": "120", "path": "src/tui/event.rs", "start_line": "32"}"#;

        let value = parse_tool_arguments(&tool, "read", args)
            .expect("line-range aliases should coerce onto offset/limit");

        assert_eq!(
            value,
            json!({"path": "src/tui/event.rs", "offset": 32, "limit": 89})
        );
    }

    #[test]
    fn parse_tool_arguments_plain_string_payload_still_rejected() {
        let message = parse_tool_arguments(&RepairableTool, "repairable", r#""not json""#)
            .expect_err("a non-object payload should be rejected");

        assert!(message.contains("must be a JSON object"), "{message}");
        assert!(
            !message.contains("Attempted repairs"),
            "no repair fired, none should be reported: {message}"
        );
    }

    #[derive(Default)]
    struct FinishedSink {
        results: std::sync::Mutex<Vec<String>>,
    }

    impl FinishedSink {
        fn result(&self) -> String {
            self.results
                .lock()
                .expect("finished sink mutex should not be poisoned")
                .last()
                .cloned()
                .expect("tool completion should be captured")
        }
    }

    impl OutputSink for FinishedSink {
        fn tool_finished(
            &self,
            _id: &str,
            result: &str,
            _status: crate::output::ToolExecutionStatus,
        ) {
            self.results
                .lock()
                .expect("finished sink mutex should not be poisoned")
                .push(result.to_string());
        }
    }

    fn command_summary_footer() -> String {
        [
            "[Command summary]",
            "command: printf long-output",
            "exit_code: 0",
            "timed_out: false",
            "duration: 120ms",
            "stdout_bytes: 5000",
            "stderr_bytes: 0",
            "combined_output_chars: 5000",
            "last_output:",
            "tail",
        ]
        .join("\n")
    }

    #[test]
    fn command_completion_truncation_preserves_summary_footer() {
        let sink = std::sync::Arc::new(FinishedSink::default());
        let shared_sink: SharedSink = sink.clone();
        let body = "x".repeat(4_500);
        let rendered = format!("{body}\n\n{}", command_summary_footer());
        let output = ToolOutput::Command {
            rendered,
            stdout: body,
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
            truncation: None,
        };
        let tool_call = ToolCall {
            id: "call-1".to_string(),
            name: "bash".to_string(),
            arguments: "{}".to_string(),
        };

        report_tool_completion(
            &shared_sink,
            &tool_call,
            &output,
            crate::output::ToolExecutionStatus::Succeeded,
        );

        let result = sink.result();
        assert!(
            result.contains("command summary preserved below"),
            "large command output should be body-truncated with footer preserved: {result}"
        );
        assert!(
            result.contains("[Command summary]\ncommand: printf long-output\nexit_code: 0"),
            "footer should remain parseable after completion truncation: {result}"
        );
        assert!(
            result.ends_with("last_output:\ntail"),
            "footer should remain the final section: {result}"
        );
    }

    fn run_git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("run git");
        assert!(status.success());
    }

    #[tokio::test]
    async fn verification_identity_distinguishes_index_and_untracked_inputs() {
        let root = tempfile::tempdir().expect("temp repository");
        run_git(root.path(), &["init", "-q"]);
        run_git(root.path(), &["config", "user.email", "test@example.com"]);
        run_git(root.path(), &["config", "user.name", "Test"]);
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").expect("manifest");
        run_git(root.path(), &["add", "Cargo.toml"]);
        run_git(root.path(), &["commit", "-qm", "initial"]);

        let clean =
            capture_verification_workspace_binding(root.path(), root.path(), "cargo test --locked")
                .await;
        std::fs::write(root.path().join("input.rs"), "untracked").expect("untracked input");
        let untracked =
            capture_verification_workspace_binding(root.path(), root.path(), "cargo test --locked")
                .await;
        assert_ne!(clean, untracked);

        run_git(root.path(), &["add", "input.rs"]);
        let indexed =
            capture_verification_workspace_binding(root.path(), root.path(), "cargo test --locked")
                .await;
        assert_ne!(untracked, indexed);

        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("dirty tracked manifest");
        let dirty =
            capture_verification_workspace_binding(root.path(), root.path(), "cargo test --locked")
                .await;
        assert_ne!(indexed, dirty, "equal HEAD must not hide tracked dirt");
    }

    #[tokio::test]
    async fn verification_identity_ignores_unrelated_untracked_notes() {
        let root = tempfile::tempdir().expect("temp repository");
        run_git(root.path(), &["init", "-q"]);
        run_git(root.path(), &["config", "user.email", "test@example.com"]);
        run_git(root.path(), &["config", "user.name", "Test"]);
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").expect("manifest");
        run_git(root.path(), &["add", "Cargo.toml"]);
        run_git(root.path(), &["commit", "-qm", "initial"]);

        let clean =
            capture_verification_workspace_binding(root.path(), root.path(), "cargo test --locked")
                .await;
        std::fs::write(root.path().join("scratch-notes.md"), "not a test input")
            .expect("unrelated note");
        let with_note =
            capture_verification_workspace_binding(root.path(), root.path(), "cargo test --locked")
                .await;

        assert_eq!(clean, with_note);
    }

    #[tokio::test]
    async fn verification_identity_supports_unversioned_projects() {
        let root = tempfile::tempdir().expect("temp project");
        std::fs::write(root.path().join("main.py"), "print('first')\n").expect("source");
        let first =
            capture_verification_workspace_binding(root.path(), root.path(), "python -m pytest")
                .await;
        assert!(matches!(first, VerificationBinding::Bound { .. }));

        std::fs::write(root.path().join("main.py"), "print('second')\n").expect("changed source");
        let second =
            capture_verification_workspace_binding(root.path(), root.path(), "python -m pytest")
                .await;
        assert_ne!(first, second);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verification_tool_fingerprint_never_executes_the_candidate() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temp project");
        let tool = root.path().join("verify-tool");
        let marker = root.path().join("tool-was-executed");
        std::fs::write(
            &tool,
            format!("#!/bin/sh\ntouch {:?}\n", marker.to_string_lossy()),
        )
        .expect("tool script");
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
            .expect("executable tool");

        let first =
            capture_verification_workspace_binding(root.path(), root.path(), "./verify-tool test")
                .await;
        assert!(matches!(first, VerificationBinding::Bound { .. }));
        assert!(
            !marker.exists(),
            "fingerprinting must never execute an unapproved command"
        );

        std::fs::write(&tool, "#!/bin/sh\nexit 2\n").expect("changed tool");
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
            .expect("executable tool");
        let second =
            capture_verification_workspace_binding(root.path(), root.path(), "./verify-tool test")
                .await;
        assert_ne!(first, second, "toolchain changes must alter the binding");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verification_binding_never_runs_git_external_diff_drivers() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("temp repository");
        run_git(root.path(), &["init", "-q"]);
        run_git(root.path(), &["config", "user.email", "test@example.com"]);
        run_git(root.path(), &["config", "user.name", "Test"]);
        std::fs::write(root.path().join("Cargo.toml"), "[workspace]\n").expect("manifest");
        run_git(root.path(), &["add", "Cargo.toml"]);
        run_git(root.path(), &["commit", "-qm", "initial"]);

        let marker = root.path().join("external-diff-ran");
        let driver = root.path().join("external-diff");
        std::fs::write(
            &driver,
            format!("#!/bin/sh\ntouch {:?}\n", marker.to_string_lossy()),
        )
        .expect("external diff driver");
        std::fs::set_permissions(&driver, std::fs::Permissions::from_mode(0o755))
            .expect("executable driver");
        run_git(
            root.path(),
            &[
                "config",
                "diff.external",
                driver.to_str().expect("utf-8 driver path"),
            ],
        );
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = []\n",
        )
        .expect("dirty manifest");

        let binding =
            capture_verification_workspace_binding(root.path(), root.path(), "cargo test --locked")
                .await;

        assert!(matches!(binding, VerificationBinding::Bound { .. }));
        assert!(
            !marker.exists(),
            "workspace fingerprinting must disable Git external diff execution"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fingerprinting_a_device_symlink_never_reads_the_device() {
        let root = tempfile::tempdir().expect("temp project");
        let link = root.path().join("device-link");
        std::os::unix::fs::symlink("/dev/zero", &link).expect("device symlink");

        let fingerprint =
            tokio::time::timeout(std::time::Duration::from_secs(1), fingerprint_path(&link))
                .await
                .expect("device fingerprint must remain bounded");

        assert!(fingerprint.contains("non-regular-target"), "{fingerprint}");
    }
}
