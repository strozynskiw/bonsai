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
    #[serde(default)]
    recommended_index: usize,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    wait_for_answer: Option<bool>,
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
        anyhow::ensure!(args.options.len() <= 4, "at most four options are allowed");
        anyhow::ensure!(
            args.recommended_index < args.options.len(),
            "recommended_index is out of range"
        );
        if let Some(seconds) = args.timeout_seconds {
            anyhow::ensure!(
                (1..=3600).contains(&seconds),
                "timeout_seconds must be between 1 and 3600"
            );
        }
        let timeout = args
            .timeout_seconds
            .or_else(|| self.interaction.goal_question_timeout());
        let recommended = args.recommended_index;
        let wait_for_answer = args
            .wait_for_answer
            .unwrap_or(origin.is_some() || self.interaction.goal_question_timeout().is_none());
        anyhow::ensure!(
            wait_for_answer || origin.is_none(),
            "Delegated questions must wait for their own answer"
        );
        let mut options: Vec<QuestionOption> = args
            .options
            .into_iter()
            .map(|opt| QuestionOption {
                label: opt.label,
                description: opt.description,
                preselected: false,
            })
            .collect();
        let option_labels: Vec<String> = options.iter().map(|opt| opt.label.clone()).collect();
        if timeout.is_some() {
            options[recommended].label.push_str(" (Recommended)");
        }
        // `args.options` was already moved out by `into_iter` above, so the
        // remaining fields can be moved rather than cloned.
        let prompt = args.question;
        let header = match timeout {
            Some(seconds) => Some(format!(
                "{} · recommended in {seconds}s",
                args.header.as_deref().unwrap_or("Question")
            )),
            None => args.header,
        };
        let multi = args.multi_select;
        let origin = origin.map(str::to_string);
        let question_text = prompt.clone();
        let build = move |id| crate::interaction::InteractionRequest::Question {
            request_id: id,
            prompt,
            header,
            options,
            multiple: multi,
            origin,
        };
        if !wait_for_answer {
            let id = self
                .interaction
                .spawn_question(build, timeout, recommended, move |outcome| {
                    let answer = match render_answer(outcome, &option_labels) {
                        Ok(ToolOutput::Text(text)) => text,
                        Ok(_) => String::new(),
                        Err(error) => error.to_string(),
                    };
                    format!("{question_text}\n{answer}")
                })
                .await
                .map_err(|status| anyhow::anyhow!("Question unavailable: {status:?}"))?;
            return Ok(ToolOutput::Text(format!(
                "Question #{id} pending. Continue independent work; the response or timed recommendation will be delivered automatically. Use /questions to reopen it."
            )));
        }
        let outcome = self
            .interaction
            .request_question(build, timeout, recommended)
            .await;
        render_answer(outcome, &option_labels)
    }
}

fn render_answer(
    outcome: Result<InteractionOutcome, InteractionStatus>,
    option_labels: &[String],
) -> anyhow::Result<ToolOutput> {
    match outcome {
        Ok(InteractionOutcome::Question(Some(InteractionAnswer::Recommended(index)))) => {
            let label = option_labels
                .get(index)
                .ok_or_else(|| anyhow::anyhow!("Recommended option is out of range"))?;
            Ok(ToolOutput::Text(format!(
                "Question timed out. Automatically assumed recommended option: {label}. This was not a user response or permission approval; the user may correct it while work continues."
            )))
        }
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

#[async_trait]
impl Tool for QuestionTool {
    fn effect_policy(&self) -> crate::tool::ToolEffectPolicy {
        crate::tool::ToolEffectPolicy::LocalState
    }

    fn name(&self) -> &str {
        "question"
    }

    fn description(&self) -> &str {
        "Ask the user a multiple-choice question. Suggest 2-4 choices with the recommended option first. In goal mode, unanswered questions automatically assume the recommendation after 60 seconds. Never use a question timeout as permission for an external or destructive action."
    }

    fn parallel_policy(&self) -> crate::tool::ParallelPolicy {
        crate::tool::ParallelPolicy::Serialized
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object(
            [
                (
                    "wait_for_answer",
                    boolean_property(
                        "Wait for the answer before doing other work. Defaults to false in goal mode; true otherwise.",
                    ),
                ),
                (
                    "recommended_index",
                    serde_json::json!({"type":"integer", "minimum":0, "description":"Zero-based recommended option; defaults to 0. Used on timeout."}),
                ),
                (
                    "timeout_seconds",
                    serde_json::json!({"type":"integer", "minimum":1, "maximum":3600, "description":"Optional question timeout; goal mode defaults to 60 seconds. Timeout assumes the recommended option."}),
                ),
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

    #[tokio::test(start_paused = true)]
    async fn goal_question_returns_pending_then_delivers_recommended_timeout_once() {
        let (service, mut rx) = InteractionService::new();
        service.set_goal_question_timeout(Some(60));
        let service = Arc::new(service);
        let tool = QuestionTool::new(service.clone());
        let output = tool
            .execute(serde_json::json!({
                "question": "Which storage?",
                "options": [{"label":"A"}, {"label":"B"}],
                "recommended_index": 1,
            }))
            .await
            .unwrap();
        assert!(matches!(output, ToolOutput::Text(text) if text.contains("pending")));
        let InteractionRequest::Question {
            request_id,
            options,
            ..
        } = rx.recv().await.unwrap()
        else {
            panic!("question expected")
        };
        assert!(options[1].label.contains("Recommended"));
        assert!(service.is_pending(request_id).await);
        tokio::time::advance(std::time::Duration::from_secs(59)).await;
        assert!(service.is_pending(request_id).await);
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        let results = service
            .wait_question_results(&tokio_util::sync::CancellationToken::new())
            .await;
        assert_eq!(results.len(), 1);
        assert!(results[0].contains("recommended option: B"));
        assert!(!service.is_pending(request_id).await);
        assert!(service.take_question_results().await.is_empty());
        assert!(
            service
                .respond(
                    request_id,
                    InteractionOutcome::Question(Some(InteractionAnswer::Choices(vec![0])))
                )
                .await
                .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn background_question_delivers_user_answer_before_deadline() {
        let (service, mut rx) = InteractionService::new();
        service.set_goal_question_timeout(Some(60));
        let service = Arc::new(service);
        let tool = QuestionTool::new(service.clone());
        tool.execute(serde_json::json!({
            "question":"Pick", "options":[{"label":"A"},{"label":"B"}]
        }))
        .await
        .unwrap();
        let InteractionRequest::Question { request_id, .. } = rx.recv().await.unwrap() else {
            panic!("question expected")
        };
        assert_eq!(service.pending_questions().await.len(), 1);
        service
            .respond(
                request_id,
                InteractionOutcome::Question(Some(InteractionAnswer::Choices(vec![1]))),
            )
            .await
            .unwrap();
        let results = service
            .wait_question_results(&tokio_util::sync::CancellationToken::new())
            .await;
        assert!(results[0].contains("Selected: B"));
        assert!(!results[0].contains("timed out"));
        assert!(service.pending_questions().await.is_empty());
    }

    #[tokio::test]
    async fn question_rejects_invalid_recommendation_and_timeout() {
        let tool = QuestionTool::new(Arc::new(InteractionService::noninteractive()));
        for extra in [
            serde_json::json!({"recommended_index":2}),
            serde_json::json!({"timeout_seconds":0}),
            serde_json::json!({"timeout_seconds":3601}),
        ] {
            let mut args =
                serde_json::json!({"question":"Pick", "options":[{"label":"A"},{"label":"B"}]});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(tool.execute(args).await.is_err());
        }
    }

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
