use std::collections::BTreeMap;
use std::sync::Arc;

use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    /// Whether the option's checkbox starts checked in a `multiple` question.
    /// Ignored for single-select questions (the cursor is the selection).
    pub preselected: bool,
}

impl QuestionOption {
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
            preselected: false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum InteractionRequest {
    Permission {
        request_id: u64,
        command: String,
        origin: Option<String>,
    },
    /// Approval to run a single command OUTSIDE the OS sandbox. This is the
    /// enforcement floor — never auto-approved, regardless of autonomy level.
    SandboxEscalation {
        request_id: u64,
        command: String,
        origin: Option<String>,
        kind: SandboxEscalationKind,
    },
    /// Approval to fetch from a web domain (WebSearch/WebFetch). Reuses
    /// [`PermissionDecision`] for the outcome (once / session / project / deny).
    WebDomain {
        request_id: u64,
        /// The full URL being fetched, shown for context.
        url: String,
        /// The host the rule would apply to (e.g. `example.com`).
        host: String,
        /// The previously approved host when this prompt is for a cross-domain
        /// redirect hop — surfaced so the user sees they are approving a *new*
        /// domain reached via redirect, not the one they originally allowed.
        redirected_from: Option<String>,
        origin: Option<String>,
    },
    Question {
        request_id: u64,
        prompt: String,
        header: Option<String>,
        options: Vec<QuestionOption>,
        multiple: bool,
        origin: Option<String>,
    },
    /// Approval to run one namespaced extension tool call (MCP tools and
    /// hooks). Reuses [`PermissionDecision`] for the outcome, like
    /// [`Self::WebDomain`].
    ExtensionTool {
        request_id: u64,
        /// The dotted display id, e.g. `"mcp.github.create_issue"`.
        id: String,
        /// The owning extension's name, e.g. `"github"`, shown for context.
        server: String,
        /// Declared capability labels (`"read"`, `"write"`, …), or empty when
        /// undeclared.
        capabilities: Vec<String>,
        /// A short, pre-redacted first-line preview of the call's arguments.
        args_preview: String,
    },
    /// One-time trust approval for a project-config hook's `shell`/`http`
    /// action. Load-time, not per-fire — asked once when the hook
    /// engine is built, not on every event it matches. Reuses
    /// [`PermissionDecision`] for the outcome, like [`Self::ExtensionTool`].
    HookTrust {
        request_id: u64,
        name: String,
        /// The lifecycle event this hook fires on, e.g. `"PreToolUse"`.
        event: String,
        /// `"shell"` or `"http"`.
        action_kind: String,
        /// The command or URL, shown for context.
        action_preview: String,
    },
}

/// Whether a sandbox-escalation prompt approves only the confinement bypass or
/// also replaces an ordinary command-permission prompt that would otherwise
/// appear immediately before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxEscalationKind {
    SandboxOnly,
    CommandAndSandbox,
}

#[derive(Debug, Clone)]
pub enum PermissionDecision {
    /// Run this once; add no rule.
    AllowOnce,
    /// "Always for session" — add an in-memory rule for matching commands.
    AllowMatching,
    /// "Always for project" — persist a rule for matching commands so it
    /// survives restarts.
    AllowForProject,
    /// "Never for project" — persist a *deny* rule for matching commands so the
    /// action is refused automatically on future runs, without re-prompting.
    /// The symmetric counterpart to [`Self::AllowForProject`]; attacks the
    /// decision-fatigue trap of an approval prompt that only remembers "yes".
    DenyForProject,
    Deny,
}

/// User's response to a sandbox-escape prompt. Unlike [`PermissionDecision`]
/// there is no project/global option — escapes are never persisted, so the
/// enforcement floor is never permanently lowered.
#[derive(Debug, Clone)]
pub enum EscalationDecision {
    /// Run this once, outside the sandbox; remember nothing.
    AllowOnce,
    /// Run it and remember this exact command for the rest of the session, so
    /// an identical escape does not re-prompt.
    AllowForSession,
    /// Refuse; the command is aborted.
    Deny,
}

#[derive(Debug, Clone)]
pub enum InteractionAnswer {
    Choices(Vec<usize>),
    /// Reserved: a free-text answer. Only constructed in tests today; the TUI
    /// question modal doesn't yet collect custom input.
    #[cfg(test)]
    Custom(String),
}

