//! Native self-update: check GitHub for a newer signed release, verify it with
//! the same Ed25519 manifest chain as `install.sh`, and atomically stage the
//! new binary over the installed one. The running process is never hot-swapped
//! — a staged update takes effect on the next launch, so every surface talks
//! about "restart to apply".
//!
//! Safety gates, in order: only official builds (embedded signing identity)
//! ever update; a binary that no longer hashes to its own manifest is never
//! replaced (only notified); Homebrew-managed and non-writable installs
//! degrade to a notice; the manifest signature, archive hash, and extracted
//! binary hash are all verified in the same pass that downloads them. A lock
//! file under the bonsai home serializes concurrent sessions, and the staged
//! version/hash recorded in the state file keeps repeat startups idempotent.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::config::{UpdateConfig, UpdateMode};
use crate::release::{self, ReleaseError, ReleaseIdentity};

const STATE_FILE: &str = "update-state.json";
const LOCK_FILE: &str = "update.lock";
const BINARY_NAME: &str = "bonsai";
/// Startup checks are rate-limited to one real lookup per this window; forced
/// (`bonsai update`) runs ignore it.
const CHECK_INTERVAL_SECS: u64 = 20 * 60 * 60;
/// A lock older than this belongs to a crashed process and may be reclaimed.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(60 * 60);
/// Cap on the decompressed binary while streaming it out of the archive.
const BINARY_LIMIT: u64 = 512 * 1024 * 1024;

/// What one update run concluded. `Staged`/`AlreadyStaged` are the only
/// outcomes where the on-disk binary is newer than the running image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UpdateOutcome {
    /// No signing identity embedded — source/dev builds never self-update.
    DevBuild,
    /// `[update] mode = "off"` (startup runs only).
    Disabled,
    /// The check interval has not elapsed (startup runs only).
    TooSoon,
    /// Another bonsai process holds the update lock.
    Busy,
    AlreadyCurrent,
    /// A newer binary is already staged on disk; restart to apply.
    AlreadyStaged {
        version: String,
    },
    /// This run downloaded, verified, and staged a newer binary.
    Staged {
        version: String,
    },
    /// A newer signed release exists but was deliberately not installed.
    NotifyOnly {
        version: String,
        reason: NotifyReason,
    },
    /// Network-shaped failure; nothing was concluded and the check interval
    /// was not consumed.
    CheckFailed,
    /// Metadata or signature verification failed; nothing was staged.
    VerificationFailed,
}

/// Why a known-newer release was surfaced instead of installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotifyReason {
    /// `mode = "notify"`, or an explicit `bonsai update --check`.
    Notify,
    /// The executable lives under a Homebrew prefix; brew owns it.
    Homebrew,
    /// The install location could not be resolved or written.
    NotWritable,
    /// The running binary does not hash to its own signed manifest —
    /// never auto-replace something we cannot attribute.
    SelfHashMismatch,
    /// `[update] pin` holds this install below the newest release.
    Pinned,
}

/// TUI entry point: respects the check interval and the configured mode.
pub(crate) async fn run_startup_update(bonsai_home: &Path, config: &UpdateConfig) -> UpdateOutcome {
    run_update(bonsai_home, config, RunKind::Startup).await
}

/// `bonsai update` entry point: ignores the interval and `mode`, but honors
/// every safety gate (pin, Homebrew, writability, verification chain).
pub(crate) async fn run_forced_update(
    bonsai_home: &Path,
    config: &UpdateConfig,
    check_only: bool,
) -> UpdateOutcome {
    let kind = if check_only {
        RunKind::ForcedCheck
    } else {
        RunKind::ForcedInstall
    };
    run_update(bonsai_home, config, kind).await
}

