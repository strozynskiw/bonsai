use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::*;

#[derive(Debug, Clone)]
struct OwnedTomlSource {
    name: String,
    content: String,
}

pub(crate) fn load_builtin_catalog() -> Result<CatalogSpec, CatalogError> {
    let mut builder = CatalogBuilder::default();
    load_connection_sources(&mut builder, SourceKind::BuiltIn, &[BUILTIN_CONNECTIONS])?;
    load_target_sources(&mut builder, SourceKind::BuiltIn, BUILTIN_TARGETS)?;
    builder.finish()
}

pub(crate) fn load_catalog_with_user_dirs(
    provider_dir: &Path,
    model_dir: &Path,
) -> Result<CatalogSpec, CatalogError> {
    load_catalog_with_dirs(provider_dir, model_dir, None)
}

fn load_catalog_with_user_and_project_dirs(
    provider_dir: &Path,
    model_dir: &Path,
    project_provider_dir: &Path,
    project_model_dir: &Path,
) -> Result<CatalogSpec, CatalogError> {
    load_catalog_with_dirs(
        provider_dir,
        model_dir,
        Some((project_provider_dir, project_model_dir)),
    )
}

fn load_catalog_with_dirs(
    provider_dir: &Path,
    model_dir: &Path,
    project_dirs: Option<(&Path, &Path)>,
) -> Result<CatalogSpec, CatalogError> {
    let mut builder = CatalogBuilder::default();
    load_connection_sources(&mut builder, SourceKind::BuiltIn, &[BUILTIN_CONNECTIONS])?;
    load_target_sources(&mut builder, SourceKind::BuiltIn, BUILTIN_TARGETS)?;

    load_catalog_dir_layer(&mut builder, SourceKind::User, provider_dir, model_dir)?;
    if let Some((project_provider_dir, project_model_dir)) = project_dirs {
        load_catalog_dir_layer(
            &mut builder,
            SourceKind::Project,
            project_provider_dir,
            project_model_dir,
        )?;
    }

    builder.finish()
}

fn load_catalog_dir_layer(
    builder: &mut CatalogBuilder,
    source_kind: SourceKind,
    provider_dir: &Path,
    model_dir: &Path,
) -> Result<(), CatalogError> {
    for source in read_toml_sources(provider_dir)? {
        let document = parse_connection_document(&source.name, &source.content)?;
        for connection in document.connections {
            builder.add_connection_patch(&source.name, source_kind, connection)?;
        }
    }

    for source in read_toml_sources(model_dir)? {
        let document = parse_target_document(&source.name, &source.content)?;
        for target in document.targets {
            builder.add_target_patch(source_kind, target)?;
        }
    }
    Ok(())
}

pub(crate) fn load_catalog_with_user_dirs_and_models_dev(
    provider_dir: &Path,
    model_dir: &Path,
    models_dev_path: &Path,
) -> Result<CatalogSpec, CatalogError> {
    let spec = load_catalog_with_user_dirs(provider_dir, model_dir)?;
    let models_dev = load_models_dev_cache(models_dev_path)?;
    Ok(spec.with_models_dev(models_dev))
}

fn load_catalog_with_user_and_project_dirs_and_models_dev(
    provider_dir: &Path,
    model_dir: &Path,
    project_provider_dir: &Path,
    project_model_dir: &Path,
    models_dev_path: &Path,
) -> Result<CatalogSpec, CatalogError> {
    let spec = load_catalog_with_user_and_project_dirs(
        provider_dir,
        model_dir,
        project_provider_dir,
        project_model_dir,
    )?;
    let models_dev = load_models_dev_cache(models_dev_path)?;
    Ok(spec.with_models_dev(models_dev))
}

#[cfg(test)]
pub(crate) fn load_catalog_from_home(home_dir: &Path) -> Result<ModelCatalog, CatalogError> {
    load_catalog_from_home_and_project(home_dir, None)
}

