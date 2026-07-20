use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use crate::diff::{DiffStats, FileDiff};
use crate::hooks::{HookContext, HookDecision, HookEngine, HookEvent};
use crate::storage::WorkspaceLockGuard;
use crate::tool::{
    ActionEffect, ActionPlan, ActionPolicy, MutationTarget, ProjectPathResolver, ReadTracker,
    ToolOutput, WorkspaceLockContext,
};
use crate::yolo::YoloMode;

static TEMP_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub(crate) enum FileMutationAction {
    Write,
    Edit,
}

impl FileMutationAction {
    fn resolve_action(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Edit => "edit",
        }
    }

    fn read_required_message(self, args_path: &str) -> String {
        match self {
            Self::Write => format!(
                "File '{args_path}' must be read before overwriting.\nUse the read tool to read the file first, then try writing again."
            ),
            Self::Edit => format!(
                "File '{args_path}' must be read before editing.\nUse the read tool to read the file first, then try editing again."
            ),
        }
    }
}

pub(crate) struct FileMutationGuard<'a> {
    project_root: &'a Path,
    read_tracker: &'a ReadTracker,
    yolo_mode: &'a YoloMode,
    hooks: &'a Arc<HookEngine>,
    workspace_locks: &'a WorkspaceLockContext,
    policy: &'a ActionPolicy,
    action: FileMutationAction,
}

impl<'a> FileMutationGuard<'a> {
    pub(crate) fn new(
        project_root: &'a Path,
        read_tracker: &'a ReadTracker,
        yolo_mode: &'a YoloMode,
        hooks: &'a Arc<HookEngine>,
        workspace_locks: &'a WorkspaceLockContext,
        policy: &'a ActionPolicy,
        action: FileMutationAction,
    ) -> Self {
        Self {
            project_root,
            read_tracker,
            yolo_mode,
            hooks,
            workspace_locks,
            policy,
            action,
        }
    }

    pub(crate) fn yolo_enabled(&self) -> bool {
        self.yolo_mode.is_enabled()
    }

    pub(crate) async fn acquire_write_lock(&self) -> Result<Option<WorkspaceLockGuard>> {
        self.workspace_locks.acquire_write().await
    }

    /// Check the complete mutation plan before the caller performs a write,
    /// delete, or rename. All file tools share this one authorization choke
    /// point through their guard.
    pub(crate) async fn authorize(&self, plan: &ActionPlan) -> Result<()> {
        self.policy.authorize(plan).await
    }

    pub(crate) fn resolve_path(&self, args_path: &str) -> Result<ResolvedMutationPath> {
        if args_path.is_empty() {
            anyhow::bail!("path is required");
        }

        let resolved = ProjectPathResolver::new(self.project_root)
            .action(self.action.resolve_action())
            .yolo(self.yolo_enabled())
            .resolve(args_path)?;
        let authorized_path = resolved
            .project_relative_path
            .as_deref()
            .or(resolved.canonical_path.as_deref())
            .unwrap_or(&resolved.path);
        let mutation_target = MutationTarget::new(args_path, authorized_path);
        Ok(ResolvedMutationPath {
            path: resolved.path,
            canonical_path: resolved.canonical_path,
            project_relative_path: resolved.project_relative_path,
            mutation_target,
        })
    }

    pub(crate) async fn read_existing_content(
        &self,
        canonical_path: &Path,
        args_path: &str,
    ) -> Result<String> {
        self.ensure_not_directory(canonical_path, args_path)?;
        let requires_read = self.yolo_mode.level().requires_read_before_write();
        if requires_read && !self.read_tracker.is_read(canonical_path).await {
            anyhow::bail!(self.action.read_required_message(args_path));
        }
        // `edit` and `apply_patch` are content-addressed: their old-string/hunk
        // context must match the on-disk text read below, so stale input becomes
        // a targeted mismatch rather than silent corruption. A whole-file write
        // has no such context, so it must retain the exact read baseline.
        if requires_read
            && matches!(self.action, FileMutationAction::Write)
            && !self.read_tracker.was_fully_read(canonical_path).await
        {
            anyhow::bail!(
                "File '{args_path}' was only read in part (a windowed or large-file read), so overwriting it could discard unseen content.\nRead the whole file first (without offset/limit), then write again — or use the edit tool for a targeted change."
            );
        }
        if requires_read
            && matches!(self.action, FileMutationAction::Write)
            && !self
                .read_tracker
                .is_unchanged_since_read(canonical_path)
                .await
        {
            anyhow::bail!(
                "File '{args_path}' changed after it was read, so overwriting it could discard newer work. Read the current file again, then retry the write."
            );
        }
        fs::read_to_string(canonical_path)
            .await
            .with_context(|| format!("Failed to read file: {args_path}"))
    }

