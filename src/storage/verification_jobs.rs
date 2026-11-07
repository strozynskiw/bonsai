//! Cross-process verification scheduler storage (#142).
//!
//! One attempt per exact verification identity, plus a weaker *lane* key that
//! serializes Cargo jobs sharing one canonical target directory even when their
//! result keys differ. Producers hold a renewable lease; subscribers follow the
//! attempt and are woken by the single terminal row it publishes.
//!
//! This mirrors the `workspace_locks` lease discipline: a lease that lapses is
//! never silently treated as permission to run concurrently. Reclaim happens
//! only once the old producer is confirmed gone — or, where the platform offers
//! no liveness probe, only after it has stopped refreshing for a further full
//! lease period.

use super::*;

/// Producer lease window. Long enough that a healthy producer always renews
/// before the safety margin, short enough that a crashed producer frees its
/// lane without a manual repair step.
pub(crate) const VERIFICATION_JOB_LEASE_MS: i64 = 30_000;

/// Unreferenced terminal attempts are expired after this window. Referenced
/// attempts (any session holds verification evidence linked to them) and
/// in-flight attempts are never expired.
pub(crate) const VERIFICATION_JOB_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Persisted state of one verification attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerificationJobState {
    /// Accepted, waiting for the lane's current attempt to finish.
    Queued,
    /// The producer holds the lease and the command is executing.
    Running,
    /// Terminal: one result was published for every subscriber.
    Completed,
    /// Terminal: every subscriber detached, so the process group was reaped.
    Cancelled,
    /// Terminal: the check could not be executed in this environment.
    EnvironmentBlocked,
    /// Terminal: the producer died or lost its lease; no result was published.
    Interrupted,
}

impl VerificationJobState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::EnvironmentBlocked => "environment_blocked",
            Self::Interrupted => "interrupted",
        }
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "cancelled" => Some(Self::Cancelled),
            "environment_blocked" => Some(Self::EnvironmentBlocked),
            "interrupted" => Some(Self::Interrupted),
            _ => None,
        }
    }

    /// Whether the attempt will never change state again.
    pub(crate) const fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// Persisted state of one session's interest in an attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerificationSubscriberState {
    /// Waiting for a queued attempt to reach the front of its lane.
    Following,
    /// This session's producer is executing the attempt.
    Running,
    /// The session consumed the terminal result.
    Completed,
    /// The session detached without consuming a result.
    Cancelled,
}

impl VerificationSubscriberState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Following => "following",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Everything the store needs to admit one verification request.
#[derive(Debug, Clone)]
pub(crate) struct VerificationJobRequest {
    pub(crate) project_root: PathBuf,
    /// Exact identity key: only an identical key may reuse a result.
    pub(crate) job_key: String,
    /// Compatibility key: attempts sharing it serialize on one lane.
    pub(crate) lane_key: String,
    pub(crate) session_id: SessionId,
    pub(crate) tool_call_id: String,
    pub(crate) command: String,
    pub(crate) command_cwd: String,
    pub(crate) timeout_secs: u64,
    pub(crate) identity_json: String,
    pub(crate) policy_json: String,
    pub(crate) lease_ms: i64,
    /// A just-completed identical attempt stays reusable for this long.
    pub(crate) reuse_window_ms: i64,
}

/// Outcome of admitting a verification request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VerificationJobClaim {
    /// This session is the producer and must execute the command.
    Started(i64),
    /// An identical attempt is already live; follow it.
    Following(i64),
    /// A different attempt holds this lane; wait for it, then promote.
    Queued { job_id: i64, ahead: u32 },
}

/// Read-only view of an attempt used by the waiting and delivery paths.
#[derive(Debug, Clone)]
pub(crate) struct VerificationJobSnapshot {
    pub(crate) state: VerificationJobState,
    pub(crate) queued_at_ms: i64,
    pub(crate) started_at_ms: Option<i64>,
    pub(crate) finished_at_ms: Option<i64>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) confined: bool,
    pub(crate) output_body: Option<String>,
    pub(crate) output_truncated: bool,
    pub(crate) terminal_reason: Option<String>,
}

