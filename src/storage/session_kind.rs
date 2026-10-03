//! Durable distinction between task work and process/configuration identity.

use super::*;

/// Why a durable session identity exists, independently of process status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// A meaningful user, provider, or tool event has begun work.
    Task,
    /// Startup or peer identity without task work.
    LifecycleOnly,
    /// Provider authorization/model selection without task work.
    ProviderProbe,
    /// Historical data without enough evidence to infer a task.
    LegacyUnclassified,
}

crate::impl_db_enum!(SessionKind {
    Task => "task",
    LifecycleOnly => "lifecycle_only",
    ProviderProbe => "provider_probe",
    LegacyUnclassified => "legacy_unclassified",
} else LegacyUnclassified);

/// Normal task selection or explicit diagnostic access to all identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionListScope {
    /// Sessions with meaningful task activity.
    Tasks,
    /// Includes startup, configuration, and ambiguous legacy records.
    All,
}

impl Storage {
    /// Load the persisted classification of an existing session.
    pub(crate) async fn session_kind(&self, session_id: SessionId) -> Result<SessionKind> {
        let kind: String = sqlx::query_scalar("SELECT kind FROM sessions WHERE id = ?")
            .bind(session_id.as_i64())
            .fetch_one(&self.pool)
            .await
            .with_context(|| format!("Failed to load classification for session {session_id}"))?;
        Ok(SessionKind::from_db_str(&kind))
    }

    /// Promote identity and task evidence in the same transaction, retaining
    /// the session id and cache-routing key even under concurrent first events.
    pub(crate) async fn promote_task_session_in_tx(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        session_id: SessionId,
    ) -> Result<()> {
        let updated = sqlx::query("UPDATE sessions SET kind = 'task' WHERE id = ?")
            .bind(session_id.as_i64())
            .execute(&mut **tx)
            .await
            .context("Failed to promote task session")?;
        anyhow::ensure!(
            updated.rows_affected() == 1,
            "Session {session_id} does not exist"
        );
        Ok(())
    }

    /// Classify configuration activity without downgrading an existing task.
    pub(crate) async fn record_provider_probe(&self, session_id: SessionId) -> Result<()> {
        sqlx::query(
            "UPDATE sessions SET kind = 'provider_probe' WHERE id = ? AND kind = 'lifecycle_only'",
        )
        .bind(session_id.as_i64())
        .execute(&self.pool)
        .await
        .context("Failed to classify provider probe")?;
        Ok(())
    }

    /// Retain a bounded startup/configuration failure without manufacturing
    /// task evidence or persisting credentials from an error payload.
    pub(crate) async fn record_lifecycle_diagnostic(
        &self,
        session_id: SessionId,
        detail: &str,
    ) -> Result<()> {
        let detail = crate::redact::redact(detail)
            .chars()
            .take(1024)
            .collect::<String>();
        sqlx::query("UPDATE sessions SET lifecycle_diagnostic = ? WHERE id = ? AND kind != 'task' AND COALESCE(lifecycle_diagnostic, '') != ?")
            .bind(&detail).bind(session_id.as_i64()).bind(&detail)
            .execute(&self.pool).await.context("Failed to retain lifecycle diagnostic")?;
        Ok(())
    }

