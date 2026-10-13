use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

/// Serializes startup snapshots and migrations across processes. Closing the
/// handle releases the lock; the file must remain so waiters share one inode.
#[derive(Debug)]
pub(super) struct StartupLock {
    _file: File,
}

impl StartupLock {
    pub(super) async fn acquire(database: &Path) -> Result<Self> {
        Self::acquire_with_timeout(database, Duration::from_secs(30)).await
    }

    async fn acquire_with_timeout(database: &Path, timeout: Duration) -> Result<Self> {
        let path = lock_path(database)?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("Failed to open SQLite startup lock {path:?}"))?;
        #[cfg(unix)]
        super::set_unix_mode(&path, 0o600)?;

        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(TryLockError::Error(error)) => {
                    return Err(error).with_context(|| {
                        format!("Failed to acquire SQLite startup lock {path:?}")
                    });
                }
                Err(TryLockError::WouldBlock) => {
                    if tokio::time::Instant::now() >= deadline {
                        anyhow::bail!("Timed out waiting for SQLite startup lock {path:?}");
                    }
                    tokio::time::sleep_until(
                        deadline.min(tokio::time::Instant::now() + Duration::from_millis(25)),
                    )
                    .await;
                }
            }
        }
    }
}

fn lock_path(database: &Path) -> Result<PathBuf> {
    let canonical = match std::fs::canonicalize(database) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = database
                .parent()
                .filter(|path| !path.as_os_str().is_empty());
            let parent = parent.unwrap_or_else(|| Path::new("."));
            let name = database
                .file_name()
                .context("SQLite database has no filename")?;
            std::fs::canonicalize(parent)
                .with_context(|| format!("Failed to resolve SQLite directory {parent:?}"))?
                .join(name)
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to resolve SQLite path {database:?}"));
        }
    };
    Ok(super::sidecar_path(&canonical, ".startup.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_lock_serializes_aliases_and_releases_on_drop() {
        let home = tempfile::tempdir().unwrap();
        let database = home.path().join("state.db");
        let guard = StartupLock::acquire(&database).await.unwrap();
        let alias = home.path().join(".").join("state.db");
        let error = StartupLock::acquire_with_timeout(&alias, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Timed out waiting"));
        let independent = StartupLock::acquire(&home.path().join("other.db"))
            .await
            .unwrap();
        drop(independent);
        drop(guard);
        let _reopened = StartupLock::acquire(&alias).await.unwrap();
        assert!(lock_path(&database).unwrap().exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(lock_path(&database).unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn cancelled_startup_waiter_does_not_keep_lock() {
        let home = tempfile::tempdir().unwrap();
        let database = home.path().join("state.db");
        let guard = StartupLock::acquire(&database).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), StartupLock::acquire(&database))
                .await
                .is_err()
        );
        drop(guard);
        let _reopened = StartupLock::acquire(&database).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_lock_resolves_existing_symlinks() {
        let home = tempfile::tempdir().unwrap();
        let database = home.path().join("state.db");
        std::fs::write(&database, []).unwrap();
        let alias = home.path().join("alias.db");
        std::os::unix::fs::symlink(&database, &alias).unwrap();
        let _guard = StartupLock::acquire(&database).await.unwrap();
        assert!(
            StartupLock::acquire_with_timeout(&alias, Duration::from_millis(50))
                .await
                .is_err()
        );
    }

    #[test]
    #[ignore = "child process used by startup_lock_excludes_other_processes"]
    fn startup_lock_child() {
        let database = std::env::var_os("BONSAI_TEST_STARTUP_LOCK_DB").unwrap();
        let ready = std::env::var_os("BONSAI_TEST_STARTUP_LOCK_READY").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let _guard = StartupLock::acquire(Path::new(&database)).await.unwrap();
            std::fs::write(ready, []).unwrap();
            // The parent terminates this process, exercising OS lock release
            // even when Rust destructors do not run.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
    }

    #[tokio::test]
    async fn startup_lock_excludes_other_processes() {
        let home = tempfile::tempdir().unwrap();
        let database = home.path().join("state.db");
        let ready = home.path().join("ready");
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "storage::startup_lock::tests::startup_lock_child",
                "--ignored",
            ])
            .env("BONSAI_TEST_STARTUP_LOCK_DB", &database)
            .env("BONSAI_TEST_STARTUP_LOCK_READY", &ready)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "lock child exited early"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            StartupLock::acquire_with_timeout(&database, Duration::from_millis(50))
                .await
                .unwrap_err()
                .to_string()
                .contains("Timed out waiting")
        );
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        let _reopened = StartupLock::acquire(&database).await.unwrap();
    }
}
