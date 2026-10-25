//! Non-expiring exclusion for a server-owned session. Every resume path probes
//! the same OS lock; stale heartbeat is never permission to steal a live owner.
use std::fs::{File, OpenOptions, TryLockError};

use anyhow::{Context, Result};

use super::{SessionId, Storage};

#[derive(Debug)]
pub(crate) struct SessionOwnerGuard {
    _file: File,
    pub(super) session_id: SessionId,
}

impl Storage {
    pub(crate) fn try_session_owner(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionOwnerGuard>> {
        let database = self.paths.db_path().canonicalize()?;
        let path =
            super::sidecar_path(&database, &format!(".session-{}.lock", session_id.as_i64()));
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(path)
            .context("Failed to open session owner lock")?;
        match file.try_lock() {
            Ok(()) => Ok(Some(SessionOwnerGuard {
                _file: file,
                session_id,
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error).context("Failed to lock session owner"),
        }
    }
}
