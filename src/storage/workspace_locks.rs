use super::*;
use std::sync::Arc;

const WORKSPACE_LOCK_PATH: &str = ".";
const WORKSPACE_LOCK_LEASE_MS: i64 = 30_000;
const WORKSPACE_LOCK_REFRESH: Duration = Duration::from_secs(10);
const WORKSPACE_LOCK_SAFETY_MARGIN: Duration = Duration::from_secs(5);
const WORKSPACE_LOCK_RETRY_MIN: Duration = Duration::from_millis(100);
const WORKSPACE_LOCK_RETRY_MAX: Duration = Duration::from_secs(2);
const WORKSPACE_LOCK_LIVE_WAIT: Duration = Duration::from_secs(2);
const WORKSPACE_LOCK_STALE_WAIT: Duration = Duration::from_secs(35);
const WORKSPACE_LOCK_POLL: Duration = Duration::from_millis(50);

const LEASE_HELD: u8 = 0;
const LEASE_LOST: u8 = 1;

fn duration_millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[derive(Debug, Clone, Copy)]
struct WorkspaceLeaseTiming {
    lease: Duration,
    refresh: Duration,
    safety_margin: Duration,
    retry_min: Duration,
    retry_max: Duration,
    live_wait: Duration,
    stale_wait: Duration,
    poll: Duration,
}

impl WorkspaceLeaseTiming {
    const PRODUCTION: Self = Self {
        lease: Duration::from_millis(WORKSPACE_LOCK_LEASE_MS as u64),
        refresh: WORKSPACE_LOCK_REFRESH,
        safety_margin: WORKSPACE_LOCK_SAFETY_MARGIN,
        retry_min: WORKSPACE_LOCK_RETRY_MIN,
        retry_max: WORKSPACE_LOCK_RETRY_MAX,
        live_wait: WORKSPACE_LOCK_LIVE_WAIT,
        stale_wait: WORKSPACE_LOCK_STALE_WAIT,
        poll: WORKSPACE_LOCK_POLL,
    };

    fn lease_ms(self) -> i64 {
        duration_millis(self.lease)
    }

