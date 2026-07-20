use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use crate::interaction::{
    InteractionAnswer, InteractionOutcome, InteractionService, InteractionStatus, QuestionOption,
};
use crate::tool::schema::{
    array_property, boolean_property, object, parse_args, string_property, with_property,
};
use crate::tool::{Tool, ToolExecutionContext, ToolOutput};

#[derive(Deserialize)]
struct QuestionArgs {
    question: String,
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    options: Vec<RawOption>,
    #[serde(default)]
    multi_select: bool,
}

#[derive(Deserialize)]
struct RawOption {
    label: String,
    #[serde(default)]
    description: String,
}

pub struct QuestionTool {
    interaction: Arc<InteractionService>,
}

impl QuestionTool {
    pub fn new(interaction: Arc<InteractionService>) -> Self {
        Self { interaction }
    }

    async fn run(
        &self,
        args: serde_json::Value,
        origin: Option<&str>,
    ) -> anyhow::Result<ToolOutput> {
        let args: QuestionArgs = parse_args("question tool", args)?;
        if args.question.trim().is_empty() {
            anyhow::bail!("question text must not be empty");
        }
        if args.options.len() < 2 {
            anyhow::bail!("at least two options are required");
        }
        let options: Vec<QuestionOption> = args
            .options
            .into_iter()
            .map(|opt| QuestionOption {
                label: opt.label,
                description: opt.description,
                preselected: false,
            })
            .collect();
        let option_labels: Vec<String> = options.iter().map(|opt| opt.label.clone()).collect();
        // `args.options` was already moved out by `into_iter` above, so the
        // remaining fields can be moved rather than cloned.
        let prompt = args.question;
        let header = args.header;
        let multi = args.multi_select;
        let origin = origin.map(str::to_string);
        let outcome = self
            .interaction
            .request(|id| crate::interaction::InteractionRequest::Question {
                request_id: id,
                prompt,
                header,
                options,
                multiple: multi,
                origin,
            })
            .await;
        match outcome {
            Ok(InteractionOutcome::Question(Some(InteractionAnswer::Choices(indices)))) => {
                let mut labels = Vec::with_capacity(indices.len());
                for idx in indices {
                    // An out-of-range index means the answer payload and the
                    // option list disagree; surface it instead of silently
                    // dropping the choice.
                    let label = option_labels.get(idx).ok_or_else(|| {
                        anyhow::anyhow!(
                            "Question prompt returned out-of-range choice index {idx} ({} options available)",
                            option_labels.len()
                        )
                    })?;
                    labels.push(label.clone());
                }
                if labels.is_empty() {
                    anyhow::bail!("Question prompt returned no selected options");
                }
                Ok(ToolOutput::Text(format!("Selected: {}", labels.join(", "))))
            }
            #[cfg(test)]
            Ok(InteractionOutcome::Question(Some(InteractionAnswer::Custom(text)))) => {
                Ok(ToolOutput::Text(format!("Custom answer: {text}")))
            }
            Ok(InteractionOutcome::Question(None)) => {
                anyhow::bail!("Question prompt cancelled by user");
            }
            Ok(_) => {
                anyhow::bail!("Question prompt received an unexpected response");
            }
            Err(InteractionStatus::Cancelled) => {
                anyhow::bail!("Question prompt cancelled by user");
            }
            Err(InteractionStatus::Noninteractive) => {
                anyhow::bail!("Question prompts are unavailable in noninteractive mode");
            }
        }
    }
}

