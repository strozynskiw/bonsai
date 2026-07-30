//! Inter-agent communication bus (peers P2): the shared state connecting the
//! `peers` tool, the `Agent`'s context injection, and the TUI event loop for
//! one process. The transport is the global SQLite store; this type adds the
//! process-local pieces — "who am I", the outgoing-echo queue for the blue
//! chat rendering, and the in-process anti-loop state (send rate limit; hop
//! tracking arrives with wake-on-done in P3).

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};

use crate::storage::{
    CommittedPeerSend, CommittedPeerWake, LeasedPeerMessage, PeerMessage, PeerMessageKind,
    PeerSendOperation, PeerSessionSummary, PeerWakeOperation, SessionId, Storage,
    WakeRelationships, WakeSubscriptionRegistration,
};
use crate::tool::SharedActiveSessionId;

/// Peer message bodies are capped so a chatty (or hostile) peer can't blow up
/// the recipient's context. Sized for a structured payload — e.g. the review
/// findings for one peer's files after a sync review — not just chatter; the
/// injection framing, redaction, and the per-recipient retention cap still
/// bound the worst case.
pub const PEER_MESSAGE_MAX_CHARS: usize = 6_000;

/// Advisory claims are one short label, not prose.
pub const PEER_CLAIM_MAX_CHARS: usize = 120;

/// `peers send` refuses to exceed this many sends per rolling hour — the model
/// is told to batch updates instead of streaming them.
const MAX_SENDS_PER_HOUR: usize = 30;
/// Auto-wakes are capped per rolling hour (anti-loop brake #4); beyond the cap
/// messages park visibly for the human instead of starting turns.
const MAX_AUTO_WAKES_PER_HOUR: usize = 10;
const RATE_WINDOW: Duration = Duration::from_secs(60 * 60);
static NEXT_LOCAL_PEER_OPERATION: AtomicU64 = AtomicU64::new(1);

/// A peer message survives this many automatic bounces before it stops waking
/// agents (anti-loop brake #3): two agents ping-ponging on auto-wake die out
/// after three hops and the conversation parks for the humans.
pub const MAX_PEER_HOPS: i64 = 3;

/// Stable, bounded database key for one provider tool-call identity.
pub(crate) fn peer_tool_operation_key(tool_call_id: &str) -> String {
    format!(
        "tool-call:{}",
        blake3::hash(tool_call_id.as_bytes()).to_hex()
    )
}

fn fresh_peer_operation_key() -> String {
    let sequence = NEXT_LOCAL_PEER_OPERATION.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("local:{}:{nanos}:{sequence}", std::process::id())
}

/// Why the current agent turn started — the hop-tracking input for sends made
/// during the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TurnOrigin {
    /// A human submitted input (or a non-peer wake); hops reset.
    #[default]
    Human,
    /// The turn was auto-started by peer messages; `hop` is the highest hop
    /// among the messages that triggered it.
    PeerWake { hop: i64 },
}

/// An outgoing message echoed into this process's own transcript, so a
/// `peers send` shows up as a `⇄ you → agent #45` blue chat message at the
/// send site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerEcho {
    pub to_session_id: SessionId,
    pub body: String,
}

/// A live peer as shown in the `/peers` view (peers P5). Carries everything the
/// modal renders, so the watcher can change-gate on it and the modal never has
/// to hit the DB itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerOverview {
    pub id: SessionId,
    pub title: String,
    /// Working (mid-turn) vs idle — a coordination signal (has the peer that
    /// owns shared files stopped editing?), not an invitation to delegate.
    pub working: bool,
    /// This session has an unfired wake subscription for the peer.
    pub waiting_on_peer: bool,
    /// The peer has an unfired wake subscription for this session.
    pub peer_waiting_on_you: bool,
    pub claims: Vec<String>,
    pub changed_files: Vec<String>,
}