impl VerificationJobSnapshot {
    /// Queue and execution durations, kept separate so waiting never consumes
    /// the command's own execution timeout budget.
    pub(crate) fn queue_ms(&self) -> i64 {
        match self.started_at_ms {
            Some(started) => started.saturating_sub(self.queued_at_ms),
            None => match self.finished_at_ms {
                Some(finished) => finished.saturating_sub(self.queued_at_ms),
                None => now_ms().saturating_sub(self.queued_at_ms),
            },
        }
    }

    pub(crate) fn compute_ms(&self) -> i64 {
        match (self.started_at_ms, self.finished_at_ms) {
            (Some(started), Some(finished)) => finished.saturating_sub(started),
            (Some(started), None) => now_ms().saturating_sub(started),
            _ => 0,
        }
    }
}

/// The single terminal payload published by an attempt.
#[derive(Debug, Clone)]
pub(crate) struct VerificationJobPayload {
    pub(crate) exit_code: Option<i32>,
    pub(crate) timed_out: bool,
    pub(crate) confined: bool,
    pub(crate) body: String,
    pub(crate) truncated: bool,
}

fn job_snapshot(row: &sqlx::sqlite::SqliteRow) -> Result<VerificationJobSnapshot> {
    let state_label: String = row.try_get("state")?;
    Ok(VerificationJobSnapshot {
        state: VerificationJobState::from_label(&state_label)
            .with_context(|| format!("Unknown verification job state {state_label:?}"))?,
        queued_at_ms: row.try_get("queued_at_ms")?,
        started_at_ms: row.try_get("started_at_ms")?,
        finished_at_ms: row.try_get("finished_at_ms")?,
        exit_code: row.try_get("exit_code")?,
        timed_out: row.try_get::<i64, _>("timed_out")? != 0,
        confined: row.try_get::<i64, _>("confined")? != 0,
        output_body: row.try_get("output_body")?,
        output_truncated: row.try_get::<i64, _>("output_truncated")? != 0,
        terminal_reason: row.try_get("terminal_reason")?,
    })
}

/// What this platform can say about a recorded producer process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProducerLiveness {
    /// The process exists (or belongs to another user, which still means it
    /// exists). A live producer's lane is never stolen.
    Alive,
    /// The process is confirmed gone.
    Dead,
    /// No cheap probe exists here, so death cannot be confirmed.
    Unknown,
}

/// Probe a recorded producer process.
#[cfg(unix)]
fn producer_liveness(pid: i64) -> ProducerLiveness {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let Ok(pid) = i32::try_from(pid) else {
        return ProducerLiveness::Dead;
    };
    match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(Errno::EPERM) => ProducerLiveness::Alive,
        Err(_) => ProducerLiveness::Dead,
    }
}

#[cfg(not(unix))]
fn producer_liveness(_pid: i64) -> ProducerLiveness {
    ProducerLiveness::Unknown
}

/// Grace past lease expiry before an unconfirmable producer counts as
/// abandoned. A healthy producer refreshes its lease every few seconds, so a
/// lease that has gone unrefreshed for a further full lease period — with no
/// process left to inspect — is reclaimed rather than wedging its lane forever.
const UNCONFIRMED_PRODUCER_GRACE_MS: i64 = VERIFICATION_JOB_LEASE_MS;

/// Whether a lapsed lease may be fenced and reclaimed.
///
/// A confirmed-live producer always keeps its lane. A producer is reclaimed only
/// when it is confirmed gone, or when this platform cannot confirm it and it has
/// already stopped refreshing for a full lease period beyond expiry.
fn may_reclaim(liveness: ProducerLiveness, lease_expired_at_ms: i64, now: i64) -> bool {
    match liveness {
        ProducerLiveness::Alive => false,
        ProducerLiveness::Dead => true,
        ProducerLiveness::Unknown => {
            now >= lease_expired_at_ms.saturating_add(UNCONFIRMED_PRODUCER_GRACE_MS)
        }
    }
}

