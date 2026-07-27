//! Peer presence for concurrent bonsai sessions in one project root
//! (inter-agent communication P1). A live process heartbeats its session row;
//! peers are the *other* live sessions of the same project. Liveness is
//! `status = 'active' AND last_heartbeat_ms >= now - threshold`, so a crashed
//! process ages out without a reaper and legacy rows (NULL heartbeat) read as
//! stale.

use super::*;

use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

/// How often a live process stamps `sessions.last_heartbeat_ms`.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);

/// A session whose heartbeat is older than this is not live (5 missed beats).
pub const PEER_LIVENESS_THRESHOLD_MS: i64 = 15_000;

/// Process-local "the agent is running a turn" flag, mirrored into the session
/// row by the heartbeat writer so peers can see working vs idle. The TUI event
/// loop stores into it each frame; `None` (no flag wired — headless) reports
/// idle.
pub type SharedBusyFlag = std::sync::Arc<std::sync::atomic::AtomicBool>;

/// Spawn the detached liveness-heartbeat task: stamps the active session's
/// `last_heartbeat_ms` (and its working/idle `busy` state) every
/// [`HEARTBEAT_INTERVAL`] so concurrent bonsai processes in the same project
/// root can tell a live peer from a crash leftover — and a free peer from a
/// busy one. Reads the shared id each tick (session rotation via `/clear` is
/// picked up automatically; `None` skips the tick). Runs for the life of the
/// process; the returned handle lets a caller abort it early.
pub fn spawn_heartbeat_writer(
    storage: Storage,
    active_session_id: crate::tool::SharedActiveSessionId,
    busy: SharedBusyFlag,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            ticker.tick().await;
            let session_id = *active_session_id.lock().await;
            let Some(session_id) = session_id else {
                continue;
            };
            let is_busy = busy.load(std::sync::atomic::Ordering::Relaxed);
            if let Err(err) = storage.record_session_heartbeat(session_id, is_busy).await {
                tracing::warn!(%err, "session heartbeat failed");
            }
        }
    })
}

/// Set the process-local active session and immediately stamp its heartbeat.
/// The periodic writer keeps it fresh afterward, but startup/session-rotation
/// must not leave a live `Active` row with a NULL heartbeat that a concurrent
/// process could promote as a crash leftover.
pub async fn activate_session_heartbeat(
    storage: &Storage,
    active_session_id: &crate::tool::SharedActiveSessionId,
    session_id: SessionId,
) -> Result<()> {
    {
        let mut active = active_session_id.lock().await;
        *active = Some(session_id);
    }
    storage.record_session_heartbeat(session_id, false).await
}

/// How long inter-agent messages are retained before the boot-time purge.
const MESSAGE_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Per-recipient message cap enforced by the boot-time purge.
const MESSAGE_CAP_PER_RECIPIENT: i64 = 200;
/// A consumer normally persists within 500 ms. Thirty seconds leaves ample
/// room for a slow SQLite writer while still making crash recovery prompt.
const PEER_DELIVERY_LEASE_MS: i64 = 30_000;
static NEXT_PEER_DELIVERY_LEASE: AtomicU64 = AtomicU64::new(1);

/// Kind of an inter-agent message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerMessageKind {
    /// A free-text message from one session to another.
    Text,
    /// Auto-generated "session #N finished its run" notice (wake-on-done).
    DoneNotice,
    /// The FYI a `wake_when_done` requester leaves for its target ("I am
    /// parked waiting for your run; no reply needed"). A dedicated kind so
    /// injection framing suppresses the reply pointer and the receiving event
    /// loop never arms an auto-wake for it — the whole point is zero peer
    /// turns.
    WakeRequest,
}

crate::impl_db_enum!(PeerMessageKind {
    Text => "text",
    DoneNotice => "done_notice",
    WakeRequest => "wake_request",
} else Text);

/// One inter-agent message row, as claimed by the receiving process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMessage {
    pub id: i64,
    pub from_session_id: SessionId,
    pub to_session_id: SessionId,
    pub kind: PeerMessageKind,
    pub body: String,
    pub hop_count: i64,
    pub created_at_ms: i64,
    /// The one-shot wait that produced a done notice. Ordinary messages and
    /// legacy rows have no subscription link.
    pub wake_subscription_id: Option<i64>,
}

/// Independent durable consumer of a peer message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerDeliveryConsumer {
    Ui,
    Agent,
}

/// Proof that one consumer currently owns a message's delivery lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerDeliveryReceipt {
    message_id: i64,
    consumer: PeerDeliveryConsumer,
    lease_token: String,
}

impl PeerDeliveryReceipt {
    pub(crate) fn message_id(&self) -> i64 {
        self.message_id
    }
}

/// A peer message paired with the lease receipt required for acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasedPeerMessage {
    pub message: PeerMessage,
    pub receipt: PeerDeliveryReceipt,
}

impl Deref for LeasedPeerMessage {
    type Target = PeerMessage;

    fn deref(&self) -> &Self::Target {
        &self.message
    }
}

/// Result of registering a one-shot peer wake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeSubscriptionOutcome {
    /// A new pending requester-to-target relationship was stored.
    Created,
    /// The same requester was already waiting for this target.
    AlreadyPending,
    /// The target is already parked waiting for the requester — subscribing
    /// would deadlock both sessions, so nothing was stored.
    ReversePending,
}

impl WakeSubscriptionOutcome {
    const fn operation_db_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::AlreadyPending => "already_pending",
            Self::ReversePending => "reverse_pending",
        }
    }

    fn from_operation_db_str(value: &str) -> Result<Self> {
        match value {
            "created" => Ok(Self::Created),
            "already_pending" => Ok(Self::AlreadyPending),
            "reverse_pending" => Ok(Self::ReversePending),
            other => anyhow::bail!("Invalid persisted peer wake outcome {other:?}"),
        }
    }
}

/// Result of registering a wake, including the durable identity that must
/// match the eventual done notice before it can bypass wake policy brakes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeSubscriptionRegistration {
    pub outcome: WakeSubscriptionOutcome,
    pub subscription_id: Option<i64>,
}

/// One idempotent peer-send attempt. All recipients are committed in the same
/// transaction and replays return the original committed rows.
#[derive(Debug)]
pub(crate) struct PeerSendOperation<'a> {
    pub project_path: &'a Path,
    pub from: SessionId,
    pub recipients: &'a [SessionId],
    pub audience: &'a str,
    pub kind: PeerMessageKind,
    pub body: &'a str,
    pub hop_count: i64,
    pub idempotency_key: &'a str,
}

/// Rows committed by an idempotent peer send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommittedPeerSend {
    pub messages: Vec<PeerMessage>,
    /// True only for the call that inserted the operation. Replays must not
    /// enqueue a second process-local transcript echo.
    pub newly_committed: bool,
}

/// One atomic wake registration and optional no-reply FYI.
#[derive(Debug)]
pub(crate) struct PeerWakeOperation<'a> {
    pub project_path: &'a Path,
    pub requester: SessionId,
    pub target: SessionId,
    pub note: &'a str,
    pub fyi_body: Option<&'a str>,
    pub hop_count: i64,
    pub idempotency_key: &'a str,
}