/// The process-local peer bus. Shared as `Arc<PeerBus>` by the `peers` tool,
/// the `Agent`, and the TUI event loop — the same sharing shape as
/// `Arc<BackgroundTaskRegistry>`.
#[derive(Debug)]
pub struct PeerBus {
    storage: Storage,
    active_session_id: SharedActiveSessionId,
    project_root: PathBuf,
    /// Outgoing echoes awaiting the event loop's per-frame drain.
    echoes: StdMutex<Vec<PeerEcho>>,
    /// Recent send instants for the rolling-hour rate limit.
    send_times: StdMutex<VecDeque<Instant>>,
    /// Why the current turn started (hop tracking, peers P3).
    turn_origin: StdMutex<TurnOrigin>,
    /// Recent auto-wake instants for the rolling-hour wake cap.
    wake_times: StdMutex<VecDeque<Instant>>,
}

impl PeerBus {
    pub fn new(
        storage: Storage,
        active_session_id: SharedActiveSessionId,
        project_root: PathBuf,
    ) -> Self {
        Self {
            storage,
            active_session_id,
            project_root,
            echoes: StdMutex::new(Vec::new()),
            send_times: StdMutex::new(VecDeque::new()),
            turn_origin: StdMutex::new(TurnOrigin::Human),
            wake_times: StdMutex::new(VecDeque::new()),
        }
    }

    /// Record why the turn that is about to start was started. Human turns
    /// reset the hop chain; peer wakes carry the triggering messages' hop.
    pub fn begin_turn(&self, origin: TurnOrigin) {
        *self.turn_origin.lock().expect("turn-origin lock poisoned") = origin;
    }

    /// Whether the current turn was started automatically by an incoming peer
    /// message rather than directly by the human.
    pub fn is_peer_wake_turn(&self) -> bool {
        matches!(
            *self.turn_origin.lock().expect("turn-origin lock poisoned"),
            TurnOrigin::PeerWake { .. }
        )
    }

    /// The hop count a message sent during the current turn should carry:
    /// one more than the hop that started this turn (0 for human turns).
    pub fn outgoing_hop(&self) -> i64 {
        match *self.turn_origin.lock().expect("turn-origin lock poisoned") {
            TurnOrigin::Human => 0,
            TurnOrigin::PeerWake { hop } => hop.saturating_add(1),
        }
    }

    /// Whether an auto-wake is allowed under the rolling-hour cap; consumes a
    /// slot when allowed (anti-loop brake #4).
    pub fn try_consume_wake_slot(&self) -> bool {
        let mut times = self.wake_times.lock().expect("wake-rate lock poisoned");
        let now = Instant::now();
        while times
            .front()
            .is_some_and(|at| now.duration_since(*at) > RATE_WINDOW)
        {
            times.pop_front();
        }
        if times.len() >= MAX_AUTO_WAKES_PER_HOUR {
            return false;
        }
        times.push_back(now);
        true
    }

    /// This process's session id, or an error when no session has started yet
    /// (the bus is only wired after the session row exists, so this is a
    /// should-not-happen guard rather than a real state).
    pub async fn self_id(&self) -> Result<SessionId> {
        match *self.active_session_id.lock().await {
            Some(id) => Ok(id),
            None => bail!("No active session yet; peer messaging is unavailable"),
        }
    }

    /// The other live bonsai sessions in this project root.
    pub async fn live_peers(&self) -> Result<Vec<PeerSessionSummary>> {
        let self_id = self.self_id().await?;
        self.storage
            .list_live_peer_sessions(&self.project_root, self_id)
            .await
    }

    /// Notify live sessions that one persistent memory tier changed. This is
    /// deliberately separate from user-facing sends: it has no echo, rate
    /// accounting, wake behavior, or model-visible body.
    pub(crate) async fn publish_memory_refresh(
        &self,
        project_id: i64,
        tier: crate::memory::entry::MemoryTier,
    ) -> Result<()> {
        let self_id = self.self_id().await?;
        self.storage
            .publish_memory_refresh(project_id, self_id, tier)
            .await?;
        Ok(())
    }

    /// A full overview of every live peer — title, claims, and recently changed
    /// files — for the `/peers` view and its live-refresh watcher (peers P5).
    pub async fn overview(&self) -> Result<Vec<PeerOverview>> {
        let peers = self.live_peers().await?;
        let relationships = self.wake_relationships().await?;
        let waiting_on = relationships.waiting_on.into_iter().collect::<HashSet<_>>();
        let waiters = relationships.waiters.into_iter().collect::<HashSet<_>>();
        let mut out = Vec::with_capacity(peers.len());
        for peer in peers {
            let claims = self.claims_for(peer.id).await.unwrap_or_default();
            let changed_files = self
                .recent_file_changes(peer.id, 5)
                .await
                .unwrap_or_default();
            out.push(PeerOverview {
                id: peer.id,
                title: peer.summary,
                working: peer.working,
                waiting_on_peer: waiting_on.contains(&peer.id),
                peer_waiting_on_you: waiters.contains(&peer.id),
                claims,
                changed_files,
            });
        }
        Ok(out)
    }