/// The persistent composer-meta hint for a startup outcome; `None` for
/// outcomes the TUI stays silent about (failures, too-soon, up to date).
pub(crate) fn startup_notice(outcome: &UpdateOutcome) -> Option<String> {
    match outcome {
        UpdateOutcome::Staged { version } | UpdateOutcome::AlreadyStaged { version } => {
            Some(format!("updated to v{version} — restart to apply"))
        }
        UpdateOutcome::NotifyOnly { version, reason } => Some(match reason {
            NotifyReason::Notify => format!("update v{version} available — run `bonsai update`"),
            NotifyReason::Homebrew => format!("update v{version} available — brew upgrade bonsai"),
            NotifyReason::NotWritable => {
                format!("update v{version} available — rerun the installer")
            }
            NotifyReason::SelfHashMismatch => {
                format!("update v{version} available — reinstall via install.sh")
            }
            NotifyReason::Pinned => format!("update v{version} available — held by update.pin"),
        }),
        UpdateOutcome::DevBuild
        | UpdateOutcome::Disabled
        | UpdateOutcome::TooSoon
        | UpdateOutcome::Busy
        | UpdateOutcome::AlreadyCurrent
        | UpdateOutcome::CheckFailed
        | UpdateOutcome::VerificationFailed => None,
    }
}

/// Upgrade a doctor `VerificationFailed` to `UpdateStaged` when the mismatch
/// is explained by a self-update that already replaced the on-disk binary:
/// the failure is real for the running image but resolves on restart.
pub(crate) async fn refine_release_status(
    status: crate::release::ReleaseStatus,
    bonsai_home: &Path,
) -> crate::release::ReleaseStatus {
    if status == crate::release::ReleaseStatus::VerificationFailed
        && let Some(version) = staged_update_ready(bonsai_home).await
    {
        return crate::release::ReleaseStatus::UpdateStaged { version };
    }
    status
}

/// If a newer staged binary is already on disk per the state file, return its
/// version. Doctor uses this to explain an online `VerificationFailed` that is
/// really "update staged; restart pending".
async fn staged_update_ready(bonsai_home: &Path) -> Option<String> {
    let state = read_state(bonsai_home);
    let current = release::current_version().ok()?;
    let version = staged_newer(&state, &current)?;
    let exe = current_exe_canonical()?;
    let disk = release::sha256_file(&exe).await.ok()?;
    state
        .staged_binary_sha256
        .as_deref()
        .filter(|staged| disk.eq_ignore_ascii_case(staged))
        .map(|_| version)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Startup,
    ForcedCheck,
    ForcedInstall,
}