    /// Retention policy: remove only newly classified, cleanly completed,
    /// unused identities. Preserve crashes, legacy rows, task data, and peer /
    /// configuration audit evidence. Never sweep unrelated sessions.
    pub(crate) async fn retire_clean_lifecycle_session(&self, session_id: SessionId) -> Result<()> {
        sqlx::query(
            r#"DELETE FROM sessions
               WHERE id = ? AND status = 'completed'
                 AND kind IN ('lifecycle_only', 'provider_probe')
                 AND lifecycle_diagnostic IS NULL
                 AND NOT EXISTS (SELECT 1 FROM agent_messages
                   WHERE from_session_id = sessions.id OR to_session_id = sessions.id)
                 AND NOT EXISTS (SELECT 1 FROM authorization_decisions WHERE session_id = sessions.id)
                 AND NOT EXISTS (SELECT 1 FROM recovery_points WHERE session_id = sessions.id)
                 AND NOT EXISTS (SELECT 1 FROM saved_plans WHERE source_session_id = sessions.id)
                 AND NOT EXISTS (SELECT 1 FROM task_runs WHERE session_id = sessions.id)
                 AND NOT EXISTS (SELECT 1 FROM messages WHERE session_id = sessions.id AND role = 'user')
                 AND NOT EXISTS (SELECT 1 FROM tool_calls WHERE session_id = sessions.id)
                 AND NOT EXISTS (SELECT 1 FROM usage_turns WHERE session_id = sessions.id)
                 AND prompt_token_count = 0 AND completion_token_count = 0"#,
        )
        .bind(session_id.as_i64())
        .execute(&self.pool)
        .await
        .context("Failed to retire clean lifecycle session")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_utils::TestStorage;

    async fn reserve(fixture: &TestStorage) -> SessionId {
        fixture
            .storage
            .start_lifecycle_session(
                fixture.project_path(),
                "anthropic",
                "claude-sonnet-4-5",
                ReasoningSelection::default(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn session_kind_startup_is_hidden_but_diagnosable() {
        let fixture = TestStorage::new().await;
        let id = reserve(&fixture).await;
        assert_eq!(
            fixture.storage.session_kind(id).await.unwrap(),
            SessionKind::LifecycleOnly
        );
        assert!(
            fixture
                .storage
                .recent_sessions_for_project(fixture.project_path(), 20)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            fixture
                .storage
                .latest_prior_session_for_project(fixture.project_path(), SessionId::from_raw(0))
                .await
                .unwrap()
                .is_none()
        );
        let diagnostic = fixture
            .storage
            .sessions_for_project(fixture.project_path(), 20, SessionListScope::All)
            .await
            .unwrap();
        assert_eq!(diagnostic.len(), 1);
        assert_eq!(diagnostic[0].id, id);
        assert_eq!(diagnostic[0].kind, SessionKind::LifecycleOnly);
    }

    #[tokio::test]
    async fn session_kind_concurrent_first_tasks_preserve_identity_and_one_active_run() {
        let fixture = TestStorage::new().await;
        let id = reserve(&fixture).await;
        let key = fixture.storage.conversation_cache_key(id).await.unwrap();
        let peer = reserve(&fixture).await;
        fixture
            .storage
            .send_peer_message(
                fixture.project_path(),
                peer,
                id,
                PeerMessageKind::Text,
                "Peer identity before the first user turn",
                0,
            )
            .await
            .unwrap();
        let (first, second) = tokio::join!(
            fixture.storage.start_task_run(id, None, "First task"),
            fixture.storage.start_task_run(id, None, "Second task"),
        );
        assert_eq!(first.unwrap().session_id, id);
        assert_eq!(second.unwrap().session_id, id);
        assert_eq!(
            fixture.storage.session_kind(id).await.unwrap(),
            SessionKind::Task
        );
        assert_eq!(
            fixture.storage.conversation_cache_key(id).await.unwrap(),
            key
        );
        assert_eq!(
            fixture
                .storage
                .pending_agent_message_count(id)
                .await
                .unwrap(),
            1
        );
        fixture
            .storage
            .mark_session_status(peer, SessionStatus::Completed)
            .await
            .unwrap();
        fixture
            .storage
            .retire_clean_lifecycle_session(peer)
            .await
            .unwrap();
        assert!(
            fixture
                .storage
                .session_summary(peer)
                .await
                .unwrap()
                .is_some(),
            "peer audit references must survive clean exit"
        );
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM task_runs WHERE session_id = ? AND outcome IS NULL",
        )
        .bind(id.as_i64())
        .fetch_one(&fixture.storage.pool)
        .await
        .unwrap();
        assert_eq!(active, 1);
        assert_eq!(
            fixture
                .storage
                .recent_sessions_for_project(fixture.project_path(), 20)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn session_kind_provider_probe_does_not_claim_or_downgrade_work() {
        let fixture = TestStorage::new().await;
        let id = reserve(&fixture).await;
        fixture.storage.record_provider_probe(id).await.unwrap();
        assert_eq!(
            fixture.storage.session_kind(id).await.unwrap(),
            SessionKind::ProviderProbe
        );
        assert!(
            fixture
                .storage
                .load_usage_dashboard()
                .await
                .unwrap()
                .status_counts
                .is_empty()
        );
        fixture
            .storage
            .start_task_run(id, None, "Explain this project")
            .await
            .unwrap();
        fixture.storage.record_provider_probe(id).await.unwrap();
        assert_eq!(
            fixture.storage.session_kind(id).await.unwrap(),
            SessionKind::Task
        );
    }

    #[tokio::test]
    async fn session_kind_clean_retention_removes_empty_cohort_and_preserves_failures() {
        let fixture = TestStorage::new().await;
        for _ in 0..50 {
            let id = reserve(&fixture).await;
            fixture
                .storage
                .mark_session_status(id, SessionStatus::Completed)
                .await
                .unwrap();
            fixture
                .storage
                .retire_clean_lifecycle_session(id)
                .await
                .unwrap();
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
            .fetch_one(&fixture.storage.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "clean startup-only rows must not accumulate");
        for status in [
            SessionStatus::Active,
            SessionStatus::Interrupted,
            SessionStatus::Failed,
        ] {
            let id = reserve(&fixture).await;
            fixture
                .storage
                .mark_session_status(id, status)
                .await
                .unwrap();
            fixture
                .storage
                .retire_clean_lifecycle_session(id)
                .await
                .unwrap();
            assert!(fixture.storage.session_summary(id).await.unwrap().is_some());
        }
        let task = fixture.start_session().await;
        fixture
            .storage
            .mark_session_status(task, SessionStatus::Completed)
            .await
            .unwrap();
        fixture
            .storage
            .retire_clean_lifecycle_session(task)
            .await
            .unwrap();
        assert_eq!(
            fixture.storage.session_kind(task).await.unwrap(),
            SessionKind::Task
        );
    }

    #[tokio::test]
    async fn session_kind_database_rejects_evidence_without_task_promotion() {
        let fixture = TestStorage::new().await;
        let id = reserve(&fixture).await;
        for query in [
            "INSERT INTO task_runs(session_id, goal_id, goal, started_at_ms) VALUES (?, 'goal', 'work', 1)",
            "INSERT INTO verification_runs(session_id, seq) VALUES (?, 0)",
            "INSERT INTO self_review_runs(session_id, seq) VALUES (?, 0)",
            "INSERT INTO todos(session_id, seq, content, status) VALUES (?, 0, 'work', 'pending')",
            "INSERT INTO episodes(session_id, seq) VALUES (?, 1)",
        ] {
            let err = sqlx::query(sqlx::AssertSqlSafe(query.to_string()))
                .bind(id.as_i64())
                .execute(&fixture.storage.pool)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("requires a task session"), "{err}");
        }
    }

    #[tokio::test]
    async fn session_kind_idle_snapshot_is_inert_and_user_snapshot_promotes() {
        use crate::session_persist::{
            SessionSnapshotData, SessionSnapshotSignatures, SessionSnapshotWriter,
        };
        let fixture = TestStorage::new().await;
        let id = reserve(&fixture).await;
        let plan = crate::plan::PlanDoc::default();
        let writer = SessionSnapshotWriter::new(&fixture.storage, id);
        let data = |transcript| SessionSnapshotData {
            transcript,
            plan: &plan,
            todos: &[],
            agent: None,
            fallback_usage: None,
            ui_peer_delivery_receipts: &[],
            agent_peer_delivery_receipts: &[],
        };
        let idle = [TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Status,
            text: "Startup ready".to_string(),
        }];
        assert!(
            !writer
                .persist(data(&idle), SessionSnapshotSignatures::default())
                .await
                .unwrap()
                .changed
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM transcript_blocks WHERE session_id = ?")
                .bind(id.as_i64())
                .fetch_one(&fixture.storage.pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
        let work = [TranscriptItem::UserMessage {
            text: "Explain the project".to_string(),
        }];
        assert!(
            writer
                .persist(data(&work), SessionSnapshotSignatures::default())
                .await
                .unwrap()
                .changed
        );
        assert_eq!(
            fixture.storage.session_kind(id).await.unwrap(),
            SessionKind::Task
        );
        let restored = fixture
            .storage
            .load_session_snapshot(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.transcript.len(), 1);
        assert_eq!(restored.summary.kind, SessionKind::Task);
    }

    #[tokio::test]
    async fn session_kind_migration_preserves_ambiguous_legacy_data() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        for migration in [
            include_str!("../../migrations/0001_initial.sql"),
            include_str!("../../migrations/0002_background_wake_subscriptions.sql"),
            include_str!("../../migrations/0003_verification_workspace_bindings.sql"),
            include_str!("../../migrations/0004_task_runs.sql"),
            include_str!("../../migrations/0005_self_review_lifecycle.sql"),
            include_str!("../../migrations/0006_episode_lifecycle_links.sql"),
            include_str!("../../migrations/0007_delegated_agent_models.sql"),
        ] {
            sqlx::raw_sql(sqlx::AssertSqlSafe(migration.to_string()))
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query(
            "INSERT INTO projects(id, path, display_name) VALUES (1, '/project', 'project')",
        )
        .execute(&pool)
        .await
        .unwrap();
        for id in 1..=2 {
            sqlx::query("INSERT INTO sessions(id, project_id, name, provider_id, model, reasoning_json, started_at_ms, updated_at_ms, conversation_cache_key) VALUES (?, 1, 'legacy', 'anthropic', 'model', '\"default\"', 1, 1, ?)")
                .bind(id).bind(format!("cache-{id}")).execute(&pool).await.unwrap();
        }
        // Migration 0004 backfilled these synthetic goals even for empty rows.
        sqlx::query("INSERT INTO task_runs(session_id, goal_id, goal, outcome, started_at_ms, ended_at_ms) VALUES (1, 'legacy-session:1', 'Historical session', 'unknown', 1, 1)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO messages(session_id, seq, role, content) VALUES (2, 0, 'user', 'Real task')").execute(&pool).await.unwrap();
        sqlx::raw_sql(include_str!("../../migrations/0008_session_kinds.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM sessions ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(kinds, ["legacy_unclassified", "task"]);
        let goals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_runs WHERE session_id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(goals, 1, "ambiguous data must survive migration");
    }

    #[tokio::test]
    async fn session_kind_failed_configuration_is_retained_without_task_evidence() {
        use crate::session_persist::{
            SessionSnapshotData, SessionSnapshotSignatures, SessionSnapshotWriter,
        };
        let fixture = TestStorage::new().await;
        let id = reserve(&fixture).await;
        fixture.storage.record_provider_probe(id).await.unwrap();
        let plan = crate::plan::PlanDoc::default();
        let transcript = [TranscriptItem::CommandOutput {
            kind: CommandOutputKind::Error,
            text: format!(
                "Authorization failed: ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa {}",
                "detail ".repeat(400)
            ),
        }];
        SessionSnapshotWriter::new(&fixture.storage, id)
            .persist(
                SessionSnapshotData {
                    transcript: &transcript,
                    plan: &plan,
                    todos: &[],
                    agent: None,
                    fallback_usage: None,
                    ui_peer_delivery_receipts: &[],
                    agent_peer_delivery_receipts: &[],
                },
                SessionSnapshotSignatures::default(),
            )
            .await
            .unwrap();
        fixture
            .storage
            .mark_session_status(id, SessionStatus::Completed)
            .await
            .unwrap();
        fixture
            .storage
            .retire_clean_lifecycle_session(id)
            .await
            .unwrap();
        let summary = fixture.storage.session_summary(id).await.unwrap().unwrap();
        assert_eq!(summary.kind, SessionKind::ProviderProbe);
        let detail = summary.lifecycle_diagnostic.unwrap();
        assert!(detail.contains("REDACTED"));
        assert!(!detail.contains("ghp_"));
        assert!(detail.chars().count() <= 1024);
        assert!(
            fixture
                .storage
                .load_usage_dashboard()
                .await
                .unwrap()
                .status_counts
                .is_empty()
        );
    }

    #[tokio::test]
    async fn session_kind_legacy_task_backfill_is_excluded_from_quality_counts() {
        let fixture = TestStorage::new().await;
        let id = fixture.start_session().await;
        let task = fixture
            .storage
            .start_task_run(id, None, "Legacy backfill")
            .await
            .unwrap();
        fixture
            .storage
            .finish_task_run(task.id, TaskOutcome::Succeeded, None)
            .await
            .unwrap();
        sqlx::query("UPDATE sessions SET kind = 'legacy_unclassified' WHERE id = ?")
            .bind(id.as_i64())
            .execute(&fixture.storage.pool)
            .await
            .unwrap();
        let dashboard = fixture.storage.load_usage_dashboard().await.unwrap();
        assert!(dashboard.task_outcome_counts.is_empty());
        assert!(dashboard.status_counts.is_empty());
        assert_eq!(dashboard.session_stats.total_sessions, 0);
        fixture
            .storage
            .mark_session_status(id, SessionStatus::Completed)
            .await
            .unwrap();
        fixture
            .storage
            .retire_clean_lifecycle_session(id)
            .await
            .unwrap();
        assert!(fixture.storage.session_summary(id).await.unwrap().is_some());
    }
}
