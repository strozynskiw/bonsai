//! Session active-time ownership shared by interactive and headless runs.

use std::sync::Arc;

use anyhow::{Result, bail};
use tokio::sync::Mutex;

use crate::storage::{SessionId, Storage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionActivity {
    pub(crate) active: bool,
    pub(crate) active_run_ms: u64,
}

#[derive(Debug, Default)]
struct ActivityState {
    owner: Option<SessionId>,
    main_runs: usize,
}

/// Serializes one session's union-of-agent activity segment.
///
/// A segment is opened by the first main-agent run and remains open until both
/// that run and every registered subagent have finished. The owner is retained
/// through detached tails so a session rotation cannot attribute their time to
/// a replacement row.
#[derive(Debug, Clone)]
pub(crate) struct SessionActivityGate {
    storage: Storage,
    state: Arc<Mutex<ActivityState>>,
}

impl SessionActivityGate {
    pub(crate) fn new(storage: Storage) -> Self {
        Self {
            storage,
            state: Arc::new(Mutex::new(ActivityState::default())),
        }
    }

    /// Claim main-agent activity and open the durable segment when needed.
    pub(crate) async fn begin_main(&self, session_id: SessionId) -> Result<SessionActivity> {
        let mut state = self.state.lock().await;
        match state.owner {
            Some(owner) if owner != session_id => {
                bail!("session activity is still owned by {owner}")
            }
            Some(_) => {}
            None => {
                let active_run_ms = self.storage.begin_session_run(session_id).await?;
                state.owner = Some(session_id);
                state.main_runs = 1;
                return Ok(SessionActivity {
                    active: true,
                    active_run_ms,
                });
            }
        }
        state.main_runs = state.main_runs.saturating_add(1);
        let Some(owner) = state.owner else {
            bail!("session activity owner disappeared while beginning a main run");
        };
        Ok(SessionActivity {
            active: true,
            active_run_ms: self.storage.session_active_run_ms_now(owner).await?,
        })
    }

    /// Release one main-agent claim and reconcile it with subagent liveness.
    pub(crate) async fn finish_main(&self, subagents_running: bool) -> Result<SessionActivity> {
        let mut state = self.state.lock().await;
        state.main_runs = state.main_runs.saturating_sub(1);
        self.reconcile_locked(&mut state, subagents_running).await
    }

    /// Reconcile authoritative subagent liveness after any runtime event.
    pub(crate) async fn reconcile(&self, subagents_running: bool) -> Result<SessionActivity> {
        let mut state = self.state.lock().await;
        self.reconcile_locked(&mut state, subagents_running).await
    }

    /// Whether an active interval still owns a session row.
    pub(crate) async fn is_active(&self) -> bool {
        self.state.lock().await.owner.is_some()
    }

    async fn reconcile_locked(
        &self,
        state: &mut ActivityState,
        subagents_running: bool,
    ) -> Result<SessionActivity> {
        let Some(owner) = state.owner else {
            return Ok(SessionActivity {
                active: false,
                active_run_ms: 0,
            });
        };
        if state.main_runs > 0 || subagents_running {
            return Ok(SessionActivity {
                active: true,
                active_run_ms: self.storage.session_active_run_ms_now(owner).await?,
            });
        }

        let active_run_ms = self.storage.finish_session_run(owner).await?;
        state.owner = None;
        Ok(SessionActivity {
            active: false,
            active_run_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detached_tail_keeps_one_persisted_union_segment_open() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let session_id = fixture.start_session().await;
        let gate = SessionActivityGate::new(fixture.storage.clone());

        let started = gate.begin_main(session_id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let tail = gate.finish_main(true).await.unwrap();
        assert!(tail.active);
        assert!(tail.active_run_ms > started.active_run_ms);

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let running = gate.reconcile(true).await.unwrap();
        assert!(running.active);
        assert!(running.active_run_ms > tail.active_run_ms);

        let finished = gate.reconcile(false).await.unwrap();
        assert!(!finished.active);
        assert_eq!(
            fixture
                .storage
                .session_active_run_ms_now(session_id)
                .await
                .unwrap(),
            finished.active_run_ms
        );
        assert_eq!(
            gate.reconcile(false).await.unwrap().active_run_ms,
            0,
            "closing an already idle gate is idempotent"
        );
    }
}
