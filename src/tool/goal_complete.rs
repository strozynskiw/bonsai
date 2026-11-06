use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

use crate::goal::{GoalRunLease, GoalService};
use crate::tool::schema::{object, parse_args, string_property};
use crate::tool::{ParallelPolicy, Tool, ToolEffectPolicy, ToolOutput};

#[derive(Deserialize)]
struct GoalCompleteArgs {
    evidence: String,
}

/// Records a run-scoped completion claim without changing the durable goal.
pub(crate) struct GoalCompleteTool {
    goal: GoalService,
    lease: GoalRunLease,
}

impl GoalCompleteTool {
    pub(crate) fn new(goal: GoalService, lease: GoalRunLease) -> Self {
        Self { goal, lease }
    }
}

#[async_trait]
impl Tool for GoalCompleteTool {
    fn name(&self) -> &str {
        "goal_complete"
    }

    fn description(&self) -> &str {
        "Claim that the full active goal is complete only after checking its requirements and evidence. Do not call for a progress update, unfinished work, or an unresolved blocker. Call alone after all other work; the completion gate still checks outstanding work and verification. After the tool returns, provide a self-contained final answer with the requested result, even if you gave details alongside this call."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [(
                "evidence",
                string_property("Brief evidence that the full goal is complete"),
            )],
            &["evidence"],
        )
    }

    fn parallel_policy(&self) -> ParallelPolicy {
        ParallelPolicy::Serialized
    }

    fn effect_policy(&self) -> ToolEffectPolicy {
        ToolEffectPolicy::LocalState
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput> {
        let args: GoalCompleteArgs = parse_args("goal_complete", args)?;
        anyhow::ensure!(
            !args.evidence.trim().is_empty() && args.evidence.chars().count() <= 4096,
            "evidence must be nonempty and at most 4096 characters"
        );
        self.goal.claim_completion(&self.lease).await?;
        Ok(ToolOutput::Text(
            "Goal completion claimed; final checks and the active run still determine the outcome. Now provide a self-contained final answer that includes the requested result, rather than referring to an earlier message."
                .into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::GoalState;
    use serde_json::json;

    #[tokio::test]
    async fn claims_require_evidence_and_a_current_goal_lease() {
        let goal = GoalService::default();
        let record = goal.set("Inspect the requested files").await.unwrap();
        let lease = goal.active_run_lease().await.unwrap();
        let tool = GoalCompleteTool::new(goal.clone(), lease.clone());
        assert!(tool.execute(json!({"evidence": " "})).await.is_err());
        assert!(!goal.completion_claimed(&lease).await);
        let output = tool
            .execute(json!({"evidence": "All files were inspected."}))
            .await
            .unwrap();
        assert!(
            matches!(output, ToolOutput::Text(text) if text.contains("self-contained final answer"))
        );
        assert!(goal.completion_claimed(&lease).await);
        goal.transition(&record.id, GoalState::Paused, None)
            .await
            .unwrap();
        assert!(tool.execute(json!({"evidence": "Done"})).await.is_err());
    }
}
