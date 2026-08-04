use serde::{Deserialize, Serialize};

use super::*;

const MAX_GOAL_CHARS: usize = 4_096;
const MAX_REASON_DETAIL_CHARS: usize = 1_024;
const MAX_GOAL_ID_CHARS: usize = 128;

/// Stable identifier for one durable execution of a user goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskRunId(i64);

impl TaskRunId {
    pub const fn from_raw(id: i64) -> Self {
        Self(id)
    }

    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for TaskRunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Terminal result of a task run. An active run is represented by a missing
/// outcome in [`TaskRun::outcome`], never by overloading the session lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Succeeded,
    Blocked,
    Failed,
    Cancelled,
    Superseded,
    /// Historical rows whose outcome cannot be inferred without guessing.
    Unknown,
}

crate::impl_db_enum!(TaskOutcome {
    Succeeded => "succeeded",
    Blocked => "blocked",
    Failed => "failed",
    Cancelled => "cancelled",
    Superseded => "superseded",
    Unknown => "unknown",
} else Unknown);

impl TaskOutcome {
    pub fn label(self) -> &'static str {
        self.as_db_str()
    }

    pub const fn requires_reason(self) -> bool {
        matches!(
            self,
            Self::Blocked | Self::Failed | Self::Cancelled | Self::Superseded
        )
    }
}

/// Machine-readable reason for a non-success task result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskTerminalReasonCode {
    GoalSuperseded,
    UserCancelled,
    BudgetExhausted,
    ProviderFailure,
    ExecutionFailure,
    VerificationFailure,
    ProcessInterrupted,
    SessionEnded,
}

crate::impl_db_enum!(TaskTerminalReasonCode {
    GoalSuperseded => "goal_superseded",
    UserCancelled => "user_cancelled",
    BudgetExhausted => "budget_exhausted",
    ProviderFailure => "provider_failure",
    ExecutionFailure => "execution_failure",
    VerificationFailure => "verification_failure",
    ProcessInterrupted => "process_interrupted",
    SessionEnded => "session_ended",
} else ExecutionFailure);

impl TaskTerminalReasonCode {
    const fn fallback_detail(self) -> &'static str {
        match self {
            Self::GoalSuperseded => "A newer user goal superseded this task.",
            Self::UserCancelled => "The user cancelled this task.",
            Self::BudgetExhausted => "The task stopped at a configured execution limit.",
            Self::ProviderFailure => "The provider failed before the task could complete.",
            Self::ExecutionFailure => "The task ended because execution failed.",
            Self::VerificationFailure => "Verification left the task incomplete.",
            Self::ProcessInterrupted => "The process ended before the task completed.",
            Self::SessionEnded => "The session ended before the task completed.",
        }
    }
}

/// Bounded, secret-redacted terminal explanation stored with a non-success
/// outcome. Consumers should group by `code` and treat `detail` as display text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskTerminalReason {
    pub code: TaskTerminalReasonCode,
    pub detail: String,
}

impl TaskTerminalReason {
    pub fn new(code: TaskTerminalReasonCode, detail: &str) -> Self {
        let detail = bounded_redacted_text(detail, MAX_REASON_DETAIL_CHARS);
        Self {
            code,
            detail: if detail.is_empty() {
                code.fallback_detail().to_string()
            } else {
                detail
            },
        }
    }
}

/// One persisted execution of a user goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRun {
    pub id: TaskRunId,
    pub session_id: SessionId,
    pub episode_seq: Option<usize>,
    pub goal_id: String,
    pub goal: String,
    pub outcome: Option<TaskOutcome>,
    pub terminal_reason: Option<TaskTerminalReason>,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
}

impl TaskRun {
    pub const fn is_active(&self) -> bool {
        self.outcome.is_none()
    }

    pub fn outcome_label(&self) -> &'static str {
        self.outcome.map_or("active", TaskOutcome::label)
    }
}

impl Storage {
    /// Start a new task and atomically supersede any still-active task owned by
    /// the same session. A terminal row is immutable and is never reopened.
    pub async fn start_task_run(
        &self,
        session_id: SessionId,
        episode_seq: Option<usize>,
        goal: &str,
    ) -> Result<TaskRun> {
        let goal_id = uuid::Uuid::now_v7().to_string();
        self.start_task_run_with_goal_id(session_id, episode_seq, &goal_id, goal)
            .await
    }

