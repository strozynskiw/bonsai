//! The `peers` tool (inter-agent communication P2): lets the model see the
//! other live bonsai sessions in this project root and message them. The
//! action enum grows per phase (`wake_when_done` lands with P3, `claim` /
//! `release` with P4) so the schema never advertises verbs that don't work.

use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

use crate::agent::WaitReason;
use crate::peer::{PEER_MESSAGE_MAX_CHARS, PeerBus, PeerOverview, peer_tool_operation_key};
use crate::storage::{SessionId, WakeSubscriptionOutcome};
use crate::tool::schema::{
    bounded_integer_property, object, parse_args, string_enum_property, string_property,
};
use crate::tool::{ParallelPolicy, Tool, ToolExecutionContext, ToolOutput};

#[derive(Debug, Deserialize)]
struct PeersArgs {
    action: PeerAction,
    #[serde(default)]
    session_id: Option<i64>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    claim: Option<String>,
    #[serde(default)]
    blocking_reason: Option<PeerBlockingReason>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PeerAction {
    List,
    Send,
    WakeWhenDone,
    Claim,
    Release,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PeerBlockingReason {
    EditConflict,
    BrokenSharedState,
}

#[derive(Debug)]
pub struct PeersTool {
    bus: Arc<PeerBus>,
    /// Whether this surface can park a turn on a nonterminal wait (TUI yes,
    /// headless/eval no). Gates `wake_when_done` out of the schema and the
    /// description, plus an execute-time refusal as defense in depth.
    can_park: bool,
    description: String,
}

impl PeersTool {
    pub fn new(bus: Arc<PeerBus>, can_park: bool) -> Self {
        let wake_sentences = if can_park {
            "Same-file overlap alone is normal: do not wait merely because list shows the \
             same path. Continue with narrow edits and, if freshness rejects a mutation, \
             re-read the current file and reapply only your hunk. If coordination helps but \
             work can proceed, send one short message and keep working. Use wake_when_done \
             only after a concrete edit collision prevents a safe change or unfinished peer \
             work has temporarily broken the shared file or build. It delivers your note to \
             a still-working peer as an FYI (no reply needed), returns that peer's current \
             status synthesized from shared state (no peer turn is spent), and parks this turn \
             — you resume automatically with a done notice when its run ends, so never poll a \
             peer in a loop. Use it only from a human-started turn and only when the peer's \
             whole run blocks you. If you need a condition inside that run (for \
             example, compilation fixed), use send to ask the peer to send you a message when \
             the condition is met, then finish your turn; that message wakes you. A turn \
             woken by any peer message must continue or finish, never park on \
             wake_when_done again. An idle peer's changes are already settled (don't wait \
             on it)."
        } else {
            "This non-interactive run cannot park a turn, so there is no wake_when_done \
             here — finish your run instead; peers watching this session are woken \
             automatically when it exits."
        };
        let description = format!(
            "Synchronize with the other live bonsai sessions (separate processes) in this \
             project — for coordination, NOT delegation: to review or investigate your own \
             changes, use the agent tool (a subagent runs now in a clean context), not a \
             peer. {wake_sentences} claim/release advertise expensive shared work (e.g. \
             'running full test suite') so peers don't duplicate it. send delivers a real \
             coordination message to one peer (session_id) or all (no session_id) — \
             including routing review results after a sync review: when you review combined \
             work with a review subagent, send each peer only the findings for files it \
             changed (peers list shows who changed what). list shows peers' \
             id/title/working-idle/wait relationships/claims/changed-files, but the stable \
             coordination fields already ride your Volatile state context each turn — call \
             list mainly for the changed files."
        );
        Self {
            bus,
            can_park,
            description,
        }
    }
}

#[async_trait]
impl Tool for PeersTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::LocalState
    }

