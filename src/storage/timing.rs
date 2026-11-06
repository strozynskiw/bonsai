//! Durability for per-task-run timing evidence (issue #146).
//!
//! [`crate::timing`] owns the interval arithmetic; this module owns the storage
//! shape. Two rules decide that shape:
//!
//! - Category totals are **additive**. A checkpoint adds increments instead of
//!   replacing totals, so a task run resumed in a new process after a crash
//!   keeps whatever the interrupted segment already recorded.
//! - Open intervals are **never invented**. Restart reconciliation clears the
//!   open markers and counts them as recovered, leaving unknown time out of the
//!   accounting rather than charging a run for a gap that may have been a dead
//!   process, a suspended machine, or an abandoned session.

use super::task_runs::bounded_redacted_text;
use super::*;
use crate::timing::{
    CategoryTiming, MAX_OWNER_CHARS, MAX_REASON_CHARS, TimingCategory, TimingCheckpoint,
    TimingLane, TimingPolicy,
};

/// A task run's persisted timing evidence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TaskRunTiming {
    /// Clock the run's budgets consumed. `None` when the run never reached a
    /// checkpoint, or predates timing evidence.
    pub(crate) policy: Option<TimingPolicy>,
    /// Run clock elapsed across every recorded segment.
    pub(crate) wall_ms: i64,
    /// Union of every interval: what the run actually took.
    pub(crate) critical_path_ms: i64,
    /// Union of model, active tool, and child-active intervals.
    pub(crate) active_compute_ms: i64,
    /// Union of queue, permission, user, and child-wait intervals.
    pub(crate) waiting_ms: i64,
    /// Intervals restart reconciliation had to abandon, whose duration is
    /// unknown. Non-zero means the totals above undercount real time.
    pub(crate) recovered_interval_count: u32,
    pub(crate) categories: Vec<CategoryTiming>,
}

impl TaskRunTiming {
    /// Summed resource time in one category, across lanes. Test-only for now:
    /// the headless surface serializes the per-category rows themselves.
    #[cfg(test)]
    pub(crate) fn summed_ms(&self, category: TimingCategory) -> i64 {
        self.categories
            .iter()
            .filter(|row| row.category == category)
            .fold(0_i64, |total, row| total.saturating_add(row.summed_ms))
    }
}

/// Runtime totals summed across every task run of a task session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuntimeTimingTotals {
    /// Run clock elapsed across every recorded segment.
    pub wall_ms: i64,
    /// Union of every interval: what the runs actually took.
    pub critical_path_ms: i64,
    /// Model, active tool, and child-active time.
    pub active_compute_ms: i64,
    /// Queue, permission, user, and child-wait time.
    pub waiting_ms: i64,
}

/// The runtime breakdown the `/usage` report renders (issue #146).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeTiming {
    pub totals: RuntimeTimingTotals,
    /// Summed resource time per category, in report order.
    pub categories: Vec<(TimingCategory, i64)>,
}

impl RuntimeTiming {
    /// The runtime breakdown for one cohort of task runs.
    pub fn new(totals: RuntimeTimingTotals, categories: Vec<(TimingCategory, i64)>) -> Self {
        Self { totals, categories }
    }

    /// Summed resource time in one category.
    pub fn category_ms(&self, category: TimingCategory) -> i64 {
        self.categories
            .iter()
            .find(|(kind, _)| *kind == category)
            .map_or(0, |(_, ms)| *ms)
    }

    /// Share of run wall time that went to waiting rather than working. This is
    /// the number the active-compute budget policy exists to keep visible: a
    /// run cannot spend most of its wall clock waiting and still read as
    /// expensive work.
    pub fn waiting_share_percent(&self) -> Option<u8> {
        if self.totals.wall_ms <= 0 {
            return None;
        }
        let percent = self.totals.waiting_ms.saturating_mul(100) / self.totals.wall_ms;
        Some(u8::try_from(percent).unwrap_or(100))
    }

