//! Process-safe, crash-recoverable transactions for wizard-managed catalog
//! provider/model pairs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{CatalogError, CatalogPaths, ConnectionId};

const CATALOG_LOCK_FILE: &str = ".catalog.lock";
const TRANSACTION_ROOT: &str = ".catalog-transactions";
const JOURNAL_FILE: &str = "journal.toml";
const PUBLISHING_MARKER: &str = "publishing";
const COMMITTED_MARKER: &str = "committed";
const PROVIDER_NEW_FILE: &str = "provider.new";
const MODEL_NEW_FILE: &str = "model.new";
const PROVIDER_OLD_FILE: &str = "provider.old";
const MODEL_OLD_FILE: &str = "model.old";
const JOURNAL_SCHEMA_VERSION: u32 = 1;

static TRANSACTION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Desired state for one wizard-managed provider/model pair.
pub(super) enum CatalogPairUpdate<'a> {
    Present {
        provider_content: &'a str,
        model_content: &'a str,
    },
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DesiredPairState {
    Present,
    Absent,
}

#[derive(Debug, Serialize, Deserialize)]
struct CatalogTransactionJournal {
    schema_version: u32,
    transaction_id: String,
    connection_id: ConnectionId,
    desired_state: DesiredPairState,
    provider_existed: bool,
    model_existed: bool,
}

#[derive(Debug)]
struct PreparedCatalogTransaction {
    directory: PathBuf,
    transaction_id: String,
    journal: CatalogTransactionJournal,
    provider_path: PathBuf,
    model_path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionCheckpoint {
    Prepared,
    ProviderPublished,
    ModelPublished,
    Validated,
    Committed,
}

trait TransactionControl {
    fn should_simulate_crash(&mut self, _checkpoint: TransactionCheckpoint) -> bool {
        false
    }
}

struct NoopTransactionControl;

impl TransactionControl for NoopTransactionControl {}

/// Run an operation while holding the process-wide catalog mutation lock.
/// Incomplete transactions are recovered before the operation can inspect the
/// provider/model directories.
pub(super) fn with_recovered_catalog_lock<T>(
    home_dir: &Path,
    operation: impl FnOnce() -> Result<T, CatalogError>,
) -> Result<T, CatalogError> {
    fs::create_dir_all(home_dir).map_err(|source| CatalogError::CreateDir {
        path: home_dir.to_path_buf(),
        source,
    })?;
    let lock_path = home_dir.join(CATALOG_LOCK_FILE);
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| CatalogError::CatalogLock {
            path: lock_path.clone(),
            source,
        })?;
    lock_file
        .lock()
        .map_err(|source| CatalogError::CatalogLock {
            path: lock_path,
            source,
        })?;
    recover_catalog_transactions(home_dir)?;
    detect_legacy_catalog_backups(home_dir)?;
    operation()
}

/// Publish both files (or remove both) under one durable journal. The caller
/// must already hold [`with_recovered_catalog_lock`] so its validation and
/// preflight checks share the same serialization boundary.
pub(super) fn commit_catalog_pair_locked<T>(
    home_dir: &Path,
    paths: &CatalogPaths,
    connection_id: &ConnectionId,
    update: CatalogPairUpdate<'_>,
    validate: impl FnOnce() -> Result<T, CatalogError>,
) -> Result<T, CatalogError> {
    commit_catalog_pair_controlled(
        home_dir,
        paths,
        connection_id,
        update,
        validate,
        &mut NoopTransactionControl,
    )
}

