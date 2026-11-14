//! Experimental transport projections, independent of provider/TUI wire formats.
use super::sink::SessionEvent;
use crate::interaction::{
    EscalationDecision as E, InteractionAnswer, InteractionOutcome as O, InteractionRequest as R,
    PermissionDecision as P,
};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) fn interaction(request: &R) -> Value {
    let permissions = [
        "allow_once",
        "allow_matching",
        "allow_for_project",
        "deny_for_project",
        "deny",
    ];
    match request {
        R::Permission {
            request_id,
            command,
            origin,
        } => {
            json!({"kind":"permission","request_id":request_id,"command":command,"origin":origin,"decisions":permissions})
        }
        R::SandboxEscalation {
            request_id,
            command,
            origin,
            kind,
        } => {
            json!({"kind":"sandbox","request_id":request_id,"command":command,"origin":origin,"command_and_sandbox":matches!(kind,crate::interaction::SandboxEscalationKind::CommandAndSandbox),"decisions":["allow_once","allow_for_session","deny"]})
        }
        R::WebDomain {
            request_id,
            url,
            host,
            redirected_from,
            origin,
        } => {
            json!({"kind":"web_domain","request_id":request_id,"url":url,"host":host,"redirected_from":redirected_from,"origin":origin,"decisions":permissions})
        }
        R::Question {
            request_id,
            prompt,
            header,
            options,
            multiple,
            origin,
        } => {
            json!({"kind":"question","request_id":request_id,"prompt":prompt,"header":header,"multiple":multiple,"origin":origin,"options":options.iter().map(|o|json!({"label":o.label,"description":o.description,"preselected":o.preselected})).collect::<Vec<_>>() })
        }
        R::ExtensionTool {
            request_id,
            id,
            server,
            capabilities,
            args_preview,
        } => {
            json!({"kind":"extension","request_id":request_id,"id":id,"server":server,"capabilities":capabilities,"args_preview":args_preview,"decisions":permissions})
        }
        R::HookTrust {
            request_id,
            name,
            event,
            action_kind,
            action_preview,
        } => {
            json!({"kind":"hook_trust","request_id":request_id,"name":name,"event":event,"action_kind":action_kind,"action_preview":action_preview,"decisions":permissions})
        }
    }
}
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Answer {
    Permission { decision: String },
    Sandbox { decision: String },
    Question { choices: Option<Vec<usize>> },
}
impl Answer {
    pub(crate) fn outcome(self) -> anyhow::Result<O> {
        Ok(match self {
            Self::Permission { decision } => O::Permission(match decision.as_str() {
                "allow_once" => P::AllowOnce,
                "allow_matching" => P::AllowMatching,
                "allow_for_project" => P::AllowForProject,
                "deny_for_project" => P::DenyForProject,
                "deny" => P::Deny,
                _ => anyhow::bail!("Invalid decision"),
            }),
            Self::Sandbox { decision } => O::SandboxEscalation(match decision.as_str() {
                "allow_once" => E::AllowOnce,
                "allow_for_session" => E::AllowForSession,
                "deny" => E::Deny,
                _ => anyhow::bail!("Invalid decision"),
            }),
            Self::Question { choices } => O::Question(choices.map(InteractionAnswer::Choices)),
        })
    }
}
/// Wire name for one event kind.
///
/// Deliberately exhaustive over `SessionEvent`: adding a variant does not
/// compile until it is named here, and `docs/server.md` plus
/// `docs/server-contract.json` then have to publish the same set (enforced by
/// `published_event_kinds_cover_every_variant_and_are_documented`).
pub(crate) fn event_kind(event: &SessionEvent) -> &'static str {
    match event {
        SessionEvent::ResyncRequired => "reset",
        SessionEvent::UserMessage(_) => "user_message",
        SessionEvent::MessageDelivery { .. } => "message_delivery",
        SessionEvent::AssistantDelta(_) => "assistant_delta",
        SessionEvent::AssistantDone => "assistant_done",
        SessionEvent::ReasoningDelta(_) => "reasoning_delta",
        SessionEvent::AttemptStarted => "attempt_started",
        SessionEvent::AttemptDiscarded => "attempt_discarded",
        SessionEvent::ToolStarted(_) => "tool_started",
        SessionEvent::ToolOutput { .. } => "tool_output",
        SessionEvent::ToolResult { .. } => "tool_result",
        SessionEvent::Usage { .. } => "usage",
        SessionEvent::Plan(_) => "plan",
        SessionEvent::Todos(_) => "todos",
        SessionEvent::Goal(_) => "goal",
        SessionEvent::Completion(_) => "completion",
        SessionEvent::Status(_) => "status",
        SessionEvent::Error(_) => "error",
        SessionEvent::Interaction(_) => "interaction",
        SessionEvent::InteractionResolved(_) => "interaction_resolved",
        SessionEvent::Lifecycle { .. } => "lifecycle",
    }
}