/// Durable result of a keyed wake operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommittedPeerWake {
    pub registration: WakeSubscriptionRegistration,
    pub fyi_message: Option<PeerMessage>,
    /// True only when this call created the operation record. A replay returns
    /// the committed outcome without duplicating its echo.
    pub newly_committed: bool,
}

type StoredPeerWakeOperation = (i64, i64, String, String, Option<i64>, Option<i64>);

/// Pending wake relationships for one session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WakeRelationships {
    /// Sessions this session is waiting for.
    pub waiting_on: Vec<SessionId>,
    /// Sessions waiting for this session's current run.
    pub waiters: Vec<SessionId>,
}

fn peer_message_from_row(row: sqlx::sqlite::SqliteRow) -> Result<PeerMessage> {
    let kind: String = row.try_get("kind")?;
    let body: String = row.try_get("body")?;
    Ok(PeerMessage {
        id: row.try_get("id")?,
        from_session_id: SessionId::from_raw(row.try_get("from_session_id")?),
        to_session_id: SessionId::from_raw(row.try_get("to_session_id")?),
        kind: PeerMessageKind::from_db_str(&kind),
        body: crate::redact::redact(&body).into_owned(),
        hop_count: row.try_get("hop_count")?,
        created_at_ms: row.try_get("created_at_ms")?,
        wake_subscription_id: row.try_get("wake_subscription_id")?,
    })
}

const PEER_MESSAGE_RETURNING: &str = "RETURNING id, from_session_id, to_session_id, kind, body, hop_count, created_at_ms, wake_subscription_id";

fn next_peer_delivery_lease_token(consumer: PeerDeliveryConsumer) -> String {
    let sequence = NEXT_PEER_DELIVERY_LEASE.fetch_add(1, Ordering::Relaxed);
    let consumer = match consumer {
        PeerDeliveryConsumer::Ui => "ui",
        PeerDeliveryConsumer::Agent => "agent",
    };
    format!("{consumer}-{}-{}-{sequence}", std::process::id(), now_ms())
}

/// A live peer session in the same project root, as shown to the model
/// (`peers list`, the volatile peer-status block) and the `/peers` view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSessionSummary {
    pub id: SessionId,
    /// Session title (`set_session_title`), empty when never set.
    pub summary: String,
    pub last_heartbeat_ms: i64,
    /// Content recency — when the session last changed anything.
    pub updated_at_ms: i64,
    /// Whether the peer's agent was running a turn at its last heartbeat —
    /// working (`true`) vs idle (`false`). Coordination signal: e.g. whether a
    /// peer that owns files you want to build on has stopped editing yet.
    pub working: bool,
    /// When the peer's current run started (`NULL` between runs) — lets the
    /// `wake_when_done` status ack say how long the run has been going.
    pub run_started_at_ms: Option<i64>,
}

impl Storage {
    /// Test-only: abort inserts addressed to `recipient`. SQLite triggers let
    /// peer-bus tests fail a chosen fan-out position without a production
    /// failpoint or process-global test state.
    #[cfg(test)]
    pub(crate) async fn fail_peer_message_recipient_for_tests(
        &self,
        recipient: SessionId,
    ) -> Result<()> {
        self.prepare_peer_message_failure_for_tests().await?;
        sqlx::query(
            "INSERT INTO peer_message_test_failures (recipient_id, kind)
             VALUES (?, NULL)",
        )
        .bind(recipient.as_i64())
        .execute(&self.pool)
        .await
        .context("Failed to select peer-recipient failure point")?;
        Ok(())
    }

    /// Test-only: abort wake-request FYI inserts after subscription insertion.
    #[cfg(test)]
    pub(crate) async fn fail_wake_request_insert_for_tests(&self) -> Result<()> {
        self.prepare_peer_message_failure_for_tests().await?;
        sqlx::query(
            "INSERT INTO peer_message_test_failures (recipient_id, kind)
             VALUES (NULL, 'wake_request')",
        )
        .execute(&self.pool)
        .await
        .context("Failed to select wake-request failure point")?;
        Ok(())
    }

