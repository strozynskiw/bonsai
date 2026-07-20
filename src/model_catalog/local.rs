use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::provider::TokenCounterKind;

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalCatalogConnectionInput {
    pub id: ConnectionId,
    pub display_name: String,
    pub transport: TransportProtocol,
    pub base_url: String,
    pub discovery: DiscoveryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalCatalogTargetInput {
    pub remote_model: String,
    /// Server-reported human name (LM Studio's `display_name`), persisted so
    /// the picker shows it even without a live cache.
    pub display_name: Option<String>,
    /// `None` when the server did not report a window and the user left it
    /// blank — the resolution ladder handles unknown windows; fabricating a
    /// number here would masquerade as real metadata.
    pub context_window: Option<u32>,
    pub output_limit: Option<u32>,
    pub tool_call: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalCatalogEntryInput {
    pub connection: LocalCatalogConnectionInput,
    pub targets: Vec<LocalCatalogTargetInput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalCatalogWriteReport {
    pub connection_id: ConnectionId,
    pub model_ids: Vec<ModelId>,
    pub provider_path: PathBuf,
    pub model_path: PathBuf,
}

#[derive(Debug, Serialize)]
struct LocalConnectionDocument {
    connections: Vec<LocalConnectionToml>,
}

#[derive(Debug, Serialize)]
struct LocalConnectionToml {
    id: ConnectionId,
    enabled: bool,
    display_name: String,
    auth: ConnectionAuth,
    transport: TransportProtocol,
    default_base_url: String,
    api_key_env: String,
    model_env: String,
    base_url_env: String,
    default_model: ModelId,
    default_endpoint_path: String,
    default_token_counter: TokenCounterKind,
    prompt_cache: bool,
    #[serde(skip_serializing_if = "is_generic_discovery")]
    discovery: DiscoveryKind,
}

fn is_generic_discovery(discovery: &DiscoveryKind) -> bool {
    *discovery == DiscoveryKind::Generic
}

#[derive(Debug, Serialize)]
struct LocalTargetDocument {
    targets: Vec<LocalTargetToml>,
}

#[derive(Debug, Serialize)]
struct LocalTargetToml {
    connection: ConnectionId,
    enabled: bool,
    model: ModelId,
    remote_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    #[serde(rename = "default")]
    is_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_window: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_limit: Option<u32>,
    token_counter: TokenCounterKind,
    features: Vec<ModelFeature>,
}

pub(crate) fn write_local_catalog_entry(
    home_dir: &Path,
    input: LocalCatalogEntryInput,
) -> Result<LocalCatalogWriteReport, CatalogError> {
    write_local_catalog_entry_with_mode(home_dir, input, LocalCatalogWriteMode::Create)
}

/// Rewrite an existing wizard-managed provider in place. The current
/// `providers/<id>.toml` and `models/<id>.toml` are set aside and restored
/// verbatim if the new entry fails to write or validate.
pub(crate) fn replace_local_catalog_entry(
    home_dir: &Path,
    input: LocalCatalogEntryInput,
) -> Result<LocalCatalogWriteReport, CatalogError> {
    write_local_catalog_entry_with_mode(home_dir, input, LocalCatalogWriteMode::Replace)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalCatalogWriteMode {
    Create,
    Replace,
}

fn write_local_catalog_entry_with_mode(
    home_dir: &Path,
    input: LocalCatalogEntryInput,
    mode: LocalCatalogWriteMode,
) -> Result<LocalCatalogWriteReport, CatalogError> {
    transaction::with_recovered_catalog_lock(home_dir, || {
        validate_local_catalog_input(&input)?;
        let paths = CatalogPaths::from_home_dir(home_dir);
        ensure_user_catalog_scaffold(&paths)?;
        let provider_path = paths
            .provider_dir
            .join(format!("{}.toml", input.connection.id.as_str()));
        let model_path = paths
            .model_dir
            .join(format!("{}.toml", input.connection.id.as_str()));
        match mode {
            LocalCatalogWriteMode::Create => {
                refuse_existing_local_catalog_file(&provider_path)?;
                refuse_existing_local_catalog_file(&model_path)?;
                refuse_existing_catalog_ids(&paths, &input)?;
            }
            LocalCatalogWriteMode::Replace => {
                if !provider_path.exists()
                    || is_pure_disable_patch(&provider_path, &input.connection.id)?
                {
                    return Err(CatalogError::NotUserManagedConnection {
                        connection_id: input.connection.id.clone(),
                    });
                }
                ensure_file_scoped_to_connection(
                    &provider_path,
                    &model_path,
                    &input.connection.id,
                )?;
            }
        }

        let prepared = prepare_local_catalog_entry(input, &provider_path, &model_path)?;
        transaction::commit_catalog_pair_locked(
            home_dir,
            &paths,
            &prepared.connection_id,
            transaction::CatalogPairUpdate::Present {
                provider_content: &prepared.provider_content,
                model_content: &prepared.model_content,
            },
            || {
                let catalog = load_catalog_from_paths_unlocked(&paths)?;
                for model_id in &prepared.model_ids {
                    catalog.resolve(&prepared.connection_id, model_id)?;
                }
                Ok(LocalCatalogWriteReport {
                    connection_id: prepared.connection_id.clone(),
                    model_ids: prepared.model_ids.clone(),
                    provider_path,
                    model_path,
                })
            },
        )
    })
}

struct PreparedLocalCatalogEntry {
    connection_id: ConnectionId,
    model_ids: Vec<ModelId>,
    provider_content: String,
    model_content: String,
}

fn prepare_local_catalog_entry(
    input: LocalCatalogEntryInput,
    provider_path: &Path,
    model_path: &Path,
) -> Result<PreparedLocalCatalogEntry, CatalogError> {
    let provider_env_prefix = env_prefix_for_connection_id(&input.connection.id);
    let targets = local_target_rows(&input)?;
    let Some(default_model) = targets.first().map(|target| target.model.clone()) else {
        return Err(CatalogError::InvalidLocalCatalogInput {
            message: "at least one model target is required".to_string(),
        });
    };
    let provider_doc = LocalConnectionDocument {
        connections: vec![LocalConnectionToml {
            id: input.connection.id.clone(),
            enabled: true,
            display_name: input.connection.display_name.trim().to_string(),
            auth: ConnectionAuth::OptionalApiKey,
            transport: input.connection.transport,
            default_base_url: input
                .connection
                .base_url
                .trim()
                .trim_end_matches('/')
                .to_string(),
            api_key_env: format!("{provider_env_prefix}_API_KEY"),
            model_env: format!("{provider_env_prefix}_MODEL"),
            base_url_env: format!("{provider_env_prefix}_BASE_URL"),
            default_model,
            default_endpoint_path: default_endpoint_path_for_transport(input.connection.transport)
                .to_string(),
            default_token_counter: TokenCounterKind::Heuristic,
            // Local backends either honor the cache hint (`prompt_cache_key` /
            // `cache_control` breakpoints) or ignore it; opting in by default
            // keeps their prefix/KV caches warm at no cost.
            prompt_cache: true,
            discovery: input.connection.discovery,
        }],
    };
    let target_doc = LocalTargetDocument { targets };
    let provider_content =
        toml::to_string_pretty(&provider_doc).map_err(|source| CatalogError::TomlSerialize {
            source_name: provider_path.display().to_string(),
            source,
        })?;
    let model_content =
        toml::to_string_pretty(&target_doc).map_err(|source| CatalogError::TomlSerialize {
            source_name: model_path.display().to_string(),
            source,
        })?;
    let model_ids = target_doc
        .targets
        .iter()
        .map(|target| target.model.clone())
        .collect();
    Ok(PreparedLocalCatalogEntry {
        connection_id: input.connection.id,
        model_ids,
        provider_content,
        model_content,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalCatalogRemoveReport {
    pub connection_id: ConnectionId,
    pub removed: Vec<PathBuf>,
}

/// Remove a wizard-managed provider: delete its `providers/<id>.toml` and
/// `models/<id>.toml`. Refuses built-ins (no user file) and hand-authored
/// files that define entries beyond this connection. The files are set aside
/// first and restored if the remaining catalog fails to validate.
pub(crate) fn remove_local_catalog_entry(
    home_dir: &Path,
    connection_id: &ConnectionId,
) -> Result<LocalCatalogRemoveReport, CatalogError> {
    transaction::with_recovered_catalog_lock(home_dir, || {
        let paths = CatalogPaths::from_home_dir(home_dir);
        ensure_user_catalog_scaffold(&paths)?;
        let provider_path = paths
            .provider_dir
            .join(format!("{}.toml", connection_id.as_str()));
        let model_path = paths
            .model_dir
            .join(format!("{}.toml", connection_id.as_str()));
        if !provider_path.exists() || is_pure_disable_patch(&provider_path, connection_id)? {
            return Err(CatalogError::NotUserManagedConnection {
                connection_id: connection_id.clone(),
            });
        }
        ensure_file_scoped_to_connection(&provider_path, &model_path, connection_id)?;
        let removed: Vec<PathBuf> = [&provider_path, &model_path]
            .into_iter()
            .filter(|path| path.exists())
            .cloned()
            .collect();

        transaction::commit_catalog_pair_locked(
            home_dir,
            &paths,
            connection_id,
            transaction::CatalogPairUpdate::Absent,
            || {
                load_catalog_from_paths_unlocked(&paths)?;
                Ok(LocalCatalogRemoveReport {
                    connection_id: connection_id.clone(),
                    removed,
                })
            },
        )
    })
}

/// Disable or re-enable a built-in provider via a user patch file containing
/// only `id` + `enabled`. Disable refuses when a user file already exists for
/// the id (a custom provider is removed, not disabled) and rolls back if the
/// patch would leave zero enabled connections. Enable deletes the patch file,
/// refusing when the file carries more than the disable patch.
pub(crate) fn set_builtin_connection_enabled(
    home_dir: &Path,
    connection_id: &ConnectionId,
    enabled: bool,
) -> Result<PathBuf, CatalogError> {
    transaction::with_recovered_catalog_lock(home_dir, || {
        set_builtin_connection_enabled_locked(home_dir, connection_id, enabled)
    })
}

fn set_builtin_connection_enabled_locked(
    home_dir: &Path,
    connection_id: &ConnectionId,
    enabled: bool,
) -> Result<PathBuf, CatalogError> {
    let paths = CatalogPaths::from_home_dir(home_dir);
    ensure_user_catalog_scaffold(&paths)?;
    let provider_path = paths
        .provider_dir
        .join(format!("{}.toml", connection_id.as_str()));

    if enabled {
        if !provider_path.exists() {
            return Ok(provider_path);
        }
        if !is_pure_disable_patch(&provider_path, connection_id)? {
            return Err(CatalogError::SharedCatalogFile {
                path: provider_path,
                connection_id: connection_id.clone(),
            });
        }
        fs::remove_file(&provider_path).map_err(|source| CatalogError::WriteFile {
            path: provider_path.clone(),
            source,
        })?;
        return Ok(provider_path);
    }

    if provider_path.exists() {
        if is_pure_disable_patch(&provider_path, connection_id)? {
            return Ok(provider_path);
        }
        return Err(CatalogError::SharedCatalogFile {
            path: provider_path,
            connection_id: connection_id.clone(),
        });
    }

    #[derive(Serialize)]
    struct DisableDocument {
        connections: Vec<DisablePatch>,
    }
    #[derive(Serialize)]
    struct DisablePatch {
        id: ConnectionId,
        enabled: bool,
    }
    let content = toml::to_string_pretty(&DisableDocument {
        connections: vec![DisablePatch {
            id: connection_id.clone(),
            enabled: false,
        }],
    })
    .map_err(|source| CatalogError::TomlSerialize {
        source_name: provider_path.display().to_string(),
        source,
    })?;
    atomic_write(&provider_path, &content)?;

    let validation = load_catalog_from_paths_unlocked(&paths).and_then(|catalog| {
        if catalog.connections().is_empty() {
            return Err(CatalogError::InvalidLocalCatalogInput {
                message: format!("disabling `{connection_id}` would leave no enabled providers"),
            });
        }
        Ok(())
    });
    if let Err(err) = validation {
        cleanup_created_local_catalog_files(&provider_path, &provider_path);
        return Err(err);
    }
    Ok(provider_path)
}

/// True when the user file is exactly the disable patch this module writes:
/// one connection entry for `connection_id` with only `id` and `enabled` keys.
fn is_pure_disable_patch(path: &Path, connection_id: &ConnectionId) -> Result<bool, CatalogError> {
    let table = read_toml_table(path)?;
    let Some(connections) = table.get("connections").and_then(toml::Value::as_array) else {
        return Ok(false);
    };
    if table.len() != 1 || connections.len() != 1 {
        return Ok(false);
    }
    let Some(entry) = connections[0].as_table() else {
        return Ok(false);
    };
    Ok(entry.len() == 2
        && entry.get("id").and_then(toml::Value::as_str) == Some(connection_id.as_str())
        && entry.get("enabled").and_then(toml::Value::as_bool) == Some(false))
}

/// Refuse to manage files that define connections or targets beyond the given
/// id — hand-authored multi-entry files are edited manually, not through the
/// provider manager.
fn ensure_file_scoped_to_connection(
    provider_path: &Path,
    model_path: &Path,
    connection_id: &ConnectionId,
) -> Result<(), CatalogError> {
    let provider_table = read_toml_table(provider_path)?;
    let foreign_connection = provider_table
        .get("connections")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .any(|entry| entry.get("id").and_then(toml::Value::as_str) != Some(connection_id.as_str()));
    if foreign_connection {
        return Err(CatalogError::SharedCatalogFile {
            path: provider_path.to_path_buf(),
            connection_id: connection_id.clone(),
        });
    }
    if model_path.exists() {
        let model_table = read_toml_table(model_path)?;
        let foreign_target = model_table
            .get("targets")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .any(|entry| {
                entry.get("connection").and_then(toml::Value::as_str)
                    != Some(connection_id.as_str())
            });
        if foreign_target {
            return Err(CatalogError::SharedCatalogFile {
                path: model_path.to_path_buf(),
                connection_id: connection_id.clone(),
            });
        }
    }
    Ok(())
}

fn read_toml_table(path: &Path) -> Result<toml::Table, CatalogError> {
    let content = fs::read_to_string(path).map_err(|source| CatalogError::ReadFile {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&content).map_err(|source| CatalogError::Toml {
        source_name: path.display().to_string(),
        source,
    })
}

fn validate_local_catalog_input(input: &LocalCatalogEntryInput) -> Result<(), CatalogError> {
    if input.connection.display_name.trim().is_empty() {
        return Err(CatalogError::InvalidLocalCatalogInput {
            message: "provider display name is required".to_string(),
        });
    }
    if input.connection.base_url.trim().is_empty() {
        return Err(CatalogError::InvalidLocalCatalogInput {
            message: "base URL is required".to_string(),
        });
    }
    if input.targets.is_empty() {
        return Err(CatalogError::InvalidLocalCatalogInput {
            message: "at least one model target is required".to_string(),
        });
    }
    for target in &input.targets {
        if target.remote_model.trim().is_empty() {
            return Err(CatalogError::InvalidLocalCatalogInput {
                message: "model id is required".to_string(),
            });
        }
        if target.context_window == Some(0) {
            return Err(CatalogError::InvalidLocalCatalogInput {
                message: format!(
                    "context window for `{}` must be greater than zero",
                    target.remote_model
                ),
            });
        }
        if target.output_limit == Some(0) {
            return Err(CatalogError::InvalidLocalCatalogInput {
                message: format!(
                    "output limit for `{}` must be greater than zero",
                    target.remote_model
                ),
            });
        }
    }
    Ok(())
}

fn refuse_existing_local_catalog_file(path: &Path) -> Result<(), CatalogError> {
    if path.exists() {
        return Err(CatalogError::LocalCatalogFileExists {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn refuse_existing_catalog_ids(
    paths: &CatalogPaths,
    input: &LocalCatalogEntryInput,
) -> Result<(), CatalogError> {
    let spec = load_catalog_with_user_dirs(&paths.provider_dir, &paths.model_dir)?;
    if spec.connections.iter().any(|connection| {
        connection
            .id
            .as_str()
            .eq_ignore_ascii_case(input.connection.id.as_str())
    }) {
        return Err(CatalogError::DuplicateConnection {
            id: input.connection.id.clone(),
        });
    }
    let target_rows = local_target_rows(input)?;
    for target in target_rows {
        if spec.targets.iter().any(|existing| {
            existing
                .connection
                .as_str()
                .eq_ignore_ascii_case(input.connection.id.as_str())
                && existing
                    .model
                    .as_str()
                    .eq_ignore_ascii_case(target.model.as_str())
        }) {
            return Err(CatalogError::DuplicateTarget {
                connection_id: input.connection.id.clone(),
                model_id: target.model,
            });
        }
    }
    Ok(())
}

fn local_target_rows(input: &LocalCatalogEntryInput) -> Result<Vec<LocalTargetToml>, CatalogError> {
    let mut rows = Vec::with_capacity(input.targets.len());
    let mut seen_slugs = HashMap::<String, usize>::new();
    for (index, target) in input.targets.iter().enumerate() {
        let base_slug = slugify_remote_model_id(&target.remote_model);
        let count = seen_slugs.entry(base_slug.clone()).or_insert(0);
        *count += 1;
        let slug = if *count == 1 {
            base_slug
        } else {
            format!("{base_slug}-{count}")
        };
        let model = format!("{}/{}", input.connection.id.as_str(), slug)
            .parse::<ModelId>()
            .map_err(|source| CatalogError::InvalidModelsDevModelId {
                source_name: "local catalog writer".to_string(),
                provider_id: input.connection.id.to_string(),
                model_id: target.remote_model.clone(),
                source,
            })?;
        let features = if target.tool_call {
            vec![ModelFeature::ToolCall]
        } else {
            Vec::new()
        };
        rows.push(LocalTargetToml {
            connection: input.connection.id.clone(),
            enabled: true,
            model,
            remote_model: target.remote_model.trim().to_string(),
            display_name: target.display_name.clone(),
            is_default: index == 0,
            context_window: target.context_window,
            output_limit: target.output_limit,
            token_counter: TokenCounterKind::Heuristic,
            features,
        });
    }
    Ok(rows)
}

pub(crate) fn slugify_remote_model_id(remote_model: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for ch in remote_model.trim().chars() {
        let next = if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            Some(ch.to_ascii_lowercase())
        } else {
            Some('-')
        };
        if let Some(ch) = next {
            if ch == '-' {
                if !previous_dash && !slug.is_empty() {
                    slug.push(ch);
                }
                previous_dash = true;
            } else {
                slug.push(ch);
                previous_dash = false;
            }
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        "model".to_string()
    } else {
        slug
    }
}

fn env_prefix_for_connection_id(connection_id: &ConnectionId) -> String {
    let mut suffix = String::new();
    let mut previous_underscore = false;
    for ch in connection_id.as_str().chars() {
        let next = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_uppercase()
        } else {
            '_'
        };
        if next == '_' {
            if !previous_underscore && !suffix.is_empty() {
                suffix.push(next);
            }
            previous_underscore = true;
        } else {
            suffix.push(next);
            previous_underscore = false;
        }
    }
    while suffix.ends_with('_') {
        suffix.pop();
    }
    if suffix.is_empty() {
        "BONSAI_LOCAL".to_string()
    } else {
        format!("BONSAI_{suffix}")
    }
}

fn default_endpoint_path_for_transport(transport: TransportProtocol) -> &'static str {
    match transport {
        TransportProtocol::OpenAiChat => "chat/completions",
        TransportProtocol::AnthropicMessages => "v1/messages",
        TransportProtocol::CodexResponses => "responses",
    }
}

fn cleanup_created_local_catalog_files(provider_path: &Path, model_path: &Path) {
    for path in [provider_path, model_path] {
        if let Err(err) = fs::remove_file(path)
            && err.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                "failed to clean up invalid local catalog file"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection_id(value: &str) -> ConnectionId {
        value.parse().unwrap()
    }

    fn model_id(value: &str) -> ModelId {
        value.parse().unwrap()
    }

    #[test]
    fn local_catalog_writer_persists_env_names_and_roundtrips() {
        let home = tempfile::TempDir::new().unwrap();
        let input = LocalCatalogEntryInput {
            connection: LocalCatalogConnectionInput {
                id: connection_id("my-local"),
                display_name: "My Local".to_string(),
                transport: TransportProtocol::OpenAiChat,
                base_url: "http://localhost:11434/v1/".to_string(),
                discovery: Default::default(),
            },
            targets: vec![LocalCatalogTargetInput {
                remote_model: "org/model:v1".to_string(),
                display_name: None,
                context_window: Some(131_072),
                output_limit: Some(8192),
                tool_call: true,
            }],
        };

        let report = write_local_catalog_entry(home.path(), input).unwrap();

        assert_eq!(report.connection_id.as_str(), "my-local");
        assert_eq!(report.model_ids[0].as_str(), "my-local/org-model-v1");
        let provider_content = fs::read_to_string(&report.provider_path).unwrap();
        assert!(provider_content.contains("id = \"my-local\""));
        assert!(provider_content.contains("auth = \"optional-api-key\""));
        assert!(provider_content.contains("api_key_env = \"BONSAI_MY_LOCAL_API_KEY\""));
        assert!(provider_content.contains("default_model = \"my-local/org-model-v1\""));
        assert!(
            provider_content.contains("prompt_cache = true"),
            "local providers should opt into prompt caching: {provider_content}"
        );
        assert!(!provider_content.contains("secret"));
        let model_content = fs::read_to_string(&report.model_path).unwrap();
        assert!(model_content.contains("model = \"my-local/org-model-v1\""));
        assert!(model_content.contains("remote_model = \"org/model:v1\""));
        assert!(model_content.contains("features = [\"tool-call\"]"));

        let catalog = load_catalog_from_home(home.path()).unwrap();
        let resolved = catalog
            .resolve(
                &connection_id("my-local"),
                &model_id("my-local/org-model-v1"),
            )
            .unwrap();
        assert_eq!(resolved.remote_model_id.as_ref(), "org/model:v1");
        assert_eq!(resolved.context_window, Some(131_072));
        assert_eq!(resolved.output_limit, Some(8192));
        assert_eq!(resolved.endpoint_path.as_deref(), Some("chat/completions"));
        assert!(resolved.features.contains(&ModelFeature::ToolCall));
    }

    #[test]
    fn local_catalog_writer_refuses_existing_files() {
        let home = tempfile::TempDir::new().unwrap();
        let input = LocalCatalogEntryInput {
            connection: LocalCatalogConnectionInput {
                id: connection_id("duplicate-local"),
                display_name: "Duplicate Local".to_string(),
                transport: TransportProtocol::AnthropicMessages,
                base_url: "http://localhost:4000".to_string(),
                discovery: Default::default(),
            },
            targets: vec![LocalCatalogTargetInput {
                remote_model: "claude-local".to_string(),
                display_name: None,
                context_window: Some(200_000),
                output_limit: None,
                tool_call: true,
            }],
        };

        write_local_catalog_entry(home.path(), input.clone()).unwrap();
        let err = write_local_catalog_entry(home.path(), input).unwrap_err();

        assert!(matches!(err, CatalogError::LocalCatalogFileExists { .. }));
    }

    #[test]
    fn replace_local_catalog_entry_rewrites_existing_provider() {
        let home = tempfile::TempDir::new().unwrap();
        let mut input = LocalCatalogEntryInput {
            connection: LocalCatalogConnectionInput {
                id: connection_id("my-local"),
                display_name: "My Local".to_string(),
                transport: TransportProtocol::OpenAiChat,
                base_url: "http://localhost:11434/v1".to_string(),
                discovery: Default::default(),
            },
            targets: vec![LocalCatalogTargetInput {
                remote_model: "old-model".to_string(),
                display_name: None,
                context_window: Some(8_192),
                output_limit: None,
                tool_call: true,
            }],
        };
        write_local_catalog_entry(home.path(), input.clone()).unwrap();

        input.connection.display_name = "My Local Edited".to_string();
        input.targets[0].remote_model = "new-model".to_string();
        input.targets[0].context_window = Some(65_536);
        let report = replace_local_catalog_entry(home.path(), input).unwrap();

        assert_eq!(report.model_ids[0].as_str(), "my-local/new-model");
        let provider_content = fs::read_to_string(&report.provider_path).unwrap();
        assert!(provider_content.contains("display_name = \"My Local Edited\""));
        let model_content = fs::read_to_string(&report.model_path).unwrap();
        assert!(model_content.contains("remote_model = \"new-model\""));
        assert!(!model_content.contains("old-model"));
        assert!(
            !report.provider_path.with_extension("toml.bak").exists(),
            "successful replace should discard the backup"
        );

        let catalog = load_catalog_from_home(home.path()).unwrap();
        let resolved = catalog
            .resolve(&connection_id("my-local"), &model_id("my-local/new-model"))
            .unwrap();
        assert_eq!(resolved.context_window, Some(65_536));
    }

    #[test]
    fn replace_local_catalog_entry_restores_previous_files_on_failure() {
        let home = tempfile::TempDir::new().unwrap();
        let input = LocalCatalogEntryInput {
            connection: LocalCatalogConnectionInput {
                id: connection_id("my-local"),
                display_name: "My Local".to_string(),
                transport: TransportProtocol::OpenAiChat,
                base_url: "http://localhost:11434/v1".to_string(),
                discovery: Default::default(),
            },
            targets: vec![LocalCatalogTargetInput {
                remote_model: "old-model".to_string(),
                display_name: None,
                context_window: Some(8_192),
                output_limit: None,
                tool_call: true,
            }],
        };
        let original = write_local_catalog_entry(home.path(), input.clone()).unwrap();
        let original_provider = fs::read_to_string(&original.provider_path).unwrap();

        // A corrupt sibling file makes the catalog reload fail after the
        // existing entry has been set aside, exercising the restore path.
        fs::write(
            original.provider_path.with_file_name("broken.toml"),
            "not [valid toml",
        )
        .unwrap();
        let mut edited = input;
        edited.connection.display_name = "Edited".to_string();
        replace_local_catalog_entry(home.path(), edited).unwrap_err();

        assert_eq!(
            fs::read_to_string(&original.provider_path).unwrap(),
            original_provider,
            "failed replace must leave the previous provider file intact"
        );
        assert!(
            !original.provider_path.with_extension("toml.bak").exists(),
            "restore should consume the backup"
        );
    }

    #[test]
    fn local_catalog_writer_refuses_case_insensitive_provider_id_collisions() {
        let home = tempfile::TempDir::new().unwrap();
        let input = LocalCatalogEntryInput {
            connection: LocalCatalogConnectionInput {
                id: connection_id("Anthropic"),
                display_name: "Local Anthropic".to_string(),
                transport: TransportProtocol::AnthropicMessages,
                base_url: "http://localhost:4000".to_string(),
                discovery: Default::default(),
            },
            targets: vec![LocalCatalogTargetInput {
                remote_model: "claude-local".to_string(),
                display_name: None,
                context_window: Some(200_000),
                output_limit: None,
                tool_call: true,
            }],
        };

        let err = write_local_catalog_entry(home.path(), input).unwrap_err();

        assert!(
            matches!(err, CatalogError::DuplicateConnection { id } if id.as_str() == "Anthropic")
        );
    }

    fn sample_input(id: &str) -> LocalCatalogEntryInput {
        LocalCatalogEntryInput {
            connection: LocalCatalogConnectionInput {
                id: connection_id(id),
                display_name: "Sample".to_string(),
                transport: TransportProtocol::OpenAiChat,
                base_url: "http://localhost:1234/v1".to_string(),
                discovery: DiscoveryKind::LmStudio,
            },
            targets: vec![LocalCatalogTargetInput {
                remote_model: "sample-model".to_string(),
                display_name: Some("Sample Model".to_string()),
                context_window: None,
                output_limit: None,
                tool_call: true,
            }],
        }
    }

    #[test]
    fn writer_persists_discovery_and_target_display_name() {
        let home = tempfile::TempDir::new().unwrap();

        let report = write_local_catalog_entry(home.path(), sample_input("lm-local")).unwrap();

        let provider_content = fs::read_to_string(&report.provider_path).unwrap();
        assert!(
            provider_content.contains("discovery = \"lm-studio\""),
            "{provider_content}"
        );
        let model_content = fs::read_to_string(&report.model_path).unwrap();
        assert!(
            model_content.contains("display_name = \"Sample Model\""),
            "{model_content}"
        );
        assert!(
            !model_content.contains("context_window"),
            "unknown context window must not be fabricated: {model_content}"
        );

        let catalog = load_catalog_from_home(home.path()).unwrap();
        let connection = catalog.connection(&connection_id("lm-local")).unwrap();
        assert_eq!(connection.discovery, DiscoveryKind::LmStudio);
        let resolved = catalog
            .resolve(
                &connection_id("lm-local"),
                &model_id("lm-local/sample-model"),
            )
            .unwrap();
        assert_eq!(resolved.display_name.as_ref(), "Sample Model");
    }

    #[test]
    fn remove_local_catalog_entry_deletes_wizard_files() {
        let home = tempfile::TempDir::new().unwrap();
        let report = write_local_catalog_entry(home.path(), sample_input("lm-local")).unwrap();

        let removed = remove_local_catalog_entry(home.path(), &connection_id("lm-local")).unwrap();

        assert_eq!(removed.removed.len(), 2);
        assert!(!report.provider_path.exists());
        assert!(!report.model_path.exists());
        let catalog = load_catalog_from_home(home.path()).unwrap();
        assert!(catalog.connection(&connection_id("lm-local")).is_none());
    }

    #[test]
    fn remove_local_catalog_entry_refuses_builtins_and_shared_files() {
        let home = tempfile::TempDir::new().unwrap();
        ensure_user_catalog_scaffold(&CatalogPaths::from_home_dir(home.path())).unwrap();

        // Built-in (no user file).
        let err = remove_local_catalog_entry(home.path(), &connection_id("opencode")).unwrap_err();
        assert!(matches!(err, CatalogError::NotUserManagedConnection { .. }));

        // Hand-authored file defining a foreign connection alongside.
        let paths = CatalogPaths::from_home_dir(home.path());
        fs::write(
            paths.provider_dir.join("multi.toml"),
            r#"
[[connections]]
id = "multi"
display_name = "Multi"
auth = "optional-api-key"
transport = "openai-chat"
default_base_url = "http://localhost:9999/v1"

[[connections]]
id = "other"
display_name = "Other"
auth = "optional-api-key"
transport = "openai-chat"
default_base_url = "http://localhost:9998/v1"
"#,
        )
        .unwrap();
        let err = remove_local_catalog_entry(home.path(), &connection_id("multi")).unwrap_err();
        assert!(matches!(err, CatalogError::SharedCatalogFile { .. }));
    }

    #[test]
    fn builtin_disable_writes_patch_and_enable_removes_it() {
        let home = tempfile::TempDir::new().unwrap();

        let path =
            set_builtin_connection_enabled(home.path(), &connection_id("codex"), false).unwrap();

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("enabled = false"), "{content}");
        let catalog = load_catalog_from_home(home.path()).unwrap();
        assert!(catalog.connection(&connection_id("codex")).is_none());
        // Idempotent disable.
        set_builtin_connection_enabled(home.path(), &connection_id("codex"), false).unwrap();

        set_builtin_connection_enabled(home.path(), &connection_id("codex"), true).unwrap();
        assert!(!path.exists());
        let catalog = load_catalog_from_home(home.path()).unwrap();
        assert!(catalog.connection(&connection_id("codex")).is_some());
    }

    #[test]
    fn remove_local_catalog_entry_refuses_builtin_disable_patch() {
        let home = tempfile::TempDir::new().unwrap();
        let path =
            set_builtin_connection_enabled(home.path(), &connection_id("codex"), false).unwrap();

        let err = remove_local_catalog_entry(home.path(), &connection_id("codex")).unwrap_err();

        assert!(matches!(err, CatalogError::NotUserManagedConnection { .. }));
        assert!(path.exists(), "remove must not delete the disable patch");
        let catalog = load_catalog_from_home(home.path()).unwrap();
        assert!(catalog.connection(&connection_id("codex")).is_none());
    }

    #[test]
    fn builtin_disable_refuses_custom_provider_files() {
        let home = tempfile::TempDir::new().unwrap();
        write_local_catalog_entry(home.path(), sample_input("lm-local")).unwrap();

        let err = set_builtin_connection_enabled(home.path(), &connection_id("lm-local"), false)
            .unwrap_err();

        assert!(matches!(err, CatalogError::SharedCatalogFile { .. }));
    }

    #[test]
    fn local_catalog_writer_deduplicates_slugged_model_ids() {
        let home = tempfile::TempDir::new().unwrap();
        let input = LocalCatalogEntryInput {
            connection: LocalCatalogConnectionInput {
                id: connection_id("local-slugs"),
                display_name: "Local Slugs".to_string(),
                transport: TransportProtocol::OpenAiChat,
                base_url: "localhost:11434/v1".to_string(),
                discovery: Default::default(),
            },
            targets: vec![
                LocalCatalogTargetInput {
                    remote_model: "team/model".to_string(),
                    display_name: None,
                    context_window: Some(4096),
                    output_limit: None,
                    tool_call: true,
                },
                LocalCatalogTargetInput {
                    remote_model: "team:model".to_string(),
                    display_name: None,
                    context_window: Some(4096),
                    output_limit: None,
                    tool_call: false,
                },
            ],
        };

        let report = write_local_catalog_entry(home.path(), input).unwrap();

        assert_eq!(
            report
                .model_ids
                .iter()
                .map(ModelId::as_str)
                .collect::<Vec<_>>(),
            vec!["local-slugs/team-model", "local-slugs/team-model-2"]
        );
    }
}