impl Storage {
    /// Admit one verification request: join an identical live attempt, queue
    /// behind a different attempt on the same lane, or start a new one.
    ///
    /// Runs in a single `BEGIN IMMEDIATE` transaction so two Bonsai processes
    /// cannot both decide they are the producer for one identity.
    pub(crate) async fn claim_verification_job(
        &self,
        request: &VerificationJobRequest,
    ) -> Result<VerificationJobClaim> {
        let project_id = self.ensure_project(&request.project_root).await?;
        let now = now_ms();
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin verification job transaction")?;

        self.reap_abandoned_verification_jobs(&mut tx, now).await?;

        // 1. An identical live attempt wins outright: no second execution.
        if let Some(existing) = sqlx::query(
            "SELECT id FROM verification_jobs \
             WHERE job_key = ? AND state IN ('queued', 'running') \
             ORDER BY id LIMIT 1",
        )
        .bind(&request.job_key)
        .fetch_optional(&mut *tx)
        .await
        .context("Failed to look up a matching verification job")?
        {
            let job_id: i64 = existing.try_get("id")?;
            subscription_upsert(
                &mut tx,
                job_id,
                request,
                VerificationSubscriberState::Following,
            )
            .await?;
            tx.commit()
                .await
                .context("Failed to commit verification job join")?;
            return Ok(VerificationJobClaim::Following(job_id));
        }

        // 1b. An identical attempt that just completed is still exact: reuse it
        // rather than paying for a second execution of the same check.
        let reuse_cutoff = now.saturating_sub(request.reuse_window_ms);
        if let Some(existing) = sqlx::query(
            "SELECT id FROM verification_jobs \
             WHERE job_key = ? AND state = 'completed' AND finished_at_ms >= ? \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(&request.job_key)
        .bind(reuse_cutoff)
        .fetch_optional(&mut *tx)
        .await
        .context("Failed to look up a reusable verification job")?
        {
            let job_id: i64 = existing.try_get("id")?;
            subscription_upsert(
                &mut tx,
                job_id,
                request,
                VerificationSubscriberState::Following,
            )
            .await?;
            tx.commit()
                .await
                .context("Failed to commit verification reuse")?;
            return Ok(VerificationJobClaim::Following(job_id));
        }

        // 2. A different attempt owns the lane: queue behind it.
        let lane_busy: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM verification_jobs \
             WHERE lane_key = ? AND state IN ('queued', 'running')",
        )
        .bind(&request.lane_key)
        .fetch_one(&mut *tx)
        .await
        .context("Failed to inspect the verification lane")?;

        let state = if lane_busy > 0 {
            VerificationJobState::Queued
        } else {
            VerificationJobState::Running
        };
        let job_id = insert_verification_job(&mut tx, project_id, request, state, now).await?;
        let subscriber_state = if state == VerificationJobState::Running {
            VerificationSubscriberState::Running
        } else {
            VerificationSubscriberState::Following
        };
        subscription_upsert(&mut tx, job_id, request, subscriber_state).await?;

        if state == VerificationJobState::Running {
            tx.commit()
                .await
                .context("Failed to commit verification job start")?;
            return Ok(VerificationJobClaim::Started(job_id));
        }

        let ahead = queue_position(&mut tx, &request.lane_key, job_id).await?;
        tx.commit()
            .await
            .context("Failed to commit verification job queue entry")?;
        Ok(VerificationJobClaim::Queued { job_id, ahead })
    }