    /// Send `body` to `to` (or broadcast to every live peer when `to` is
    /// `None`). Validates the target is live, enforces the body cap and the
    /// rolling-hour rate limit, stamps the anti-loop hop count from the current
    /// turn's origin, queues the outgoing echo for the transcript, and returns
    /// the recipient ids.
    pub async fn send(&self, to: Option<SessionId>, body: &str) -> Result<Vec<SessionId>> {
        let operation_key = fresh_peer_operation_key();
        self.send_idempotent(to, body, &operation_key).await
    }

    /// Execute a peer send under a durable operation identity. Replaying the
    /// key returns the original recipients without inserting or echoing them
    /// again.
    pub(crate) async fn send_idempotent(
        &self,
        to: Option<SessionId>,
        body: &str,
        operation_key: &str,
    ) -> Result<Vec<SessionId>> {
        let hop_count = self.outgoing_hop();
        let body = body.trim();
        if body.is_empty() {
            bail!("Peer message body is empty");
        }
        if body.chars().count() > PEER_MESSAGE_MAX_CHARS {
            bail!("Peer message exceeds {PEER_MESSAGE_MAX_CHARS} characters; shorten it");
        }
        let body = crate::redact::redact(body).into_owned();
        let self_id = self.self_id().await?;
        let audience = to
            .map(|target| format!("session:{}", target.as_i64()))
            .unwrap_or_else(|| "broadcast".to_string());
        let replay_probe = PeerSendOperation {
            project_path: &self.project_root,
            from: self_id,
            recipients: &[],
            audience: &audience,
            kind: PeerMessageKind::Text,
            body: &body,
            hop_count,
            idempotency_key: operation_key,
        };
        if let Some(committed) = self.storage.replay_peer_send(&replay_probe).await? {
            return Ok(committed
                .messages
                .into_iter()
                .map(|message| message.to_session_id)
                .collect());
        }

        let peers = self.live_peers().await?;
        let recipients: Vec<SessionId> = match to {
            Some(target) => {
                if !peers.iter().any(|peer| peer.id == target) {
                    bail!(
                        "Session #{target} is not a live peer in this project; run peers list first"
                    );
                }
                vec![target]
            }
            None => peers.iter().map(|peer| peer.id).collect(),
        };
        if recipients.is_empty() {
            bail!("No live peer sessions in this project to message");
        }
        self.check_send_rate()?;
        let committed: CommittedPeerSend = self
            .storage
            .commit_peer_send(PeerSendOperation {
                project_path: &self.project_root,
                from: self_id,
                recipients: &recipients,
                audience: &audience,
                kind: PeerMessageKind::Text,
                body: &body,
                hop_count,
                idempotency_key: operation_key,
            })
            .await?;
        if committed.newly_committed {
            for message in &committed.messages {
                self.push_echo(PeerEcho {
                    to_session_id: message.to_session_id,
                    body: message.body.clone(),
                });
            }
        }
        Ok(committed
            .messages
            .into_iter()
            .map(|message| message.to_session_id)
            .collect())
    }

    /// Subscribe this session to a one-shot wake when `target` finishes its
    /// current run. The subscription records the current turn's hop so the
    /// eventual done notice carries the anti-loop chain forward; the cap is
    /// enforced here, at creation — refusing at resume time would strand an
    /// already-parked waiter.
    pub async fn wake_when_done(
        &self,
        target: SessionId,
        note: &str,
    ) -> Result<WakeSubscriptionRegistration> {
        let operation_key = fresh_peer_operation_key();
        self.wake_when_done_idempotent(target, note, &operation_key)
            .await
    }

