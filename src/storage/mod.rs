use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_openai::types::chat::ChatCompletionRequestMessage;
use directories::BaseDirs;
use serde_json::{Value, json};
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};

use crate::agent::{
    CompactionEvent, ContextControlState, ContextMessageSnapshot, ContextRewriteKind, UsageTurn,
    UsageTurnStatus,
};
use crate::plan::PlanDoc;
use crate::provider::{EstimateConfidence, InputCacheUsage, ReasoningSelection, TokenCounterKind};
use crate::session::{ProviderSession, SaveAuthPolicy, SessionStore};
use crate::todo::{TodoItem, TodoStatus};
use crate::tool::read_evidence::{InspectionEventRecord, ReadEvidenceRecord};
use crate::tui::event::CommandOutputKind;
use crate::tui::transcript::{
    ExecutionGroup, ToolActivity, ToolStatus, TranscriptItem, is_noisy_context_fallback_status,
};
use crate::util::time::{now_ms, system_time_from_ms, system_time_to_ms};

/// Execute a SQLite mutation inside a transaction and preserve its error context.
macro_rules! storage_op {
    ($tx:expr, $operation:literal, $query:expr $(,)?) => {
        ($query)
            .execute(&mut **$tx)
            .await
            .context(concat!("Failed to ", $operation))
    };
}

mod authorization;
mod builtin_subagents;
mod context;
mod episodes;
pub(crate) use episodes::content_snippet as episode_content_snippet;
mod memory;
mod peers;
mod permissions;
mod plans;
mod providers;
mod quality_evidence;
mod read_evidence;
mod recovery;
mod self_review;
mod sessions;
#[cfg(test)]
pub(crate) mod test_utils;
#[cfg(test)]
mod tests;
mod todos;
mod transcript;
mod usage_stats;
mod verification;
mod workspace_locks;

pub(crate) use authorization::{AuthorizationDecisionRecord, NewAuthorizationDecision};
#[cfg(test)]
pub(crate) use peers::PEER_LIVENESS_THRESHOLD_MS;
pub(crate) use peers::{
    CommittedPeerSend, CommittedPeerWake, PeerSendOperation, PeerWakeOperation,
};
pub use peers::{
    LeasedPeerMessage, PeerDeliveryConsumer, PeerDeliveryReceipt, PeerMessage, PeerMessageKind,
    PeerSessionSummary, SharedBusyFlag, WakeRelationships, WakeSubscriptionOutcome,
    WakeSubscriptionRegistration, activate_session_heartbeat, spawn_heartbeat_writer,
};
pub use permissions::{PermissionScope, RuleKind, StoredPermissionRule};
pub(crate) use recovery::{NewRecoveryPoint, RecoveryId, RecoveryPoint, RecoveryState};
pub use usage_stats::{
    DailyUsage, LocalToday, ModelUsage, QualityEvidenceIntegrity, UsageDashboard,
};
pub(crate) use workspace_locks::WorkspaceLeaseFence;
pub use workspace_locks::WorkspaceLockGuard;

const BONSAI_HOME_ENV: &str = "BONSAI_HOME";
const BONSAI_HOME_DIR: &str = ".bonsai";
const DB_FILE: &str = "bonsai.db";
const UPGRADE_BACKUP_DIR: &str = "backups";
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");
const SESSION_SUMMARY_PROJECTION: &str = r#"
              sessions.id,
              projects.path AS project_path,
              sessions.name,
              sessions.summary,
              sessions.provider_id,
              sessions.model,
              sessions.reasoning_json,
              sessions.status,
              sessions.terminal_reason,
              sessions.updated_at_ms,
              sessions.prompt_token_count,
              sessions.completion_token_count,
              sessions.cache_read_input_token_count,
              sessions.cache_creation_input_token_count,
              sessions.cache_measured_input_token_count,
              sessions.cost_micros,
              sessions.no_cache_cost_micros,
              sessions.source_plan_id,
              COUNT(messages.id) AS message_count
