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
pub(crate) fn event(event: &SessionEvent) -> Value {
    match event {
        SessionEvent::ResyncRequired => json!({"kind":"reset","resync_required":true}),
        SessionEvent::UserMessage(text) => json!({"kind":"user_message","text":text}),
        SessionEvent::AssistantDelta(text) => json!({"kind":"assistant_delta","text":text}),
        SessionEvent::AssistantDone => json!({"kind":"assistant_done"}),
        SessionEvent::ReasoningDelta(text) => json!({"kind":"reasoning_delta","text":text}),
        SessionEvent::AttemptStarted => json!({"kind":"attempt_started"}),
        SessionEvent::AttemptDiscarded => json!({"kind":"attempt_discarded"}),
        SessionEvent::ToolStarted(call) => {
            json!({"kind":"tool_started","id":call.id,"name":call.name,"arguments":call.arguments})
        }
        SessionEvent::ToolOutput { id, text } => json!({"kind":"tool_output","id":id,"text":text}),
        SessionEvent::ToolResult { id, result } => {
            json!({"kind":"tool_result","id":id,"result":result})
        }
        SessionEvent::Usage {
            prompt_tokens,
            completion_tokens,
            cost_micros,
        } => {
            json!({"kind":"usage","prompt_tokens":prompt_tokens,"completion_tokens":completion_tokens,"cost_micros":cost_micros})
        }
        SessionEvent::Plan(plan) => json!({"kind":"plan","plan":plan}),
        SessionEvent::Todos(todos) => json!({"kind":"todos","todos":todos}),
        SessionEvent::Completion(report) => json!({"kind":"completion","report":report}),
        SessionEvent::Status(text) => json!({"kind":"status","text":text}),
        // Internal errors may contain paths, provider diagnostics, or credentials.
        SessionEvent::Error(_) => {
            json!({"kind":"error","code":"execution_failed","message":"Execution failed; inspect local diagnostics"})
        }
        SessionEvent::Interaction(r) => json!({"kind":"interaction","request":interaction(r)}),
        SessionEvent::InteractionResolved(id) => {
            json!({"kind":"interaction_resolved","request_id":id})
        }
        SessionEvent::Lifecycle { run_id, state } => {
            json!({"kind":"lifecycle","run_id":run_id,"state":state})
        }
    }
}
pub(crate) fn snapshot(
    s: &super::controller::SessionSnapshot,
    offset: usize,
    limit: usize,
) -> Value {
    json!({"session_id":s.control.session_id.as_i64(),"run_id":s.control.run_id,"state":s.control.state,
        "failure":s.control.failure.as_ref().map(|_|"execution_failed"),"pending":s.control.pending.iter().map(interaction).collect::<Vec<_>>(),
        "watermark":s.watermark,"transcript":s.transcript.iter().skip(offset).take(limit).collect::<Vec<_>>(),"total":s.transcript.len(),"offset":offset,
        "plan":s.plan,"todos":s.todos,"budget":s.budget,"usage":{"prompt_tokens":s.usage.0,"completion_tokens":s.usage.1,"cost_micros":s.usage.2}})
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
}
