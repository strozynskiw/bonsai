use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::storage::{NewRecoveryPoint, RecoveryId, RecoveryPoint, RecoveryState, Storage};
use crate::tool::ApprovalLevel;
use crate::util::time::now_ms;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const RECOVERY_REF_ROOT: &str = "refs/bonsai/recovery";
const RECOVERY_STARTUP_GRACE_MS: i64 = 30_000;
static RECOVERY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RecoveryMode {
    #[default]
    Auto,
    Off,
    Worktree,
}

impl RecoveryMode {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "off" | "none" => Some(Self::Off),
            "worktree" | "on" => Some(Self::Worktree),
            _ => None,
        }
    }

    pub(crate) fn should_isolate(self, autonomy: ApprovalLevel) -> bool {
        match self {
            Self::Auto => autonomy >= ApprovalLevel::Balanced,
            Self::Off => false,
            Self::Worktree => true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryCommand {
    List,
    Inspect {
        id: RecoveryId,
    },
    Merge {
        id: RecoveryId,
    },
    Keep {
        id: RecoveryId,
        branch: Option<String>,
    },
    Discard {
        id: RecoveryId,
    },
}

#[derive(Debug)]
pub(crate) struct RecoveryWorkspace {
    storage: Storage,
    point: RecoveryPoint,
    execution_path: PathBuf,
}

impl RecoveryWorkspace {
    pub(crate) async fn prepare(
        storage: Storage,
        project_path: &Path,
        mode: RecoveryMode,
        autonomy: ApprovalLevel,
    ) -> Result<Option<Self>> {
        if !mode.should_isolate(autonomy) {
            return Ok(None);
        }

        let project_path = canonical_path(project_path)?;
        let project_id = storage.ensure_project(&project_path).await?;
        let trust =
            crate::workspace_trust::WorkspaceTrustGate::load(storage.clone(), project_id).await?;
        anyhow::ensure!(
            trust.state() == crate::workspace_trust::WorkspaceTrust::Trusted,
            "Recovery isolation requires a trusted workspace. Trust this project in an interactive session first, or use --isolation off to opt out."
        );
        let repository_path = match repository_root(&project_path).await {
            Ok(path) => path,
            Err(err) if mode == RecoveryMode::Auto => {
                anyhow::bail!(
                    "Automatic recovery isolation requires a Git repository: {err}. \
                     Use --isolation off to run without a recovery boundary."
                );
            }
            Err(err) => return Err(err),
        };
        let home_dir = canonical_path(storage.home_dir())?;
        anyhow::ensure!(
            !home_dir.starts_with(&repository_path),
            "Bonsai home {} is inside repository {}. Move BONSAI_HOME outside the repository or use --isolation off.",
            home_dir.display(),
            repository_path.display()
        );
        let relative_project_path =
            project_path
                .strip_prefix(&repository_path)
                .with_context(|| {
                    format!(
                        "Project path {} is not inside repository {}",
                        project_path.display(),
                        repository_path.display()
                    )
                })?;
        let id = generated_recovery_id(&repository_path);
        let baseline_ref = format!("{RECOVERY_REF_ROOT}/{id}/baseline");
        let worktree_path = managed_worktree_path(storage.home_dir(), &repository_path, &id);
        let index_path = managed_index_path(storage.home_dir(), &id, "baseline");
        let original_head =
            optional_git_stdout(&repository_path, &["rev-parse", "--verify", "HEAD"]).await?;
        let source_index_tree = run_git_stdout(&repository_path, &["write-tree"])
            .await
            .with_context(
                || "Cannot create a recovery point while the Git index contains unresolved changes",
            )?;
        let baseline_commit = snapshot_commit(
            &repository_path,
            &index_path,
            original_head.as_deref(),
            &baseline_ref,
            &format!("Bonsai recovery {id}: baseline"),
        )
        .await?;

        if let Some(parent) = worktree_path.parent() {
            tokio::fs::create_dir_all(parent).await.with_context(|| {
                format!("Failed to create recovery directory {}", parent.display())
            })?;
        }
        if let Err(err) = run_git(
            &repository_path,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("--detach"),
                worktree_path.as_os_str().to_owned(),
                OsString::from(&baseline_commit),
            ],
            &[],
        )
        .await
        {
            if let Err(err) = delete_ref(&repository_path, &baseline_ref).await {
                tracing::warn!(%err, "failed to clean up baseline ref after worktree creation error");
            }
            return Err(err.context("Failed to create isolated recovery worktree"));
        }

        let new_point = NewRecoveryPoint {
            id: &id,
            project_path: &project_path,
            repository_path: &repository_path,
            worktree_path: &worktree_path,
            baseline_ref: &baseline_ref,
            source_index_tree: &source_index_tree,
        };
        if let Err(err) = storage.insert_recovery_point(new_point).await {
            if let Err(err) = remove_worktree(&repository_path, &worktree_path).await {
                tracing::warn!(%err, "failed to clean up worktree after recovery point insertion error");
            }
            if let Err(err) = delete_ref(&repository_path, &baseline_ref).await {
                tracing::warn!(%err, "failed to clean up baseline ref after recovery point insertion error");
            }
            return Err(err);
        }
        let execution_path = worktree_path.join(relative_project_path);
        tokio::fs::create_dir_all(&execution_path)
            .await
            .with_context(|| {
                format!(
                    "Failed to create isolated project path {}",
                    execution_path.display()
                )
            })?;
        let point = storage.recovery_point(&id).await?;
        Ok(Some(Self {
            storage,
            point,
            execution_path,
        }))
    }

    pub(crate) fn id(&self) -> &RecoveryId {
        &self.point.id
    }

    pub(crate) fn execution_path(&self) -> &Path {
        &self.execution_path
    }

    pub(crate) fn source_path(&self) -> &Path {
        &self.point.project_path
    }

    pub(crate) async fn finalize(&mut self) -> Result<()> {
        match capture_result(&self.storage, &self.point).await {
            Ok(point) => {
                self.point = point;
                Ok(())
            }
            Err(err) => {
                let message = format!("{err:#}");
                if let Err(mark_err) = self
                    .storage
                    .mark_recovery_state(
                        &self.point.id,
                        RecoveryState::Failed,
                        None,
                        Some(&message),
                        false,
                    )
                    .await
                {
                    tracing::warn!(error = %mark_err, "failed to persist recovery capture failure");
                }
                Err(err)
            }
        }
    }
}

pub(crate) async fn execute(command: RecoveryCommand) -> Result<()> {
    let storage = Storage::open().await?;
    let result = match command {
        RecoveryCommand::List => list(&storage).await,
        RecoveryCommand::Inspect { id } => inspect(&storage, &id).await,
        RecoveryCommand::Merge { id } => merge(&storage, &id).await,
        RecoveryCommand::Keep { id, branch } => keep(&storage, &id, branch.as_deref()).await,
        RecoveryCommand::Discard { id } => discard(&storage, &id).await,
    };
    storage.close().await;
    result
}

pub(crate) async fn run_tui(
    resume: Option<crate::cli::ResumeTarget>,
    mode: RecoveryMode,
    accessibility: crate::tui::accessibility::TuiAccessibility,
) -> Result<()> {
    let storage = Storage::open().await?;
    let source_path = std::env::current_dir().context("Failed to determine current directory")?;
    let autonomy = storage
        .approval_level()
        .await?
        .unwrap_or(ApprovalLevel::Balanced);
    let mut recovery = RecoveryWorkspace::prepare(storage, &source_path, mode, autonomy).await?;
    let context = recovery.as_ref().map(|workspace| {
        crate::bootstrap::TuiRunContext::for_recovery(
            workspace.execution_path().to_path_buf(),
            workspace.source_path().to_path_buf(),
            workspace.id().clone(),
        )
    });
    if let Some(workspace) = recovery.as_ref() {
        eprintln!(
            "Recovery {}: interactive session isolated in {}",
            workspace.id(),
            workspace.execution_path().display()
        );
    }
    let run_result = crate::bootstrap::run_tui(resume, context, accessibility).await;
    if let Some(workspace) = recovery.as_mut() {
        if let Err(finalize_err) = workspace.finalize().await {
            if run_result.is_ok() {
                return Err(finalize_err);
            }
            tracing::warn!(error = %finalize_err, "failed to finalize interactive recovery point after TUI failure");
        } else {
            eprintln!(
                "Recovery {} ready. Inspect with: bonsai recovery inspect {}",
                workspace.id(),
                workspace.id()
            );
        }
    }
    run_result
}

async fn list(storage: &Storage) -> Result<()> {
    let points = storage.list_recovery_points(100).await?;
    if points.is_empty() {
        println!("No recovery points.");
        return Ok(());
    }
    println!("ID\tSTATE\tSESSION\tNEXT\tPROJECT");
    for point in points {
        let activity = recovery_activity(storage, &point).await?;
        let session = point
            .session_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{}\t{}\t{}\t{}\t{}",
            point.id,
            activity.label(),
            session,
            activity.next_action(),
            point.project_path.display()
        );
    }
    Ok(())
}

