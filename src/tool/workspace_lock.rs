use std::path::PathBuf;

use anyhow::Result;

use crate::storage::{SessionId, Storage, WorkspaceLockGuard};
use crate::tool::SharedActiveSessionId;

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceLockContext {
    project_root: PathBuf,
    storage: Option<Storage>,
    active_session_id: Option<SharedActiveSessionId>,
}

impl WorkspaceLockContext {
    pub(crate) fn disabled(project_root: PathBuf) -> Self {
        Self {
            project_root,
            storage: None,
            active_session_id: None,
        }
    }

    pub(crate) fn new(
        project_root: PathBuf,
        storage: Storage,
        active_session_id: SharedActiveSessionId,
    ) -> Self {
        Self {
            project_root,
            storage: Some(storage),
            active_session_id: Some(active_session_id),
        }
    }

    pub(crate) async fn acquire_write(&self) -> Result<Option<WorkspaceLockGuard>> {
        let Some(storage) = &self.storage else {
            return Ok(None);
        };
        let Some(session_id) = self.active_session().await else {
            return Ok(None);
        };
        storage
            .acquire_workspace_write_lock(&self.project_root, session_id)
            .await
            .map(Some)
    }

    async fn active_session(&self) -> Option<SessionId> {
        let active_session_id = self.active_session_id.as_ref()?;
        *active_session_id.lock().await
    }
}
