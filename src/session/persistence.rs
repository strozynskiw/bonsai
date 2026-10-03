use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::{BaseDirs, ProjectDirs};
use serde::Deserialize;
use tracing::{info, warn};

use crate::model_catalog::{ConnectionId, ModelCatalog, ModelId};
use crate::util::time::now_ms;

use super::*;

const BONSAI_HOME_DIR: &str = ".bonsai";
const SESSION_FILE: &str = "sessions.toml";
const LEGACY_CONFIG_FILE: &str = "config.toml";

#[derive(Debug, Clone, Deserialize)]
struct LegacyConfig {
    api_key: String,
    model: String,
    #[serde(default = "default_legacy_base_url")]
    base_url: String,
}

fn default_legacy_base_url() -> String {
    "https://opencode.ai/zen/go/v1".to_string()
}

/// Whether `path` is a `.bonsai/config.toml` written by the layered-config
/// schema (`src/config/`) rather than the legacy `{api_key, model,
/// base_url}` shape this module expects. Both live at the same
/// `$BONSAI_HOME/config.toml` path, and the layered schema has no `api_key`/
/// `model` keys at all — without this check, a fresh install with only a
/// new-schema global config would fail `toml::from_str::<LegacyConfig>` and
/// break startup. `schema_version` is the layered schema's unambiguous marker
/// (mirrors the opposite-direction check in `crate::config`, which treats a
/// file with `api_key`/`model` and no `schema_version` as legacy).
fn is_layered_config_schema(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| content.parse::<toml::Table>().ok())
        .is_some_and(|table| table.contains_key("schema_version"))
}

impl ProviderSession {
    fn preserve_auth_from(&mut self, existing: &Self) -> bool {
        let mut changed = false;
        if self.api_key.trim().is_empty() && !existing.api_key.trim().is_empty() {
            // Legacy plaintext is copied only into runtime memory so the async
            // migration can move it into the OS store. Serialization always
            // omits this field.
            self.api_key = existing.api_key.clone();
            changed = true;
        }
        if matches!(self.credential_source, CredentialSource::None)
            && !matches!(existing.credential_source, CredentialSource::None)
        {
            self.credential_source = existing.credential_source.clone();
            changed = true;
        }
        if self.account_id.trim().is_empty() && !existing.account_id.trim().is_empty() {
            self.account_id = existing.account_id.clone();
            changed = true;
        }
        if !self.is_fedramp_account && existing.is_fedramp_account {
            self.is_fedramp_account = true;
            changed = true;
        }
        if self.authorized_at.is_none() && existing.authorized_at.is_some() {
            self.authorized_at = existing.authorized_at;
            changed = true;
        }
        changed
    }
}

impl SessionStore {
    #[cfg(test)]
    pub async fn load_with_storage(storage: &crate::storage::Storage) -> Result<Self> {
        Self::load_with_storage_and_catalog(storage, None).await
    }

    #[cfg(test)]
    pub(crate) async fn load_with_storage_and_credential_store(
        storage: &crate::storage::Storage,
        credential_store: CredentialStore,
    ) -> Result<Self> {
        Self::load_with_storage_catalog_and_credentials(storage, None, credential_store).await
    }

    pub(crate) async fn load_with_storage_and_catalog(
        storage: &crate::storage::Storage,
        catalog: Option<&ModelCatalog>,
    ) -> Result<Self> {
        Self::load_with_storage_catalog_and_credentials(
            storage,
            catalog,
            CredentialStore::with_home(storage.home_dir()),
        )
        .await
    }

    async fn load_with_storage_catalog_and_credentials(
        storage: &crate::storage::Storage,
        catalog: Option<&ModelCatalog>,
        credential_store: CredentialStore,
    ) -> Result<Self> {
        if let Some(mut store) = storage.load_session_store_raw().await? {
            store.credential_store = credential_store.clone();
            let migrated_plaintext = store.resolve_persisted_credentials().await;
            store.apply_env_overrides_with_catalog(catalog);
            store.normalize_sessions_with_catalog(catalog);
            if migrated_plaintext {
                storage
                    .save_session_store_with_auth_policy(&store, SaveAuthPolicy::AllowClear)
                    .await?;
                Self::scrub_bonsai_owned_legacy_credentials()?;
                warn!(
                    "migrated plaintext provider credentials from legacy files; external backups and copies cannot be scrubbed automatically"
                );
            }
            store.persistence = SessionPersistence::Sqlite(storage.clone());
            info!(
                path = %storage.db_path().display(),
                current_provider = %store.current_provider,
                providers = store.providers.len(),
                provider_state = %store.provider_state_summary(),
                "session store loaded from SQLite"
            );
            return Ok(store);
        }

        let mut store = Self::load_with_catalog(catalog)?;
        store.credential_store = credential_store;
        let migrated_plaintext = store.resolve_persisted_credentials().await;
        storage
            .save_session_store_with_auth_policy(&store, SaveAuthPolicy::AllowClear)
            .await?;
        if migrated_plaintext {
            Self::scrub_bonsai_owned_legacy_credentials()?;
            warn!(
                "migrated plaintext provider credentials from legacy files; external backups and copies cannot be scrubbed automatically"
            );
        }
        store.persistence = SessionPersistence::Sqlite(storage.clone());
        info!(
            path = %storage.db_path().display(),
            current_provider = %store.current_provider,
            providers = store.providers.len(),
            provider_state = %store.provider_state_summary(),
            "migrated session store to SQLite"
        );
        Ok(store)
    }

    fn load_with_catalog(catalog: Option<&ModelCatalog>) -> Result<Self> {
        let path = Self::path()?;
        let legacy_session_paths = vec![Self::legacy_session_path()?];
        let legacy_config_paths = Self::legacy_config_paths()?;
        Self::load_from_paths_with_catalog(
            &path,
            &legacy_session_paths,
            &legacy_config_paths,
            catalog,
        )
    }