async fn inspect(storage: &Storage, id: &RecoveryId) -> Result<()> {
    let point = ensure_result(storage, id).await?;
    let result_ref = point
        .result_ref
        .as_deref()
        .context("Recovery point has no captured result")?;
    let stat = run_git_stdout(
        &point.repository_path,
        &["diff", "--stat", &point.baseline_ref, result_ref],
    )
    .await?;
    let names = run_git_stdout(
        &point.repository_path,
        &["diff", "--name-status", &point.baseline_ref, result_ref],
    )
    .await?;
    println!("Recovery: {}", point.id);
    println!("State: {}", point.state.as_db_str());
    println!("Project: {}", point.project_path.display());
    println!(
        "Session: {}",
        point
            .session_id
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    if stat.is_empty() {
        println!("Changes: none");
    } else {
        println!("\n{stat}");
        if !names.is_empty() {
            println!("\n{names}");
        }
    }
    if let Some(path) = point.worktree_path {
        println!("\nWorktree: {}", path.display());
    }
    if let Some(branch) = point.branch_name {
        println!("Branch: {branch}");
    }
    if let Some(error) = point.error {
        println!("Error: {error}");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryActivity {
    Starting,
    Live,
    Stale,
    Ready,
    Failed,
    Merged,
    Kept,
    Discarded,
}

impl RecoveryActivity {
    const fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Live => "live",
            Self::Stale => "stale",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Merged => "merged",
            Self::Kept => "kept",
            Self::Discarded => "discarded",
        }
    }

    const fn next_action(self) -> &'static str {
        match self {
            Self::Starting | Self::Live => "wait",
            Self::Stale | Self::Failed => "inspect",
            Self::Ready => "merge|keep|discard",
            Self::Merged | Self::Kept | Self::Discarded => "-",
        }
    }
}