fn commit_catalog_pair_controlled<T>(
    home_dir: &Path,
    paths: &CatalogPaths,
    connection_id: &ConnectionId,
    update: CatalogPairUpdate<'_>,
    validate: impl FnOnce() -> Result<T, CatalogError>,
    control: &mut impl TransactionControl,
) -> Result<T, CatalogError> {
    let transaction = prepare_catalog_transaction(home_dir, paths, connection_id, update)?;
    if control.should_simulate_crash(TransactionCheckpoint::Prepared) {
        return Err(simulated_crash_error(
            &transaction,
            TransactionCheckpoint::Prepared,
        ));
    }

    if let Err(error) = create_durable_marker(&transaction.directory, PUBLISHING_MARKER) {
        let _ = cleanup_transaction(&transaction.directory);
        return Err(error);
    }

    if let Err(error) = publish_provider(&transaction) {
        return rollback_after_error(&transaction, error);
    }
    if control.should_simulate_crash(TransactionCheckpoint::ProviderPublished) {
        return Err(simulated_crash_error(
            &transaction,
            TransactionCheckpoint::ProviderPublished,
        ));
    }

    if let Err(error) = publish_model(&transaction) {
        return rollback_after_error(&transaction, error);
    }
    if control.should_simulate_crash(TransactionCheckpoint::ModelPublished) {
        return Err(simulated_crash_error(
            &transaction,
            TransactionCheckpoint::ModelPublished,
        ));
    }

    let validated = match validate() {
        Ok(validated) => validated,
        Err(error) => return rollback_after_error(&transaction, error),
    };
    if control.should_simulate_crash(TransactionCheckpoint::Validated) {
        return Err(simulated_crash_error(
            &transaction,
            TransactionCheckpoint::Validated,
        ));
    }

    if let Err(error) = create_durable_marker(&transaction.directory, COMMITTED_MARKER) {
        return rollback_after_error(&transaction, error);
    }
    if control.should_simulate_crash(TransactionCheckpoint::Committed) {
        return Err(simulated_crash_error(
            &transaction,
            TransactionCheckpoint::Committed,
        ));
    }

    if let Err(error) = cleanup_transaction(&transaction.directory) {
        // The committed marker is durable, so recovery will retain the new
        // pair and retry cleanup. Do not turn a committed mutation into a false
        // failure merely because derived journal cleanup was interrupted.
        tracing::warn!(
            path = %transaction.directory.display(),
            error = %error,
            "catalog pair committed; deferred transaction cleanup until next load"
        );
    }
    Ok(validated)
}

fn prepare_catalog_transaction(
    home_dir: &Path,
    paths: &CatalogPaths,
    connection_id: &ConnectionId,
    update: CatalogPairUpdate<'_>,
) -> Result<PreparedCatalogTransaction, CatalogError> {
    let transaction_root = home_dir.join(TRANSACTION_ROOT);
    fs::create_dir_all(&transaction_root).map_err(|source| CatalogError::CreateDir {
        path: transaction_root.clone(),
        source,
    })?;
    sync_directory(home_dir)?;
    let (transaction_id, directory) = create_unique_transaction_dir(&transaction_root)?;
    sync_directory(&transaction_root)?;

    let provider_path = paths
        .provider_dir
        .join(format!("{}.toml", connection_id.as_str()));
    let model_path = paths
        .model_dir
        .join(format!("{}.toml", connection_id.as_str()));

    let prepared = (|| {
        let desired_state = match update {
            CatalogPairUpdate::Present {
                provider_content,
                model_content,
            } => {
                durable_write_new(
                    &directory.join(PROVIDER_NEW_FILE),
                    provider_content.as_bytes(),
                )?;
                durable_write_new(&directory.join(MODEL_NEW_FILE), model_content.as_bytes())?;
                DesiredPairState::Present
            }
            CatalogPairUpdate::Absent => DesiredPairState::Absent,
        };
        let provider_existed =
            snapshot_existing_file(&provider_path, &directory.join(PROVIDER_OLD_FILE))?;
        let model_existed = snapshot_existing_file(&model_path, &directory.join(MODEL_OLD_FILE))?;
        let journal = CatalogTransactionJournal {
            schema_version: JOURNAL_SCHEMA_VERSION,
            transaction_id: transaction_id.clone(),
            connection_id: connection_id.clone(),
            desired_state,
            provider_existed,
            model_existed,
        };
        let content =
            toml::to_string_pretty(&journal).map_err(|source| CatalogError::TomlSerialize {
                source_name: directory.join(JOURNAL_FILE).display().to_string(),
                source,
            })?;
        durable_write_new(&directory.join(JOURNAL_FILE), content.as_bytes())?;
        sync_directory(&directory)?;
        Ok(PreparedCatalogTransaction {
            directory: directory.clone(),
            transaction_id: transaction_id.clone(),
            journal,
            provider_path,
            model_path,
        })
    })();

    if prepared.is_err() {
        let _ = cleanup_transaction(&directory);
    }
    prepared
}

