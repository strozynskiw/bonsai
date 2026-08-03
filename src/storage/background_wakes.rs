//! Durable, exactly-once wake subscriptions for process-local background work.

use super::*;

/// Kind of process-local work a subscription observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundWakeTargetKind {
    BackgroundTask,
    Terminal,
}

impl BackgroundWakeTargetKind {
    const fn as_db_str(self) -> &'static str {
        match self {
            Self::BackgroundTask => "background_task",
            Self::Terminal => "terminal",
        }
    }

    fn from_db_str(value: &str) -> Result<Self> {
        match value {
            "background_task" => Ok(Self::BackgroundTask),
            "terminal" => Ok(Self::Terminal),
            _ => anyhow::bail!("Unknown stored background wake target kind: {value}"),
        }
    }
}

/// A parked requester's exact one-shot wait for local work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundWakeWait {
    pub subscription_id: i64,
    pub requester_generation: i64,
}

/// Immutable arguments used to register or replay a work wait.
#[derive(Debug, Clone, Copy)]
pub struct BackgroundWakeRegistration<'a> {
    pub requester: SessionId,
    pub requester_generation: i64,
    pub owner_runtime_id: &'a str,
    pub operation_key: &'a str,
    pub target_kind: BackgroundWakeTargetKind,
    pub target_id: &'a str,
    pub target_incarnation: &'a str,
    pub observed_version: u64,
    pub output_threshold: Option<usize>,
    pub deadline_at_ms: Option<i64>,
}

/// Immutable registration returned after insert or idempotent replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredBackgroundWake {
    pub wait: BackgroundWakeWait,
    pub target_kind: BackgroundWakeTargetKind,
    pub target_id: String,
    pub target_incarnation: String,
    pub observed_version: u64,
    pub output_threshold: Option<usize>,
    pub deadline_at_ms: Option<i64>,
    pub newly_created: bool,
}

/// Target observation that may atomically wake subscriptions.
#[derive(Debug, Clone, Copy)]
pub struct BackgroundWakeTrigger<'a> {
    pub target_kind: BackgroundWakeTargetKind,
    pub target_id: &'a str,
    pub target_incarnation: &'a str,
    pub wake_reason: &'a str,
    pub wake_version: u64,
    pub total_output_chars: usize,
    pub output: &'a str,
    pub output_truncated: bool,
}

type StoredBackgroundWakeRegistration = (
    i64,
    i64,
    String,
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
);

/// A claimed wake, including only the bounded/redacted payload supplied by the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundWakeDelivery {
    pub wait: BackgroundWakeWait,
    pub target_kind: BackgroundWakeTargetKind,
    pub target_id: String,
    pub target_incarnation: String,
    pub observed_version: u64,
    pub wake_reason: String,
    pub wake_version: u64,
    pub output: String,
    pub output_truncated: bool,
}