    #[cfg(test)]
    fn load_from_paths(
        path: &Path,
        legacy_session_paths: &[PathBuf],
        legacy_config_paths: &[PathBuf],
    ) -> Result<Self> {
        Self::load_from_paths_with_catalog(path, legacy_session_paths, legacy_config_paths, None)
    }

    fn load_from_paths_with_catalog(
        path: &Path,
        legacy_session_paths: &[PathBuf],
        legacy_config_paths: &[PathBuf],
        catalog: Option<&ModelCatalog>,
    ) -> Result<Self> {
        if path.exists() {
            info!(path = %path.display(), "loading session store from disk");
            let mut store = Self::load_from_with_catalog(path, catalog)?;
            let mut recovered = false;
            for legacy in legacy_session_paths {
                if legacy == path || !legacy.exists() {
                    continue;
                }
                recovered |= store.preserve_existing_auth_from(legacy)?;
            }
            if recovered {
                info!(
                    path = %path.display(),
                    "recovered provider auth from legacy session store during startup"
                );
                store.save_to(path)?;
            }
            return Ok(store);
        }

        for legacy in legacy_session_paths {
            if legacy == path || !legacy.exists() {
                continue;
            }
            info!(
                from = %legacy.display(),
                to = %path.display(),
                "sessions.toml missing; migrating from legacy session store"
            );
            let store = Self::load_from_with_catalog(legacy, catalog)?;
            store.save_to(path)?;
            return Ok(store);
        }

        for legacy in legacy_config_paths {
            if !legacy.exists() || is_layered_config_schema(legacy) {
                continue;
            }
            info!(
                legacy = %legacy.display(),
                "sessions.toml missing; migrating from legacy config.toml"
            );
            let mut store = Self::from_legacy_config_path(legacy)?;
            store.apply_env_overrides_with_catalog(catalog);
            store.normalize_sessions_with_catalog(catalog);
            store.save_to(path)?;
            return Ok(store);
        }

        info!(
            path = %path.display(),
            "no session file found; starting with a fresh store"
        );
        let mut store = Self::default();
        store.apply_env_overrides_with_catalog(catalog);
        store.normalize_sessions_with_catalog(catalog);
        Ok(store)
    }

    pub async fn save_async(&self) -> Result<()> {
        self.save_with_auth_policy_async(SaveAuthPolicy::PreserveExisting)
            .await
    }

    pub(crate) async fn save_allowing_auth_clear_async(&self) -> Result<()> {
        self.save_with_auth_policy_async(SaveAuthPolicy::AllowClear)
            .await
    }

    async fn save_with_auth_policy_async(&self, auth_policy: SaveAuthPolicy) -> Result<()> {
        match &self.persistence {
            SessionPersistence::Toml => {
                let path = Self::path()?;
                self.save_to_with_auth_policy(&path, auth_policy)
            }
            SessionPersistence::Sqlite(storage) => {
                let store = self.clone_for_persistence();
                storage
                    .save_session_store_with_auth_policy(&store, auth_policy)
                    .await
            }
        }
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self> {
        Self::load_from_with_catalog(path, None)
    }

    fn load_from_with_catalog(
        path: impl AsRef<Path>,
        catalog: Option<&ModelCatalog>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read session file {:?}", path))?;
        let mut store: SessionStore = toml::from_str(&content)
            .with_context(|| format!("Failed to parse session file {:?}", path))?;
        store.migrate_legacy_into_providers();
        store.migrate_legacy_model_roles();
        store.apply_env_overrides_with_catalog(catalog);
        store.normalize_sessions_with_catalog(catalog);
        info!(
            path = %path.display(),
            current_provider = %store.current_provider,
            providers = store.providers.len(),
            provider_state = %store.provider_state_summary(),
            "session store loaded"
        );
        Ok(store)
    }

    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        self.save_to_with_auth_policy(path, SaveAuthPolicy::PreserveExisting)
    }

    #[cfg(test)]
    fn save_to_allowing_auth_clear(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        self.save_to_with_auth_policy(path, SaveAuthPolicy::AllowClear)
    }

    fn save_to_with_auth_policy(&self, path: &Path, auth_policy: SaveAuthPolicy) -> Result<()> {
        let mut store = self.clone();
        if matches!(auth_policy, SaveAuthPolicy::PreserveExisting)
            && path.exists()
            && let Err(err) = store.preserve_existing_auth_from(path)
        {
            let backup_path = backup_existing_session_file(path)
                .with_context(|| format!("Failed to back up malformed session file {:?}", path))?;
            warn!(
                path = %path.display(),
                backup = %backup_path.display(),
                error = %err,
                "could not preserve provider auth from existing session store; backed up before overwriting"
            );
        }

        let content =
            toml::to_string_pretty(&store).context("Failed to serialize session store")?;
        write_private_file_atomically(path, &content)?;

        info!(
            path = %path.display(),
            current_provider = %store.current_provider,
            providers = store.providers.len(),
            provider_state = %store.provider_state_summary(),
            "session store saved"
        );
        Ok(())
    }

    fn preserve_existing_auth_from(&mut self, path: &Path) -> Result<bool> {
        let existing = Self::load_from(path)?;
        let changed = self.preserve_existing_auth_from_store(&existing);
        if changed {
            tracing::info!(
                path = %path.display(),
                "preserved existing provider auth while merging session store"
            );
        }
        Ok(changed)
    }