    fn safety_margin_ms(self) -> i64 {
        duration_millis(self.safety_margin)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceLeaseStatus {
    Held,
    Lost,
}

#[derive(Debug)]
struct WorkspaceLeaseState {
    status: std::sync::atomic::AtomicU8,
    confirmed_until_ms: std::sync::atomic::AtomicI64,
    safety_margin_ms: i64,
    cancellation: tokio_util::sync::CancellationToken,
    #[cfg(test)]
    injected_refresh_failures: std::sync::atomic::AtomicUsize,
}

impl WorkspaceLeaseState {
    fn new(confirmed_until_ms: i64, safety_margin_ms: i64) -> Self {
        Self {
            status: std::sync::atomic::AtomicU8::new(LEASE_HELD),
            confirmed_until_ms: std::sync::atomic::AtomicI64::new(confirmed_until_ms),
            safety_margin_ms,
            cancellation: tokio_util::sync::CancellationToken::new(),
            #[cfg(test)]
            injected_refresh_failures: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn status(&self) -> WorkspaceLeaseStatus {
        match self.status.load(std::sync::atomic::Ordering::Acquire) {
            LEASE_HELD => WorkspaceLeaseStatus::Held,
            _ => WorkspaceLeaseStatus::Lost,
        }
    }

    fn mark_lost(&self) {
        if self
            .status
            .compare_exchange(
                LEASE_HELD,
                LEASE_LOST,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
        {
            self.cancellation.cancel();
        }
    }

    fn confirm_until(&self, expires_at_ms: i64) {
        self.confirmed_until_ms
            .store(expires_at_ms, std::sync::atomic::Ordering::Release);
    }

    fn remaining_before_safety_margin(&self, now: i64) -> Option<Duration> {
        if self.status() == WorkspaceLeaseStatus::Lost {
            return None;
        }
        let safe_until = self
            .confirmed_until_ms
            .load(std::sync::atomic::Ordering::Acquire)
            .saturating_sub(self.safety_margin_ms);
        let remaining = safe_until.saturating_sub(now);
        if remaining <= 0 {
            None
        } else {
            Some(Duration::from_millis(remaining as u64))
        }
    }

    fn ensure_held(&self) -> Result<()> {
        if self.remaining_before_safety_margin(now_ms()).is_some() {
            return Ok(());
        }
        self.mark_lost();
        anyhow::bail!(
            "Workspace lock lease was lost before the operation could commit; no new mutation was authorized. Retry after checking concurrent Bonsai sessions."
        )
    }

    #[cfg(test)]
    fn should_inject_refresh_failure(&self) -> bool {
        use std::sync::atomic::Ordering;

        self.injected_refresh_failures
            .fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |remaining| match remaining {
                    0 => None,
                    usize::MAX => Some(usize::MAX),
                    value => Some(value - 1),
                },
            )
            .is_ok()
    }
}

/// Synchronous final-commit fence shared with descriptor-relative filesystem
/// code. A successful async confirmation extends the row first; this fence
/// then prevents a long staging write from renaming/unlinking after the lease
/// enters its safety margin or is marked lost by the renewal task.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceLeaseFence {
    state: Arc<WorkspaceLeaseState>,
}

impl WorkspaceLeaseFence {
    pub(crate) fn ensure_held(&self) -> Result<()> {
        self.state.ensure_held()
    }
}

/// Lease row held while a tool mutates the shared working tree.
pub struct WorkspaceLockGuard {
    storage: Storage,
    id: i64,
    timing: WorkspaceLeaseTiming,
    state: Arc<WorkspaceLeaseState>,
    refresh_task: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for WorkspaceLockGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceLockGuard")
            .field("id", &self.id)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl WorkspaceLockGuard {
    pub(crate) fn status(&self) -> WorkspaceLeaseStatus {
        self.state.status()
    }

    /// Drive one protected async phase while the lease remains held. Renewal
    /// loss cancels the phase before a conflicting owner can reclaim the row.
    pub(crate) async fn run_while_held<T>(
        &self,
        operation: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let cancellation = self.state.cancellation.clone();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                anyhow::bail!(
                    "Workspace lock lease was lost while the protected operation was running; the operation was cancelled before commit."
                )
            }
            result = operation => {
                let value = result?;
                self.confirm_held().await?;
                Ok(value)
            }
        }
    }

    /// Confirm that this exact lock row still exists and atomically extend its
    /// expiry. Mutation code calls this immediately before staging a commit.
    pub async fn confirm_held(&self) -> Result<WorkspaceLeaseFence> {
        self.state.ensure_held()?;
        let refreshed_until = match self.refresh_once().await {
            Ok(refreshed) => refreshed,
            Err(error) => {
                self.state.mark_lost();
                return Err(error)
                    .context("Failed to confirm workspace lock ownership before commit");
            }
        };
        let Some(expires_at) = refreshed_until else {
            self.state.mark_lost();
            anyhow::bail!(
                "Workspace lock lease was lost before the operation could commit; the ownership row no longer exists or has expired."
            );
        };
        self.state.confirm_until(expires_at);
        self.state.ensure_held()?;
        Ok(WorkspaceLeaseFence {
            state: self.state.clone(),
        })
    }

    async fn refresh_once(&self) -> Result<Option<i64>> {
        #[cfg(test)]
        if self.state.should_inject_refresh_failure() {
            anyhow::bail!("injected workspace lock refresh failure");
        }
        self.storage
            .refresh_workspace_lock(self.id, self.timing.lease_ms())
            .await
    }

    #[cfg(test)]
    fn inject_refresh_failures(&self, failures: usize) {
        self.state
            .injected_refresh_failures
            .store(failures, std::sync::atomic::Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) async fn expire_for_tests(&mut self) -> Result<()> {
        if let Some(refresh_task) = self.refresh_task.take() {
            refresh_task.abort();
        }
        sqlx::query("UPDATE workspace_locks SET expires_at_ms = ? WHERE id = ?")
            // Use an absolute past value instead of the process clock. SQLite's
            // julianday clock can trail it by a millisecond, which made the row
            // occasionally still refreshable in this fence regression test.
            .bind(0_i64)
            .bind(self.id)
            .execute(&self.storage.pool)
            .await
            .context("Failed to expire test workspace lock")?;
        Ok(())
    }
}

impl Drop for WorkspaceLockGuard {
    fn drop(&mut self) {
        self.state.mark_lost();
        if let Some(refresh_task) = self.refresh_task.take() {
            refresh_task.abort();
        }
        let storage = self.storage.clone();
        let id = self.id;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(err) = storage.release_workspace_lock(id).await {
                    tracing::debug!(lock_id = id, %err, "workspace lock release failed");
                }
            });
        }
    }
}

