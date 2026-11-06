use super::*;
use crate::goal::GoalRecord;

impl Storage {
    /// Recorded goal run time plus the elapsed portion of a verified live run.
    /// Paused gaps and abandoned process intervals are never invented.
    ///
    /// # Errors
    /// Returns an error if the task-run timing query fails.
    pub(crate) async fn goal_elapsed_ms(
        &self,
        session_id: SessionId,
        goal_id: &str,
        live_run: Option<(TaskRunId, Duration)>,
    ) -> Result<u64> {
        let live_run_id = live_run.map(|(id, _)| id.as_i64());
        let live_elapsed_ms = live_run.map_or(0, |(_, elapsed)| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        });
        let elapsed: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(CASE WHEN id = ? AND outcome IS NULL \
             THEN ? ELSE COALESCE(wall_ms, 0) END), 0) \
             FROM task_runs WHERE session_id = ? AND goal_id = ?",
        )
        .bind(live_run_id)
        .bind(live_elapsed_ms)
        .bind(session_id.as_i64())
        .bind(goal_id)
        .fetch_one(&self.pool)
        .await
        .context("Failed to load goal elapsed runtime")?;
        Ok(u64::try_from(elapsed).unwrap_or(0))
    }

    pub(crate) async fn current_goal(&self, session_id: SessionId) -> Result<Option<GoalRecord>> {
        let raw: Option<String> = sqlx::query_scalar(
            "SELECT record_json FROM session_goals WHERE session_id = ? AND is_current = 1",
        )
        .bind(session_id.as_i64())
        .fetch_optional(&self.pool)
        .await
        .context("Failed to load session goal")?;
        raw.map(|raw| serde_json::from_str(&raw).context("Failed to parse persisted session goal"))
            .transpose()
    }

    /// Keep cleared and superseded goals addressable alongside task evidence.
    pub(crate) async fn save_goal(
        &self,
        session_id: SessionId,
        record: Option<&GoalRecord>,
    ) -> Result<()> {
        let mut tx = self.begin_write().await?;
        let previous: Option<String> = sqlx::query_scalar(
            "SELECT record_json FROM session_goals WHERE session_id = ? AND is_current = 1",
        )
        .bind(session_id.as_i64())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(previous) = previous {
            let mut previous: GoalRecord =
                serde_json::from_str(&previous).context("Failed to parse outgoing goal")?;
            if record.is_none_or(|record| record.id != previous.id) {
                let (outcome, code, reason) = match record {
                    Some(next) => (
                        "superseded",
                        "goal_superseded",
                        format!("Superseded by goal {}.", next.id),
                    ),
                    None => (
                        "cancelled",
                        "user_cancelled",
                        "Goal cleared by user; history retained.".to_string(),
                    ),
                };
                if previous.state == crate::goal::GoalState::Active {
                    previous.state = crate::goal::GoalState::Paused;
                }
                previous.reason = Some(reason.clone());
                previous.updated_at_ms = now_ms();
                sqlx::query("UPDATE session_goals SET record_json = ? WHERE goal_id = ?")
                    .bind(serde_json::to_string(&previous)?)
                    .bind(&previous.id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE task_runs SET outcome = ?, terminal_reason_code = ?, terminal_reason_detail = ?, ended_at_ms = MAX(?, started_at_ms) WHERE session_id = ? AND goal_id = ? AND outcome IS NULL")
                    .bind(outcome).bind(code).bind(reason).bind(now_ms()).bind(session_id.as_i64()).bind(&previous.id)
                    .execute(&mut *tx).await.context("Failed to close outgoing goal task")?;
            }
        }
        sqlx::query("UPDATE session_goals SET is_current = 0 WHERE session_id = ?")
            .bind(session_id.as_i64())
            .execute(&mut *tx)
            .await?;
        if let Some(record) = record {
            sqlx::query("INSERT INTO session_goals(goal_id, session_id, is_current, record_json) VALUES (?, ?, 1, ?) ON CONFLICT(goal_id) DO UPDATE SET is_current = 1, record_json = excluded.record_json")
                .bind(&record.id).bind(session_id.as_i64()).bind(serde_json::to_string(record)?)
                .execute(&mut *tx).await.context("Failed to save session goal")?;
        }
        tx.commit().await.context("Failed to commit session goal")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::{GoalService, GoalState};

    #[tokio::test]
    async fn goals_survive_restore_clear_and_replacement() {
        let fixture = test_utils::TestStorage::new().await;
        let session_id = fixture.start_session().await;
        let service = GoalService::default();
        service
            .bind(fixture.storage.clone(), session_id)
            .await
            .unwrap();
        let first = service
            .set("Ship the full feature\nincluding tests")
            .await
            .unwrap();
        let restored = GoalService::default();
        restored
            .bind(fixture.storage.clone(), session_id)
            .await
            .unwrap();
        let record = restored.snapshot().await.unwrap();
        assert_eq!(record.objective, first.objective);
        assert_eq!(record.state, GoalState::Paused);
        restored.set("Next feature").await.unwrap();
        restored.clear().await.unwrap();
        assert!(
            fixture
                .storage
                .current_goal(session_id)
                .await
                .unwrap()
                .is_none()
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM session_goals WHERE session_id = ?")
                .bind(session_id.as_i64())
                .fetch_one(&fixture.storage.pool)
                .await
                .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn goal_timer_combines_recorded_runs_with_only_the_verified_live_run() {
        let fixture = test_utils::TestStorage::new().await;
        let session = fixture.start_session().await;
        let first = fixture
            .storage
            .start_task_run_with_goal_id(session, None, "goal-a", "First run")
            .await
            .unwrap();
        sqlx::query("UPDATE task_runs SET wall_ms = 2500 WHERE id = ?")
            .bind(first.id.as_i64())
            .execute(&fixture.storage.pool)
            .await
            .unwrap();
        fixture
            .storage
            .finish_task_run(first.id, TaskOutcome::Succeeded, None)
            .await
            .unwrap();
        let other = fixture
            .storage
            .start_task_run_with_goal_id(session, None, "goal-b", "Unrelated work")
            .await
            .unwrap();
        sqlx::query("UPDATE task_runs SET wall_ms = 99999 WHERE id = ?")
            .bind(other.id.as_i64())
            .execute(&fixture.storage.pool)
            .await
            .unwrap();
        fixture
            .storage
            .finish_task_run(other.id, TaskOutcome::Succeeded, None)
            .await
            .unwrap();
        let live = fixture
            .storage
            .start_task_run_with_goal_id(session, None, "goal-a", "Resume")
            .await
            .unwrap();
        // A partial checkpoint must not be counted again on top of live wall time.
        sqlx::query("UPDATE task_runs SET started_at_ms = 10000, wall_ms = 500 WHERE id = ?")
            .bind(live.id.as_i64())
            .execute(&fixture.storage.pool)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .storage
                .goal_elapsed_ms(
                    session,
                    "goal-a",
                    Some((live.id, Duration::from_millis(3000)))
                )
                .await
                .unwrap(),
            5500
        );
        assert_eq!(
            fixture
                .storage
                .goal_elapsed_ms(session, "goal-a", None)
                .await
                .unwrap(),
            3000
        );
        assert_eq!(
            fixture
                .storage
                .goal_elapsed_ms(
                    session,
                    "goal-b",
                    Some((live.id, Duration::from_millis(3000)))
                )
                .await
                .unwrap(),
            99999
        );
    }

    #[tokio::test]
    async fn goal_timer_does_not_charge_paused_or_crashed_time_after_restore() {
        let fixture = test_utils::TestStorage::new().await;
        let session = fixture.start_session().await;
        let service = GoalService::default();
        service
            .bind(fixture.storage.clone(), session)
            .await
            .unwrap();
        let goal = service.set("Deliver feature").await.unwrap();
        let run = fixture
            .storage
            .start_task_run_with_goal_id(session, None, &goal.id, &goal.objective)
            .await
            .unwrap();
        sqlx::query("UPDATE task_runs SET started_at_ms = 1, wall_ms = 4000 WHERE id = ?")
            .bind(run.id.as_i64())
            .execute(&fixture.storage.pool)
            .await
            .unwrap();
        let restored = GoalService::default();
        restored
            .bind(fixture.storage.clone(), session)
            .await
            .unwrap();
        assert_eq!(restored.snapshot().await.unwrap().state, GoalState::Paused);
        assert_eq!(
            fixture
                .storage
                .goal_elapsed_ms(session, &goal.id, None)
                .await
                .unwrap(),
            4000
        );
    }
}