/// Load the user catalog plus a project layer admitted by workspace trust.
///
/// Passing `None` keeps project-owned provider configuration inert.
pub(crate) fn load_catalog_from_home_and_project(
    home_dir: &Path,
    trusted_project_root: Option<&Path>,
) -> Result<ModelCatalog, CatalogError> {
    let paths = CatalogPaths::from_home_dir(home_dir);
    transaction::with_recovered_catalog_lock(home_dir, || {
        ensure_user_catalog_scaffold(&paths)?;
        load_catalog_from_paths_and_project_unlocked(&paths, trusted_project_root)
    })
}

/// Load the catalog after synchronously refreshing its Models.dev metadata.
///
/// Non-interactive entry points use this to ensure their one-shot run sees the
/// current cache. The TUI instead loads the cache immediately and refreshes it
/// in the background so opening the model picker never waits on the network.
pub(crate) async fn load_catalog_from_home_with_refresh(
    home_dir: &Path,
) -> Result<ModelCatalog, CatalogError> {
    load_catalog_from_home_and_project_with_refresh(home_dir, None).await
}

/// Refresh Models.dev metadata and load an optional trusted project layer.
pub(crate) async fn load_catalog_from_home_and_project_with_refresh(
    home_dir: &Path,
    trusted_project_root: Option<&Path>,
) -> Result<ModelCatalog, CatalogError> {
    let paths = CatalogPaths::from_home_dir(home_dir);
    transaction::with_recovered_catalog_lock(home_dir, || ensure_user_catalog_scaffold(&paths))?;
    let config = ModelsDevConfig::from_env(|var| std::env::var(var).ok());
    let models_dev = match refresh_models_dev_cache(&config, &paths.models_dev_cache_path).await {
        Ok(models_dev) => models_dev,
        Err(err) => {
            tracing::warn!(
                home = %home_dir.display(),
                error = %err,
                "failed to refresh Models.dev catalog; using cached model metadata"
            );
            let catalog = load_catalog_from_home_and_project(home_dir, trusted_project_root)?;
            catalog.record_models_dev_refresh_failure(&err);
            return Ok(catalog);
        }
    };
    transaction::with_recovered_catalog_lock(home_dir, || {
        ensure_user_catalog_scaffold(&paths)?;
        let spec =
            load_catalog_spec_from_paths(&paths, trusted_project_root)?.with_models_dev(models_dev);
        catalog_from_spec_and_paths(spec, &paths, trusted_project_root)
    })
}

/// [`load_catalog_from_home_and_project`], degrading to the built-in catalog
/// on any user-catalog load failure — interactive startup must never fail on
/// a broken user catalog file, only lose its customizations for the session.
pub(crate) fn load_catalog_with_builtin_fallback(
    home_dir: &Path,
    trusted_project_root: Option<&Path>,
) -> Result<ModelCatalog, CatalogError> {
    match load_catalog_from_home_and_project(home_dir, trusted_project_root) {
        Ok(catalog) => Ok(catalog),
        Err(err) => {
            tracing::warn!(
                home = %home_dir.display(),
                error = %err,
                "failed to load user model catalog; falling back to built-in catalog"
            );
            ModelCatalog::load_builtin()
        }
    }
}

/// Spawn the background Models.dev refresh interactive startup uses so the
/// model picker never waits on the network: replace the catalog's metadata on
/// success, record the failure on the catalog otherwise (surfaced by
/// `/refresh` and doctor rather than the terminal).
pub(crate) fn spawn_models_dev_refresh(home_dir: PathBuf, catalog: std::sync::Arc<ModelCatalog>) {
    tokio::spawn(async move {
        match refresh_models_dev_cache_from_home(&home_dir).await {
            Ok(models_dev) => catalog.replace_models_dev_metadata(models_dev),
            Err(err) => {
                catalog.record_models_dev_refresh_failure(&err);
                tracing::warn!(
                    home = %home_dir.display(),
                    error = %err,
                    "failed to refresh Models.dev catalog in the background"
                );
            }
        }
    });
}

