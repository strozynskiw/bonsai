//! Session objectives shared by the engine and interactive surfaces.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::storage::{SessionId, Storage};

pub(crate) const FOREGROUND_INTERRUPTED: &str = "Foreground run interrupted by user.";

/// Lifecycle of an explicitly authorized session objective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum GoalState {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl GoalState {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::UsageLimited => "usage-limited",
            Self::BudgetLimited => "budget-limited",
            Self::Complete => "complete",
        }
    }
}

/// Durable identity and intent. Usage and execution evidence remain in the
/// existing session and task-run ledgers, linked by `id` (`task_runs.goal_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct GoalRecord {
    pub(crate) id: String,
    pub(crate) objective: String,
    pub(crate) state: GoalState,
    pub(crate) reason: Option<String>,
    pub(crate) question_timeout_seconds: u64,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
}

#[derive(Default)]
struct GoalData {
    record: Option<GoalRecord>,
    binding: Option<(Storage, SessionId)>,
    automatic_work_suspended: bool,
}

/// A separately locked control plane, accessible while the agent owns its run
/// lock. Mutations persist before becoming visible and retain superseded rows.
#[derive(Clone, Default)]
pub(crate) struct GoalService(Arc<Mutex<GoalData>>, Arc<AtomicBool>);

impl std::fmt::Debug for GoalService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoalService").finish_non_exhaustive()
    }
}

impl GoalService {
    /// Only an explicit human turn can authorize unrelated work after a goal
    /// was stopped or cleared. Merely observing a child event cannot do so.
    pub(crate) async fn begin_human_turn(&self, authorizes_work: bool) {
        let mut data = self.0.lock().await;
        if authorizes_work
            && data
                .record
                .as_ref()
                .is_none_or(|record| record.state != GoalState::Active)
        {
            data.automatic_work_suspended = false;
        }
    }

    pub(crate) async fn automatic_work_allowed(&self) -> bool {
        !self.0.lock().await.automatic_work_suspended && !self.start_requested()
    }
    /// Drop process-local attachment when the durable session changes.
    pub(crate) async fn detach(&self) {
        self.cancel_start();
        *self.0.lock().await = GoalData::default();
    }

    /// Surface-level timeout/accounting gates can refine an engine terminal.
    /// Explicit user pause and objective replacement always take precedence.
    pub(crate) async fn reconcile_terminal(
        &self,
        id: &str,
        state: GoalState,
        reason: &str,
    ) -> anyhow::Result<()> {
        let mut data = self.0.lock().await;
        let Some(mut record) = data.record.clone().filter(|goal| goal.id == id) else {
            return Ok(());
        };
        if !matches!(record.state, GoalState::Active | GoalState::Complete)
            && !(record.state == GoalState::Paused
                && record.reason.as_deref() == Some(FOREGROUND_INTERRUPTED))
        {
            return Ok(());
        }
        record.state = state;
        record.reason = Some(crate::redact::redact(reason).chars().take(2048).collect());
        record.updated_at_ms = crate::util::time::now_ms();
        Self::save(&data, Some(&record)).await?;
        data.automatic_work_suspended = state != GoalState::Active;
        data.record = Some(record);
        Ok(())
    }
    pub(crate) fn request_start(&self) {
        self.1.store(true, Ordering::Release);
    }

    pub(crate) fn start_requested(&self) -> bool {
        self.1.load(Ordering::Acquire)
    }

    pub(crate) fn cancel_start(&self) {
        self.1.store(false, Ordering::Release);
    }

    pub(crate) async fn take_requested_start(&self) -> Option<GoalRecord> {
        if !self.1.swap(false, Ordering::AcqRel) {
            return None;
        }
        self.snapshot()
            .await
            .filter(|goal| goal.state == GoalState::Active)
    }
    pub(crate) async fn snapshot(&self) -> Option<GoalRecord> {
        self.0.lock().await.record.clone()
    }

    /// Attach a durable session without starting work. Interrupted active work
    /// requires an explicit resume; budget and blocked states survive intact.
    pub(crate) async fn bind(&self, storage: Storage, session_id: SessionId) -> anyhow::Result<()> {
        let mut data = self.0.lock().await;
        self.cancel_start();
        // A failed target load must never leave a control handle attached to
        // the outgoing session after the UI has switched session identity.
        data.record = None;
        data.binding = Some((storage.clone(), session_id));
        let mut record = storage.current_goal(session_id).await?;
        if let Some(goal) = record.as_mut()
            && goal.state == GoalState::Active
        {
            goal.state = GoalState::Paused;
            goal.reason = Some("Session restored; use /goal resume to continue.".into());
            goal.updated_at_ms = crate::util::time::now_ms();
            storage.save_goal(session_id, Some(goal)).await?;
        }
        data.record = record;
        data.automatic_work_suspended = data.record.is_some();
        data.binding = Some((storage, session_id));
        Ok(())
    }