    /// Start a new execution attempt for the latest terminal task while
    /// preserving its stable goal id. An active task is returned as-is; resume
    /// callers can therefore choose not to create a new attempt at all.
    pub async fn retry_latest_task_run(&self, session_id: SessionId) -> Result<Option<TaskRun>> {
        let Some(latest) = self.latest_task_run_inner(session_id).await? else {
            return Ok(None);
        };
        if latest.is_active() {
            return Ok(Some(latest));
        }
        self.start_task_run_with_goal_id(
            session_id,
            latest.episode_seq,
            &latest.goal_id,
            &latest.goal,
        )
        .await
        .map(Some)
    }

    async fn start_task_run_with_goal_id(
        &self,
        session_id: SessionId,
        episode_seq: Option<usize>,
        goal_id: &str,
        goal: &str,
    ) -> Result<TaskRun> {
        let goal_id = bounded_redacted_text(goal_id, MAX_GOAL_ID_CHARS);
        anyhow::ensure!(!goal_id.is_empty(), "Task goal id cannot be empty");
        let goal = bounded_redacted_text(goal, MAX_GOAL_CHARS);
        anyhow::ensure!(!goal.is_empty(), "Task goal cannot be empty");
        let episode_seq = episode_seq
            .map(i64::try_from)
            .transpose()
            .context("Task episode sequence exceeds SQLite range")?;
        let now = now_ms();
        let mut tx = self
            .begin_write()
            .await
            .context("Failed to begin task-run transaction")?;

        sqlx::query(
            r#"
            UPDATE task_runs
            SET outcome = ?, terminal_reason_code = ?, terminal_reason_detail = ?, ended_at_ms = ?
            WHERE session_id = ? AND outcome IS NULL
            "#,
        )
        .bind(TaskOutcome::Superseded.as_db_str())
        .bind(TaskTerminalReasonCode::GoalSuperseded.as_db_str())
        .bind(TaskTerminalReasonCode::GoalSuperseded.fallback_detail())
        .bind(now)
        .bind(session_id.as_i64())
        .execute(&mut *tx)
        .await
        .with_context(|| format!("Failed to supersede active task for session {session_id}"))?;

        let id: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO task_runs (
              session_id, episode_seq, goal_id, goal, started_at_ms
            )
            VALUES (?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(session_id.as_i64())
        .bind(episode_seq)
        .bind(&goal_id)
        .bind(&goal)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .with_context(|| format!("Failed to start task for session {session_id}"))?;
        tx.commit()
            .await
            .context("Failed to commit task-run start")?;

        Ok(TaskRun {
            id: TaskRunId::from_raw(id),
            session_id,
            episode_seq: episode_seq.and_then(|seq| usize::try_from(seq).ok()),
            goal_id,
            goal,
            outcome: None,
            terminal_reason: None,
            started_at_ms: now,
            ended_at_ms: None,
        })
    }

    /// Finish an active task exactly once. If another shutdown path already
    /// terminalized it, return the stored result without rewriting history.
    pub async fn finish_task_run(
        &self,
        task_run_id: TaskRunId,
        outcome: TaskOutcome,
        terminal_reason: Option<&TaskTerminalReason>,
    ) -> Result<TaskRun> {
        let terminal_reason =
            terminal_reason.map(|reason| TaskTerminalReason::new(reason.code, &reason.detail));
        validate_terminal_outcome(outcome, terminal_reason.as_ref())?;
        let now = now_ms();
        sqlx::query(
            r#"
            UPDATE task_runs
            SET outcome = ?, terminal_reason_code = ?, terminal_reason_detail = ?, ended_at_ms = ?
            WHERE id = ? AND outcome IS NULL
            "#,
        )
        .bind(outcome.as_db_str())
        .bind(
            terminal_reason
                .as_ref()
                .map(|reason| reason.code.as_db_str()),
        )
        .bind(
            terminal_reason
                .as_ref()
                .map(|reason| reason.detail.as_str()),
        )
        .bind(now)
        .bind(task_run_id.as_i64())
        .execute(&self.pool)
        .await
        .with_context(|| format!("Failed to finish task run {task_run_id}"))?;

        self.task_run(task_run_id)
            .await?
            .with_context(|| format!("Task run {task_run_id} does not exist"))
    }

    /// Finish the active task for a session, if any, without touching an
    /// already-terminal result.
    pub async fn finish_active_task_run(
        &self,
        session_id: SessionId,
        outcome: TaskOutcome,
        terminal_reason: Option<&TaskTerminalReason>,
    ) -> Result<Option<TaskRun>> {
        let Some(task_run) = self.active_task_run(session_id).await? else {
            return Ok(None);
        };
        self.finish_task_run(task_run.id, outcome, terminal_reason)
            .await
            .map(Some)
    }

    pub async fn task_run(&self, task_run_id: TaskRunId) -> Result<Option<TaskRun>> {
        let row = sqlx::query("SELECT * FROM task_runs WHERE id = ?")
            .bind(task_run_id.as_i64())
            .fetch_optional(&self.pool)
            .await
            .with_context(|| format!("Failed to load task run {task_run_id}"))?;
        row.map(task_run_from_row).transpose()
    }

    pub async fn active_task_run(&self, session_id: SessionId) -> Result<Option<TaskRun>> {
        let row =
            sqlx::query("SELECT * FROM task_runs WHERE session_id = ? AND outcome IS NULL LIMIT 1")
                .bind(session_id.as_i64())
                .fetch_optional(&self.pool)
                .await
                .with_context(|| format!("Failed to load active task for session {session_id}"))?;
        row.map(task_run_from_row).transpose()
    }

    async fn latest_task_run_inner(&self, session_id: SessionId) -> Result<Option<TaskRun>> {
        let row = sqlx::query(
            "SELECT * FROM task_runs WHERE session_id = ? ORDER BY started_at_ms DESC, id DESC LIMIT 1",
        )
        .bind(session_id.as_i64())
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("Failed to load latest task for session {session_id}"))?;
        row.map(task_run_from_row).transpose()
    }