/// Refresh the cached Models.dev snapshot and return its metadata.
pub(crate) async fn refresh_models_dev_cache_from_home(
    home_dir: &Path,
) -> Result<ModelsDevCatalog, CatalogError> {
    let paths = CatalogPaths::from_home_dir(home_dir);
    transaction::with_recovered_catalog_lock(home_dir, || ensure_user_catalog_scaffold(&paths))?;
    let config = ModelsDevConfig::from_env(|var| std::env::var(var).ok());
    refresh_models_dev_cache(&config, &paths.models_dev_cache_path).await
}

pub(super) fn load_catalog_from_paths_unlocked(
    paths: &CatalogPaths,
) -> Result<ModelCatalog, CatalogError> {
    load_catalog_from_paths_and_project_unlocked(paths, None)
}

fn load_catalog_from_paths_and_project_unlocked(
    paths: &CatalogPaths,
    trusted_project_root: Option<&Path>,
) -> Result<ModelCatalog, CatalogError> {
    let spec = load_catalog_spec_from_paths(paths, trusted_project_root)?;
    catalog_from_spec_and_paths(spec, paths, trusted_project_root)
}

fn load_catalog_spec_from_paths(
    paths: &CatalogPaths,
    trusted_project_root: Option<&Path>,
) -> Result<CatalogSpec, CatalogError> {
    let Some(project_root) = trusted_project_root else {
        return load_catalog_with_user_dirs_and_models_dev(
            &paths.provider_dir,
            &paths.model_dir,
            &paths.models_dev_cache_path,
        );
    };
    let project_provider_dir = project_root.join(".bonsai/providers");
    let project_model_dir = project_root.join(".bonsai/models");
    load_catalog_with_user_and_project_dirs_and_models_dev(
        &paths.provider_dir,
        &paths.model_dir,
        &project_provider_dir,
        &project_model_dir,
        &paths.models_dev_cache_path,
    )
}

fn catalog_from_spec_and_paths(
    spec: CatalogSpec,
    paths: &CatalogPaths,
    trusted_project_root: Option<&Path>,
) -> Result<ModelCatalog, CatalogError> {
    let live_availability = load_live_availability_dir(&paths.live_models_dir);
    Ok(ModelCatalog::from_spec_with_live_availability(
        spec,
        Some(paths.live_models_dir.clone()),
        Some(paths.home_dir.clone()),
        trusted_project_root.map(Path::to_path_buf),
        live_availability,
    ))
}

pub(crate) fn ensure_user_catalog_scaffold(paths: &CatalogPaths) -> Result<(), CatalogError> {
    ensure_dir(&paths.provider_dir)?;
    ensure_dir(&paths.model_dir)?;
    ensure_example_file(
        &paths.provider_dir.join(EXAMPLE_PROVIDER_FILE),
        EXAMPLE_PROVIDER_TOML,
    )?;
    ensure_example_file(
        &paths.model_dir.join(EXAMPLE_MODEL_FILE),
        EXAMPLE_MODEL_TOML,
    )?;
    Ok(())
}

fn ensure_dir(path: &Path) -> Result<(), CatalogError> {
    fs::create_dir_all(path).map_err(|source| CatalogError::CreateDir {
        path: path.to_path_buf(),
        source,
    })
}