async fn recovery_activity(storage: &Storage, point: &RecoveryPoint) -> Result<RecoveryActivity> {
    match point.state {
        RecoveryState::Active => {
            if let Some(session_id) = point.session_id
                && storage.is_session_live(session_id).await?
            {
                return Ok(RecoveryActivity::Live);
            }
            if point.session_id.is_none()
                && point.updated_at_ms >= now_ms().saturating_sub(RECOVERY_STARTUP_GRACE_MS)
            {
                Ok(RecoveryActivity::Starting)
            } else {
                Ok(RecoveryActivity::Stale)
            }
        }
        RecoveryState::Ready => Ok(RecoveryActivity::Ready),
        RecoveryState::Failed => Ok(RecoveryActivity::Failed),
        RecoveryState::Merged => Ok(RecoveryActivity::Merged),
        RecoveryState::Kept => Ok(RecoveryActivity::Kept),
        RecoveryState::Discarded => Ok(RecoveryActivity::Discarded),
    }
}

async fn merge(storage: &Storage, id: &RecoveryId) -> Result<()> {
    let point = ensure_result(storage, id).await?;
    ensure_actionable(&point)?;
    let result_ref = point
        .result_ref
        .as_deref()
        .context("Recovery point has no captured result")?;
    let source_match = verify_source_state(storage.home_dir(), &point, result_ref).await?;

    let patch = run_git_bytes(
        &point.repository_path,
        [
            OsString::from("diff"),
            OsString::from("--binary"),
            OsString::from("--full-index"),
            OsString::from(&point.baseline_ref),
            OsString::from(result_ref),
        ],
        &[],
    )
    .await?;
    if source_match == SourceMatch::Baseline && !patch.is_empty() {
        run_git_with_input(&point.repository_path, &["apply", "--check"], &patch).await?;
        run_git_with_input(&point.repository_path, &["apply"], &patch).await?;
    }
    cleanup_managed_recovery(&point).await?;
    storage
        .mark_recovery_state(id, RecoveryState::Merged, None, None, true)
        .await?;
    println!(
        "Merged recovery {id} into {}.",
        point.project_path.display()
    );
    Ok(())
}