async fn run_update(bonsai_home: &Path, config: &UpdateConfig, kind: RunKind) -> UpdateOutcome {
    let Ok(current) = release::current_version() else {
        return UpdateOutcome::VerificationFailed;
    };
    match release::identity() {
        ReleaseIdentity::Official { .. } => {}
        ReleaseIdentity::Development => return UpdateOutcome::DevBuild,
        ReleaseIdentity::Invalid => return UpdateOutcome::VerificationFailed,
    }
    let forced = kind != RunKind::Startup;
    if !forced && config.mode == UpdateMode::Off {
        return UpdateOutcome::Disabled;
    }

    let mut state = read_state(bonsai_home);
    let exe = current_exe_canonical();
    // The disk hash is only needed when a staged update is recorded — don't
    // hash a large binary on every ordinary startup.
    let disk_sha = match &exe {
        Some(path) if state.staged_version.is_some() => release::sha256_file(path).await.ok(),
        _ => None,
    };
    let now = now_secs();
    // Forced runs bypass `mode = "off"` and the interval, never the gates.
    let preflight_mode = if forced {
        UpdateMode::Auto
    } else {
        config.mode
    };
    match preflight(&state, preflight_mode, &current, disk_sha.as_deref(), now) {
        Preflight::Disabled => return UpdateOutcome::Disabled,
        Preflight::AlreadyStaged { version } => {
            return UpdateOutcome::AlreadyStaged { version };
        }
        Preflight::TooSoon { clear_staged } => {
            if clear_staged {
                clear_staged_state(&mut state, bonsai_home);
            }
            if !forced {
                return UpdateOutcome::TooSoon;
            }
        }
        Preflight::Check { clear_staged } => {
            if clear_staged {
                clear_staged_state(&mut state, bonsai_home);
            }
        }
    }

    let Some(_lock) = UpdateLock::acquire(bonsai_home) else {
        return UpdateOutcome::Busy;
    };
    // Re-check under the lock: a concurrent session may have staged (and
    // recorded) an update between our preflight and lock acquisition.
    state = read_state(bonsai_home);
    if let Some(version) = staged_newer(&state, &current)
        && let Some(path) = &exe
        && let Ok(disk) = release::sha256_file(path).await
        && state
            .staged_binary_sha256
            .as_deref()
            .is_some_and(|staged| disk.eq_ignore_ascii_case(staged))
    {
        return UpdateOutcome::AlreadyStaged { version };
    }

    let verifier = match release::ReleaseVerifier::embedded() {
        Ok(Some(verifier)) => verifier,
        Ok(None) | Err(_) => return UpdateOutcome::VerificationFailed,
    };
    let Ok(client) = release::release_client() else {
        return UpdateOutcome::CheckFailed;
    };
    let own_install_verified = match release::verify_current_install(&client, &verifier).await {
        Ok(_) => true,
        Err(ReleaseError::Executable) => false,
        // A network failure must not consume the check interval — retry on
        // the next startup instead of going quiet for a day.
        Err(ReleaseError::Network) => return UpdateOutcome::CheckFailed,
        Err(_) => {
            record_check(&mut state, bonsai_home, now);
            return UpdateOutcome::VerificationFailed;
        }
    };
    let update = match release::find_verified_update(&client, &verifier, &current).await {
        Ok(update) => update,
        Err(ReleaseError::Network) => return UpdateOutcome::CheckFailed,
        Err(_) => {
            record_check(&mut state, bonsai_home, now);
            return UpdateOutcome::VerificationFailed;
        }
    };
    let Some(update) = update else {
        record_check(&mut state, bonsai_home, now);
        return UpdateOutcome::AlreadyCurrent;
    };
    let version = update.version.to_string();
    record_check(&mut state, bonsai_home, now);

    if !own_install_verified {
        return UpdateOutcome::NotifyOnly {
            version,
            reason: NotifyReason::SelfHashMismatch,
        };
    }
    if pin_blocks(&update.version, config.pin.as_ref()) {
        return UpdateOutcome::NotifyOnly {
            version,
            reason: NotifyReason::Pinned,
        };
    }
    if kind == RunKind::ForcedCheck
        || (kind == RunKind::Startup && config.mode == UpdateMode::Notify)
    {
        return UpdateOutcome::NotifyOnly {
            version,
            reason: NotifyReason::Notify,
        };
    }

    let Some(install_path) = exe else {
        return UpdateOutcome::NotifyOnly {
            version,
            reason: NotifyReason::NotWritable,
        };
    };
    if package_managed_prefix(&install_path, cfg!(target_os = "macos")) {
        return UpdateOutcome::NotifyOnly {
            version,
            reason: NotifyReason::Homebrew,
        };
    }

    stage_update(
        bonsai_home,
        &mut state,
        &client,
        &update,
        version,
        &install_path,
    )
    .await
}