impl Storage {
    /// Acquire a project-wide write lease for the working tree.
    ///
    /// Reads are optimistic and never wait for this lease. Writers remain
    /// serialized so two Bonsai sessions cannot commit overlapping mutations
    /// concurrently, and the lease fence still protects the final commit.
    pub async fn acquire_workspace_write_lock(
        &self,
        project_root: &Path,
        owner_session_id: SessionId,
    ) -> Result<WorkspaceLockGuard> {
        self.acquire_workspace_write_lock_with_timing(
            project_root,
            owner_session_id,
            WorkspaceLeaseTiming::PRODUCTION,
        )
        .await
    }

    async fn acquire_workspace_write_lock_with_timing(
        &self,
        project_root: &Path,
        owner_session_id: SessionId,
        timing: WorkspaceLeaseTiming,
    ) -> Result<WorkspaceLockGuard> {
        anyhow::ensure!(
            timing.safety_margin < timing.lease,
            "Workspace lock safety margin must be shorter than its lease"
        );
        let project_id = self.ensure_project(project_root).await?;
        let owner_pid = i64::from(std::process::id());
        let wait_started = Instant::now();
        loop {
            let now = now_ms();
            if let Some(id) = self
                .try_acquire_workspace_write_lock_with_lease(
                    project_id,
                    owner_session_id,
                    owner_pid,
                    now,
                    timing.lease_ms(),
                )
                .await?
            {
                let state = Arc::new(WorkspaceLeaseState::new(
                    now.saturating_add(timing.lease_ms()),
                    timing.safety_margin_ms(),
                ));
                return Ok(WorkspaceLockGuard {
                    storage: self.clone(),
                    id,
                    timing,
                    state: state.clone(),
                    refresh_task: Some(self.spawn_workspace_lock_refresh(id, state, timing)),
                });
            }
            let waited = wait_started.elapsed();
            if waited >= timing.live_wait
                && let Some((session_id, held_mode)) =
                    self.live_workspace_lock_conflict(project_id, now).await?
            {
                anyhow::bail!(
                    "Workspace write lock is busy: live Bonsai session #{session_id} holds a {held_mode} lock. Wait for that run to finish or coordinate with it via peers, then retry; stopped waiting after {}s instead of blocking for the full lease.",
                    timing.live_wait.as_secs_f32(),
                );
            }
            if waited >= timing.stale_wait {
                anyhow::bail!(
                    "Timed out waiting for workspace write lock after {}s; another Bonsai session is still mutating this project.",
                    timing.stale_wait.as_secs(),
                );
            }
            tokio::time::sleep(timing.poll).await;
        }
    }