async fn keep(storage: &Storage, id: &RecoveryId, requested_branch: Option<&str>) -> Result<()> {
    let point = ensure_result(storage, id).await?;
    ensure_actionable(&point)?;
    let result_ref = point
        .result_ref
        .as_deref()
        .context("Recovery point has no captured result")?;
    let branch = requested_branch
        .map(str::to_string)
        .unwrap_or_else(|| format!("bonsai/recovery-{}", short_id(id)));
    run_git_stdout(
        &point.repository_path,
        &["check-ref-format", "--branch", &branch],
    )
    .await?;
    run_git(
        &point.repository_path,
        [
            OsString::from("branch"),
            OsString::from(&branch),
            OsString::from(result_ref),
        ],
        &[],
    )
    .await?;
    cleanup_managed_recovery(&point).await?;
    storage
        .mark_recovery_state(id, RecoveryState::Kept, Some(&branch), None, true)
        .await?;
    println!("Kept recovery {id} as branch {branch}.");
    Ok(())
}

async fn discard(storage: &Storage, id: &RecoveryId) -> Result<()> {
    let point = storage.recovery_point(id).await?;
    ensure_actionable(&point)?;
    ensure_not_live(storage, &point).await?;
    cleanup_managed_recovery(&point).await?;
    storage
        .mark_recovery_state(id, RecoveryState::Discarded, None, None, true)
        .await?;
    println!("Discarded recovery {id}.");
    Ok(())
}

async fn ensure_result(storage: &Storage, id: &RecoveryId) -> Result<RecoveryPoint> {
    let point = storage.recovery_point(id).await?;
    match point.state {
        RecoveryState::Active => {
            ensure_not_live(storage, &point).await?;
            capture_result(storage, &point).await
        }
        RecoveryState::Failed => capture_result(storage, &point).await,
        _ => Ok(point),
    }
}

async fn ensure_not_live(storage: &Storage, point: &RecoveryPoint) -> Result<()> {
    let activity = recovery_activity(storage, point).await?;
    anyhow::ensure!(
        !matches!(
            activity,
            RecoveryActivity::Starting | RecoveryActivity::Live
        ),
        "Recovery {} is still {}; wait for the run to finish before changing it",
        point.id,
        activity.label()
    );
    Ok(())
}