    /// Look up a committed wake operation without creating one. The tool uses
    /// this before applying live/idle guards so a lost-result retry preserves
    /// the already-durable wait outcome.
    pub(crate) async fn replay_wake_when_done(
        &self,
        target: SessionId,
        note: &str,
        operation_key: &str,
    ) -> Result<Option<WakeSubscriptionRegistration>> {
        let note = crate::redact::redact(note.trim()).into_owned();
        let fyi_body = (!note.is_empty()).then(|| {
            crate::redact::redact(&format!(
                "FYI — I am waiting for your run to finish (no reply needed). {note}"
            ))
            .into_owned()
        });
        let self_id = self.self_id().await?;
        Ok(self
            .storage
            .replay_peer_wake(&PeerWakeOperation {
                project_path: &self.project_root,
                requester: self_id,
                target,
                note: &note,
                fyi_body: fyi_body.as_deref(),
                hop_count: self.outgoing_hop(),
                idempotency_key: operation_key,
            })
            .await?
            .map(|committed| committed.registration))
    }

    /// Atomically register a wake and its optional FYI under one durable tool
    /// operation identity.
    pub(crate) async fn wake_when_done_idempotent(
        &self,
        target: SessionId,
        note: &str,
        operation_key: &str,
    ) -> Result<WakeSubscriptionRegistration> {
        let hop = self.outgoing_hop();
        if hop >= MAX_PEER_HOPS {
            bail!(
                "This turn is already {hop} peer-wake hops deep (cap {MAX_PEER_HOPS}); \
                 waiting again risks an unattended agent chain. Summarize the situation \
                 for the human instead of parking."
            );
        }
        let note = crate::redact::redact(note.trim()).into_owned();
        let self_id = self.self_id().await?;
        let fyi_body = (!note.is_empty()).then(|| {
            crate::redact::redact(&format!(
                "FYI — I am waiting for your run to finish (no reply needed). {note}"
            ))
            .into_owned()
        });
        let committed: CommittedPeerWake = self
            .storage
            .commit_peer_wake(PeerWakeOperation {
                project_path: &self.project_root,
                requester: self_id,
                target,
                note: &note,
                fyi_body: fyi_body.as_deref(),
                hop_count: hop,
                idempotency_key: operation_key,
            })
            .await?;
        if committed.newly_committed
            && let Some(message) = committed.fyi_message
        {
            self.push_echo(PeerEcho {
                to_session_id: message.to_session_id,
                body: message.body,
            });
        }
        Ok(committed.registration)
    }

    /// This session's pending outgoing waits and incoming waiters.
    pub async fn wake_relationships(&self) -> Result<WakeRelationships> {
        let self_id = self.self_id().await?;
        self.storage.wake_relationships(self_id).await
    }

    /// Cancel this session's outstanding waits without producing done notices.
    /// A newly started unrelated turn calls this before clearing its local park.
    pub async fn cancel_own_wake_subscriptions(&self) -> Result<u64> {
        let self_id = self.self_id().await?;
        self.storage.cancel_wake_subscriptions(self_id).await
    }

    /// Fire the wake subscriptions targeting this session (called when its
    /// agent run finishes). Returns the requesters that were notified.
    pub async fn fire_own_wake_subscriptions(&self) -> Result<Vec<SessionId>> {
        let self_id = self.self_id().await?;
        self.storage.fire_wake_subscriptions(self_id).await
    }

    /// Expire this session's pending wait on `target` because the target can
    /// no longer fire it (crashed, exited, or parked/idle itself). The
    /// synthesized done notice arrives through the normal inbox, so the park
    /// resumes on the standard path. Returns whether a pending wait was
    /// claimed (false = already fired/expired; benign race).
    pub async fn expire_wake_subscription(&self, target: SessionId) -> Result<bool> {
        let self_id = self.self_id().await?;
        self.storage.expire_wake_subscription(self_id, target).await
    }

    /// Resume this session's exact pending wait, allowing the agent to check
    /// whether the original blocker still exists. Returns whether the pending
    /// subscription was claimed.
    pub async fn recheck_wake_subscription(&self, wait: crate::agent::PeerWait) -> Result<bool> {
        let self_id = self.self_id().await?;
        self.storage
            .recheck_wake_subscription(self_id, wait.session_id, wait.subscription_id)
            .await
    }

