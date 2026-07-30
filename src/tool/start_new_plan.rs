//! The `start_new_plan` tool starts a fresh canvas planning continuation. A
//! coding turn holds the agent lock and therefore cannot prepare the canvas or
//! swap its own registry in place, so this tool asks for confirmation and ends
//! the turn with [`WaitReason::StartNewPlan`]. The TUI event loop protects any
//! non-empty canvas in the saved-plan library, clears it, then re-dispatches the
//! conversation under the planning persona.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::agent::WaitReason;
use crate::interaction::{
    InteractionAnswer, InteractionOutcome, InteractionRequest, InteractionService, QuestionOption,
};
use crate::tool::schema::{object, parse_args, string_property};
use crate::tool::{Tool, ToolExecutionContext, ToolOutput};

#[derive(Deserialize)]
struct StartNewPlanArgs {
    /// A one-line description of what the model intends to plan, shown to the
    /// user in the confirmation prompt so the choice is informed. Optional.
    #[serde(default)]
    summary: Option<String>,
}

pub struct StartNewPlanTool {
    interaction: Arc<InteractionService>,
}

impl StartNewPlanTool {
    pub fn new(interaction: Arc<InteractionService>) -> Self {
        Self { interaction }
    }

    async fn run(
        &self,
        args: serde_json::Value,
        origin: Option<&str>,
    ) -> anyhow::Result<ToolOutput> {
        let args: StartNewPlanArgs = parse_args("start_new_plan tool", args)?;
        let summary = args
            .summary
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let prompt = match summary {
            Some(what) => format!(
                "This looks like planning work ({what}). Start a fresh plan on the live canvas? Any existing non-empty canvas will be saved automatically first."
            ),
            None => {
                "This looks like planning work. Start a fresh plan on the live canvas? Any existing non-empty canvas will be saved automatically first."
                    .to_string()
            }
        };
        let options = vec![
            QuestionOption {
                label: "Start new plan".to_string(),
                description: "Save any existing canvas, then plan on a fresh canvas".to_string(),
                preselected: false,
            },
            QuestionOption {
                label: "Keep coding".to_string(),
                description: "Stay in the coding agent".to_string(),
                preselected: false,
            },
        ];
        let origin = origin.map(str::to_string);
        let outcome = self
            .interaction
            .request(|id| InteractionRequest::Question {
                request_id: id,
                prompt,
                header: Some("Start new plan".to_string()),
                options,
                multiple: false,
                origin,
            })
            .await;
        match outcome {
            // Index 0 is "Start new plan"; anything else keeps coding.
            Ok(InteractionOutcome::Question(Some(InteractionAnswer::Choices(indices))))
                if indices.first() == Some(&0) =>
            {
                Ok(ToolOutput::WaitStarted {
                    reason: WaitReason::StartNewPlan,
                    message: "Starting a new plan — protecting the current canvas first."
                        .to_string(),
                })
            }
            // Any other answer (the second option, a custom reply, or a
            // cancel) means stay in the coding agent. A non-interactive
            // surface cannot start a new plan, so it also stays. None of these
            // are errors: the coding turn simply continues.
            _ => Ok(ToolOutput::Text(
                "Staying in the coding agent. Continue the work here, or produce a brief plan inline in chat — do not write an ad-hoc plan file.".to_string(),
            )),
        }
    }
}

#[async_trait]
impl Tool for StartNewPlanTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::LocalState
    }

    fn name(&self) -> &str {
        "start_new_plan"
    }

    fn description(&self) -> &str {
        "Ask the user to start a fresh plan on the live plan canvas. \
         Call this when the user asks you to plan, design, or scope a feature rather than \
         implement it now — or proactively, when you judge the task is large or risky enough \
         that a reviewed plan should precede implementation (multi-file feature work, \
         architectural changes, ambiguous scope). The user confirms or declines the switch, so \
         offering it is cheap. On confirmation any existing non-empty canvas is saved to the \
         plan library, then the conversation continues under the planning persona with a fresh \
         canvas; if the user declines, keep working in the coding agent. \
         Never write an ad-hoc plan file (e.g. *.plan.md) — the canvas is the plan surface."
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [(
                "summary",
                string_property(
                    "One line describing what you intend to plan, shown to the user in the confirmation prompt",
                ),
            )],
            &[],
        )
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        self.run(args, None).await
    }

    async fn execute_with_context(
        &self,
        args: serde_json::Value,
        context: ToolExecutionContext,
    ) -> anyhow::Result<ToolOutput> {
        self.run(args, context.origin()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn confirming_starts_a_new_plan_transition() {
        let (service, mut rx) = InteractionService::new();
        let service = Arc::new(service);
        let tool = StartNewPlanTool::new(service.clone());
        let answerer = tokio::spawn(async move {
            let InteractionRequest::Question { request_id, .. } = rx.recv().await.unwrap() else {
                panic!("expected a question request");
            };
            service
                .respond(
                    request_id,
                    InteractionOutcome::Question(Some(InteractionAnswer::Choices(vec![0]))),
                )
                .await
                .unwrap();
        });
        let out = tool
            .execute(serde_json::json!({"summary": "a refresh modal"}))
            .await
            .unwrap();
        answerer.await.unwrap();
        assert!(matches!(
            out,
            ToolOutput::WaitStarted {
                reason: WaitReason::StartNewPlan,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn declining_keeps_coding() {
        let (service, mut rx) = InteractionService::new();
        let service = Arc::new(service);
        let tool = StartNewPlanTool::new(service.clone());
        let answerer = tokio::spawn(async move {
            let InteractionRequest::Question { request_id, .. } = rx.recv().await.unwrap() else {
                panic!("expected a question request");
            };
            service
                .respond(
                    request_id,
                    InteractionOutcome::Question(Some(InteractionAnswer::Choices(vec![1]))),
                )
                .await
                .unwrap();
        });
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        answerer.await.unwrap();
        assert!(matches!(out, ToolOutput::Text(_)));
    }

    #[tokio::test]
    async fn noninteractive_keeps_coding() {
        let tool = StartNewPlanTool::new(Arc::new(InteractionService::noninteractive()));
        let out = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(matches!(out, ToolOutput::Text(_)));
    }
}