/// Download, verify, extract, and atomically stage `update` over
/// `install_path`. `state` already carries the recorded check time; the
/// staged fields are persisted here on success.
async fn stage_update(
    bonsai_home: &Path,
    state: &mut UpdateState,
    client: &reqwest::Client,
    update: &release::VerifiedUpdate,
    version: String,
    install_path: &Path,
) -> UpdateOutcome {
    let Some(target) = release::target_triple() else {
        return UpdateOutcome::VerificationFailed;
    };
    let Some(asset) = update
        .manifest
        .assets
        .iter()
        .find(|asset| asset.target == target)
    else {
        return UpdateOutcome::VerificationFailed;
    };

    // Creating the staging file next to the executable doubles as the
    // writability probe and guarantees the final rename is same-filesystem.
    let Some(install_dir) = install_path.parent() else {
        return UpdateOutcome::NotifyOnly {
            version,
            reason: NotifyReason::NotWritable,
        };
    };
    let staging = match tempfile::Builder::new()
        .prefix(".bonsai-update-")
        .tempfile_in(install_dir)
    {
        Ok(staging) => staging,
        Err(err) => {
            tracing::debug!(error = %err, "self-update staging file creation failed");
            return UpdateOutcome::NotifyOnly {
                version,
                reason: NotifyReason::NotWritable,
            };
        }
    };

    let archive_url = match update.asset_url(&asset.archive) {
        Ok(url) => url.to_string(),
        Err(_) => return UpdateOutcome::VerificationFailed,
    };
    let archive = match release::fetch_asset(client, &archive_url, release::ARCHIVE_LIMIT).await {
        Ok(bytes) => bytes,
        Err(ReleaseError::Network) => return UpdateOutcome::CheckFailed,
        Err(_) => return UpdateOutcome::VerificationFailed,
    };
    if !sha256_hex(&archive).eq_ignore_ascii_case(&asset.archive_sha256) {
        return UpdateOutcome::VerificationFailed;
    }

    let extraction = tokio::task::spawn_blocking(move || {
        let mut staging = staging;
        let result = extract_binary_entry(&archive, staging.as_file_mut());
        (staging, result)
    })
    .await;
    let (staging, extracted_sha) = match extraction {
        Ok((staging, Ok(sha))) => (staging, sha),
        Ok((_, Err(err))) => {
            tracing::warn!(error = %err, "self-update archive extraction failed");
            return UpdateOutcome::VerificationFailed;
        }
        Err(err) => {
            tracing::warn!(error = %err, "self-update extraction task failed");
            return UpdateOutcome::VerificationFailed;
        }
    };
    if !extracted_sha.eq_ignore_ascii_case(&asset.binary_sha256) {
        return UpdateOutcome::VerificationFailed;
    }

    if let Err(err) = persist_staged(staging, install_path) {
        tracing::warn!(error = %err, "self-update binary rename failed");
        return UpdateOutcome::NotifyOnly {
            version,
            reason: NotifyReason::NotWritable,
        };
    }

    state.staged_version = Some(version.clone());
    state.staged_binary_sha256 = Some(asset.binary_sha256.clone());
    if let Err(err) = write_state(bonsai_home, state) {
        // The binary is already staged, so losing this record costs accuracy:
        // doctor can't explain the coming hash mismatch, and the recorded
        // check time would suppress a re-check for the full interval.
        // Surrender the interval (best-effort) so the next startup
        // re-discovers the situation immediately instead.
        tracing::warn!(error = %err, "failed to persist staged-update state");
        state.last_checked_at_secs = 0;
        let _ = write_state(bonsai_home, state);
    }
    tracing::info!(version = %version, "self-update staged; effective on next launch");
    UpdateOutcome::Staged { version }
}

/// A newer release exists but the configured pin holds this install back.
fn pin_blocks(candidate: &Version, pin: Option<&Version>) -> bool {
    pin.is_some_and(|pin| candidate > pin)
}

/// Canonicalized so `~/.local/bin/bonsai` symlinks resolve to the real
/// install location before we classify or replace it.
fn current_exe_canonical() -> Option<PathBuf> {
    std::env::current_exe().ok()?.canonicalize().ok()
}

/// Paths a package manager owns; replacing the binary there would fight the
/// manager's own bookkeeping. `/usr/local` is Homebrew territory only on
/// macOS — on Linux it is an ordinary prefix and the writability probe
/// decides instead.
fn package_managed_prefix(path: &Path, macos: bool) -> bool {
    path.starts_with("/opt/homebrew")
        || path.starts_with("/home/linuxbrew/.linuxbrew")
        || (macos && path.starts_with("/usr/local"))
}

// ---------------------------------------------------------------------------
// Preflight: the offline decision, factored pure for tests.

#[derive(Debug, PartialEq, Eq)]
enum Preflight {
    Disabled,
    AlreadyStaged { version: String },
    TooSoon { clear_staged: bool },
    Check { clear_staged: bool },
}

fn preflight(
    state: &UpdateState,
    mode: UpdateMode,
    current: &Version,
    disk_sha256: Option<&str>,
    now_secs: u64,
) -> Preflight {
    if mode == UpdateMode::Off {
        return Preflight::Disabled;
    }
    let clear_staged = match staged_newer(state, current) {
        Some(version) => {
            let still_staged = disk_sha256.is_some_and(|disk| {
                state
                    .staged_binary_sha256
                    .as_deref()
                    .is_some_and(|staged| disk.eq_ignore_ascii_case(staged))
            });
            if still_staged {
                return Preflight::AlreadyStaged { version };
            }
            // The user reinstalled or replaced the binary since staging.
            true
        }
        // Restarted into the staged build (or partial/garbage fields): the
        // record has served its purpose.
        None => state.staged_version.is_some() || state.staged_binary_sha256.is_some(),
    };
    if now_secs.saturating_sub(state.last_checked_at_secs) < CHECK_INTERVAL_SECS {
        Preflight::TooSoon { clear_staged }
    } else {
        Preflight::Check { clear_staged }
    }
}