    /// Whether any run recorded timing at all.
    pub fn has_data(&self) -> bool {
        self.totals.wall_ms > 0 || !self.categories.is_empty()
    }
}

impl Storage {
    /// Add one checkpoint's increments to a task run's timing evidence.
    ///
    /// Everything in one transaction: category increments, the live open
    /// markers, and the run-level totals. A checkpoint that records nothing
    /// still clears stale open markers, so a category that finished between two
    /// checkpoints stops advertising in-flight work.
    pub(crate) async fn checkpoint_task_run_timing(
        &self,
        task_run_id: TaskRunId,
        checkpoint: &TimingCheckpoint,
    ) -> Result<()> {
        let mut transaction = self
            .begin_write()
            .await
            .context("Failed to begin task-run timing transaction")?;

        // `open_count` is a live reading, not an increment: reset the previous
        // checkpoint's markers before writing this checkpoint's.
        storage_op!(
            &mut transaction,
            "clear open task-run timing markers",
            sqlx::query(
                "UPDATE task_run_timings SET open_count = 0 \
                 WHERE task_run_id = ? AND open_count > 0",
            )
            .bind(task_run_id.as_i64()),
        )?;

        for row in &checkpoint.categories {
            let owner = row
                .latest_owner
                .as_deref()
                .map(|value| bounded_redacted_text(value, MAX_OWNER_CHARS));
            let reason = row
                .latest_reason
                .as_deref()
                .map(|value| bounded_redacted_text(value, MAX_REASON_CHARS));
            storage_op!(
                &mut transaction,
                "record task-run timing category",
                sqlx::query(
                    r#"
                    INSERT INTO task_run_timings (
                      task_run_id, category, lane, summed_ms, interval_count,
                      open_count, latest_owner, latest_reason
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(task_run_id, category, lane) DO UPDATE SET
                      summed_ms = task_run_timings.summed_ms + excluded.summed_ms,
                      interval_count =
                        task_run_timings.interval_count + excluded.interval_count,
                      open_count = excluded.open_count,
                      latest_owner = COALESCE(
                        excluded.latest_owner, task_run_timings.latest_owner
                      ),
                      latest_reason = COALESCE(
                        excluded.latest_reason, task_run_timings.latest_reason
                      )
                    "#,
                )
                .bind(task_run_id.as_i64())
                .bind(row.category.as_db_str())
                .bind(row.lane.as_db_str())
                .bind(row.summed_ms)
                .bind(i64::from(row.interval_count))
                .bind(i64::from(row.open_count))
                .bind(owner)
                .bind(reason),
            )?;
        }

        storage_op!(
            &mut transaction,
            "record task-run timing totals",
            sqlx::query(
                r#"
                UPDATE task_runs
                SET wall_ms = COALESCE(wall_ms, 0) + ?,
                    critical_path_ms = COALESCE(critical_path_ms, 0) + ?,
                    active_compute_ms = COALESCE(active_compute_ms, 0) + ?,
                    wait_ms = COALESCE(wait_ms, 0) + ?,
                    timing_policy = ?
                WHERE id = ?
                "#,
            )
            .bind(checkpoint.wall_ms)
            .bind(checkpoint.critical_path_ms)
            .bind(checkpoint.active_compute_ms)
            .bind(checkpoint.waiting_ms)
            .bind(checkpoint.policy.as_db_str())
            .bind(task_run_id.as_i64()),
        )?;

        transaction
            .commit()
            .await
            .context("Failed to commit task-run timing checkpoint")?;
        Ok(())
    }

    /// Load a task run's timing evidence. `None` when the run does not exist.
    pub(crate) async fn task_run_timing(
        &self,
        task_run_id: TaskRunId,
    ) -> Result<Option<TaskRunTiming>> {
        let row = sqlx::query(
            "SELECT timing_policy, COALESCE(wall_ms, 0) AS wall_ms, \
             COALESCE(critical_path_ms, 0) AS critical_path_ms, \
             COALESCE(active_compute_ms, 0) AS active_compute_ms, \
             COALESCE(wait_ms, 0) AS wait_ms \
             FROM task_runs WHERE id = ?",
        )
        .bind(task_run_id.as_i64())
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("Failed to load timing for task run {task_run_id}"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let policy = row
            .try_get::<Option<String>, _>("timing_policy")?
            .as_deref()
            .and_then(TimingPolicy::from_db_str);
        let mut timing = TaskRunTiming {
            policy,
            wall_ms: row.try_get("wall_ms")?,
            critical_path_ms: row.try_get("critical_path_ms")?,
            active_compute_ms: row.try_get("active_compute_ms")?,
            waiting_ms: row.try_get("wait_ms")?,
            recovered_interval_count: 0,
            categories: Vec::new(),
        };

        let rows = sqlx::query(
            "SELECT category, lane, summed_ms, interval_count, open_count, \
             recovered_count, latest_owner, latest_reason \
             FROM task_run_timings WHERE task_run_id = ? \
             ORDER BY category, lane",
        )
        .bind(task_run_id.as_i64())
        .fetch_all(&self.pool)
        .await
        .with_context(|| format!("Failed to load timing rows for task run {task_run_id}"))?;
        for row in rows {
            let category_value: String = row.try_get("category")?;
            let lane_value: String = row.try_get("lane")?;
            // A category this build does not know is not attributed to a known
            // one: misreporting wait time as model work is the failure mode this
            // whole table exists to prevent.
            let (Some(category), Some(lane)) = (
                TimingCategory::from_db_str(&category_value),
                TimingLane::from_db_str(&lane_value),
            ) else {
                continue;
            };
            let recovered =
                u32::try_from(row.try_get::<i64, _>("recovered_count")?).unwrap_or(u32::MAX);
            timing.recovered_interval_count =
                timing.recovered_interval_count.saturating_add(recovered);
            timing.categories.push(CategoryTiming {
                category,
                lane,
                summed_ms: row.try_get("summed_ms")?,
                interval_count: u32::try_from(row.try_get::<i64, _>("interval_count")?)
                    .unwrap_or(u32::MAX),
                open_count: u32::try_from(row.try_get::<i64, _>("open_count")?).unwrap_or(u32::MAX),
                latest_owner: row.try_get("latest_owner")?,
                latest_reason: row.try_get("latest_reason")?,
            });
        }
        Ok(Some(timing))
    }

    /// Runtime totals and per-category resource time across every task run.
    ///
    /// Two sums with no JSON decode, so unlike the inspection cohort this can
    /// cover all history without costing the modal its interactive latency.
    pub(crate) async fn runtime_timing(&self) -> Result<RuntimeTiming> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(r.wall_ms), 0) AS wall_ms, \
             COALESCE(SUM(r.critical_path_ms), 0) AS critical_path_ms, \
             COALESCE(SUM(r.active_compute_ms), 0) AS active_compute_ms, \
             COALESCE(SUM(r.wait_ms), 0) AS wait_ms \
             FROM task_runs r JOIN sessions s ON s.id = r.session_id \
             WHERE s.kind = 'task'",
        )
        .fetch_one(&self.pool)
        .await
        .context("Failed to load runtime timing totals")?;
        let totals = RuntimeTimingTotals {
            wall_ms: row.try_get("wall_ms")?,
            critical_path_ms: row.try_get("critical_path_ms")?,
            active_compute_ms: row.try_get("active_compute_ms")?,
            waiting_ms: row.try_get("wait_ms")?,
        };