    #[cfg(test)]
    pub(crate) async fn latest_task_run(&self, session_id: SessionId) -> Result<Option<TaskRun>> {
        self.latest_task_run_inner(session_id).await
    }

    /// Add episode ownership after the agent has observed the opening user
    /// turn. The task may be inserted before that episode exists in memory.
    pub async fn attach_task_run_episode(
        &self,
        task_run_id: TaskRunId,
        episode_seq: usize,
    ) -> Result<()> {
        let episode_seq =
            i64::try_from(episode_seq).context("Task episode sequence exceeds SQLite range")?;
        sqlx::query("UPDATE task_runs SET episode_seq = ? WHERE id = ? AND episode_seq IS NULL")
            .bind(episode_seq)
            .bind(task_run_id.as_i64())
            .execute(&self.pool)
            .await
            .with_context(|| format!("Failed to attach episode to task run {task_run_id}"))?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn task_runs_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<TaskRun>> {
        let rows =
            sqlx::query("SELECT * FROM task_runs WHERE session_id = ? ORDER BY started_at_ms, id")
                .bind(session_id.as_i64())
                .fetch_all(&self.pool)
                .await
                .with_context(|| format!("Failed to list task runs for session {session_id}"))?;
        rows.into_iter().map(task_run_from_row).collect()
    }
}

fn validate_terminal_outcome(
    outcome: TaskOutcome,
    reason: Option<&TaskTerminalReason>,
) -> Result<()> {
    anyhow::ensure!(
        outcome != TaskOutcome::Unknown,
        "Unknown is reserved for migrated historical task runs"
    );
    anyhow::ensure!(
        outcome.requires_reason() == reason.is_some(),
        "Task outcome {} {} a terminal reason",
        outcome.label(),
        if outcome.requires_reason() {
            "requires"
        } else {
            "does not accept"
        }
    );
    if let Some(reason) = reason {
        anyhow::ensure!(
            !reason.detail.trim().is_empty(),
            "Task terminal reason detail cannot be empty"
        );
        anyhow::ensure!(
            reason.detail.chars().count() <= MAX_REASON_DETAIL_CHARS,
            "Task terminal reason detail is too long"
        );
    }
    Ok(())
}

fn task_run_from_row(row: sqlx::sqlite::SqliteRow) -> Result<TaskRun> {
    let episode_seq = row
        .try_get::<Option<i64>, _>("episode_seq")?
        .map(usize::try_from)
        .transpose()
        .context("Persisted task episode sequence is invalid")?;
    let outcome = row
        .try_get::<Option<String>, _>("outcome")?
        .map(|value| TaskOutcome::from_db_str(&value));
    let reason_code = row
        .try_get::<Option<String>, _>("terminal_reason_code")?
        .map(|value| TaskTerminalReasonCode::from_db_str(&value));
    let reason_detail = row.try_get::<Option<String>, _>("terminal_reason_detail")?;
    let terminal_reason = match (reason_code, reason_detail) {
        (Some(code), Some(detail)) => Some(TaskTerminalReason { code, detail }),
        (None, None) => None,
        _ => anyhow::bail!("Persisted task run has an incomplete terminal reason"),
    };
    Ok(TaskRun {
        id: TaskRunId::from_raw(row.try_get("id")?),
        session_id: SessionId::from_raw(row.try_get("session_id")?),
        episode_seq,
        goal_id: row.try_get("goal_id")?,
        goal: row.try_get("goal")?,
        outcome,
        terminal_reason,
        started_at_ms: row.try_get("started_at_ms")?,
        ended_at_ms: row.try_get("ended_at_ms")?,
    })
}

pub(super) fn latest_task_run_from_session_row(
    row: &sqlx::sqlite::SqliteRow,
    session_id: SessionId,
) -> Result<Option<TaskRun>> {
    let Some(id) = row.try_get::<Option<i64>, _>("latest_task_id")? else {
        return Ok(None);
    };
    let episode_seq = row
        .try_get::<Option<i64>, _>("latest_task_episode_seq")?
        .map(usize::try_from)
        .transpose()
        .context("Persisted latest-task episode sequence is invalid")?;
    let outcome = row
        .try_get::<Option<String>, _>("latest_task_outcome")?
        .map(|value| TaskOutcome::from_db_str(&value));
    let reason_code = row
        .try_get::<Option<String>, _>("latest_task_terminal_reason_code")?
        .map(|value| TaskTerminalReasonCode::from_db_str(&value));
    let reason_detail = row.try_get::<Option<String>, _>("latest_task_terminal_reason_detail")?;
    let terminal_reason = match (reason_code, reason_detail) {
        (Some(code), Some(detail)) => Some(TaskTerminalReason { code, detail }),
        (None, None) => None,
        _ => anyhow::bail!("Persisted latest task has an incomplete terminal reason"),
    };
    Ok(Some(TaskRun {
        id: TaskRunId::from_raw(id),
        session_id,
        episode_seq,
        goal_id: row.try_get("latest_task_goal_id")?,
        goal: row.try_get("latest_task_goal")?,
        outcome,
        terminal_reason,
        started_at_ms: row.try_get("latest_task_started_at_ms")?,
        ended_at_ms: row.try_get("latest_task_ended_at_ms")?,
    }))
}

fn bounded_redacted_text(value: &str, max_chars: usize) -> String {
    crate::redact::redact(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_detail_is_redacted_bounded_and_non_empty() {
        let secret = format!("ghp_{}", "a".repeat(40));
        let long = format!("provider returned {secret} {}", "x".repeat(2_000));
        let reason = TaskTerminalReason::new(TaskTerminalReasonCode::ProviderFailure, &long);
        assert!(!reason.detail.contains(&secret));
        assert!(reason.detail.contains("[REDACTED:GitHub token]"));
        assert!(reason.detail.chars().count() <= MAX_REASON_DETAIL_CHARS);

        let fallback = TaskTerminalReason::new(TaskTerminalReasonCode::UserCancelled, "  \n ");
        assert!(!fallback.detail.is_empty());
    }
}