/// The staged version, when the state records one strictly newer than the
/// running build with its hash present.
fn staged_newer(state: &UpdateState, current: &Version) -> Option<String> {
    let version = state.staged_version.as_ref()?;
    state.staged_binary_sha256.as_ref()?;
    let parsed = Version::parse(version).ok()?;
    (parsed > *current).then(|| version.clone())
}

// ---------------------------------------------------------------------------
// State file (`~/.bonsai/update-state.json`), read leniently.

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UpdateState {
    #[serde(default)]
    last_checked_at_secs: u64,
    #[serde(default)]
    staged_version: Option<String>,
    #[serde(default)]
    staged_binary_sha256: Option<String>,
}

fn state_path(bonsai_home: &Path) -> PathBuf {
    bonsai_home.join(STATE_FILE)
}

fn read_state(bonsai_home: &Path) -> UpdateState {
    std::fs::read_to_string(state_path(bonsai_home))
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn write_state(bonsai_home: &Path, state: &UpdateState) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::create_dir_all(bonsai_home)?;
    let json = serde_json::to_string(state).map_err(std::io::Error::other)?;
    let mut file = tempfile::NamedTempFile::new_in(bonsai_home)?;
    file.write_all(json.as_bytes())?;
    file.write_all(b"\n")?;
    file.persist(state_path(bonsai_home))
        .map_err(|err| err.error)?;
    Ok(())
}

fn record_check(state: &mut UpdateState, bonsai_home: &Path, now_secs: u64) {
    state.last_checked_at_secs = now_secs;
    if let Err(err) = write_state(bonsai_home, state) {
        tracing::debug!(error = %err, "failed to persist update state");
    }
}

fn clear_staged_state(state: &mut UpdateState, bonsai_home: &Path) {
    state.staged_version = None;
    state.staged_binary_sha256 = None;
    if let Err(err) = write_state(bonsai_home, state) {
        tracing::debug!(error = %err, "failed to persist update state");
    }
}

fn now_secs() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs(),
        Err(_) => {
            // now = 0 lands inside the check interval, so this effectively
            // pauses self-update — say so instead of failing silently.
            tracing::warn!("system clock is before the Unix epoch; self-update checks are paused");
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-session lock (`~/.bonsai/update.lock`).

/// `create_new`-based lock; the guard removes the file on drop, including
/// every early-return path. A lock older than [`LOCK_STALE_AFTER`] is treated
/// as abandoned by a crashed process and reclaimed once.
#[derive(Debug)]
struct UpdateLock {
    path: PathBuf,
}

impl UpdateLock {
    fn acquire(bonsai_home: &Path) -> Option<Self> {
        std::fs::create_dir_all(bonsai_home).ok()?;
        let path = bonsai_home.join(LOCK_FILE);
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write as _;
                    let _ = writeln!(file, "{}", std::process::id());
                    return Some(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > LOCK_STALE_AFTER);
                    if stale && attempt == 0 {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    return None;
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Archive extraction.

#[derive(Debug, Error)]
enum StageError {
    #[error("archive is not a readable tar.gz")]
    Archive,
    #[error("archive does not contain a `bonsai` entry")]
    MissingBinary,
    #[error("archive contains more than one `bonsai` entry")]
    DuplicateBinary,
    #[error("`bonsai` archive entry is not a regular file")]
    NotARegularFile,
    #[error("`bonsai` archive entry exceeds the size limit")]
    TooLarge,
    #[error("failed writing the staged binary")]
    Io,
}

/// Stream exactly the top-level regular-file `bonsai` entry out of the
/// archive into `dest`, returning its SHA-256. Every other entry (including
/// path-traversal names and links) is ignored — nothing from the archive is
/// ever extracted by its own path.
fn extract_binary_entry(archive: &[u8], dest: &mut std::fs::File) -> Result<String, StageError> {
    use std::io::{Read as _, Write as _};
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    let mut extracted: Option<String> = None;
    for entry in tar.entries().map_err(|_| StageError::Archive)? {
        let mut entry = entry.map_err(|_| StageError::Archive)?;
        if !entry_is_binary(&entry) {
            continue;
        }
        if entry.header().entry_type() != tar::EntryType::Regular {
            return Err(StageError::NotARegularFile);
        }
        if extracted.is_some() {
            return Err(StageError::DuplicateBinary);
        }
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let read = entry.read(&mut buffer).map_err(|_| StageError::Archive)?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(read as u64);
            if total > BINARY_LIMIT {
                return Err(StageError::TooLarge);
            }
            hasher.update(&buffer[..read]);
            dest.write_all(&buffer[..read])
                .map_err(|_| StageError::Io)?;
        }
        extracted = Some(hex_digest(hasher));
    }
    extracted.ok_or(StageError::MissingBinary)
}

/// True for an entry whose path is exactly `bonsai` (a leading `./` is
/// tolerated). `../bonsai`, nested paths, and unreadable names never match.
fn entry_is_binary(entry: &tar::Entry<'_, impl std::io::Read>) -> bool {
    let Ok(path) = entry.path() else {
        return false;
    };
    let mut components = path
        .components()
        .skip_while(|component| matches!(component, std::path::Component::CurDir));
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(name)), None) if name == BINARY_NAME
    )
}

