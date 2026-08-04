use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionTargetAvailability {
    Available,
    NotFound,
    DifferentProject,
    Live,
}

impl Storage {
    pub async fn ensure_project(&self, path: &Path) -> Result<i64> {
        let path = canonicalize_lossy(path);
        let path_text = path.to_string_lossy().to_string();
        let display_name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| path_text.clone());
        sqlx::query(
            r#"
            INSERT INTO projects (path, display_name)
            VALUES (?, ?)
            ON CONFLICT(path) DO UPDATE SET
              display_name = excluded.display_name
            "#,
        )
        .bind(&path_text)
        .bind(&display_name)
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to save project {}", path.display()))?;

        sqlx::query_scalar("SELECT id FROM projects WHERE path = ?")
            .bind(path_text)
            .fetch_one(&self.pool)
            .await
            .context("Failed to load persisted project id")
    }

    pub async fn start_session(
        &self,
        project_path: &Path,
        provider_id: &str,
        model: &str,
        reasoning: ReasoningSelection,
    ) -> Result<SessionId> {
        let project_id = self.ensure_project(project_path).await?;
        let name = project_path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| project_path.to_string_lossy().to_string());
        let reasoning_json =
            serde_json::to_string(&reasoning).context("Failed to serialize session reasoning")?;
        let conversation_cache_key = crate::provider::new_conversation_cache_key();
        let now = now_ms();

        let id = sqlx::query_scalar(
            r#"
            INSERT INTO sessions (
              project_id, name, provider_id, model, reasoning_json,
              conversation_cache_key, status, started_at_ms, updated_at_ms
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(project_id)
        .bind(name)
        .bind(provider_id)
        .bind(model)
        .bind(reasoning_json)
        .bind(conversation_cache_key)
        .bind(SessionStatus::Active.as_db_str())
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .context("Failed to create persisted session")?;
        Ok(SessionId::from_raw(id))
    }

    /// Load the opaque provider cache-routing identity for a persisted session.
    pub(crate) async fn conversation_cache_key(&self, session_id: SessionId) -> Result<String> {
        sqlx::query_scalar("SELECT conversation_cache_key FROM sessions WHERE id = ?")
            .bind(session_id.as_i64())
            .fetch_one(&self.pool)
            .await
            .with_context(|| format!("Failed to load cache key for session {session_id}"))
    }

    /// Record a liveness heartbeat for `session_id`, carrying whether the
    /// agent is currently running a turn (`busy`) so peers can tell working
    /// from idle. It also checkpoints an open active-run timer, but never
    /// touches `updated_at_ms`, which orders `/resume` by content recency and
    /// must not advance for an idle-but-alive session.
    pub async fn record_session_heartbeat(&self, session_id: SessionId, busy: bool) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            r#"
            UPDATE sessions
            SET active_run_ms = active_run_ms + CASE
                  WHEN active_run_started_at_ms IS NOT NULL AND ? > active_run_started_at_ms
                    THEN ? - active_run_started_at_ms
                  ELSE 0
                END,
                active_run_started_at_ms = CASE
                  WHEN active_run_started_at_ms IS NOT NULL THEN ?
                  ELSE NULL
                END,
                last_heartbeat_ms = ?, busy = ?
            WHERE id = ?
            "#,
        )
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(busy)
        .bind(session_id.as_i64())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to record heartbeat for session {session_id}"))?;
        Ok(())
    }

    /// Start one foreground execution segment and return active milliseconds
    /// already consumed by this resumable session. A stale open segment from a
    /// crash is recovered only through its last heartbeat, never through the
    /// idle gap before this resume.
    pub(crate) async fn begin_session_run(&self, session_id: SessionId) -> Result<u64> {
        let now = now_ms();
        let active_run_ms: i64 = sqlx::query_scalar(
            r#"
            UPDATE sessions
            SET active_run_ms = active_run_ms + CASE
                  WHEN active_run_started_at_ms IS NOT NULL
                       AND last_heartbeat_ms IS NOT NULL
                       AND last_heartbeat_ms > active_run_started_at_ms
                    THEN last_heartbeat_ms - active_run_started_at_ms
                  ELSE 0
                END,
                active_run_started_at_ms = ?, status = ?, terminal_reason = NULL,
                updated_at_ms = ?, ended_at_ms = NULL
            WHERE id = ?
            RETURNING active_run_ms
            "#,
        )
        .bind(now)
        .bind(SessionStatus::Active.as_db_str())
        .bind(now)
        .bind(session_id.as_i64())
        .fetch_one(&self.pool)
        .await
        .with_context(|| format!("Failed to begin active run for session {session_id}"))?;
        Ok(u64::try_from(active_run_ms).unwrap_or(0))
    }

    /// Return cumulative active milliseconds, including an open segment up to
    /// the instant of this query without mutating its start time.
    pub(crate) async fn session_active_run_ms_now(&self, session_id: SessionId) -> Result<u64> {
        let now = now_ms();
        let active_run_ms: i64 = sqlx::query_scalar(
            r#"
            SELECT active_run_ms + CASE
              WHEN active_run_started_at_ms IS NOT NULL AND ? > active_run_started_at_ms
                THEN ? - active_run_started_at_ms
              ELSE 0
            END
            FROM sessions WHERE id = ?
            "#,
        )
        .bind(now)
        .bind(now)
        .bind(session_id.as_i64())
        .fetch_one(&self.pool)
        .await
        .with_context(|| format!("Failed to load active time for session {session_id}"))?;
        Ok(u64::try_from(active_run_ms).unwrap_or(0))
    }

    /// Close the current foreground execution segment and return cumulative
    /// active milliseconds. Safe to call more than once; a closed segment is a
    /// no-op.
    pub(crate) async fn finish_session_run(&self, session_id: SessionId) -> Result<u64> {
        let now = now_ms();
        // IMPORTANT LIFECYCLE INVARIANT: closing the active run also clears the
        // peer-visible busy bit. Do not split these updates; a crash between two
        // statements would leave an idle session advertised as permanently busy.
        let active_run_ms: i64 = sqlx::query_scalar(
            r#"
            UPDATE sessions
            SET active_run_ms = active_run_ms + CASE
                  WHEN active_run_started_at_ms IS NOT NULL AND ? > active_run_started_at_ms
                    THEN ? - active_run_started_at_ms
                  ELSE 0
                END,
                active_run_started_at_ms = NULL,
                busy = 0
            WHERE id = ?
            RETURNING active_run_ms
            "#,
        )
        .bind(now)
        .bind(now)
        .bind(session_id.as_i64())
        .fetch_one(&self.pool)
        .await
        .with_context(|| format!("Failed to finish active run for session {session_id}"))?;
        Ok(u64::try_from(active_run_ms).unwrap_or(0))
    }

    #[cfg(test)]
    pub(crate) async fn session_active_run_ms(&self, session_id: SessionId) -> Result<u64> {
        let active_run_ms: i64 =
            sqlx::query_scalar("SELECT active_run_ms FROM sessions WHERE id = ?")
                .bind(session_id.as_i64())
                .fetch_one(&self.pool)
                .await
                .with_context(|| format!("Failed to load active time for session {session_id}"))?;
        Ok(u64::try_from(active_run_ms).unwrap_or(0))
    }

    pub async fn mark_session_status(
        &self,
        session_id: SessionId,
        status: SessionStatus,
    ) -> Result<()> {
        self.mark_session_termination(session_id, status, None)
            .await
    }

    pub(crate) async fn mark_session_termination(
        &self,
        session_id: SessionId,
        status: SessionStatus,
        reason: Option<crate::run_budget::RunBudgetExhaustion>,
    ) -> Result<()> {
        let now = now_ms();
        let ended_at = status.is_terminal().then_some(now);
        let terminal_reason = reason
            .map(crate::run_budget::RunBudgetExhaustion::to_json)
            .transpose()?;
        // IMPORTANT LIFECYCLE INVARIANT: no terminal session may remain busy.
        // Peers and `/sessions` treat that bit as live work; leaving it set after
        // interruption creates a permanently running ghost session.
        sqlx::query(
            r#"
            UPDATE sessions
            SET status = ?, terminal_reason = ?, updated_at_ms = ?,
                ended_at_ms = COALESCE(?, ended_at_ms),
                active_run_ms = active_run_ms + CASE
                  WHEN active_run_started_at_ms IS NOT NULL AND ? > active_run_started_at_ms
                    THEN ? - active_run_started_at_ms
                  ELSE 0
                END,
                active_run_started_at_ms = NULL,
                busy = CASE WHEN ? THEN 0 ELSE busy END
            WHERE id = ?
            "#,
        )
        .bind(status.as_db_str())
        .bind(terminal_reason)
        .bind(now)
        .bind(ended_at)
        .bind(now)
        .bind(now)
        .bind(status.is_terminal())
        .bind(session_id.as_i64())
        .execute(&self.pool)
        .await
        .with_context(|| {
            format!(
                "Failed to mark session {session_id} as {}",
                status.as_db_str()
            )
        })?;
        // A finished session holds no advisory claims (peers P4).
        if status.is_terminal() {
            self.release_all_peer_claims(session_id).await?;
        }
        Ok(())
    }

    pub async fn set_session_summary(&self, session_id: SessionId, summary: &str) -> Result<()> {
        let now = now_ms();
        sqlx::query(
            r#"
            UPDATE sessions
            SET summary = ?, updated_at_ms = ?
            WHERE id = ?
            "#,
        )
        .bind(summary.trim())
        .bind(now)
        .bind(session_id.as_i64())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to update session {session_id} title"))?;
        Ok(())
    }

    pub async fn set_session_run_selection(
        &self,
        session_id: SessionId,
        provider_id: &str,
        model: &str,
        reasoning: ReasoningSelection,
    ) -> Result<()> {
        let reasoning_json =
            serde_json::to_string(&reasoning).context("Failed to serialize session reasoning")?;
        let now = now_ms();
        sqlx::query(
            r#"
            UPDATE sessions
            SET provider_id = ?, model = ?, reasoning_json = ?, updated_at_ms = ?
            WHERE id = ?
            "#,
        )
        .bind(provider_id)
        .bind(model)
        .bind(reasoning_json)
        .bind(now)
        .bind(session_id.as_i64())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to update run selection for session {session_id}"))?;
        Ok(())
    }

    pub async fn switch_active_session(
        &self,
        current_session_id: SessionId,
        next_session_id: SessionId,
        project_path: &Path,
        current_status: SessionStatus,
    ) -> Result<ResumeSessionOutcome> {
        let project_path = canonical_project_path(project_path);
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin session switch transaction")?;
        let now = now_ms();
        let live_floor = now.saturating_sub(super::peers::PEER_LIVENESS_THRESHOLD_MS);
        let claimed: Option<i64> = sqlx::query_scalar(
            r#"
            UPDATE sessions
            SET status = ?, terminal_reason = NULL, updated_at_ms = ?, ended_at_ms = NULL,
                active_run_ms = active_run_ms + CASE
                  WHEN active_run_started_at_ms IS NOT NULL
                       AND last_heartbeat_ms IS NOT NULL
                       AND last_heartbeat_ms > active_run_started_at_ms
                    THEN last_heartbeat_ms - active_run_started_at_ms
                  ELSE 0
                END,
                active_run_started_at_ms = NULL,
                last_heartbeat_ms = ?,
                busy = 0
            WHERE id = ?
              AND project_id = (SELECT id FROM projects WHERE path = ?)
              AND NOT (
                status = ?
                AND last_heartbeat_ms IS NOT NULL
                AND last_heartbeat_ms >= ?
              )
            RETURNING id
            "#,
        )
        .bind(SessionStatus::Active.as_db_str())
        .bind(now)
        .bind(now)
        .bind(next_session_id.as_i64())
        .bind(&project_path)
        .bind(SessionStatus::Active.as_db_str())
        .bind(live_floor)
        .fetch_optional(&mut *tx)
        .await
        .with_context(|| format!("Failed to claim session {next_session_id} for resume"))?;
        if claimed.is_none() {
            tx.rollback()
                .await
                .context("Failed to roll back refused session switch")?;
            return self
                .resume_outcome_for_unclaimed_session(&project_path, next_session_id)
                .await;
        }

        let completed = sqlx::query(
            r#"
            UPDATE sessions
            SET status = ?, updated_at_ms = ?, ended_at_ms = COALESCE(ended_at_ms, ?),
                active_run_ms = active_run_ms + CASE
                  WHEN active_run_started_at_ms IS NOT NULL AND ? > active_run_started_at_ms
                    THEN ? - active_run_started_at_ms
                  ELSE 0
                END,
                active_run_started_at_ms = NULL,
                busy = 0
            WHERE id = ?
            "#,
        )
        .bind(current_status.as_db_str())
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(now)
        .bind(current_session_id.as_i64())
        .execute(&mut *tx)
        .await
        .with_context(|| format!("Failed to complete session {current_session_id}"))?;
        anyhow::ensure!(
            completed.rows_affected() == 1,
            "Cannot switch from missing session {current_session_id}"
        );
        self.attach_recovery_workspace_session_in_tx(
            &mut tx,
            project_path.as_ref(),
            next_session_id,
            now,
        )
        .await?;
        tx.commit()
            .await
            .context("Failed to commit session switch")?;
        Ok(ResumeSessionOutcome::Resumed)
    }

    pub async fn claim_session_for_resume(
        &self,
        project_path: &Path,
        session_id: SessionId,
    ) -> Result<ResumeSessionOutcome> {
        let project_path = canonical_project_path(project_path);
        let now = now_ms();
        let live_floor = now.saturating_sub(super::peers::PEER_LIVENESS_THRESHOLD_MS);
        let claimed: Option<i64> = sqlx::query_scalar(
            r#"
            UPDATE sessions
            SET status = ?, terminal_reason = NULL, updated_at_ms = ?, ended_at_ms = NULL,
                active_run_ms = active_run_ms + CASE
                  WHEN active_run_started_at_ms IS NOT NULL
                       AND last_heartbeat_ms IS NOT NULL
                       AND last_heartbeat_ms > active_run_started_at_ms
                    THEN last_heartbeat_ms - active_run_started_at_ms
                  ELSE 0
                  END,
                active_run_started_at_ms = NULL,
                last_heartbeat_ms = ?,
                busy = 0
            WHERE id = ?
              AND project_id = (SELECT id FROM projects WHERE path = ?)
              AND NOT (
                status = ?
                AND last_heartbeat_ms IS NOT NULL
                AND last_heartbeat_ms >= ?
              )
            RETURNING id
            "#,
        )
        .bind(SessionStatus::Active.as_db_str())
        .bind(now)
        .bind(now)
        .bind(session_id.as_i64())
        .bind(&project_path)
        .bind(SessionStatus::Active.as_db_str())
        .bind(live_floor)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("Failed to claim session {session_id} for resume"))?;
        if claimed.is_some() {
            return Ok(ResumeSessionOutcome::Resumed);
        }
        self.resume_outcome_for_unclaimed_session(&project_path, session_id)
            .await
    }

    pub async fn is_session_live(&self, session_id: SessionId) -> Result<bool> {
        let row = sqlx::query("SELECT status, last_heartbeat_ms FROM sessions WHERE id = ?")
            .bind(session_id.as_i64())
            .fetch_optional(&self.pool)
            .await
            .with_context(|| format!("Failed to inspect liveness for session {session_id}"))?;
        let Some(row) = row else {
            return Ok(false);
        };
        let status = SessionStatus::from_db_str(&row.try_get::<String, _>("status")?);
        let heartbeat = row.try_get::<Option<i64>, _>("last_heartbeat_ms")?;
        Ok(status == SessionStatus::Active
            && heartbeat.is_some_and(|heartbeat| {
                heartbeat >= now_ms().saturating_sub(super::peers::PEER_LIVENESS_THRESHOLD_MS)
            }))
    }

    /// Flip every leftover `Active` session for `project_path` (a row that was
    /// never marked terminal because the prior run did not exit cleanly) to
    /// `Interrupted`, then return summaries for the rows promoted *by this call*.
    ///
    /// `current_session_id` is excluded so the freshly started session for this
    /// run is never promoted. A clean exit leaves its row `Completed`, so it is
    /// not `Active` here and produces no summary — the caller uses a non-empty
    /// result as the signal to surface the "Interrupted session found" hint.
    /// The call is idempotent: a second invocation finds nothing `Active` left
    /// to promote and returns an empty vec.
    ///
    /// A session with a **fresh heartbeat** is a live concurrent process, not a
    /// crash leftover, and is never promoted — this is what makes multiple
    /// bonsai sessions in one project root safe. Legacy rows (`NULL`
    /// heartbeat) and stale heartbeats are promoted exactly as before; the
    /// known tradeoff is that a session that crashed less than
    /// [`PEER_LIVENESS_THRESHOLD_MS`](super::peers::PEER_LIVENESS_THRESHOLD_MS)
    /// before a boot survives one cycle as `Active` and is promoted on the
    /// next.
    pub async fn promote_active_sessions_to_interrupted(
        &self,
        project_path: &Path,
        current_session_id: SessionId,
        limit: i64,
    ) -> Result<Vec<SessionSummary>> {
        let project_path = canonical_project_path(project_path);
        let now = now_ms();

        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin interrupted-session promotion")?;

        let promoted_ids: Vec<i64> = sqlx::query_scalar(
            r#"
            UPDATE sessions
            SET status = ?, updated_at_ms = ?, ended_at_ms = COALESCE(ended_at_ms, ?),
                active_run_ms = active_run_ms + CASE
                  WHEN active_run_started_at_ms IS NOT NULL
                       AND last_heartbeat_ms IS NOT NULL
                       AND last_heartbeat_ms > active_run_started_at_ms
                    THEN last_heartbeat_ms - active_run_started_at_ms
                  ELSE 0
                END,
                active_run_started_at_ms = NULL
            WHERE project_id = (SELECT id FROM projects WHERE path = ?)
              AND status = ?
              AND id != ?
              AND (last_heartbeat_ms IS NULL OR last_heartbeat_ms < ?)
            RETURNING id
            "#,
        )
        .bind(SessionStatus::Interrupted.as_db_str())
        .bind(now)
        .bind(now)
        .bind(&project_path)
        .bind(SessionStatus::Active.as_db_str())
        .bind(current_session_id.as_i64())
        .bind(now.saturating_sub(super::peers::PEER_LIVENESS_THRESHOLD_MS))
        .fetch_all(&mut *tx)
        .await
        .context("Failed to promote interrupted sessions")?;

        if promoted_ids.is_empty() {
            tx.commit()
                .await
                .context("Failed to commit interrupted-session promotion")?;
            return Ok(Vec::new());
        }

        // The process died while these sessions were live. Close only active
        // task rows; an outcome persisted before the crash is immutable and
        // must remain independent from the interrupted session lifecycle.
        let placeholders = vec!["?"; promoted_ids.len()].join(", ");
        let finish_tasks = format!(
            "UPDATE task_runs SET outcome = ?, terminal_reason_code = ?, \
             terminal_reason_detail = ?, ended_at_ms = ? \
             WHERE outcome IS NULL AND session_id IN ({placeholders})"
        );
        let mut finish_tasks_query = sqlx::query(sqlx::AssertSqlSafe(finish_tasks))
            .bind(TaskOutcome::Failed.as_db_str())
            .bind(TaskTerminalReasonCode::ProcessInterrupted.as_db_str())
            .bind("The prior Bonsai process ended before this task completed.")
            .bind(now);
        for id in &promoted_ids {
            finish_tasks_query = finish_tasks_query.bind(id);
        }
        storage_op!(
            &mut tx,
            "finish tasks from interrupted sessions",
            finish_tasks_query,
        )?;

        // Crash leftovers hold no advisory claims (peers P4); release inside
        // the same transaction as the promotion.
        let placeholders = vec!["?"; promoted_ids.len()].join(", ");
        let release = format!(
            "UPDATE peer_claims SET released_at_ms = ? \
             WHERE released_at_ms IS NULL AND session_id IN ({placeholders})"
        );
        let mut release_query = sqlx::query(sqlx::AssertSqlSafe(release)).bind(now);
        for id in &promoted_ids {
            release_query = release_query.bind(id);
        }
        storage_op!(&mut tx, "release promoted sessions' claims", release_query,)?;

        let placeholders = vec!["?"; promoted_ids.len()].join(", ");
        let query = session_summary_query(
            &format!("sessions.id IN ({placeholders})"),
            "ORDER BY sessions.updated_at_ms DESC\n            LIMIT ?",
        );
        let mut select = sqlx::query(sqlx::AssertSqlSafe(query));
        for id in &promoted_ids {
            select = select.bind(id);
        }
        let rows = select
            .bind(limit.max(1))
            .fetch_all(&mut *tx)
            .await
            .context("Failed to list interrupted sessions")?;

        tx.commit()
            .await
            .context("Failed to commit interrupted-session promotion")?;

        // A promoted session died without firing its wake subscriptions; fire
        // them now so no waiter stays parked on a crash leftover. Outside the
        // promotion transaction on purpose (firing is idempotent and its own
        // tx); a failure here must not roll back the promotion.
        for id in &promoted_ids {
            let target = SessionId::from_raw(*id);
            if let Err(err) = self.fire_wake_subscriptions(target).await {
                tracing::warn!(
                    error = %format!("{err:#}"),
                    session = %target,
                    "failed to fire wake subscriptions for promoted session"
                );
            }
        }

        rows.into_iter().map(session_summary_from_row).collect()
    }

    pub async fn recent_sessions_for_project(
        &self,
        project_path: &Path,
        limit: i64,
    ) -> Result<Vec<SessionSummary>> {
        let project_path = canonical_project_path(project_path);
        let query = session_summary_query(
            "projects.path = ?",
            "ORDER BY sessions.updated_at_ms DESC\n            LIMIT ?",
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(project_path)
            .bind(limit.max(1))
            .fetch_all(&self.pool)
            .await
            .context("Failed to list project sessions")?;

        rows.into_iter().map(session_summary_from_row).collect()
    }

    pub async fn latest_prior_session_for_project(
        &self,
        project_path: &Path,
        current_session_id: SessionId,
    ) -> Result<Option<SessionSummary>> {
        let project_path = canonical_project_path(project_path);
        let query = session_summary_query(
            "projects.path = ? AND sessions.id != ?",
            "ORDER BY sessions.updated_at_ms DESC\n            LIMIT 1",
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(project_path)
            .bind(current_session_id.as_i64())
            .fetch_optional(&self.pool)
            .await
            .context("Failed to resolve latest project session")?;

        row.map(session_summary_from_row).transpose()
    }

    #[cfg(test)]
    pub async fn load_session_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionSnapshot>> {
        self.load_session_snapshot_inner(session_id, true).await
    }

    /// Runtime resume loader. When the episode kill switch is set, do not
    /// hydrate archive rows that the unwired agent would immediately discard;
    /// this keeps the opt-out inert even for sessions with large old archives.
    pub(crate) async fn load_runtime_session_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionSnapshot>> {
        self.load_session_snapshot_inner(session_id, crate::episode::episodes_enabled())
            .await
    }

    async fn load_session_snapshot_inner(
        &self,
        session_id: SessionId,
        include_episodes: bool,
    ) -> Result<Option<SessionSnapshot>> {
        let Some(summary) = self.session_summary(session_id).await? else {
            return Ok(None);
        };
        let context_snapshot = self.load_context_messages(session_id).await?;
        Ok(Some(SessionSnapshot {
            summary,
            transcript: self.load_transcript_items(session_id).await?,
            context_messages: context_snapshot.messages,
            context_message_ids: context_snapshot.ids,
            context_controls: self.load_context_controls(session_id).await?,
            context_sources: self.load_context_sources(session_id).await?,
            read_evidence: self.load_read_evidence(session_id).await?,
            inspection_events: self.load_inspection_events(session_id).await?,
            compaction_events: self.load_compaction_events(session_id).await?,
            episodes: if include_episodes {
                self.load_episodes(session_id).await?
            } else {
                Vec::new()
            },
            usage_turns: self.load_usage_turns(session_id).await?,
            verification_runs: self.load_verification_runs(session_id).await?,
            self_review_runs: self.load_self_review_runs(session_id).await?,
            message_history: self.load_message_history(session_id).await?,
            plan: self.load_plan_snapshot(session_id).await?,
            todos: self.load_todos_snapshot(session_id).await?,
        }))
    }

    pub async fn forget_session(
        &self,
        project_path: &Path,
        session_id: SessionId,
    ) -> Result<ForgetSessionOutcome> {
        let project_path = canonical_project_path(project_path);
        let live_floor = now_ms().saturating_sub(super::peers::PEER_LIVENESS_THRESHOLD_MS);
        let deleted: Option<i64> = sqlx::query_scalar(
            r#"
            DELETE FROM sessions
            WHERE id = ?
              AND project_id = (SELECT id FROM projects WHERE path = ?)
              AND NOT (
                status = ?
                AND last_heartbeat_ms IS NOT NULL
                AND last_heartbeat_ms >= ?
              )
            RETURNING id
            "#,
        )
        .bind(session_id.as_i64())
        .bind(&project_path)
        .bind(SessionStatus::Active.as_db_str())
        .bind(live_floor)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("Failed to forget session {session_id}"))?;
        if deleted.is_some() {
            return Ok(ForgetSessionOutcome::Forgotten);
        }
        Ok(
            match self
                .session_target_availability(&project_path, session_id)
                .await?
            {
                SessionTargetAvailability::NotFound => ForgetSessionOutcome::NotFound,
                SessionTargetAvailability::DifferentProject => {
                    ForgetSessionOutcome::DifferentProject
                }
                SessionTargetAvailability::Live | SessionTargetAvailability::Available => {
                    ForgetSessionOutcome::Live
                }
            },
        )
    }

    async fn resume_outcome_for_unclaimed_session(
        &self,
        project_path: &str,
        session_id: SessionId,
    ) -> Result<ResumeSessionOutcome> {
        Ok(
            match self
                .session_target_availability(project_path, session_id)
                .await?
            {
                SessionTargetAvailability::NotFound => ResumeSessionOutcome::NotFound,
                SessionTargetAvailability::DifferentProject => {
                    ResumeSessionOutcome::DifferentProject
                }
                SessionTargetAvailability::Live | SessionTargetAvailability::Available => {
                    ResumeSessionOutcome::Live
                }
            },
        )
    }

    async fn session_target_availability(
        &self,
        project_path: &str,
        session_id: SessionId,
    ) -> Result<SessionTargetAvailability> {
        let row = sqlx::query(
            r#"
            SELECT projects.path, sessions.status, sessions.last_heartbeat_ms
            FROM sessions
            JOIN projects ON projects.id = sessions.project_id
            WHERE sessions.id = ?
            "#,
        )
        .bind(session_id.as_i64())
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("Failed to inspect session {session_id}"))?;
        let Some(row) = row else {
            return Ok(SessionTargetAvailability::NotFound);
        };
        if row.try_get::<String, _>("path")? != project_path {
            return Ok(SessionTargetAvailability::DifferentProject);
        }
        let status = SessionStatus::from_db_str(&row.try_get::<String, _>("status")?);
        let heartbeat = row.try_get::<Option<i64>, _>("last_heartbeat_ms")?;
        if status == SessionStatus::Active
            && heartbeat.is_some_and(|heartbeat| {
                heartbeat >= now_ms().saturating_sub(super::peers::PEER_LIVENESS_THRESHOLD_MS)
            })
        {
            Ok(SessionTargetAvailability::Live)
        } else {
            Ok(SessionTargetAvailability::Available)
        }
    }

    #[cfg(test)]
    pub(crate) async fn update_session_usage(
        &self,
        session_id: SessionId,
        prompt_tokens: u64,
        completion_tokens: u64,
        cost_micros: Option<u64>,
        input_cache: Option<InputCacheUsage>,
    ) -> Result<()> {
        let usage = crate::agent::UsageTotals {
            prompt_tokens,
            completion_tokens,
            cost_micros,
            no_cache_cost_micros: cost_micros,
            input_cache,
        };
        self.update_session_usage_totals(session_id, &usage).await
    }

    pub(crate) async fn update_session_usage_totals(
        &self,
        session_id: SessionId,
        usage: &crate::agent::UsageTotals,
    ) -> Result<()> {
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin usage update transaction")?;
        let now = now_ms();
        self.update_session_usage_in_tx(&mut tx, session_id, usage, now)
            .await?;
        tx.commit()
            .await
            .context("Failed to commit usage update transaction")?;
        Ok(())
    }

    pub(crate) async fn update_session_usage_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
        usage: &crate::agent::UsageTotals,
        now: i64,
    ) -> Result<()> {
        let input_cache = usage.input_cache.unwrap_or_default();
        sqlx::query(
            r#"
            UPDATE sessions
            SET prompt_token_count = ?,
                completion_token_count = ?,
                cache_read_input_token_count = ?,
                cache_creation_input_token_count = ?,
                cache_measured_input_token_count = ?,
                cost_micros = ?,
                no_cache_cost_micros = COALESCE(
                  (SELECT SUM(no_cache_cost_micros)
                   FROM usage_turns
                   WHERE session_id = ? AND turn_cost_micros IS NOT NULL
                   HAVING COUNT(*) > 0
                     AND COUNT(no_cache_cost_micros) = COUNT(*)
                     AND MIN(turn_cost_micros) >= 0
                     AND MIN(no_cache_cost_micros) >= 0
                     AND SUM(turn_cost_micros) = ?),
                  ?
                ),
                updated_at_ms = ?
            WHERE id = ?
            "#,
        )
        .bind(i64::try_from(usage.prompt_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(usage.completion_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(input_cache.read_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(input_cache.creation_tokens).unwrap_or(i64::MAX))
        .bind(i64::try_from(input_cache.total_input_tokens).unwrap_or(i64::MAX))
        .bind(encode_cost_micros(usage.cost_micros))
        .bind(session_id.as_i64())
        .bind(encode_cost_micros(usage.cost_micros))
        .bind(encode_cost_micros(usage.no_cache_cost_micros))
        .bind(now)
        .bind(session_id.as_i64())
        .execute(&mut **tx)
        .await
        .with_context(|| format!("Failed to update usage for session {session_id}"))?;
        Ok(())
    }

    /// Session-scoped FTS over the persisted transcript messages (user /
    /// assistant / thinking only — tool bytes never enter this index). Used by
    /// the `recall` tool's query mode; the cross-session [`Self::search_messages`]
    /// stays the TUI `/search` surface.
    pub async fn search_session_messages(
        &self,
        session_id: SessionId,
        query: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>> {
        let fts_query = fts_literal_query(query);
        let rows = sqlx::query(
            r#"
            SELECT
              messages.role,
              snippet(messages_fts, 0, '[', ']', '...', 12) AS content
            FROM messages_fts
            JOIN messages ON messages.id = messages_fts.rowid
            WHERE messages_fts MATCH ? AND messages.session_id = ?
            ORDER BY rank
            LIMIT ?
            "#,
        )
        .bind(fts_query)
        .bind(session_id.as_i64())
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .context("Failed to search session messages")?;

        rows.into_iter()
            .map(|row| Ok((row.try_get("role")?, row.try_get("content")?)))
            .collect()
    }

    pub async fn search_messages(&self, query: &str, limit: i64) -> Result<Vec<SearchHit>> {
        let fts_query = fts_literal_query(query);
        let rows = sqlx::query(
            r#"
            SELECT
              messages.session_id,
              projects.path AS project_path,
              messages.role,
              snippet(messages_fts, 0, '[', ']', '...', 12) AS content,
              sessions.updated_at_ms
            FROM messages_fts
            JOIN messages ON messages.id = messages_fts.rowid
            JOIN sessions ON sessions.id = messages.session_id
            JOIN projects ON projects.id = sessions.project_id
            WHERE messages_fts MATCH ?
            ORDER BY rank
            LIMIT ?
            "#,
        )
        .bind(fts_query)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .context("Failed to search persisted messages")?;

        rows.into_iter()
            .map(|row| {
                Ok(SearchHit {
                    session_id: SessionId::from_raw(row.try_get("session_id")?),
                    project_path: row.try_get("project_path")?,
                    role: row.try_get("role")?,
                    content: row.try_get("content")?,
                    updated_at_ms: row.try_get("updated_at_ms")?,
                })
            })
            .collect()
    }
    pub(crate) async fn session_summary(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionSummary>> {
        let query = session_summary_query("sessions.id = ?", "");
        let row = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(session_id.as_i64())
            .fetch_optional(&self.pool)
            .await
            .with_context(|| format!("Failed to load session {session_id}"))?;

        row.map(session_summary_from_row).transpose()
    }
}

fn reasoning_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ReasoningSelection> {
    let reasoning_json: String = row.try_get("reasoning_json")?;
    serde_json::from_str(&reasoning_json).context("Failed to parse persisted session reasoning")
}

fn session_summary_query(where_clause: &str, suffix: &str) -> String {
    format!(
        r#"
            SELECT
{SESSION_SUMMARY_PROJECTION}
            FROM sessions
            JOIN projects ON projects.id = sessions.project_id
            LEFT JOIN task_runs AS latest_task ON latest_task.id = (
              SELECT task_runs.id
              FROM task_runs
              WHERE task_runs.session_id = sessions.id
              ORDER BY task_runs.started_at_ms DESC, task_runs.id DESC
              LIMIT 1
            )
            LEFT JOIN messages ON messages.session_id = sessions.id
            WHERE {where_clause}
            GROUP BY sessions.id
            {suffix}
            "#
    )
}

fn session_summary_from_row(row: sqlx::sqlite::SqliteRow) -> Result<SessionSummary> {
    let id = SessionId::from_raw(
        row.try_get("id")
            .context("Persisted session row has no valid id")?,
    );
    let decode = || -> Result<SessionSummary> {
        Ok(SessionSummary {
            id,
            project_path: row.try_get("project_path")?,
            name: row.try_get("name")?,
            summary: row.try_get("summary")?,
            provider_id: row.try_get("provider_id")?,
            model: row.try_get("model")?,
            reasoning: reasoning_from_row(&row)?,
            status: SessionStatus::from_db_str(&row.try_get::<String, _>("status")?),
            terminal_reason: row
                .try_get::<Option<String>, _>("terminal_reason")?
                .map(|value| crate::run_budget::RunBudgetExhaustion::from_json(&value))
                .transpose()?,
            latest_task: super::task_runs::latest_task_run_from_session_row(&row, id)?
                .map(Box::new),
            updated_at_ms: row.try_get("updated_at_ms")?,
            message_count: row.try_get("message_count")?,
            prompt_token_count: row.try_get("prompt_token_count")?,
            completion_token_count: row.try_get("completion_token_count")?,
            cache_read_input_token_count: row.try_get("cache_read_input_token_count")?,
            cache_creation_input_token_count: row.try_get("cache_creation_input_token_count")?,
            cache_measured_input_token_count: row.try_get("cache_measured_input_token_count")?,
            cost_micros: row.try_get("cost_micros")?,
            no_cache_cost_micros: row.try_get("no_cache_cost_micros")?,
            source_plan_id: row
                .try_get::<Option<i64>, _>("source_plan_id")?
                .map(SavedPlanId::from_raw),
        })
    };
    decode().with_context(|| {
        format!(
            "Persisted session {id} is damaged; run `bonsai doctor` and restore bonsai.db from backup if needed"
        )
    })
}
fn fts_literal_query(query: &str) -> String {
    let escaped = query.trim().replace('"', "\"\"");
    format!("\"{escaped}\"")
}