    /// Extend a producer lease. `None` means the lease is no longer held.
    pub(crate) async fn refresh_verification_job_lease(
        &self,
        job_id: i64,
        session_id: SessionId,
        lease_ms: i64,
    ) -> Result<Option<i64>> {
        let expires_at = sqlx::query_scalar(
            "UPDATE verification_jobs \
             SET lease_expires_at_ms = \
                 CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) + ? \
             WHERE id = ? AND producer_session_id = ? AND state = 'running' \
               AND lease_expires_at_ms >= CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER) \
             RETURNING lease_expires_at_ms",
        )
        .bind(lease_ms)
        .bind(job_id)
        .bind(session_id.as_i64())
        .fetch_optional(&self.pool)
        .await
        .context("Failed to refresh the verification job lease")?;
        Ok(expires_at)
    }

    /// Publish the one terminal result for an attempt. Returns `false` when the
    /// lease was already lost, so a fenced producer cannot overwrite a result
    /// another owner may already have published.
    pub(crate) async fn publish_verification_job_result(
        &self,
        job_id: i64,
        session_id: SessionId,
        payload: &VerificationJobPayload,
    ) -> Result<bool> {
        let now = now_ms();
        let updated = sqlx::query(
            "UPDATE verification_jobs \
             SET state = 'completed', finished_at_ms = ?, exit_code = ?, timed_out = ?, \
                 confined = ?, output_body = ?, output_truncated = ?, \
                 lease_expires_at_ms = NULL \
             WHERE id = ? AND producer_session_id = ? AND state = 'running' \
               AND lease_expires_at_ms >= ?",
        )
        .bind(now)
        .bind(payload.exit_code)
        .bind(i64::from(payload.timed_out))
        .bind(i64::from(payload.confined))
        .bind(&payload.body)
        .bind(i64::from(payload.truncated))
        .bind(job_id)
        .bind(session_id.as_i64())
        .bind(now)
        .execute(&self.pool)
        .await
        .context("Failed to publish the verification job result")?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Close an attempt without a reusable result. Used for producer failure,
    /// subscriber-wide cancellation and environment blocks.
    pub(crate) async fn close_verification_job(
        &self,
        job_id: i64,
        state: VerificationJobState,
        reason: &str,
    ) -> Result<bool> {
        anyhow::ensure!(
            state.is_terminal(),
            "Verification job can only be closed into a terminal state"
        );
        let updated = sqlx::query(
            "UPDATE verification_jobs \
             SET state = ?, finished_at_ms = ?, terminal_reason = ?, \
                 lease_expires_at_ms = NULL \
             WHERE id = ? AND state IN ('queued', 'running')",
        )
        .bind(state.label())
        .bind(now_ms())
        .bind(reason)
        .bind(job_id)
        .execute(&self.pool)
        .await
        .context("Failed to close the verification job")?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Detach one subscriber. Returns the number of subscribers that still need
    /// a result, so the caller can decide whether the producer must stop.
    pub(crate) async fn cancel_verification_subscription(
        &self,
        job_id: i64,
        session_id: SessionId,
        tool_call_id: &str,
    ) -> Result<u32> {
        let now = now_ms();
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin verification cancellation transaction")?;
        storage_op!(
            &mut tx,
            "cancel verification subscription",
            sqlx::query(
                "UPDATE verification_job_subscribers \
                 SET state = ?, finished_at_ms = ? \
                 WHERE job_id = ? AND session_id = ? AND tool_call_id = ? \
                   AND state IN ('following', 'running')",
            )
            .bind(VerificationSubscriberState::Cancelled.label())
            .bind(now)
            .bind(job_id)
            .bind(session_id.as_i64())
            .bind(tool_call_id),
        )?;
        let remaining = active_subscription_count(&mut tx, job_id).await?;
        if remaining == 0 {
            storage_op!(
                &mut tx,
                "clear verification producers",
                sqlx::query(
                    "UPDATE verification_jobs SET producer_session_id = NULL \
                     WHERE id = ? AND state = 'running'",
                )
                .bind(job_id),
            )?;
            // A queued attempt nobody is waiting for would hold its lane
            // forever, so it is retired instead of being left behind.
            storage_op!(
                &mut tx,
                "retire an unsubscribed queued verification job",
                sqlx::query(
                    "UPDATE verification_jobs \
                     SET state = 'cancelled', finished_at_ms = ?, \
                         terminal_reason = 'every subscriber detached', \
                         lease_expires_at_ms = NULL \
                     WHERE id = ? AND state = 'queued'",
                )
                .bind(now)
                .bind(job_id),
            )?;
        }
        tx.commit()
            .await
            .context("Failed to commit verification cancellation")?;
        Ok(remaining)
    }

    /// Subscribers that still need a result from this attempt.
    pub(crate) async fn active_verification_subscribers(&self, job_id: i64) -> Result<u32> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM verification_job_subscribers \
             WHERE job_id = ? AND state IN ('following', 'running')",
        )
        .bind(job_id)
        .fetch_one(&self.pool)
        .await
        .context("Failed to count verification subscribers")?;
        u32::try_from(count).context("Verification subscriber count is out of range")
    }