#[async_trait]
impl Tool for QuestionTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::LocalState
    }

    fn name(&self) -> &str {
        "question"
    }

    fn description(&self) -> &str {
        "Ask the user a multiple-choice question and wait for their response. Use options to suggest 2-4 short, distinct choices."
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [
                (
                    "question",
                    string_property("The question to present to the user"),
                ),
                (
                    "header",
                    string_property("Short header text shown above the choices (optional)"),
                ),
                (
                    "options",
                    with_property(
                        with_property(
                            array_property(
                                "List of options to present",
                                object(
                                    [
                                        (
                                            "label",
                                            string_property("Choice label shown to the user"),
                                        ),
                                        (
                                            "description",
                                            string_property("Optional supporting description"),
                                        ),
                                    ],
                                    &["label"],
                                ),
                            ),
                            "minItems",
                            serde_json::Value::from(2),
                        ),
                        "maxItems",
                        serde_json::Value::from(4),
                    ),
                ),
                (
                    "multi_select",
                    boolean_property(
                        "If true, the user can select multiple options (default: false)",
                    ),
                ),
            ],
            &["question", "options"],
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
    use crate::interaction::InteractionRequest;

    #[tokio::test]
    async fn question_passes_through_choices() {
        let (service, mut rx) = InteractionService::new();
        let tool = QuestionTool::new(std::sync::Arc::new(service));
        let handle = tokio::spawn({
            let service = tool.interaction.clone();
            async move {
                service
                    .request(|id| InteractionRequest::Question {
                        request_id: id,
                        prompt: "Which?".to_string(),
                        header: None,
                        options: vec![
                            QuestionOption {
                                label: "A".to_string(),
                                description: "".to_string(),
                                preselected: false,
                            },
                            QuestionOption {
                                label: "B".to_string(),
                                description: "".to_string(),
                                preselected: false,
                            },
                        ],
                        multiple: false,
                        origin: None,
                    })
                    .await
            }
        });
        let request = rx.recv().await.expect("request");
        let id = match request {
            InteractionRequest::Question { request_id, .. } => request_id,
            _ => panic!("expected question"),
        };
        tool.interaction
            .respond(
                id,
                InteractionOutcome::Question(Some(InteractionAnswer::Custom("yes".to_string()))),
            )
            .await
            .unwrap();
        let outcome = handle.await.unwrap().expect("ok");
        // Outcome is the InteractionOutcome from the service; verify the
        // payload is what we sent.
        match outcome {
            InteractionOutcome::Question(Some(InteractionAnswer::Custom(text))) => {
                assert_eq!(text, "yes");
            }
            other => panic!("expected custom answer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_returns_selected_option_labels() {
        let (service, mut rx) = InteractionService::new();
        let service = std::sync::Arc::new(service);
        let tool = QuestionTool::new(service.clone());
        let responder = tokio::spawn(async move {
            let request = rx.recv().await.expect("request");
            let id = match request {
                InteractionRequest::Question { request_id, .. } => request_id,
                _ => panic!("expected question"),
            };
            service
                .respond(
                    id,
                    InteractionOutcome::Question(Some(InteractionAnswer::Choices(vec![1]))),
                )
                .await
                .unwrap();
        });

        let result = tool
            .execute(serde_json::json!({
                "question": "Which?",
                "options": [{"label": "A"}, {"label": "B"}]
            }))
            .await
            .unwrap();
        responder.await.unwrap();

        let text = match result {
            ToolOutput::Text(text) => text,
            _ => panic!("expected text"),
        };
        assert_eq!(text, "Selected: B");
    }

    #[tokio::test]
    async fn execute_rejects_single_option() {
        let tool = QuestionTool::new(std::sync::Arc::new(InteractionService::noninteractive()));

        let err = tool
            .execute(serde_json::json!({
                "question": "Which?",
                "options": [{"label": "Only"}]
            }))
            .await
            .expect_err("single-option question should be rejected");

        assert!(
            err.to_string().contains("at least two options"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn noninteractive_question_returns_clear_error() {
        let tool = QuestionTool::new(std::sync::Arc::new(InteractionService::noninteractive()));

        let err = tool
            .execute(serde_json::json!({
                "question": "Which?",
                "options": [{"label": "A"}, {"label": "B"}]
            }))
            .await
            .expect_err("question should fail in noninteractive mode");

        assert!(
            err.to_string().contains("noninteractive mode"),
            "unexpected error: {err}"
        );
    }
}