    /// Establish a new audit boundary even when replacing a paused goal.
    pub(crate) async fn set(&self, objective: &str) -> anyhow::Result<GoalRecord> {
        let objective = objective.trim();
        anyhow::ensure!(!objective.is_empty(), "Goal objective must not be empty");
        anyhow::ensure!(
            objective.chars().count() <= 16_384,
            "Goal objective exceeds 16384 characters"
        );
        let now = crate::util::time::now_ms();
        let record = GoalRecord {
            id: uuid::Uuid::now_v7().to_string(),
            objective: objective.to_string(),
            state: GoalState::Active,
            reason: None,
            question_timeout_seconds: 60,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let mut data = self.0.lock().await;
        Self::save(&data, Some(&record)).await?;
        data.automatic_work_suspended = false;
        data.record = Some(record.clone());
        Ok(record)
    }

    pub(crate) async fn clear(&self) -> anyhow::Result<()> {
        let mut data = self.0.lock().await;
        Self::save(&data, None).await?;
        data.automatic_work_suspended = true;
        data.record = None;
        Ok(())
    }

    /// Change only the matching goal. A late run cannot complete its replacement
    /// or undo a user pause that arrived while a provider request was finishing.
    pub(crate) async fn transition(
        &self,
        id: &str,
        state: GoalState,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        let mut data = self.0.lock().await;
        let Some(mut record) = data.record.clone().filter(|goal| goal.id == id) else {
            return Ok(());
        };
        if record.state != GoalState::Active && state != GoalState::Active {
            return Ok(());
        }
        anyhow::ensure!(
            state != GoalState::Active
                || !matches!(
                    record.state,
                    GoalState::BudgetLimited | GoalState::UsageLimited | GoalState::Complete
                ),
            "This goal cannot resume from {}; inspect its stop reason and configured limits first",
            record.state.label()
        );
        record.state = state;
        record.reason = reason.map(|text| crate::redact::redact(text).chars().take(2048).collect());
        record.updated_at_ms = crate::util::time::now_ms();
        Self::save(&data, Some(&record)).await?;
        data.automatic_work_suspended = state != GoalState::Active;
        data.record = Some(record);
        Ok(())
    }

    async fn save(data: &GoalData, goal: Option<&GoalRecord>) -> anyhow::Result<()> {
        if let Some((storage, session_id)) = &data.binding {
            storage.save_goal(*session_id, goal).await?;
        }
        Ok(())
    }
}

/// Parsed once by all command surfaces; objective text keeps its whitespace.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GoalCommand<'a> {
    Status,
    Set(&'a str),
    Pause,
    Resume,
    Clear,
}

impl<'a> GoalCommand<'a> {
    pub(crate) fn parse(input: &'a str) -> Option<Self> {
        let mut parts = input.trim().splitn(2, char::is_whitespace);
        if parts.next()? != "/goal" {
            return None;
        }
        Some(match parts.next().unwrap_or("").trim() {
            "" | "status" => Self::Status,
            "pause" => Self::Pause,
            "resume" => Self::Resume,
            "clear" => Self::Clear,
            objective => Self::Set(objective.strip_prefix("set ").unwrap_or(objective).trim()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stale_run_cannot_complete_replacement_or_paused_goal() {
        let service = GoalService::default();
        let old = service.set("First objective").await.unwrap();
        let current = service.set("Second objective").await.unwrap();
        service
            .transition(&old.id, GoalState::Complete, None)
            .await
            .unwrap();
        assert_eq!(service.snapshot().await.unwrap().id, current.id);
        service
            .transition(&current.id, GoalState::Paused, Some("User pause"))
            .await
            .unwrap();
        service
            .transition(&current.id, GoalState::Complete, None)
            .await
            .unwrap();
        assert_eq!(service.snapshot().await.unwrap().state, GoalState::Paused);
    }

    #[tokio::test]
    async fn resume_cannot_bypass_exhaustion() {
        let service = GoalService::default();
        let record = service.set("Keep going").await.unwrap();
        service
            .transition(&record.id, GoalState::BudgetLimited, Some("max turns"))
            .await
            .unwrap();
        assert!(
            service
                .transition(&record.id, GoalState::Active, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn pause_and_clear_do_not_wake_on_child_completion() {
        let service = GoalService::default();
        let record = service.set("Run background verification").await.unwrap();
        assert!(service.automatic_work_allowed().await);
        service
            .transition(&record.id, GoalState::Paused, Some("Paused by user."))
            .await
            .unwrap();
        assert!(!service.automatic_work_allowed().await);
        service.clear().await.unwrap();
        assert!(!service.automatic_work_allowed().await);
        service.begin_human_turn(false).await;
        assert!(
            !service.automatic_work_allowed().await,
            "a status question cannot restart stopped work"
        );
        service.begin_human_turn(true).await;
        assert!(
            service.automatic_work_allowed().await,
            "a new explicit human task can authorize its own background work"
        );
    }

    #[test]
    fn parsing_preserves_multiline_objective_and_command_boundary() {
        assert_eq!(
            GoalCommand::parse("/goal ship\nwith tests"),
            Some(GoalCommand::Set("ship\nwith tests"))
        );
        assert_eq!(GoalCommand::parse("/goalkeeper"), None);
        assert_eq!(
            GoalCommand::parse("/goal resume"),
            Some(GoalCommand::Resume)
        );
    }
}