    pub(crate) fn ensure_not_directory(
        &self,
        canonical_path: &Path,
        args_path: &str,
    ) -> Result<()> {
        if canonical_path.is_dir() {
            anyhow::bail!("Path is a directory, not a file: {}", args_path);
        }
        Ok(())
    }

    /// `tool_name` is the calling tool's registered name (`"write"`,
    /// `"edit"`, or `"apply_patch"`) — this is the single choke point all
    /// three route through, so it's the one place a `PreFileWrite`/
    /// `PostFileWrite` hook can see every mutation with per-file granularity
    /// (an `apply_patch` call touching N files fires N times, once per file).
    pub(crate) async fn write_content(
        &self,
        tool_name: &str,
        args_path: &str,
        diff: &FileDiff,
        request: FileWriteRequest<'_>,
    ) -> Result<()> {
        let workspace_lock = request.workspace_lock;
        let preflight = self.preflight_file_write(tool_name, args_path, diff);
        let preflighted = match workspace_lock {
            Some(workspace_lock) => workspace_lock.run_while_held(preflight).await?,
            None => preflight.await?,
        };
        self.commit_write_content(&preflighted, request).await?;
        let post = async {
            self.post_file_write(&preflighted).await;
            Ok(())
        };
        match workspace_lock {
            Some(workspace_lock) => workspace_lock
                .run_while_held(post)
                .await
                .context(
                    "File content committed, but PostFileWrite was cancelled after workspace lease loss",
                )?,
            None => post.await?,
        }
        Ok(())
    }

    /// Run the veto-capable hook phase for one exact proposed file diff.
    /// Multi-file callers invoke this for every path before committing any
    /// write, so a later veto cannot observe or require rollback of an earlier
    /// mutation.
    pub(crate) async fn preflight_file_write<'evidence>(
        &self,
        tool_name: &'evidence str,
        args_path: &'evidence str,
        diff: &'evidence FileDiff,
    ) -> Result<PreflightedFileWrite<'evidence>> {
        let rendered_diff = diff.render_for_hook();
        let outcome = self
            .hooks
            .fire(
                HookEvent::PreFileWrite,
                HookContext {
                    tool_name: Some(tool_name),
                    file_path: Some(Path::new(args_path)),
                    diff: Some(&rendered_diff),
                    ..Default::default()
                },
            )
            .await;
        log_hook_warnings(&outcome.warnings);
        if let HookDecision::Block { reason } = outcome.decision {
            anyhow::bail!(reason);
        }
        Ok(PreflightedFileWrite {
            tool_name,
            args_path,
            diff,
        })
    }

    /// Emit the non-veto lifecycle event only after the mutation transaction
    /// has fully committed. The evidence is the same `FileDiff` preflighted
    /// before the write.
    pub(crate) async fn post_file_write(&self, preflighted: &PreflightedFileWrite<'_>) {
        let rendered_diff = preflighted.diff.render_for_hook();
        let outcome = self
            .hooks
            .fire(
                HookEvent::PostFileWrite,
                HookContext {
                    tool_name: Some(preflighted.tool_name),
                    file_path: Some(Path::new(preflighted.args_path)),
                    diff: Some(&rendered_diff),
                    ..Default::default()
                },
            )
            .await;
        log_hook_warnings(&outcome.warnings);
    }

    /// Commit one already-authorized and already-preflighted file image.
    /// Multi-file callers are responsible for emitting post events only after
    /// every commit succeeds.
    pub(crate) async fn commit_write_content(
        &self,
        preflighted: &PreflightedFileWrite<'_>,
        request: FileWriteRequest<'_>,
    ) -> Result<()> {
        let FileWriteRequest {
            workspace_lock,
            write_path,
            project_relative_path,
            mark_read_path,
            content,
            precondition,
            write_context,
            parent_dirs,
        } = request;
        atomic_write(AtomicWriteRequest {
            project_root: self.project_root,
            write_path,
            project_relative_path,
            content,
            precondition,
            parent_dirs,
            allow_external: self.yolo_enabled(),
            workspace_lock,
        })
        .await
        .with_context(|| write_context.message(preflighted.args_path))?;

        if let Some(canonical) = mark_read_path
            .map(Path::to_path_buf)
            .or_else(|| write_path.canonicalize().ok())
        {
            self.read_tracker.mark_read(&canonical).await;
        }
        Ok(())
    }

    /// Commit one already-authorized and already-preflighted deletion.
    pub(crate) async fn commit_delete_file(
        &self,
        preflighted: &PreflightedFileWrite<'_>,
        workspace_lock: Option<&WorkspaceLockGuard>,
        path: &Path,
        project_relative_path: Option<&Path>,
        precondition: WritePrecondition<'_>,
    ) -> Result<()> {
        remove_file(
            self.project_root,
            path,
            project_relative_path,
            precondition,
            self.yolo_enabled(),
            workspace_lock,
        )
        .await
        .with_context(|| format!("Failed to delete file: {}", preflighted.args_path))?;
        Ok(())
    }
}

