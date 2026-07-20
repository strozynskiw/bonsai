//! Trust-on-first-use for project-owned configuration and instructions.
//!
//! A repository can carry MCP launch commands, lifecycle hooks, skills,
//! subagents, and steering files. Until its root is explicitly trusted, those
//! project-owned surfaces stay inert; only user-global configuration remains
//! active. The decision is stored in its own permission namespace so it cannot
//! accidentally authorize a shell command, domain, or extension tool.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use crate::interaction::{
    InteractionAnswer, InteractionOutcome, InteractionRequest, InteractionService, QuestionOption,
};
use crate::permissions::{Permission, PermissionManager};
use crate::storage::Storage;

/// Stable per-project key in the `workspace_trust` permission namespace.
pub(crate) const PROJECT_TRUST_PATTERN: &str = "workspace.trust.v1";

/// Whether project-owned executable/configuration surfaces may be activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceTrust {
    Trusted,
    Restricted,
}

/// Persistent decision handle plus its resolved current state.
#[derive(Clone)]
pub(crate) struct WorkspaceTrustGate {
    permissions: PermissionManager,
}

impl WorkspaceTrustGate {
    /// Load the persisted decision for one project root.
    pub(crate) async fn load(storage: Storage, project_id: i64) -> Result<Self> {
        Ok(Self {
            permissions: PermissionManager::load_workspace_trust(storage, project_id).await?,
        })
    }

    /// Current startup posture. Unknown and explicitly denied projects both
    /// remain restricted; [`Self::needs_prompt`] distinguishes the former so
    /// a recorded "keep restricted" decision does not ask again.
    pub(crate) fn state(&self) -> WorkspaceTrust {
        if self.permissions.check_one(PROJECT_TRUST_PATTERN) == Permission::Allow {
            WorkspaceTrust::Trusted
        } else {
            WorkspaceTrust::Restricted
        }
    }

    pub(crate) fn needs_prompt(&self) -> bool {
        self.permissions.check_one(PROJECT_TRUST_PATTERN) == Permission::Ask
    }

    /// Persist the user's explicit first-run trust posture.
    pub(crate) async fn set_state(&self, state: WorkspaceTrust) -> Result<()> {
        match state {
            WorkspaceTrust::Trusted => {
                self.permissions
                    .allow_for_project(PROJECT_TRUST_PATTERN)
                    .await
            }
            WorkspaceTrust::Restricted => {
                self.permissions
                    .deny_for_project(PROJECT_TRUST_PATTERN)
                    .await
            }
        }
    }