    /// Mark a subscriber's delivery complete with its own wait/compute split.
    pub(crate) async fn complete_verification_subscription(
        &self,
        job_id: i64,
        session_id: SessionId,
        tool_call_id: &str,
        queue_ms: i64,
        compute_ms: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE verification_job_subscribers \
             SET state = ?, finished_at_ms = ?, queue_ms = ?, compute_ms = ? \
             WHERE job_id = ? AND session_id = ? AND tool_call_id = ?",
        )
        .bind(VerificationSubscriberState::Completed.label())
        .bind(now_ms())
        .bind(queue_ms)
        .bind(compute_ms)
        .bind(job_id)
        .bind(session_id.as_i64())
        .bind(tool_call_id)
        .execute(&self.pool)
        .await
        .context("Failed to complete the verification subscription")?;
        Ok(())
    }

    pub(crate) async fn load_verification_job(
        &self,
        job_id: i64,
    ) -> Result<Option<VerificationJobSnapshot>> {
        let row = sqlx::query(
            "SELECT state, queued_at_ms, started_at_ms, finished_at_ms, \
             exit_code, timed_out, confined, output_body, output_truncated, terminal_reason \
             FROM verification_jobs WHERE id = ?",
        )
        .bind(job_id)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to load the verification job")?;
        row.as_ref().map(job_snapshot).transpose()
    }

    /// Bind a shared attempt to one session's verification evidence. A linked
    /// attempt is retained and is not quarantined as an accidental copy.
    pub(crate) async fn link_verification_shared_job(
        &self,
        job_id: i64,
        session_id: SessionId,
        tool_call_id: &str,
        shared_job_key: &str,
        evidence_digest: &str,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO verification_shared_job_links \
               (job_id, session_id, tool_call_id, shared_job_key, evidence_digest, created_at_ms) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT DO NOTHING",
        )
        .bind(job_id)
        .bind(session_id.as_i64())
        .bind(tool_call_id)
        .bind(shared_job_key)
        .bind(evidence_digest)
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .context("Failed to link the shared verification job")?;
        Ok(())
    }

    /// Expire unreferenced terminal attempts. In-flight attempts and attempts
    /// any session's evidence references are preserved.
    pub(crate) async fn expire_verification_jobs(&self, retention_ms: i64) -> Result<u64> {
        let cutoff = now_ms().saturating_sub(retention_ms);
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin verification retention transaction")?;
        let deleted = sqlx::query(
            "DELETE FROM verification_jobs \
             WHERE state NOT IN ('queued', 'running') \
               AND finished_at_ms IS NOT NULL \
               AND finished_at_ms < ? \
               AND NOT EXISTS ( \
                 SELECT 1 FROM verification_shared_job_links \
                 WHERE verification_shared_job_links.job_id = verification_jobs.id \
               )",
        )
        .bind(cutoff)
        .execute(&mut *tx)
        .await
        .context("Failed to expire verification jobs")?
        .rows_affected();
        sqlx::query(
            "DELETE FROM verification_job_subscribers \
             WHERE job_id NOT IN (SELECT id FROM verification_jobs)",
        )
        .execute(&mut *tx)
        .await
        .context("Failed to expire orphaned verification subscribers")?;
        tx.commit()
            .await
            .context("Failed to commit verification retention")?;
        Ok(deleted)
    }

    /// Fence and reclaim attempts whose producer lease lapsed. A lapsed lease
    /// belonging to a still-live producer process is not stolen.
    async fn reap_abandoned_verification_jobs(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        now: i64,
    ) -> Result<Vec<i64>> {
        let abandoned = sqlx::query(
            "SELECT id, producer_pid, lease_expires_at_ms FROM verification_jobs \
             WHERE state = 'running' AND lease_expires_at_ms IS NOT NULL \
               AND lease_expires_at_ms < ?",
        )
        .bind(now)
        .fetch_all(&mut **tx)
        .await
        .context("Failed to inspect expired verification jobs")?;

        let mut interrupted = Vec::new();
        for row in abandoned {
            let job_id: i64 = row.try_get("id")?;
            let producer_pid: Option<i64> = row.try_get("producer_pid")?;
            let lease_expired_at_ms: Option<i64> = row.try_get("lease_expires_at_ms")?;
            // An attempt with no recorded producer pid belongs to a producer
            // that died before it could be identified, so nothing can be
            // confirmed about it.
            let liveness = producer_pid.map_or(ProducerLiveness::Unknown, producer_liveness);
            if !may_reclaim(liveness, lease_expired_at_ms.unwrap_or(now), now) {
                // Never steal a live producer's lane; wait for its own renewal.
                continue;
            }
            let updated = sqlx::query(
                "UPDATE verification_jobs \
                 SET state = 'interrupted', finished_at_ms = ?, \
                     terminal_reason = 'producer_lease_lost', lease_expires_at_ms = NULL \
                 WHERE id = ? AND state = 'running'",
            )
            .bind(now)
            .bind(job_id)
            .execute(&mut **tx)
            .await
            .context("Failed to interrupt the abandoned verification job")?
            .rows_affected();
            if updated == 1 {
                interrupted.push(job_id);
            }
        }
        Ok(interrupted)
    }

    /// Public reclaim pass: fence and interrupt attempts whose producer lease
    /// lapsed and whose producer process is confirmed gone. Returns the ids of
    /// the attempts that were marked interrupted.
    pub(crate) async fn reap_verification_jobs(&self) -> Result<Vec<i64>> {
        let now = now_ms();
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin verification reclaim transaction")?;
        let interrupted = self.reap_abandoned_verification_jobs(&mut tx, now).await?;
        tx.commit()
            .await
            .context("Failed to commit verification reclaim")?;
        Ok(interrupted)
    }

    /// Per-database key for domain-separated keyed fingerprints of
    /// secret-bearing inputs. Created once, never exposed, and never persisted
    /// alongside the values it fingerprints.
    pub(crate) async fn verification_scheduler_secret_key(&self) -> Result<[u8; 32]> {
        let stored = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT secret_key FROM verification_scheduler_keys WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await
        .context("Failed to load the verification scheduler key")?;
        if let Some(key) = stored {
            return scheduler_key_bytes(&key);
        }

        let mut material = Vec::new();
        material.extend_from_slice(self.db_path().to_string_lossy().as_bytes());
        material.extend_from_slice(&std::process::id().to_le_bytes());
        material.extend_from_slice(&now_ms().to_le_bytes());
        // UUIDv7 draws its random half from the OS CSPRNG, so the key does not
        // rest on the database path, the pid, and the wall clock alone.
        material.extend_from_slice(uuid::Uuid::now_v7().as_bytes());
        let generated = blake3::hash(&material).as_bytes().to_vec();
        sqlx::query(
            "INSERT INTO verification_scheduler_keys (id, secret_key, created_at_ms) \
             VALUES (1, ?, ?) ON CONFLICT(id) DO NOTHING",
        )
        .bind(&generated)
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .context("Failed to store the verification scheduler key")?;
        // Re-read so two concurrent first calls agree on one key.
        let stored = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT secret_key FROM verification_scheduler_keys WHERE id = 1",
        )
        .fetch_one(&self.pool)
        .await
        .context("Failed to reload the verification scheduler key")?;
        scheduler_key_bytes(&stored)
    }

    #[cfg(test)]
    pub(crate) async fn verification_job_count_for_tests(&self) -> Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM verification_jobs")
            .fetch_one(&self.pool)
            .await
            .context("Failed to count verification jobs")
    }

    #[cfg(test)]
    pub(crate) async fn verification_job_state_for_tests(
        &self,
        job_id: i64,
    ) -> Result<Option<VerificationJobState>> {
        Ok(self
            .load_verification_job(job_id)
            .await?
            .map(|job| job.state))
    }

    /// Force a producer's lease past its window and record the pid of a process
    /// that is definitely gone, so the reclaim pass can be exercised without a
    /// real crash.
    #[cfg(test)]
    pub(crate) async fn verification_job_ids_for_tests(&self) -> Result<Vec<i64>> {
        sqlx::query_scalar("SELECT id FROM verification_jobs ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .context("Failed to list verification jobs")
    }

    /// Every stored identity and policy document, for redaction regressions.
    #[cfg(test)]
    pub(crate) async fn verification_job_documents_for_tests(&self) -> Result<String> {
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT identity_json || policy_json FROM verification_jobs ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to load verification job documents")?;
        Ok(rows.join("\n"))
    }

    /// Force a producer's lease past its window and record the pid of a process
    /// that is definitely gone, so the reclaim pass can be exercised without a
    /// real crash.
    #[cfg(test)]
    pub(crate) async fn expire_verification_job_for_tests(
        &self,
        job_id: i64,
        producer_pid: i64,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE verification_jobs SET lease_expires_at_ms = 0, producer_pid = ? WHERE id = ?",
        )
        .bind(producer_pid)
        .bind(job_id)
        .execute(&self.pool)
        .await
        .context("Failed to expire a test verification job")?;
        Ok(())
    }

    /// Move a terminal attempt's completion time into the past.
    #[cfg(test)]
    pub(crate) async fn age_verification_job_for_tests(
        &self,
        job_id: i64,
        finished_at_ms: i64,
    ) -> Result<()> {
        sqlx::query("UPDATE verification_jobs SET finished_at_ms = ? WHERE id = ?")
            .bind(finished_at_ms)
            .bind(job_id)
            .execute(&self.pool)
            .await
            .context("Failed to age a test verification job")?;
        Ok(())
    }

    /// How many shared attempts this session's verification evidence is linked
    /// to. A linked attempt is retained and is exempt from duplicate quarantine.
    #[cfg(test)]
    pub(crate) async fn linked_shared_job_count_for_tests(
        &self,
        session_id: SessionId,
    ) -> Result<i64> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM verification_shared_job_links WHERE session_id = ?",
        )
        .bind(session_id.as_i64())
        .fetch_one(&self.pool)
        .await
        .context("Failed to count linked shared verification jobs")
    }
}