/// Capability proving that the veto-capable lifecycle hook accepted one exact
/// per-file diff. Its fields are private so callers cannot bypass preflight and
/// still reach the commit methods.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PreflightedFileWrite<'a> {
    tool_name: &'a str,
    args_path: &'a str,
    diff: &'a FileDiff,
}

/// One file image ready for the shared atomic commit path.
///
/// Required values are supplied at construction; optional path and lock
/// relationships are named so callers cannot silently swap same-typed
/// positional arguments.
#[derive(Debug)]
pub(crate) struct FileWriteRequest<'a> {
    workspace_lock: Option<&'a WorkspaceLockGuard>,
    write_path: &'a Path,
    project_relative_path: Option<&'a Path>,
    mark_read_path: Option<&'a Path>,
    content: &'a str,
    precondition: WritePrecondition<'a>,
    write_context: WriteContentContext,
    parent_dirs: ParentDirs,
}

impl<'a> FileWriteRequest<'a> {
    pub(crate) fn new(
        write_path: &'a Path,
        content: &'a str,
        precondition: WritePrecondition<'a>,
        write_context: WriteContentContext,
        parent_dirs: ParentDirs,
    ) -> Self {
        Self {
            workspace_lock: None,
            write_path,
            project_relative_path: None,
            mark_read_path: None,
            content,
            precondition,
            write_context,
            parent_dirs,
        }
    }

    pub(crate) fn with_workspace_lock(
        mut self,
        workspace_lock: Option<&'a WorkspaceLockGuard>,
    ) -> Self {
        self.workspace_lock = workspace_lock;
        self
    }

    pub(crate) fn with_project_relative_path(
        mut self,
        project_relative_path: Option<&'a Path>,
    ) -> Self {
        self.project_relative_path = project_relative_path;
        self
    }

    pub(crate) fn with_read_tracking_path(mut self, mark_read_path: Option<&'a Path>) -> Self {
        self.mark_read_path = mark_read_path;
        self
    }
}

fn log_hook_warnings(warnings: &[String]) {
    for warning in warnings {
        tracing::warn!(%warning, "file-mutation hook warning");
    }
}

/// Owns the shared state (`project_root`, `read_tracker`, `yolo_mode`) that the
/// write and edit tools need to build a [`FileMutationGuard`] per invocation.
pub(crate) struct FileMutationContext {
    project_root: PathBuf,
    read_tracker: ReadTracker,
    yolo_mode: YoloMode,
    hooks: Arc<HookEngine>,
    workspace_locks: WorkspaceLockContext,
    policy: ActionPolicy,
}