async fn capture_result(storage: &Storage, point: &RecoveryPoint) -> Result<RecoveryPoint> {
    ensure_actionable(point)?;
    let worktree_path = point
        .worktree_path
        .as_deref()
        .context("Recovery worktree is no longer available")?;
    anyhow::ensure!(
        worktree_path.is_dir(),
        "Recovery worktree {} is missing",
        worktree_path.display()
    );
    let result_ref = format!("{RECOVERY_REF_ROOT}/{}/result", point.id);
    let index_path = managed_index_path(storage.home_dir(), &point.id, "result");
    snapshot_commit(
        worktree_path,
        &index_path,
        Some(&point.baseline_ref),
        &result_ref,
        &format!("Bonsai recovery {}: result", point.id),
    )
    .await?;
    storage.mark_recovery_ready(&point.id, &result_ref).await?;
    storage.recovery_point(&point.id).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceMatch {
    Baseline,
    Result,
}

async fn verify_source_state(
    home_dir: &Path,
    point: &RecoveryPoint,
    result_ref: &str,
) -> Result<SourceMatch> {
    let current_index_tree = run_git_stdout(&point.repository_path, &["write-tree"])
        .await
        .context("Cannot verify the source Git index")?;
    anyhow::ensure!(
        current_index_tree == point.source_index_tree,
        "The source Git index changed after recovery {} started. Refusing to merge; use `bonsai recovery keep {}` and integrate the branch manually.",
        point.id,
        point.id
    );
    let index_path = managed_index_path(home_dir, &point.id, "verify");
    let current_tree = snapshot_tree(&point.repository_path, &index_path).await?;
    let baseline_tree = run_git_stdout(
        &point.repository_path,
        &["rev-parse", &format!("{}^{{tree}}", point.baseline_ref)],
    )
    .await?;
    if current_tree == baseline_tree {
        return Ok(SourceMatch::Baseline);
    }
    let result_tree = run_git_stdout(
        &point.repository_path,
        &["rev-parse", &format!("{result_ref}^{{tree}}")],
    )
    .await?;
    if current_tree == result_tree {
        return Ok(SourceMatch::Result);
    }
    anyhow::bail!(
        "The source working tree changed after recovery {} started. Refusing to merge; use `bonsai recovery keep {}` and integrate the branch manually.",
        point.id,
        point.id
    )
}

fn ensure_actionable(point: &RecoveryPoint) -> Result<()> {
    anyhow::ensure!(
        matches!(
            point.state,
            RecoveryState::Active | RecoveryState::Ready | RecoveryState::Failed
        ),
        "Recovery {} is already {}",
        point.id,
        point.state.as_db_str()
    );
    Ok(())
}

async fn cleanup_managed_recovery(point: &RecoveryPoint) -> Result<()> {
    if let Some(worktree_path) = point.worktree_path.as_deref() {
        remove_worktree(&point.repository_path, worktree_path).await?;
    }
    if let Some(result_ref) = point.result_ref.as_deref() {
        delete_ref(&point.repository_path, result_ref).await?;
    }
    delete_ref(&point.repository_path, &point.baseline_ref).await?;
    Ok(())
}

async fn repository_root(project_path: &Path) -> Result<PathBuf> {
    let root = run_git_stdout(project_path, &["rev-parse", "--show-toplevel"])
        .await
        .with_context(|| format!("{} is not a Git repository", project_path.display()))?;
    canonical_path(Path::new(&root))
}

fn canonical_path(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .with_context(|| format!("Failed to resolve path {}", path.display()))
}

fn generated_recovery_id(repository_path: &Path) -> RecoveryId {
    let timestamp = now_ms().max(0);
    let sequence = RECOVERY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let repository_hash = blake3::hash(repository_path.as_os_str().as_encoded_bytes());
    RecoveryId::from_generated(format!(
        "{timestamp}-{}-{sequence}-{}",
        std::process::id(),
        &repository_hash.to_hex()[..8]
    ))
}

fn short_id(id: &RecoveryId) -> &str {
    let value = id.as_str();
    let start = value.len().saturating_sub(16);
    &value[start..]
}

fn managed_worktree_path(home_dir: &Path, repository_path: &Path, id: &RecoveryId) -> PathBuf {
    let hash = blake3::hash(repository_path.as_os_str().as_encoded_bytes());
    home_dir
        .join("worktrees")
        .join(&hash.to_hex()[..12])
        .join(id.as_str())
}

fn managed_index_path(home_dir: &Path, id: &RecoveryId, purpose: &str) -> PathBuf {
    home_dir
        .join("recovery")
        .join("indexes")
        .join(format!("{}-{purpose}.index", id.as_str()))
}

async fn snapshot_commit(
    repository_path: &Path,
    index_path: &Path,
    parent: Option<&str>,
    reference: &str,
    message: &str,
) -> Result<String> {
    let tree = snapshot_tree(repository_path, index_path).await?;
    let mut args = vec![OsString::from("commit-tree"), OsString::from(tree)];
    if let Some(parent) = parent {
        args.push(OsString::from("-p"));
        args.push(OsString::from(parent));
    }
    args.push(OsString::from("-m"));
    args.push(OsString::from(message));
    let identity = [
        ("GIT_AUTHOR_NAME", OsString::from("Bonsai Recovery")),
        ("GIT_AUTHOR_EMAIL", OsString::from("recovery@bonsai.local")),
        ("GIT_COMMITTER_NAME", OsString::from("Bonsai Recovery")),
        (
            "GIT_COMMITTER_EMAIL",
            OsString::from("recovery@bonsai.local"),
        ),
    ];
    let commit = run_git_stdout_os(repository_path, args, &identity).await?;
    run_git(
        repository_path,
        [
            OsString::from("update-ref"),
            OsString::from(reference),
            OsString::from(&commit),
        ],
        &[],
    )
    .await?;
    Ok(commit)
}

async fn snapshot_tree(repository_path: &Path, index_path: &Path) -> Result<String> {
    if let Some(parent) = index_path.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "Failed to create recovery index directory {}",
                parent.display()
            )
        })?;
    }
    remove_file_if_present(index_path).await?;
    let index_env = [("GIT_INDEX_FILE", index_path.as_os_str().to_owned())];
    let head = optional_git_stdout(repository_path, &["rev-parse", "--verify", "HEAD"]).await?;
    let initialize = if head.is_some() {
        vec![OsString::from("read-tree"), OsString::from("HEAD")]
    } else {
        vec![OsString::from("read-tree"), OsString::from("--empty")]
    };
    let result = async {
        run_git(repository_path, initialize, &index_env).await?;
        run_git(
            repository_path,
            [
                OsString::from("add"),
                OsString::from("-A"),
                OsString::from("--"),
                OsString::from("."),
            ],
            &index_env,
        )
        .await?;
        run_git_stdout_os(repository_path, [OsString::from("write-tree")], &index_env).await
    }
    .await;
    if let Err(err) = remove_file_if_present(index_path).await {
        tracing::warn!(path = %index_path.display(), error = %err, "failed to remove temporary recovery index");
    }
    result
}