    fn name(&self) -> &str {
        "peers"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        let actions: &[&str] = if self.can_park {
            &["list", "send", "wake_when_done", "claim", "release"]
        } else {
            &["list", "send", "claim", "release"]
        };
        object(
            [
                (
                    "action",
                    string_enum_property("Peer operation to perform", actions),
                ),
                (
                    "session_id",
                    bounded_integer_property(
                        "Target peer session id (from peers list). Required for wake_when_done; omit on send to broadcast to every live peer.",
                        Some(1),
                        None,
                    ),
                ),
                (
                    "message",
                    string_property(
                        "Message body for send (max 6000 characters — keep coordination chatter to a sentence or two; structured payloads like per-file review findings may use the space), or the note delivered to the target as a no-reply FYI with a wake_when_done request",
                    ),
                ),
                (
                    "claim",
                    string_property(
                        "Short claim label for claim/release (max 120 characters), e.g. 'running full test suite'",
                    ),
                ),
                (
                    "blocking_reason",
                    string_enum_property(
                        "Concrete blocker required for wake_when_done. Same-file overlap alone is not a blocker.",
                        &["edit_conflict", "broken_shared_state"],
                    ),
                ),
            ],
            &["action"],
        )
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        ParallelPolicy::Serialized
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        self.execute_operation(args, None).await
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: ToolExecutionContext,
    ) -> Result<ToolOutput> {
        let operation_key = peer_tool_operation_key(context.tool_call_id());
        self.execute_operation(args, Some(&operation_key)).await
    }
}

