//! Provider credential provenance and protected credential-store access.
//!
//! Runtime provider sessions hold the resolved secret so transports do not
//! need an async lookup on every turn. Persistence stores only a
//! [`CredentialSource`] reference; the secret stays in an environment
//! variable, the Codex auth cache, a protected file, the OS credential store,
//! or process memory.

use std::fmt;
use std::path::Path;
#[cfg(not(test))]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

mod file;

use file::FileCredentialBackend;

const KEYRING_SERVICE: &str = "dev.bonsai.provider";
const KEYRING_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a runtime provider secret came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reference", rename_all = "snake_case")]
pub enum CredentialSource {
    /// No credential is configured.
    #[default]
    None,
    /// Resolve the named environment variable on each process start.
    Environment(String),
    /// Resolve the provider id from the operating system credential store.
    Keyring,
    /// Resolve the provider id from Bonsai's owner-only credential directory.
    File,
    /// Re-import the external Codex CLI auth cache on each process start.
    CodexCache,
    /// Keep the secret in this process only; it intentionally cannot resume.
    Session,
}

impl CredentialSource {
    pub(crate) fn as_db_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Environment(_) => "environment",
            Self::Keyring => "keyring",
            Self::File => "file",
            Self::CodexCache => "codex_cache",
            Self::Session => "session",
        }
    }

    pub(crate) fn reference(&self) -> &str {
        match self {
            Self::Environment(variable) => variable,
            Self::None | Self::Keyring | Self::File | Self::CodexCache | Self::Session => "",
        }
    }

    pub(crate) fn from_db(source: &str, reference: String) -> Self {
        match source {
            "environment" if !reference.trim().is_empty() => Self::Environment(reference),
            "keyring" => Self::Keyring,
            "file" => Self::File,
            "codex_cache" => Self::CodexCache,
            "session" => Self::Session,
            _ => Self::None,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Environment(_) => "environment",
            Self::Keyring => "OS credential store",
            Self::File => "protected file",
            Self::CodexCache => "Codex auth cache",
            Self::Session => "session only",
        }
    }
}

/// User choice for a newly entered credential.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialPersistence {
    /// Persist in Bonsai's owner-only credential directory.
    #[default]
    File,
    /// Persist in the native OS credential store.
    Keyring,
    /// Never persist the secret beyond the current process.
    Session,
}