        let rows = sqlx::query(
            "SELECT t.category AS category, SUM(t.summed_ms) AS summed_ms \
             FROM task_run_timings t JOIN task_runs r ON r.id = t.task_run_id \
             JOIN sessions s ON s.id = r.session_id \
             WHERE s.kind = 'task' GROUP BY 1",
        )
        .fetch_all(&self.pool)
        .await
        .context("Failed to load runtime timing categories")?;
        let mut categories = Vec::with_capacity(rows.len());
        for row in rows {
            let raw: String = row.try_get("category")?;
            let Some(category) = TimingCategory::from_db_str(&raw) else {
                continue;
            };
            categories.push((category, row.try_get("summed_ms")?));
        }
        categories.sort_by_key(|(category, _)| *category);
        Ok(RuntimeTiming::new(totals, categories))
    }

    /// Abandon the timing intervals a dead process left open on this session.
    ///
    /// Scope is deliberately the whole session, including a task run that is
    /// still active: after a crash the abandoned row *is* the active one, so
    /// restricting this to terminal rows would leave every crashed run's open
    /// markers in place forever. A live owner cannot be reconciled by mistake —
    /// the session claim refuses to resume a session another process still
    /// shows a fresh heartbeat for.
    ///
    /// Exactly-once by construction: the update only matches rows that still
    /// carry open markers, so calling it again finds nothing to reconcile and
    /// returns zero. Interrupted intervals contribute no duration — the run
    /// undercounts instead of inventing time that may never have been spent.
    /// Returns the number of abandoned intervals.
    pub(crate) async fn reconcile_open_task_run_timings(
        &self,
        session_id: SessionId,
    ) -> Result<u32> {
        let mut transaction = self
            .begin_write()
            .await
            .context("Failed to begin task-run timing reconciliation")?;
        let open_intervals: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(open_count), 0) FROM task_run_timings \
             WHERE open_count > 0 \
               AND task_run_id IN (SELECT id FROM task_runs WHERE session_id = ?)",
        )
        .bind(session_id.as_i64())
        .fetch_one(&mut *transaction)
        .await
        .with_context(|| {
            format!("Failed to count open timing intervals for session {session_id}")
        })?;
        storage_op!(
            &mut transaction,
            "reconcile open task-run timing intervals",
            sqlx::query(
                "UPDATE task_run_timings \
                 SET recovered_count = recovered_count + open_count, open_count = 0 \
                 WHERE open_count > 0 \
                   AND task_run_id IN (SELECT id FROM task_runs WHERE session_id = ?)",
            )
            .bind(session_id.as_i64()),
        )?;
        transaction
            .commit()
            .await
            .context("Failed to commit task-run timing reconciliation")?;
        Ok(u32::try_from(open_intervals).unwrap_or(u32::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_utils::TestStorage;
    use crate::timing::{CategoryTimingDelta, TimingCheckpoint, TimingLedger};

    fn category_delta(
        category: TimingCategory,
        lane: TimingLane,
        summed_ms: i64,
        interval_count: u32,
        open_count: u32,
    ) -> CategoryTimingDelta {
        CategoryTimingDelta {
            category,
            lane,
            summed_ms,
            interval_count,
            open_count,
            latest_owner: None,
            latest_reason: None,
        }
    }

    fn checkpoint(
        categories: Vec<CategoryTimingDelta>,
        wall_ms: i64,
        active_compute_ms: i64,
    ) -> TimingCheckpoint {
        TimingCheckpoint {
            policy: TimingPolicy::ActiveCompute,
            wall_ms,
            critical_path_ms: active_compute_ms,
            active_compute_ms,
            waiting_ms: wall_ms.saturating_sub(active_compute_ms),
            categories,
            open_interval_count: 0,
        }
    }

    #[tokio::test]
    async fn outgoing_goal_keeps_final_timing_checkpoint() {
        for replace in [false, true] {
            let fixture = TestStorage::new().await;
            let session = fixture.start_session().await;
            let service = crate::goal::GoalService::default();
            service
                .bind(fixture.storage.clone(), session)
                .await
                .unwrap();
            let goal = service.set("First goal").await.unwrap();
            let task = fixture
                .storage
                .start_task_run_with_goal_id(session, None, &goal.id, &goal.objective)
                .await
                .unwrap();
            fixture
                .storage
                .checkpoint_task_run_timing(task.id, &checkpoint(vec![], 500, 300))
                .await
                .unwrap();

            if replace {
                service.set("Next goal").await.unwrap();
            } else {
                service.clear().await.unwrap();
            }
            assert!(
                fixture
                    .storage
                    .task_run(task.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .outcome
                    .is_some()
            );
            fixture
                .storage
                .checkpoint_task_run_timing(task.id, &checkpoint(vec![], 700, 400))
                .await
                .unwrap();
            assert_eq!(
                fixture
                    .storage
                    .goal_elapsed_ms(session, &goal.id, None)
                    .await
                    .unwrap(),
                1200
            );
        }
    }

    #[tokio::test]
    async fn checkpoints_add_increments_instead_of_overwriting_totals() {
        let fixture = TestStorage::new().await;
        let session = fixture.start_session().await;
        let task = fixture
            .storage
            .start_task_run(session, None, "Measure the run")
            .await
            .unwrap();

        fixture
            .storage
            .checkpoint_task_run_timing(
                task.id,
                &checkpoint(
                    vec![category_delta(
                        TimingCategory::ModelRequest,
                        TimingLane::Parent,
                        1_000,
                        2,
                        0,
                    )],
                    1_400,
                    1_000,
                ),
            )
            .await
            .unwrap();
        fixture
            .storage
            .checkpoint_task_run_timing(
                task.id,
                &checkpoint(
                    vec![category_delta(
                        TimingCategory::ModelRequest,
                        TimingLane::Parent,
                        500,
                        1,
                        0,
                    )],
                    900,
                    500,
                ),
            )
            .await
            .unwrap();

        let timing = fixture
            .storage
            .task_run_timing(task.id)
            .await
            .unwrap()
            .expect("the task run exists");
        assert_eq!(timing.summed_ms(TimingCategory::ModelRequest), 1_500);
        assert_eq!(timing.wall_ms, 2_300);
        assert_eq!(timing.active_compute_ms, 1_500);
        assert_eq!(timing.waiting_ms, 800);
        assert_eq!(timing.policy, Some(TimingPolicy::ActiveCompute));
        assert_eq!(timing.recovered_interval_count, 0);
    }

    #[tokio::test]
    async fn a_checkpoint_with_no_intervals_clears_stale_open_markers() {
        let fixture = TestStorage::new().await;
        let session = fixture.start_session().await;
        let task = fixture
            .storage
            .start_task_run(session, None, "Wait for a tool")
            .await
            .unwrap();

        let mut open = checkpoint(
            vec![category_delta(
                TimingCategory::ToolActive,
                TimingLane::Parent,
                0,
                0,
                1,
            )],
            0,
            0,
        );
        open.open_interval_count = 1;
        fixture
            .storage
            .checkpoint_task_run_timing(task.id, &open)
            .await
            .unwrap();
        let timing = fixture
            .storage
            .task_run_timing(task.id)
            .await
            .unwrap()
            .expect("the task run exists");
        assert_eq!(timing.categories[0].open_count, 1);

        fixture
            .storage
            .checkpoint_task_run_timing(task.id, &checkpoint(Vec::new(), 10, 0))
            .await
            .unwrap();
        let timing = fixture
            .storage
            .task_run_timing(task.id)
            .await
            .unwrap()
            .expect("the task run exists");
        assert_eq!(timing.categories[0].open_count, 0);
        assert_eq!(timing.categories[0].interval_count, 0);
    }

    #[tokio::test]
    async fn reconciliation_abandons_open_intervals_exactly_once() {
        let fixture = TestStorage::new().await;
        let session = fixture.start_session().await;
        let task = fixture
            .storage
            .start_task_run(session, None, "Crash mid-interval")
            .await
            .unwrap();

        let open = TimingCheckpoint {
            policy: TimingPolicy::ActiveCompute,
            wall_ms: 5_000,
            critical_path_ms: 0,
            active_compute_ms: 0,
            waiting_ms: 0,
            categories: vec![category_delta(
                TimingCategory::UserWait,
                TimingLane::Parent,
                0,
                0,
                1,
            )],
            open_interval_count: 1,
        };
        fixture
            .storage
            .checkpoint_task_run_timing(task.id, &open)
            .await
            .unwrap();

        assert_eq!(
            fixture
                .storage
                .reconcile_open_task_run_timings(session)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            fixture
                .storage
                .reconcile_open_task_run_timings(session)
                .await
                .unwrap(),
            0,
            "a second reconciliation must not re-count the same interval"
        );

        let timing = fixture
            .storage
            .task_run_timing(task.id)
            .await
            .unwrap()
            .expect("the task run exists");
        assert_eq!(timing.categories[0].open_count, 0);
        assert_eq!(timing.recovered_interval_count, 1);
        assert_eq!(
            timing.summed_ms(TimingCategory::UserWait),
            0,
            "an interrupted interval must not invent a duration"
        );
    }

    #[tokio::test]
    async fn session_totals_sum_every_task_run_of_the_session() {
        let fixture = TestStorage::new().await;
        let session = fixture.start_session().await;
        let first = fixture
            .storage
            .start_task_run(session, None, "First goal")
            .await
            .unwrap();
        fixture
            .storage
            .finish_task_run(first.id, TaskOutcome::Succeeded, None)
            .await
            .unwrap();
        let second = fixture
            .storage
            .start_task_run(session, None, "Second goal")
            .await
            .unwrap();
        for (task, active, wait) in [(first.id, 1_000, 5_000), (second.id, 300, 200)] {
            fixture
                .storage
                .checkpoint_task_run_timing(
                    task,
                    &checkpoint(
                        vec![category_delta(
                            TimingCategory::ToolActive,
                            TimingLane::Parent,
                            active,
                            1,
                            0,
                        )],
                        active + wait,
                        active,
                    ),
                )
                .await
                .unwrap();
        }

        let runtime = fixture.storage.runtime_timing().await.unwrap();
        assert_eq!(runtime.totals.active_compute_ms, 1_300);
        assert_eq!(runtime.totals.waiting_ms, 5_200);
        assert_eq!(runtime.totals.wall_ms, 6_500);
        assert_eq!(runtime.category_ms(TimingCategory::ToolActive), 1_300);
        assert_eq!(runtime.waiting_share_percent(), Some(80));
    }

    #[tokio::test]
    async fn a_ledger_checkpoint_round_trips_through_storage() {
        let fixture = TestStorage::new().await;
        let session = fixture.start_session().await;
        let task = fixture
            .storage
            .start_task_run(session, None, "Record a real ledger")
            .await
            .unwrap();
        let clock = std::sync::Arc::new(crate::timing::FakeClock::new(1_700_000_000_000));
        let ledger = TimingLedger::new(clock.clone(), TimingPolicy::ActiveCompute);

        let model = ledger.begin(TimingCategory::ModelRequest, TimingLane::Parent, None, None);
        clock.advance_ms(1_200);
        assert_eq!(ledger.end(model), Some(1_200));
        let tools = ledger.begin(
            TimingCategory::ToolActive,
            TimingLane::Parent,
            Some("bash"),
            None,
        );
        clock.advance_ms(400);
        assert_eq!(ledger.end(tools), Some(400));

        fixture
            .storage
            .checkpoint_task_run_timing(task.id, &ledger.take_checkpoint())
            .await
            .unwrap();
        // Nothing new happened: the second checkpoint contributes nothing.
        let empty = ledger.take_checkpoint();
        assert_eq!(empty.categories.len(), 0);
        fixture
            .storage
            .checkpoint_task_run_timing(task.id, &empty)
            .await
            .unwrap();

        let timing = fixture
            .storage
            .task_run_timing(task.id)
            .await
            .unwrap()
            .expect("the task run exists");
        assert_eq!(timing.summed_ms(TimingCategory::ModelRequest), 1_200);
        assert_eq!(timing.summed_ms(TimingCategory::ToolActive), 400);
        assert_eq!(timing.active_compute_ms, 1_600);
        assert_eq!(
            timing.categories[1].latest_owner.as_deref(),
            Some("bash"),
            "the owner label survives persistence"
        );
    }
}