    pub(crate) fn preserve_existing_auth_from_store(&mut self, existing: &Self) -> bool {
        let mut changed = false;
        for (id, existing_session) in &existing.providers {
            let session = self
                .providers
                .entry(id.clone())
                .or_insert_with(|| existing_session.clone());
            if session.preserve_auth_from(existing_session) {
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn clone_for_persistence(&self) -> Self {
        let mut store = self.clone();
        store.persistence = SessionPersistence::Toml;
        store.clear_runtime_secrets_for_persistence();
        store
    }

    /// Mirrors the persisted column set one-to-one, hence the argument count.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_persistent_parts(
        current_provider: String,
        active_connection_id: Option<ConnectionId>,
        active_model_id: Option<ModelId>,
        providers: HashMap<ConnectionId, ProviderSession>,
        model_roles: std::collections::BTreeMap<
            crate::model_role::LegacyModelRole,
            crate::model_role::ModelShortcutBinding,
        >,
        model_shortcuts: std::collections::BTreeMap<
            crate::model_role::ModelShortcutKey,
            crate::model_role::ModelShortcutBinding,
        >,
        theme: String,
        mode_models: std::collections::BTreeMap<String, String>,
    ) -> Self {
        let mut store = Self {
            current_provider,
            active_connection_id,
            active_model_id,
            providers,
            model_shortcuts,
            mode_models,
            legacy_model_roles: model_roles,
            theme,
            legacy: LegacySessionFields::default(),
            persistence: SessionPersistence::Toml,
            credential_store: CredentialStore::default(),
        };
        store.migrate_legacy_model_roles();
        store
    }

    fn migrate_legacy_into_providers(&mut self) {
        if let Some(opencode) = self.legacy.opencode_go.take() {
            self.providers
                .entry(ConnectionId::fallback("opencode"))
                .or_insert(opencode);
        }
        if let Some(codex) = self.legacy.codex.take() {
            self.providers
                .entry(ConnectionId::fallback("codex"))
                .or_insert(codex);
        }

        // v0.1.0-alpha.1 persisted the OpenCode Go connection under this id.
        // The connection was renamed to `opencode` before the 1.0 schema was
        // frozen. Move the complete provider record so credentials, the model,
        // and authorization metadata survive the public-alpha upgrade.
        if let Some(opencode) = self.providers.remove("opencode-go") {
            match self.providers.entry(ConnectionId::fallback("opencode")) {
                Entry::Vacant(entry) => {
                    entry.insert(opencode);
                }
                Entry::Occupied(mut entry) => {
                    let current = entry.get_mut();
                    current.preserve_auth_from(&opencode);
                    if current.base_url.trim().is_empty() {
                        current.base_url = opencode.base_url;
                    }
                    if current.model.trim().is_empty() {
                        current.model = opencode.model;
                    }
                }
            }
        }
        if self.current_provider == "opencode-go" {
            self.current_provider = "opencode".to_string();
        }
    }

    fn from_legacy_config_path(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read legacy config from {:?}", path))?;
        let legacy: LegacyConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse legacy config from {:?}", path))?;

        let mut providers = HashMap::new();
        let mut opencode = ProviderSession::new(legacy.api_key, legacy.base_url, legacy.model);
        opencode.credential_source = CredentialSource::None;
        providers.insert(ConnectionId::fallback("opencode"), opencode);
        providers.insert(
            ConnectionId::fallback("codex"),
            ProviderSession::new(String::new(), String::new(), String::new()),
        );
        Ok(Self {
            current_provider: default_current_provider(),
            active_connection_id: None,
            active_model_id: None,
            providers,
            model_shortcuts: Default::default(),
            mode_models: Default::default(),
            legacy_model_roles: Default::default(),
            theme: String::new(),
            legacy: LegacySessionFields::default(),
            persistence: SessionPersistence::Toml,
            credential_store: CredentialStore::default(),
        })
    }

    pub(crate) fn path() -> Result<PathBuf> {
        Ok(Self::bonsai_home_dir()?.join(SESSION_FILE))
    }

    fn legacy_session_path() -> Result<PathBuf> {
        Ok(Self::project_dirs()?.config_dir().join(SESSION_FILE))
    }

    fn legacy_config_paths() -> Result<Vec<PathBuf>> {
        let paths = vec![
            Self::bonsai_home_dir()?.join(LEGACY_CONFIG_FILE),
            Self::project_dirs()?.config_dir().join(LEGACY_CONFIG_FILE),
        ];
        Ok(paths)
    }

    #[cfg(not(test))]
    fn scrub_bonsai_owned_legacy_credentials() -> Result<()> {
        let mut paths = vec![Self::path()?, Self::legacy_session_path()?];
        paths.extend(Self::legacy_config_paths()?);
        paths.sort();
        paths.dedup();

        scrub_legacy_credential_files(&paths)
    }

    #[cfg(test)]
    fn scrub_bonsai_owned_legacy_credentials() -> Result<()> {
        Ok(())
    }

    fn project_dirs() -> Result<ProjectDirs> {
        ProjectDirs::from("", "", "bonsai").context("Failed to determine config directory")
    }

    fn bonsai_home_dir() -> Result<PathBuf> {
        let dirs = BaseDirs::new().context("Failed to determine home directory")?;
        Ok(home_bonsai_dir(dirs.home_dir()))
    }
}

fn scrub_legacy_credential_files(paths: &[PathBuf]) -> Result<()> {
    for path in paths.iter().filter(|path| path.exists()) {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to inspect legacy credentials in {path:?}"))?;
        let mut document = toml::from_str::<toml::Value>(&content)
            .with_context(|| format!("Failed to parse legacy credentials in {path:?}"))?;
        if !scrub_api_key_values(&mut document) {
            continue;
        }
        let content = toml::to_string_pretty(&document)
            .with_context(|| format!("Failed to serialize scrubbed legacy file {path:?}"))?;
        write_private_file_atomically(path, &content)
            .with_context(|| format!("Failed to scrub legacy credentials in {path:?}"))?;
    }
    Ok(())
}

fn scrub_api_key_values(value: &mut toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => {
            let mut changed = false;
            for (key, value) in table {
                if key == "api_key" && value.as_str().is_some_and(|secret| !secret.is_empty()) {
                    *value = toml::Value::String(String::new());
                    changed = true;
                } else {
                    changed |= scrub_api_key_values(value);
                }
            }
            changed
        }
        toml::Value::Array(values) => {
            let mut changed = false;
            for value in values {
                changed |= scrub_api_key_values(value);
            }
            changed
        }
        _ => false,
    }
}

fn harden_private_file(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)
            .with_context(|| format!("Failed to read permissions for {path:?}"))?
            .permissions();
        permissions.set_mode(0o600);
        std::fs::set_permissions(path, permissions)
            .with_context(|| format!("Failed to set private permissions on {path:?}"))?;
    }
    Ok(())
}