impl CredentialPersistence {
    pub const ALL: [Self; 3] = [Self::File, Self::Keyring, Self::Session];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Keyring => "keyring",
            Self::Session => "session",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::File => "protected file",
            Self::Keyring => "OS credential store",
            Self::Session => "this session only",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "file" => Some(Self::File),
            "keyring" => Some(Self::Keyring),
            "session" => Some(Self::Session),
            _ => None,
        }
    }

    pub(crate) const fn source(self) -> CredentialSource {
        match self {
            Self::File => CredentialSource::File,
            Self::Keyring => CredentialSource::Keyring,
            Self::Session => CredentialSource::Session,
        }
    }

    pub(crate) fn cycled(self) -> Self {
        match self {
            Self::File => Self::Keyring,
            Self::Keyring => Self::Session,
            Self::Session => Self::File,
        }
    }

    pub(crate) fn cycled_back(self) -> Self {
        match self {
            Self::Keyring => Self::File,
            Self::Session => Self::Keyring,
            Self::File => Self::Session,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialDeleteOutcome {
    /// A stored credential existed and was deleted.
    Deleted,
    /// No credential existed; the desired cleared state already held.
    NotFound,
}

/// Per-backend result of a complete provider credential cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CredentialRemovalReport {
    /// Protected-file backend result.
    pub file: CredentialDeleteOutcome,
    /// Native OS credential-store result.
    pub keyring: CredentialDeleteOutcome,
}

trait CredentialBackend: Send + Sync {
    fn set(&self, provider_id: &str, secret: &str) -> Result<()>;
    fn get(&self, provider_id: &str) -> Result<Option<String>>;
    fn delete(&self, provider_id: &str) -> Result<CredentialDeleteOutcome>;
}

#[derive(Clone)]
pub(crate) struct CredentialStore {
    file: Arc<dyn CredentialBackend>,
    keyring: Arc<dyn CredentialBackend>,
    keyring_read_active: Arc<AtomicBool>,
    keyring_read_timeout: Duration,
}

/// Previous contents of the credential stores touched by an authorization.
/// This contains secrets and must not be logged or formatted.
pub(crate) struct CredentialBackup {
    entries: Vec<(CredentialSource, Option<String>)>,
    pub(crate) effective_source: CredentialSource,
    pub(crate) preserve_previous: bool,
    pub(crate) warnings: Vec<String>,
}

/// Held by the native read thread until it finishes, including after timeout.
struct KeyringReadGuard(Arc<AtomicBool>);

impl Drop for KeyringReadGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl super::SessionStore {
    pub(crate) async fn backup_provider_credentials(
        &self,
        provider_id: &str,
        requested_source: &CredentialSource,
    ) -> Result<CredentialBackup> {
        let previous_source = self
            .providers
            .get(provider_id)
            .map(|s| &s.credential_source);
        let mut sources = Vec::new();
        for source in previous_source
            .into_iter()
            .chain(std::iter::once(requested_source))
        {
            if matches!(source, CredentialSource::File | CredentialSource::Keyring)
                && !sources.contains(source)
            {
                sources.push(source.clone());
            }
        }
        let mut backup = Vec::new();
        let mut effective_source = requested_source.clone();
        let mut preserve_previous = false;
        let mut warnings = Vec::new();
        for source in sources {
            match self.credential_store.get(&source, provider_id).await {
                Ok(secret) => backup.push((source, secret)),
                Err(error) => {
                    if previous_source == Some(&source) {
                        preserve_previous = true;
                        warnings.push(format!("The previous {} credential could not be accessed and may remain stored: {error:#}", source.label()));
                    }
                    if *requested_source == source {
                        effective_source = CredentialSource::Session;
                        warnings.push(format!("The {} credential store is unavailable; the new credential is available only for this session: {error:#}", source.label()));
                    }
                }
            }
        }
        Ok(CredentialBackup {
            entries: backup,
            effective_source,
            preserve_previous,
            warnings,
        })
    }

    pub(crate) async fn restore_provider_credentials(
        &self,
        provider_id: &str,
        backup: CredentialBackup,
    ) -> Result<()> {
        let mut errors = Vec::new();
        for (source, secret) in backup.entries {
            let result = match secret {
                Some(secret) => {
                    self.credential_store
                        .set(&source, provider_id, &secret)
                        .await
                }
                None => self
                    .credential_store
                    .delete(&source, provider_id)
                    .await
                    .map(|_| ()),
            };
            if let Err(error) = result {
                errors.push(format!("{}: {error:#}", source.label()));
            }
        }
        anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(())
    }

    /// Resolve persisted references and migrate legacy plaintext values into
    /// the OS credential store when possible. Returns whether plaintext was
    /// observed and therefore requires an immediate database scrub.
    pub(super) async fn resolve_persisted_credentials(&mut self) -> bool {
        let provider_ids = self.providers.keys().cloned().collect::<Vec<_>>();
        let mut migrated_plaintext = false;

        for provider_id in provider_ids {
            let legacy_plaintext = self.providers.get(&provider_id).is_some_and(|session| {
                matches!(session.credential_source, CredentialSource::None)
                    && !session.api_key.trim().is_empty()
            });
            if legacy_plaintext {
                migrated_plaintext = true;
                let is_codex_cache = crate::provider::metadata_for(provider_id.as_str())
                    .is_some_and(|metadata| metadata.auth_requirement.uses_codex_cache());
                if is_codex_cache {
                    if let Some(session) = self.providers.get_mut(&provider_id) {
                        session.credential_source = CredentialSource::CodexCache;
                    }
                } else {
                    let secret = self.providers[&provider_id].api_key.clone();
                    match self
                        .credential_store
                        .set(&CredentialSource::File, provider_id.as_str(), &secret)
                        .await
                    {
                        Ok(()) => {
                            if let Some(session) = self.providers.get_mut(&provider_id) {
                                session.credential_source = CredentialSource::File;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                provider = %provider_id,
                                %error,
                                "legacy plaintext credential is session-only because protected file storage is unavailable; reauthorize to persist it"
                            );
                            if let Some(session) = self.providers.get_mut(&provider_id) {
                                session.credential_source = CredentialSource::Session;
                            }
                        }
                    }
                }
            }

            let source = self.providers[&provider_id].credential_source.clone();
            match source {
                CredentialSource::None => {
                    if let Some(session) = self.providers.get_mut(&provider_id) {
                        session.api_key.clear();
                    }
                }
                CredentialSource::Environment(variable) => {
                    let secret = std::env::var(&variable)
                        .ok()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_default();
                    if let Some(session) = self.providers.get_mut(&provider_id) {
                        session.api_key = secret;
                    }
                }
                CredentialSource::File | CredentialSource::Keyring => {
                    match self
                        .credential_store
                        .get(&source, provider_id.as_str())
                        .await
                    {
                        Ok(Some(secret)) => {
                            if let Some(session) = self.providers.get_mut(&provider_id) {
                                session.api_key = secret;
                            }
                        }
                        Ok(None) => {
                            tracing::warn!(
                                provider = %provider_id,
                                source = source.label(),
                                "credential reference has no matching stored secret; reauthorize the provider"
                            );
                            if let Some(session) = self.providers.get_mut(&provider_id) {
                                session.api_key.clear();
                                session.credential_source = CredentialSource::None;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                provider = %provider_id,
                                %error,
                                source = source.label(),
                                "failed to resolve stored credential; provider remains unauthorized"
                            );
                            if let Some(session) = self.providers.get_mut(&provider_id) {
                                session.api_key.clear();
                            }
                        }
                    }
                }
                CredentialSource::CodexCache => {
                    match crate::provider::codex_cached_authorization().await {
                        Ok(Some(authorization)) => {
                            if let Some(session) = self.providers.get_mut(&provider_id) {
                                session.api_key = authorization.api_key;
                                session.account_id = authorization.account_id;
                                session.is_fedramp_account = authorization.is_fedramp;
                            }
                        }
                        Ok(None) => {
                            tracing::warn!(
                                provider = %provider_id,
                                "Codex auth-cache reference could not be resolved; run `codex login`"
                            );
                            if let Some(session) = self.providers.get_mut(&provider_id) {
                                session.api_key.clear();
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                provider = %provider_id,
                                %error,
                                "failed to refresh Codex auth-cache reference"
                            );
                            if let Some(session) = self.providers.get_mut(&provider_id) {
                                session.api_key.clear();
                            }
                        }
                    }
                }
                CredentialSource::Session => {
                    // A process-only credential cannot survive a restart.
                    if let Some(session) = self.providers.get_mut(&provider_id) {
                        session.api_key.clear();
                        session.credential_source = CredentialSource::None;
                    }
                }
            }
        }

        migrated_plaintext
    }

    pub(crate) async fn set_provider_credential(
        &mut self,
        provider_id: &str,
        secret: String,
        requested_source: CredentialSource,
    ) -> CredentialSource {
        self.ensure_provider(provider_id);
        let previous_source = self.providers[provider_id].credential_source.clone();
        let resolved_source = if secret.trim().is_empty() {
            CredentialSource::None
        } else if matches!(
            requested_source,
            CredentialSource::File | CredentialSource::Keyring
        ) {
            match self
                .credential_store
                .set(&requested_source, provider_id, &secret)
                .await
            {
                Ok(()) => requested_source.clone(),
                Err(error) => {
                    tracing::warn!(
                        provider = %provider_id,
                        %error,
                        source = requested_source.label(),
                        "credential store is unavailable; authorization is session-only"
                    );
                    CredentialSource::Session
                }
            }
        } else {
            requested_source
        };

        if previous_source != resolved_source
            && matches!(
                previous_source,
                CredentialSource::File | CredentialSource::Keyring
            )
            && let Err(error) = self
                .credential_store
                .delete(&previous_source, provider_id)
                .await
        {
            tracing::warn!(provider = %provider_id, %error, "failed to remove superseded credential");
        }

        let session = self.session_mut(provider_id);
        session.api_key = secret;
        session.credential_source = resolved_source.clone();
        resolved_source
    }

    pub(crate) async fn clear_provider_credential(
        &mut self,
        provider_id: &str,
    ) -> Result<CredentialRemovalReport> {
        // Only attempt the backend that matches the provider's current
        // credential source. Checking a backend the provider never used
        // (especially the OS keyring) can fail hard when the platform
        // credential service is unavailable, even for a no-op lookup.
        let current_source = self
            .providers
            .get(provider_id)
            .map(|session| session.credential_source.clone())
            .unwrap_or(CredentialSource::None);
        let need_file = matches!(current_source, CredentialSource::File);
        let need_keyring = matches!(current_source, CredentialSource::Keyring);

        let file = if need_file {
            self.credential_store
                .delete(&CredentialSource::File, provider_id)
                .await
        } else {
            Ok(CredentialDeleteOutcome::NotFound)
        };
        let keyring = if need_keyring {
            self.credential_store
                .delete(&CredentialSource::Keyring, provider_id)
                .await
        } else {
            Ok(CredentialDeleteOutcome::NotFound)
        };

        let mut failures = Vec::new();
        if let Err(error) = &file {
            failures.push(format!("protected file: {error:#}"));
        }
        if let Err(error) = &keyring {
            failures.push(format!("OS credential store: {error:#}"));
        }
        if !failures.is_empty() {
            anyhow::bail!(
                "Failed to remove stored credentials for {provider_id}: {}",
                failures.join("; ")
            );
        }
        if let Some(session) = self.providers.get_mut(provider_id) {
            session.api_key.clear();
            session.credential_source = CredentialSource::None;
        }
        Ok(CredentialRemovalReport {
            file: file?,
            keyring: keyring?,
        })
    }

    pub(super) fn clear_runtime_secrets_for_persistence(&mut self) {
        for session in self.providers.values_mut() {
            session.api_key.clear();
        }
    }
}

impl fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialStore")
            .finish_non_exhaustive()
    }
}