fn ensure_example_file(path: &Path, content: &str) -> Result<(), CatalogError> {
    if path.exists() {
        return Ok(());
    }
    atomic_write(path, content)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogPaths {
    pub home_dir: PathBuf,
    pub provider_dir: PathBuf,
    pub model_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub models_dev_cache_path: PathBuf,
    pub live_models_dir: PathBuf,
}

impl CatalogPaths {
    pub(crate) fn from_home_dir(home_dir: &Path) -> Self {
        let cache_dir = home_dir.join("cache");
        Self {
            home_dir: home_dir.to_path_buf(),
            provider_dir: home_dir.join("providers"),
            model_dir: home_dir.join("models"),
            models_dev_cache_path: cache_dir.join(MODELS_DEV_CACHE_FILE),
            live_models_dir: cache_dir.join(LIVE_MODELS_CACHE_DIR),
            cache_dir,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ConnectionDocument {
    #[serde(default)]
    connections: Vec<ConnectionSpecPatch>,
}

#[derive(Debug, Deserialize)]
struct TargetDocument {
    #[serde(default)]
    targets: Vec<TargetSpecPatch>,
}

/// How long a fetched models.dev snapshot stays fresh before the next catalog
/// load refetches it. Model metadata changes on launch cadence, not per-hour,
/// so an hour keeps repeated startups network-free without going stale.
const DEFAULT_MODELS_DEV_REFRESH_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelsDevConfig {
    pub url: String,
    pub path_override: Option<PathBuf>,
    pub fetch_disabled: bool,
    /// Zero disables the freshness skip (every load refetches, the pre-TTL
    /// behavior); tests rely on that to exercise the network path.
    pub refresh_ttl: Duration,
}

impl ModelsDevConfig {
    pub(crate) fn from_env(mut value_for: impl FnMut(&str) -> Option<String>) -> Self {
        let url = value_for(MODELS_DEV_URL_ENV)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODELS_DEV_URL.to_string());
        let path_override = value_for(MODELS_DEV_PATH_ENV)
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);
        let fetch_disabled = value_for(DISABLE_MODELS_FETCH_ENV)
            .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
            .unwrap_or(false);
        let refresh_ttl = value_for(MODELS_DEV_TTL_ENV)
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_MODELS_DEV_REFRESH_TTL);

        Self {
            url,
            path_override,
            fetch_disabled,
            refresh_ttl,
        }
    }

    pub(crate) fn load_path<'a>(&'a self, default_cache_path: &'a Path) -> &'a Path {
        self.path_override.as_deref().unwrap_or(default_cache_path)
    }
}

pub(crate) fn load_models_dev_cache(path: &Path) -> Result<ModelsDevCatalog, CatalogError> {
    match fs::read_to_string(path) {
        Ok(content) => parse_models_dev_catalog(&path.display().to_string(), &content),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(ModelsDevCatalog::default()),
        Err(err) => Err(CatalogError::ReadFile {
            path: path.to_path_buf(),
            source: err,
        }),
    }
}

#[cfg(test)]
pub(crate) fn write_models_dev_cache(path: &Path, content: &str) -> Result<(), CatalogError> {
    parse_models_dev_catalog(&path.display().to_string(), content)?;
    atomic_write(path, content)
}

pub(crate) async fn refresh_models_dev_cache(
    config: &ModelsDevConfig,
    default_cache_path: &Path,
) -> Result<ModelsDevCatalog, CatalogError> {
    let load_path = config.load_path(default_cache_path);
    if config.fetch_disabled || config.path_override.is_some() {
        return load_models_dev_cache(load_path);
    }

    // Serve a recent snapshot without a network round-trip. A cache that is
    // fresh by mtime but corrupt or empty falls through to a fetch.
    if models_dev_cache_is_fresh(default_cache_path, config.refresh_ttl)
        && let Ok(catalog) = load_models_dev_cache(default_cache_path)
        && !catalog.is_empty()
    {
        tracing::debug!(
            cache = %default_cache_path.display(),
            "models.dev cache is fresh; skipping refetch"
        );
        return Ok(catalog);
    }

    let content = fetch_models_dev_catalog(&reqwest::Client::new(), &config.url).await?;
    let catalog = parse_models_dev_catalog(&config.url, &content)?;
    atomic_write(default_cache_path, &content)?;
    Ok(catalog)
}

fn models_dev_cache_is_fresh(path: &Path, ttl: Duration) -> bool {
    if ttl.is_zero() {
        return false;
    }
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age < ttl)
}

#[cfg(test)]
pub(crate) fn load_catalog_sources(
    connection_sources: &[TomlSource],
    target_sources: &[TomlSource],
) -> Result<CatalogSpec, CatalogError> {
    let mut builder = CatalogBuilder::default();
    load_connection_sources(&mut builder, SourceKind::BuiltIn, connection_sources)?;
    load_target_sources(&mut builder, SourceKind::BuiltIn, target_sources)?;
    builder.finish()
}