#[derive(Debug, Clone)]
pub enum InteractionOutcome {
    Permission(PermissionDecision),
    SandboxEscalation(EscalationDecision),
    Question(Option<InteractionAnswer>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionStatus {
    Cancelled,
    Noninteractive,
}

#[derive(Clone)]
pub struct InteractionService {
    sender: mpsc::UnboundedSender<InteractionRequest>,
    pending: Arc<Mutex<PendingState>>,
    noninteractive: bool,
}

#[derive(Default)]
struct PendingState {
    next_request_id: u64,
    responders: BTreeMap<u64, oneshot::Sender<InteractionOutcome>>,
}

impl PendingState {
    fn next_request_id(&mut self) -> u64 {
        let mut candidate = self.next_request_id.max(1);
        while self.responders.contains_key(&candidate) {
            candidate = candidate.checked_add(1).unwrap_or(1);
        }
        self.next_request_id = candidate.checked_add(1).unwrap_or(1);
        candidate
    }
}

impl InteractionService {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<InteractionRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                sender: tx,
                pending: Arc::new(Mutex::new(PendingState::default())),
                noninteractive: false,
            },
            rx,
        )
    }

    pub fn noninteractive() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self {
            sender: tx,
            pending: Arc::new(Mutex::new(PendingState::default())),
            noninteractive: true,
        }
    }

    pub(crate) fn is_noninteractive(&self) -> bool {
        self.noninteractive
    }

    /// Number of unresolved permission/question responders owned by this
    /// service. Completion gates use this authoritative state instead of
    /// inferring pending user input from assistant prose.
    pub(crate) async fn pending_count(&self) -> usize {
        self.pending.lock().await.responders.len()
    }

    pub async fn request(
        &self,
        build: impl FnOnce(u64) -> InteractionRequest,
    ) -> Result<InteractionOutcome, InteractionStatus> {
        self.request_inner(build, None).await
    }

    /// Submit an interaction request and stop waiting if `cancellation_token`
    /// fires, removing the pending responder before returning.
    pub async fn request_with_cancellation(
        &self,
        build: impl FnOnce(u64) -> InteractionRequest,
        cancellation_token: CancellationToken,
    ) -> Result<InteractionOutcome, InteractionStatus> {
        self.request_inner(build, Some(cancellation_token)).await
    }

    async fn request_inner(
        &self,
        build: impl FnOnce(u64) -> InteractionRequest,
        cancellation_token: Option<CancellationToken>,
    ) -> Result<InteractionOutcome, InteractionStatus> {
        if self.noninteractive {
            return Err(InteractionStatus::Noninteractive);
        }
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending.lock().await;
        let request_id = pending.next_request_id();
        let request = build(request_id);
        pending.responders.insert(request_id, tx);
        drop(pending);
        if self.sender.send(request).is_err() {
            self.remove_pending(request_id).await;
            return Err(InteractionStatus::Cancelled);
        }
        if let Some(cancellation_token) = cancellation_token {
            tokio::select! {
                outcome = rx => match outcome {
                    Ok(outcome) => Ok(outcome),
                    Err(_) => {
                        self.remove_pending(request_id).await;
                        Err(InteractionStatus::Cancelled)
                    }
                },
                () = cancellation_token.cancelled() => {
                    self.remove_pending(request_id).await;
                    Err(InteractionStatus::Cancelled)
                }
            }
        } else {
            match rx.await {
                Ok(outcome) => Ok(outcome),
                Err(_) => {
                    self.remove_pending(request_id).await;
                    Err(InteractionStatus::Cancelled)
                }
            }
        }
    }

    async fn remove_pending(&self, request_id: u64) {
        let mut pending = self.pending.lock().await;
        pending.responders.remove(&request_id);
    }

    pub async fn respond(
        &self,
        request_id: u64,
        outcome: InteractionOutcome,
    ) -> Result<(), InteractionStatus> {
        let mut pending = self.pending.lock().await;
        if let Some(responder) = pending.responders.remove(&request_id) {
            if responder.send(outcome).is_err() {
                return Err(InteractionStatus::Cancelled);
            }
            return Ok(());
        }
        Err(InteractionStatus::Cancelled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn permission_prompt_returns_decision() {
        let (service, mut rx) = InteractionService::new();
        let handle = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .request(|id| InteractionRequest::Permission {
                        request_id: id,
                        command: "rm -rf /tmp/x".to_string(),
                        origin: None,
                    })
                    .await
            }
        });
        let request = rx.recv().await.expect("request");
        let request_id = match &request {
            InteractionRequest::Permission { request_id, .. } => *request_id,
            _ => panic!("expected permission request"),
        };
        assert!(
            service
                .respond(
                    request_id,
                    InteractionOutcome::Permission(PermissionDecision::AllowOnce),
                )
                .await
                .is_ok()
        );
        let outcome = handle.await.unwrap().expect("ok");
        assert!(matches!(
            outcome,
            InteractionOutcome::Permission(PermissionDecision::AllowOnce)
        ));
    }

    #[tokio::test]
    async fn sandbox_escalation_returns_decision() {
        let (service, mut rx) = InteractionService::new();
        let handle = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .request(|id| InteractionRequest::SandboxEscalation {
                        request_id: id,
                        command: "curl https://example.com".to_string(),
                        origin: None,
                        kind: SandboxEscalationKind::SandboxOnly,
                    })
                    .await
            }
        });
        let request_id = match rx.recv().await.expect("request") {
            InteractionRequest::SandboxEscalation { request_id, .. } => request_id,
            _ => panic!("expected sandbox escalation request"),
        };
        assert!(
            service
                .respond(
                    request_id,
                    InteractionOutcome::SandboxEscalation(EscalationDecision::AllowForSession),
                )
                .await
                .is_ok()
        );
        let outcome = handle.await.unwrap().expect("ok");
        assert!(matches!(
            outcome,
            InteractionOutcome::SandboxEscalation(EscalationDecision::AllowForSession)
        ));
    }

    #[tokio::test]
    async fn web_domain_prompt_returns_decision() {
        let (service, mut rx) = InteractionService::new();
        let handle = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .request(|id| InteractionRequest::WebDomain {
                        request_id: id,
                        url: "https://example.com/page".to_string(),
                        host: "example.com".to_string(),
                        redirected_from: None,
                        origin: None,
                    })
                    .await
            }
        });
        let request_id = match rx.recv().await.expect("request") {
            InteractionRequest::WebDomain { request_id, .. } => request_id,
            _ => panic!("expected web domain request"),
        };
        assert!(
            service
                .respond(
                    request_id,
                    InteractionOutcome::Permission(PermissionDecision::AllowForProject),
                )
                .await
                .is_ok()
        );
        let outcome = handle.await.unwrap().expect("ok");
        assert!(matches!(
            outcome,
            InteractionOutcome::Permission(PermissionDecision::AllowForProject)
        ));
    }

    #[tokio::test]
    async fn extension_tool_prompt_returns_decision() {
        let (service, mut rx) = InteractionService::new();
        let handle = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .request(|id| InteractionRequest::ExtensionTool {
                        request_id: id,
                        id: "mcp.github.create_issue".to_string(),
                        server: "github".to_string(),
                        capabilities: vec!["write".to_string()],
                        args_preview: "{\"title\":\"bug\"}".to_string(),
                    })
                    .await
            }
        });
        let request_id = match rx.recv().await.expect("request") {
            InteractionRequest::ExtensionTool { request_id, .. } => request_id,
            _ => panic!("expected extension tool request"),
        };
        assert!(
            service
                .respond(
                    request_id,
                    InteractionOutcome::Permission(PermissionDecision::AllowForProject),
                )
                .await
                .is_ok()
        );
        let outcome = handle.await.unwrap().expect("ok");
        assert!(matches!(
            outcome,
            InteractionOutcome::Permission(PermissionDecision::AllowForProject)
        ));
    }

    #[tokio::test]
    async fn concurrent_permission_prompts_keep_independent_responders() {
        let (service, mut rx) = InteractionService::new();
        let first = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .request(|id| InteractionRequest::Permission {
                        request_id: id,
                        command: "sleep 1".to_string(),
                        origin: None,
                    })
                    .await
            }
        });
        let second = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .request(|id| InteractionRequest::Permission {
                        request_id: id,
                        command: "sleep 2".to_string(),
                        origin: None,
                    })
                    .await
            }
        });

        let first_id = match rx.recv().await.expect("first request") {
            InteractionRequest::Permission { request_id, .. } => request_id,
            _ => panic!("expected first permission request"),
        };
        let second_id = match rx.recv().await.expect("second request") {
            InteractionRequest::Permission { request_id, .. } => request_id,
            _ => panic!("expected second permission request"),
        };

        assert_ne!(first_id, second_id);
        service
            .respond(
                second_id,
                InteractionOutcome::Permission(PermissionDecision::Deny),
            )
            .await
            .expect("second response should be delivered");
        service
            .respond(
                first_id,
                InteractionOutcome::Permission(PermissionDecision::AllowOnce),
            )
            .await
            .expect("first response should still be deliverable");

        assert!(matches!(
            first.await.expect("first task should finish"),
            Ok(InteractionOutcome::Permission(
                PermissionDecision::AllowOnce
            ))
        ));
        assert!(matches!(
            second.await.expect("second task should finish"),
            Ok(InteractionOutcome::Permission(PermissionDecision::Deny))
        ));
    }

    #[tokio::test]
    async fn question_prompt_can_be_cancelled_by_drop() {
        let (service, rx) = InteractionService::new();
        drop(rx);
        let result = service
            .request(|id| InteractionRequest::Question {
                request_id: id,
                prompt: "pick".to_string(),
                header: None,
                options: vec![QuestionOption {
                    label: "Yes".to_string(),
                    description: "".to_string(),
                    preselected: false,
                }],
                multiple: false,
                origin: None,
            })
            .await;
        assert_eq!(result.err(), Some(InteractionStatus::Cancelled));
    }

    #[tokio::test]
    async fn noninteractive_request_fails_without_sending_prompt() {
        let service = InteractionService::noninteractive();
        let result = service
            .request(|id| InteractionRequest::Question {
                request_id: id,
                prompt: "pick".to_string(),
                header: None,
                options: vec![QuestionOption {
                    label: "Yes".to_string(),
                    description: String::new(),
                    preselected: false,
                }],
                multiple: false,
                origin: None,
            })
            .await;

        assert_eq!(result.err(), Some(InteractionStatus::Noninteractive));
    }
}