fn write_private_file_atomically(path: &Path, content: &str) -> Result<()> {
    // IMPORTANT PERSISTENCE INVARIANT: sessions.toml can be read by another
    // Bonsai process at any moment. Never replace this with `fs::write` or any
    // in-place truncation: readers then observe partial TOML, create bogus
    // `.invalid-*.bak` files, and may overwrite valid provider state. The
    // same-directory durable rename below is the required write path.
    crate::resource::repository::write_text(
        path,
        content,
        crate::resource::repository::WriteMode::Upsert,
    )
    .with_context(|| format!("Failed to atomically write private file {path:?}"))?;
    harden_private_file(path)
}

fn home_bonsai_dir(home: &Path) -> PathBuf {
    home.join(BONSAI_HOME_DIR)
}

fn backup_existing_session_file(path: &Path) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| SESSION_FILE.into());
    let timestamp = now_ms();
    let backup_path = path.with_file_name(format!("{file_name}.invalid-{timestamp}.bak"));
    std::fs::copy(path, &backup_path)
        .with_context(|| format!("Failed to copy {:?} to {:?}", path, backup_path))?;
    Ok(backup_path)
}

#[cfg(test)]
mod tests {
    #![allow(
        unsafe_code,
        reason = "edition-2024 requires `unsafe` around std::env::{set_var,remove_var}; test-only"
    )]
    use super::*;

    fn custom_openai_catalog() -> crate::model_catalog::ModelCatalog {
        let connection_id = "local-openai"
            .parse::<crate::model_catalog::ConnectionId>()
            .unwrap();
        let model_id = "local/qwen3-coder"
            .parse::<crate::model_catalog::ModelId>()
            .unwrap();
        crate::model_catalog::ModelCatalog::from_spec(crate::model_catalog::CatalogSpec {
            connections: vec![crate::model_catalog::ConnectionSpec {
                id: connection_id.clone(),
                enabled: true,
                display_name: "Local OpenAI".into(),
                auth: crate::model_catalog::ConnectionAuth::OptionalApiKey,
                transport: crate::model_catalog::TransportProtocol::OpenAiChat,
                default_base_url: "http://localhost:11434/v1".into(),
                origins: Vec::new(),
                api_key_env: Some("LOCAL_OPENAI_API_KEY".into()),
                model_env: Some("LOCAL_OPENAI_MODEL".into()),
                base_url_env: Some("LOCAL_OPENAI_BASE_URL".into()),
                default_model: Some(model_id.clone()),
                default_endpoint_path: Some("chat/completions".into()),
                default_token_counter: Some(crate::provider::TokenCounterKind::Tiktoken),
                peak_pricing_windows_utc: Vec::new(),
                models_dev_provider: None,
                model_exclude_prefixes: Vec::new(),
                reasoning_codec: None,
                prompt_cache: false,
                prompt_cache_header: None,
                prompt_cache_policy: Default::default(),
                reasoning_content_echo: false,
                usage_frame_ends_stream: false,
                auth_header: None,
                discovery: Default::default(),
            }],
            targets: vec![crate::model_catalog::TargetSpec {
                connection: connection_id,
                enabled: true,
                model: model_id,
                display_name: None,
                metadata_model: None,
                remote_model: Some("qwen3-coder:latest".into()),
                aliases: Vec::new(),
                recommended: true,
                recommended_effort: None,
                discouraged_efforts: Vec::new(),
                is_default: true,
                transport: None,
                prompt_cache_policy: None,
                endpoint_path: None,
                context_window: Some(131_072),
                output_limit: None,
                token_counter: None,
                max_tokens: None,
                reasoning_codec: None,
                reasoning_options: None,
                features: vec![crate::model_catalog::ModelFeature::ToolCall],
                pricing: None,
                pricing_tiers: Vec::new(),
                peak_pricing: None,
                peak_pricing_tiers: Vec::new(),
                roles: Vec::new(),
                pinned: false,
                pinned_fields: Vec::new(),
            }],
            models_dev: crate::model_catalog::ModelsDevCatalog::default(),
            connection_sources: std::collections::HashMap::new(),
        })
    }

    fn make_store() -> SessionStore {
        let mut store = SessionStore::default();
        store.ensure_provider("opencode");
        store.ensure_provider("openai-compatible");
        store.ensure_provider("anthropic-compatible");
        store.ensure_provider("codex");
        store.ensure_provider("anthropic");
        store.ensure_provider("minimax-coding-plan");
        store
    }

    #[test]
    fn home_bonsai_dir_uses_dot_bonsai_under_home() {
        assert_eq!(
            home_bonsai_dir(Path::new("/tmp/example-home")),
            PathBuf::from("/tmp/example-home/.bonsai")
        );
    }

    #[test]
    fn load_migrates_legacy_session_store_to_home_path() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let primary = temp_dir
            .path()
            .join("home")
            .join(".bonsai")
            .join("sessions.toml");
        let legacy = temp_dir.path().join("legacy").join("sessions.toml");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy,
            r#"
                current_provider = "codex"

                [providers.codex]
                api_key = "codex-token"
                account_id = "codex-account"
                base_url = "https://chatgpt.com/backend-api/codex"
                model = "gpt-5.5"
            "#,
        )
        .unwrap();

        let store = SessionStore::load_from_paths(&primary, &[legacy], &[]).unwrap();

        assert_eq!(store.current_kind_id(), "codex");
        assert_eq!(store.session("codex").api_key, "codex-token");
        assert_eq!(store.session("codex").account_id, "codex-account");
        assert!(primary.exists());
    }

    #[test]
    fn load_migrates_legacy_config_toml_when_no_session_store_exists() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let primary = temp_dir
            .path()
            .join("home")
            .join(".bonsai")
            .join("sessions.toml");
        let legacy_config = temp_dir
            .path()
            .join("home")
            .join(".bonsai")
            .join("config.toml");
        std::fs::create_dir_all(legacy_config.parent().unwrap()).unwrap();
        std::fs::write(
            &legacy_config,
            r#"
                api_key = "sk-legacy"
                model = "some-model"
            "#,
        )
        .unwrap();

        let store = SessionStore::load_from_paths(&primary, &[], &[legacy_config]).unwrap();

        assert_eq!(store.session("opencode").api_key, "sk-legacy");
        assert_eq!(store.session("opencode").model, "some-model");
        assert!(primary.exists());
    }

    #[test]
    fn public_alpha_session_fixture_upgrades_without_losing_user_state() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let alpha = temp_dir.path().join("v0.1.0-alpha.1-sessions.toml");
        let upgraded = temp_dir.path().join("sessions.toml");
        std::fs::write(
            &alpha,
            include_str!("../../tests/fixtures/upgrade/v0.1.0-alpha.1/sessions.toml"),
        )
        .unwrap();

        let store = SessionStore::load_from_paths(&upgraded, &[alpha], &[]).unwrap();

        assert_eq!(store.current_kind_id(), "opencode");
        assert_eq!(store.theme, "tokyo-night");
        assert_eq!(store.session("opencode").model, "qwen3.7-max");
        assert_eq!(
            store.session("opencode").base_url,
            "https://opencode.ai/zen/go/v1"
        );
        assert_eq!(store.session("opencode").api_key, "alpha-fixture-token");
        assert_eq!(store.session("codex").account_id, "alpha-account");

        let persisted = std::fs::read_to_string(upgraded).unwrap();
        assert!(!persisted.contains("alpha-fixture-token"));
        assert!(!persisted.contains("opencode-go"));
        assert!(persisted.contains("[providers.opencode]"));
    }

    #[test]
    fn public_alpha_config_fixture_upgrades_to_current_provider_state() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let primary = temp_dir.path().join("sessions.toml");
        let alpha = temp_dir.path().join("v0.1.0-alpha.1-config.toml");
        std::fs::write(
            &alpha,
            include_str!("../../tests/fixtures/upgrade/v0.1.0-alpha.1/config.toml"),
        )
        .unwrap();

        let store = SessionStore::load_from_paths(&primary, &[], &[alpha]).unwrap();

        assert_eq!(store.current_kind_id(), "opencode");
        assert_eq!(store.session("opencode").model, "qwen3.7-max");
        assert_eq!(store.session("opencode").api_key, "alpha-config-token");
        assert!(primary.exists());
        assert!(
            !std::fs::read_to_string(primary)
                .unwrap()
                .contains("alpha-config-token")
        );
    }

    /// Regression: `$BONSAI_HOME/config.toml` is shared by this legacy
    /// migrator and the new layered-config loader (`crate::config`). A file
    /// written for the new schema has no `api_key`/`model` keys at all, so
    /// blindly attempting `LegacyConfig` deserialization on it used to fail
    /// outright and break startup for a fresh install that only has a
    /// new-schema global config.
    #[test]
    fn layered_schema_config_toml_is_not_mistaken_for_legacy_config() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let layered_config = temp_dir.path().join("config.toml");
        std::fs::write(
            &layered_config,
            "schema_version = 1\n[sandbox]\ndeny_network = true\n",
        )
        .unwrap();
        assert!(is_layered_config_schema(&layered_config));

        let primary = temp_dir
            .path()
            .join("home")
            .join(".bonsai")
            .join("sessions.toml");
        // Must not error attempting to parse the layered schema as the legacy
        // {api_key, model, base_url} shape; falls through to a fresh store
        // instead of the legacy-config migration branch (which would have
        // written `primary`).
        SessionStore::load_from_paths(&primary, &[], &[layered_config]).unwrap();
        assert!(!primary.exists());
    }

    #[test]
    fn legacy_config_toml_is_not_mistaken_for_layered_schema() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let legacy_config = temp_dir.path().join("config.toml");
        std::fs::write(&legacy_config, "api_key = \"sk-legacy\"\nmodel = \"m\"\n").unwrap();
        assert!(!is_layered_config_schema(&legacy_config));
    }

    #[test]
    fn legacy_credential_scrub_removes_nested_api_keys() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");
        std::fs::write(
            &path,
            r#"
                api_key = "top-secret"

                [providers.anthropic]
                api_key = "nested-secret"
                model = "claude-sonnet-4-5"
            "#,
        )
        .unwrap();

        scrub_legacy_credential_files(std::slice::from_ref(&path)).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("top-secret"));
        assert!(!content.contains("nested-secret"));
        let document = toml::from_str::<toml::Value>(&content).unwrap();
        assert_eq!(document["api_key"].as_str(), Some(""));
        assert_eq!(
            document["providers"]["anthropic"]["api_key"].as_str(),
            Some("")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn load_with_storage_keeps_catalog_connection_current() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = crate::storage::Storage::open_at(temp_dir.path().join("bonsai.db"))
            .await
            .unwrap();
        let catalog = custom_openai_catalog();
        let mut providers = HashMap::new();
        providers.insert("local-openai".parse().unwrap(), ProviderSession::default());
        let store = SessionStore::from_persistent_parts(
            "local-openai".to_string(),
            None,
            None,
            providers,
            Default::default(),
            Default::default(),
            String::new(),
            Default::default(),
        );
        storage
            .save_session_store_with_auth_policy(&store, SaveAuthPolicy::PreserveExisting)
            .await
            .unwrap();

        let loaded = SessionStore::load_with_storage_and_catalog(&storage, Some(&catalog))
            .await
            .unwrap();

        assert_eq!(loaded.current_kind_id(), "local-openai");
        assert_eq!(
            loaded.session("local-openai").base_url,
            "http://localhost:11434/v1"
        );
        assert_eq!(loaded.session("local-openai").model, "local/qwen3-coder");
        assert_eq!(
            loaded.active_connection_id().map(ToString::to_string),
            Some("local-openai".to_string())
        );
        assert_eq!(
            loaded.active_model_id().map(ToString::to_string),
            Some("local/qwen3-coder".to_string())
        );
    }

    #[test]
    fn load_recovers_legacy_auth_when_primary_has_blank_provider() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let primary = temp_dir.path().join("primary").join("sessions.toml");
        let legacy = temp_dir.path().join("legacy").join("sessions.toml");
        std::fs::create_dir_all(primary.parent().unwrap()).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(
            &primary,
            r#"
                current_provider = "anthropic"

                [providers.anthropic]
                api_key = ""
                base_url = "https://api.anthropic.com"
                model = "claude-sonnet-4-5"
            "#,
        )
        .unwrap();
        std::fs::write(
            &legacy,
            r#"
                current_provider = "anthropic"

                [providers.anthropic]
                api_key = "sk-ant-legacy"
                base_url = "https://api.anthropic.com"
                model = "claude-sonnet-4-5"
            "#,
        )
        .unwrap();

        let store = SessionStore::load_from_paths(&primary, &[legacy], &[]).unwrap();

        assert_eq!(store.session("anthropic").api_key, "sk-ant-legacy");

        let reloaded = SessionStore::load_from(&primary).unwrap();
        assert!(reloaded.session("anthropic").api_key.is_empty());
        assert!(
            !std::fs::read_to_string(&primary)
                .unwrap()
                .contains("sk-ant-legacy")
        );
    }

    #[test]
    fn load_does_not_replace_primary_auth_with_legacy_auth() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let primary = temp_dir.path().join("primary").join("sessions.toml");
        let legacy = temp_dir.path().join("legacy").join("sessions.toml");
        std::fs::create_dir_all(primary.parent().unwrap()).unwrap();
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(
            &primary,
            r#"
                current_provider = "anthropic"

                [providers.anthropic]
                api_key = "sk-ant-primary"
                base_url = "https://api.anthropic.com"
                model = "claude-sonnet-4-5"
            "#,
        )
        .unwrap();
        std::fs::write(
            &legacy,
            r#"
                current_provider = "anthropic"

                [providers.anthropic]
                api_key = "sk-ant-legacy"
                base_url = "https://api.anthropic.com"
                model = "claude-sonnet-4-5"
            "#,
        )
        .unwrap();

        let store = SessionStore::load_from_paths(&primary, &[legacy], &[]).unwrap();

        assert_eq!(store.session("anthropic").api_key, "sk-ant-primary");
    }

    #[test]
    fn save_to_preserves_existing_auth_when_stale_store_has_blanks() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");

        let mut authorized = make_store();
        authorized.session_mut("anthropic").api_key = "sk-ant-saved".to_string();
        authorized.session_mut("anthropic").credential_source = CredentialSource::Keyring;
        authorized.session_mut("anthropic").authorized_at = Some(SystemTime::UNIX_EPOCH);
        authorized.save_to(&path).unwrap();

        let stale = make_store();
        stale.save_to(&path).unwrap();

        let loaded = SessionStore::load_from(&path).unwrap();
        assert!(loaded.session("anthropic").api_key.is_empty());
        assert_eq!(
            loaded.session("anthropic").credential_source,
            CredentialSource::Keyring
        );
        assert_eq!(
            loaded.session("anthropic").authorized_at,
            Some(SystemTime::UNIX_EPOCH)
        );
    }

    #[test]
    fn save_to_preserve_existing_auth_keeps_cleared_context_window() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");

        let mut authorized = make_store();
        authorized.session_mut("anthropic").api_key = "sk-ant-saved".to_string();
        authorized.session_mut("anthropic").credential_source = CredentialSource::Keyring;
        authorized.session_mut("anthropic").context_window = Some(200_000);
        authorized.save_to(&path).unwrap();

        let mut updated = make_store();
        updated.session_mut("anthropic").context_window = None;
        updated.save_to(&path).unwrap();

        let loaded = SessionStore::load_from(&path).unwrap();
        assert!(loaded.session("anthropic").api_key.is_empty());
        assert_eq!(
            loaded.session("anthropic").credential_source,
            CredentialSource::Keyring
        );
        assert_eq!(loaded.session("anthropic").context_window, None);
    }

    #[test]
    fn save_to_allowing_auth_clear_can_clear_existing_auth() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");

        let mut authorized = make_store();
        authorized.session_mut("anthropic").api_key = "sk-ant-saved".to_string();
        authorized.save_to(&path).unwrap();

        let stale = make_store();
        stale.save_to_allowing_auth_clear(&path).unwrap();

        let loaded = SessionStore::load_from(&path).unwrap();
        assert!(loaded.session("anthropic").api_key.is_empty());
    }

    #[test]
    fn minimax_base_url_is_not_rewritten() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");
        std::fs::write(
            &path,
            r#"
                current_provider = "minimax-coding-plan"

                [providers.minimax-coding-plan]
                api_key = "sk-mm"
                base_url = "https://api.minimax.io/anthropic/v1"
                model = "MiniMax-M3"
            "#,
        )
        .unwrap();

        let store = SessionStore::load_from(&path).unwrap();
        assert_eq!(
            store.session("minimax-coding-plan").base_url,
            "https://api.minimax.io/anthropic/v1"
        );
    }

    #[test]
    fn invalid_current_provider_is_normalized_and_persisted() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");
        std::fs::write(
            &path,
            r#"
                current_provider = "does-not-exist"

                [providers.minimax-coding-plan]
                api_key = "sk-mm"
                base_url = "https://api.minimax.io/anthropic"
                model = "MiniMax-M3"
            "#,
        )
        .unwrap();

        let store = SessionStore::load_from(&path).unwrap();
        assert_eq!(store.current_kind_id(), DEFAULT_PROVIDER_ID);
        assert_eq!(store.current_provider, DEFAULT_PROVIDER_ID);

        store.save_to(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains(r#"current_provider = "opencode""#));
        assert!(!saved.contains(r#"current_provider = "does-not-exist""#));
    }

    #[test]
    fn roundtrip_persists_providers_map() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");

        let mut store = make_store();
        store.set_current_kind_id("minimax-coding-plan");
        store.session_mut("minimax-coding-plan").api_key = "sk-mm".to_string();
        store.session_mut("codex").reasoning =
            ReasoningSelection::from_effort(crate::provider::ReasoningEffort::High);
        store.save_to(&path).unwrap();

        let loaded = SessionStore::load_from(&path).unwrap();
        assert_eq!(loaded.current_kind_id(), "minimax-coding-plan");
        assert!(loaded.session("minimax-coding-plan").api_key.is_empty());
        assert_eq!(
            loaded.session("codex").reasoning,
            ReasoningSelection::from_effort(crate::provider::ReasoningEffort::High)
        );
        assert_eq!(
            loaded.session("codex").model_reasoning.get("gpt-5.6-sol"),
            Some(&ReasoningSelection::from_effort(
                crate::provider::ReasoningEffort::High
            ))
        );
    }

    #[test]
    fn active_model_target_roundtrips_through_disk() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");

        let mut store = make_store();
        store.set_active_model_target(
            "codex",
            Some("codex".parse().unwrap()),
            Some("openai/gpt-5.5".parse().unwrap()),
            "gpt-5.5",
        );
        store.save_to(&path).unwrap();

        let loaded = SessionStore::load_from(&path).unwrap();
        assert_eq!(loaded.current_kind_id(), "codex");
        assert_eq!(
            loaded.active_connection_id().map(|id| id.as_str()),
            Some("codex")
        );
        assert_eq!(
            loaded.active_model_id().map(|id| id.as_str()),
            Some("openai/gpt-5.5")
        );
        assert_eq!(loaded.session("codex").model, "openai/gpt-5.5");
    }

    #[test]
    fn model_reasoning_roundtrip_per_model() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");

        let mut store = make_store();
        let metadata = crate::provider::metadata_for("codex").unwrap();
        let codex = store.session_mut("codex");
        codex.model = "gpt-5.5".to_string();
        codex.store_model_reasoning(
            "gpt-5.5",
            ReasoningSelection::from_effort(crate::provider::ReasoningEffort::High),
        );
        codex.store_model_reasoning(
            "gpt-5.4-mini",
            ReasoningSelection::from_effort(crate::provider::ReasoningEffort::Low),
        );
        store.save_to(&path).unwrap();

        let loaded = SessionStore::load_from(&path).unwrap();
        let codex = loaded.session("codex");
        assert_eq!(
            codex.reasoning_for_model(metadata, "gpt-5.5"),
            ReasoningSelection::from_effort(crate::provider::ReasoningEffort::High)
        );
        assert_eq!(
            codex.reasoning_for_model(metadata, "gpt-5.4-mini"),
            ReasoningSelection::from_effort(crate::provider::ReasoningEffort::Low)
        );
    }

    #[test]
    fn missing_profile_loads_as_auto() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");
        std::fs::write(
            &path,
            r#"
                current_provider = "codex"

                [providers.codex]
                api_key = "token"
                base_url = "https://chatgpt.com/backend-api/codex"
                model = "gpt-5.5"
            "#,
        )
        .unwrap();

        let store = SessionStore::load_from(&path).unwrap();
        assert_eq!(
            store.session("codex").reasoning,
            ReasoningSelection::default()
        );
    }

    #[test]
    fn invalid_profile_effort_loads_as_auto() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");
        std::fs::write(
            &path,
            r#"
                current_provider = "codex"

                [providers.codex]
                api_key = "token"
                base_url = "https://chatgpt.com/backend-api/codex"
                model = "gpt-5.5"

                [providers.codex.reasoning]
                effort = "turbo"
            "#,
        )
        .unwrap();

        let store = SessionStore::load_from(&path).unwrap();
        assert_eq!(
            store.session("codex").reasoning,
            ReasoningSelection::default()
        );
    }

    #[test]
    fn unsupported_profile_effort_normalizes_to_auto() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");
        std::fs::write(
            &path,
            r#"
                current_provider = "anthropic"

                [providers.anthropic]
                api_key = "sk-ant"
                base_url = "https://api.anthropic.com"
                model = "claude-sonnet-4-5"

                [providers.anthropic.reasoning]
                effort = "high"
            "#,
        )
        .unwrap();

        let store = SessionStore::load_from(&path).unwrap();
        assert_eq!(
            store.session("anthropic").reasoning,
            ReasoningSelection::default()
        );
    }

    #[test]
    fn minimax_unsupported_profile_effort_normalizes_to_auto() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");
        std::fs::write(
            &path,
            r#"
                current_provider = "minimax-coding-plan"

                [providers.minimax-coding-plan]
                api_key = "sk-mm"
                base_url = "https://api.minimax.io/anthropic"
                model = "MiniMax-M3"

                [providers.minimax-coding-plan.reasoning]
                effort = "high"
            "#,
        )
        .unwrap();

        let store = SessionStore::load_from(&path).unwrap();
        assert_eq!(
            store.session("minimax-coding-plan").reasoning,
            ReasoningSelection::default()
        );
    }

    #[test]
    fn provider_keys_never_roundtrip_through_toml() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");

        let mut store = make_store();
        store.session_mut("opencode").api_key = "sk-oc".to_string();
        store.session_mut("anthropic").api_key = "sk-ant".to_string();
        store.session_mut("minimax-coding-plan").api_key = "sk-mm".to_string();
        store.save_to(&path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("sk-oc"));
        assert!(!saved.contains("sk-ant"));
        assert!(!saved.contains("sk-mm"));

        let loaded = SessionStore::load_from(&path).unwrap();
        assert!(loaded.session("opencode").api_key.is_empty());
        assert!(loaded.session("anthropic").api_key.is_empty());
        assert!(loaded.session("minimax-coding-plan").api_key.is_empty());
    }

    #[test]
    fn empty_env_var_does_not_restore_a_persisted_secret() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");

        let mut store = make_store();
        store.session_mut("anthropic").api_key = "sk-ant-123".to_string();
        store.session_mut("anthropic").credential_source =
            CredentialSource::Environment("ANTHROPIC_API_KEY".to_string());
        store.session_mut("anthropic").authorized_at = Some(SystemTime::UNIX_EPOCH);
        store.set_current_kind_id("anthropic");
        store.save_to(&path).unwrap();

        crate::util::test_env::with_var("ANTHROPIC_API_KEY", Some(""), || {
            let loaded = SessionStore::load_from(&path).unwrap();

            assert!(loaded.session("anthropic").api_key.is_empty());
            assert!(!loaded.authorized_provider_ids().contains(&"anthropic"));
            assert!(
                !std::fs::read_to_string(&path)
                    .unwrap()
                    .contains("sk-ant-123")
            );
        });
    }

    #[tokio::test]
    async fn keyring_reference_resumes_authorized_provider_and_current_choice() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let storage = crate::storage::Storage::open_at(temp_dir.path().join("bonsai.db"))
            .await
            .unwrap();
        let credentials = CredentialStore::memory();
        credentials
            .set(&CredentialSource::Keyring, "anthropic", "sk-ant-123")
            .await
            .unwrap();

        // First "session": user authorizes a provider, switches to it, then
        // exits. The store should have written the key and current_provider
        // to disk.
        let mut first_run = make_store();
        first_run.session_mut("anthropic").api_key = "sk-ant-123".to_string();
        first_run.session_mut("anthropic").credential_source = CredentialSource::Keyring;
        first_run.session_mut("anthropic").authorized_at = Some(SystemTime::UNIX_EPOCH);
        first_run.set_current_kind_id("anthropic");
        storage
            .save_session_store_with_auth_policy(&first_run, SaveAuthPolicy::AllowClear)
            .await
            .unwrap();

        // Second "session": a brand-new SessionStore loads the file and
        // should see the authorization and the current provider choice.
        let second_run =
            SessionStore::load_with_storage_and_credential_store(&storage, credentials)
                .await
                .unwrap();
        assert_eq!(second_run.current_kind_id(), "anthropic");
        assert!(second_run.authorized_provider_ids().contains(&"anthropic"));
        assert_eq!(second_run.session("anthropic").api_key, "sk-ant-123");
    }

    #[test]
    fn current_provider_survives_multiple_save_load_cycles() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");

        // First "session": authorize a non-default provider and switch to it.
        let mut store = make_store();
        store.session_mut("anthropic").api_key = "sk-ant".to_string();
        store.set_current_kind_id("anthropic");
        store.save_to(&path).unwrap();

        // Second "session": load, verify, then save again unchanged (simulating
        // a second close with no further user action).
        let reloaded = SessionStore::load_from(&path).unwrap();
        assert_eq!(reloaded.current_kind_id(), "anthropic");
        reloaded.save_to(&path).unwrap();

        // Third "session": load again and confirm the choice is still there.
        let reloaded_again = SessionStore::load_from(&path).unwrap();
        assert_eq!(
            reloaded_again.current_kind_id(),
            "anthropic",
            "current_provider must survive repeated save/load cycles"
        );
    }

    #[test]
    fn legacy_fields_migrate_into_providers_map() {
        let toml = r#"
            current_provider = "opencode"

            [opencode_go]
            api_key = "sk-legacy"
            base_url = "https://example.com/v1"
            model = "qwen3.7-max"

            [codex]
            api_key = ""
            base_url = "https://chatgpt.com/backend-api/codex"
            model = "gpt-5.5"
        "#;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("sessions.toml");
        std::fs::write(&path, toml).unwrap();

        let store = SessionStore::load_from(&path).unwrap();
        let opencode = store.session("opencode");
        assert_eq!(opencode.api_key, "sk-legacy");
        assert_eq!(opencode.base_url, "https://example.com/v1");
    }
}