async fn remove_file_if_present(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("Failed to remove {}", path.display())),
    }
}

async fn remove_worktree(repository_path: &Path, worktree_path: &Path) -> Result<()> {
    let remove_result = run_git(
        repository_path,
        [
            OsString::from("worktree"),
            OsString::from("remove"),
            OsString::from("--force"),
            worktree_path.as_os_str().to_owned(),
        ],
        &[],
    )
    .await;
    if let Err(err) = remove_result
        && worktree_path.exists()
    {
        return Err(err);
    }
    run_git(
        repository_path,
        [OsString::from("worktree"), OsString::from("prune")],
        &[],
    )
    .await
}

async fn delete_ref(repository_path: &Path, reference: &str) -> Result<()> {
    if optional_git_stdout(repository_path, &["rev-parse", "--verify", reference])
        .await?
        .is_some()
    {
        run_git(
            repository_path,
            [
                OsString::from("update-ref"),
                OsString::from("-d"),
                OsString::from(reference),
            ],
            &[],
        )
        .await?;
    }
    Ok(())
}

async fn optional_git_stdout(repository_path: &Path, args: &[&str]) -> Result<Option<String>> {
    let output = git_output(repository_path, args.iter().map(OsString::from), &[], None).await?;
    if output.status.success() {
        Ok(Some(trimmed_stdout(&output)?))
    } else {
        Ok(None)
    }
}

async fn run_git_stdout(repository_path: &Path, args: &[&str]) -> Result<String> {
    run_git_stdout_os(repository_path, args.iter().map(OsString::from), &[]).await
}

async fn run_git_stdout_os<I>(
    repository_path: &Path,
    args: I,
    env: &[(&str, OsString)],
) -> Result<String>
where
    I: IntoIterator<Item = OsString>,
{
    let output = git_output(repository_path, args, env, None).await?;
    ensure_git_success(&output)?;
    trimmed_stdout(&output)
}

async fn run_git<I>(repository_path: &Path, args: I, env: &[(&str, OsString)]) -> Result<()>
where
    I: IntoIterator<Item = OsString>,
{
    let output = git_output(repository_path, args, env, None).await?;
    ensure_git_success(&output)
}

async fn run_git_bytes<I>(
    repository_path: &Path,
    args: I,
    env: &[(&str, OsString)],
) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = OsString>,
{
    let output = git_output(repository_path, args, env, None).await?;
    ensure_git_success(&output)?;
    Ok(output.stdout)
}

async fn run_git_with_input(repository_path: &Path, args: &[&str], input: &[u8]) -> Result<()> {
    let output = git_output(
        repository_path,
        args.iter().map(OsString::from),
        &[],
        Some(input),
    )
    .await?;
    ensure_git_success(&output)
}

async fn git_output<I>(
    repository_path: &Path,
    args: I,
    env: &[(&str, OsString)],
    input: Option<&[u8]>,
) -> Result<Output>
where
    I: IntoIterator<Item = OsString>,
{
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repository_path)
        .args(args)
        .kill_on_drop(true)
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().context("Failed to start git")?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().context("Failed to open git stdin")?;
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(input)
            .await
            .context("Failed to write Git patch")?;
    }
    tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .context("Git operation timed out")?
        .context("Failed to wait for git")
}

fn ensure_git_success(output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    anyhow::bail!(
        "Git exited with {}{}",
        output.status,
        if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        }
    )
}