fn create_unique_transaction_dir(root: &Path) -> Result<(String, PathBuf), CatalogError> {
    for _ in 0..32 {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let counter = TRANSACTION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let transaction_id = format!("{}-{timestamp}-{counter}", std::process::id());
        let directory = root.join(&transaction_id);
        match fs::create_dir(&directory) {
            Ok(()) => return Ok((transaction_id, directory)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(CatalogError::CreateDir {
                    path: directory,
                    source,
                });
            }
        }
    }
    Err(CatalogError::InvalidLocalCatalogJournal {
        path: root.to_path_buf(),
        message: "could not allocate a unique transaction directory".to_string(),
    })
}

fn snapshot_existing_file(source: &Path, backup: &Path) -> Result<bool, CatalogError> {
    match source.try_exists() {
        Ok(false) => return Ok(false),
        Ok(true) => {}
        Err(source_error) => {
            return Err(CatalogError::ReadFile {
                path: source.to_path_buf(),
                source: source_error,
            });
        }
    }
    fs::copy(source, backup).map_err(|source_error| CatalogError::WriteFile {
        path: backup.to_path_buf(),
        source: source_error,
    })?;
    sync_file(backup)?;
    Ok(true)
}

fn publish_provider(transaction: &PreparedCatalogTransaction) -> Result<(), CatalogError> {
    publish_one(transaction, &transaction.provider_path, PROVIDER_NEW_FILE)
}

fn publish_model(transaction: &PreparedCatalogTransaction) -> Result<(), CatalogError> {
    publish_one(transaction, &transaction.model_path, MODEL_NEW_FILE)
}

fn publish_one(
    transaction: &PreparedCatalogTransaction,
    target: &Path,
    staged_name: &str,
) -> Result<(), CatalogError> {
    match transaction.journal.desired_state {
        DesiredPairState::Present => durable_replace_from(
            &transaction.directory.join(staged_name),
            target,
            &transaction.transaction_id,
        ),
        DesiredPairState::Absent => durable_remove(target),
    }
}

fn rollback_after_error<T>(
    transaction: &PreparedCatalogTransaction,
    original_error: CatalogError,
) -> Result<T, CatalogError> {
    rollback_transaction(transaction)?;
    if let Err(error) = cleanup_transaction(&transaction.directory) {
        tracing::warn!(
            path = %transaction.directory.display(),
            error = %error,
            "catalog rollback completed; deferred transaction cleanup"
        );
    }
    Err(original_error)
}

fn rollback_transaction(transaction: &PreparedCatalogTransaction) -> Result<(), CatalogError> {
    restore_one(
        transaction,
        &transaction.provider_path,
        PROVIDER_OLD_FILE,
        transaction.journal.provider_existed,
    )?;
    restore_one(
        transaction,
        &transaction.model_path,
        MODEL_OLD_FILE,
        transaction.journal.model_existed,
    )?;
    Ok(())
}

fn restore_one(
    transaction: &PreparedCatalogTransaction,
    target: &Path,
    backup_name: &str,
    existed: bool,
) -> Result<(), CatalogError> {
    if !existed {
        return durable_remove(target);
    }
    let backup = transaction.directory.join(backup_name);
    if !backup.is_file() {
        return Err(CatalogError::InvalidLocalCatalogJournal {
            path: transaction.directory.join(JOURNAL_FILE),
            message: format!("required backup `{backup_name}` is missing"),
        });
    }
    durable_replace_from(&backup, target, &transaction.transaction_id)
}