impl Storage {
    /// Registers a durable work wait or replays an exact tool-call registration.
    pub async fn register_background_wake(
        &self,
        registration: BackgroundWakeRegistration<'_>,
    ) -> Result<RegisteredBackgroundWake> {
        if registration.operation_key.is_empty() {
            anyhow::bail!("A background wake registration requires an operation key");
        }
        let now = now_ms();
        let created: Option<i64> = sqlx::query_scalar(
            r#"
            INSERT INTO background_wake_subscriptions (
              requester_session_id, requester_generation, owner_runtime_id, operation_key,
              target_kind, target_id, target_incarnation, observed_version,
              output_threshold, deadline_at_ms, created_at_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(requester_session_id, operation_key) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(registration.requester.as_i64())
        .bind(registration.requester_generation)
        .bind(registration.owner_runtime_id)
        .bind(registration.operation_key)
        .bind(registration.target_kind.as_db_str())
        .bind(registration.target_id)
        .bind(registration.target_incarnation)
        .bind(
            i64::try_from(registration.observed_version)
                .context("Background wake version overflow")?,
        )
        .bind(
            registration
                .output_threshold
                .map(i64::try_from)
                .transpose()
                .context("Background wake output threshold overflow")?,
        )
        .bind(registration.deadline_at_ms)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to register background wake")?;
        let stored: StoredBackgroundWakeRegistration = sqlx::query_as(
            "SELECT id, requester_generation, target_kind, target_id, target_incarnation, \
                    observed_version, output_threshold, deadline_at_ms \
             FROM background_wake_subscriptions \
             WHERE requester_session_id = ? AND operation_key = ?",
        )
        .bind(registration.requester.as_i64())
        .bind(registration.operation_key)
        .fetch_one(&self.pool)
        .await
        .context("Background wake registration disappeared during replay")?;
        let (
            id,
            requester_generation,
            kind,
            target_id,
            target_incarnation,
            observed_version,
            output_threshold,
            deadline_at_ms,
        ) = stored;
        let target_kind = BackgroundWakeTargetKind::from_db_str(&kind)?;
        let observed_version =
            u64::try_from(observed_version).context("Stored negative background wake version")?;
        let output_threshold = output_threshold
            .map(usize::try_from)
            .transpose()
            .context("Stored negative background wake output threshold")?;
        if created.is_none()
            && (target_kind != registration.target_kind
                || target_id != registration.target_id
                || target_incarnation != registration.target_incarnation
                || observed_version != registration.observed_version
                || output_threshold != registration.output_threshold
                || deadline_at_ms != registration.deadline_at_ms)
        {
            anyhow::bail!("Background wake operation key was reused with different arguments");
        }
        Ok(RegisteredBackgroundWake {
            wait: BackgroundWakeWait {
                subscription_id: id,
                requester_generation,
            },
            target_kind,
            target_id,
            target_incarnation,
            observed_version,
            output_threshold,
            deadline_at_ms,
            newly_created: created.is_some(),
        })
    }

    /// Atomically claims every qualifying pending wait for a changed target.
    pub async fn fire_background_wakes(
        &self,
        owner_runtime_id: &str,
        trigger: BackgroundWakeTrigger<'_>,
    ) -> Result<Vec<BackgroundWakeDelivery>> {
        let now = now_ms();
        let output = crate::redact::redact(trigger.output);
        type ClaimedWakeRow = (i64, i64, String, String, i64, i64, i64, String, String);
        let rows: Vec<ClaimedWakeRow> = sqlx::query_as(
            r#"
            UPDATE background_wake_subscriptions
            SET fired_at_ms = ?, wake_reason = ?, wake_version = ?, wake_output = ?,
                wake_output_truncated = ?
            WHERE owner_runtime_id = ?
              AND target_kind = ? AND target_id = ? AND target_incarnation = ?
              AND fired_at_ms IS NULL
              AND (observed_version < ? OR ? = 'removed')
              AND (? != 'output_threshold' OR output_threshold IS NULL OR ? >= output_threshold)
            RETURNING id, requester_generation, target_id, target_incarnation,
                      observed_version, wake_version, wake_output_truncated,
                      wake_reason, COALESCE(wake_output, '')
            "#,
        )
        .bind(now)
        .bind(trigger.wake_reason)
        .bind(i64::try_from(trigger.wake_version).context("Background wake version overflow")?)
        .bind(output.as_ref())
        .bind(i64::from(trigger.output_truncated))
        .bind(owner_runtime_id)
        .bind(trigger.target_kind.as_db_str())
        .bind(trigger.target_id)
        .bind(trigger.target_incarnation)
        .bind(i64::try_from(trigger.wake_version).context("Background wake version overflow")?)
        .bind(trigger.wake_reason)
        .bind(trigger.wake_reason)
        .bind(
            i64::try_from(trigger.total_output_chars)
                .context("Background wake output length overflow")?,
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to claim background wakes")?;
        rows.into_iter()
            .map(
                |(
                    subscription_id,
                    requester_generation,
                    target_id,
                    target_incarnation,
                    observed_version,
                    wake_version,
                    truncated,
                    wake_reason,
                    output,
                )| {
                    Ok(BackgroundWakeDelivery {
                        wait: BackgroundWakeWait {
                            subscription_id,
                            requester_generation,
                        },
                        target_kind: trigger.target_kind,
                        target_id,
                        target_incarnation,
                        observed_version: u64::try_from(observed_version)
                            .context("Stored negative background wake version")?,
                        wake_reason,
                        wake_version: u64::try_from(wake_version)
                            .context("Stored negative background wake version")?,
                        output,
                        output_truncated: truncated != 0,
                    })
                },
            )
            .collect()
    }

    /// Claim a deadline once without touching the observed process.
    pub async fn fire_due_background_wakes(
        &self,
        owner_runtime_id: &str,
    ) -> Result<Vec<BackgroundWakeDelivery>> {
        let now = now_ms();
        let rows: Vec<(i64, i64, String, String, i64, String)> = sqlx::query_as(
            r#"
            UPDATE background_wake_subscriptions
            SET fired_at_ms = ?, wake_reason = 'deadline', wake_version = observed_version,
                wake_output = '', wake_output_truncated = 0
            WHERE owner_runtime_id = ? AND fired_at_ms IS NULL
              AND deadline_at_ms IS NOT NULL AND deadline_at_ms <= ?
            RETURNING id, requester_generation, target_id, target_incarnation, observed_version, target_kind
            "#,
        )
        .bind(now)
        .bind(owner_runtime_id)
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .context("Failed to claim due background wakes")?;
        rows.into_iter()
            .map(
                |(
                    subscription_id,
                    requester_generation,
                    target_id,
                    target_incarnation,
                    observed_version,
                    kind,
                )| {
                    let target_kind = match kind.as_str() {
                        "background_task" => BackgroundWakeTargetKind::BackgroundTask,
                        "terminal" => BackgroundWakeTargetKind::Terminal,
                        _ => anyhow::bail!("Unknown stored background wake target kind: {kind}"),
                    };
                    Ok(BackgroundWakeDelivery {
                        wait: BackgroundWakeWait {
                            subscription_id,
                            requester_generation,
                        },
                        target_kind,
                        target_id,
                        target_incarnation,
                        observed_version: u64::try_from(observed_version)
                            .context("Stored negative background wake version")?,
                        wake_reason: "deadline".to_string(),
                        wake_version: u64::try_from(observed_version)
                            .context("Stored negative background wake version")?,
                        output: String::new(),
                        output_truncated: false,
                    })
                },
            )
            .collect()
    }

    /// Cancels all undelivered waits for a requester generation.
    pub async fn cancel_background_wakes(
        &self,
        requester: SessionId,
        requester_generation: i64,
    ) -> Result<u64> {
        let result = sqlx::query(
            "UPDATE background_wake_subscriptions \
             SET fired_at_ms = ?, wake_reason = 'cancelled', wake_version = observed_version, \
                 wake_output = '', wake_output_truncated = 0 \
             WHERE requester_session_id = ? AND requester_generation = ? AND fired_at_ms IS NULL",
        )
        .bind(now_ms())
        .bind(requester.as_i64())
        .bind(requester_generation)
        .execute(&self.pool)
        .await
        .context("Failed to cancel background wakes")?;
        Ok(result.rows_affected())
    }

    /// Invalidates waits whose requester session is no longer live. This never
    /// touches waits owned by another live runtime sharing the database.
    pub async fn cancel_stale_background_wakes(&self) -> Result<u64> {
        let stale_before = now_ms().saturating_sub(PEER_LIVENESS_THRESHOLD_MS);
        let result = sqlx::query(
            "UPDATE background_wake_subscriptions \
             SET fired_at_ms = ?, wake_reason = 'owner_lost', wake_version = observed_version, \
                 wake_output = '', wake_output_truncated = 0 \
             WHERE fired_at_ms IS NULL AND requester_session_id IN ( \
               SELECT id FROM sessions \
               WHERE status != 'active' OR last_heartbeat_ms IS NULL OR last_heartbeat_ms < ? \
             )",
        )
        .bind(now_ms())
        .bind(stale_before)
        .execute(&self.pool)
        .await
        .context("Failed to cancel stale background wakes")?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::{BackgroundWakeRegistration, BackgroundWakeTargetKind, BackgroundWakeTrigger};
    use crate::storage::test_utils::TestStorage;

    fn registration<'a>(
        requester: crate::storage::SessionId,
        owner_runtime_id: &'a str,
        operation_key: &'a str,
        observed_version: u64,
        output_threshold: Option<usize>,
    ) -> BackgroundWakeRegistration<'a> {
        BackgroundWakeRegistration {
            requester,
            requester_generation: 7,
            owner_runtime_id,
            operation_key,
            target_kind: BackgroundWakeTargetKind::BackgroundTask,
            target_id: "bg-1",
            target_incarnation: "test-task",
            observed_version,
            output_threshold,
            deadline_at_ms: None,
        }
    }

    fn trigger<'a>(
        reason: &'a str,
        version: u64,
        output_chars: usize,
    ) -> BackgroundWakeTrigger<'a> {
        BackgroundWakeTrigger {
            target_kind: BackgroundWakeTargetKind::BackgroundTask,
            target_id: "bg-1",
            target_incarnation: "test-task",
            wake_reason: reason,
            wake_version: version,
            total_output_chars: output_chars,
            output: "bounded tail",
            output_truncated: true,
        }
    }

