//! `InteractionRequest` ⇄ ACP `session/request_permission`.
//!
//! The client only ever carries the user's *decision* for one pending prompt.
//! Every mapping here is at most as broad as the TUI's narrowest equivalent:
//! "always" is session-scoped (never a persisted project rule), sandbox
//! escalation stays per exact command, and anything unrecognized — an unknown
//! option, a cancelled request, a transport failure — is a denial.

use agent_client_protocol::v1::schema::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, ToolCallStatus,
    ToolCallUpdateFields, ToolKind,
};

use crate::interaction::{
    EscalationDecision, InteractionAnswer, InteractionOutcome, InteractionRequest,
    PermissionDecision,
};

pub(crate) const ALLOW_ONCE: &str = "allow_once";
pub(crate) const ALLOW_SESSION: &str = "allow_session";
pub(crate) const REJECT: &str = "reject";
const CHOICE_PREFIX: &str = "choice_";

/// The options offered for a request, or `None` when ACP v1 cannot present
/// it (multi-select questions) and bonsai must decline it itself.
pub(crate) fn options(request: &InteractionRequest) -> Option<Vec<PermissionOption>> {
    let standard = |once: &str, always: &str| {
        vec![
            PermissionOption::new(ALLOW_ONCE, once, PermissionOptionKind::AllowOnce),
            PermissionOption::new(ALLOW_SESSION, always, PermissionOptionKind::AllowAlways),
            PermissionOption::new(REJECT, "Reject", PermissionOptionKind::RejectOnce),
        ]
    };
    match request {
        InteractionRequest::Permission { .. }
        | InteractionRequest::WebDomain { .. }
        | InteractionRequest::ExtensionTool { .. }
        | InteractionRequest::HookTrust { .. } => {
            Some(standard("Allow once", "Always allow in this session"))
        }
        InteractionRequest::SandboxEscalation { .. } => Some(standard(
            "Run outside the sandbox once",
            "Allow this exact command outside the sandbox for this session",
        )),
        InteractionRequest::Question {
            options, multiple, ..
        } => (!multiple && !options.is_empty()).then(|| {
            options
                .iter()
                .enumerate()
                .map(|(index, option)| {
                    PermissionOption::new(
                        format!("{CHOICE_PREFIX}{index}"),
                        option.label.clone(),
                        PermissionOptionKind::AllowOnce,
                    )
                })
                .chain(std::iter::once(PermissionOption::new(
                    REJECT,
                    "Skip",
                    PermissionOptionKind::RejectOnce,
                )))
                .collect()
        }),
    }
}

/// Title and kind for the tool-call card the prompt is attached to.
pub(crate) fn describe(request: &InteractionRequest) -> ToolCallUpdateFields {
    let (title, kind) = match request {
        InteractionRequest::Permission { command, .. } => (command.clone(), ToolKind::Execute),
        InteractionRequest::SandboxEscalation { command, .. } => (
            format!("Run outside the sandbox: {command}"),
            ToolKind::Execute,
        ),
        InteractionRequest::WebDomain {
            url,
            host,
            redirected_from,
            ..
        } => (
            match redirected_from {
                Some(from) => format!("Fetch {url} (redirected from {from} to {host})"),
                None => format!("Fetch {url}"),
            },
            ToolKind::Fetch,
        ),
        InteractionRequest::Question { prompt, .. } => (prompt.clone(), ToolKind::Other),
        InteractionRequest::ExtensionTool {
            id, args_preview, ..
        } => (format!("{id} {args_preview}"), ToolKind::Other),
        InteractionRequest::HookTrust {
            name,
            event,
            action_preview,
            ..
        } => (
            format!("Trust hook {name} ({event}): {action_preview}"),
            ToolKind::Execute,
        ),
    };
    ToolCallUpdateFields::new()
        .title(title)
        .kind(kind)
        .status(ToolCallStatus::Pending)
}

/// Translate the client's answer. Fails closed.
pub(crate) fn outcome(
    request: &InteractionRequest,
    answer: Option<&RequestPermissionOutcome>,
) -> InteractionOutcome {
    let selected = match answer {
        Some(RequestPermissionOutcome::Selected(selected)) => selected.option_id.0.as_ref(),
        _ => return deny(request),
    };
    match request {
        InteractionRequest::Permission { .. }
        | InteractionRequest::WebDomain { .. }
        | InteractionRequest::ExtensionTool { .. }
        | InteractionRequest::HookTrust { .. } => match selected {
            ALLOW_ONCE => InteractionOutcome::Permission(PermissionDecision::AllowOnce),
            ALLOW_SESSION => InteractionOutcome::Permission(PermissionDecision::AllowMatching),
            _ => deny(request),
        },
        InteractionRequest::SandboxEscalation { .. } => match selected {
            ALLOW_ONCE => InteractionOutcome::SandboxEscalation(EscalationDecision::AllowOnce),
            ALLOW_SESSION => {
                InteractionOutcome::SandboxEscalation(EscalationDecision::AllowForSession)
            }
            _ => deny(request),
        },
        InteractionRequest::Question {
            options, multiple, ..
        } => selected
            .strip_prefix(CHOICE_PREFIX)
            .and_then(|index| index.parse::<usize>().ok())
            .filter(|index| !multiple && *index < options.len())
            .map_or_else(
                || deny(request),
                |index| InteractionOutcome::Question(Some(InteractionAnswer::Choices(vec![index]))),
            ),
    }
}