fn recover_catalog_transactions(home_dir: &Path) -> Result<(), CatalogError> {
    let root = home_dir.join(TRANSACTION_ROOT);
    if !root.exists() {
        return Ok(());
    }
    let mut entries = fs::read_dir(&root)
        .map_err(|source| CatalogError::ReadDir {
            path: root.clone(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| CatalogError::ReadDir {
            path: root.clone(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let directory = entry.path();
        if !entry
            .file_type()
            .map_err(|source| CatalogError::ReadFile {
                path: directory.clone(),
                source,
            })?
            .is_dir()
        {
            return Err(CatalogError::InvalidLocalCatalogJournal {
                path: directory,
                message: "transaction root contains a non-directory artifact".to_string(),
            });
        }
        recover_one_transaction(home_dir, &directory)?;
    }
    Ok(())
}

fn recover_one_transaction(home_dir: &Path, directory: &Path) -> Result<(), CatalogError> {
    let journal_path = directory.join(JOURNAL_FILE);
    if !journal_path.exists() {
        if directory.join(PUBLISHING_MARKER).exists() || directory.join(COMMITTED_MARKER).exists() {
            return Err(CatalogError::InvalidLocalCatalogJournal {
                path: journal_path,
                message: "phase marker exists without recovery metadata".to_string(),
            });
        }
        cleanup_transaction(directory)?;
        return Ok(());
    }
    let journal_content =
        fs::read_to_string(&journal_path).map_err(|source| CatalogError::ReadFile {
            path: journal_path.clone(),
            source,
        })?;
    let journal: CatalogTransactionJournal =
        toml::from_str(&journal_content).map_err(|source| CatalogError::Toml {
            source_name: journal_path.display().to_string(),
            source,
        })?;
    let directory_id = directory
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    if journal.schema_version != JOURNAL_SCHEMA_VERSION || journal.transaction_id != directory_id {
        return Err(CatalogError::InvalidLocalCatalogJournal {
            path: journal_path,
            message: "unsupported schema or transaction identity mismatch".to_string(),
        });
    }
    let paths = CatalogPaths::from_home_dir(home_dir);
    let transaction = PreparedCatalogTransaction {
        directory: directory.to_path_buf(),
        transaction_id: journal.transaction_id.clone(),
        provider_path: paths
            .provider_dir
            .join(format!("{}.toml", journal.connection_id.as_str())),
        model_path: paths
            .model_dir
            .join(format!("{}.toml", journal.connection_id.as_str())),
        journal,
    };
    if directory.join(COMMITTED_MARKER).exists() {
        cleanup_transaction(directory)?;
        return Ok(());
    }
    if directory.join(PUBLISHING_MARKER).exists() {
        rollback_transaction(&transaction)?;
    }
    cleanup_transaction(directory)
}

fn detect_legacy_catalog_backups(home_dir: &Path) -> Result<(), CatalogError> {
    let paths = CatalogPaths::from_home_dir(home_dir);
    for directory in [&paths.provider_dir, &paths.model_dir] {
        if !directory.exists() {
            continue;
        }
        for entry in fs::read_dir(directory).map_err(|source| CatalogError::ReadDir {
            path: directory.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| CatalogError::ReadDir {
                path: directory.clone(),
                source,
            })?;
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".toml.bak"))
            {
                return Err(CatalogError::InvalidLocalCatalogJournal {
                    path,
                    message: "legacy backup requires manual recovery before catalog loading"
                        .to_string(),
                });
            }
        }
    }
    Ok(())
}

fn create_durable_marker(directory: &Path, name: &str) -> Result<(), CatalogError> {
    durable_write_new(&directory.join(name), b"")?;
    sync_directory(directory)
}

fn durable_write_new(path: &Path, bytes: &[u8]) -> Result<(), CatalogError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| CatalogError::WriteFile {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|source| CatalogError::WriteFile {
            path: path.to_path_buf(),
            source,
        })
}

fn durable_replace_from(
    source: &Path,
    target: &Path,
    transaction_id: &str,
) -> Result<(), CatalogError> {
    let parent = target
        .parent()
        .ok_or_else(|| CatalogError::InvalidLocalCatalogJournal {
            path: target.to_path_buf(),
            message: "catalog target has no parent directory".to_string(),
        })?;
    fs::create_dir_all(parent).map_err(|source_error| CatalogError::CreateDir {
        path: parent.to_path_buf(),
        source: source_error,
    })?;
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "catalog.toml".into());
    let temporary = target.with_file_name(format!(".{file_name}.{transaction_id}.tmp"));
    fs::copy(source, &temporary).map_err(|source_error| CatalogError::WriteFile {
        path: temporary.clone(),
        source: source_error,
    })?;
    sync_file(&temporary)?;
    #[cfg(windows)]
    if target.exists() {
        fs::remove_file(target).map_err(|source_error| CatalogError::WriteFile {
            path: target.to_path_buf(),
            source: source_error,
        })?;
    }
    fs::rename(&temporary, target).map_err(|source_error| CatalogError::RenameFile {
        path: target.to_path_buf(),
        temp_path: temporary,
        source: source_error,
    })?;
    sync_directory(parent)
}