    #[tokio::test]
    async fn output_threshold_uses_total_output_and_claims_once() {
        let fixture = TestStorage::new().await;
        let requester = fixture.start_session().await;
        fixture
            .storage
            .register_background_wake(registration(
                requester,
                "test-runtime",
                "call-1",
                1,
                Some(40_000),
            ))
            .await
            .expect("subscription should register");

        let first = fixture
            .storage
            .fire_background_wakes("test-runtime", trigger("succeeded", 2, 40_000))
            .await
            .expect("wake should fire");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].wake_reason, "succeeded");
        assert!(first[0].output_truncated);

        let duplicate = fixture
            .storage
            .fire_background_wakes("test-runtime", trigger("succeeded", 2, 40_000))
            .await
            .expect("duplicate claim should succeed");
        assert!(duplicate.is_empty());
    }

    #[tokio::test]
    async fn removal_and_cancellation_claim_pending_waits() {
        let fixture = TestStorage::new().await;
        let requester = fixture.start_session().await;
        fixture
            .storage
            .register_background_wake(registration(
                requester,
                "test-runtime",
                "call-remove",
                4,
                None,
            ))
            .await
            .expect("subscription should register");
        let removed = fixture
            .storage
            .fire_background_wakes("test-runtime", trigger("removed", 4, 0))
            .await
            .expect("removal should fire");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].wake_reason, "removed");

        fixture
            .storage
            .register_background_wake(registration(
                requester,
                "test-runtime",
                "call-cancel",
                5,
                None,
            ))
            .await
            .expect("subscription should register");
        assert_eq!(
            fixture
                .storage
                .cancel_background_wakes(requester, 7)
                .await
                .expect("cancellation should succeed"),
            1
        );
        let fired = fixture
            .storage
            .fire_background_wakes("test-runtime", trigger("succeeded", 6, 0))
            .await
            .expect("post-cancel fire should succeed");
        assert!(fired.is_empty());
    }

    #[tokio::test]
    async fn runtime_owner_isolates_target_and_deadline_claims() {
        let fixture = TestStorage::new().await;
        let requester = fixture.start_session().await;
        fixture
            .storage
            .register_background_wake(registration(requester, "runtime-a", "call-a", 1, None))
            .await
            .expect("first subscription should register");
        fixture
            .storage
            .register_background_wake(registration(requester, "runtime-b", "call-b", 1, None))
            .await
            .expect("second subscription should register");

        let a = fixture
            .storage
            .fire_background_wakes("runtime-a", trigger("succeeded", 2, 0))
            .await
            .expect("runtime A claim should succeed");
        assert_eq!(a.len(), 1);

        let b = fixture
            .storage
            .fire_background_wakes("runtime-b", trigger("succeeded", 2, 0))
            .await
            .expect("runtime B claim should succeed");
        assert_eq!(b.len(), 1);
    }
}