impl CredentialStore {
    pub(crate) fn with_home(bonsai_home: &Path) -> Self {
        Self {
            file: Arc::new(FileCredentialBackend::new(bonsai_home)),
            keyring: if Self::native_access_disabled() {
                Arc::new(DisabledCredentialBackend)
            } else {
                native_credential_backend()
            },
            keyring_read_active: Arc::new(AtomicBool::new(false)),
            keyring_read_timeout: KEYRING_READ_TIMEOUT,
        }
    }

    /// Whether verifier isolation forbids all native credential entry access.
    pub(crate) fn native_access_disabled() -> bool {
        std::env::var_os("BONSAI_DISABLE_KEYRING").as_deref() == Some(std::ffi::OsStr::new("1"))
    }

    fn backend(&self, source: &CredentialSource) -> Result<Arc<dyn CredentialBackend>> {
        match source {
            CredentialSource::File => Ok(self.file.clone()),
            CredentialSource::Keyring => Ok(self.keyring.clone()),
            _ => anyhow::bail!("{} is not a persistent credential source", source.label()),
        }
    }

    pub(crate) async fn set(
        &self,
        source: &CredentialSource,
        provider_id: &str,
        secret: &str,
    ) -> Result<()> {
        let backend = self.backend(source)?;
        let provider_id = provider_id.to_string();
        let secret = secret.to_string();
        tokio::task::spawn_blocking(move || backend.set(&provider_id, &secret))
            .await
            .context("credential-store task failed")?
    }