fn scheduler_key_bytes(bytes: &[u8]) -> Result<[u8; 32]> {
    <[u8; 32]>::try_from(bytes).context("Verification scheduler key has an unexpected length")
}

async fn insert_verification_job(
    tx: &mut Transaction<'_, Sqlite>,
    project_id: i64,
    request: &VerificationJobRequest,
    state: VerificationJobState,
    now: i64,
) -> Result<i64> {
    let running = state == VerificationJobState::Running;
    sqlx::query(
        "INSERT INTO verification_jobs ( \
           job_key, lane_key, project_id, requested_by_session_id, producer_session_id, \
           producer_pid, state, command, command_cwd, timeout_secs, identity_json, \
           policy_json, lease_expires_at_ms, queued_at_ms, started_at_ms \
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         RETURNING id",
    )
    .bind(&request.job_key)
    .bind(&request.lane_key)
    .bind(project_id)
    .bind(request.session_id.as_i64())
    .bind(running.then(|| request.session_id.as_i64()))
    .bind(running.then(|| i64::from(std::process::id())))
    .bind(state.label())
    .bind(&request.command)
    .bind(&request.command_cwd)
    .bind(i64::try_from(request.timeout_secs).unwrap_or(i64::MAX))
    .bind(&request.identity_json)
    .bind(&request.policy_json)
    .bind(running.then(|| now.saturating_add(request.lease_ms)))
    .bind(now)
    .bind(running.then_some(now))
    .fetch_one(&mut **tx)
    .await
    .and_then(|row| row.try_get::<i64, _>("id"))
    .context("Failed to insert the verification job")
}