impl FileMutationContext {
    pub(crate) fn new_with_locks(
        project_root: PathBuf,
        read_tracker: ReadTracker,
        yolo_mode: YoloMode,
        hooks: Arc<HookEngine>,
        workspace_locks: WorkspaceLockContext,
        policy: ActionPolicy,
    ) -> Self {
        let project_root = project_root.canonicalize().unwrap_or(project_root);
        Self {
            project_root,
            read_tracker,
            yolo_mode,
            hooks,
            workspace_locks,
            policy,
        }
    }

    pub(crate) fn guard(&self, action: FileMutationAction) -> FileMutationGuard<'_> {
        FileMutationGuard::new(
            &self.project_root,
            &self.read_tracker,
            &self.yolo_mode,
            &self.hooks,
            &self.workspace_locks,
            &self.policy,
            action,
        )
    }

    /// The shared freshness ledger used to plan mutations before the guard
    /// commits them. Callers must still use [`Self::guard`] for authorization,
    /// hooks, atomic writes, and workspace locking.
    pub(crate) fn read_tracker(&self) -> &ReadTracker {
        &self.read_tracker
    }

    pub(crate) fn project_root(&self) -> &Path {
        &self.project_root
    }
}

/// State that must still hold immediately before a file mutation commits.
///
/// The final check sits after temporary-file staging and hooks, immediately
/// before the rename/remove step, so a concurrent editor cannot be silently
/// overwritten merely because the model read an older file body.
#[derive(Debug, Clone, Copy)]
pub(crate) enum WritePrecondition<'a> {
    /// No content precondition is available (used only for rollback).
    None,
    /// The destination must not exist when the mutation commits.
    Absent,
    /// The destination must still contain these exact bytes when it commits.
    Exact(&'a str),
}

struct AtomicWriteRequest<'a> {
    project_root: &'a Path,
    write_path: &'a Path,
    project_relative_path: Option<&'a Path>,
    content: &'a str,
    precondition: WritePrecondition<'a>,
    parent_dirs: ParentDirs,
    allow_external: bool,
    workspace_lock: Option<&'a WorkspaceLockGuard>,
}

async fn atomic_write(request: AtomicWriteRequest<'_>) -> Result<()> {
    let AtomicWriteRequest {
        project_root,
        write_path,
        project_relative_path,
        content,
        precondition,
        parent_dirs,
        allow_external,
        workspace_lock,
    } = request;
    let lease_fence = match workspace_lock {
        Some(workspace_lock) => Some(workspace_lock.confirm_held().await?),
        None => None,
    };
    #[cfg(unix)]
    if let Some(relative_path) = project_relative_path {
        let project_root = project_root.to_path_buf();
        let relative_path = relative_path.to_path_buf();
        let content = content.as_bytes().to_vec();
        let precondition = OwnedWritePrecondition::from(precondition);
        let create_parents = matches!(parent_dirs, ParentDirs::Create);
        let temp_name = temporary_file_name(write_path);
        let lease_fence = lease_fence.clone();
        return tokio::task::spawn_blocking(move || {
            crate::tool::secure_fs::atomic_write(
                &project_root,
                &relative_path,
                &content,
                precondition.as_borrowed(),
                create_parents,
                &temp_name,
                lease_fence.as_ref(),
            )
        })
        .await
        .context("descriptor-relative file mutation task failed")?;
    }

    #[cfg(unix)]
    if !allow_external {
        anyhow::bail!(
            "Refusing a file mutation without a descriptor-relative project path: {}",
            write_path.display()
        );
    }
    #[cfg(not(unix))]
    let _ = (project_root, project_relative_path, allow_external);
    if matches!(parent_dirs, ParentDirs::Create) {
        ensure_parent_dir(write_path).await?;
    }
    path_atomic_write(write_path, content, precondition, lease_fence.as_ref()).await
}