    async fn live_workspace_lock_conflict(
        &self,
        project_id: i64,
        now: i64,
    ) -> Result<Option<(SessionId, String)>> {
        let live_floor = now.saturating_sub(super::peers::PEER_LIVENESS_THRESHOLD_MS);
        let row = sqlx::query(
            r#"
            SELECT workspace_locks.owner_session_id, workspace_locks.mode
            FROM workspace_locks
            JOIN sessions ON sessions.id = workspace_locks.owner_session_id
            WHERE workspace_locks.project_id = ?
              AND workspace_locks.path = ?
              AND workspace_locks.expires_at_ms >= ?
              AND sessions.status = ?
              AND sessions.last_heartbeat_ms IS NOT NULL
              AND sessions.last_heartbeat_ms >= ?
            ORDER BY workspace_locks.expires_at_ms ASC
            LIMIT 1
            "#,
        )
        .bind(project_id)
        .bind(WORKSPACE_LOCK_PATH)
        .bind(now)
        .bind(SessionStatus::Active.as_db_str())
        .bind(live_floor)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to inspect live workspace lock owner")?;
        row.map(|row| {
            Ok((
                SessionId::from_raw(row.try_get("owner_session_id")?),
                row.try_get("mode")?,
            ))
        })
        .transpose()
    }

    #[cfg(test)]
    async fn try_acquire_workspace_write_lock(
        &self,
        project_id: i64,
        owner_session_id: SessionId,
        owner_pid: i64,
        now: i64,
    ) -> Result<Option<i64>> {
        self.try_acquire_workspace_write_lock_with_lease(
            project_id,
            owner_session_id,
            owner_pid,
            now,
            WORKSPACE_LOCK_LEASE_MS,
        )
        .await
    }

    async fn try_acquire_workspace_write_lock_with_lease(
        &self,
        project_id: i64,
        owner_session_id: SessionId,
        owner_pid: i64,
        now: i64,
        lease_ms: i64,
    ) -> Result<Option<i64>> {
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin workspace lock transaction")?;
        sqlx::query("DELETE FROM workspace_locks WHERE expires_at_ms < ?")
            .bind(now)
            .execute(&mut *tx)
            .await
            .context("Failed to clear expired workspace locks")?;

        let conflict_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM workspace_locks
            WHERE project_id = ?
              AND path = ?
            "#,
        )
        .bind(project_id)
        .bind(WORKSPACE_LOCK_PATH)
        .fetch_one(&mut *tx)
        .await
        .context("Failed to inspect workspace write locks")?;

        if conflict_count > 0 {
            tx.commit()
                .await
                .context("Failed to finish workspace lock check")?;
            return Ok(None);
        }