pub(crate) fn event(event: &SessionEvent) -> Value {
    let mut payload = match event {
        SessionEvent::ResyncRequired => json!({"resync_required":true}),
        SessionEvent::UserMessage(text) => json!({"text":text}),
        SessionEvent::MessageDelivery {
            run_id,
            request_id,
            state,
        } => {
            json!({"run_id":run_id,"request_id":request_id,"state":state})
        }
        SessionEvent::AssistantDelta(text) => json!({"text":text}),
        SessionEvent::AssistantDone => json!({}),
        SessionEvent::ReasoningDelta(text) => json!({"text":text}),
        SessionEvent::AttemptStarted => json!({}),
        SessionEvent::AttemptDiscarded => json!({}),
        SessionEvent::ToolStarted(call) => {
            json!({"id":call.id,"name":call.name,"arguments":call.arguments})
        }
        SessionEvent::ToolOutput { id, text } => json!({"id":id,"text":text}),
        SessionEvent::ToolResult { id, result } => json!({"id":id,"result":result}),
        SessionEvent::Usage {
            prompt_tokens,
            completion_tokens,
            cost_micros,
        } => {
            json!({"prompt_tokens":prompt_tokens,"completion_tokens":completion_tokens,"cost_micros":cost_micros})
        }
        SessionEvent::Plan(plan) => json!({"plan":plan}),
        SessionEvent::Todos(todos) => json!({"todos":todos}),
        SessionEvent::Goal(goal) => json!({"goal":goal}),
        SessionEvent::Completion(report) => json!({"report":report}),
        SessionEvent::Status(text) => json!({"text":text}),
        // Internal errors may contain paths, provider diagnostics, or credentials.
        SessionEvent::Error(_) => {
            json!({"code":"execution_failed","message":"Execution failed; inspect local diagnostics"})
        }
        SessionEvent::Interaction(r) => json!({"request":interaction(r)}),
        SessionEvent::InteractionResolved(id) => json!({"request_id":id}),
        SessionEvent::Lifecycle { run_id, state } => {
            json!({"run_id":run_id,"state":state})
        }
    };
    if let Some(object) = payload.as_object_mut() {
        object.insert("kind".into(), Value::String(event_kind(event).into()));
    }
    payload
}
pub(crate) fn snapshot(
    s: &super::controller::SessionSnapshot,
    offset: usize,
    limit: usize,
) -> Value {
    json!({"session_id":s.control.session_id.as_i64(),"run_id":s.control.run_id,"state":s.control.state,
        "failure":s.control.failure.as_ref().map(|_|"execution_failed"),"pending":s.control.pending.iter().map(interaction).collect::<Vec<_>>(),
        "watermark":s.watermark,"transcript":s.transcript.iter().skip(offset).take(limit).collect::<Vec<_>>(),"total":s.transcript.len(),"offset":offset,
        "plan":s.plan,"todos":s.todos,"goal":s.goal,"goal_elapsed_ms":s.goal_elapsed_ms,"budget":s.budget,"usage":{"prompt_tokens":s.usage.0,"completion_tokens":s.usage.1,"cost_micros":s.usage.2}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_contract_fixtures_match_real_projections_and_decisions() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../docs/server-contract.json")).unwrap();
        assert_eq!(fixture["version"], "experimental-v0");
        for answer in fixture["answers"].as_array().unwrap() {
            let decoded: Answer = serde_json::from_value(answer.clone()).unwrap();
            assert_eq!(serde_json::to_value(&decoded).unwrap(), *answer);
            decoded.outcome().unwrap();
        }
        let events = [
            SessionEvent::UserMessage("Hello".into()),
            SessionEvent::AssistantDelta("Hi".into()),
            SessionEvent::AssistantDone,
            SessionEvent::Lifecycle {
                run_id: Some("run-1".into()),
                state: "running".into(),
            },
            SessionEvent::Error("private diagnostic must not cross wire".into()),
            SessionEvent::ResyncRequired,
            SessionEvent::ToolResult {
                id: "call-1".into(),
                result: edit_result(),
            },
        ];
        assert_eq!(
            json!(events.iter().map(event).collect::<Vec<_>>()),
            fixture["events"]
        );
        assert_eq!(
            interaction(&R::Permission {
                request_id: 7,
                command: "echo fixture".into(),
                origin: None
            }),
            fixture["permission_prompt"]
        );
        assert!(
            serde_json::from_value::<Answer>(
                json!({"kind":"permission","decision":"deny","approve_all":true})
            )
            .is_err()
        );
    }

    /// One representative event per `SessionEvent` variant. This list, plus the
    /// exhaustive `event_kind` match, is what makes the published-kind check
    /// cover the whole enum rather than the convenient half of it.
    fn representative_events() -> Vec<SessionEvent> {
        vec![
            SessionEvent::ResyncRequired,
            SessionEvent::UserMessage("Hello".into()),
            SessionEvent::MessageDelivery {
                run_id: "run-1".into(),
                request_id: "request-2".into(),
                state: "delivered".into(),
            },
            SessionEvent::AssistantDelta("Hi".into()),
            SessionEvent::AssistantDone,
            SessionEvent::ReasoningDelta("thinking".into()),
            SessionEvent::AttemptStarted,
            SessionEvent::AttemptDiscarded,
            SessionEvent::ToolStarted(crate::output::ToolCallStart {
                id: "call-1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            }),
            SessionEvent::ToolOutput {
                id: "call-1".into(),
                text: "partial".into(),
            },
            SessionEvent::ToolResult {
                id: "call-1".into(),
                result: edit_result(),
            },
            SessionEvent::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                cost_micros: None,
            },
            SessionEvent::Plan(Box::default()),
            SessionEvent::Todos(vec![crate::todo::TodoItem {
                content: "Ship it".into(),
                status: crate::todo::TodoStatus::Pending,
            }]),
            SessionEvent::Goal(Some(crate::goal::GoalRecord {
                id: "goal-1".into(),
                objective: "Ship it".into(),
                state: crate::goal::GoalState::Active,
                reason: None,
                question_timeout_seconds: 0,
                created_at_ms: 1,
                updated_at_ms: 2,
            })),
            SessionEvent::Completion(Box::new(completion_report())),
            SessionEvent::Status("working".into()),
            SessionEvent::Error("private diagnostic must not cross wire".into()),
            SessionEvent::Interaction(R::Permission {
                request_id: 7,
                command: "echo fixture".into(),
                origin: None,
            }),
            SessionEvent::InteractionResolved(7),
            SessionEvent::Lifecycle {
                run_id: Some("run-1".into()),
                state: "running".into(),
            },
        ]
    }

    /// A successful mutation result, so the published diff shape stays pinned.
    fn edit_result() -> crate::protocol::ToolResult {
        let diff = crate::diff::FileDiff {
            path: "src/main.rs".into(),
            status: crate::diff::DiffStatus::Modified,
            hunks: vec![crate::diff::DiffHunk {
                old_start: 1,
                new_start: 1,
                lines: vec![
                    crate::diff::DiffLine {
                        kind: crate::diff::DiffLineKind::Removed,
                        content: "fn old() {}".into(),
                        old_line: Some(1),
                        new_line: None,
                    },
                    crate::diff::DiffLine {
                        kind: crate::diff::DiffLineKind::Added,
                        content: "fn new() {}".into(),
                        old_line: None,
                        new_line: Some(1),
                    },
                ],
            }],
            truncated: false,
            old_size: Some(12),
            new_size: 12,
            added_lines: 1,
            removed_lines: 1,
            additional_files: Box::default(),
        };
        let mut result = crate::protocol::ToolResult::legacy(
            "Edited src/main.rs".into(),
            crate::protocol::ToolExecutionStatus::Succeeded,
        );
        result.content = crate::protocol::ToolResultContent::Edit {
            summary: "Edited src/main.rs".into(),
            diff: Box::new(diff),
        };
        result
    }

    fn completion_report() -> crate::completion_report::CompletionReport {
        crate::completion_report::CompletionReport {
            status: crate::completion_report::CompletionStatus::Completed,
            completion_guard: None,
            changed_files: Vec::new(),
            verification: None,
            verification_evidence: None,
            review: None,
            authorization: Default::default(),
            usage: crate::completion_report::CompletionUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cost_micros: None,
                session_budget: Default::default(),
                budget_exhaustion: None,
            },
            caveats: Vec::new(),
        }
    }

    /// Every emitted kind is published in the fixture and documented in the
    /// reader-facing event table, so neither can drift behind the server.
    #[test]
    fn published_event_kinds_cover_every_variant_and_are_documented() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../docs/server-contract.json")).unwrap();
        let published = fixture["event_kinds"]
            .as_array()
            .expect("the fixture publishes an event_kinds list")
            .iter()
            .map(|kind| {
                kind.as_str()
                    .expect("a published event kind is a string")
                    .to_string()
            })
            .collect::<std::collections::BTreeSet<_>>();
        let samples = representative_events();
        for sample in &samples {
            assert_eq!(
                event(sample)["kind"],
                json!(event_kind(sample)),
                "the projection and the published kind must agree"
            );
        }
        let emitted = samples
            .iter()
            .map(|sample| event_kind(sample).to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            emitted, published,
            "publish every SessionEvent kind in docs/server-contract.json"
        );
        assert_eq!(
            documented_event_kinds(),
            published,
            "the docs/server.md event table must list every published kind"
        );
    }

    /// Kinds listed in the `## Event stream` table of the published docs.
    fn documented_event_kinds() -> std::collections::BTreeSet<String> {
        let docs = include_str!("../../docs/server.md");
        let section = docs
            .split("## Event stream")
            .nth(1)
            .expect("docs/server.md has an event stream section");
        let table = section.split("\n## ").next().unwrap_or_default();
        table
            .lines()
            .filter_map(|line| {
                let cell = line.strip_prefix('|')?.split('|').next()?.trim();
                let kind = cell.strip_prefix('`')?.strip_suffix('`')?;
                (kind != "kind").then(|| kind.to_string())
            })
            .collect()
    }
}
