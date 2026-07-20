//! Lossless, atomic persistence for editable resource files.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

static PATH_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Whether an atomic write may replace an existing resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteMode {
    CreateNew,
    Upsert,
}

/// Mutation selected by a serialized text update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextMutation {
    Keep,
    Replace(String),
}

/// Read a UTF-8 resource, distinguishing an absent file from every other read
/// failure.
pub(crate) fn read_text(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Atomically write a resource in its own directory.
pub(crate) fn write_text(path: &Path, content: &str, mode: WriteMode) -> io::Result<()> {
    with_path_locks(&[path], || atomic_write_unlocked(path, content, mode))
}

/// Serialize a read-modify-write update for one UTF-8 resource.
///
/// The callback returns `None` when no write is needed. A missing file is
/// represented as an empty string; every other read error is propagated.
pub(crate) fn update_text<T>(
    path: &Path,
    update: impl FnOnce(&str) -> io::Result<(Option<String>, T)>,
) -> io::Result<T> {
    mutate_text(path, |existing| {
        let (replacement, outcome) = update(existing.unwrap_or_default())?;
        Ok((
            replacement.map_or(TextMutation::Keep, TextMutation::Replace),
            outcome,
        ))
    })
}

/// Serialize a read/replace/remove decision for one UTF-8 resource.
pub(crate) fn mutate_text<T>(
    path: &Path,
    update: impl FnOnce(Option<&str>) -> io::Result<(TextMutation, T)>,
) -> io::Result<T> {
    with_path_locks(&[path], || {
        let existing = read_text(path)?;
        let (mutation, outcome) = update(existing.as_deref())?;
        match mutation {
            TextMutation::Keep => {}
            TextMutation::Replace(replacement) => {
                atomic_write_unlocked(path, &replacement, WriteMode::Upsert)?;
            }
        }
        Ok(outcome)
    })
}

/// Save a new or edited agent definition, removing its previous path only after
/// the replacement is durable. A failed rename removal rolls the new path back.
pub(crate) fn save_agent(
    previous_path: Option<&Path>,
    path: &Path,
    content: &str,
) -> io::Result<()> {
    let paths = previous_path.map_or_else(|| vec![path], |previous| vec![previous, path]);
    with_path_locks(&paths, || {
        let mode = match previous_path {
            Some(previous) if previous == path => {
                match fs::metadata(path) {
                    Ok(metadata) if metadata.is_file() => {}
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("resource path is not a file: {}", path.display()),
                        ));
                    }
                    Err(error) => return Err(error),
                }
                WriteMode::Upsert
            }
            _ => WriteMode::CreateNew,
        };
        atomic_write_unlocked(path, content, mode)?;

        if let Some(previous) = previous_path
            && previous != path
            && let Err(remove_error) = fs::remove_file(previous)
        {
            return match fs::remove_file(path) {
                Ok(()) => Err(remove_error),
                Err(rollback_error) => Err(io::Error::new(
                    remove_error.kind(),
                    format!(
                        "failed to remove renamed resource {}: {remove_error}; also failed to roll back {}: {rollback_error}",
                        previous.display(),
                        path.display()
                    ),
                )),
            };
        }
        sync_parent(path)?;
        if let Some(previous) = previous_path
            && previous != path
        {
            sync_parent(previous)?;
        }
        Ok(())
    })
}

/// Remove a resource while serializing against other in-process mutations.
pub(crate) fn remove_file(path: &Path) -> io::Result<()> {
    with_path_locks(&[path], || {
        fs::remove_file(path)?;
        sync_parent(path)
    })
}

fn path_lock(path: &Path) -> Arc<Mutex<()>> {
    let normalized = path.to_path_buf();
    let mut locks = PATH_LOCKS.lock().unwrap_or_else(|error| error.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&normalized).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(normalized, Arc::downgrade(&lock));
    lock
}

fn with_path_locks<T>(paths: &[&Path], operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let mut paths = paths
        .iter()
        .map(|path| path.to_path_buf())
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    let locks = paths.iter().map(|path| path_lock(path)).collect::<Vec<_>>();
    let _guards = locks
        .iter()
        .map(|lock| lock.lock().unwrap_or_else(|error| error.into_inner()))
        .collect::<Vec<_>>();
    operation()
}

fn atomic_write_unlocked(path: &Path, content: &str, mode: WriteMode) -> io::Result<()> {
    atomic_write_with_hook(path, content, mode, || Ok(()))
}

fn atomic_write_with_hook(
    path: &Path,
    content: &str,
    mode: WriteMode,
    before_persist: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let existing_permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let mut temporary = tempfile::Builder::new()
        .prefix(".bonsai-resource-")
        .tempfile_in(parent)?;
    if let Some(permissions) = existing_permissions {
        temporary.as_file().set_permissions(permissions)?;
    }
    temporary.write_all(content.as_bytes())?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    before_persist()?;
    match mode {
        WriteMode::CreateNew => temporary
            .persist_noclobber(path)
            .map_err(|error| error.error)?,
        WriteMode::Upsert => temporary.persist(path).map_err(|error| error.error)?,
    };
    sync_parent(path)
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;

    #[test]
    fn failed_pre_persist_hook_leaves_original_unchanged() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join("agent.md");
        fs::write(&path, "original").unwrap();

        let error = atomic_write_with_hook(&path, "replacement", WriteMode::Upsert, || {
            Err(io::Error::new(io::ErrorKind::WriteZero, "injected"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
        assert_eq!(fs::read_to_string(path).unwrap(), "original");
    }

    #[test]
    fn concurrent_updates_do_not_lose_either_change() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = directory.path().join(".disabled");
        fs::write(&path, "base\n").unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for name in ["alpha", "beta"] {
            let path = path.clone();
            let barrier = barrier.clone();
            threads.push(thread::spawn(move || {
                barrier.wait();
                update_text(&path, |existing| {
                    Ok((Some(format!("{existing}{name}\n")), ()))
                })
                .unwrap();
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }

        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("base\n"));
        assert!(content.contains("alpha\n"));
        assert!(content.contains("beta\n"));
    }

    #[test]
    fn renamed_agent_replaces_old_path_without_clobbering_an_existing_target() {
        let directory = tempfile::TempDir::new().unwrap();
        let old = directory.path().join("old.md");
        let new = directory.path().join("new.md");
        fs::write(&old, "old").unwrap();

        save_agent(Some(&old), &new, "new").unwrap();

        assert!(!old.exists());
        assert_eq!(fs::read_to_string(&new).unwrap(), "new");
        fs::write(&old, "restored").unwrap();
        let error = save_agent(Some(&old), &new, "clobber").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&old).unwrap(), "restored");
        assert_eq!(fs::read_to_string(&new).unwrap(), "new");
    }
}
