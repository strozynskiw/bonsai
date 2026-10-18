use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::timing::{SharedTimingLedger, TimingCategory, TimingLane, TimingSpan};

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
    /// The question deadline elapsed; this is an assumption, not user consent.
    Recommended(usize),
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
    question_results: Arc<Mutex<QuestionResults>>,
    question_changed: Arc<Notify>,
    goal_question_timeout: Arc<AtomicU64>,
    sender: mpsc::UnboundedSender<InteractionRequest>,
    pending: Arc<Mutex<PendingState>>,
    noninteractive: bool,
    /// The active run's timing ledger, when it has one.
    ///
    /// Every interactive prompt passes through here, whichever tool asked for
    /// it, so this is the one place that can report *what the run was waiting
    /// for* rather than only how long the tool call took. Without it, an
    /// unanswered permission prompt looks like an active tool process.
    timing: Option<SharedTimingLedger>,
}

#[derive(Default)]
struct QuestionResults {
    active: usize,
    completed: Vec<String>,
}

struct InteractionWait {
    ledger: SharedTimingLedger,
    span: TimingSpan,
}

impl Drop for InteractionWait {
    fn drop(&mut self) {
        self.ledger.end(self.span);
    }
}

#[derive(Default)]
struct PendingState {
    next_request_id: u64,
    responders: BTreeMap<u64, oneshot::Sender<InteractionOutcome>>,
    questions: BTreeMap<u64, InteractionRequest>,
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
                question_results: Arc::new(Mutex::new(QuestionResults::default())),
                question_changed: Arc::new(Notify::new()),
                goal_question_timeout: Arc::new(AtomicU64::new(0)),
                sender: tx,
                pending: Arc::new(Mutex::new(PendingState::default())),
                noninteractive: false,
                timing: None,
            },
            rx,
        )
    }

    pub fn noninteractive() -> Self {
        let (tx, _rx) = mpsc::unbounded_channel();
        Self {
            question_results: Arc::new(Mutex::new(QuestionResults::default())),
            question_changed: Arc::new(Notify::new()),
            goal_question_timeout: Arc::new(AtomicU64::new(0)),
            sender: tx,
            pending: Arc::new(Mutex::new(PendingState::default())),
            noninteractive: true,
            timing: None,
        }
    }

    /// Attach the active run's ledger so prompts record their own wait.
    #[must_use]
    pub fn with_timing(mut self, timing: SharedTimingLedger) -> Self {
        self.timing = Some(timing);
        self
    }

    /// Category and bounded labels for one prompt's wait interval.
    fn wait_labels(request: &InteractionRequest) -> (TimingCategory, &'static str, &str) {
        match request {
            InteractionRequest::Permission { command, .. }
            | InteractionRequest::SandboxEscalation { command, .. } => {
                (TimingCategory::PermissionWait, "permission", command)
            }
            InteractionRequest::WebDomain { host, .. } => {
                (TimingCategory::PermissionWait, "web domain", host)
            }
            InteractionRequest::ExtensionTool { id, .. } => {
                (TimingCategory::PermissionWait, "extension tool", id)
            }
            InteractionRequest::HookTrust { name, .. } => {
                (TimingCategory::PermissionWait, "hook trust", name)
            }
            InteractionRequest::Question { prompt, .. } => {
                (TimingCategory::UserWait, "question", prompt)
            }
        }
    }

    /// Open a wait interval for a prompt that has just been delivered.
    ///
    /// Waits are attributed to the parent lane. A delegated child shares this
    /// service and the run's ledger, so a child's prompt currently lands in the
    /// parent's wait total; separating that needs the child's lane to travel
    /// with the request, which today it does not.
    fn begin_wait(&self, request: &InteractionRequest) -> Option<InteractionWait> {
        let ledger = self.timing.as_ref()?;
        let (category, owner, reason) = Self::wait_labels(request);
        Some(InteractionWait {
            ledger: ledger.clone(),
            span: ledger.begin(category, TimingLane::Parent, Some(owner), Some(reason)),
        })
    }

    pub(crate) fn is_noninteractive(&self) -> bool {
        self.noninteractive
    }

    /// Number of unresolved permission/question responders owned by this
    /// service. Completion gates use this authoritative state instead of
    /// inferring pending user input from assistant prose.
    pub(crate) async fn pending_count(&self) -> usize {
        self.pending
            .lock()
            .await
            .responders
            .values()
            .filter(|sender| !sender.is_closed())
            .count()
    }

    pub(crate) fn set_goal_question_timeout(&self, seconds: Option<u64>) {
        self.goal_question_timeout
            .store(seconds.unwrap_or(0), Ordering::Relaxed);
    }

    pub(crate) fn goal_question_timeout(&self) -> Option<u64> {
        match self.goal_question_timeout.load(Ordering::Relaxed) {
            0 => None,
            seconds => Some(seconds),
        }
    }

    /// Pending question snapshots let the UI reopen a dismissed prompt without
    /// restarting its deadline or inventing a second responder.
    pub(crate) async fn pending_questions(&self) -> Vec<InteractionRequest> {
        let pending = self.pending.lock().await;
        pending
            .questions
            .iter()
            .filter(|(id, _)| {
                pending
                    .responders
                    .get(id)
                    .is_some_and(|sender| !sender.is_closed())
            })
            .map(|(_, request)| request.clone())
            .collect()
    }

    pub(crate) async fn is_pending(&self, id: u64) -> bool {
        self.pending
            .lock()
            .await
            .responders
            .get(&id)
            .is_some_and(|sender| !sender.is_closed())
    }

    pub(crate) async fn request_question(
        &self,
        build: impl FnOnce(u64) -> InteractionRequest,
        timeout_seconds: Option<u64>,
        recommended: usize,
    ) -> Result<InteractionOutcome, InteractionStatus> {
        self.request_inner(
            build,
            None,
            timeout_seconds.map(|seconds| (Duration::from_secs(seconds), recommended)),
            true,
        )
        .await
    }

    /// Ask without parking the tool loop. The coordinator consumes the answer
    /// at its next safe boundary and waits for it before claiming completion.
    pub(crate) async fn spawn_question(
        &self,
        build: impl FnOnce(u64) -> InteractionRequest + Send + 'static,
        timeout_seconds: Option<u64>,
        recommended: usize,
        render: impl FnOnce(Result<InteractionOutcome, InteractionStatus>) -> String + Send + 'static,
    ) -> Result<u64, InteractionStatus> {
        if self.noninteractive {
            return Err(InteractionStatus::Noninteractive);
        }
        self.question_results.lock().await.active += 1;
        let service = self.clone();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut id = 0;
            let outcome = service
                .request_inner(
                    |request_id| {
                        id = request_id;
                        let _ = tx.send(request_id);
                        build(request_id)
                    },
                    None,
                    timeout_seconds.map(|seconds| (Duration::from_secs(seconds), recommended)),
                    false,
                )
                .await;
            let text = format!("Question #{id}: {}", render(outcome));
            let mut results = service.question_results.lock().await;
            results.active = results.active.saturating_sub(1);
            results.completed.push(text);
            drop(results);
            service.question_changed.notify_one();
        });
        rx.await.map_err(|_| InteractionStatus::Cancelled)
    }

    pub(crate) async fn take_question_results(&self) -> Vec<String> {
        std::mem::take(&mut self.question_results.lock().await.completed)
    }

    pub(crate) async fn has_question_work(&self) -> bool {
        let results = self.question_results.lock().await;
        results.active > 0 || !results.completed.is_empty()
    }

    pub(crate) async fn wait_question_results(
        &self,
        cancellation: &CancellationToken,
    ) -> Vec<String> {
        let mut wait = None;
        loop {
            let changed = self.question_changed.notified();
            {
                let mut results = self.question_results.lock().await;
                if !results.completed.is_empty() || results.active == 0 {
                    return std::mem::take(&mut results.completed);
                }
            }
            if wait.is_none() {
                wait = self.timing.as_ref().map(|ledger| InteractionWait {
                    ledger: ledger.clone(),
                    span: ledger.begin(
                        TimingCategory::UserWait,
                        TimingLane::Parent,
                        Some("questions"),
                        Some("Waiting for pending question answers"),
                    ),
                });
            }
            tokio::select! {
                () = changed => {},
                () = cancellation.cancelled() => return Vec::new(),
            }
        }
    }

    /// Only questions are cancelled; approval responders are never answered
    /// by this path or by a question deadline.
    pub(crate) async fn cancel_questions(&self) {
        let mut pending = self.pending.lock().await;
        let questions = std::mem::take(&mut pending.questions);
        for id in questions.keys() {
            pending.responders.remove(id);
        }
    }

    pub async fn request(
        &self,
        build: impl FnOnce(u64) -> InteractionRequest,
    ) -> Result<InteractionOutcome, InteractionStatus> {
        self.request_inner(build, None, None, true).await
    }

    /// Submit an interaction request and stop waiting if `cancellation_token`
    /// fires, removing the pending responder before returning.
    pub async fn request_with_cancellation(
        &self,
        build: impl FnOnce(u64) -> InteractionRequest,
        cancellation_token: CancellationToken,
    ) -> Result<InteractionOutcome, InteractionStatus> {
        self.request_inner(build, Some(cancellation_token), None, true)
            .await
    }

    async fn request_inner(
        &self,
        build: impl FnOnce(u64) -> InteractionRequest,
        cancellation_token: Option<CancellationToken>,
        question_timeout: Option<(Duration, usize)>,
        record_wait: bool,
    ) -> Result<InteractionOutcome, InteractionStatus> {
        if self.noninteractive {
            return Err(InteractionStatus::Noninteractive);
        }
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending.lock().await;
        let request_id = pending.next_request_id();
        let request = build(request_id);
        // Timeout defaults are only permitted for the question tool. Approval
        // requests always retain their explicit permission path.
        let question_timeout = if matches!(request, InteractionRequest::Question { .. }) {
            pending.questions.insert(request_id, request.clone());
            question_timeout
        } else {
            None
        };
        pending.responders.insert(request_id, tx);
        drop(pending);
        // From here the user has been asked and the run is blocked until they
        // answer, cancel, or the prompt dies with the UI. That whole stretch is
        // one wait interval, attributed to the prompt rather than to the tool
        // call that asked for it.
        let wait = record_wait.then(|| self.begin_wait(&request)).flatten();
        let outcome = if self.sender.send(request).is_err() {
            self.remove_pending(request_id).await;
            Err(InteractionStatus::Cancelled)
        } else {
            let cancellation_token = cancellation_token.unwrap_or_default();
            let deadline = async {
                match question_timeout {
                    Some((duration, _)) => tokio::time::sleep(duration).await,
                    None => std::future::pending::<()>().await,
                }
            };
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
                },
                () = deadline => {
                    self.remove_pending(request_id).await;
                    Ok(InteractionOutcome::Question(Some(InteractionAnswer::Recommended(
                        question_timeout.map_or(0, |(_, index)| index),
                    ))))
                }
            }
        };
        self.remove_pending(request_id).await;
        drop(wait);
        outcome
    }

    async fn remove_pending(&self, request_id: u64) {
        let mut pending = self.pending.lock().await;
        pending.responders.remove(&request_id);
        pending.questions.remove(&request_id);
    }

    pub async fn respond(
        &self,
        request_id: u64,
        outcome: InteractionOutcome,
    ) -> Result<(), InteractionStatus> {
        let mut pending = self.pending.lock().await;
        pending.questions.remove(&request_id);
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

    #[tokio::test(start_paused = true)]
    async fn pending_background_question_does_not_exclude_independent_work_from_time_budget() {
        let ledger = Arc::new(crate::timing::TimingLedger::system(
            crate::timing::TimingPolicy::ActiveCompute,
        ));
        let (service, mut rx) = InteractionService::new();
        let service = service.with_timing(ledger.clone());
        service
            .spawn_question(
                |request_id| InteractionRequest::Question {
                    request_id,
                    prompt: "Pick".into(),
                    header: None,
                    options: vec![QuestionOption::new("A", ""), QuestionOption::new("B", "")],
                    multiple: false,
                    origin: None,
                },
                Some(60),
                0,
                |_| "resolved".into(),
            )
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap();
        assert_eq!(ledger.snapshot().open_interval_count, 0);
        let waiter = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .wait_question_results(&CancellationToken::new())
                    .await
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(ledger.snapshot().open_interval_count, 1);
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert_eq!(
            ledger.snapshot().open_interval_count,
            0,
            "steering drops the wait future and must close its timing interval"
        );
        service.cancel_questions().await;
        let _ = service
            .wait_question_results(&CancellationToken::new())
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn goal_timeout_never_answers_permission_or_sandbox_requests() {
        let (service, mut rx) = InteractionService::new();
        service.set_goal_question_timeout(Some(1));
        for sandbox in [false, true] {
            let cancellation = CancellationToken::new();
            let prompt = tokio::spawn({
                let service = service.clone();
                let cancellation = cancellation.clone();
                async move {
                    service
                        .request_with_cancellation(
                            |request_id| {
                                if sandbox {
                                    InteractionRequest::SandboxEscalation {
                                        request_id,
                                        command: "deploy".into(),
                                        origin: None,
                                        kind: SandboxEscalationKind::SandboxOnly,
                                    }
                                } else {
                                    InteractionRequest::Permission {
                                        request_id,
                                        command: "deploy".into(),
                                        origin: None,
                                    }
                                }
                            },
                            cancellation,
                        )
                        .await
                }
            });
            let _ = rx.recv().await.unwrap();
            tokio::time::advance(Duration::from_secs(3600)).await;
            assert!(
                !prompt.is_finished(),
                "goal timeout must never authorize an action"
            );
            assert_eq!(service.pending_count().await, 1);
            cancellation.cancel();
            assert!(matches!(
                prompt.await.unwrap(),
                Err(InteractionStatus::Cancelled)
            ));
        }
    }

    #[tokio::test]
    async fn permission_waits_are_recorded_on_the_run_ledger() {
        let ledger: crate::timing::SharedTimingLedger = std::sync::Arc::new(
            crate::timing::TimingLedger::system(crate::timing::TimingPolicy::ActiveCompute),
        );
        let (service, mut rx) = InteractionService::new();
        let service = service.with_timing(ledger.clone());
        let prompt = tokio::spawn({
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
        prompt.await.expect("join").expect("ok");

        // The prompt is a wait, not work: it is counted once, closed, and kept
        // out of the active-compute clock a budget consumes.
        let snapshot = ledger.snapshot();
        assert_eq!(snapshot.categories.len(), 1);
        let row = &snapshot.categories[0];
        assert_eq!(row.category, crate::timing::TimingCategory::PermissionWait);
        assert_eq!(row.interval_count, 1);
        assert_eq!(row.open_count, 0);
        assert_eq!(row.latest_owner.as_deref(), Some("permission"));
        assert_eq!(snapshot.active_compute_ms, 0);
        assert_eq!(snapshot.open_interval_count, 0);
    }

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