async fn path_atomic_write(
    write_path: &Path,
    content: &str,
    precondition: WritePrecondition<'_>,
    lease_fence: Option<&crate::storage::WorkspaceLeaseFence>,
) -> Result<()> {
    let existing_permissions = fs::metadata(write_path)
        .await
        .ok()
        .map(|metadata| metadata.permissions());
    let parent = write_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = write_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let counter = TEMP_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(
        ".{file_name}.bonsai-tmp-{}-{counter}",
        std::process::id()
    ));

    let result = async {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .await
            .with_context(|| format!("Failed to create temporary file {:?}", temp_path))?;
        if let Some(permissions) = existing_permissions {
            file.set_permissions(permissions)
                .await
                .with_context(|| format!("Failed to preserve permissions for {:?}", write_path))?;
        }
        file.write_all(content.as_bytes())
            .await
            .with_context(|| format!("Failed to write temporary file {:?}", temp_path))?;
        file.flush()
            .await
            .with_context(|| format!("Failed to flush temporary file {:?}", temp_path))?;
        file.sync_all()
            .await
            .with_context(|| format!("Failed to sync temporary file {:?}", temp_path))?;
        drop(file);
        verify_precondition(write_path, precondition).await?;
        if let Some(fence) = lease_fence {
            fence.ensure_held()?;
        }
        fs::rename(&temp_path, write_path)
            .await
            .with_context(|| format!("Failed to rename temporary file into {:?}", write_path))?;
        Ok(())
    }
    .await;

    if result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }
    result
}

async fn remove_file(
    project_root: &Path,
    path: &Path,
    project_relative_path: Option<&Path>,
    precondition: WritePrecondition<'_>,
    allow_external: bool,
    workspace_lock: Option<&WorkspaceLockGuard>,
) -> Result<()> {
    let lease_fence = match workspace_lock {
        Some(workspace_lock) => Some(workspace_lock.confirm_held().await?),
        None => None,
    };
    #[cfg(unix)]
    if let Some(relative_path) = project_relative_path {
        let project_root = project_root.to_path_buf();
        let relative_path = relative_path.to_path_buf();
        let precondition = OwnedWritePrecondition::from(precondition);
        let lease_fence = lease_fence.clone();
        return tokio::task::spawn_blocking(move || {
            crate::tool::secure_fs::remove_file(
                &project_root,
                &relative_path,
                precondition.as_borrowed(),
                lease_fence.as_ref(),
            )
        })
        .await
        .context("descriptor-relative file deletion task failed")?;
    }

    #[cfg(unix)]
    if !allow_external {
        anyhow::bail!(
            "Refusing a file deletion without a descriptor-relative project path: {}",
            path.display()
        );
    }
    #[cfg(not(unix))]
    let _ = (project_root, project_relative_path, allow_external);
    verify_precondition(path, precondition).await?;
    if let Some(fence) = lease_fence.as_ref() {
        fence.ensure_held()?;
    }
    fs::remove_file(path).await?;
    Ok(())
}

/// Restore a previously captured file image after a later mutation in the same
/// transaction fails. Rollback deliberately bypasses lifecycle hooks: hooks
/// already approved the original plan, and invoking them again while recovering
/// could turn a recoverable failure into permanent partial state.
pub(crate) async fn restore_file_content(
    project_root: &Path,
    path: &Path,
    project_relative_path: Option<&Path>,
    content: &str,
) -> Result<()> {
    atomic_write(AtomicWriteRequest {
        project_root,
        write_path: path,
        project_relative_path,
        content,
        precondition: WritePrecondition::None,
        parent_dirs: ParentDirs::Create,
        allow_external: project_relative_path.is_none(),
        workspace_lock: None,
    })
    .await
}

pub(crate) async fn remove_created_file(
    project_root: &Path,
    path: &Path,
    project_relative_path: Option<&Path>,
) -> Result<()> {
    remove_file(
        project_root,
        path,
        project_relative_path,
        WritePrecondition::None,
        project_relative_path.is_none(),
        None,
    )
    .await
}

fn temporary_file_name(write_path: &Path) -> std::ffi::OsString {
    let file_name = write_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let counter = TEMP_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".{file_name}.bonsai-tmp-{}-{counter}", std::process::id()).into()
}