fn load_connection_sources(
    builder: &mut CatalogBuilder,
    source_kind: SourceKind,
    sources: &[TomlSource<'_>],
) -> Result<(), CatalogError> {
    for source in sources {
        let document = parse_connection_document(source.name, source.content)?;
        for connection in document.connections {
            builder.add_connection_patch(source.name, source_kind, connection)?;
        }
    }
    Ok(())
}

fn load_target_sources(
    builder: &mut CatalogBuilder,
    source_kind: SourceKind,
    sources: &[TomlSource<'_>],
) -> Result<(), CatalogError> {
    for source in sources {
        let document = parse_target_document(source.name, source.content)?;
        for target in document.targets {
            builder.add_target_patch(source_kind, target)?;
        }
    }
    Ok(())
}

fn parse_connection_document(
    source_name: &str,
    content: &str,
) -> Result<ConnectionDocument, CatalogError> {
    toml::from_str(content).map_err(|err| CatalogError::Toml {
        source_name: source_name.to_string(),
        source: err,
    })
}

fn parse_target_document(source_name: &str, content: &str) -> Result<TargetDocument, CatalogError> {
    toml::from_str(content).map_err(|err| CatalogError::Toml {
        source_name: source_name.to_string(),
        source: err,
    })
}

async fn fetch_models_dev_catalog(
    client: &reqwest::Client,
    url: &str,
) -> Result<String, CatalogError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| CatalogError::ModelsDevFetch {
            url: url.to_string(),
            source: err,
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(CatalogError::ModelsDevHttpStatus {
            url: url.to_string(),
            status: status.as_u16(),
        });
    }
    response
        .text()
        .await
        .map_err(|err| CatalogError::ModelsDevFetch {
            url: url.to_string(),
            source: err,
        })
}

pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<(), CatalogError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| CatalogError::CreateDir {
            path: parent.to_path_buf(),
            source: err,
        })?;
    }

    let temp_path = temp_path_for(path);
    fs::write(&temp_path, content).map_err(|err| CatalogError::WriteFile {
        path: temp_path.clone(),
        source: err,
    })?;
    fs::rename(&temp_path, path).map_err(|err| CatalogError::RenameFile {
        path: path.to_path_buf(),
        temp_path,
        source: err,
    })
}

fn temp_path_for(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| MODELS_DEV_CACHE_FILE.into());
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!(
        ".{file_name}.tmp-{}-{timestamp}",
        std::process::id()
    ))
}

fn read_toml_sources(dir: &Path) -> Result<Vec<OwnedTomlSource>, CatalogError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(dir).map_err(|err| CatalogError::ReadDir {
        path: dir.to_path_buf(),
        source: err,
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| CatalogError::ReadDir {
            path: dir.to_path_buf(),
            source: err,
        })?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            paths.push(path);
        }
    }
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));

    paths
        .into_iter()
        .map(|path| {
            let content = fs::read_to_string(&path).map_err(|err| CatalogError::ReadFile {
                path: path.clone(),
                source: err,
            })?;
            Ok(OwnedTomlSource {
                name: path.display().to_string(),
                content,
            })
        })
        .collect()
}