async fn subscription_upsert(
    tx: &mut Transaction<'_, Sqlite>,
    job_id: i64,
    request: &VerificationJobRequest,
    state: VerificationSubscriberState,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO verification_job_subscribers \
           (job_id, session_id, tool_call_id, state, joined_at_ms) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(job_id, session_id, tool_call_id) DO UPDATE SET \
           state = CASE \
             WHEN verification_job_subscribers.state = 'completed' \
               THEN verification_job_subscribers.state \
             ELSE excluded.state \
           END",
    )
    .bind(job_id)
    .bind(request.session_id.as_i64())
    .bind(&request.tool_call_id)
    .bind(state.label())
    .bind(now_ms())
    .execute(&mut **tx)
    .await
    .context("Failed to record the verification subscription")?;
    Ok(())
}

/// How many active attempts in the lane precede `job_id`.
async fn queue_position(
    tx: &mut Transaction<'_, Sqlite>,
    lane_key: &str,
    job_id: i64,
) -> Result<u32> {
    let ours: Option<(i64, i64)> =
        sqlx::query_as("SELECT queued_at_ms, id FROM verification_jobs WHERE id = ?")
            .bind(job_id)
            .fetch_optional(&mut **tx)
            .await
            .context("Failed to load the queue position")?;
    let Some((queued_at, id)) = ours else {
        return Ok(0);
    };
    let ahead: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM verification_jobs \
         WHERE lane_key = ? AND state IN ('queued', 'running') \
           AND (queued_at_ms < ? OR (queued_at_ms = ? AND id < ?))",
    )
    .bind(lane_key)
    .bind(queued_at)
    .bind(queued_at)
    .bind(id)
    .fetch_one(&mut **tx)
    .await
    .context("Failed to count the verification queue")?;
    u32::try_from(ahead).context("Verification queue depth is out of range")
}