    /// Messages still awaiting injection into this agent — the actionable-inbox
    /// gate a peer wake re-checks before starting a turn (anti-loop brake #2).
    pub async fn pending_inbox_count(&self) -> Result<i64> {
        let self_id = self.self_id().await?;
        self.storage.pending_agent_message_count(self_id).await
    }

    /// Claim shared work advisorily (peers P4): visible to every live peer in
    /// `peers list` and the volatile peer-status block.
    pub async fn claim(&self, claim: &str) -> Result<()> {
        let claim = claim.trim();
        if claim.is_empty() {
            bail!("Claim text is empty");
        }
        if claim.chars().count() > PEER_CLAIM_MAX_CHARS {
            bail!("Claim exceeds {PEER_CLAIM_MAX_CHARS} characters; shorten it");
        }
        let self_id = self.self_id().await?;
        self.storage
            .add_peer_claim(&self.project_root, self_id, claim)
            .await
    }

    /// Release one advisory claim; returns whether a live claim was released.
    pub async fn release(&self, claim: &str) -> Result<bool> {
        let self_id = self.self_id().await?;
        self.storage.release_peer_claim(self_id, claim.trim()).await
    }

    /// The unreleased claims held by `session_id` (own or a peer's).
    pub async fn claims_for(&self, session_id: SessionId) -> Result<Vec<String>> {
        self.storage.peer_claims_for_session(session_id).await
    }

    /// Claim the messages not yet rendered by this process's UI (the 2s
    /// poller path).
    pub async fn claim_ui_undelivered(&self) -> Result<Vec<LeasedPeerMessage>> {
        let self_id = self.self_id().await?;
        self.storage.claim_ui_undelivered_messages(self_id).await
    }

    /// Claim the messages not yet injected into this process's agent (the
    /// loop-top injection path).
    pub async fn claim_agent_undelivered(&self) -> Result<Vec<LeasedPeerMessage>> {
        let self_id = self.self_id().await?;
        self.storage.claim_agent_undelivered_messages(self_id).await
    }

    /// Current durable agent-inbox rows among a wake batch's message ids.
    pub async fn pending_agent_messages_by_id(
        &self,
        message_ids: &[i64],
    ) -> Result<Vec<PeerMessage>> {
        let self_id = self.self_id().await?;
        self.storage
            .pending_agent_messages_by_id(self_id, message_ids)
            .await
    }

    /// The files a peer session changed most recently (newest first).
    pub async fn recent_file_changes(
        &self,
        session_id: SessionId,
        limit: i64,
    ) -> Result<Vec<String>> {
        self.storage
            .recent_session_file_changes(session_id, limit)
            .await
    }

    /// File paths other sessions recorded in this project since `since_ms`.
    /// Unlike [`Self::recent_file_changes`], this is project-scoped and excludes
    /// this bus's active session so foreground-Bash attribution can be cautious.
    pub async fn peer_file_changes_since(&self, since_ms: i64) -> Result<Vec<String>> {
        let self_id = self.self_id().await?;
        self.storage
            .other_session_file_changes_since(&self.project_root, self_id, since_ms)
            .await
    }

    /// Drain the outgoing echoes queued since the last frame.
    pub fn drain_echoes(&self) -> Vec<PeerEcho> {
        std::mem::take(&mut self.echoes.lock().expect("echo queue lock poisoned"))
    }

    fn push_echo(&self, echo: PeerEcho) {
        self.echoes
            .lock()
            .expect("echo queue lock poisoned")
            .push(echo);
    }

    fn check_send_rate(&self) -> Result<()> {
        let mut times = self.send_times.lock().expect("send-rate lock poisoned");
        let now = Instant::now();
        while times
            .front()
            .is_some_and(|at| now.duration_since(*at) > RATE_WINDOW)
        {
            times.pop_front();
        }
        if times.len() >= MAX_SENDS_PER_HOUR {
            bail!(
                "Peer send rate limit reached ({MAX_SENDS_PER_HOUR}/hour); batch your updates into fewer messages"
            );
        }
        times.push_back(now);
        Ok(())
    }
}