fn load_live_availability_dir(dir: &Path) -> HashMap<ConnectionId, LiveModelAvailability> {
    if !dir.exists() {
        return HashMap::new();
    }

    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) => {
            tracing::warn!(
                path = %dir.display(),
                error = %err,
                "failed to read live model availability cache directory"
            );
            return HashMap::new();
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    paths.push(path);
                }
            }
            Err(err) => {
                tracing::warn!(
                    path = %dir.display(),
                    error = %err,
                    "failed to read live model availability cache entry"
                );
            }
        }
    }
    paths.sort_by(|left, right| left.file_name().cmp(&right.file_name()));

    let mut availability = HashMap::new();
    for path in paths {
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Ok(connection_id) = stem.parse::<ConnectionId>() else {
            tracing::warn!(
                path = %path.display(),
                "ignoring live model availability cache with invalid connection id"
            );
            continue;
        };
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to read live model availability cache"
                );
                continue;
            }
        };
        match serde_json::from_str::<LiveModelAvailability>(&content) {
            Ok(models) => {
                availability.insert(connection_id, models);
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "failed to parse live model availability cache"
                );
            }
        }
    }
    availability
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_id(value: &str) -> ModelId {
        value.parse().unwrap()
    }

    #[test]
    fn models_dev_cache_missing_is_empty_and_write_validates_before_replace() {
        let temp = tempfile::TempDir::new().unwrap();
        let cache_path = temp.path().join("cache").join("models-dev.json");

        assert!(load_models_dev_cache(&cache_path).unwrap().is_empty());

        let valid = r#"
            {
              "openai": {
                "models": {
                  "gpt-5": { "id": "gpt-5", "name": "GPT-5" }
                }
              }
            }
        "#;
        write_models_dev_cache(&cache_path, valid).unwrap();
        let first_write = std::fs::read_to_string(&cache_path).unwrap();
        let loaded = load_models_dev_cache(&cache_path).unwrap();
        assert_eq!(loaded.len(), 1);

        let result = write_models_dev_cache(&cache_path, "not json");

        assert!(matches!(result, Err(CatalogError::ModelsDevJson { .. })));
        assert_eq!(std::fs::read_to_string(&cache_path).unwrap(), first_write);
    }

    #[tokio::test]
    async fn refresh_models_dev_cache_fetches_and_writes_catalog() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"
                {
                  "openai": {
                    "models": {
                      "gpt-5": { "id": "gpt-5", "name": "GPT-5" }
                    }
                  }
                }
                "#,
            ))
            .mount(&server)
            .await;
        let temp = tempfile::TempDir::new().unwrap();
        let cache_path = temp.path().join("cache").join("models-dev.json");
        let config = ModelsDevConfig {
            url: format!("{}/api.json", server.uri()),
            path_override: None,
            fetch_disabled: false,
            refresh_ttl: Duration::ZERO,
        };

        let catalog = refresh_models_dev_cache(&config, &cache_path)
            .await
            .unwrap();

        assert_eq!(catalog.len(), 1);
        assert!(cache_path.exists());
        assert!(
            std::fs::read_to_string(&cache_path)
                .unwrap()
                .contains("GPT-5")
        );
    }

    #[tokio::test]
    async fn refresh_models_dev_cache_preserves_cache_on_http_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let temp = tempfile::TempDir::new().unwrap();
        let cache_path = temp.path().join("cache").join("models-dev.json");
        let valid = r#"
            {
              "openai": {
                "models": {
                  "gpt-5": { "id": "gpt-5", "name": "GPT-5" }
                }
              }
            }
        "#;
        write_models_dev_cache(&cache_path, valid).unwrap();
        let before = std::fs::read_to_string(&cache_path).unwrap();
        // Zero TTL forces the network path even though the cache was written
        // a moment ago.
        let config = ModelsDevConfig {
            url: format!("{}/api.json", server.uri()),
            path_override: None,
            fetch_disabled: false,
            refresh_ttl: Duration::ZERO,
        };

        let result = refresh_models_dev_cache(&config, &cache_path).await;

        assert!(matches!(
            result,
            Err(CatalogError::ModelsDevHttpStatus { status: 500, .. })
        ));
        assert_eq!(std::fs::read_to_string(&cache_path).unwrap(), before);
    }

    #[tokio::test]
    async fn refresh_models_dev_cache_skips_fetch_while_cache_is_fresh() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let temp = tempfile::TempDir::new().unwrap();
        let cache_path = temp.path().join("cache").join("models-dev.json");
        write_models_dev_cache(
            &cache_path,
            r#"
            {
              "openai": {
                "models": {
                  "gpt-5": { "id": "gpt-5", "name": "GPT-5 Cached" }
                }
              }
            }
            "#,
        )
        .unwrap();
        let config = ModelsDevConfig {
            url: format!("{}/api.json", server.uri()),
            path_override: None,
            fetch_disabled: false,
            refresh_ttl: Duration::from_secs(3600),
        };

        let catalog = refresh_models_dev_cache(&config, &cache_path)
            .await
            .unwrap();

        let model = catalog.model(&model_id("openai/gpt-5")).unwrap();
        assert_eq!(model.display_name.as_ref(), "GPT-5 Cached");
        // `.expect(0)` on the mock asserts no request was made on drop.
    }

    #[tokio::test]
    async fn refresh_models_dev_cache_fetches_when_fresh_cache_is_empty() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{ "openai": { "models": { "gpt-5": { "id": "gpt-5", "name": "GPT-5" } } } }"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        let temp = tempfile::TempDir::new().unwrap();
        let cache_path = temp.path().join("cache").join("models-dev.json");
        // Fresh by mtime but holds no models — must fall through to a fetch.
        write_models_dev_cache(&cache_path, "{}").unwrap();
        let config = ModelsDevConfig {
            url: format!("{}/api.json", server.uri()),
            path_override: None,
            fetch_disabled: false,
            refresh_ttl: Duration::from_secs(3600),
        };

        let catalog = refresh_models_dev_cache(&config, &cache_path)
            .await
            .unwrap();

        assert_eq!(catalog.len(), 1);
    }

    #[tokio::test]
    async fn refresh_models_dev_cache_uses_path_override_without_fetching() {
        let temp = tempfile::TempDir::new().unwrap();
        let override_path = temp.path().join("snapshot.json");
        let default_cache_path = temp.path().join("cache").join("models-dev.json");
        write_models_dev_cache(
            &override_path,
            r#"
            {
              "openai": {
                "models": {
                  "gpt-5": { "id": "gpt-5", "name": "GPT-5 Snapshot" }
                }
              }
            }
            "#,
        )
        .unwrap();
        let config = ModelsDevConfig {
            url: "http://127.0.0.1:9/api.json".to_string(),
            path_override: Some(override_path),
            fetch_disabled: false,
            refresh_ttl: Duration::ZERO,
        };

        let catalog = refresh_models_dev_cache(&config, &default_cache_path)
            .await
            .unwrap();

        let model = catalog.model(&model_id("openai/gpt-5")).unwrap();
        assert_eq!(model.display_name.as_ref(), "GPT-5 Snapshot");
        assert!(!default_cache_path.exists());
    }

    #[test]
    fn load_catalog_with_user_dirs_and_models_dev_uses_cache_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let provider_dir = temp.path().join("providers");
        let model_dir = temp.path().join("models");
        let cache_path = temp.path().join("cache").join("models-dev.json");
        write_models_dev_cache(
            &cache_path,
            r#"
            {
              "openai": {
                "models": {
                  "gpt-5": { "id": "gpt-5", "name": "GPT-5" }
                }
              }
            }
            "#,
        )
        .unwrap();

        let spec =
            load_catalog_with_user_dirs_and_models_dev(&provider_dir, &model_dir, &cache_path)
                .unwrap();

        assert!(spec.models_dev.model(&model_id("openai/gpt-5")).is_some());
    }

    #[test]
    fn models_dev_config_reads_env_values() {
        let config = ModelsDevConfig::from_env(|key| match key {
            MODELS_DEV_URL_ENV => Some("https://example.test/api.json".to_string()),
            MODELS_DEV_PATH_ENV => Some("/tmp/models-dev.json".to_string()),
            DISABLE_MODELS_FETCH_ENV => Some("1".to_string()),
            _ => None,
        });

        assert_eq!(config.url, "https://example.test/api.json");
        assert_eq!(
            config.path_override.as_deref(),
            Some(Path::new("/tmp/models-dev.json"))
        );
        assert!(config.fetch_disabled);
        assert_eq!(config.refresh_ttl, DEFAULT_MODELS_DEV_REFRESH_TTL);
        assert_eq!(
            config.load_path(Path::new("/default/models-dev.json")),
            Path::new("/tmp/models-dev.json")
        );
    }
}