impl PeersTool {
    async fn execute_operation(
        &self,
        args: serde_json::Value,
        operation_key: Option<&str>,
    ) -> Result<ToolOutput> {
        let args: PeersArgs = parse_args("peers tool", args)?;
        let output = match args.action {
            PeerAction::List => {
                let peers = self.bus.overview().await?;
                self.render_peer_list(&peers)
            }
            PeerAction::Send => {
                let Some(body) = args
                    .message
                    .as_deref()
                    .map(str::trim)
                    .filter(|body| !body.is_empty())
                else {
                    bail!("message is required for peers action 'send'");
                };
                if body.chars().count() > PEER_MESSAGE_MAX_CHARS {
                    bail!("message exceeds {PEER_MESSAGE_MAX_CHARS} characters; shorten it");
                }
                let to = args.session_id.map(SessionId::from_raw);
                let recipients = match operation_key {
                    Some(operation_key) => {
                        self.bus.send_idempotent(to, body, operation_key).await?
                    }
                    None => self.bus.send(to, body).await?,
                };
                match recipients.as_slice() {
                    [only] => format!("Message sent to session #{only}."),
                    many => format!(
                        "Message broadcast to {} live peer sessions ({}).",
                        many.len(),
                        many.iter()
                            .map(|id| format!("#{id}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                }
            }
            PeerAction::WakeWhenDone => {
                let Some(target) = args.session_id.map(SessionId::from_raw) else {
                    bail!("session_id is required for peers action 'wake_when_done'");
                };
                // Defense in depth: this surface's schema omits the action,
                // but a model may still guess it.
                if !self.can_park {
                    return Ok(ToolOutput::Text(format!(
                        "wake_when_done is not available in this non-interactive run (the \
                         process cannot park a turn). Use peers send to leave session \
                         #{target} a message, or simply finish — peers watching this \
                         session are woken automatically when it exits."
                    )));
                }
                let note = args.message.as_deref().map(str::trim).unwrap_or_default();
                if note.chars().count() > PEER_MESSAGE_MAX_CHARS {
                    bail!("message exceeds {PEER_MESSAGE_MAX_CHARS} characters; shorten it");
                }

                // A lost-result retry must report the durable outcome before
                // today's live/idle snapshot can steer a brand-new wait away.
                if let Some(operation_key) = operation_key
                    && let Some(registration) = self
                        .bus
                        .replay_wake_when_done(target, note, operation_key)
                        .await?
                {
                    let peers = self.bus.live_peers().await?;
                    let snapshot = if let Some(peer) = peers.iter().find(|peer| peer.id == target) {
                        self.peer_status_snapshot(peer).await
                    } else {
                        format!(
                            "Peer status: session #{target} is no longer live; the previously committed wait remains authoritative."
                        )
                    };
                    return self.wake_registration_output(target, note, registration, snapshot);
                }

                if self.bus.is_peer_wake_turn() {
                    return Ok(ToolOutput::Text(format!(
                        "This turn was explicitly woken by a peer message, so it cannot park \
                         on wake_when_done again. Continue now using the new information. If \
                         you still need a condition-specific notification, use peers send to \
                         ask session #{target} to send you a message when that condition is \
                         met, then finish this turn; its message will wake you."
                    )));
                }

                if args.blocking_reason.is_none() {
                    return Ok(ToolOutput::Text(format!(
                        "wake_when_done requires blocking_reason: edit_conflict or \
                         broken_shared_state. Merely seeing session #{target} edit the same \
                         file is normal concurrency, not a blocker. Continue with a narrow \
                         edit; if freshness rejects it, re-read the current file and reapply \
                         only your hunk. Use peers send for useful coordination that does not \
                         require parking."
                    )));
                }

                // A brand-new wake fires only when the target's run ends. An
                // idle peer may never run again, so subscribing to it would
                // hang — and its changes are already settled anyway.
                let peers = self.bus.live_peers().await?;
                match peers.iter().find(|peer| peer.id == target) {
                    None => bail!(
                        "session #{target} is not a live peer in this project. Check the peer overview for current session ids."
                    ),
                    Some(peer) if !peer.working => {
                        let snapshot = self.peer_status_snapshot(peer).await;
                        format!(
                            "Session #{target} is idle — its run already finished, so its changes are settled. \
                             Re-read the affected files and continue now; there is nothing to wait for.\n\n{snapshot}"
                        )
                    }
                    Some(peer) => {
                        let registration = match operation_key {
                            Some(operation_key) => {
                                self.bus
                                    .wake_when_done_idempotent(target, note, operation_key)
                                    .await?
                            }
                            None => self.bus.wake_when_done(target, note).await?,
                        };
                        let snapshot = self.peer_status_snapshot(peer).await;
                        return self.wake_registration_output(target, note, registration, snapshot);
                    }
                }
            }
            PeerAction::Claim => {
                let claim = required_claim(args.claim.as_deref(), "claim")?;
                self.bus.claim(claim).await?;
                format!("Claimed \"{claim}\" — visible to every live peer. Release it when done.")
            }
            PeerAction::Release => {
                let claim = required_claim(args.claim.as_deref(), "release")?;
                if self.bus.release(claim).await? {
                    format!("Released \"{claim}\".")
                } else {
                    format!("No live claim \"{claim}\" to release.")
                }
            }
        };
        Ok(ToolOutput::Text(output))
    }
}

fn required_claim<'a>(claim: Option<&'a str>, action: &str) -> Result<&'a str> {
    let Some(claim) = claim.map(str::trim).filter(|value| !value.is_empty()) else {
        bail!("claim is required for peers action '{action}'");
    };
    Ok(claim)
}

/// `4m 07s`-style rendering for the status ack's run-duration clause.
fn format_run_duration(ms: i64) -> String {
    let secs = (ms / 1000).max(0);
    let mins = secs / 60;
    let rem = secs % 60;
    if mins > 0 {
        format!("{mins}m {rem:02}s")
    } else {
        format!("{rem}s")
    }
}

impl PeersTool {
    fn wake_registration_output(
        &self,
        target: SessionId,
        note: &str,
        registration: crate::storage::WakeSubscriptionRegistration,
        snapshot: String,
    ) -> Result<ToolOutput> {
        let outcome = registration.outcome;
        let state = match outcome {
            WakeSubscriptionOutcome::Created => "Waiting",
            WakeSubscriptionOutcome::AlreadyPending => "Already waiting",
            WakeSubscriptionOutcome::ReversePending => {
                return Ok(ToolOutput::Text(format!(
                    "Session #{target} is already parked waiting for YOUR run \
                     to finish — waiting on it would deadlock both sessions. \
                     Continue your work; #{target} wakes automatically with \
                     your done notice when this run ends.\n\n{snapshot}"
                )));
            }
        };
        let delivery = if note.is_empty() {
            "No note was sent; the peer can see this wait in its session state."
        } else if outcome == WakeSubscriptionOutcome::Created {
            "Your note was delivered to the target as an FYI (\"no reply needed\")."
        } else {
            "No new note was sent — you were already waiting on this session."
        };
        Ok(ToolOutput::WaitStarted {
            reason: WaitReason::Peer(crate::agent::PeerWait {
                session_id: target,
                subscription_id: registration.subscription_id.ok_or_else(|| {
                    anyhow::anyhow!("wake registration did not return a subscription identity")
                })?,
            }),
            message: format!(
                "{state} for session #{target} — this turn is parked; you \
                 resume automatically with a done notice when its run ends.\n\n\
                 {snapshot}\n\n{delivery}"
            ),
        })
    }

    /// The status ack `wake_when_done` returns before parking (and with its
    /// idle/deadlock refusals): everything the DB already knows about the
    /// target, so the handshake costs the peer zero model turns.
    async fn peer_status_snapshot(&self, peer: &crate::storage::PeerSessionSummary) -> String {
        let claims = self.bus.claims_for(peer.id).await.unwrap_or_default();
        let changed = self
            .bus
            .recent_file_changes(peer.id, 5)
            .await
            .unwrap_or_default();
        let title = if peer.summary.trim().is_empty() {
            "(no title)".to_string()
        } else {
            format!("\"{}\"", peer.summary.trim())
        };
        let state = if peer.working {
            match peer.run_started_at_ms {
                Some(started) => format!(
                    "working, current run active for {}",
                    format_run_duration(crate::util::time::now_ms().saturating_sub(started))
                ),
                None => "working".to_string(),
            }
        } else {
            "idle".to_string()
        };
        format!(
            "Peer status (synthesized from shared state; no peer turn was used):\n\
             - #{id} {title} — {state}\n\
             - recently changed: {changed}\n\
             - claims: {claims}",
            id = peer.id,
            changed = if changed.is_empty() {
                "none recorded".to_string()
            } else {
                changed.join(", ")
            },
            claims = if claims.is_empty() {
                "none".to_string()
            } else {
                claims.join(", ")
            },
        )
    }

    fn render_peer_list(&self, peers: &[PeerOverview]) -> String {
        if peers.is_empty() {
            return "No other live bonsai sessions in this project.".to_string();
        }
        let mut lines = vec![format!(
            "{} live peer session{} in this project:",
            peers.len(),
            if peers.len() == 1 { "" } else { "s" }
        )];
        for peer in peers.iter().take(5) {
            let title = if peer.title.trim().is_empty() {
                "(no title)".to_string()
            } else {
                format!("\"{}\"", peer.title.trim())
            };
            let changed = if peer.changed_files.is_empty() {
                "none recorded".to_string()
            } else {
                peer.changed_files.join(", ")
            };
            let claims = if peer.claims.is_empty() {
                String::new()
            } else {
                format!("; claims: {}", peer.claims.join(", "))
            };
            let waits = match (peer.waiting_on_peer, peer.peer_waiting_on_you) {
                (true, true) => "; you are waiting for it; it is waiting for you",
                (true, false) => "; you are waiting for this run",
                (false, true) => "; waiting for your run",
                (false, false) => "",
            };
            let state = if peer.working { "working" } else { "idle" };
            lines.push(format!(
                "#{} {title} — {state}{waits}; changed: {changed}{claims}",
                peer.id
            ));
        }
        if peers.len() > 5 {
            lines.push(format!("(+{} more)", peers.len() - 5));
        }
        lines.push(
            "Changed-file overlap is informational, not a lock. Keep working unless a \
             concrete conflict blocks a safe edit. Message one with peers send \
             (session_id: N) or all with peers send."
                .to_string(),
        );
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_utils::TestStorage;

    async fn tool_with_peer() -> (TestStorage, PeersTool, SessionId) {
        let fixture = TestStorage::new().await;
        let me = fixture.start_session().await;
        fixture
            .storage
            .record_session_heartbeat(me, false)
            .await
            .unwrap();
        let peer = fixture.start_session().await;
        fixture
            .storage
            .record_session_heartbeat(peer, false)
            .await
            .unwrap();
        let bus = Arc::new(PeerBus::new(
            fixture.storage.clone(),
            Arc::new(tokio::sync::Mutex::new(Some(me))),
            fixture.project_path().to_path_buf(),
        ));
        (fixture, PeersTool::new(bus, true), peer)
    }

    #[tokio::test]
    async fn list_shows_live_peer_with_changed_files() {
        let (fixture, tool, peer) = tool_with_peer().await;
        fixture
            .storage
            .record_session_file_changes(peer, &["src/a.rs".to_string()])
            .await
            .unwrap();

        let listed = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();

        let ToolOutput::Text(text) = listed else {
            panic!("list should return text");
        };
        assert!(text.contains(&format!("#{peer}")), "{text}");
        assert!(text.contains("src/a.rs"), "{text}");
    }

    #[tokio::test]
    async fn list_shows_working_vs_idle_state() {
        let (fixture, tool, peer) = tool_with_peer().await;

        // Peer heartbeats as mid-turn → working.
        fixture
            .storage
            .record_session_heartbeat(peer, true)
            .await
            .unwrap();
        let ToolOutput::Text(text) = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap()
        else {
            panic!("list should return text");
        };
        assert!(text.contains("working"), "{text}");

        // Turn finished → idle (a coordination signal for peers).
        fixture
            .storage
            .record_session_heartbeat(peer, false)
            .await
            .unwrap();
        let ToolOutput::Text(text) = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap()
        else {
            panic!("list should return text");
        };
        assert!(text.contains("idle"), "{text}");
    }

    #[tokio::test]
    async fn list_treats_changed_file_overlap_as_advisory() {
        let (_fixture, tool, _peer) = tool_with_peer().await;

        let ToolOutput::Text(text) = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap()
        else {
            panic!("list should return text");
        };

        assert!(text.contains("Changed-file overlap is informational, not a lock"));
    }

    #[tokio::test]
    async fn tool_description_reserves_waiting_for_concrete_interference() {
        let (_fixture, tool, _peer) = tool_with_peer().await;

        assert!(
            tool.description()
                .contains("Same-file overlap alone is normal")
        );
        assert!(
            tool.description()
                .contains("only after a concrete edit collision")
        );
    }

    #[tokio::test]
    async fn schema_exposes_only_concrete_wait_reasons() {
        let (_fixture, tool, _peer) = tool_with_peer().await;
        let schema = tool.parameters_schema();

        assert_eq!(
            schema["properties"]["blocking_reason"]["enum"],
            serde_json::json!(["edit_conflict", "broken_shared_state"])
        );
    }

    #[tokio::test]
    async fn wake_without_concrete_reason_keeps_agent_working() {
        let (fixture, tool, peer) = tool_with_peer().await;
        fixture
            .storage
            .record_session_heartbeat(peer, true)
            .await
            .unwrap();

        let reply = tool
            .execute(serde_json::json!({
                "action": "wake_when_done",
                "session_id": peer.as_i64(),
                "message": "you are also editing this file",
            }))
            .await
            .unwrap();

        assert!(
            matches!(&reply, ToolOutput::Text(text)
                if text.contains("same file is normal concurrency")
                    && text.contains("Continue with a narrow edit")),
            "same-file overlap must not park the agent: {reply:?}"
        );
        assert!(
            fixture
                .storage
                .fire_wake_subscriptions(peer)
                .await
                .unwrap()
                .is_empty(),
            "a refused overlap-only wait must not create a subscription"
        );
    }

    #[tokio::test]
    async fn send_delivers_to_target_and_echoes() {
        let (fixture, tool, peer) = tool_with_peer().await;

        let sent = tool
            .execute(serde_json::json!({
                "action": "send",
                "session_id": peer.as_i64(),
                "message": "wake me when you are done; I'll validate our work",
            }))
            .await
            .unwrap();

        assert!(matches!(sent, ToolOutput::Text(text) if text.contains(&format!("#{peer}"))));
        let inbox = fixture
            .storage
            .claim_ui_undelivered_messages(peer)
            .await
            .unwrap();
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].body.contains("validate our work"));
        // The outgoing echo renders the "you → #N" blue message at the send site.
        let echoes = tool.bus.drain_echoes();
        assert_eq!(echoes.len(), 1);
        assert_eq!(echoes[0].to_session_id, peer);
    }

    #[tokio::test]
    async fn repeated_tool_call_id_does_not_duplicate_send_or_echo() {
        let (fixture, tool, peer) = tool_with_peer().await;
        let args = serde_json::json!({
            "action": "send",
            "session_id": peer.as_i64(),
            "message": "one durable operation",
        });
        let sink: crate::output::SharedSink = Arc::new(crate::output::StdoutSink);

        tool.execute_with_context(
            args.clone(),
            ToolExecutionContext::new("peer-call-1".to_string(), sink.clone()),
        )
        .await
        .unwrap();
        assert_eq!(tool.bus.drain_echoes().len(), 1);

        tool.execute_with_context(
            args,
            ToolExecutionContext::new("peer-call-1".to_string(), sink),
        )
        .await
        .unwrap();
        assert!(tool.bus.drain_echoes().is_empty());
        assert_eq!(
            fixture
                .storage
                .claim_ui_undelivered_messages(peer)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn wake_when_done_delivers_fyi_and_parks_with_status_ack() {
        let (fixture, tool, peer) = tool_with_peer().await;
        // A wake is only meaningful for a peer that is still working (its run
        // will end and fire the subscription).
        fixture
            .storage
            .record_session_heartbeat(peer, true)
            .await
            .unwrap();
        fixture
            .storage
            .record_session_file_changes(peer, &["src/tui/layout.rs".to_string()])
            .await
            .unwrap();

        let reply = tool
            .execute(serde_json::json!({
                "action": "wake_when_done",
                "session_id": peer.as_i64(),
                "message": "I'll validate our work and let you know",
                "blocking_reason": "edit_conflict",
            }))
            .await
            .unwrap();
        let ToolOutput::WaitStarted {
            reason: WaitReason::Peer(waiting_for),
            message,
        } = &reply
        else {
            panic!("wake_when_done on a working peer must park, got: {reply:?}");
        };
        assert_eq!(waiting_for.session_id, peer);
        assert!(waiting_for.subscription_id > 0);
        // The status ack: synthesized from shared state, no peer turn spent.
        assert!(message.contains("no peer turn was used"), "{message}");
        assert!(message.contains("working"), "{message}");
        assert!(message.contains("src/tui/layout.rs"), "{message}");
        assert!(message.contains("delivered to the target"), "{message}");

        // The ask is real: exactly one wake_request FYI landed at the target.
        let inbox = fixture
            .storage
            .claim_ui_undelivered_messages(peer)
            .await
            .unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].kind, crate::storage::PeerMessageKind::WakeRequest);
        assert!(
            inbox[0].body.contains("no reply needed"),
            "{}",
            inbox[0].body
        );
        assert!(
            inbox[0].body.contains("I'll validate our work"),
            "{}",
            inbox[0].body
        );
        // …and echoes as a blue outgoing message at the send site.
        let echoes = tool.bus.drain_echoes();
        assert_eq!(echoes.len(), 1);
        assert_eq!(echoes[0].to_session_id, peer);

        let relationships = tool.bus.wake_relationships().await.unwrap();
        assert_eq!(relationships.waiting_on, vec![peer]);

        // A repeated wait re-parks but must not spam a second FYI.
        let duplicate = tool
            .execute(serde_json::json!({
                "action": "wake_when_done",
                "session_id": peer.as_i64(),
                "message": "duplicate",
                "blocking_reason": "edit_conflict",
            }))
            .await
            .unwrap();
        assert!(matches!(
            &duplicate,
            ToolOutput::WaitStarted { message, .. }
                if message.contains("Already waiting") && message.contains("No new note")
        ));
        assert!(
            fixture
                .storage
                .claim_ui_undelivered_messages(peer)
                .await
                .unwrap()
                .is_empty(),
            "a repeated wait must not resend the FYI"
        );
        assert!(tool.bus.drain_echoes().is_empty());

        // …and the subscription fires exactly once when the target finishes.
        let notified = fixture.storage.fire_wake_subscriptions(peer).await.unwrap();
        assert_eq!(notified.len(), 1);

        let missing_target = tool
            .execute(serde_json::json!({"action": "wake_when_done"}))
            .await
            .expect_err("wake_when_done requires session_id");
        assert!(
            missing_target
                .to_string()
                .contains("session_id is required")
        );
    }

    #[tokio::test]
    async fn repeated_wake_tool_call_reports_committed_wait_after_target_finishes() {
        let (fixture, tool, peer) = tool_with_peer().await;
        fixture
            .storage
            .record_session_heartbeat(peer, true)
            .await
            .unwrap();
        let args = serde_json::json!({
            "action": "wake_when_done",
            "session_id": peer.as_i64(),
            "message": "finish once",
            "blocking_reason": "broken_shared_state",
        });
        let sink: crate::output::SharedSink = Arc::new(crate::output::StdoutSink);

        let first = tool
            .execute_with_context(
                args.clone(),
                ToolExecutionContext::new("wake-call-1".to_string(), sink.clone()),
            )
            .await
            .unwrap();
        let ToolOutput::WaitStarted {
            reason: WaitReason::Peer(first_wait),
            ..
        } = first
        else {
            panic!("first call must park");
        };
        assert_eq!(tool.bus.drain_echoes().len(), 1);

        fixture.storage.fire_wake_subscriptions(peer).await.unwrap();
        fixture
            .storage
            .record_session_heartbeat(peer, false)
            .await
            .unwrap();
        let replay = tool
            .execute_with_context(
                args,
                ToolExecutionContext::new("wake-call-1".to_string(), sink),
            )
            .await
            .unwrap();
        let ToolOutput::WaitStarted {
            reason: WaitReason::Peer(replayed_wait),
            ..
        } = replay
        else {
            panic!("a committed wait replay must still report WaitStarted");
        };
        assert_eq!(replayed_wait, first_wait);
        assert!(tool.bus.drain_echoes().is_empty());
        assert!(
            tool.bus
                .wake_relationships()
                .await
                .unwrap()
                .waiting_on
                .is_empty()
        );
    }

    #[tokio::test]
    async fn wake_when_done_without_note_parks_silently() {
        let (fixture, tool, peer) = tool_with_peer().await;
        fixture
            .storage
            .record_session_heartbeat(peer, true)
            .await
            .unwrap();

        let reply = tool
            .execute(serde_json::json!({
                "action": "wake_when_done",
                "session_id": peer.as_i64(),
                "blocking_reason": "edit_conflict",
            }))
            .await
            .unwrap();
        assert!(matches!(
            &reply,
            ToolOutput::WaitStarted { message, .. } if message.contains("No note was sent")
        ));
        assert!(
            fixture
                .storage
                .claim_ui_undelivered_messages(peer)
                .await
                .unwrap()
                .is_empty(),
            "an empty note must not produce an FYI message"
        );
        assert!(tool.bus.drain_echoes().is_empty());
    }

    #[tokio::test]
    async fn wake_when_done_refuses_when_target_already_waiting_on_us() {
        let (fixture, tool, peer) = tool_with_peer().await;
        fixture
            .storage
            .record_session_heartbeat(peer, true)
            .await
            .unwrap();
        // The peer is already parked waiting on us.
        let me = tool.bus.self_id().await.unwrap();
        fixture
            .storage
            .add_wake_subscription(fixture.project_path(), peer, me, "", 0)
            .await
            .unwrap();

        let reply = tool
            .execute(serde_json::json!({
                "action": "wake_when_done",
                "session_id": peer.as_i64(),
                "blocking_reason": "edit_conflict",
            }))
            .await
            .unwrap();
        assert!(
            matches!(&reply, ToolOutput::Text(text)
                if text.contains("waiting for YOUR run") && text.contains("deadlock")),
            "a reverse-pending pair must refuse to park, got: {reply:?}"
        );
        assert!(
            tool.bus
                .wake_relationships()
                .await
                .unwrap()
                .waiting_on
                .is_empty(),
            "the refused wait must not be stored"
        );
    }

    #[tokio::test]
    async fn wake_when_done_refused_without_park_capability() {
        let (fixture, tool, peer) = tool_with_peer().await;
        fixture
            .storage
            .record_session_heartbeat(peer, true)
            .await
            .unwrap();
        let no_park = PeersTool::new(tool.bus.clone(), false);

        // The schema must not advertise the action…
        let schema = no_park.parameters_schema();
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(
            !actions.contains(&"wake_when_done".to_string()),
            "{actions:?}"
        );
        assert!(actions.contains(&"send".to_string()), "{actions:?}");
        assert!(!no_park.description().contains("wake_when_done delivers"));

        // …and a guessed call is refused without parking or subscribing.
        let reply = no_park
            .execute(serde_json::json!({
                "action": "wake_when_done",
                "session_id": peer.as_i64(),
                "blocking_reason": "broken_shared_state",
            }))
            .await
            .unwrap();
        assert!(
            matches!(&reply, ToolOutput::Text(text) if text.contains("cannot park")),
            "got: {reply:?}"
        );
        assert!(
            fixture
                .storage
                .fire_wake_subscriptions(peer)
                .await
                .unwrap()
                .is_empty(),
            "no subscription may be registered on a non-parking surface"
        );
    }

    #[tokio::test]
    async fn peer_woken_turn_cannot_park_again() {
        let (fixture, tool, peer) = tool_with_peer().await;
        fixture
            .storage
            .record_session_heartbeat(peer, true)
            .await
            .unwrap();
        tool.bus
            .begin_turn(crate::peer::TurnOrigin::PeerWake { hop: 0 });

        let reply = tool
            .execute(serde_json::json!({
                "action": "wake_when_done",
                "session_id": peer.as_i64(),
                "blocking_reason": "edit_conflict",
            }))
            .await
            .unwrap();
        assert!(
            matches!(&reply, ToolOutput::Text(text)
                if text.contains("cannot park")
                    && text.contains("use peers send")
                    && text.contains("message will wake you")),
            "a peer-woken turn must steer to a condition-specific send, got: {reply:?}"
        );
        assert!(
            fixture
                .storage
                .fire_wake_subscriptions(peer)
                .await
                .unwrap()
                .is_empty(),
            "the refused repeated wait must not leave a subscription"
        );
    }

    #[tokio::test]
    async fn wake_when_done_on_idle_peer_steers_to_proceed_without_subscribing() {
        // The fixture peer heartbeats as idle. Waiting on it would hang (no
        // future run to fire the wake), so the tool must steer the agent to
        // proceed instead of registering a subscription.
        let (fixture, tool, peer) = tool_with_peer().await;

        let reply = tool
            .execute(serde_json::json!({
                "action": "wake_when_done",
                "session_id": peer.as_i64(),
                "blocking_reason": "edit_conflict",
            }))
            .await
            .unwrap();
        assert!(
            matches!(&reply, ToolOutput::Text(text)
                if text.contains("idle")
                    && text.contains("continue now")
                    && text.contains("Peer status")),
            "an idle target must steer the agent to proceed with a status snapshot, got: {reply:?}"
        );

        // No subscription was registered and no peer message was sent.
        let notified = fixture.storage.fire_wake_subscriptions(peer).await.unwrap();
        assert!(
            notified.is_empty(),
            "no wait should be registered for an idle peer"
        );
        let inbox = fixture
            .storage
            .claim_ui_undelivered_messages(peer)
            .await
            .unwrap();
        assert!(inbox.is_empty(), "no peer message for a non-wait");
    }

    #[tokio::test]
    async fn overlong_wake_note_does_not_subscribe() {
        let (fixture, tool, peer) = tool_with_peer().await;
        // Working peer, so the request reaches note validation.
        fixture
            .storage
            .record_session_heartbeat(peer, true)
            .await
            .unwrap();
        let note = "x".repeat(PEER_MESSAGE_MAX_CHARS + 1);
        let err = tool
            .execute(serde_json::json!({
                "action": "wake_when_done",
                "session_id": peer.as_i64(),
                "message": note,
                "blocking_reason": "broken_shared_state",
            }))
            .await
            .expect_err("wake note should exceed the peer body cap");

        assert!(err.to_string().contains("exceeds"), "{err:#}");
        assert!(
            fixture
                .storage
                .fire_wake_subscriptions(peer)
                .await
                .unwrap()
                .is_empty(),
            "failed wake request must not leave an armed subscription"
        );
        assert!(
            fixture
                .storage
                .claim_ui_undelivered_messages(peer)
                .await
                .unwrap()
                .is_empty(),
            "failed wake must not deliver a visible message"
        );
    }

    #[tokio::test]
    async fn claim_and_release_round_trip_and_show_in_list() {
        let (fixture, tool, peer) = tool_with_peer().await;

        // The PEER claims the test suite; our list must show it.
        fixture
            .storage
            .add_peer_claim(fixture.project_path(), peer, "running full test suite")
            .await
            .unwrap();
        let listed = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(
            matches!(&listed, ToolOutput::Text(text) if text.contains("running full test suite")),
            "peer claims must be visible in list"
        );

        // Our own claim/release verbs round-trip.
        let claimed = tool
            .execute(serde_json::json!({"action": "claim", "claim": "owns src/tui/"}))
            .await
            .unwrap();
        assert!(matches!(&claimed, ToolOutput::Text(text) if text.contains("Claimed")));
        let released = tool
            .execute(serde_json::json!({"action": "release", "claim": "owns src/tui/"}))
            .await
            .unwrap();
        assert!(matches!(&released, ToolOutput::Text(text) if text.contains("Released")));
        let missing = tool
            .execute(serde_json::json!({"action": "release", "claim": "owns src/tui/"}))
            .await
            .unwrap();
        assert!(matches!(&missing, ToolOutput::Text(text) if text.contains("No live claim")));

        let no_claim = tool
            .execute(serde_json::json!({"action": "claim"}))
            .await
            .expect_err("claim requires text");
        assert!(no_claim.to_string().contains("claim is required"));
    }

    #[tokio::test]
    async fn send_requires_message_and_live_target() {
        let (_fixture, tool, _peer) = tool_with_peer().await;

        let missing = tool
            .execute(serde_json::json!({"action": "send", "session_id": 1}))
            .await
            .expect_err("send without message must fail");
        assert!(missing.to_string().contains("message is required"));

        let dead = tool
            .execute(serde_json::json!({
                "action": "send",
                "session_id": 9_999,
                "message": "hello?",
            }))
            .await
            .expect_err("send to a non-live session must fail");
        assert!(dead.to_string().contains("not a live peer"), "{dead}");
    }
}