async fn active_subscription_count(tx: &mut Transaction<'_, Sqlite>, job_id: i64) -> Result<u32> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM verification_job_subscribers \
         WHERE job_id = ? AND state IN ('following', 'running')",
    )
    .bind(job_id)
    .fetch_one(&mut **tx)
    .await
    .context("Failed to count verification subscribers")?;
    u32::try_from(count).context("Verification subscriber count is out of range")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_confirmed_dead_producer_frees_its_lane_immediately() {
        let expired = 1_000_000;

        assert!(
            !may_reclaim(ProducerLiveness::Alive, expired, i64::MAX),
            "a live producer's lane is never stolen, however long it holds it"
        );
        assert!(may_reclaim(ProducerLiveness::Dead, expired, expired + 1));
        assert!(
            !may_reclaim(ProducerLiveness::Unknown, expired, expired + 1),
            "an unconfirmable producer keeps its lane while it can still be refreshing"
        );
        assert!(
            !may_reclaim(
                ProducerLiveness::Unknown,
                expired,
                expired + UNCONFIRMED_PRODUCER_GRACE_MS - 1
            ),
            "the grace window is not shortened by a millisecond"
        );
        assert!(
            may_reclaim(
                ProducerLiveness::Unknown,
                expired,
                expired + UNCONFIRMED_PRODUCER_GRACE_MS
            ),
            "a lane whose producer never refreshed again must not wedge forever"
        );
    }
}
