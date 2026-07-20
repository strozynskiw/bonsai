use std::collections::BTreeMap;
use std::sync::{Arc, Mutex as StdMutex};

use tokio_util::sync::CancellationToken;

pub(super) struct ActiveSubagentRun {
    active_runs: Arc<StdMutex<BTreeMap<String, CancellationToken>>>,
    id: String,
    token: CancellationToken,
}

impl ActiveSubagentRun {
    pub(super) fn insert(
        active_runs: Arc<StdMutex<BTreeMap<String, CancellationToken>>>,
        id: String,
        token: CancellationToken,
    ) -> Self {
        {
            let mut runs = active_runs.lock().unwrap_or_else(|p| p.into_inner());
            runs.insert(id.clone(), token.clone());
        }
        Self {
            active_runs,
            id,
            token,
        }
    }
}

impl Drop for ActiveSubagentRun {
    fn drop(&mut self) {
        self.token.cancel();
        let mut runs = self.active_runs.lock().unwrap_or_else(|p| p.into_inner());
        runs.remove(&self.id);
    }
}

/// Cancels a subagent provider call if the parent future is dropped before the
/// nested run completes normally.
pub(super) struct CancelSubagentOnDrop {
    token: CancellationToken,
}

impl CancelSubagentOnDrop {
    pub(super) fn new(token: CancellationToken) -> Self {
        Self { token }
    }
}

impl Drop for CancelSubagentOnDrop {
    fn drop(&mut self) {
        self.token.cancel();
    }
}