pub(crate) fn deny(request: &InteractionRequest) -> InteractionOutcome {
    match request {
        InteractionRequest::SandboxEscalation { .. } => {
            InteractionOutcome::SandboxEscalation(EscalationDecision::Deny)
        }
        InteractionRequest::Question { .. } => InteractionOutcome::Question(None),
        _ => InteractionOutcome::Permission(PermissionDecision::Deny),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::SelectedPermissionOutcome;

    fn all_requests() -> Vec<InteractionRequest> {
        vec![
            InteractionRequest::Permission {
                request_id: 1,
                command: "rm -rf build".into(),
                origin: None,
            },
            InteractionRequest::SandboxEscalation {
                request_id: 2,
                command: "curl example.com".into(),
                origin: None,
                kind: crate::interaction::SandboxEscalationKind::CommandAndSandbox,
            },
            InteractionRequest::WebDomain {
                request_id: 3,
                url: "https://example.com".into(),
                host: "example.com".into(),
                redirected_from: None,
                origin: None,
            },
            InteractionRequest::Question {
                request_id: 4,
                prompt: "Which?".into(),
                header: None,
                options: vec![
                    crate::interaction::QuestionOption::new("A", ""),
                    crate::interaction::QuestionOption::new("B", ""),
                ],
                multiple: false,
                origin: None,
            },
            InteractionRequest::ExtensionTool {
                request_id: 5,
                id: "mcp.x.y".into(),
                server: "x".into(),
                capabilities: Vec::new(),
                args_preview: "{}".into(),
            },
            InteractionRequest::HookTrust {
                request_id: 6,
                name: "fmt".into(),
                event: "PreToolUse".into(),
                action_kind: "shell".into(),
                action_preview: "cargo fmt".into(),
            },
        ]
    }

    fn select(id: &str) -> RequestPermissionOutcome {
        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            agent_client_protocol::v1::schema::PermissionOptionId::new(id),
        ))
    }

    fn is_denial(outcome: &InteractionOutcome) -> bool {
        matches!(
            outcome,
            InteractionOutcome::Permission(PermissionDecision::Deny)
                | InteractionOutcome::SandboxEscalation(EscalationDecision::Deny)
                | InteractionOutcome::Question(None)
        )
    }

    /// No answer an ACP client can send may persist a project rule, broaden a
    /// sandbox escape beyond one exact command, or turn into anything but a
    /// denial when it is cancelled, unknown, or forged.
    #[test]
    fn no_client_answer_escalates_beyond_the_narrowest_grant() {
        let forged = [
            "allow_for_project",
            "allow_always",
            "approve_all",
            "yolo",
            "choice_9",
            "choice_-1",
            "",
        ];
        for request in all_requests() {
            assert!(is_denial(&outcome(&request, None)));
            assert!(is_denial(&outcome(
                &request,
                Some(&RequestPermissionOutcome::Cancelled)
            )));
            assert!(is_denial(&outcome(&request, Some(&select(REJECT)))));
            for id in forged {
                assert!(
                    is_denial(&outcome(&request, Some(&select(id)))),
                    "forged option {id:?} must deny {request:?}"
                );
            }
            for option in options(&request).unwrap() {
                let granted = outcome(&request, Some(&select(&option.option_id.0)));
                assert!(
                    !matches!(
                        granted,
                        InteractionOutcome::Permission(
                            PermissionDecision::AllowForProject
                                | PermissionDecision::DenyForProject
                        )
                    ),
                    "ACP must never write persistent project rules"
                );
            }
        }
    }

    #[test]
    fn standard_options_map_to_once_session_and_reject() {
        let request = &all_requests()[0];
        assert!(matches!(
            outcome(request, Some(&select(ALLOW_ONCE))),
            InteractionOutcome::Permission(PermissionDecision::AllowOnce)
        ));
        assert!(matches!(
            outcome(request, Some(&select(ALLOW_SESSION))),
            InteractionOutcome::Permission(PermissionDecision::AllowMatching)
        ));
        let sandbox = &all_requests()[1];
        assert!(matches!(
            outcome(sandbox, Some(&select(ALLOW_SESSION))),
            InteractionOutcome::SandboxEscalation(EscalationDecision::AllowForSession)
        ));
    }

    #[test]
    fn single_select_questions_map_choices_and_multi_select_is_declined() {
        let question = &all_requests()[3];
        assert!(matches!(
            outcome(question, Some(&select("choice_1"))),
            InteractionOutcome::Question(Some(InteractionAnswer::Choices(ref c))) if c == &[1]
        ));
        let multi = InteractionRequest::Question {
            request_id: 9,
            prompt: "Pick".into(),
            header: None,
            options: vec![crate::interaction::QuestionOption::new("A", "")],
            multiple: true,
            origin: None,
        };
        assert!(options(&multi).is_none());
        assert!(is_denial(&outcome(&multi, Some(&select("choice_0")))));
    }
}