fn durable_remove(path: &Path) -> Result<(), CatalogError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(CatalogError::WriteFile {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn sync_file(path: &Path) -> Result<(), CatalogError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|source| CatalogError::WriteFile {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), CatalogError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| CatalogError::WriteFile {
            path: path.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), CatalogError> {
    Ok(())
}

fn cleanup_transaction(directory: &Path) -> Result<(), CatalogError> {
    match fs::remove_dir_all(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(CatalogError::WriteFile {
                path: directory.to_path_buf(),
                source,
            });
        }
    }
    if let Some(root) = directory.parent() {
        sync_directory(root)?;
    }
    Ok(())
}

fn simulated_crash_error(
    transaction: &PreparedCatalogTransaction,
    checkpoint: TransactionCheckpoint,
) -> CatalogError {
    CatalogError::InvalidLocalCatalogInput {
        message: format!(
            "injected catalog transaction crash at {checkpoint:?} ({})",
            transaction.transaction_id
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;
    use std::time::Duration;

    use super::*;

    struct CrashAt {
        checkpoint: TransactionCheckpoint,
        on_reach: Option<Box<dyn FnMut() + Send>>,
    }

    impl TransactionControl for CrashAt {
        fn should_simulate_crash(&mut self, checkpoint: TransactionCheckpoint) -> bool {
            if checkpoint != self.checkpoint {
                return false;
            }
            if let Some(on_reach) = &mut self.on_reach {
                on_reach();
            }
            true
        }
    }

    fn pair_paths(home: &Path, connection: &ConnectionId) -> (CatalogPaths, PathBuf, PathBuf) {
        let paths = CatalogPaths::from_home_dir(home);
        fs::create_dir_all(&paths.provider_dir).unwrap();
        fs::create_dir_all(&paths.model_dir).unwrap();
        let provider = paths.provider_dir.join(format!("{connection}.toml"));
        let model = paths.model_dir.join(format!("{connection}.toml"));
        (paths, provider, model)
    }

    fn write_old_pair(home: &Path, connection: &ConnectionId) -> (PathBuf, PathBuf) {
        let (_paths, provider, model) = pair_paths(home, connection);
        fs::write(&provider, "old-provider").unwrap();
        fs::write(&model, "old-model").unwrap();
        (provider, model)
    }

    fn assert_pair(provider: &Path, model: &Path, expected_provider: &str, expected_model: &str) {
        assert_eq!(fs::read_to_string(provider).unwrap(), expected_provider);
        assert_eq!(fs::read_to_string(model).unwrap(), expected_model);
    }

    #[test]
    fn every_publish_phase_recovers_one_complete_generation() {
        for checkpoint in [
            TransactionCheckpoint::Prepared,
            TransactionCheckpoint::ProviderPublished,
            TransactionCheckpoint::ModelPublished,
            TransactionCheckpoint::Validated,
            TransactionCheckpoint::Committed,
        ] {
            let home = tempfile::TempDir::new().unwrap();
            let connection = "journal-test".parse::<ConnectionId>().unwrap();
            let (provider, model) = write_old_pair(home.path(), &connection);
            let paths = CatalogPaths::from_home_dir(home.path());
            let result = with_recovered_catalog_lock(home.path(), || {
                commit_catalog_pair_controlled(
                    home.path(),
                    &paths,
                    &connection,
                    CatalogPairUpdate::Present {
                        provider_content: "new-provider",
                        model_content: "new-model",
                    },
                    || Ok(()),
                    &mut CrashAt {
                        checkpoint,
                        on_reach: None,
                    },
                )
            });
            assert!(result.is_err(), "{checkpoint:?}");

            with_recovered_catalog_lock(home.path(), || Ok(())).unwrap();
            if checkpoint == TransactionCheckpoint::Committed {
                assert_pair(&provider, &model, "new-provider", "new-model");
            } else {
                assert_pair(&provider, &model, "old-provider", "old-model");
            }
            let root = home.path().join(TRANSACTION_ROOT);
            assert_eq!(fs::read_dir(root).unwrap().count(), 0);
        }
    }

    #[test]
    fn newer_writer_recovers_crashed_writer_before_publishing() {
        let home = tempfile::TempDir::new().unwrap();
        let home_path = home.path().to_path_buf();
        let connection = "concurrent-test".parse::<ConnectionId>().unwrap();
        let (provider, model) = write_old_pair(&home_path, &connection);
        let (reached_tx, reached_rx) = mpsc::channel();
        let resume = Arc::new(Barrier::new(2));
        let first_home = home_path.clone();
        let first_connection = connection.clone();
        let first_resume = resume.clone();
        let first = thread::spawn(move || {
            let paths = CatalogPaths::from_home_dir(&first_home);
            with_recovered_catalog_lock(&first_home, || {
                commit_catalog_pair_controlled(
                    &first_home,
                    &paths,
                    &first_connection,
                    CatalogPairUpdate::Present {
                        provider_content: "first-provider",
                        model_content: "first-model",
                    },
                    || Ok(()),
                    &mut CrashAt {
                        checkpoint: TransactionCheckpoint::ProviderPublished,
                        on_reach: Some(Box::new(move || {
                            reached_tx.send(()).unwrap();
                            first_resume.wait();
                        })),
                    },
                )
            })
        });
        reached_rx.recv().unwrap();

        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let second_home = home_path.clone();
        let second_connection = connection.clone();
        let second = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let paths = CatalogPaths::from_home_dir(&second_home);
            let result = with_recovered_catalog_lock(&second_home, || {
                commit_catalog_pair_locked(
                    &second_home,
                    &paths,
                    &second_connection,
                    CatalogPairUpdate::Present {
                        provider_content: "second-provider",
                        model_content: "second-model",
                    },
                    || Ok(()),
                )
            });
            done_tx.send(()).unwrap();
            result
        });
        started_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
        resume.wait();

        assert!(first.join().unwrap().is_err());
        second.join().unwrap().unwrap();
        assert_pair(&provider, &model, "second-provider", "second-model");
    }

    #[test]
    fn phase_marker_without_journal_is_reported() {
        let home = tempfile::TempDir::new().unwrap();
        let directory = home.path().join(TRANSACTION_ROOT).join("orphan");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(PUBLISHING_MARKER), b"").unwrap();

        let error = with_recovered_catalog_lock(home.path(), || Ok(())).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::InvalidLocalCatalogJournal { .. }
        ));
    }

    #[test]
    fn legacy_fixed_backup_is_reported_instead_of_ignored() {
        let home = tempfile::TempDir::new().unwrap();
        let paths = CatalogPaths::from_home_dir(home.path());
        fs::create_dir_all(&paths.provider_dir).unwrap();
        fs::write(paths.provider_dir.join("legacy.toml.bak"), "old").unwrap();

        let error = with_recovered_catalog_lock(home.path(), || Ok(())).unwrap_err();

        assert!(matches!(
            error,
            CatalogError::InvalidLocalCatalogJournal { .. }
        ));
    }
}