/// Make the staged file executable, flush it, and atomically rename it over
/// the install path. Renaming over a running binary is safe on Unix — the
/// running image keeps its unlinked inode.
fn persist_staged(staging: tempfile::NamedTempFile, install_path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        staging
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o755))?;
    }
    staging.as_file().sync_all()?;
    staging.persist(install_path).map_err(|err| err.error)?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher)
}

fn hex_digest(hasher: Sha256) -> String {
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(last_checked: u64, staged: Option<(&str, &str)>) -> UpdateState {
        UpdateState {
            last_checked_at_secs: last_checked,
            staged_version: staged.map(|(version, _)| version.to_string()),
            staged_binary_sha256: staged.map(|(_, sha)| sha.to_string()),
        }
    }

    fn v(version: &str) -> Version {
        Version::parse(version).unwrap()
    }

    /// A "now" far past `last_checked_at_secs = 0`, so tests that don't
    /// exercise the interval always fall on the check side of it.
    const NOW: u64 = 10 * CHECK_INTERVAL_SECS;

    #[test]
    fn preflight_fresh_state_checks() {
        let result = preflight(
            &UpdateState::default(),
            UpdateMode::Auto,
            &v("0.2.0"),
            None,
            NOW,
        );
        assert_eq!(
            result,
            Preflight::Check {
                clear_staged: false
            }
        );
    }

    #[test]
    fn preflight_mode_off_disables() {
        let result = preflight(
            &UpdateState::default(),
            UpdateMode::Off,
            &v("0.2.0"),
            None,
            NOW,
        );
        assert_eq!(result, Preflight::Disabled);
    }

    #[test]
    fn preflight_within_interval_is_too_soon() {
        let state = state(1_000, None);
        let result = preflight(
            &state,
            UpdateMode::Auto,
            &v("0.2.0"),
            None,
            1_000 + CHECK_INTERVAL_SECS - 1,
        );
        assert_eq!(
            result,
            Preflight::TooSoon {
                clear_staged: false
            }
        );
        let result = preflight(
            &state,
            UpdateMode::Auto,
            &v("0.2.0"),
            None,
            1_000 + CHECK_INTERVAL_SECS,
        );
        assert_eq!(
            result,
            Preflight::Check {
                clear_staged: false
            }
        );
    }

    #[test]
    fn preflight_staged_with_matching_disk_hash_reports_already_staged() {
        let state = state(0, Some(("0.3.0", "abc123")));
        let result = preflight(&state, UpdateMode::Auto, &v("0.2.0"), Some("ABC123"), NOW);
        assert_eq!(
            result,
            Preflight::AlreadyStaged {
                version: "0.3.0".to_string()
            }
        );
    }

    #[test]
    fn preflight_staged_with_mismatched_disk_hash_clears_and_checks() {
        // The user reinstalled something else since we staged.
        let state = state(0, Some(("0.3.0", "abc123")));
        let result = preflight(&state, UpdateMode::Auto, &v("0.2.0"), Some("other"), NOW);
        assert_eq!(result, Preflight::Check { clear_staged: true });
    }

    #[test]
    fn preflight_after_restart_into_staged_build_clears_the_record() {
        // Running version equals (or exceeds) the staged one: the update
        // applied; the record must be cleared without re-notifying.
        let state = state(0, Some(("0.3.0", "abc123")));
        let result = preflight(&state, UpdateMode::Auto, &v("0.3.0"), Some("abc123"), NOW);
        assert_eq!(result, Preflight::Check { clear_staged: true });
    }

    #[test]
    fn preflight_prerelease_ordering_is_semver() {
        // 0.3.0-rc.1 staged while running 0.3.0-rc.2 is not "newer".
        let older = state(0, Some(("0.3.0-rc.1", "abc")));
        let result = preflight(&older, UpdateMode::Auto, &v("0.3.0-rc.2"), Some("abc"), NOW);
        assert_eq!(result, Preflight::Check { clear_staged: true });
        // rc.2 staged over rc.1 is.
        let newer = state(0, Some(("0.3.0-rc.2", "abc")));
        let result = preflight(&newer, UpdateMode::Auto, &v("0.3.0-rc.1"), Some("abc"), NOW);
        assert_eq!(
            result,
            Preflight::AlreadyStaged {
                version: "0.3.0-rc.2".to_string()
            }
        );
    }

    #[test]
    fn pin_gate_holds_releases_above_the_pin() {
        assert!(pin_blocks(&v("0.4.0"), Some(&v("0.3.0"))));
        assert!(!pin_blocks(&v("0.3.0"), Some(&v("0.3.0"))));
        assert!(!pin_blocks(&v("0.2.5"), Some(&v("0.3.0"))));
        assert!(!pin_blocks(&v("9.9.9"), None));
        // Prerelease ordering: an rc is below its release.
        assert!(!pin_blocks(&v("0.3.0-rc.1"), Some(&v("0.3.0"))));
    }

    #[test]
    fn package_managed_prefixes_are_classified() {
        let brew = Path::new("/opt/homebrew/bin/bonsai");
        let usr_local = Path::new("/usr/local/bin/bonsai");
        let linuxbrew = Path::new("/home/linuxbrew/.linuxbrew/bin/bonsai");
        let local = Path::new("/home/user/.local/bin/bonsai");
        assert!(package_managed_prefix(brew, true));
        assert!(package_managed_prefix(linuxbrew, false));
        assert!(package_managed_prefix(usr_local, true));
        // On Linux /usr/local is an ordinary prefix; writability decides.
        assert!(!package_managed_prefix(usr_local, false));
        assert!(!package_managed_prefix(local, true));
        assert!(!package_managed_prefix(local, false));
    }

    #[test]
    fn state_file_round_trips_and_tolerates_corruption() {
        let home = tempfile::TempDir::new().unwrap();
        let state = state(42, Some(("0.3.0", "deadbeef")));
        write_state(home.path(), &state).unwrap();
        assert_eq!(read_state(home.path()), state);

        std::fs::write(state_path(home.path()), b"{not json").unwrap();
        assert_eq!(read_state(home.path()), UpdateState::default());
        assert_eq!(
            read_state(Path::new("/nonexistent")),
            UpdateState::default()
        );
    }

    #[test]
    fn lock_is_exclusive_and_reclaims_stale_holders() {
        let home = tempfile::TempDir::new().unwrap();
        let lock = UpdateLock::acquire(home.path()).unwrap();
        assert!(UpdateLock::acquire(home.path()).is_none());
        drop(lock);
        // Released on drop: can be re-acquired.
        let lock = UpdateLock::acquire(home.path()).unwrap();
        drop(lock);

        // A stale lock (old mtime) is reclaimed.
        let lock_path = home.path().join(LOCK_FILE);
        std::fs::write(&lock_path, b"1\n").unwrap();
        let stale = std::time::SystemTime::now() - (LOCK_STALE_AFTER + Duration::from_secs(60));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap();
        file.set_modified(stale).unwrap();
        drop(file);
        assert!(UpdateLock::acquire(home.path()).is_some());
    }

    fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (name, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn dest_file() -> (tempfile::TempDir, std::fs::File) {
        let dir = tempfile::TempDir::new().unwrap();
        let file = std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(dir.path().join("staged"))
            .unwrap();
        (dir, file)
    }

    #[test]
    fn extraction_streams_the_bonsai_entry_and_hashes_it() {
        let payload = b"#!/bin/true fake binary";
        let archive = tar_gz(&[("bonsai", payload)]);
        let (_dir, mut dest) = dest_file();
        let sha = extract_binary_entry(&archive, &mut dest).unwrap();
        assert_eq!(sha, sha256_hex(payload));
        let written = std::fs::read(_dir.path().join("staged")).unwrap();
        assert_eq!(written, payload);
    }

    #[test]
    fn extraction_tolerates_leading_dot_slash_and_ignores_other_entries() {
        let payload = b"binary";
        let archive = tar_gz(&[("README.md", b"docs"), ("./bonsai", payload)]);
        let (_dir, mut dest) = dest_file();
        let sha = extract_binary_entry(&archive, &mut dest).unwrap();
        assert_eq!(sha, sha256_hex(payload));
    }

    #[test]
    fn extraction_rejects_missing_traversal_and_link_binaries() {
        let (_dir, mut dest) = dest_file();
        // Traversal name never matches, so the binary is simply missing. The
        // Builder's checked path API refuses `..`, so write the raw GNU name
        // bytes the way a hostile archive would carry them.
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let payload = b"evil";
        let mut header = tar::Header::new_gnu();
        {
            let name = b"../bonsai";
            let gnu = header.as_gnu_mut().unwrap();
            gnu.name[..name.len()].copy_from_slice(name);
        }
        header.set_entry_type(tar::EntryType::Regular);
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append(&header, &payload[..]).unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();
        assert!(matches!(
            extract_binary_entry(&archive, &mut dest),
            Err(StageError::MissingBinary)
        ));

        // A symlink entry named `bonsai` is rejected outright.
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder
            .append_link(&mut header, "bonsai", "/bin/true")
            .unwrap();
        let archive = builder.into_inner().unwrap().finish().unwrap();
        let (_dir, mut dest) = dest_file();
        assert!(matches!(
            extract_binary_entry(&archive, &mut dest),
            Err(StageError::NotARegularFile)
        ));
    }

    #[test]
    fn extraction_rejects_duplicate_binaries() {
        let archive = tar_gz(&[("bonsai", b"one"), ("./bonsai", b"two")]);
        let (_dir, mut dest) = dest_file();
        assert!(matches!(
            extract_binary_entry(&archive, &mut dest),
            Err(StageError::DuplicateBinary)
        ));
    }

    #[test]
    fn startup_notices_cover_every_reason() {
        let staged = UpdateOutcome::Staged {
            version: "0.3.0".to_string(),
        };
        assert_eq!(
            startup_notice(&staged).unwrap(),
            "updated to v0.3.0 — restart to apply"
        );
        for (reason, needle) in [
            (NotifyReason::Notify, "bonsai update"),
            (NotifyReason::Homebrew, "brew upgrade"),
            (NotifyReason::NotWritable, "installer"),
            (NotifyReason::SelfHashMismatch, "install.sh"),
            (NotifyReason::Pinned, "update.pin"),
        ] {
            let outcome = UpdateOutcome::NotifyOnly {
                version: "0.3.0".to_string(),
                reason,
            };
            let notice = startup_notice(&outcome).unwrap();
            assert!(notice.contains(needle), "{reason:?}: {notice}");
            assert!(notice.contains("v0.3.0"), "{reason:?}: {notice}");
        }
        assert!(startup_notice(&UpdateOutcome::TooSoon).is_none());
        assert!(startup_notice(&UpdateOutcome::CheckFailed).is_none());
    }
}