fn trimmed_stdout(output: &Output) -> Result<String> {
    let stdout = std::str::from_utf8(&output.stdout).context("Git returned non-UTF-8 output")?;
    Ok(stdout.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Bonsai Test")
            .env("GIT_AUTHOR_EMAIL", "test@bonsai.local")
            .env("GIT_COMMITTER_NAME", "Bonsai Test")
            .env("GIT_COMMITTER_EMAIL", "test@bonsai.local")
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn recovery_fixture() -> (tempfile::TempDir, Storage, PathBuf) {
        let root = tempfile::TempDir::new().expect("temp directory should open");
        let repository = root.path().join("project");
        std::fs::create_dir_all(&repository).expect("repository should be created");
        git(&repository, &["init", "--quiet"]);
        std::fs::write(repository.join("tracked.txt"), "committed\n")
            .expect("tracked file should be written");
        git(&repository, &["add", "tracked.txt"]);
        git(&repository, &["commit", "--quiet", "-m", "initial"]);
        std::fs::write(repository.join("tracked.txt"), "staged\n")
            .expect("staged content should be written");
        git(&repository, &["add", "tracked.txt"]);
        std::fs::write(repository.join("tracked.txt"), "working\n")
            .expect("working content should be written");
        std::fs::write(repository.join("untracked.txt"), "starting untracked\n")
            .expect("untracked file should be written");
        let storage = Storage::open_at(root.path().join("bonsai/bonsai.db"))
            .await
            .expect("storage should open");
        let project_id = storage
            .ensure_project(&repository)
            .await
            .expect("project should persist");
        crate::permissions::PermissionManager::load_workspace_trust(storage.clone(), project_id)
            .await
            .expect("trust permissions should load")
            .allow_for_project(crate::workspace_trust::PROJECT_TRUST_PATTERN)
            .await
            .expect("project should be trusted");
        (root, storage, repository)
    }

    #[test]
    fn recovery_mode_auto_only_isolates_mutating_autonomy() {
        assert!(!RecoveryMode::Auto.should_isolate(ApprovalLevel::Ask));
        assert!(!RecoveryMode::Auto.should_isolate(ApprovalLevel::Conservative));
        assert!(RecoveryMode::Auto.should_isolate(ApprovalLevel::Balanced));
        assert!(RecoveryMode::Auto.should_isolate(ApprovalLevel::AutoAccept));
        assert!(RecoveryMode::Auto.should_isolate(ApprovalLevel::Yolo));
    }

    #[test]
    fn recovery_mode_explicit_values_override_autonomy() {
        assert!(!RecoveryMode::Off.should_isolate(ApprovalLevel::Yolo));
        assert!(RecoveryMode::Worktree.should_isolate(ApprovalLevel::Ask));
    }

    #[test]
    fn generated_ids_are_valid_and_distinct() {
        let first = generated_recovery_id(Path::new("/tmp/project"));
        let second = generated_recovery_id(Path::new("/tmp/project"));
        assert_ne!(first, second);
        assert!(RecoveryId::parse(first.as_str()).is_ok());
        assert!(RecoveryId::parse(second.as_str()).is_ok());
    }

    #[tokio::test]
    async fn isolated_recovery_preserves_dirty_start_and_merges_result() {
        let (_root, storage, repository) = recovery_fixture().await;
        let index_before = run_git_stdout(&repository, &["write-tree"])
            .await
            .expect("index tree should be readable");
        let mut workspace = RecoveryWorkspace::prepare(
            storage.clone(),
            &repository,
            RecoveryMode::Worktree,
            ApprovalLevel::Ask,
        )
        .await
        .expect("recovery should prepare")
        .expect("worktree should be enabled");
        assert_eq!(
            std::fs::read_to_string(workspace.execution_path().join("tracked.txt"))
                .expect("isolated tracked file should exist"),
            "working\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.execution_path().join("untracked.txt"))
                .expect("isolated untracked file should exist"),
            "starting untracked\n"
        );
        std::fs::write(
            workspace.execution_path().join("tracked.txt"),
            "agent result\n",
        )
        .expect("isolated file should be changed");
        std::fs::write(workspace.execution_path().join("created.txt"), "created\n")
            .expect("isolated file should be created");
        workspace.finalize().await.expect("result should capture");
        let id = workspace.id().clone();

        merge(&storage, &id).await.expect("result should merge");

        assert_eq!(
            std::fs::read_to_string(repository.join("tracked.txt"))
                .expect("source tracked file should exist"),
            "agent result\n"
        );
        assert_eq!(
            std::fs::read_to_string(repository.join("created.txt"))
                .expect("source created file should exist"),
            "created\n"
        );
        assert_eq!(
            run_git_stdout(&repository, &["write-tree"])
                .await
                .expect("index tree should remain readable"),
            index_before,
            "merge-back must not modify the user's Git index"
        );
        let point = storage
            .recovery_point(&id)
            .await
            .expect("record should remain");
        assert_eq!(point.state, RecoveryState::Merged);
        assert!(point.worktree_path.is_none());
    }

    #[tokio::test]
    async fn merge_refuses_when_source_changes_after_recovery_starts() {
        let (_root, storage, repository) = recovery_fixture().await;
        let mut workspace = RecoveryWorkspace::prepare(
            storage.clone(),
            &repository,
            RecoveryMode::Worktree,
            ApprovalLevel::Ask,
        )
        .await
        .expect("recovery should prepare")
        .expect("worktree should be enabled");
        std::fs::write(
            workspace.execution_path().join("tracked.txt"),
            "agent result\n",
        )
        .expect("isolated file should be changed");
        workspace.finalize().await.expect("result should capture");
        let id = workspace.id().clone();
        std::fs::write(repository.join("tracked.txt"), "concurrent edit\n")
            .expect("source should change");

        let error = merge(&storage, &id)
            .await
            .expect_err("concurrent source edit must block merge");

        assert!(error.to_string().contains("source working tree changed"));
        discard(&storage, &id)
            .await
            .expect("blocked recovery should still be discardable");
    }

    #[tokio::test]
    async fn recovery_activity_distinguishes_starting_live_and_stale_runs() {
        let (_root, storage, repository) = recovery_fixture().await;
        let workspace = RecoveryWorkspace::prepare(
            storage.clone(),
            &repository,
            RecoveryMode::Worktree,
            ApprovalLevel::Ask,
        )
        .await
        .expect("recovery should prepare")
        .expect("worktree should be enabled");
        let id = workspace.id().clone();
        let mut point = storage
            .recovery_point(&id)
            .await
            .expect("point should load");
        assert_eq!(
            recovery_activity(&storage, &point)
                .await
                .expect("activity should resolve"),
            RecoveryActivity::Starting
        );

        let session_id = storage
            .start_session(
                &repository,
                "anthropic",
                "test-model",
                crate::provider::ReasoningSelection::default(),
            )
            .await
            .expect("session should start");
        storage
            .attach_recovery_session(&id, session_id)
            .await
            .expect("session should attach");
        storage
            .record_session_heartbeat(session_id, true)
            .await
            .expect("heartbeat should record");
        point = storage
            .recovery_point(&id)
            .await
            .expect("point should reload");
        assert_eq!(
            recovery_activity(&storage, &point)
                .await
                .expect("activity should resolve"),
            RecoveryActivity::Live
        );
        assert!(ensure_result(&storage, &id).await.is_err());

        storage
            .set_session_heartbeat_for_tests(
                session_id,
                now_ms() - crate::storage::PEER_LIVENESS_THRESHOLD_MS - 1,
            )
            .await
            .expect("heartbeat should become stale");
        point = storage
            .recovery_point(&id)
            .await
            .expect("point should reload");
        assert_eq!(
            recovery_activity(&storage, &point)
                .await
                .expect("activity should resolve"),
            RecoveryActivity::Stale
        );

        let recovered = ensure_result(&storage, &id)
            .await
            .expect("stale result should capture");
        assert_eq!(recovered.state, RecoveryState::Ready);
        discard(&storage, &id)
            .await
            .expect("recovered result should be discardable");
    }

    #[tokio::test]
    async fn workspace_session_attachment_tracks_tui_session_rotation() {
        let (_root, storage, repository) = recovery_fixture().await;
        let workspace = RecoveryWorkspace::prepare(
            storage.clone(),
            &repository,
            RecoveryMode::Worktree,
            ApprovalLevel::Ask,
        )
        .await
        .expect("recovery should prepare")
        .expect("worktree should be enabled");
        let id = workspace.id().clone();
        let session_id = storage
            .start_session(
                &repository,
                "anthropic",
                "test-model",
                crate::provider::ReasoningSelection::default(),
            )
            .await
            .expect("session should start");

        assert!(
            storage
                .attach_recovery_workspace_session(workspace.execution_path(), session_id)
                .await
                .expect("workspace attachment should resolve")
        );
        assert_eq!(
            storage
                .recovery_point(&id)
                .await
                .expect("point should reload")
                .session_id,
            Some(session_id)
        );
        discard(&storage, &id)
            .await
            .expect("recovery should be discardable");
    }
}
