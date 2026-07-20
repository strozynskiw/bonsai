use super::*;

impl Storage {
    #[cfg(test)]
    pub(crate) async fn replace_agent_context_snapshot(
        &self,
        session_id: SessionId,
        context: &ContextMessageSnapshot,
        read_evidence: &[ReadEvidenceRecord],
        inspection_events: &[InspectionEventRecord],
        peer_delivery_receipts: &[PeerDeliveryReceipt],
    ) -> Result<()> {
        self.with_session_snapshot_tx("agent context snapshot", async move |tx, now| {
            self.replace_context_snapshot_in_tx(tx, session_id, context, now)
                .await?;
            self.replace_read_evidence_snapshot_in_tx(tx, session_id, read_evidence, now)
                .await?;
            self.replace_inspection_events_snapshot_in_tx(tx, session_id, inspection_events, now)
                .await?;
            self.acknowledge_peer_deliveries_in_tx(
                tx,
                session_id,
                PeerDeliveryConsumer::Agent,
                peer_delivery_receipts,
                now,
            )
            .await
        })
        .await
    }

    pub(crate) async fn replace_context_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        snapshot: &ContextMessageSnapshot,
        now: i64,
    ) -> Result<()> {
        sqlx::query("DELETE FROM context_messages WHERE session_id = ?")
            .bind(session_id.as_i64())
            .execute(&mut **tx)
            .await
            .context("Failed to delete context messages")?;

        for (seq, message) in snapshot.messages.iter().enumerate() {
            let raw_json =
                serde_json::to_string(message).context("Failed to serialize context message")?;
            let stable_id = snapshot
                .ids
                .get(seq)
                .cloned()
                .unwrap_or_else(|| format!("msg-{seq}"));
            sqlx::query(
                r#"
                INSERT INTO context_messages (
                  session_id, seq, stable_id, raw_json
                )
                VALUES (?, ?, ?, ?)
                "#,
            )
            .bind(session_id.as_i64())
            .bind(seq as i64)
            .bind(stable_id)
            .bind(raw_json)
            .execute(&mut **tx)
            .await?;
        }

        touch_session(tx, session_id, now).await
    }

    async fn reconcile_session_no_cache_cost_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
    ) -> Result<()> {
        // IMPORTANT ACCOUNTING INVARIANT: the per-turn ledger is authoritative
        // only when it completely prices the session's recorded actual cost.
        // Do not weaken these HAVING guards: a partial sum can be lower than
        // actual cost and is no safer than the stale aggregate it would replace.
        sqlx::query(
            r#"
            UPDATE sessions
            SET no_cache_cost_micros = COALESCE(
                  (SELECT SUM(no_cache_cost_micros)
                   FROM usage_turns
                   WHERE session_id = ? AND turn_cost_micros IS NOT NULL
                   HAVING COUNT(*) > 0
                     AND COUNT(no_cache_cost_micros) = COUNT(*)
                     AND MIN(turn_cost_micros) >= 0
                     AND MIN(no_cache_cost_micros) >= 0
                     AND SUM(turn_cost_micros) = sessions.cost_micros),
                  no_cache_cost_micros
                )
            WHERE id = ?
            "#,
        )
        .bind(session_id.as_i64())
        .bind(session_id.as_i64())
        .execute(&mut **tx)
        .await
        .with_context(|| format!("Failed to reconcile no-cache cost for session {session_id}"))?;
        Ok(())
    }

    #[cfg(test)]
    pub async fn replace_context_control_snapshot(
        &self,
        session_id: SessionId,
        controls: &HashMap<String, ContextControlState>,
        sources: &HashMap<String, Vec<ChatCompletionRequestMessage>>,
    ) -> Result<()> {
        self.with_session_snapshot_tx("context control snapshot", async move |tx, now| {
            self.replace_context_control_snapshot_in_tx(tx, session_id, controls, sources, now)
                .await
        })
        .await
    }

    pub(crate) async fn replace_context_control_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        controls: &HashMap<String, ContextControlState>,
        sources: &HashMap<String, Vec<ChatCompletionRequestMessage>>,
        now: i64,
    ) -> Result<()> {
        sqlx::query("DELETE FROM context_controls WHERE session_id = ?")
            .bind(session_id.as_i64())
            .execute(&mut **tx)
            .await
            .context("Failed to delete context controls")?;
        sqlx::query("DELETE FROM context_sources WHERE session_id = ?")
            .bind(session_id.as_i64())
            .execute(&mut **tx)
            .await
            .context("Failed to delete context sources")?;

        for (node_id, state) in controls {
            if !state.is_active() {
                continue;
            }
            let state_json =
                serde_json::to_string(state).context("Failed to serialize context control")?;
            sqlx::query(
                r#"
                INSERT INTO context_controls (
                  session_id, node_id, state_json
                )
                VALUES (?, ?, ?)
                "#,
            )
            .bind(session_id.as_i64())
            .bind(node_id)
            .bind(state_json)
            .execute(&mut **tx)
            .await?;
        }

        for (node_id, messages) in sources {
            let messages_json = serde_json::to_string(messages)
                .context("Failed to serialize context source messages")?;
            sqlx::query(
                r#"
                INSERT INTO context_sources (
                  session_id, node_id, messages_json
                )
                VALUES (?, ?, ?)
                "#,
            )
            .bind(session_id.as_i64())
            .bind(node_id)
            .bind(messages_json)
            .execute(&mut **tx)
            .await?;
        }

        touch_session(tx, session_id, now).await
    }
    pub(super) async fn load_context_messages(
        &self,
        session_id: SessionId,
    ) -> Result<ContextMessageSnapshot> {
        let rows =
            sqlx::query(
                "SELECT seq, stable_id, raw_json FROM context_messages WHERE session_id = ? ORDER BY seq",
            )
                .bind(session_id.as_i64())
                .fetch_all(&self.pool)
                .await
                .with_context(|| {
                    format!("Failed to load context messages for session {session_id}")
                })?;

        let mut messages = Vec::with_capacity(rows.len());
        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            let seq: i64 = row.try_get("seq")?;
            let stable_id: String = row.try_get("stable_id")?;
            let raw_json: String = row.try_get("raw_json")?;
            let mut message = serde_json::from_str(&raw_json)
                .context("Failed to parse persisted context message")?;
            crate::util::tool_args::normalize_chat_message_tool_call_arguments(&mut message);
            messages.push(message);
            ids.push(if stable_id.trim().is_empty() {
                format!("msg-{seq}")
            } else {
                stable_id
            });
        }
        Ok(ContextMessageSnapshot { messages, ids })
    }

    pub(super) async fn load_context_controls(
        &self,
        session_id: SessionId,
    ) -> Result<HashMap<String, ContextControlState>> {
        let rows = sqlx::query(
            "SELECT node_id, state_json FROM context_controls WHERE session_id = ? ORDER BY node_id",
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load context controls for session {session_id}"))?;

        rows.into_iter()
            .map(|row| {
                let node_id: String = row.try_get("node_id")?;
                let state_json: String = row.try_get("state_json")?;
                let state = serde_json::from_str(&state_json)
                    .context("Failed to parse persisted context control")?;
                Ok((node_id, state))
            })
            .collect()
    }

    #[cfg(test)]
    pub async fn replace_compaction_events_snapshot(
        &self,
        session_id: SessionId,
        events: &[CompactionEvent],
    ) -> Result<()> {
        self.with_session_snapshot_tx("compaction events snapshot", async move |tx, now| {
            self.replace_compaction_events_snapshot_in_tx(tx, session_id, events, now)
                .await
        })
        .await
    }

    pub(crate) async fn replace_compaction_events_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        events: &[CompactionEvent],
        now: i64,
    ) -> Result<()> {
        sqlx::query("DELETE FROM compaction_events WHERE session_id = ?")
            .bind(session_id.as_i64())
            .execute(&mut **tx)
            .await
            .context("Failed to delete compaction events")?;

        for event in events {
            sqlx::query(
                r#"
                INSERT INTO compaction_events (
                  session_id, seq, occurred_at_ms, before_tokens, after_tokens,
                  messages_omitted, tool_outputs_stubbed, summary_available,
                  repack_id, repack_reason, prefix_hash_before, prefix_hash_after,
                  cacheable_prefix_tokens_before, cacheable_prefix_tokens_after
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(session_id.as_i64())
            .bind(event.seq as i64)
            .bind(event.occurred_at_ms)
            .bind(event.before_tokens as i64)
            .bind(event.after_tokens as i64)
            .bind(event.messages_omitted as i64)
            .bind(event.tool_outputs_stubbed as i64)
            .bind(i64::from(event.summary_available))
            .bind(event.repack_id.as_deref().unwrap_or(""))
            .bind(event.repack_reason.as_deref().unwrap_or(""))
            .bind(event.prefix_hash_before.as_deref().unwrap_or(""))
            .bind(event.prefix_hash_after.as_deref().unwrap_or(""))
            .bind(
                event
                    .cacheable_prefix_tokens_before
                    .map(|tokens| tokens as i64),
            )
            .bind(
                event
                    .cacheable_prefix_tokens_after
                    .map(|tokens| tokens as i64),
            )
            .execute(&mut **tx)
            .await?;
        }

        touch_session(tx, session_id, now).await
    }

    pub(super) async fn load_compaction_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<CompactionEvent>> {
        let rows = sqlx::query(
            "SELECT seq, occurred_at_ms, before_tokens, after_tokens, messages_omitted, \
             tool_outputs_stubbed, summary_available, repack_id, repack_reason, \
             prefix_hash_before, prefix_hash_after, cacheable_prefix_tokens_before, \
             cacheable_prefix_tokens_after \
             FROM compaction_events WHERE session_id = ? ORDER BY seq",
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load compaction events for session {session_id}"))?;

        rows.into_iter()
            .map(|row| {
                Ok(CompactionEvent {
                    seq: row.try_get::<i64, _>("seq")? as usize,
                    occurred_at_ms: row.try_get("occurred_at_ms")?,
                    before_tokens: row.try_get::<i64, _>("before_tokens")? as usize,
                    after_tokens: row.try_get::<i64, _>("after_tokens")? as usize,
                    messages_omitted: row.try_get::<i64, _>("messages_omitted")? as usize,
                    tool_outputs_stubbed: row.try_get::<i64, _>("tool_outputs_stubbed")? as usize,
                    summary_available: row.try_get::<i64, _>("summary_available")? != 0,
                    repack_id: non_empty_string(row.try_get("repack_id")?),
                    repack_reason: non_empty_string(row.try_get("repack_reason")?),
                    prefix_hash_before: non_empty_string(row.try_get("prefix_hash_before")?),
                    prefix_hash_after: non_empty_string(row.try_get("prefix_hash_after")?),
                    cacheable_prefix_tokens_before: row
                        .try_get::<Option<i64>, _>("cacheable_prefix_tokens_before")?
                        .map(|tokens| tokens as usize),
                    cacheable_prefix_tokens_after: row
                        .try_get::<Option<i64>, _>("cacheable_prefix_tokens_after")?
                        .map(|tokens| tokens as usize),
                })
            })
            .collect()
    }

    pub async fn sync_usage_turns_snapshot(
        &self,
        session_id: SessionId,
        turns: &[UsageTurn],
    ) -> Result<()> {
        self.with_session_snapshot_tx("usage turns snapshot", async move |tx, now| {
            self.sync_usage_turns_snapshot_in_tx(tx, session_id, turns, now)
                .await
        })
        .await
    }

    pub(crate) async fn sync_usage_turns_snapshot_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        turns: &[UsageTurn],
        now: i64,
    ) -> Result<()> {
        let persisted_max_seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) FROM usage_turns WHERE session_id = ?",
        )
        .bind(session_id.as_i64())
        .fetch_one(&mut **tx)
        .await
        .context("Failed to inspect persisted usage turns")?;

        for turn in turns {
            if usize_to_i64(turn.seq) <= persisted_max_seq {
                self.update_mutable_usage_turn_fields_in_tx(tx, session_id, turn)
                    .await?;
                continue;
            }
            let provider_attempts_json = serde_json::to_string(&turn.provider_attempts)
                .context("Failed to serialize provider attempt diagnostics")?;
            sqlx::query(
                r#"
                INSERT INTO usage_turns (
                  session_id, seq, lane_kind, lane_id, lane_seq,
                  parent_tool_call_id, launch_group_id, status, finish_reason,
                  reasoning_chars, provider_attempts_json, provider_id, model,
                  effective_reasoning,
                  prompt_tokens, completion_tokens,
                  cache_read_input_tokens, cache_creation_input_tokens,
                  cache_measured_input_tokens, turn_cost_micros, no_cache_cost_micros,
                  estimated_prompt_tokens, estimate_source, estimate_confidence,
                  tool_schema_tokens, tool_schema_hash, tool_schema_names_json,
                  request_body_bytes, request_body_hash, cache_mechanism,
                  cache_route_fingerprint,
                  expected_cacheable_percent, actual_cache_read_percent,
                  local_reusable_prefix_tokens, local_reusable_prefix_percent,
                  cacheable_prefix_tokens, volatile_tail_tokens,
                  context_window_tokens, rewrite_kind, rewrite_saved_tokens,
                  episode_seq,
                  created_at_ms, latency_ms, ttft_ms, prefix_hash,
                  inspection_executed, inspection_reused, inspection_rejected,
                  inspection_returned_chars, inspection_avoided_chars,
                  delegated_parent_overlap
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                "#,
            )
            .bind(session_id.as_i64())
            .bind(usize_to_i64(turn.seq))
            .bind(turn.lane_kind.as_db_str())
            .bind(&turn.lane_id)
            .bind(usize_to_i64(turn.lane_seq))
            .bind(turn.parent_tool_call_id.as_deref())
            .bind(turn.launch_group_id.as_deref())
            .bind(turn.status.as_db_str())
            .bind(turn.finish_reason.as_ref().map(|reason| reason.as_db_str()))
            .bind(usize_to_i64(turn.reasoning_chars))
            .bind(provider_attempts_json)
            .bind(turn.provider_id.as_deref())
            .bind(turn.model.as_deref())
            .bind(turn.effective_reasoning.map(|reasoning| reasoning.label()))
            .bind(optional_u64_to_i64(turn.prompt_tokens))
            .bind(optional_u64_to_i64(turn.completion_tokens))
            .bind(optional_u64_to_i64(turn.cache_read_input_tokens))
            .bind(optional_u64_to_i64(turn.cache_creation_input_tokens))
            .bind(optional_u64_to_i64(turn.cache_measured_input_tokens))
            .bind(optional_u64_to_i64(turn.turn_cost_micros))
            .bind(optional_u64_to_i64(turn.no_cache_cost_micros))
            .bind(turn.estimated_prompt_tokens.map(usize_to_i64))
            .bind(turn.estimate_source.map(|source| source.as_db_str()))
            .bind(
                turn.estimate_confidence
                    .map(|confidence| confidence.as_db_str()),
            )
            .bind(turn.tool_schema_tokens.map(usize_to_i64))
            .bind(turn.tool_schema_hash.as_deref())
            .bind(tool_schema_names_json(&turn.tool_schema_names))
            .bind(turn.request_body_bytes.map(usize_to_i64))
            .bind(turn.request_body_hash.as_deref())
            .bind(turn.cache_mechanism.as_deref())
            .bind(turn.cache_route_fingerprint.as_deref())
            .bind(optional_u64_to_i64(turn.expected_cacheable_percent))
            .bind(optional_u64_to_i64(turn.actual_cache_read_percent))
            .bind(turn.local_reusable_prefix_tokens.map(usize_to_i64))
            .bind(optional_u64_to_i64(turn.local_reusable_prefix_percent))
            .bind(turn.cacheable_prefix_tokens.map(usize_to_i64))
            .bind(turn.volatile_tail_tokens.map(usize_to_i64))
            .bind(turn.context_window_tokens.map(usize_to_i64))
            .bind(turn.rewrite_kind.as_db_str())
            .bind(turn.rewrite_saved_tokens.map(usize_to_i64))
            .bind(turn.episode_seq.map(usize_to_i64))
            // Real per-turn timestamp when stamped; fall back to the flush
            // time for legacy/unstamped rows so ordering never regresses.
            .bind(if turn.created_at_ms > 0 {
                turn.created_at_ms
            } else {
                now
            })
            .bind(optional_u64_to_i64(turn.latency_ms))
            .bind(optional_u64_to_i64(turn.ttft_ms))
            .bind(turn.prefix_hash.as_deref())
            .bind(usize_to_i64(turn.inspection_executed))
            .bind(usize_to_i64(turn.inspection_reused))
            .bind(usize_to_i64(turn.inspection_rejected))
            .bind(usize_to_i64(turn.inspection_returned_chars))
            .bind(usize_to_i64(turn.inspection_avoided_chars))
            .bind(usize_to_i64(turn.delegated_parent_overlap))
            .execute(&mut **tx)
            .await?;
        }

        self.reconcile_session_no_cache_cost_in_tx(tx, session_id)
            .await?;
        touch_session(tx, session_id, now).await
    }

    async fn update_mutable_usage_turn_fields_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        turn: &UsageTurn,
    ) -> Result<()> {
        sqlx::query(
            r#"
            WITH incoming (
              parent_tool_call_id, launch_group_id,
              cache_read_input_tokens, cache_creation_input_tokens,
              cache_measured_input_tokens, actual_cache_read_percent,
              inspection_executed, inspection_reused, inspection_rejected,
              inspection_returned_chars, inspection_avoided_chars,
              delegated_parent_overlap
            ) AS (VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?))
            UPDATE usage_turns
            SET parent_tool_call_id = incoming.parent_tool_call_id,
                launch_group_id = incoming.launch_group_id,
                cache_read_input_tokens = incoming.cache_read_input_tokens,
                cache_creation_input_tokens = incoming.cache_creation_input_tokens,
                cache_measured_input_tokens = incoming.cache_measured_input_tokens,
                actual_cache_read_percent = incoming.actual_cache_read_percent,
                inspection_executed = incoming.inspection_executed,
                inspection_reused = incoming.inspection_reused,
                inspection_rejected = incoming.inspection_rejected,
                inspection_returned_chars = incoming.inspection_returned_chars,
                inspection_avoided_chars = incoming.inspection_avoided_chars,
                delegated_parent_overlap = incoming.delegated_parent_overlap
            FROM incoming
            WHERE usage_turns.session_id = ? AND usage_turns.seq = ?
              AND (
                usage_turns.parent_tool_call_id IS NOT incoming.parent_tool_call_id OR
                usage_turns.launch_group_id IS NOT incoming.launch_group_id OR
                usage_turns.cache_read_input_tokens IS NOT incoming.cache_read_input_tokens OR
                usage_turns.cache_creation_input_tokens IS NOT incoming.cache_creation_input_tokens OR
                usage_turns.cache_measured_input_tokens IS NOT incoming.cache_measured_input_tokens OR
                usage_turns.actual_cache_read_percent IS NOT incoming.actual_cache_read_percent OR
                usage_turns.inspection_executed IS NOT incoming.inspection_executed OR
                usage_turns.inspection_reused IS NOT incoming.inspection_reused OR
                usage_turns.inspection_rejected IS NOT incoming.inspection_rejected OR
                usage_turns.inspection_returned_chars IS NOT incoming.inspection_returned_chars OR
                usage_turns.inspection_avoided_chars IS NOT incoming.inspection_avoided_chars OR
                usage_turns.delegated_parent_overlap IS NOT incoming.delegated_parent_overlap
              )
            "#,
        )
        .bind(turn.parent_tool_call_id.as_deref())
        .bind(turn.launch_group_id.as_deref())
        .bind(optional_u64_to_i64(turn.cache_read_input_tokens))
        .bind(optional_u64_to_i64(turn.cache_creation_input_tokens))
        .bind(optional_u64_to_i64(turn.cache_measured_input_tokens))
        .bind(optional_u64_to_i64(turn.actual_cache_read_percent))
        .bind(usize_to_i64(turn.inspection_executed))
        .bind(usize_to_i64(turn.inspection_reused))
        .bind(usize_to_i64(turn.inspection_rejected))
        .bind(usize_to_i64(turn.inspection_returned_chars))
        .bind(usize_to_i64(turn.inspection_avoided_chars))
        .bind(usize_to_i64(turn.delegated_parent_overlap))
        .bind(session_id.as_i64())
        .bind(usize_to_i64(turn.seq))
        .execute(&mut **tx)
        .await
        .context("Failed to update mutable usage-turn attribution")?;
        Ok(())
    }

    pub(super) async fn load_usage_turns(&self, session_id: SessionId) -> Result<Vec<UsageTurn>> {
        let rows = sqlx::query(
            "SELECT seq, lane_kind, lane_id, lane_seq, parent_tool_call_id, launch_group_id, \
             status, finish_reason, reasoning_chars, provider_attempts_json, provider_id, model, \
             effective_reasoning, \
             prompt_tokens, completion_tokens, cache_read_input_tokens, \
             cache_creation_input_tokens, cache_measured_input_tokens, turn_cost_micros, \
             no_cache_cost_micros, estimated_prompt_tokens, estimate_source, estimate_confidence, \
             tool_schema_tokens, tool_schema_hash, tool_schema_names_json, \
             request_body_bytes, request_body_hash, cache_mechanism, cache_route_fingerprint, \
             expected_cacheable_percent, actual_cache_read_percent, \
             local_reusable_prefix_tokens, local_reusable_prefix_percent, \
             cacheable_prefix_tokens, volatile_tail_tokens, \
             context_window_tokens, rewrite_kind, rewrite_saved_tokens, episode_seq, \
             created_at_ms, latency_ms, ttft_ms, prefix_hash, \
             inspection_executed, inspection_reused, inspection_rejected, \
             inspection_returned_chars, inspection_avoided_chars, delegated_parent_overlap \
             FROM usage_turns WHERE session_id = ? ORDER BY seq",
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load usage turns for session {session_id}"))?;

        rows.into_iter()
            .map(|row| {
                let estimate_source = row
                    .try_get::<Option<String>, _>("estimate_source")?
                    .and_then(non_empty_string)
                    .map(|value| TokenCounterKind::from_db_str(&value));
                let estimate_confidence = row
                    .try_get::<Option<String>, _>("estimate_confidence")?
                    .and_then(non_empty_string)
                    .map(|value| EstimateConfidence::from_db_str(&value));
                Ok(UsageTurn {
                    seq: row.try_get::<i64, _>("seq")? as usize,
                    lane_kind: crate::agent::ExecutionLaneKind::from_db_str(
                        &row.try_get::<String, _>("lane_kind")?,
                    )
                    .ok_or_else(|| anyhow::anyhow!("Invalid execution lane kind"))?,
                    lane_id: row.try_get("lane_id")?,
                    lane_seq: row.try_get::<i64, _>("lane_seq")? as usize,
                    parent_tool_call_id: row
                        .try_get::<Option<String>, _>("parent_tool_call_id")?
                        .and_then(non_empty_string),
                    launch_group_id: row
                        .try_get::<Option<String>, _>("launch_group_id")?
                        .and_then(non_empty_string),
                    status: UsageTurnStatus::from_db_str(&row.try_get::<String, _>("status")?),
                    finish_reason: row
                        .try_get::<Option<String>, _>("finish_reason")?
                        .and_then(non_empty_string)
                        .map(|value| crate::provider::FinishReason::from_db_str(&value)),
                    reasoning_chars: row.try_get::<i64, _>("reasoning_chars")? as usize,
                    provider_attempts: serde_json::from_str(
                        &row.try_get::<String, _>("provider_attempts_json")?,
                    )
                    .context("Failed to deserialize provider attempt diagnostics")?,
                    // Restored on load so the snapshot rewrite (DELETE+INSERT of
                    // the in-memory vec) cannot erase attribution on resume.
                    provider_id: row
                        .try_get::<Option<String>, _>("provider_id")?
                        .and_then(non_empty_string),
                    model: row
                        .try_get::<Option<String>, _>("model")?
                        .and_then(non_empty_string),
                    effective_reasoning: row
                        .try_get::<Option<String>, _>("effective_reasoning")?
                        .and_then(non_empty_string)
                        .and_then(|value| crate::provider::ReasoningSelection::parse(&value)),
                    prompt_tokens: optional_i64_to_u64(row.try_get("prompt_tokens")?),
                    completion_tokens: optional_i64_to_u64(row.try_get("completion_tokens")?),
                    cache_read_input_tokens: optional_i64_to_u64(
                        row.try_get("cache_read_input_tokens")?,
                    ),
                    cache_creation_input_tokens: optional_i64_to_u64(
                        row.try_get("cache_creation_input_tokens")?,
                    ),
                    cache_measured_input_tokens: optional_i64_to_u64(
                        row.try_get("cache_measured_input_tokens")?,
                    ),
                    turn_cost_micros: optional_i64_to_u64(row.try_get("turn_cost_micros")?),
                    no_cache_cost_micros: optional_i64_to_u64(row.try_get("no_cache_cost_micros")?),
                    estimated_prompt_tokens: optional_i64_to_usize(
                        row.try_get("estimated_prompt_tokens")?,
                    ),
                    estimate_source,
                    estimate_confidence,
                    tool_schema_tokens: optional_i64_to_usize(row.try_get("tool_schema_tokens")?),
                    tool_schema_hash: row
                        .try_get::<Option<String>, _>("tool_schema_hash")?
                        .and_then(non_empty_string),
                    tool_schema_names: tool_schema_names_from_json(
                        row.try_get::<Option<String>, _>("tool_schema_names_json")?,
                    ),
                    request_body_bytes: optional_i64_to_usize(row.try_get("request_body_bytes")?),
                    request_body_hash: row
                        .try_get::<Option<String>, _>("request_body_hash")?
                        .and_then(non_empty_string),
                    cache_mechanism: row
                        .try_get::<Option<String>, _>("cache_mechanism")?
                        .and_then(non_empty_string),
                    cache_route_fingerprint: row
                        .try_get::<Option<String>, _>("cache_route_fingerprint")?
                        .and_then(non_empty_string),
                    expected_cacheable_percent: optional_i64_to_u64(
                        row.try_get("expected_cacheable_percent")?,
                    ),
                    actual_cache_read_percent: optional_i64_to_u64(
                        row.try_get("actual_cache_read_percent")?,
                    ),
                    local_reusable_prefix_tokens: optional_i64_to_usize(
                        row.try_get("local_reusable_prefix_tokens")?,
                    ),
                    local_reusable_prefix_percent: optional_i64_to_u64(
                        row.try_get("local_reusable_prefix_percent")?,
                    ),
                    cacheable_prefix_tokens: optional_i64_to_usize(
                        row.try_get("cacheable_prefix_tokens")?,
                    ),
                    volatile_tail_tokens: optional_i64_to_usize(
                        row.try_get("volatile_tail_tokens")?,
                    ),
                    context_window_tokens: optional_i64_to_usize(
                        row.try_get("context_window_tokens")?,
                    ),
                    rewrite_kind: ContextRewriteKind::from_db_str(
                        &row.try_get::<String, _>("rewrite_kind")?,
                    ),
                    rewrite_saved_tokens: optional_i64_to_usize(
                        row.try_get("rewrite_saved_tokens")?,
                    ),
                    episode_seq: optional_i64_to_usize(row.try_get("episode_seq")?),
                    created_at_ms: row.try_get::<i64, _>("created_at_ms")?,
                    latency_ms: optional_i64_to_u64(row.try_get("latency_ms")?),
                    ttft_ms: optional_i64_to_u64(row.try_get("ttft_ms")?),
                    prefix_hash: row
                        .try_get::<Option<String>, _>("prefix_hash")?
                        .and_then(non_empty_string),
                    inspection_executed: row.try_get::<i64, _>("inspection_executed")? as usize,
                    inspection_reused: row.try_get::<i64, _>("inspection_reused")? as usize,
                    inspection_rejected: row.try_get::<i64, _>("inspection_rejected")? as usize,
                    inspection_returned_chars: row.try_get::<i64, _>("inspection_returned_chars")?
                        as usize,
                    inspection_avoided_chars: row.try_get::<i64, _>("inspection_avoided_chars")?
                        as usize,
                    delegated_parent_overlap: row.try_get::<i64, _>("delegated_parent_overlap")?
                        as usize,
                })
            })
            .collect()
    }

    pub(super) async fn load_context_sources(
        &self,
        session_id: SessionId,
    ) -> Result<HashMap<String, Vec<ChatCompletionRequestMessage>>> {
        let rows = sqlx::query(
            "SELECT node_id, messages_json FROM context_sources WHERE session_id = ? ORDER BY node_id",
        )
        .bind(session_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load context sources for session {session_id}"))?;

        rows.into_iter()
            .map(|row| {
                let node_id: String = row.try_get("node_id")?;
                let messages_json: String = row.try_get("messages_json")?;
                let messages = serde_json::from_str(&messages_json)
                    .context("Failed to parse persisted context source messages")?;
                Ok((node_id, messages))
            })
            .collect()
    }
}

fn non_empty_string(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn tool_schema_names_json(names: &[String]) -> String {
    serde_json::to_string(names).unwrap_or_else(|_err| "[]".to_string())
}

fn tool_schema_names_from_json(value: Option<String>) -> Vec<String> {
    value
        .and_then(|value| serde_json::from_str::<Vec<String>>(&value).ok())
        .unwrap_or_default()
}

fn usize_to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn optional_u64_to_i64(value: Option<u64>) -> Option<i64> {
    value.map(|value| i64::try_from(value).unwrap_or(i64::MAX))
}

fn optional_i64_to_u64(value: Option<i64>) -> Option<u64> {
    value.and_then(|value| u64::try_from(value).ok())
}

fn optional_i64_to_usize(value: Option<i64>) -> Option<usize> {
    value.and_then(|value| usize::try_from(value).ok())
}