    pub(crate) async fn get(
        &self,
        source: &CredentialSource,
        provider_id: &str,
    ) -> Result<Option<String>> {
        let backend = self.backend(source)?;
        let provider_id = provider_id.to_string();
        if matches!(source, CredentialSource::Keyring) {
            anyhow::ensure!(
                self.keyring_read_active
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok(),
                "An OS credential store read is still pending; unlock the store and retry"
            );
            let guard = KeyringReadGuard(self.keyring_read_active.clone());
            let (sender, receiver) = tokio::sync::oneshot::channel();
            // A native keyring may block indefinitely. A detached OS thread
            // avoids keeping Tokio's blocking pool alive during shutdown.
            std::thread::Builder::new()
                .name("bonsai-keyring-read".into())
                .spawn(move || {
                    let result = backend.get(&provider_id);
                    drop(guard);
                    let _ = sender.send(result);
                })
                .context("failed to start OS credential store read")?;
            return match tokio::time::timeout(self.keyring_read_timeout, receiver).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => anyhow::bail!("OS credential store read failed unexpectedly"),
                Err(_) => anyhow::bail!(
                    "OS credential store did not respond; unlock it and retry. The stored credential was not removed"
                ),
            };
        }
        tokio::task::spawn_blocking(move || backend.get(&provider_id))
            .await
            .context("credential-store task failed")?
    }

    pub(crate) async fn delete(
        &self,
        source: &CredentialSource,
        provider_id: &str,
    ) -> Result<CredentialDeleteOutcome> {
        let backend = self.backend(source)?;
        let provider_id = provider_id.to_string();
        tokio::task::spawn_blocking(move || backend.delete(&provider_id))
            .await
            .context("credential-store task failed")?
    }

    #[cfg(test)]
    pub(crate) fn memory() -> Self {
        Self {
            file: Arc::new(MemoryCredentialBackend::default()),
            keyring: Arc::new(MemoryCredentialBackend::default()),
            keyring_read_active: Arc::new(AtomicBool::new(false)),
            keyring_read_timeout: KEYRING_READ_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) fn memory_with_keyring_delete_failure() -> Self {
        Self {
            file: Arc::new(MemoryCredentialBackend::default()),
            keyring: Arc::new(MemoryCredentialBackend::failing_delete()),
            keyring_read_active: Arc::new(AtomicBool::new(false)),
            keyring_read_timeout: KEYRING_READ_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) fn memory_with_keyring_disabled() -> Self {
        Self {
            file: Arc::new(MemoryCredentialBackend::default()),
            keyring: Arc::new(DisabledCredentialBackend),
            keyring_read_active: Arc::new(AtomicBool::new(false)),
            keyring_read_timeout: KEYRING_READ_TIMEOUT,
        }
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        #[cfg(test)]
        {
            Self::memory()
        }
        #[cfg(not(test))]
        {
            let home = crate::storage::BonsaiPaths::discover()
                .map(|paths| paths.home_dir().to_path_buf())
                .unwrap_or_else(|_| PathBuf::from(".bonsai"));
            Self::with_home(&home)
        }
    }
}

/// Fail closed before constructing or contacting a native credential entry.
#[derive(Debug)]
struct DisabledCredentialBackend;

impl CredentialBackend for DisabledCredentialBackend {
    fn set(&self, _provider_id: &str, _secret: &str) -> Result<()> {
        anyhow::bail!("OS credential store access is disabled")
    }

    fn get(&self, _provider_id: &str) -> Result<Option<String>> {
        anyhow::bail!("OS credential store access is disabled")
    }

    fn delete(&self, _provider_id: &str) -> Result<CredentialDeleteOutcome> {
        anyhow::bail!("OS credential store access is disabled")
    }
}

#[derive(Debug)]
struct NativeCredentialBackend;

#[cfg(not(test))]
fn native_credential_backend() -> Arc<dyn CredentialBackend> {
    Arc::new(NativeCredentialBackend)
}

#[cfg(test)]
fn native_credential_backend() -> Arc<dyn CredentialBackend> {
    Arc::new(MemoryCredentialBackend::default())
}

impl NativeCredentialBackend {
    fn entry(provider_id: &str) -> Result<keyring::Entry> {
        if CredentialStore::native_access_disabled() {
            anyhow::bail!("OS credential store access is disabled");
        }
        keyring::Entry::new(KEYRING_SERVICE, provider_id)
            .with_context(|| format!("OS credential store is unavailable for {provider_id}"))
    }
}

impl CredentialBackend for NativeCredentialBackend {
    fn set(&self, provider_id: &str, secret: &str) -> Result<()> {
        Self::entry(provider_id)?
            .set_password(secret)
            .with_context(|| format!("Failed to store credentials for {provider_id}"))
    }

    fn get(&self, provider_id: &str) -> Result<Option<String>> {
        match Self::entry(provider_id)?.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("Failed to load credentials for {provider_id}"))
            }
        }
    }

    fn delete(&self, provider_id: &str) -> Result<CredentialDeleteOutcome> {
        match Self::entry(provider_id)?.delete_credential() {
            Ok(()) => Ok(CredentialDeleteOutcome::Deleted),
            Err(keyring::Error::NoEntry) => Ok(CredentialDeleteOutcome::NotFound),
            Err(error) => Err(error)
                .with_context(|| format!("Failed to delete credentials for {provider_id}")),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct MemoryCredentialBackend {
    secrets: Mutex<HashMap<String, String>>,
    delete_failures_remaining: Mutex<usize>,
}

#[cfg(test)]
impl MemoryCredentialBackend {
    fn failing_delete() -> Self {
        Self {
            secrets: Mutex::new(HashMap::new()),
            delete_failures_remaining: Mutex::new(1),
        }
    }
}

#[cfg(test)]
impl CredentialBackend for MemoryCredentialBackend {
    fn set(&self, provider_id: &str, secret: &str) -> Result<()> {
        self.secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(provider_id.to_string(), secret.to_string());
        Ok(())
    }

    fn get(&self, provider_id: &str) -> Result<Option<String>> {
        Ok(self
            .secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(provider_id)
            .cloned())
    }

    fn delete(&self, provider_id: &str) -> Result<CredentialDeleteOutcome> {
        let mut failures = self
            .delete_failures_remaining
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *failures > 0 {
            *failures -= 1;
            anyhow::bail!("injected credential delete failure for {provider_id}");
        }
        drop(failures);
        let removed = self
            .secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(provider_id);
        Ok(if removed.is_some() {
            CredentialDeleteOutcome::Deleted
        } else {
            CredentialDeleteOutcome::NotFound
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStore;

    type ReleaseGate = Arc<(Mutex<bool>, std::sync::Condvar)>;
    type CallCount = Arc<std::sync::atomic::AtomicUsize>;

    struct BlockingReadBackend {
        released: ReleaseGate,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CredentialBackend for BlockingReadBackend {
        fn set(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn get(&self, _: &str) -> Result<Option<String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (lock, wake) = &*self.released;
            let released = lock.lock().unwrap();
            drop(wake.wait_while(released, |released| !*released).unwrap());
            Ok(Some("synthetic-secret".into()))
        }
        fn delete(&self, _: &str) -> Result<CredentialDeleteOutcome> {
            Ok(CredentialDeleteOutcome::NotFound)
        }
    }

    fn blocked_store() -> (CredentialStore, ReleaseGate, CallCount) {
        let released = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store = CredentialStore {
            file: Arc::new(MemoryCredentialBackend::default()),
            keyring: Arc::new(BlockingReadBackend {
                released: released.clone(),
                calls: calls.clone(),
            }),
            keyring_read_active: Arc::new(AtomicBool::new(false)),
            keyring_read_timeout: Duration::from_millis(50),
        };
        (store, released, calls)
    }

    fn release_backend(released: &ReleaseGate) {
        let (lock, wake) = &**released;
        *lock.lock().unwrap() = true;
        wake.notify_all();
    }

    #[tokio::test]
    async fn blocked_keyring_read_is_bounded_and_preserves_credential_reference() {
        let (store, released, calls) = blocked_store();
        let mut session = SessionStore {
            credential_store: store.clone(),
            ..SessionStore::default()
        };
        session.ensure_provider("anthropic");
        session.session_mut("anthropic").credential_source = CredentialSource::Keyring;
        tokio::time::timeout(
            Duration::from_secs(2),
            session.resolve_persisted_credentials(),
        )
        .await
        .unwrap();
        assert_eq!(
            session.session("anthropic").credential_source,
            CredentialSource::Keyring
        );
        assert!(session.session("anthropic").api_key.is_empty());
        let error = store
            .get(&CredentialSource::Keyring, "other")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("still pending"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        release_backend(&released);
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.keyring_read_active.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            store
                .get(&CredentialSource::Keyring, "anthropic")
                .await
                .unwrap()
                .as_deref(),
            Some("synthetic-secret")
        );
    }

    #[test]
    fn blocked_keyring_read_does_not_hold_tokio_runtime_shutdown() {
        let (store, released, _) = blocked_store();
        let (finished, received) = std::sync::mpsc::channel();
        let runtime_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(store.get(&CredentialSource::Keyring, "anthropic"));
            assert!(result.unwrap_err().to_string().contains("did not respond"));
            drop(runtime);
            finished.send(()).unwrap();
        });
        let result = received.recv_timeout(Duration::from_secs(5));
        release_backend(&released);
        runtime_thread.join().unwrap();
        assert!(result.is_ok(), "keyring read held runtime shutdown");
    }

    #[tokio::test]
    async fn isolated_verifier_disables_all_native_credential_operations() {
        crate::util::test_env::with_var_async("BONSAI_DISABLE_KEYRING", Some("1"), async {
            let home = tempfile::tempdir().unwrap();
            let store = CredentialStore::with_home(home.path());
            // Exercise the production backend too, not the test memory stand-in.
            let native = NativeCredentialBackend;
            assert!(native.set("test", "synthetic").is_err());
            assert!(native.get("test").is_err());
            assert!(native.delete("test").is_err());
            assert!(
                store
                    .set(&CredentialSource::Keyring, "test", "synthetic")
                    .await
                    .is_err()
            );
            assert!(store.get(&CredentialSource::Keyring, "test").await.is_err());
            assert!(
                store
                    .delete(&CredentialSource::Keyring, "test")
                    .await
                    .is_err()
            );
            store
                .set(&CredentialSource::File, "test", "synthetic")
                .await
                .unwrap();
            assert_eq!(
                store
                    .get(&CredentialSource::File, "test")
                    .await
                    .unwrap()
                    .as_deref(),
                Some("synthetic")
            );
            assert_eq!(
                store.delete(&CredentialSource::File, "test").await.unwrap(),
                CredentialDeleteOutcome::Deleted
            );
            assert!(
                store
                    .get(&CredentialSource::File, "test")
                    .await
                    .unwrap()
                    .is_none()
            );
        })
        .await;
    }

    #[tokio::test]
    async fn memory_store_round_trips_and_deletes_without_exposing_debug_data() {
        let store = CredentialStore::memory();
        store
            .set(&CredentialSource::Keyring, "anthropic", "sk-ant-test")
            .await
            .unwrap();
        assert_eq!(
            store
                .get(&CredentialSource::Keyring, "anthropic")
                .await
                .unwrap()
                .as_deref(),
            Some("sk-ant-test")
        );
        assert!(!format!("{store:?}").contains("sk-ant-test"));

        store
            .delete(&CredentialSource::Keyring, "anthropic")
            .await
            .unwrap();
        assert!(
            store
                .get(&CredentialSource::Keyring, "anthropic")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn credential_removal_failure_is_visible_unchanged_and_retryable() {
        let credentials = CredentialStore::memory_with_keyring_delete_failure();
        credentials
            .set(&CredentialSource::Keyring, "anthropic", "readable-secret")
            .await
            .unwrap();
        let mut session_store = SessionStore {
            credential_store: credentials.clone(),
            ..SessionStore::default()
        };
        session_store.ensure_provider("anthropic");
        let session = session_store.session_mut("anthropic");
        session.api_key = "readable-secret".to_string();
        session.credential_source = CredentialSource::Keyring;

        let error = session_store
            .clear_provider_credential("anthropic")
            .await
            .expect_err("the first keyring delete is injected to fail");
        assert!(error.to_string().contains("OS credential store"));
        assert_eq!(
            session_store.session("anthropic").api_key,
            "readable-secret"
        );
        assert_eq!(
            session_store.session("anthropic").credential_source,
            CredentialSource::Keyring
        );
        assert_eq!(
            credentials
                .get(&CredentialSource::Keyring, "anthropic")
                .await
                .unwrap()
                .as_deref(),
            Some("readable-secret")
        );

        let removed = session_store
            .clear_provider_credential("anthropic")
            .await
            .unwrap();
        assert_eq!(removed.file, CredentialDeleteOutcome::NotFound);
        assert_eq!(removed.keyring, CredentialDeleteOutcome::Deleted);
        assert!(session_store.session("anthropic").api_key.is_empty());
        assert_eq!(
            session_store.session("anthropic").credential_source,
            CredentialSource::None
        );

        let repeated = session_store
            .clear_provider_credential("anthropic")
            .await
            .unwrap();
        assert_eq!(repeated.file, CredentialDeleteOutcome::NotFound);
        assert_eq!(repeated.keyring, CredentialDeleteOutcome::NotFound);
    }

    #[test]
    fn database_encoding_contains_only_references() {
        let source = CredentialSource::Environment("ANTHROPIC_API_KEY".to_string());
        assert_eq!(source.as_db_str(), "environment");
        assert_eq!(source.reference(), "ANTHROPIC_API_KEY");
        assert_eq!(
            CredentialSource::from_db(source.as_db_str(), source.reference().to_string()),
            source
        );
        assert_eq!(
            CredentialSource::from_db("file", String::new()),
            CredentialSource::File
        );
    }
}
