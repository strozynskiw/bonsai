use super::message_injection::harness_note;
use super::*;
use crate::goal::GoalState;

impl Agent {
    /// Reconstruct durable intent after every compaction and provider change.
    /// The note is append-only and reinserted only if it left working context.
    pub(super) async fn refresh_goal_context(&mut self) -> Result<bool> {
        let Some(run_id) = self.goal_run_id.as_deref() else {
            return Ok(true);
        };
        let Some(goal) = self.goal.snapshot().await else {
            return Ok(false);
        };
        if goal.state != GoalState::Active || goal.id != run_id {
            return Ok(false);
        }
        let note = harness_note(&format!(
            "Active session goal ({}):\n{}\n\n\
             Keep this full objective through compaction. New user messages are steering, \
             clarification, or status requests unless /goal explicitly replaces the objective. \
             Continue actionable work within the existing run and session limits; inspect current \
             files, todo state, and verification evidence before repeating work. A progress report \
             is not completion. Finish only when the entire objective and required checks are \
             satisfied; otherwise report the remaining blocker or exhausted limit. Ask questions \
             with the question tool, put the recommended option first, and continue independent \
             work when possible. Goal questions default to that recommendation after {} seconds. \
             A timeout never grants a permission or sandbox approval.",
            goal.id, goal.objective, goal.question_timeout_seconds,
        ));
        if !self.messages.iter().enumerate().any(|(index, message)| {
            message_content_string(message) == note
                && !self.message_has_control(index, message, |control| {
                    control.drop_next_turn || control.stubbed
                })
        }) {
            self.push_user_message_raw(&note);
        }
        if let Some(interaction) = &self.interaction {
            interaction.set_goal_question_timeout(Some(goal.question_timeout_seconds));
        }
        Ok(true)
    }

    pub(crate) async fn finish_goal_run(
        &self,
        id: &str,
        result: &Result<AgentRunResult>,
    ) -> Result<()> {
        let (state, reason) = match result {
            Ok(AgentRunResult::Completed(_)) => (GoalState::Complete, None),
            Ok(AgentRunResult::Interrupted(_)) => (
                GoalState::Paused,
                Some(crate::goal::FOREGROUND_INTERRUPTED.to_string()),
            ),
            Ok(AgentRunResult::Incomplete { failure, .. }) => {
                (GoalState::Blocked, Some(format!("{failure:?}")))
            }
            Ok(AgentRunResult::Waiting(_)) => (
                GoalState::Active,
                Some("Waiting for an external dependency.".to_string()),
            ),
            Err(error) => {
                let state = match error.downcast_ref::<crate::run_budget::RunBudgetExhaustion>() {
                    Some(
                        crate::run_budget::RunBudgetExhaustion::ProviderRateLimit { .. }
                        | crate::run_budget::RunBudgetExhaustion::ProviderQuota,
                    ) => GoalState::UsageLimited,
                    Some(_) => GoalState::BudgetLimited,
                    None => GoalState::Blocked,
                };
                (state, Some(format!("{error:#}")))
            }
        };
        self.goal.transition(id, state, reason.as_deref()).await
    }
}