        let expires_at = now.saturating_add(lease_ms);
        let id = sqlx::query(
            r#"
            INSERT INTO workspace_locks (
              project_id, path, mode, owner_session_id, owner_pid, expires_at_ms
            )
            VALUES (?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(project_id)
        .bind(WORKSPACE_LOCK_PATH)
        .bind("write")
        .bind(owner_session_id.as_i64())
        .bind(owner_pid)
        .bind(expires_at)
        .fetch_one(&mut *tx)
        .await
        .and_then(|row| row.try_get::<i64, _>("id"))
        .context("Failed to insert workspace lock")?;
        tx.commit()
            .await
            .context("Failed to commit workspace lock")?;
        Ok(Some(id))
    }

    pub(crate) async fn release_workspace_lock(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM workspace_locks WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("Failed to release workspace lock")?;
        Ok(())
    }

    fn spawn_workspace_lock_refresh(
        &self,
        id: i64,
        state: Arc<WorkspaceLeaseState>,
        timing: WorkspaceLeaseTiming,
    ) -> tokio::task::JoinHandle<()> {
        let storage = self.clone();
        tokio::spawn(async move {
            let mut delay = timing.refresh;
            let mut retry_delay = timing.retry_min;
            loop {
                let Some(remaining) = state.remaining_before_safety_margin(now_ms()) else {
                    state.mark_lost();
                    break;
                };
                tokio::time::sleep(delay.min(remaining)).await;
                if state.status() == WorkspaceLeaseStatus::Lost {
                    break;
                }
                if state.remaining_before_safety_margin(now_ms()).is_none() {
                    state.mark_lost();
                    break;
                }
                let refresh = {
                    #[cfg(test)]
                    {
                        if state.should_inject_refresh_failure() {
                            Err(anyhow::anyhow!("injected workspace lock refresh failure"))
                        } else {
                            storage.refresh_workspace_lock(id, timing.lease_ms()).await
                        }
                    }
                    #[cfg(not(test))]
                    {
                        storage.refresh_workspace_lock(id, timing.lease_ms()).await
                    }
                };
                match refresh {
                    Ok(Some(expires_at)) => {
                        state.confirm_until(expires_at);
                        delay = timing.refresh;
                        retry_delay = timing.retry_min;
                    }
                    Ok(None) => {
                        state.mark_lost();
                        tracing::warn!(
                            lock_id = id,
                            "workspace lock ownership row disappeared or expired"
                        );
                        break;
                    }
                    Err(err) => {
                        tracing::warn!(
                            lock_id = id,
                            error = %format!("{err:#}"),
                            "workspace lock refresh failed; retrying before safety margin"
                        );
                        let Some(remaining) = state.remaining_before_safety_margin(now_ms()) else {
                            state.mark_lost();
                            break;
                        };
                        delay = retry_delay.min(remaining);
                        retry_delay = retry_delay.saturating_mul(2).min(timing.retry_max);
                    }
                }
            }
        })
    }

    async fn refresh_workspace_lock(&self, id: i64, lease_ms: i64) -> Result<Option<i64>> {
        let expires_at = sqlx::query_scalar(
            r#"
            UPDATE workspace_locks
            SET expires_at_ms = CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) + ?
            WHERE id = ?
              AND expires_at_ms >= CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER)
            RETURNING expires_at_ms
            "#,
        )
        .bind(lease_ms)
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to refresh workspace lock")?;
        Ok(expires_at)
    }

    #[cfg(test)]
    pub(crate) async fn workspace_lock_count_for_tests(&self) -> Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM workspace_locks")
            .fetch_one(&self.pool)
            .await
            .context("Failed to count workspace locks")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_utils::TestStorage;

    #[test]
    fn wait_window_covers_a_full_stale_lease() {
        assert!(
            WORKSPACE_LOCK_STALE_WAIT.as_millis() >= WORKSPACE_LOCK_LEASE_MS as u128,
            "acquisition wait must outlive stale lock leases"
        );
    }

    fn fast_timing() -> WorkspaceLeaseTiming {
        WorkspaceLeaseTiming {
            lease: Duration::from_millis(400),
            refresh: Duration::from_millis(60),
            safety_margin: Duration::from_millis(100),
            retry_min: Duration::from_millis(20),
            retry_max: Duration::from_millis(50),
            live_wait: Duration::from_millis(80),
            stale_wait: Duration::from_millis(500),
            poll: Duration::from_millis(10),
        }
    }

    #[tokio::test]
    async fn live_lock_owner_fails_fast_with_coordination_hint() {
        let fixture = TestStorage::new().await;
        let owner = fixture.start_session().await;
        let contender = fixture.start_session().await;
        fixture
            .storage
            .record_session_heartbeat(owner, true)
            .await
            .unwrap();
        let _owner_guard = fixture
            .storage
            .acquire_workspace_write_lock_with_timing(fixture.project_path(), owner, fast_timing())
            .await
            .unwrap();

        let started = Instant::now();
        let error = fixture
            .storage
            .acquire_workspace_write_lock_with_timing(
                fixture.project_path(),
                contender,
                fast_timing(),
            )
            .await
            .expect_err("a live owner must keep the lock");

        assert!(started.elapsed() < Duration::from_millis(300));
        assert!(error.to_string().contains(&format!("session #{owner}")));
        assert!(error.to_string().contains("coordinate with it via peers"));
    }

    #[tokio::test]
    async fn transient_refresh_error_recovers_and_keeps_writer_exclusive() {
        let fixture = TestStorage::new().await;
        let owner = fixture.start_session().await;
        let contender = fixture.start_session().await;
        let guard = fixture
            .storage
            .acquire_workspace_write_lock_with_timing(fixture.project_path(), owner, fast_timing())
            .await
            .unwrap();
        guard.inject_refresh_failures(1);

        tokio::time::sleep(Duration::from_millis(180)).await;

        assert_eq!(guard.status(), WorkspaceLeaseStatus::Held);
        guard.confirm_held().await.unwrap();
        let project_id = fixture
            .storage
            .ensure_project(fixture.project_path())
            .await
            .unwrap();
        let contender_lock = fixture
            .storage
            .try_acquire_workspace_write_lock(
                project_id,
                contender,
                i64::from(std::process::id()),
                now_ms(),
            )
            .await
            .unwrap();
        assert!(contender_lock.is_none());
    }

    #[tokio::test]
    async fn persistent_refresh_errors_cancel_before_a_contender_can_enter() {
        let fixture = TestStorage::new().await;
        let owner = fixture.start_session().await;
        let contender = fixture.start_session().await;
        let guard = fixture
            .storage
            .acquire_workspace_write_lock_with_timing(fixture.project_path(), owner, fast_timing())
            .await
            .unwrap();
        guard.inject_refresh_failures(usize::MAX);

        let cancellation = guard.run_while_held(std::future::pending::<Result<()>>());
        let error = tokio::time::timeout(Duration::from_secs(1), cancellation)
            .await
            .expect("lease loss should cancel before the timeout")
            .expect_err("persistent renewal failure must lose the lease");

        assert!(error.to_string().contains("lease was lost"), "{error:#}");
        assert_eq!(guard.status(), WorkspaceLeaseStatus::Lost);
        let project_id = fixture
            .storage
            .ensure_project(fixture.project_path())
            .await
            .unwrap();
        let contender_lock = fixture
            .storage
            .try_acquire_workspace_write_lock(
                project_id,
                contender,
                i64::from(std::process::id()),
                now_ms(),
            )
            .await
            .unwrap();
        assert!(
            contender_lock.is_none(),
            "the owner must cancel inside the safety margin, before row expiry"
        );
        assert!(guard.confirm_held().await.is_err());
    }

    #[tokio::test]
    async fn long_operation_refreshes_past_multiple_initial_lease_windows() {
        let fixture = TestStorage::new().await;
        let owner = fixture.start_session().await;
        let contender = fixture.start_session().await;
        let guard = fixture
            .storage
            .acquire_workspace_write_lock_with_timing(fixture.project_path(), owner, fast_timing())
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(950)).await;

        assert_eq!(guard.status(), WorkspaceLeaseStatus::Held);
        guard.confirm_held().await.unwrap();
        let project_id = fixture
            .storage
            .ensure_project(fixture.project_path())
            .await
            .unwrap();
        let contender_lock = fixture
            .storage
            .try_acquire_workspace_write_lock(
                project_id,
                contender,
                i64::from(std::process::id()),
                now_ms(),
            )
            .await
            .unwrap();
        assert!(contender_lock.is_none());
    }

    #[tokio::test]
    async fn stale_owner_cannot_refresh_a_replacement_lock_row() {
        let fixture = TestStorage::new().await;
        let owner = fixture.start_session().await;
        let contender = fixture.start_session().await;
        let mut guard = fixture
            .storage
            .acquire_workspace_write_lock_with_timing(fixture.project_path(), owner, fast_timing())
            .await
            .unwrap();
        if let Some(refresh_task) = guard.refresh_task.take() {
            refresh_task.abort();
        }
        fixture
            .storage
            .release_workspace_lock(guard.id)
            .await
            .unwrap();
        let project_id = fixture
            .storage
            .ensure_project(fixture.project_path())
            .await
            .unwrap();
        let replacement_id = fixture
            .storage
            .try_acquire_workspace_write_lock(
                project_id,
                contender,
                i64::from(std::process::id()),
                now_ms(),
            )
            .await
            .unwrap()
            .unwrap();

        let error = guard
            .confirm_held()
            .await
            .expect_err("the old row id is the fencing token");

        assert!(error.to_string().contains("ownership row"), "{error:#}");
        assert_ne!(guard.id, replacement_id);
        assert_eq!(guard.status(), WorkspaceLeaseStatus::Lost);
        fixture
            .storage
            .release_workspace_lock(replacement_id)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn writers_serialize_and_release() {
        let fixture = TestStorage::new().await;
        let project_id = fixture
            .storage
            .ensure_project(fixture.project_path())
            .await
            .unwrap();
        let writer_a = fixture.start_session().await;
        let writer_b = fixture.start_session().await;
        let owner_pid = i64::from(std::process::id());

        let first_write = fixture
            .storage
            .try_acquire_workspace_write_lock(project_id, writer_a, owner_pid, now_ms())
            .await
            .unwrap()
            .unwrap();
        let blocked_write = fixture
            .storage
            .try_acquire_workspace_write_lock(project_id, writer_b, owner_pid, now_ms())
            .await
            .unwrap();
        assert!(blocked_write.is_none());

        fixture
            .storage
            .release_workspace_lock(first_write)
            .await
            .unwrap();
        let second_write = fixture
            .storage
            .try_acquire_workspace_write_lock(project_id, writer_b, owner_pid, now_ms())
            .await
            .unwrap()
            .unwrap();
        fixture
            .storage
            .release_workspace_lock(second_write)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .storage
                .workspace_lock_count_for_tests()
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn reads_do_not_wait_for_a_writer_lease() {
        use crate::tool::{ReadTool, ReadTracker, Tool, ToolOutput};

        let fixture = TestStorage::new().await;
        let writer = fixture.start_session().await;
        std::fs::write(fixture.project_path().join("visible.txt"), "current\n").unwrap();
        let _write_guard = fixture
            .storage
            .acquire_workspace_write_lock(fixture.project_path(), writer)
            .await
            .unwrap();
        let read_tool = ReadTool::new(fixture.project_path().to_path_buf(), ReadTracker::new());

        let output = tokio::time::timeout(
            Duration::from_secs(1),
            read_tool.execute(serde_json::json!({ "path": "visible.txt" })),
        )
        .await
        .expect("an optimistic read must not wait for the writer lease")
        .unwrap();

        let ToolOutput::Read { text, .. } = output else {
            panic!("read tool returned an unexpected output variant");
        };
        assert!(text.contains("current"), "{text}");
    }

    #[tokio::test]
    async fn expired_locks_are_reclaimed() {
        let fixture = TestStorage::new().await;
        let project_id = fixture
            .storage
            .ensure_project(fixture.project_path())
            .await
            .unwrap();
        let stale_writer = fixture.start_session().await;
        let writer = fixture.start_session().await;
        let owner_pid = i64::from(std::process::id());
        let now = now_ms();

        let stale_write = fixture
            .storage
            .try_acquire_workspace_write_lock(project_id, stale_writer, owner_pid, now)
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE workspace_locks SET expires_at_ms = ? WHERE id = ?")
            .bind(now.saturating_sub(1))
            .bind(stale_write)
            .execute(&fixture.storage.pool)
            .await
            .unwrap();

        let write = fixture
            .storage
            .try_acquire_workspace_write_lock(project_id, writer, owner_pid, now)
            .await
            .unwrap();
        assert!(write.is_some());
        assert_eq!(
            fixture
                .storage
                .workspace_lock_count_for_tests()
                .await
                .unwrap(),
            1
        );

        fixture
            .storage
            .release_workspace_lock(write.unwrap())
            .await
            .unwrap();
    }
}