"#;
const SAVED_PLAN_SUMMARY_PROJECTION: &str = r#"
              saved_plans.id AS id,
              projects.path AS project_path,
              saved_plans.title,
              saved_plans.source_session_id,
              saved_plans.branch,
              saved_plans.status,
              saved_plans.execution_session_id,
              saved_plans.saved_at_ms,
              saved_plans.updated_at_ms,
              saved_plans.section_count,
              saved_plans.task_count
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BonsaiPaths {
    home_dir: PathBuf,
    db_path: PathBuf,
}

impl BonsaiPaths {
    pub fn discover() -> Result<Self> {
        if let Some(value) = std::env::var_os(BONSAI_HOME_ENV) {
            let home_dir = PathBuf::from(value);
            if !home_dir.as_os_str().is_empty() {
                return Ok(Self::from_home_dir(home_dir));
            }
        }

        let dirs = BaseDirs::new().context("Failed to determine home directory")?;
        Ok(Self::from_home_dir(dirs.home_dir().join(BONSAI_HOME_DIR)))
    }

    fn from_home_dir(home_dir: PathBuf) -> Self {
        let db_path = home_dir.join(DB_FILE);
        Self { home_dir, db_path }
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

#[derive(Debug, Clone)]
pub struct Storage {
    pool: SqlitePool,
    paths: BonsaiPaths,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(i64);

impl SessionId {
    pub const fn from_raw(id: i64) -> Self {
        Self(id)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SavedPlanId(i64);

impl SavedPlanId {
    pub const fn from_raw(id: i64) -> Self {
        Self(id)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for SavedPlanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub project_path: String,
    pub name: String,
    pub summary: String,
    pub provider_id: String,
    pub model: String,
    pub reasoning: ReasoningSelection,
    pub status: SessionStatus,
    pub terminal_reason: Option<crate::run_budget::RunBudgetExhaustion>,
    pub updated_at_ms: i64,
    pub message_count: i64,
    pub prompt_token_count: i64,
    pub completion_token_count: i64,
    pub cache_read_input_token_count: i64,
    pub cache_creation_input_token_count: i64,
    pub cache_measured_input_token_count: i64,
    pub cost_micros: i64,
    pub no_cache_cost_micros: i64,
    pub source_plan_id: Option<SavedPlanId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Active,
    Completed,
    Forgotten,
    Interrupted,
    Failed,
    Other(String),
}

crate::impl_db_enum!(SessionStatus {
    Active => "active",
    Completed => "completed",
    Forgotten => "forgotten",
    Interrupted => "interrupted",
    Failed => "failed",
} other = Other);

impl SessionStatus {
    pub fn label(&self) -> &str {
        self.as_db_str()
    }

    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Forgotten | Self::Interrupted | Self::Failed
        )
    }
}

/// Result of a project-scoped request to delete one persisted session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgetSessionOutcome {
    /// The session and its cascading state were deleted.
    Forgotten,
    /// No session with the requested id exists.
    NotFound,
    /// The id belongs to a different canonical project.
    DifferentProject,
    /// A fresh heartbeat still owns the active session.
    Live,
}

/// Result of atomically claiming a persisted session for resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeSessionOutcome {
    /// The target was claimed and made active.
    Resumed,
    /// No session with the requested id exists.
    NotFound,
    /// The id belongs to a different canonical project.
    DifferentProject,
    /// A fresh heartbeat still owns the active session.
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub session_id: SessionId,
    pub project_path: String,
    pub role: String,
    pub content: String,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub summary: SessionSummary,
    pub transcript: Vec<TranscriptItem>,
    pub context_messages: Vec<ChatCompletionRequestMessage>,
    pub context_message_ids: Vec<String>,
    pub context_controls: HashMap<String, ContextControlState>,
    pub context_sources: HashMap<String, Vec<ChatCompletionRequestMessage>>,
    pub read_evidence: Vec<ReadEvidenceRecord>,
    pub inspection_events: Vec<InspectionEventRecord>,
    pub compaction_events: Vec<CompactionEvent>,
    pub episodes: Vec<crate::episode::Episode>,
    pub usage_turns: Vec<UsageTurn>,
    pub verification_runs: Vec<crate::verification::VerificationRunRecord>,
    pub self_review_runs: Vec<crate::self_review::SelfReviewRunRecord>,
    pub message_history: Vec<(String, String)>,
    pub plan: PlanDoc,
    pub todos: Vec<TodoItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlanSummary {
    pub id: SavedPlanId,
    pub project_path: String,
    pub title: String,
    /// The session the plan was saved from; `None` once that session has been
    /// deleted (the snapshot itself lives on independently).
    pub source_session_id: Option<SessionId>,
    pub branch: Option<String>,
    pub status: SavedPlanStatus,
    pub execution_session_id: Option<SessionId>,
    pub saved_at_ms: i64,
    pub updated_at_ms: i64,
    pub section_count: i64,
    pub task_count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SavedPlanStatus {
    Draft,
    Started,
    Other(String),
}

crate::impl_db_enum!(SavedPlanStatus {
    Draft => "draft",
    Started => "started",
} other = Other);

impl SavedPlanStatus {
    pub fn label(&self) -> &str {
        self.as_db_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedPlanSnapshot {
    pub summary: SavedPlanSummary,
    pub plan: PlanDoc,
}

impl Storage {
    pub async fn open() -> Result<Self> {
        Self::open_paths(BonsaiPaths::discover()?).await
    }

    pub(crate) fn home_dir(&self) -> &Path {
        self.paths.home_dir()
    }

    /// Close the connection pool, awaiting every connection's teardown. In WAL
    /// mode SQLite folds the write-ahead log back into the main database when
    /// the last connection closes; on a large WAL (tens of MB) that checkpoint
    /// is a multi-second, fsync-heavy operation. Doing it here — explicitly and
    /// on a measured await point during shutdown — keeps it from happening as an
    /// untimed `Drop` after the runtime is torn down, where it stalls process
    /// exit invisibly (the "closing takes a few seconds" report).
    pub(crate) async fn close(&self) {
        self.pool.close().await;
    }

    pub(crate) async fn open_at(db_path: impl AsRef<Path>) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        let home_dir = db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::open_paths(BonsaiPaths { home_dir, db_path }).await
    }

    async fn open_paths(paths: BonsaiPaths) -> Result<Self> {
        Self::open_paths_with_migrator(paths, &MIGRATOR).await
    }

    async fn open_paths_with_migrator(paths: BonsaiPaths, migrator: &Migrator) -> Result<Self> {
        std::fs::create_dir_all(paths.home_dir()).with_context(|| {
            format!(
                "Failed to create Bonsai home directory {:?}",
                paths.home_dir()
            )
        })?;

        let database_existed =
            std::fs::metadata(paths.db_path()).is_ok_and(|metadata| metadata.len() > 0);

        let options = SqliteConnectOptions::new()
            .filename(paths.db_path())
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .with_context(|| format!("Failed to open SQLite database {:?}", paths.db_path()))?;

        let backup = prepare_upgrade_backup(&pool, &paths, migrator, database_existed).await?;
        if let Err(error) = migrator.run(&pool).await {
            pool.close().await;
            if let Some(backup) = backup {
                restore_upgrade_backup(&paths, &backup).with_context(|| {
                    format!(
                        "Failed to restore pre-upgrade backup {:?} after migration error: {error}",
                        backup.path
                    )
                })?;
                anyhow::bail!(
                    "Failed to run SQLite migrations; restored the database from {:?}: {error}",
                    backup.path
                );
            }
            return Err(error).context("Failed to run SQLite migrations");
        }

        #[cfg(unix)]
        harden_storage_permissions(&paths)?;

        Ok(Self { pool, paths })
    }

    pub fn db_path(&self) -> &Path {
        self.paths.db_path()
    }

    pub(crate) fn upgrade_backup_dir(&self) -> PathBuf {
        self.paths.home_dir().join(UPGRADE_BACKUP_DIR)
    }

    /// Verify that the migrated database can be read and written without
    /// changing durable application state.
    pub(crate) async fn health_check(&self) -> Result<()> {
        let results = sqlx::query_scalar::<_, String>("PRAGMA quick_check")
            .fetch_all(&self.pool)
            .await
            .context("Failed to run SQLite quick check")?;
        if results.iter().any(|result| result != "ok") {
            anyhow::bail!("SQLite quick check reported an integrity error");
        }

        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin SQLite write probe")?;
        storage_op!(
            &mut tx,
            "create SQLite write probe",
            sqlx::query("CREATE TEMP TABLE bonsai_doctor_probe (value INTEGER NOT NULL)"),
        )?;
        storage_op!(
            &mut tx,
            "write SQLite probe",
            sqlx::query("INSERT INTO bonsai_doctor_probe (value) VALUES (1)"),
        )?;
        storage_op!(
            &mut tx,
            "remove SQLite write probe",
            sqlx::query("DROP TABLE bonsai_doctor_probe"),
        )?;
        tx.commit()
            .await
            .context("Failed to commit SQLite write probe")?;
        Ok(())
    }

    /// Begin a write transaction with `BEGIN IMMEDIATE`.
    ///
    /// Deliberate, don't "simplify" back to `pool.begin()`. A plain `BEGIN` is
    /// *deferred*: the transaction opens as a reader and only tries to take the
    /// write lock at its first INSERT/UPDATE/DELETE. If another connection wrote
    /// in that window, the upgrade fails with `SQLITE_BUSY_SNAPSHOT` (code 5)
    /// — and SQLite deliberately does *not* invoke the busy handler for it,
    /// because waiting cannot resolve a stale read snapshot. That is why the
    /// pool's 5s `busy_timeout` never applied and callers saw an instant
    /// "database is locked" under concurrent writes.
    ///
    /// `BEGIN IMMEDIATE` takes the write lock up front, so contention becomes an
    /// ordinary wait that the busy handler *does* honor.
    pub(crate) async fn begin_write(
        &self,
    ) -> std::result::Result<Transaction<'static, Sqlite>, sqlx::Error> {
        self.pool.begin_with("BEGIN IMMEDIATE").await
    }

    /// Run `body` inside a fresh transaction, committing on success and
    /// attaching `label` to the begin/commit error context. `body` receives the
    /// open transaction and a single `now` timestamp shared by all its writes;
    /// it is responsible for its own DELETE/INSERT statements and for calling
    /// [`touch_session`] where the session's `updated_at_ms` should advance.
    pub(crate) async fn replace_quality_evidence_snapshot(
        &self,
        session_id: SessionId,
        verification_runs: &[crate::verification::VerificationRunRecord],
        self_review_runs: &[crate::self_review::SelfReviewRunRecord],
    ) -> Result<()> {
        self.with_session_snapshot_tx("quality evidence snapshot", async move |tx, now| {
            self.replace_verification_runs_snapshot_in_tx(tx, session_id, verification_runs, now)
                .await?;
            self.replace_self_review_runs_snapshot_in_tx(tx, session_id, self_review_runs, now)
                .await
        })
        .await
    }

    pub(crate) async fn with_session_snapshot_tx<F>(
        &self,
        label: &'static str,
        body: F,
    ) -> Result<()>
    where
        F: AsyncFnOnce(&mut Transaction<'_, Sqlite>, i64) -> Result<()>,
    {
        let mut tx = self
            .begin_write()
            .await
            .with_context(|| format!("Failed to begin {label} transaction"))?;
        body(&mut tx, now_ms()).await?;
        tx.commit()
            .await
            .with_context(|| format!("Failed to commit {label}"))?;
        Ok(())
    }
}

/// Narrow read/write accessors used only by the storage integration tests so the
/// raw `pool` field stays encapsulated and `sqlx` stays out of test bodies.
#[cfg(test)]
impl Storage {
    pub(crate) async fn count_transcript_blocks(&self, session_id: SessionId) -> Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM transcript_blocks WHERE session_id = ?")
            .bind(session_id.as_i64())
            .fetch_one(&self.pool)
            .await
            .context("Failed to count transcript blocks")
    }

    pub(crate) async fn count_usage_turns(&self, session_id: SessionId) -> Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM usage_turns WHERE session_id = ?")
            .bind(session_id.as_i64())
            .fetch_one(&self.pool)
            .await
            .context("Failed to count usage turns")
    }

    pub(crate) async fn count_episode_archive_rows(&self, session_id: SessionId) -> Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM episode_archive WHERE session_id = ?")
            .bind(session_id.as_i64())
            .fetch_one(&self.pool)
            .await
            .context("Failed to count episode archive rows")
    }

    pub(crate) async fn source_plan_id(&self, session_id: SessionId) -> Result<Option<i64>> {
        sqlx::query_scalar("SELECT source_plan_id FROM sessions WHERE id = ?")
            .bind(session_id.as_i64())
            .fetch_one(&self.pool)
            .await
            .context("Failed to load source_plan_id")
    }

    pub(crate) async fn ended_at_ms(&self, session_id: SessionId) -> Result<Option<i64>> {
        sqlx::query_scalar("SELECT ended_at_ms FROM sessions WHERE id = ?")
            .bind(session_id.as_i64())
            .fetch_one(&self.pool)
            .await
            .context("Failed to load ended_at_ms")
    }

    pub(crate) async fn insert_legacy_transcript_block(
        &self,
        session_id: SessionId,
        seq: i64,
        kind: &str,
        body: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO transcript_blocks (
              session_id, seq, kind, title, body, metadata_json
            )
            VALUES (?, ?, ?, '', ?, '{}')
            "#,
        )
        .bind(session_id.as_i64())
        .bind(seq)
        .bind(kind)
        .bind(body)
        .execute(&self.pool)
        .await
        .context("Failed to insert legacy transcript block")?;
        Ok(())
    }
}

pub(crate) fn decode_cost_micros(cost_micros: i64) -> Option<u64> {
    u64::try_from(cost_micros).ok()
}

pub(crate) fn decode_input_cache_usage(
    read_tokens: i64,
    creation_tokens: i64,
    total_input_tokens: i64,
) -> Option<InputCacheUsage> {
    let total_input_tokens = u64::try_from(total_input_tokens).ok()?;
    if total_input_tokens == 0 {
        return None;
    }
    Some(InputCacheUsage::new(
        u64::try_from(read_tokens).unwrap_or(0),
        u64::try_from(creation_tokens).unwrap_or(0),
        total_input_tokens,
    ))
}

fn encode_cost_micros(cost_micros: Option<u64>) -> i64 {
    cost_micros
        .map(|cost| i64::try_from(cost).unwrap_or(i64::MAX))
        .unwrap_or(-1)
}
fn canonicalize_lossy(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Canonicalize `path` (falling back to the input on failure) and render it as
/// the lossy UTF-8 string used as the `projects.path` lookup key.
pub(crate) fn canonical_project_path(path: &Path) -> String {
    canonicalize_lossy(path).to_string_lossy().to_string()
}

/// Bump `sessions.updated_at_ms` for `session_id` to `now` within an open
/// transaction. Shared by every `replace_*_snapshot` writer.
async fn touch_session(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: SessionId,
    now: i64,
) -> Result<()> {
    sqlx::query("UPDATE sessions SET updated_at_ms = ? WHERE id = ?")
        .bind(now)
        .bind(session_id.as_i64())
        .execute(&mut **tx)
        .await
        .with_context(|| format!("Failed to touch session {session_id}"))?;
    Ok(())
}

#[derive(Debug)]
struct UpgradeBackup {
    path: PathBuf,
}

async fn prepare_upgrade_backup(
    pool: &SqlitePool,
    paths: &BonsaiPaths,
    migrator: &Migrator,
    database_existed: bool,
) -> Result<Option<UpgradeBackup>> {
    if !database_existed {
        return Ok(None);
    }

    let Some(target_version) = migrator
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .map(|migration| migration.version)
        .max()
    else {
        return Ok(None);
    };
    let current_version = current_migration_version(pool).await?;
    if current_version.is_some_and(|version| version >= target_version) {
        return Ok(None);
    }

    let backup_dir = paths.home_dir().join(UPGRADE_BACKUP_DIR);
    std::fs::create_dir_all(&backup_dir)
        .with_context(|| format!("Failed to create upgrade backup directory {backup_dir:?}"))?;
    #[cfg(unix)]
    set_unix_mode(&backup_dir, 0o700)?;

    let source_label = current_version
        .map(|version| format!("v{version}"))
        .unwrap_or_else(|| "unversioned".to_string());
    let backup_path = backup_dir.join(format!(
        "bonsai-{source_label}-before-v{target_version}-{}.db",
        now_ms()
    ));
    sqlx::query("VACUUM INTO ?")
        .bind(backup_path.to_string_lossy().as_ref())
        .execute(pool)
        .await
        .with_context(|| format!("Failed to create pre-upgrade backup {backup_path:?}"))?;
    #[cfg(unix)]
    set_unix_mode(&backup_path, 0o600)?;

    tracing::info!(
        backup = %backup_path.display(),
        from_version = ?current_version,
        to_version = target_version,
        "created pre-upgrade SQLite backup"
    );
    Ok(Some(UpgradeBackup { path: backup_path }))
}

async fn current_migration_version(pool: &SqlitePool) -> Result<Option<i64>> {
    let migration_table_exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_one(pool)
    .await
    .context("Failed to inspect the SQLite migration table")?;
    if migration_table_exists == 0 {
        return Ok(None);
    }

    sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success = 1")
        .fetch_one(pool)
        .await
        .context("Failed to read the current SQLite migration version")
}

fn restore_upgrade_backup(paths: &BonsaiPaths, backup: &UpgradeBackup) -> Result<()> {
    let db_path = paths.db_path();
    let restore_path = paths
        .home_dir()
        .join(format!(".{DB_FILE}.restore-{}.tmp", now_ms()));
    std::fs::copy(&backup.path, &restore_path).with_context(|| {
        format!(
            "Failed to stage upgrade backup {:?} at {:?}",
            backup.path, restore_path
        )
    })?;
    #[cfg(unix)]
    set_unix_mode(&restore_path, 0o600)?;

    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(db_path, suffix);
        if sidecar.exists() {
            std::fs::remove_file(&sidecar)
                .with_context(|| format!("Failed to remove stale SQLite sidecar {sidecar:?}"))?;
        }
    }

    let failed_path = paths
        .home_dir()
        .join(format!(".{DB_FILE}.failed-upgrade-{}.tmp", now_ms()));
    if db_path.exists() {
        std::fs::rename(db_path, &failed_path).with_context(|| {
            format!("Failed to move the failed upgrade database to {failed_path:?}")
        })?;
    }
    if let Err(error) = std::fs::rename(&restore_path, db_path) {
        if failed_path.exists() {
            let _ = std::fs::rename(&failed_path, db_path);
        }
        return Err(error).context("Failed to activate the restored pre-upgrade database");
    }
    if failed_path.exists()
        && let Err(error) = std::fs::remove_file(&failed_path)
    {
        tracing::warn!(
            path = %failed_path.display(),
            error = %error,
            "restored upgrade backup but could not remove failed temporary database"
        );
    }
    Ok(())
}

/// Restrict the on-disk session store to the owner. The database and its
/// WAL/SHM sidecars can hold API keys and tool output, so each is set to `0600`
/// and the containing home directory to `0700`. Sidecars are created lazily by
/// SQLite, so they are only hardened when present.
#[cfg(unix)]
fn harden_storage_permissions(paths: &BonsaiPaths) -> Result<()> {
    set_unix_mode(paths.home_dir(), 0o700)?;

    let db_path = paths.db_path();
    set_unix_mode(db_path, 0o600)?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(db_path, suffix);
        if sidecar.exists() {
            set_unix_mode(&sidecar, 0o600)?;
        }
    }
    Ok(())
}

/// `bonsai.db` + `-wal`/`-shm` — the suffix is appended to the whole file name,
/// not the extension, so build it on the `OsStr` to stay lossless.
fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(unix)]
fn set_unix_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut perms = std::fs::metadata(path)
        .with_context(|| format!("Failed to read metadata for {path:?}"))?
        .permissions();
    perms.set_mode(mode);
    std::fs::set_permissions(path, perms)
        .with_context(|| format!("Failed to set permissions on {path:?}"))?;
    Ok(())
}