    /// Queue the one-time interactive question once the TUI event loop is
    /// about to start. Startup has already deliberately used the restricted
    /// posture, so a newly trusted workspace becomes active on the next launch
    /// rather than spawning configured code halfway through a session.
    pub(crate) fn prompt_after_startup(
        &self,
        interaction: Arc<InteractionService>,
        project_root: &Path,
    ) {
        if !self.needs_prompt() {
            return;
        }
        let permissions = self.permissions.clone();
        let root = project_root.display().to_string();
        tokio::spawn(async move {
            let outcome = interaction
                .request(move |request_id| InteractionRequest::Question {
                    request_id,
                    header: Some("Workspace trust".to_string()),
                    prompt: format!(
                        "Project: {root}\n\nProject-owned configuration is disabled until trusted."
                    ),
                    options: vec![
                        QuestionOption {
                            label: "Trust & restart".to_string(),
                            description:
                                "Enable .bonsai config, MCP, hooks, skills, subagents, and steering."
                                    .to_string(),
                            preselected: false,
                        },
                        QuestionOption {
                            label: "Keep restricted".to_string(),
                            description: "Leave project-owned configuration disabled.".to_string(),
                            preselected: false,
                        },
                    ],
                    multiple: false,
                    origin: None,
                })
                .await;

            match outcome {
                Ok(InteractionOutcome::Question(Some(InteractionAnswer::Choices(choices))))
                    if choices.contains(&0) =>
                {
                    if let Err(error) = permissions.allow_for_project(PROJECT_TRUST_PATTERN).await {
                        tracing::warn!(%error, "failed to persist workspace trust decision");
                    }
                }
                Ok(InteractionOutcome::Question(Some(InteractionAnswer::Choices(_)))) => {
                    if let Err(error) = permissions.deny_for_project(PROJECT_TRUST_PATTERN).await {
                        tracing::warn!(%error, "failed to persist restricted workspace decision");
                    }
                }
                Ok(_) | Err(_) => {
                    // Cancellation/noninteractive failure is intentionally not
                    // persisted: the next interactive launch can ask again.
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn project_trust_and_restriction_persist_in_their_own_namespace() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let project_id = fixture
            .storage
            .ensure_project(fixture.project_path())
            .await
            .unwrap();
        let gate = WorkspaceTrustGate::load(fixture.storage.clone(), project_id)
            .await
            .unwrap();
        assert_eq!(gate.state(), WorkspaceTrust::Restricted);
        assert!(gate.needs_prompt());

        gate.set_state(WorkspaceTrust::Trusted).await.unwrap();
        let reloaded = WorkspaceTrustGate::load(fixture.storage.clone(), project_id)
            .await
            .unwrap();
        assert_eq!(reloaded.state(), WorkspaceTrust::Trusted);

        reloaded
            .set_state(WorkspaceTrust::Restricted)
            .await
            .unwrap();
        let denied = WorkspaceTrustGate::load(fixture.storage, project_id)
            .await
            .unwrap();
        assert_eq!(denied.state(), WorkspaceTrust::Restricted);
        assert!(!denied.needs_prompt());
    }

    /// Drive the one-time startup trust question end-to-end through the real
    /// [`InteractionService`], answering with `answer` (`None` models a
    /// dismissed/cancelled question). Returns the storage fixture + project id
    /// so callers can reload the gate and assert what was persisted.
    async fn prompt_and_answer(
        answer: Option<Vec<usize>>,
    ) -> (crate::storage::test_utils::TestStorage, i64) {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let project_id = fixture
            .storage
            .ensure_project(fixture.project_path())
            .await
            .unwrap();
        let gate = WorkspaceTrustGate::load(fixture.storage.clone(), project_id)
            .await
            .unwrap();
        assert!(gate.needs_prompt(), "fresh project must ask");

        let (service, mut rx) = InteractionService::new();
        let service = Arc::new(service);
        gate.prompt_after_startup(service.clone(), fixture.project_path());

        let request = rx.recv().await.expect("trust question reaches the UI");
        let InteractionRequest::Question { request_id, .. } = request else {
            panic!("unexpected interaction request kind");
        };
        service
            .respond(
                request_id,
                InteractionOutcome::Question(answer.map(InteractionAnswer::Choices)),
            )
            .await
            .expect("answer must be deliverable");
        (fixture, project_id)
    }

    async fn reload(
        fixture: &crate::storage::test_utils::TestStorage,
        id: i64,
    ) -> WorkspaceTrustGate {
        WorkspaceTrustGate::load(fixture.storage.clone(), id)
            .await
            .unwrap()
    }

    /// Wait for the spawned prompt task's persistence to become visible, or
    /// time out and return the last observed gate.
    async fn reload_until(
        fixture: &crate::storage::test_utils::TestStorage,
        id: i64,
        settled: impl Fn(&WorkspaceTrustGate) -> bool,
    ) -> WorkspaceTrustGate {
        for _ in 0..100 {
            let gate = reload(fixture, id).await;
            if settled(&gate) {
                return gate;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        reload(fixture, id).await
    }

    #[tokio::test]
    async fn startup_prompt_trust_choice_persists_trusted() {
        let (fixture, id) = prompt_and_answer(Some(vec![0])).await;
        let gate = reload_until(&fixture, id, |gate| !gate.needs_prompt()).await;
        assert_eq!(gate.state(), WorkspaceTrust::Trusted);
        assert!(!gate.needs_prompt(), "decision must be durable");
    }

    #[tokio::test]
    async fn startup_prompt_restricted_choice_persists_and_never_asks_again() {
        let (fixture, id) = prompt_and_answer(Some(vec![1])).await;
        let gate = reload_until(&fixture, id, |gate| !gate.needs_prompt()).await;
        assert_eq!(gate.state(), WorkspaceTrust::Restricted);
        assert!(
            !gate.needs_prompt(),
            "an explicit 'keep restricted' must not re-ask on the next launch"
        );
    }

    /// The deliberate cancel-vs-deny distinction: dismissing the question
    /// persists nothing, so the next interactive launch asks again — while the
    /// posture stays restricted in the meantime.
    #[tokio::test]
    async fn startup_prompt_dismissal_persists_nothing_and_asks_again() {
        let (fixture, id) = prompt_and_answer(None).await;
        // Nothing to await: give the spawned task time to (wrongly) persist.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let gate = reload(&fixture, id).await;
        assert_eq!(gate.state(), WorkspaceTrust::Restricted);
        assert!(
            gate.needs_prompt(),
            "a dismissed question must ask again next launch"
        );
    }
}
