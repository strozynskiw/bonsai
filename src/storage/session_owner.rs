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

impl Drop for SessionOwnerGuard {
    fn drop(&mut self) {
        // A forked child can temporarily retain a duplicate descriptor even
        // when the file is close-on-exec. Unlock the shared file description
        // before closing so a drained session is immediately reclaimable.
        if let Err(error) = self._file.unlock() {
            tracing::warn!(%error, "failed to unlock session owner guard");
        }
    }
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

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn released_owner_unlocks_even_if_descriptor_was_duplicated() {
        let fixture = crate::storage::test_utils::TestStorage::new().await;
        let session_id = fixture.start_session().await;
        let guard = fixture
            .storage
            .try_session_owner(session_id)
            .unwrap()
            .unwrap();
        let duplicate = guard._file.try_clone().unwrap();
        assert!(
            fixture
                .storage
                .try_session_owner(session_id)
                .unwrap()
                .is_none()
        );
        drop(guard);
        let next = fixture.storage.try_session_owner(session_id).unwrap();
        assert!(
            next.is_some(),
            "released owner must not be held by a duplicate descriptor"
        );
        drop(next);
        drop(duplicate);
    }
}