#[derive(Debug, Clone)]
enum OwnedWritePrecondition {
    None,
    Absent,
    Exact(String),
}

impl OwnedWritePrecondition {
    fn as_borrowed(&self) -> WritePrecondition<'_> {
        match self {
            Self::None => WritePrecondition::None,
            Self::Absent => WritePrecondition::Absent,
            Self::Exact(content) => WritePrecondition::Exact(content),
        }
    }
}

impl From<WritePrecondition<'_>> for OwnedWritePrecondition {
    fn from(value: WritePrecondition<'_>) -> Self {
        match value {
            WritePrecondition::None => Self::None,
            WritePrecondition::Absent => Self::Absent,
            WritePrecondition::Exact(content) => Self::Exact(content.to_string()),
        }
    }
}

async fn verify_precondition(path: &Path, precondition: WritePrecondition<'_>) -> Result<()> {
    match precondition {
        WritePrecondition::None => Ok(()),
        WritePrecondition::Absent => match fs::metadata(path).await {
            Ok(_) => anyhow::bail!(
                "File changed while preparing this mutation: {} was created. Read it and retry.",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error)
                .with_context(|| format!("Failed to verify file precondition: {}", path.display())),
        },
        WritePrecondition::Exact(expected) => {
            let current = fs::read_to_string(path).await.with_context(|| {
                format!("Failed to verify file precondition: {}", path.display())
            })?;
            if current == expected {
                Ok(())
            } else {
                anyhow::bail!(
                    "File changed while preparing this mutation: {} no longer matches the content you read. Read it again and retry.",
                    path.display()
                )
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedMutationPath {
    pub(crate) path: PathBuf,
    pub(crate) canonical_path: Option<PathBuf>,
    pub(crate) project_relative_path: Option<PathBuf>,
    pub(crate) mutation_target: MutationTarget,
}

/// Whether [`FileMutationGuard::write_content`] should create any missing parent
/// directories before writing the file.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ParentDirs {
    Create,
    Keep,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum WriteContentContext {
    WriteFile,
    CreateFile,
}

impl WriteContentContext {
    fn message(self, args_path: &str) -> String {
        match self {
            Self::WriteFile => format!("Failed to write file: {args_path}"),
            Self::CreateFile => format!("Failed to create file: {args_path}"),
        }
    }
}

pub(crate) async fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    Ok(())
}

/// Effect selection shared by the whole-file `write` and `edit` tools: replacing
/// non-empty existing content with an empty body is a [`ActionEffect::Truncate`];
/// every other mutation is a plain [`ActionEffect::WorkspaceWrite`]. `old` is the
/// prior on-disk content, or `None` when the file did not exist.
pub(crate) fn write_effects(old: Option<&str>, new: &str) -> Vec<ActionEffect> {
    if old.is_some_and(|old| !old.is_empty() && new.is_empty()) {
        vec![ActionEffect::Truncate]
    } else {
        vec![ActionEffect::WorkspaceWrite]
    }
}

pub(crate) fn build_mutation_output(
    diff: FileDiff,
    summary: impl FnOnce(DiffStats) -> String,
) -> ToolOutput {
    let stats = diff.stats();
    ToolOutput::Edit {
        summary: summary(stats),
        diff,
    }
}

/// Successful output for a no-op mutation: the requested content already
/// matches what's on disk. This is almost always a duplicate retry of a
/// mutation that already landed, so the desired end state holds — reporting
/// it as an error only sends models into re-read/retry loops (observed live).
/// Callers skip the disk write entirely.
pub(crate) fn noop_mutation_output(args_path: &str) -> ToolOutput {
    ToolOutput::Text(format!(
        "File '{args_path}' already contains exactly this content — the change had already been \
         applied. The file on disk already holds this exact content, so the write was a no-op. \
         It is safe to continue."
    ))
}

/// Rejects content that is a replayed context-elision placeholder rather than
/// real text. Large write/edit bodies are elided from the conversation history
/// (`<omitted: N bytes, M lines>` / `<content elided: …>`), and a model
/// re-emitting a call from its visible history can send the placeholder itself
/// as content — observed live: a 296-line test file was
/// overwritten with the 32-byte marker string. Deterministically refuse and
/// teach recovery instead of destroying the file.
pub(crate) fn reject_replayed_elision_marker(content: &str, args_path: &str) -> Result<()> {
    let trimmed = content.trim();
    // The cap only rules out a real file that happens to be a single `<…>` line;
    // it must exceed the longest marker `omitted_text_summary` can emit (now a
    // fuller, self-describing sentence, ~230 bytes) so a verbatim replay is still
    // caught. A whole-file replay is a single line starting with the marker
    // prefix and ending in `>`.
    let looks_like_marker = trimmed.len() < 400
        && !trimmed.contains('\n')
        && trimmed.ends_with('>')
        && (trimmed.starts_with("<omitted:") || trimmed.starts_with("<content elided:"));
    if looks_like_marker {
        anyhow::bail!(
            "The content for '{args_path}' is a context-elision placeholder ({trimmed}), not real \
             file content. Your earlier call's body was elided from the conversation to save \
             context; the actual text lives in the file on disk. Read the file to get the current \
             content, or provide the full new content you intend to write."
        );
    }
    // Episode/recall markers replayed as file content. An episode marker body
    // (harness envelope + card) tops out well under this cap, and no real file
    // opens with these exact sentinel lines — same rationale as the elision
    // guard above, different placeholder family.
    let episode_marker = trimmed.len() < 4_000
        && (trimmed.starts_with(crate::agent::EPISODE_MARKER_PREFIX)
            || trimmed.starts_with("Harness note: [Episode archived]")
            || trimmed.starts_with("[reused previous recall]"));
    if episode_marker {
        anyhow::bail!(
            "The content for '{args_path}' is an archived-context marker, not real file content. \
             That text stands in for conversation history that was archived; the actual file \
             content lives on disk. Read the file to get the current content, or provide the full \
             new content you intend to write."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_utils::TestStorage;
    use crate::tool::test_utils::TestFixture;

    #[test]
    fn file_write_request_keeps_optional_path_roles_distinct() {
        let write_path = Path::new("/tmp/write-target");
        let project_relative_path = Path::new("src/lib.rs");
        let read_tracking_path = Path::new("/project/src/lib.rs");

        let request = FileWriteRequest::new(
            write_path,
            "content",
            WritePrecondition::Absent,
            WriteContentContext::CreateFile,
            ParentDirs::Create,
        )
        .with_project_relative_path(Some(project_relative_path))
        .with_read_tracking_path(Some(read_tracking_path));

        assert_eq!(request.write_path, write_path);
        assert_eq!(request.project_relative_path, Some(project_relative_path));
        assert_eq!(request.mark_read_path, Some(read_tracking_path));
        assert!(request.workspace_lock.is_none());
    }

    #[tokio::test]
    async fn write_action_keeps_existing_read_before_write_message() {
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "old");
        let canonical_path = file_path.canonicalize().unwrap();
        let yolo_mode = YoloMode::new();
        let hooks = Arc::new(HookEngine::disabled());
        let workspace_locks = WorkspaceLockContext::disabled(fixture.project_root.clone());
        let policy = ActionPolicy::testing();
        let guard = FileMutationGuard::new(
            &fixture.project_root,
            &fixture.read_tracker,
            &yolo_mode,
            &hooks,
            &workspace_locks,
            &policy,
            FileMutationAction::Write,
        );

        let err = guard
            .read_existing_content(&canonical_path, "test.txt")
            .await
            .unwrap_err();

        assert!(err.to_string().contains("must be read before overwriting"));
        assert!(err.to_string().contains("try writing again"));
    }

    #[tokio::test]
    async fn edit_reads_current_content_when_file_changed_since_read() {
        // A file changed on disk since the model read it must NOT bail an edit
        // with "modified since read, read it again" — that ordered re-read was
        // answered by the read-dedup with a content-omitted stub and looped the
        // model. `edit` is content-addressed, so instead it
        // returns the CURRENT on-disk content; the edit's `old_string` then
        // matches (or misses with a targeted error) against reality.
        let fixture = TestFixture::new();
        let file_path = fixture.create_file("test.txt", "old");
        let canonical_path = file_path.canonicalize().unwrap();
        fixture.read_tracker.mark_read(&canonical_path).await;
        std::fs::write(&file_path, "changed").unwrap();
        let yolo_mode = YoloMode::new();
        let hooks = Arc::new(HookEngine::disabled());
        let workspace_locks = WorkspaceLockContext::disabled(fixture.project_root.clone());
        let policy = ActionPolicy::testing();
        let guard = FileMutationGuard::new(
            &fixture.project_root,
            &fixture.read_tracker,
            &yolo_mode,
            &hooks,
            &workspace_locks,
            &policy,
            FileMutationAction::Edit,
        );

        let content = guard
            .read_existing_content(&canonical_path, "test.txt")
            .await
            .expect("a changed-since-read file must not bail an edit — it returns current content");

        assert_eq!(
            content, "changed",
            "edit must operate on the current on-disk content, not the stale read baseline",
        );
    }

    #[test]
    fn replayed_episode_and_recall_markers_are_rejected_as_file_content() {
        let marker = "[Episode archived] #3 \"Fix resume flake\" — this earlier task's working \
                      context (14 messages, ~21k tokens) was archived as a completed unit.\n\
                      ## Episode card\n- Goal: fix the flake";
        let err = reject_replayed_elision_marker(marker, "src/lib.rs")
            .expect_err("an episode marker replayed as file content must be refused");
        assert!(err.to_string().contains("archived-context marker"), "{err}");

        let enveloped = format!("Harness note: {marker}");
        assert!(reject_replayed_elision_marker(&enveloped, "src/lib.rs").is_err());

        let pointer = "[reused previous recall] episode 3 output is already live above.";
        assert!(reject_replayed_elision_marker(pointer, "src/lib.rs").is_err());

        // Real content that merely mentions the marker deeper in stays writable.
        let genuine = format!("fn main() {{}}\n// docs mention {marker}");
        assert!(reject_replayed_elision_marker(&genuine, "src/lib.rs").is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_write_preserves_existing_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = TestFixture::new();
        let file_path = fixture.create_file("script.sh", "#!/bin/sh\nexit 0\n");
        std::fs::set_permissions(&file_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        atomic_write(AtomicWriteRequest {
            project_root: &fixture.project_root,
            write_path: &file_path,
            project_relative_path: Some(Path::new("script.sh")),
            content: "#!/bin/sh\nexit 1\n",
            precondition: WritePrecondition::Exact("#!/bin/sh\nexit 0\n"),
            parent_dirs: ParentDirs::Keep,
            allow_external: false,
            workspace_lock: None,
        })
        .await
        .unwrap();

        let metadata = std::fs::metadata(&file_path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o755);
        assert_eq!(
            std::fs::read_to_string(&file_path).unwrap(),
            "#!/bin/sh\nexit 1\n"
        );
    }

    #[tokio::test]
    async fn expired_workspace_lease_cannot_commit_file_content() {
        let fixture = TestStorage::new().await;
        let session_id = fixture.start_session().await;
        let file_path = fixture.project_path().join("fenced.txt");
        std::fs::write(&file_path, "original\n").unwrap();
        let mut workspace_lock = fixture
            .storage
            .acquire_workspace_write_lock(fixture.project_path(), session_id)
            .await
            .unwrap();
        workspace_lock.expire_for_tests().await.unwrap();

        let error = atomic_write(AtomicWriteRequest {
            project_root: fixture.project_path(),
            write_path: &file_path,
            project_relative_path: Some(Path::new("fenced.txt")),
            content: "replacement\n",
            precondition: WritePrecondition::Exact("original\n"),
            parent_dirs: ParentDirs::Keep,
            allow_external: false,
            workspace_lock: Some(&workspace_lock),
        })
        .await
        .expect_err("an expired lease must fence the final rename");

        assert!(error.to_string().contains("lease was lost"), "{error:#}");
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), "original\n");
    }
}