    /// Remove the peer-message failure trigger installed by a test.
    #[cfg(test)]
    pub(crate) async fn clear_peer_message_failure_for_tests(&self) -> Result<()> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS peer_message_test_failures (
               recipient_id INTEGER,
               kind TEXT
             )",
        )
        .execute(&self.pool)
        .await
        .context("Failed to create peer-message test failure state")?;
        sqlx::query("DELETE FROM peer_message_test_failures")
            .execute(&self.pool)
            .await
            .context("Failed to clear peer-message test trigger")?;
        Ok(())
    }

    #[cfg(test)]
    async fn prepare_peer_message_failure_for_tests(&self) -> Result<()> {
        self.clear_peer_message_failure_for_tests().await?;
        sqlx::query(
            "CREATE TRIGGER IF NOT EXISTS fail_peer_message_insert
             BEFORE INSERT ON agent_messages
             WHEN EXISTS (
               SELECT 1 FROM peer_message_test_failures
               WHERE (recipient_id IS NULL OR recipient_id = NEW.to_session_id)
                 AND (kind IS NULL OR kind = NEW.kind)
             )
             BEGIN
               SELECT RAISE(ABORT, 'injected peer-message failure');
             END",
        )
        .execute(&self.pool)
        .await
        .context("Failed to install peer-message test trigger")?;
        Ok(())
    }

    /// Test-only: pin a session's heartbeat to an arbitrary timestamp so tests
    /// can simulate stale (crashed) and fresh (live peer) processes.
    #[cfg(test)]
    pub(crate) async fn set_session_heartbeat_for_tests(
        &self,
        session_id: SessionId,
        at_ms: i64,
    ) -> Result<()> {
        sqlx::query("UPDATE sessions SET last_heartbeat_ms = ? WHERE id = ?")
            .bind(at_ms)
            .bind(session_id.as_i64())
            .execute(&self.pool)
            .await
            .context("Failed to pin test heartbeat")?;
        Ok(())
    }

    /// Test-only: read a session's raw heartbeat column.
    #[cfg(test)]
    pub(crate) async fn session_heartbeat_for_tests(
        &self,
        session_id: SessionId,
    ) -> Result<Option<i64>> {
        sqlx::query_scalar("SELECT last_heartbeat_ms FROM sessions WHERE id = ?")
            .bind(session_id.as_i64())
            .fetch_one(&self.pool)
            .await
            .context("Failed to read test heartbeat")
    }

    /// The other live sessions for `project_path`: `active`, fresh heartbeat,
    /// not `self_id`. Ordered by content recency (most recently working
    /// first).
    pub async fn list_live_peer_sessions(
        &self,
        project_path: &Path,
        self_id: SessionId,
    ) -> Result<Vec<PeerSessionSummary>> {
        let project_path = canonical_project_path(project_path);
        let live_floor = now_ms().saturating_sub(PEER_LIVENESS_THRESHOLD_MS);
        let rows = sqlx::query(
            r#"
            SELECT sessions.id, sessions.summary,
                   sessions.last_heartbeat_ms, sessions.updated_at_ms, sessions.busy,
                   sessions.active_run_started_at_ms
            FROM sessions
            JOIN projects ON projects.id = sessions.project_id
            WHERE projects.path = ?
              AND sessions.status = ?
              AND sessions.id != ?
              AND sessions.last_heartbeat_ms >= ?
            ORDER BY sessions.updated_at_ms DESC
            "#,
        )
        .bind(&project_path)
        .bind(SessionStatus::Active.as_db_str())
        .bind(self_id.as_i64())
        .bind(live_floor)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list live peer sessions")?;

        rows.into_iter()
            .map(|row| {
                Ok(PeerSessionSummary {
                    id: SessionId::from_raw(row.try_get("id")?),
                    summary: row.try_get("summary")?,
                    last_heartbeat_ms: row.try_get("last_heartbeat_ms")?,
                    updated_at_ms: row.try_get("updated_at_ms")?,
                    working: row.try_get("busy")?,
                    run_started_at_ms: row.try_get("active_run_started_at_ms")?,
                })
            })
            .collect()
    }

    /// Insert one inter-agent message addressed to `to`. Broadcast is fan-out
    /// at the call site (one insert per live recipient), keeping the delivery
    /// flags single-recipient.
    #[cfg(test)]
    pub async fn send_peer_message(
        &self,
        project_path: &Path,
        from: SessionId,
        to: SessionId,
        kind: PeerMessageKind,
        body: &str,
        hop_count: i64,
    ) -> Result<()> {
        let body = crate::redact::redact(body);
        let project_id = self.ensure_project(project_path).await?;
        sqlx::query(
            r#"
            INSERT INTO agent_messages (
              project_id, from_session_id, to_session_id, kind, body,
              hop_count, created_at_ms
            )
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(project_id)
        .bind(from.as_i64())
        .bind(to.as_i64())
        .bind(kind.as_db_str())
        .bind(body.as_ref())
        .bind(hop_count)
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to send peer message from {from} to {to}"))?;
        Ok(())
    }

    /// Return an already-committed keyed send before the caller re-evaluates
    /// volatile liveness. This keeps a retry successful even if a target has
    /// exited or a broadcast's live recipient set has changed since commit.
    pub(crate) async fn replay_peer_send(
        &self,
        operation: &PeerSendOperation<'_>,
    ) -> Result<Option<CommittedPeerSend>> {
        let body = crate::redact::redact(operation.body);
        let project_id = self.ensure_project(operation.project_path).await?;
        let existing: Option<(i64, i64, String, String, String)> = sqlx::query_as(
            r#"
            SELECT id, project_id, audience, kind, body
            FROM peer_send_operations
            WHERE from_session_id = ? AND idempotency_key = ?
            "#,
        )
        .bind(operation.from.as_i64())
        .bind(operation.idempotency_key)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to look up peer send replay")?;
        let Some((operation_id, stored_project_id, audience, kind, stored_body)) = existing else {
            return Ok(None);
        };
        if stored_project_id != project_id
            || audience != operation.audience
            || kind != operation.kind.as_db_str()
            || stored_body != body.as_ref()
        {
            anyhow::bail!("Peer send idempotency key was reused with different arguments");
        }
        let rows = sqlx::query(
            r#"
            SELECT id, from_session_id, to_session_id, kind, body,
                   hop_count, created_at_ms, wake_subscription_id
            FROM agent_messages
            WHERE send_operation_id = ?
            ORDER BY id ASC
            "#,
        )
        .bind(operation_id)
        .fetch_all(&self.pool)
        .await
        .context("Failed to load replayed peer send recipients")?;
        let messages = rows
            .into_iter()
            .map(peer_message_from_row)
            .collect::<Result<Vec<_>>>()?;
        if messages.is_empty() {
            anyhow::bail!("Committed peer send operation has no recipient rows");
        }
        Ok(Some(CommittedPeerSend {
            messages,
            newly_committed: false,
        }))
    }

    /// Commit every recipient of a peer send atomically. The operation key is
    /// scoped to the sending session; replaying it returns the original rows
    /// even if the set of currently-live broadcast recipients has changed.
    pub(crate) async fn commit_peer_send(
        &self,
        operation: PeerSendOperation<'_>,
    ) -> Result<CommittedPeerSend> {
        if operation.recipients.is_empty() {
            anyhow::bail!("A peer send operation requires at least one recipient");
        }
        if operation.idempotency_key.is_empty() {
            anyhow::bail!("A peer send operation requires an idempotency key");
        }

        let body = crate::redact::redact(operation.body);
        let project_id = self.ensure_project(operation.project_path).await?;
        let now = now_ms();
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin atomic peer send")?;

        // This is deliberately the transaction's first statement. It obtains
        // SQLite's write lock before recipient fan-out, so concurrent retries
        // serialize on the unique operation identity.
        let created_operation_id: Option<i64> = sqlx::query_scalar(
            r#"
            INSERT INTO peer_send_operations (
              project_id, from_session_id, idempotency_key, audience, kind,
              body, hop_count, created_at_ms
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(from_session_id, idempotency_key) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(project_id)
        .bind(operation.from.as_i64())
        .bind(operation.idempotency_key)
        .bind(operation.audience)
        .bind(operation.kind.as_db_str())
        .bind(body.as_ref())
        .bind(operation.hop_count)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .with_context(|| {
            format!(
                "Failed to reserve peer send operation for session {}",
                operation.from
            )
        })?;

        let (operation_id, newly_committed) = if let Some(operation_id) = created_operation_id {
            (operation_id, true)
        } else {
            let existing: (i64, i64, String, String, String) = sqlx::query_as(
                r#"
                SELECT id, project_id, audience, kind, body
                FROM peer_send_operations
                WHERE from_session_id = ? AND idempotency_key = ?
                "#,
            )
            .bind(operation.from.as_i64())
            .bind(operation.idempotency_key)
            .fetch_one(&mut *tx)
            .await
            .context("Committed peer send operation disappeared during replay")?;
            let (operation_id, stored_project_id, audience, kind, stored_body) = existing;
            if stored_project_id != project_id
                || audience != operation.audience
                || kind != operation.kind.as_db_str()
                || stored_body != body.as_ref()
            {
                anyhow::bail!("Peer send idempotency key was reused with different arguments");
            }
            (operation_id, false)
        };

        let messages = if newly_committed {
            let mut messages = Vec::with_capacity(operation.recipients.len());
            for recipient in operation.recipients {
                let row = sqlx::query(
                    r#"
                    INSERT INTO agent_messages (
                      project_id, from_session_id, to_session_id, kind, body,
                      hop_count, created_at_ms, send_operation_id
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    RETURNING id, from_session_id, to_session_id, kind, body,
                              hop_count, created_at_ms, wake_subscription_id
                    "#,
                )
                .bind(project_id)
                .bind(operation.from.as_i64())
                .bind(recipient.as_i64())
                .bind(operation.kind.as_db_str())
                .bind(body.as_ref())
                .bind(operation.hop_count)
                .bind(now)
                .bind(operation_id)
                .fetch_one(&mut *tx)
                .await
                .with_context(|| {
                    format!(
                        "Failed to commit peer send from {} to {recipient}",
                        operation.from
                    )
                })?;
                messages.push(peer_message_from_row(row)?);
            }
            messages
        } else {
            let rows = sqlx::query(
                r#"
                SELECT id, from_session_id, to_session_id, kind, body,
                       hop_count, created_at_ms, wake_subscription_id
                FROM agent_messages
                WHERE send_operation_id = ?
                ORDER BY id ASC
                "#,
            )
            .bind(operation_id)
            .fetch_all(&mut *tx)
            .await
            .context("Failed to load committed peer send recipients")?;
            rows.into_iter()
                .map(peer_message_from_row)
                .collect::<Result<Vec<_>>>()?
        };

        if messages.is_empty() {
            anyhow::bail!("Committed peer send operation has no recipient rows");
        }
        tx.commit()
            .await
            .context("Failed to commit atomic peer send")?;
        Ok(CommittedPeerSend {
            messages,
            newly_committed,
        })
    }

    /// Lease messages for `to` that have not been durably rendered by the UI.
    /// Unacknowledged leases become claimable again after expiry.
    pub async fn claim_ui_undelivered_messages(
        &self,
        to: SessionId,
    ) -> Result<Vec<LeasedPeerMessage>> {
        self.lease_undelivered_messages(to, PeerDeliveryConsumer::Ui)
            .await
    }

    /// Lease messages for `to` that have not been durably persisted in agent
    /// context. This delivery lane is independent from the UI lane.
    pub async fn claim_agent_undelivered_messages(
        &self,
        to: SessionId,
    ) -> Result<Vec<LeasedPeerMessage>> {
        self.lease_undelivered_messages(to, PeerDeliveryConsumer::Agent)
            .await
    }

    async fn lease_undelivered_messages(
        &self,
        to: SessionId,
        consumer: PeerDeliveryConsumer,
    ) -> Result<Vec<LeasedPeerMessage>> {
        let now = now_ms();
        let expires_at = now.saturating_add(PEER_DELIVERY_LEASE_MS);
        let lease_token = next_peer_delivery_lease_token(consumer);
        let query = match consumer {
            PeerDeliveryConsumer::Ui => format!(
                r#"
                UPDATE agent_messages
                SET ui_lease_token = ?, ui_lease_expires_at_ms = ?
                WHERE to_session_id = ?
                  AND delivered_ui_at_ms IS NULL
                  AND (ui_lease_expires_at_ms IS NULL OR ui_lease_expires_at_ms <= ?)
                {PEER_MESSAGE_RETURNING}
                "#
            ),
            PeerDeliveryConsumer::Agent => format!(
                r#"
                UPDATE agent_messages
                SET agent_lease_token = ?, agent_lease_expires_at_ms = ?
                WHERE to_session_id = ?
                  AND delivered_agent_at_ms IS NULL
                  AND (agent_lease_expires_at_ms IS NULL OR agent_lease_expires_at_ms <= ?)
                {PEER_MESSAGE_RETURNING}
                "#
            ),
        };
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(&lease_token)
            .bind(expires_at)
            .bind(to.as_i64())
            .bind(now)
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("Failed to lease {consumer:?} peer messages"))?;
        let mut messages: Vec<PeerMessage> = rows
            .into_iter()
            .map(peer_message_from_row)
            .collect::<Result<_>>()?;
        messages.sort_by_key(|message| message.id);
        Ok(messages
            .into_iter()
            .map(|message| LeasedPeerMessage {
                receipt: PeerDeliveryReceipt {
                    message_id: message.id,
                    consumer,
                    lease_token: lease_token.clone(),
                },
                message,
            })
            .collect())
    }

    /// Acknowledge UI leases without another durable write. Production callers
    /// normally use [`Self::acknowledge_peer_deliveries_in_tx`] so transcript
    /// persistence and acknowledgement commit atomically.
    #[cfg(test)]
    pub(crate) async fn acknowledge_ui_peer_deliveries(
        &self,
        to: SessionId,
        receipts: &[PeerDeliveryReceipt],
    ) -> Result<()> {
        self.acknowledge_peer_deliveries(to, PeerDeliveryConsumer::Ui, receipts)
            .await
    }

    /// Acknowledge agent-context leases without another durable write.
    #[cfg(test)]
    pub(crate) async fn acknowledge_agent_peer_deliveries(
        &self,
        to: SessionId,
        receipts: &[PeerDeliveryReceipt],
    ) -> Result<()> {
        self.acknowledge_peer_deliveries(to, PeerDeliveryConsumer::Agent, receipts)
            .await
    }

    #[cfg(test)]
    async fn acknowledge_peer_deliveries(
        &self,
        to: SessionId,
        consumer: PeerDeliveryConsumer,
        receipts: &[PeerDeliveryReceipt],
    ) -> Result<()> {
        self.with_session_snapshot_tx("peer delivery acknowledgement", async move |tx, now| {
            self.acknowledge_peer_deliveries_in_tx(tx, to, consumer, receipts, now)
                .await
        })
        .await
    }

    pub(crate) async fn acknowledge_peer_deliveries_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        to: SessionId,
        consumer: PeerDeliveryConsumer,
        receipts: &[PeerDeliveryReceipt],
        now: i64,
    ) -> Result<()> {
        for receipt in receipts {
            if receipt.consumer != consumer {
                anyhow::bail!(
                    "Peer delivery receipt {} belongs to {:?}, not {consumer:?}",
                    receipt.message_id,
                    receipt.consumer
                );
            }
            let result = match consumer {
                PeerDeliveryConsumer::Ui => {
                    sqlx::query(
                        r#"
                    UPDATE agent_messages
                    SET delivered_ui_at_ms = ?, ui_lease_token = NULL,
                        ui_lease_expires_at_ms = NULL
                    WHERE id = ? AND to_session_id = ?
                      AND delivered_ui_at_ms IS NULL AND ui_lease_token = ?
                    "#,
                    )
                    .bind(now)
                    .bind(receipt.message_id)
                    .bind(to.as_i64())
                    .bind(&receipt.lease_token)
                    .execute(&mut **tx)
                    .await?
                }
                PeerDeliveryConsumer::Agent => {
                    sqlx::query(
                        r#"
                    UPDATE agent_messages
                    SET delivered_agent_at_ms = ?, agent_lease_token = NULL,
                        agent_lease_expires_at_ms = NULL
                    WHERE id = ? AND to_session_id = ?
                      AND delivered_agent_at_ms IS NULL AND agent_lease_token = ?
                    "#,
                    )
                    .bind(now)
                    .bind(receipt.message_id)
                    .bind(to.as_i64())
                    .bind(&receipt.lease_token)
                    .execute(&mut **tx)
                    .await?
                }
            };
            if result.rows_affected() == 1 {
                continue;
            }
            let acknowledged: i64 = match consumer {
                PeerDeliveryConsumer::Ui => sqlx::query_scalar(
                    "SELECT delivered_ui_at_ms IS NOT NULL FROM agent_messages \
                     WHERE id = ? AND to_session_id = ?",
                )
                .bind(receipt.message_id)
                .bind(to.as_i64())
                .fetch_optional(&mut **tx)
                .await?
                .unwrap_or_default(),
                PeerDeliveryConsumer::Agent => sqlx::query_scalar(
                    "SELECT delivered_agent_at_ms IS NOT NULL FROM agent_messages \
                     WHERE id = ? AND to_session_id = ?",
                )
                .bind(receipt.message_id)
                .bind(to.as_i64())
                .fetch_optional(&mut **tx)
                .await?
                .unwrap_or_default(),
            };
            if acknowledged == 0 {
                anyhow::bail!(
                    "Peer message {} is no longer leased by this {consumer:?} consumer",
                    receipt.message_id
                );
            }
        }
        Ok(())
    }

    /// Messages still awaiting agent injection for `to` — the actionable-inbox
    /// gate a peer wake re-checks before starting a turn.
    pub async fn pending_agent_message_count(&self, to: SessionId) -> Result<i64> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_messages
             WHERE to_session_id = ? AND delivered_agent_at_ms IS NULL",
        )
        .bind(to.as_i64())
        .fetch_one(&self.pool)
        .await
        .context("Failed to count pending peer messages")
    }

    /// Return the still-unacknowledged agent messages among `message_ids`.
    /// Used by the wake gate to derive authority from current durable rows.
    pub async fn pending_agent_messages_by_id(
        &self,
        to: SessionId,
        message_ids: &[i64],
    ) -> Result<Vec<PeerMessage>> {
        if message_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<Sqlite>::new(
            "SELECT id, from_session_id, to_session_id, kind, body, hop_count, \
             created_at_ms, wake_subscription_id FROM agent_messages \
             WHERE to_session_id = ",
        );
        query.push_bind(to.as_i64());
        query.push(" AND delivered_agent_at_ms IS NULL AND id IN (");
        {
            let mut separated = query.separated(", ");
            for message_id in message_ids {
                separated.push_bind(message_id);
            }
        }
        query.push(") ORDER BY id");
        query
            .build()
            .fetch_all(&self.pool)
            .await
            .context("Failed to load pending peer wake messages")?
            .into_iter()
            .map(peer_message_from_row)
            .collect()
    }

    /// Upsert the files `session_id` changed this flush (path-keyed, so
    /// cross-process readers never race a delete window).
    pub async fn record_session_file_changes(
        &self,
        session_id: SessionId,
        paths: &[String],
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let now = now_ms();
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin file-change upsert")?;
        for path in paths {
            sqlx::query(
                r#"
                INSERT INTO session_file_changes (session_id, path, last_changed_at_ms)
                VALUES (?, ?, ?)
                ON CONFLICT(session_id, path) DO UPDATE SET
                  last_changed_at_ms = excluded.last_changed_at_ms
                "#,
            )
            .bind(session_id.as_i64())
            .bind(path)
            .bind(now)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("Failed to record file change for {path}"))?;
        }
        tx.commit().await.context("Failed to commit file changes")
    }

    /// The most recently changed files for a session, newest first.
    pub async fn recent_session_file_changes(
        &self,
        session_id: SessionId,
        limit: i64,
    ) -> Result<Vec<String>> {
        sqlx::query_scalar(
            r#"
            SELECT path FROM session_file_changes
            WHERE session_id = ?
            ORDER BY last_changed_at_ms DESC
            LIMIT ?
            "#,
        )
        .bind(session_id.as_i64())
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .context("Failed to list session file changes")
    }

    /// File paths recorded by sessions other than `session_id` in this project
    /// at or after `since_ms`. Used to avoid attributing foreground-Bash
    /// snapshot changes to this session when another Bonsai session recorded the
    /// same path during its self-review window.
    pub async fn other_session_file_changes_since(
        &self,
        project_path: &Path,
        session_id: SessionId,
        since_ms: i64,
    ) -> Result<Vec<String>> {
        let project_id = self.ensure_project(project_path).await?;
        sqlx::query_scalar(
            r#"
            SELECT DISTINCT session_file_changes.path
            FROM session_file_changes
            INNER JOIN sessions ON sessions.id = session_file_changes.session_id
            WHERE sessions.project_id = ?
              AND sessions.id != ?
              AND session_file_changes.last_changed_at_ms >= ?
            ORDER BY session_file_changes.path ASC
            "#,
        )
        .bind(project_id)
        .bind(session_id.as_i64())
        .bind(since_ms)
        .fetch_all(&self.pool)
        .await
        .context("Failed to list other-session file changes since timestamp")
    }

    /// Subscribe `requester` to a one-shot wake when `target`'s current run
    /// finishes (peers P3). Refuses when the reverse pair is already pending —
    /// two sessions parked on each other would deadlock (neither run ends, so
    /// neither wake fires). The reverse guard lives inside the INSERT itself:
    /// a single SQLite write statement, so two concurrent mutual subscribes
    /// serialize and exactly one is refused.
    #[cfg(test)]
    pub async fn add_wake_subscription(
        &self,
        project_path: &Path,
        requester: SessionId,
        target: SessionId,
        note: &str,
        hop_count: i64,
    ) -> Result<WakeSubscriptionRegistration> {
        let note = crate::redact::redact(note);
        let project_id = self.ensure_project(project_path).await?;
        let created_id: Option<i64> = sqlx::query_scalar(
            r#"
            INSERT INTO peer_wake_subscriptions (
              project_id, requester_session_id, target_session_id, note,
              hop_count, created_at_ms
            )
            SELECT ?, ?, ?, ?, ?, ?
            WHERE NOT EXISTS (
              SELECT 1 FROM peer_wake_subscriptions
              WHERE requester_session_id = ?
                AND target_session_id = ?
                AND fired_at_ms IS NULL
            )
            ON CONFLICT(requester_session_id, target_session_id)
              WHERE fired_at_ms IS NULL
            DO NOTHING
            RETURNING id
            "#,
        )
        .bind(project_id)
        .bind(requester.as_i64())
        .bind(target.as_i64())
        .bind(note.as_ref())
        .bind(hop_count)
        .bind(now_ms())
        .bind(target.as_i64())
        .bind(requester.as_i64())
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("Failed to add wake subscription {requester} -> {target}"))?;
        if let Some(subscription_id) = created_id {
            return Ok(WakeSubscriptionRegistration {
                outcome: WakeSubscriptionOutcome::Created,
                subscription_id: Some(subscription_id),
            });
        }
        // Zero rows: either the reverse pair blocked the insert or our own
        // pair already exists. The follow-up read only picks the message text,
        // so its race window is benign.
        let reverse_pending: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
              SELECT 1 FROM peer_wake_subscriptions
              WHERE requester_session_id = ?
                AND target_session_id = ?
                AND fired_at_ms IS NULL
            )
            "#,
        )
        .bind(target.as_i64())
        .bind(requester.as_i64())
        .fetch_one(&self.pool)
        .await
        .context("Failed to check reverse wake subscription")?;
        if reverse_pending {
            return Ok(WakeSubscriptionRegistration {
                outcome: WakeSubscriptionOutcome::ReversePending,
                subscription_id: None,
            });
        }
        let subscription_id = sqlx::query_scalar(
            r#"
            SELECT id FROM peer_wake_subscriptions
            WHERE requester_session_id = ? AND target_session_id = ?
              AND fired_at_ms IS NULL
            "#,
        )
        .bind(requester.as_i64())
        .bind(target.as_i64())
        .fetch_one(&self.pool)
        .await
        .context("Pending wake subscription disappeared during registration")?;
        Ok(WakeSubscriptionRegistration {
            outcome: WakeSubscriptionOutcome::AlreadyPending,
            subscription_id: Some(subscription_id),
        })
    }

    /// Return a previously committed wake outcome before the caller evaluates
    /// volatile peer liveness. A tool retry must still report its committed
    /// wait after the target finishes or exits.
    pub(crate) async fn replay_peer_wake(
        &self,
        operation: &PeerWakeOperation<'_>,
    ) -> Result<Option<CommittedPeerWake>> {
        let note = crate::redact::redact(operation.note);
        let fyi_body = operation.fyi_body.map(crate::redact::redact);
        let project_id = self.ensure_project(operation.project_path).await?;
        let existing: Option<StoredPeerWakeOperation> = sqlx::query_as(
            r#"
                SELECT project_id, target_session_id, note, outcome,
                       subscription_id, fyi_message_id
                FROM peer_wake_operations
                WHERE requester_session_id = ? AND idempotency_key = ?
                "#,
        )
        .bind(operation.requester.as_i64())
        .bind(operation.idempotency_key)
        .fetch_optional(&self.pool)
        .await
        .context("Failed to look up peer wake replay")?;
        let Some((
            stored_project_id,
            target_session_id,
            stored_note,
            outcome,
            subscription_id,
            fyi_message_id,
        )) = existing
        else {
            return Ok(None);
        };
        if stored_project_id != project_id
            || target_session_id != operation.target.as_i64()
            || stored_note != note.as_ref()
        {
            anyhow::bail!("Peer wake idempotency key was reused with different arguments");
        }
        let outcome = WakeSubscriptionOutcome::from_operation_db_str(&outcome)?;
        let fyi_message = if let Some(message_id) = fyi_message_id {
            let row = sqlx::query(
                r#"
                SELECT id, from_session_id, to_session_id, kind, body,
                       hop_count, created_at_ms, wake_subscription_id
                FROM agent_messages
                WHERE id = ?
                "#,
            )
            .bind(message_id)
            .fetch_one(&self.pool)
            .await
            .context("Committed peer wake FYI disappeared during replay")?;
            Some(peer_message_from_row(row)?)
        } else {
            None
        };
        if outcome == WakeSubscriptionOutcome::Created {
            match (fyi_body.as_ref(), fyi_message.as_ref()) {
                (Some(expected), Some(message)) if message.body == expected.as_ref() => {}
                (None, None) => {}
                _ => anyhow::bail!("Committed peer wake operation has an inconsistent FYI"),
            }
        } else if fyi_message.is_some() {
            anyhow::bail!("A non-created peer wake operation unexpectedly has an FYI");
        }
        Ok(Some(CommittedPeerWake {
            registration: WakeSubscriptionRegistration {
                outcome,
                subscription_id,
            },
            fyi_message,
            newly_committed: false,
        }))
    }

    /// Register a wake and its optional no-reply FYI in one transaction. The
    /// operation record makes every outcome replayable, including joining an
    /// existing wait or refusing a reverse-pending deadlock.
    pub(crate) async fn commit_peer_wake(
        &self,
        operation: PeerWakeOperation<'_>,
    ) -> Result<CommittedPeerWake> {
        if operation.idempotency_key.is_empty() {
            anyhow::bail!("A peer wake operation requires an idempotency key");
        }

        let note = crate::redact::redact(operation.note);
        let fyi_body = operation.fyi_body.map(crate::redact::redact);
        let project_id = self.ensure_project(operation.project_path).await?;
        let now = now_ms();
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin atomic peer wake registration")?;

        // Reserve the tool-call identity first so concurrent retries serialize
        // before inspecting or changing the pending subscription graph.
        let created_operation_id: Option<i64> = sqlx::query_scalar(
            r#"
            INSERT INTO peer_wake_operations (
              project_id, requester_session_id, idempotency_key,
              target_session_id, note, hop_count, created_at_ms
            )
            VALUES (?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(requester_session_id, idempotency_key) DO NOTHING
            RETURNING id
            "#,
        )
        .bind(project_id)
        .bind(operation.requester.as_i64())
        .bind(operation.idempotency_key)
        .bind(operation.target.as_i64())
        .bind(note.as_ref())
        .bind(operation.hop_count)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .with_context(|| {
            format!(
                "Failed to reserve wake operation {} -> {}",
                operation.requester, operation.target
            )
        })?;

        let Some(operation_id) = created_operation_id else {
            let existing: StoredPeerWakeOperation = sqlx::query_as(
                r#"
                    SELECT project_id, target_session_id, note, outcome,
                           subscription_id, fyi_message_id
                    FROM peer_wake_operations
                    WHERE requester_session_id = ? AND idempotency_key = ?
                    "#,
            )
            .bind(operation.requester.as_i64())
            .bind(operation.idempotency_key)
            .fetch_one(&mut *tx)
            .await
            .context("Committed peer wake operation disappeared during replay")?;
            let (
                stored_project_id,
                target_session_id,
                stored_note,
                outcome,
                subscription_id,
                fyi_message_id,
            ) = existing;
            if stored_project_id != project_id
                || target_session_id != operation.target.as_i64()
                || stored_note != note.as_ref()
            {
                anyhow::bail!("Peer wake idempotency key was reused with different arguments");
            }
            let outcome = WakeSubscriptionOutcome::from_operation_db_str(&outcome)?;
            let fyi_message = if let Some(message_id) = fyi_message_id {
                let row = sqlx::query(
                    r#"
                    SELECT id, from_session_id, to_session_id, kind, body,
                           hop_count, created_at_ms, wake_subscription_id
                    FROM agent_messages
                    WHERE id = ?
                    "#,
                )
                .bind(message_id)
                .fetch_one(&mut *tx)
                .await
                .context("Committed peer wake FYI disappeared during replay")?;
                Some(peer_message_from_row(row)?)
            } else {
                None
            };
            if outcome == WakeSubscriptionOutcome::Created {
                match (fyi_body.as_ref(), fyi_message.as_ref()) {
                    (Some(expected), Some(message)) if message.body == expected.as_ref() => {}
                    (None, None) => {}
                    _ => anyhow::bail!("Committed peer wake operation has an inconsistent FYI"),
                }
            } else if fyi_message.is_some() {
                anyhow::bail!("A non-created peer wake operation unexpectedly has an FYI");
            }
            tx.commit()
                .await
                .context("Failed to finish peer wake replay")?;
            return Ok(CommittedPeerWake {
                registration: WakeSubscriptionRegistration {
                    outcome,
                    subscription_id,
                },
                fyi_message,
                newly_committed: false,
            });
        };

        let created_subscription_id: Option<i64> = sqlx::query_scalar(
            r#"
            INSERT INTO peer_wake_subscriptions (
              project_id, requester_session_id, target_session_id, note,
              hop_count, created_at_ms
            )
            SELECT ?, ?, ?, ?, ?, ?
            WHERE NOT EXISTS (
              SELECT 1 FROM peer_wake_subscriptions
              WHERE requester_session_id = ?
                AND target_session_id = ?
                AND fired_at_ms IS NULL
            )
            ON CONFLICT DO NOTHING
            RETURNING id
            "#,
        )
        .bind(project_id)
        .bind(operation.requester.as_i64())
        .bind(operation.target.as_i64())
        .bind(note.as_ref())
        .bind(operation.hop_count)
        .bind(now)
        .bind(operation.target.as_i64())
        .bind(operation.requester.as_i64())
        .fetch_optional(&mut *tx)
        .await
        .with_context(|| {
            format!(
                "Failed to register atomic wake {} -> {}",
                operation.requester, operation.target
            )
        })?;

        let registration = if let Some(subscription_id) = created_subscription_id {
            WakeSubscriptionRegistration {
                outcome: WakeSubscriptionOutcome::Created,
                subscription_id: Some(subscription_id),
            }
        } else {
            let reverse_pending: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS (
                  SELECT 1 FROM peer_wake_subscriptions
                  WHERE requester_session_id = ?
                    AND target_session_id = ?
                    AND fired_at_ms IS NULL
                )
                "#,
            )
            .bind(operation.target.as_i64())
            .bind(operation.requester.as_i64())
            .fetch_one(&mut *tx)
            .await
            .context("Failed to check reverse wake subscription")?;
            if reverse_pending {
                WakeSubscriptionRegistration {
                    outcome: WakeSubscriptionOutcome::ReversePending,
                    subscription_id: None,
                }
            } else {
                let subscription_id = sqlx::query_scalar(
                    r#"
                    SELECT id FROM peer_wake_subscriptions
                    WHERE requester_session_id = ? AND target_session_id = ?
                      AND fired_at_ms IS NULL
                    "#,
                )
                .bind(operation.requester.as_i64())
                .bind(operation.target.as_i64())
                .fetch_one(&mut *tx)
                .await
                .context("Pending wake subscription disappeared during atomic registration")?;
                WakeSubscriptionRegistration {
                    outcome: WakeSubscriptionOutcome::AlreadyPending,
                    subscription_id: Some(subscription_id),
                }
            }
        };

        let fyi_message = if registration.outcome == WakeSubscriptionOutcome::Created {
            if let (Some(subscription_id), Some(fyi_body)) =
                (registration.subscription_id, fyi_body.as_ref())
            {
                let row = sqlx::query(
                    r#"
                    INSERT INTO agent_messages (
                      project_id, from_session_id, to_session_id, kind, body,
                      hop_count, created_at_ms, wake_subscription_id
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    RETURNING id, from_session_id, to_session_id, kind, body,
                              hop_count, created_at_ms, wake_subscription_id
                    "#,
                )
                .bind(project_id)
                .bind(operation.requester.as_i64())
                .bind(operation.target.as_i64())
                .bind(PeerMessageKind::WakeRequest.as_db_str())
                .bind(fyi_body.as_ref())
                .bind(operation.hop_count)
                .bind(now)
                .bind(subscription_id)
                .fetch_one(&mut *tx)
                .await
                .context("Failed to insert atomic wake-request FYI")?;
                Some(peer_message_from_row(row)?)
            } else {
                None
            }
        } else {
            None
        };

        storage_op!(
            &mut tx,
            "finalize atomic peer wake operation",
            sqlx::query(
                r#"
            UPDATE peer_wake_operations
            SET outcome = ?, subscription_id = ?, fyi_message_id = ?
            WHERE id = ?
            "#,
            )
            .bind(registration.outcome.operation_db_str())
            .bind(registration.subscription_id)
            .bind(fyi_message.as_ref().map(|message| message.id))
            .bind(operation_id),
        )?;

        tx.commit()
            .await
            .context("Failed to commit atomic peer wake registration")?;
        Ok(CommittedPeerWake {
            registration,
            fyi_message,
            newly_committed: true,
        })
    }

    /// Return this session's unfired outgoing waits and incoming waiters.
    pub async fn wake_relationships(&self, session_id: SessionId) -> Result<WakeRelationships> {
        let waiting_on = sqlx::query_scalar(
            r#"
            SELECT target_session_id
            FROM peer_wake_subscriptions
            WHERE requester_session_id = ? AND fired_at_ms IS NULL
            ORDER BY created_at_ms ASC
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .context("Failed to list pending peer wake targets")?
        .into_iter()
        .map(SessionId::from_raw)
        .collect();
        let waiters = sqlx::query_scalar(
            r#"
            SELECT requester_session_id
            FROM peer_wake_subscriptions
            WHERE target_session_id = ? AND fired_at_ms IS NULL
            ORDER BY created_at_ms ASC
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .context("Failed to list pending peer wake requesters")?
        .into_iter()
        .map(SessionId::from_raw)
        .collect();
        Ok(WakeRelationships {
            waiting_on,
            waiters,
        })
    }

    /// Fire the unfired wake subscriptions targeting `target`: claim them
    /// atomically (single-shot; a subscription can never fire twice) and insert
    /// one `done_notice` message per requester in the same transaction. Returns
    /// the notified requesters.
    pub async fn fire_wake_subscriptions(&self, target: SessionId) -> Result<Vec<SessionId>> {
        let now = now_ms();
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin wake-subscription firing")?;

        let claimed: Vec<(i64, i64, i64, String, i64)> = sqlx::query_as(
            r#"
            UPDATE peer_wake_subscriptions
            SET fired_at_ms = ?
            WHERE target_session_id = ? AND fired_at_ms IS NULL
            RETURNING id, project_id, requester_session_id, note, hop_count
            "#,
        )
        .bind(now)
        .bind(target.as_i64())
        .fetch_all(&mut *tx)
        .await
        .context("Failed to claim wake subscriptions")?;

        let mut requesters = Vec::with_capacity(claimed.len());
        for (subscription_id, project_id, requester, note, hop_count) in &claimed {
            let note = note.trim();
            let body = if note.is_empty() {
                format!("session #{target} finished its run.")
            } else {
                format!("session #{target} finished its run. Your note: {note}")
            };
            let body = crate::redact::redact(&body);
            // The done notice inherits the subscription's originating hop so a
            // wake chain cannot reset the anti-loop counter by parking.
            storage_op!(
                &mut tx,
                "insert done notice",
                sqlx::query(
                    r#"
                INSERT INTO agent_messages (
                  project_id, from_session_id, to_session_id, kind, body,
                  hop_count, created_at_ms, wake_subscription_id
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                "#,
                )
                .bind(project_id)
                .bind(target.as_i64())
                .bind(requester)
                .bind(PeerMessageKind::DoneNotice.as_db_str())
                .bind(body.as_ref())
                .bind(hop_count)
                .bind(now)
                .bind(subscription_id),
            )?;
            requesters.push(SessionId::from_raw(*requester));
        }

        tx.commit()
            .await
            .context("Failed to commit wake-subscription firing")?;
        Ok(requesters)
    }

    /// Expire one pending wake subscription because its target can no longer
    /// fire it (process gone, or parked/idle without a run to finish): claim
    /// the row and synthesize the done notice the real fire would have sent.
    /// The `fired_at_ms IS NULL` guard makes expiry and the real fire mutually
    /// exclusive — whichever claims first wins, the other is a no-op. Returns
    /// whether this call claimed the row.
    pub async fn expire_wake_subscription(
        &self,
        requester: SessionId,
        target: SessionId,
    ) -> Result<bool> {
        let now = now_ms();
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin wake-subscription expiry")?;
        let claimed: Option<(i64, i64, String, i64)> = sqlx::query_as(
            r#"
            UPDATE peer_wake_subscriptions
            SET fired_at_ms = ?
            WHERE requester_session_id = ? AND target_session_id = ?
              AND fired_at_ms IS NULL
            RETURNING id, project_id, note, hop_count
            "#,
        )
        .bind(now)
        .bind(requester.as_i64())
        .bind(target.as_i64())
        .fetch_optional(&mut *tx)
        .await
        .context("Failed to claim wake subscription for expiry")?;
        let Some((subscription_id, project_id, note, hop_count)) = claimed else {
            return Ok(false);
        };
        let note = note.trim();
        let body = if note.is_empty() {
            format!(
                "session #{target} is no longer running (it exited, crashed, or went idle \
                 without firing its done notice). Treat its changes as settled and continue."
            )
        } else {
            format!(
                "session #{target} is no longer running (it exited, crashed, or went idle \
                 without firing its done notice). Treat its changes as settled and continue. \
                 Your note: {note}"
            )
        };
        let body = crate::redact::redact(&body);
        storage_op!(
            &mut tx,
            "insert expiry done notice",
            sqlx::query(
                r#"
            INSERT INTO agent_messages (
              project_id, from_session_id, to_session_id, kind, body,
              hop_count, created_at_ms, wake_subscription_id
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            )
            .bind(project_id)
            .bind(target.as_i64())
            .bind(requester.as_i64())
            .bind(PeerMessageKind::DoneNotice.as_db_str())
            .bind(body.as_ref())
            .bind(hop_count)
            .bind(now)
            .bind(subscription_id),
        )?;
        tx.commit()
            .await
            .context("Failed to commit wake-subscription expiry")?;
        Ok(true)
    }

    /// Claim an exact pending wake subscription and synthesize a done notice
    /// that asks the requester to re-check its blocker. The subscription,
    /// requester, and target guards prevent an abandoned or replaced wait from
    /// being resumed; `fired_at_ms IS NULL` makes this mutually exclusive with
    /// a real wake or liveness expiry.
    pub async fn recheck_wake_subscription(
        &self,
        requester: SessionId,
        target: SessionId,
        subscription_id: i64,
    ) -> Result<bool> {
        let now = now_ms();
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin wake-subscription recheck")?;
        let claimed: Option<(i64, i64, String, i64)> = sqlx::query_as(
            r#"
            UPDATE peer_wake_subscriptions
            SET fired_at_ms = ?
            WHERE id = ? AND requester_session_id = ? AND target_session_id = ?
              AND fired_at_ms IS NULL
            RETURNING project_id, target_session_id, note, hop_count
            "#,
        )
        .bind(now)
        .bind(subscription_id)
        .bind(requester.as_i64())
        .bind(target.as_i64())
        .fetch_optional(&mut *tx)
        .await
        .context("Failed to claim wake subscription for recheck")?;
        let Some((project_id, target_session_id, note, hop_count)) = claimed else {
            return Ok(false);
        };
        let target = SessionId::from_raw(target_session_id);
        let note = note.trim();
        let body = if note.is_empty() {
            format!(
                "The wait on session #{target} reached its time limit. Re-check whether the blocker persists before deciding whether to wait again."
            )
        } else {
            format!(
                "The wait on session #{target} reached its time limit. Re-check whether the blocker persists before deciding whether to wait again. Your note: {note}"
            )
        };
        let body = crate::redact::redact(&body);
        storage_op!(
            &mut tx,
            "insert wake recheck notice",
            sqlx::query(
                r#"
            INSERT INTO agent_messages (
              project_id, from_session_id, to_session_id, kind, body,
              hop_count, created_at_ms, wake_subscription_id
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            )
            .bind(project_id)
            .bind(target.as_i64())
            .bind(requester.as_i64())
            .bind(PeerMessageKind::DoneNotice.as_db_str())
            .bind(body.as_ref())
            .bind(hop_count)
            .bind(now)
            .bind(subscription_id),
        )?;
        tx.commit()
            .await
            .context("Failed to commit wake-subscription recheck")?;
        Ok(true)
    }

    /// Upsert an advisory claim for `session_id` (peers P4). Re-claiming a
    /// released claim revives it.
    pub async fn add_peer_claim(
        &self,
        project_path: &Path,
        session_id: SessionId,
        claim: &str,
    ) -> Result<()> {
        let project_id = self.ensure_project(project_path).await?;
        sqlx::query(
            r#"
            INSERT INTO peer_claims (project_id, session_id, claim, created_at_ms)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(session_id, claim) DO UPDATE SET
              released_at_ms = NULL,
              created_at_ms = excluded.created_at_ms
            "#,
        )
        .bind(project_id)
        .bind(session_id.as_i64())
        .bind(claim)
        .bind(now_ms())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to add claim for session {session_id}"))?;
        Ok(())
    }

    /// Release one claim (exact text) held by `session_id`. Returns whether a
    /// live claim was released.
    pub async fn release_peer_claim(&self, session_id: SessionId, claim: &str) -> Result<bool> {
        let result = sqlx::query(
            r#"
            UPDATE peer_claims
            SET released_at_ms = ?
            WHERE session_id = ? AND claim = ? AND released_at_ms IS NULL
            "#,
        )
        .bind(now_ms())
        .bind(session_id.as_i64())
        .bind(claim)
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to release claim for session {session_id}"))?;
        Ok(result.rows_affected() > 0)
    }

    /// Release every live claim held by `session_id` — called when a session
    /// reaches a terminal status (clean exit or crash-recovery promotion).
    pub async fn release_all_peer_claims(&self, session_id: SessionId) -> Result<()> {
        sqlx::query(
            "UPDATE peer_claims SET released_at_ms = ? WHERE session_id = ? AND released_at_ms IS NULL",
        )
        .bind(now_ms())
        .bind(session_id.as_i64())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to release claims for session {session_id}"))?;
        Ok(())
    }

    /// The unreleased claims held by `session_id`, oldest first.
    pub async fn peer_claims_for_session(&self, session_id: SessionId) -> Result<Vec<String>> {
        sqlx::query_scalar(
            r#"
            SELECT claim FROM peer_claims
            WHERE session_id = ? AND released_at_ms IS NULL
            ORDER BY created_at_ms ASC
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .context("Failed to list session claims")
    }

    /// Boot-time retention: drop messages older than the retention window and
    /// trim each recipient's backlog to the cap. No steady-state reaper.
    pub async fn purge_stale_peer_state(&self) -> Result<()> {
        let cutoff = now_ms().saturating_sub(MESSAGE_RETENTION_MS);
        sqlx::query("DELETE FROM agent_messages WHERE created_at_ms < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .context("Failed to purge old peer messages")?;
        sqlx::query(
            r#"
            DELETE FROM agent_messages
            WHERE id NOT IN (
              SELECT id FROM agent_messages ranked
              WHERE ranked.to_session_id = agent_messages.to_session_id
              ORDER BY ranked.id DESC
              LIMIT ?
            )
            "#,
        )
        .bind(MESSAGE_CAP_PER_RECIPIENT)
        .execute(&self.pool)
        .await
        .context("Failed to trim peer message backlog")?;
        sqlx::query(
            "DELETE FROM peer_send_operations
             WHERE created_at_ms < ?
               AND NOT EXISTS (
                 SELECT 1 FROM agent_messages
                 WHERE send_operation_id = peer_send_operations.id
               )",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .context("Failed to purge stale peer send operations")?;
        // Wake subscriptions: fired rows are pure history after the retention
        // window; an unfired row that old is orphaned by definition (its
        // requester's park ended long ago, by sweep or by human input).
        sqlx::query(
            "DELETE FROM peer_wake_subscriptions
             WHERE COALESCE(fired_at_ms, created_at_ms) < ?",
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await
        .context("Failed to purge stale wake subscriptions")?;
        sqlx::query("DELETE FROM peer_wake_operations WHERE created_at_ms < ?")
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .context("Failed to purge stale peer wake operations")?;
        Ok(())
    }
}