/// Build the body for one canonical untrusted runtime frame. The agent owns
/// framing/redaction so peer, shell, terminal, and subagent data cannot drift
/// into bespoke delimiter formats; this helper adds only peer-specific reply
/// guidance.
pub fn peer_injection_body(messages: &[PeerMessage]) -> String {
    let mut sections = Vec::with_capacity(messages.len());
    for message in messages {
        let from = message.from_session_id;
        let body = &message.body;
        // The trailer varies by kind on purpose: text messages are answerable
        // (pointer at how to reply), wake-request FYIs are explicitly not —
        // inviting a reply there would spend the peer turn the ask/ack
        // protocol exists to avoid.
        let section = match message.kind {
            PeerMessageKind::Text => format!(
                "Message from bonsai session #{from}:\n{body}\n\n\
                 If it asks a question you can answer, reply with the peers tool \
                 (action: send, session_id: {from})."
            ),
            PeerMessageKind::DoneNotice => format!(
                "Notice from bonsai session #{from}:\n{body}\n\n\
                 If it asks a question you can answer, reply with the peers tool \
                 (action: send, session_id: {from})."
            ),
            PeerMessageKind::WakeRequest => format!(
                "FYI from bonsai session #{from} — it is parked waiting for your current \
                 run to finish:\n{body}\n\n\
                 No reply is needed and none is expected; session #{from} resumes \
                 automatically when your run ends."
            ),
            PeerMessageKind::MemoryRefresh => continue,
        };
        sections.push(section);
    }
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: PeerMessageKind, body: &str) -> PeerMessage {
        PeerMessage {
            id: 1,
            from_session_id: SessionId::from_raw(45),
            to_session_id: SessionId::from_raw(7),
            kind,
            body: body.to_string(),
            hop_count: 0,
            created_at_ms: 0,
            wake_subscription_id: None,
        }
    }

    #[test]
    fn injection_frames_as_untrusted_and_answerable() {
        let framed = crate::tool::wrap_untrusted_content(
            "bonsai peer messages",
            &peer_injection_body(&[message(
                PeerMessageKind::Text,
                "what did you change in src/tui/?",
            )]),
        );
        assert!(framed.contains("Message from bonsai session #45:"));
        assert!(framed.contains("what did you change in src/tui/?"));
        assert!(framed.contains("UNTRUSTED external data"));
        assert!(framed.contains("session_id: 45"), "{framed}");
    }

    #[test]
    fn injection_redacts_credentials_in_peer_bodies() {
        let framed = crate::redact::redact(&peer_injection_body(&[message(
            PeerMessageKind::Text,
            "use ghp_1234567890abcdefghijklmnopqrstuvwxyz1234 to auth",
        )]))
        .into_owned();
        assert!(!framed.contains("ghp_1234567890abcdefghijklmnopqrstuvwxyz1234"));
    }

    #[test]
    fn outgoing_hop_tracks_turn_origin() {
        let bus = test_bus();
        // Default (human) turns send fresh messages.
        assert_eq!(bus.outgoing_hop(), 0);
        // A peer-wake turn increments the chain it was woken by.
        bus.begin_turn(TurnOrigin::PeerWake { hop: 1 });
        assert_eq!(bus.outgoing_hop(), 2);
        // The next human turn resets the chain.
        bus.begin_turn(TurnOrigin::Human);
        assert_eq!(bus.outgoing_hop(), 0);
    }

    #[test]
    fn wake_slots_are_capped_per_rolling_hour() {
        let bus = test_bus();
        for _ in 0..10 {
            assert!(bus.try_consume_wake_slot());
        }
        assert!(
            !bus.try_consume_wake_slot(),
            "the 11th auto-wake within an hour must be refused"
        );
    }

    fn test_bus() -> PeerBus {
        // Storage is unused by the synchronous hop/rate state; a throwaway
        // in-memory handle keeps construction simple.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let storage = runtime.block_on(async {
            crate::storage::Storage::open_at(tempfile::TempDir::new().unwrap().path().join("t.db"))
                .await
                .unwrap()
        });
        PeerBus::new(
            storage,
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            PathBuf::from("."),
        )
    }

    #[test]
    fn wake_request_injection_frames_as_no_reply_fyi() {
        let framed = crate::tool::wrap_untrusted_content(
            "bonsai peer messages",
            &peer_injection_body(&[message(
                PeerMessageKind::WakeRequest,
                "FYI — I am waiting for your run to finish (no reply needed). rebasing after you",
            )]),
        );
        assert!(framed.contains("FYI from bonsai session #45"), "{framed}");
        assert!(framed.contains("No reply is needed"), "{framed}");
        assert!(framed.contains("UNTRUSTED external data"), "{framed}");
        assert!(
            !framed.contains("reply with the peers tool"),
            "a wake FYI must not invite a reply: {framed}"
        );
    }

    #[test]
    fn done_notice_uses_notice_header() {
        let framed = peer_injection_body(&[message(
            PeerMessageKind::DoneNotice,
            "session #45 finished its run.",
        )]);
        assert!(framed.contains("Notice from bonsai session #45:"));
    }

    #[test]
    fn memory_refresh_is_never_injected() {
        assert!(peer_injection_body(&[message(PeerMessageKind::MemoryRefresh, "")]).is_empty());
    }

    #[tokio::test]
    async fn broadcast_uses_one_redacted_body_for_storage_and_every_echo() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let sender = fixture.start_session().await;
        fixture
            .storage
            .record_session_heartbeat(sender, false)
            .await
            .unwrap();
        let first = fixture.start_session().await;
        fixture
            .storage
            .record_session_heartbeat(first, false)
            .await
            .unwrap();
        let second = fixture.start_session().await;
        fixture
            .storage
            .record_session_heartbeat(second, false)
            .await
            .unwrap();

        let bus = PeerBus::new(
            fixture.storage.clone(),
            std::sync::Arc::new(tokio::sync::Mutex::new(Some(sender))),
            fixture.project_path().to_path_buf(),
        );
        let secret = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        let body = format!("please review src/peer.rs; keep feature = true; token={secret}");
        let expected = crate::redact::redact(&body).into_owned();

        let mut recipients = bus.send(None, &body).await.unwrap();
        recipients.sort_unstable();
        assert_eq!(recipients, vec![first, second]);

        let mut echoes = bus.drain_echoes();
        echoes.sort_by_key(|echo| echo.to_session_id);
        assert_eq!(echoes.len(), 2);
        assert!(echoes.iter().all(|echo| echo.body == expected));

        for recipient in recipients {
            let inbox = fixture
                .storage
                .claim_ui_undelivered_messages(recipient)
                .await
                .unwrap();
            assert_eq!(inbox.len(), 1);
            assert_eq!(inbox[0].body, expected);
        }
        assert!(!expected.contains(&secret));
        assert!(expected.contains("[REDACTED:GitHub token]"));
        assert!(expected.contains("src/peer.rs; keep feature = true"));

        bus.wake_when_done_idempotent(
            first,
            &format!("rebase after {secret}"),
            "redacted-wake-test",
        )
        .await
        .unwrap();
        let wake_echoes = bus.drain_echoes();
        assert_eq!(wake_echoes.len(), 1);
        let wake_inbox = fixture
            .storage
            .claim_ui_undelivered_messages(first)
            .await
            .unwrap();
        assert_eq!(wake_inbox.len(), 1);
        assert_eq!(wake_echoes[0].body, wake_inbox[0].body);
        assert!(!wake_echoes[0].body.contains(&secret));
        assert!(wake_echoes[0].body.contains("[REDACTED:GitHub token]"));
    }

    #[tokio::test]
    async fn broadcast_failure_rolls_back_every_recipient_and_echo() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let sender = fixture.start_session().await;
        let first = fixture.start_session().await;
        let second = fixture.start_session().await;
        for session_id in [sender, first, second] {
            fixture
                .storage
                .record_session_heartbeat(session_id, false)
                .await
                .unwrap();
        }
        let bus = PeerBus::new(
            fixture.storage.clone(),
            std::sync::Arc::new(tokio::sync::Mutex::new(Some(sender))),
            fixture.project_path().to_path_buf(),
        );

        for (index, failing_recipient) in [first, second].into_iter().enumerate() {
            fixture
                .storage
                .fail_peer_message_recipient_for_tests(failing_recipient)
                .await
                .unwrap();
            let error = bus
                .send_idempotent(None, "atomic broadcast", &format!("failed-{index}"))
                .await
                .expect_err("the injected recipient must abort the fan-out");
            assert!(error.to_string().contains("Failed to commit peer send"));
            assert!(bus.drain_echoes().is_empty());
            for recipient in [first, second] {
                assert!(
                    fixture
                        .storage
                        .claim_ui_undelivered_messages(recipient)
                        .await
                        .unwrap()
                        .is_empty(),
                    "failure at recipient {index} left a partial row for {recipient}"
                );
            }
        }
        fixture
            .storage
            .clear_peer_message_failure_for_tests()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn broadcast_retry_returns_original_rows_without_duplicate_echoes() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let sender = fixture.start_session().await;
        let first = fixture.start_session().await;
        let second = fixture.start_session().await;
        for session_id in [sender, first, second] {
            fixture
                .storage
                .record_session_heartbeat(session_id, false)
                .await
                .unwrap();
        }
        let bus = PeerBus::new(
            fixture.storage.clone(),
            std::sync::Arc::new(tokio::sync::Mutex::new(Some(sender))),
            fixture.project_path().to_path_buf(),
        );

        let mut first_result = bus
            .send_idempotent(None, "one committed broadcast", "same-send")
            .await
            .unwrap();
        first_result.sort_unstable();
        assert_eq!(first_result, vec![first, second]);
        assert_eq!(bus.drain_echoes().len(), 2);

        // Replay is keyed to the committed operation, not today's liveness
        // snapshot. Both original recipients may disappear after commit.
        for recipient in [first, second] {
            fixture
                .storage
                .set_session_heartbeat_for_tests(recipient, 0)
                .await
                .unwrap();
        }

        let mut replay = bus
            .send_idempotent(None, "one committed broadcast", "same-send")
            .await
            .unwrap();
        replay.sort_unstable();
        assert_eq!(replay, first_result);
        assert!(bus.drain_echoes().is_empty());
        for recipient in [first, second] {
            let inbox = fixture
                .storage
                .claim_ui_undelivered_messages(recipient)
                .await
                .unwrap();
            assert_eq!(inbox.len(), 1, "retry duplicated recipient {recipient}");
        }
    }

    #[tokio::test]
    async fn wake_fyi_failure_rolls_back_and_retry_is_idempotent() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let requester = fixture.start_session().await;
        let target = fixture.start_session().await;
        let bus = PeerBus::new(
            fixture.storage.clone(),
            std::sync::Arc::new(tokio::sync::Mutex::new(Some(requester))),
            fixture.project_path().to_path_buf(),
        );

        fixture
            .storage
            .fail_wake_request_insert_for_tests()
            .await
            .unwrap();
        bus.wake_when_done_idempotent(target, "finish the refactor", "same-wake")
            .await
            .expect_err("the injected FYI failure must abort registration");
        assert!(bus.drain_echoes().is_empty());
        assert!(
            bus.wake_relationships()
                .await
                .unwrap()
                .waiting_on
                .is_empty()
        );
        assert!(
            fixture
                .storage
                .claim_ui_undelivered_messages(target)
                .await
                .unwrap()
                .is_empty()
        );

        fixture
            .storage
            .clear_peer_message_failure_for_tests()
            .await
            .unwrap();
        let committed = bus
            .wake_when_done_idempotent(target, "finish the refactor", "same-wake")
            .await
            .unwrap();
        assert_eq!(
            committed.outcome,
            crate::storage::WakeSubscriptionOutcome::Created
        );
        let subscription_id = committed.subscription_id.unwrap();
        assert_eq!(bus.drain_echoes().len(), 1);
        assert_eq!(
            fixture
                .storage
                .claim_ui_undelivered_messages(target)
                .await
                .unwrap()
                .len(),
            1
        );

        let replay = bus
            .wake_when_done_idempotent(target, "finish the refactor", "same-wake")
            .await
            .unwrap();
        assert_eq!(replay.subscription_id, Some(subscription_id));
        assert!(bus.drain_echoes().is_empty());
        assert!(
            fixture
                .storage
                .claim_ui_undelivered_messages(target)
                .await
                .unwrap()
                .is_empty()
        );

        fixture
            .storage
            .fire_wake_subscriptions(target)
            .await
            .unwrap();
        let replay_after_fire = bus
            .wake_when_done_idempotent(target, "finish the refactor", "same-wake")
            .await
            .unwrap();
        assert_eq!(replay_after_fire.subscription_id, Some(subscription_id));
        assert!(
            bus.wake_relationships()
                .await
                .unwrap()
                .waiting_on
                .is_empty()
        );
    }
}
