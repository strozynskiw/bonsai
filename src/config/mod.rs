//! Layered `.bonsai/config.toml` loading — the substrate the extension
//! runtime, MCP client, and hooks engine build on. A
//! user extends bonsai by editing config, never by touching source.
//!
//! Precedence, highest first: `CLI > env > .env > project > global >
//! defaults`. `.env` is loaded by `dotenvy::dotenv()` before this module ever
//! runs (`src/main.rs`) and dotenvy never overwrites a pre-set process env
//! var, so "env > .env" falls out for free — nothing to build here. The CLI
//! layer (a `--config <path>` flag) is not wired up yet; only the env
//! override (`BONSAI_CONFIG`) and the file layers are implemented.
//!
//! [`load`] is infallible: a missing file is not a diagnostic, and a
//! malformed *entry* (one bad `[mcp.servers.x]`, one bad `[[hooks]]`) yields a
//! [`ConfigDiagnostic`] naming the entry and field while the rest of the file
//! still loads. Only a file that fails to parse as TOML at all, or the legacy
//! global-config collision, drops that whole layer.

mod merge;
pub(crate) mod schema;
mod validate;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[cfg(test)]
pub(crate) use schema::HookMatcher;
pub(crate) use schema::{
    BatchingPolicy, Capability, DeclaredCapabilities, FailureBehavior, HookAction, HookDef,
    HookEvent, McpServerConfig, McpTransportConfig, UpdateMode,
};

use schema::ConfigFile;

/// Schema version this build understands. A file declaring a higher version
/// still loads (best-effort) with a diagnostic — future migrations match on
/// the file's own `schema_version`.
const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Where a value or extension definition came from — powers provenance in
/// `/config`, `/mcp`, `/hooks`. File-layer analog of
/// [`crate::resource::discovery::Provenance`]. `Cli` (a `--config` flag) is a
/// reserved future variant per the module doc's precedence table; add it once
/// that flag exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigSource {
    Global,
    Project,
    /// The project layer was redirected by `BONSAI_CONFIG`.
    Env,
}

impl ConfigSource {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Env => "env (BONSAI_CONFIG)",
        }
    }
}

/// A non-fatal load problem. Never produced for a merely-missing file.
#[derive(Debug, Clone)]
pub(crate) struct ConfigDiagnostic {
    pub source: ConfigSource,
    pub path: PathBuf,
    /// Names the extension/entry, e.g. `"mcp.servers.context7"`,
    /// `"hooks[2] (cargo-fmt)"`, `"sandbox"`, `"[skills]"`.
    pub scope: String,
    /// Names the field where possible, e.g. `` `url` is required when
    /// transport = "http" ``.
    pub message: String,
}

impl std::fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({}, {}): {}",
            self.scope,
            self.source.label(),
            self.path.display(),
            self.message
        )
    }
}

/// Where each layer was resolved from, for `/config`'s per-key file paths and
/// `/config edit`.
#[derive(Debug, Clone)]
pub(crate) struct ConfigLayers {
    pub project_root: PathBuf,
    pub bonsai_home: PathBuf,
    pub global_path: PathBuf,
    /// The path actually read for the project layer — the project
    /// `.bonsai/config.toml` unless redirected.
    pub project_path: PathBuf,
    pub project_path_source: ConfigSource,
}

impl Default for ConfigLayers {
    fn default() -> Self {
        Self {
            project_root: PathBuf::new(),
            bonsai_home: PathBuf::new(),
            global_path: PathBuf::new(),
            project_path: PathBuf::new(),
            project_path_source: ConfigSource::Project,
        }
    }
}

/// Configured MCP servers keyed by name, each tagged with the layer that won.
pub(crate) type McpServerMap = BTreeMap<String, (McpServerConfig, ConfigSource)>;
/// Configured hooks in fire order, each tagged with the layer that won.
pub(crate) type HookList = Vec<(HookDef, ConfigSource)>;

/// Merged verification overrides. `None` means manifest detection remains active;
/// an explicitly empty command list disables that lane.
#[derive(Debug, Clone, Default)]
pub(crate) struct VerificationConfig {
    pub test: Option<Vec<String>>,
    pub build: Option<Vec<String>>,
    pub after_edit: crate::verification::VerifyAfterEdit,
}

/// The merged, validated runtime view of config.
#[derive(Debug, Clone, Default)]
pub(crate) struct Config {
    pub mcp_servers: McpServerMap,
    pub hooks: HookList,
    pub sandbox: SandboxConfig,
    pub verification: VerificationConfig,
    pub update: UpdateConfig,
    pub diagnostics: Vec<ConfigDiagnostic>,
    pub layers: ConfigLayers,
}

impl Config {
    /// No config loaded — the default for tests and any path that never calls
    /// [`load`] (evals, unit tests constructing an `Agent` directly).
    pub(crate) fn empty() -> Self {
        Self::default()
    }
}

/// The merged `[sandbox]` view: `writable_roots` is a union across layers
/// (additive, like the env var it absorbs); `deny_network` is a genuine
/// scalar where the higher-precedence layer's `Some` wins.
#[derive(Debug, Clone, Default)]
pub(crate) struct SandboxConfig {
    pub writable_roots: Vec<PathBuf>,
    pub deny_network: Option<bool>,
}

/// The merged `[update]` view consumed by the self-updater (`src/update.rs`).
/// `pin` caps how far auto-install may go: a signed release newer than the pin
/// is surfaced but never installed (the deliberate-downgrade escape hatch).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct UpdateConfig {
    pub mode: UpdateMode,
    pub pin: Option<semver::Version>,
}

/// Load and merge every layer. `project_root`/`bonsai_home` are resolved by
/// the caller (project cwd; `SessionStore::bonsai_home_dir`-style resolution)
/// so this stays pure and testable.
pub(crate) fn load(project_root: &Path, bonsai_home: &Path) -> Config {
    let env_override = std::env::var_os("BONSAI_CONFIG")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    load_with_env_override(project_root, bonsai_home, env_override)
}

/// Load the global layer while keeping an untrusted repository's project
/// `.bonsai/config.toml` inert. An explicit `BONSAI_CONFIG` path remains
/// active: it is supplied through the user's process environment rather than
/// discovered from the repository.
pub(crate) fn load_without_project_config(project_root: &Path, bonsai_home: &Path) -> Config {
    let env_override = std::env::var_os("BONSAI_CONFIG")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty());
    load_with_env_override_and_project_trust(project_root, bonsai_home, env_override, false)
}

/// [`load`], with the `BONSAI_CONFIG` lookup passed in explicitly instead of
/// read from the real process environment — the test seam. `BONSAI_CONFIG` is
/// process-global, so mutating it from a test races every other test's
/// `load()` on other threads; this keeps `load()`'s public entry point simple
/// while letting tests exercise the override deterministically.
fn load_with_env_override(
    project_root: &Path,
    bonsai_home: &Path,
    env_override: Option<PathBuf>,
) -> Config {
    load_with_env_override_and_project_trust(project_root, bonsai_home, env_override, true)
}

fn load_with_env_override_and_project_trust(
    project_root: &Path,
    bonsai_home: &Path,
    env_override: Option<PathBuf>,
    trust_project_config: bool,
) -> Config {
    let mut diagnostics = Vec::new();

    let global_path = bonsai_home.join("config.toml");
    let global_file = read_layer(&global_path, ConfigSource::Global, &mut diagnostics);

    let (project_path, project_source) = match env_override {
        Some(path) => (path, ConfigSource::Env),
        None => (
            project_root.join(".bonsai").join("config.toml"),
            ConfigSource::Project,
        ),
    };
    let project_file = if trust_project_config || matches!(project_source, ConfigSource::Env) {
        read_layer(&project_path, project_source, &mut diagnostics)
    } else {
        ConfigFile::default()
    };

    let (mcp_servers, hooks, sandbox, verification, update) =
        merge::merge(&global_file, &project_file, project_source);

    Config {
        mcp_servers,
        hooks,
        sandbox,
        verification,
        update,
        diagnostics,
        layers: ConfigLayers {
            project_root: project_root.to_path_buf(),
            bonsai_home: bonsai_home.to_path_buf(),
            global_path,
            project_path,
            project_path_source: project_source,
        },
    }
}

fn read_layer(
    path: &Path,
    source: ConfigSource,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> ConfigFile {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let (file, mut file_diagnostics) = parse_config_file(&contents, source, path);
            diagnostics.append(&mut file_diagnostics);
            file
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
        Err(err) => {
            diagnostics.push(ConfigDiagnostic {
                source,
                path: path.to_path_buf(),
                scope: "<file>".to_string(),
                message: format!("failed to read: {err}"),
            });
            ConfigFile::default()
        }
    }
}

const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "schema_version",
    // Accepted for backward compatibility; reads are now always optimistic.
    "agent_read_isolation",
    "sandbox",
    "mcp",
    "hooks",
    "verification",
    "update",
];
const RESERVED_TOP_LEVEL_KEYS: &[&str] = &["skills", "commands", "providers"];

/// Parse one file's contents into a [`ConfigFile`], walking the raw TOML table
/// so a single malformed entry degrades to a diagnostic instead of failing the
/// whole file. See the module doc for the exact resilience contract.
fn parse_config_file(
    contents: &str,
    source: ConfigSource,
    path: &Path,
) -> (ConfigFile, Vec<ConfigDiagnostic>) {
    let mut diagnostics = Vec::new();

    let root: toml::Table = match toml::from_str(contents) {
        Ok(table) => table,
        Err(err) => {
            diagnostics.push(ConfigDiagnostic {
                source,
                path: path.to_path_buf(),
                scope: "<file>".to_string(),
                message: format!("failed to parse TOML: {err}"),
            });
            return (ConfigFile::default(), diagnostics);
        }
    };

    let schema_version = root
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        .map(|value| value.max(0) as u32)
        .unwrap_or(1);
    if schema_version > CURRENT_SCHEMA_VERSION {
        diagnostics.push(ConfigDiagnostic {
            source,
            path: path.to_path_buf(),
            scope: "schema_version".to_string(),
            message: format!(
                "schema_version {schema_version} was written by a newer bonsai (this build understands {CURRENT_SCHEMA_VERSION}); loading best-effort"
            ),
        });
    }

    // Legacy collision: `~/.bonsai/config.toml` was historically
    // api_key/model/base_url (`src/session/persistence.rs`). A file with
    // those keys and no `schema_version` predates this schema entirely —
    // treat the layer as empty rather than mis-parsing it; session auth
    // import is a separate path and keeps working unchanged.
    if source == ConfigSource::Global
        && !root.contains_key("schema_version")
        && root.contains_key("api_key")
        && root.contains_key("model")
    {
        diagnostics.push(ConfigDiagnostic {
            source,
            path: path.to_path_buf(),
            scope: "<file>".to_string(),
            message: "this looks like a legacy bonsai config (api_key/model, no schema_version); \
                      treating it as empty for the new config schema — provider auth import is unaffected"
                .to_string(),
        });
        return (ConfigFile::default(), diagnostics);
    }

    for key in root.keys() {
        if KNOWN_TOP_LEVEL_KEYS.contains(&key.as_str()) {
            continue;
        }
        let message = if RESERVED_TOP_LEVEL_KEYS.contains(&key.as_str()) {
            format!(
                "`{key}` is reserved for a future bonsai release and is not yet supported; ignoring"
            )
        } else {
            format!("unknown top-level key `{key}`; ignoring (forward compatibility)")
        };
        diagnostics.push(ConfigDiagnostic {
            source,
            path: path.to_path_buf(),
            scope: format!("[{key}]"),
            message,
        });
    }

    let sandbox = parse_section_lenient(&root, "sandbox", source, path, &mut diagnostics);
    let mcp_servers = parse_mcp_servers(&root, source, path, &mut diagnostics);
    let hooks = parse_hooks(&root, source, path, &mut diagnostics);
    let verification = parse_section_lenient(&root, "verification", source, path, &mut diagnostics);
    let update = parse_update(&root, source, path, &mut diagnostics);

    (
        ConfigFile {
            sandbox,
            mcp: schema::McpSection {
                servers: mcp_servers,
            },
            hooks,
            verification,
            update,
        },
        diagnostics,
    )
}

/// `[update]` with semver validation of `pin` at parse time, so the merge can
/// treat a surviving pin as always parseable.
fn parse_update(
    root: &toml::Table,
    source: ConfigSource,
    path: &Path,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> schema::UpdateSection {
    let mut update: schema::UpdateSection =
        parse_section_lenient(root, "update", source, path, diagnostics);
    if let Some(pin) = &update.pin
        && semver::Version::parse(pin).is_err()
    {
        diagnostics.push(ConfigDiagnostic {
            source,
            path: path.to_path_buf(),
            scope: "update".to_string(),
            message: format!("`pin` is not a valid semver version: `{pin}`; ignoring"),
        });
        update.pin = None;
    }
    update
}

/// Deserialize `root[key]` into `T`, defaulting (with a diagnostic) on
/// failure rather than aborting the whole file.
fn parse_section_lenient<T: serde::de::DeserializeOwned + Default>(
    root: &toml::Table,
    key: &str,
    source: ConfigSource,
    path: &Path,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> T {
    let Some(value) = root.get(key) else {
        return T::default();
    };
    match value.clone().try_into() {
        Ok(section) => section,
        Err(err) => {
            diagnostics.push(ConfigDiagnostic {
                source,
                path: path.to_path_buf(),
                scope: key.to_string(),
                message: err.to_string(),
            });
            T::default()
        }
    }
}

fn parse_mcp_servers(
    root: &toml::Table,
    source: ConfigSource,
    path: &Path,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> BTreeMap<String, McpServerConfig> {
    let mut servers = BTreeMap::new();
    let Some(mcp_value) = root.get("mcp") else {
        return servers;
    };
    let Some(mcp_table) = mcp_value.as_table() else {
        diagnostics.push(ConfigDiagnostic {
            source,
            path: path.to_path_buf(),
            scope: "mcp".to_string(),
            message: "`[mcp]` must be a table".to_string(),
        });
        return servers;
    };
    let Some(servers_value) = mcp_table.get("servers") else {
        return servers;
    };
    let Some(servers_table) = servers_value.as_table() else {
        diagnostics.push(ConfigDiagnostic {
            source,
            path: path.to_path_buf(),
            scope: "mcp.servers".to_string(),
            message: "`[mcp.servers]` must be a table of named servers".to_string(),
        });
        return servers;
    };

    for (name, value) in servers_table {
        let scope = format!("mcp.servers.{name}");
        if let Err(err) = validate::validate_extension_name(name) {
            diagnostics.push(ConfigDiagnostic {
                source,
                path: path.to_path_buf(),
                scope,
                message: err,
            });
            continue;
        }
        match value.clone().try_into::<McpServerConfig>() {
            Ok(config) => match validate::validate_mcp_server(&config) {
                Ok(()) => {
                    servers.insert(name.clone(), config);
                }
                Err(message) => diagnostics.push(ConfigDiagnostic {
                    source,
                    path: path.to_path_buf(),
                    scope,
                    message,
                }),
            },
            Err(err) => diagnostics.push(ConfigDiagnostic {
                source,
                path: path.to_path_buf(),
                scope,
                message: err.to_string(),
            }),
        }
    }
    servers
}

fn parse_hooks(
    root: &toml::Table,
    source: ConfigSource,
    path: &Path,
    diagnostics: &mut Vec<ConfigDiagnostic>,
) -> Vec<HookDef> {
    let mut hooks = Vec::new();
    let Some(hooks_value) = root.get("hooks") else {
        return hooks;
    };
    let Some(hooks_array) = hooks_value.as_array() else {
        diagnostics.push(ConfigDiagnostic {
            source,
            path: path.to_path_buf(),
            scope: "hooks".to_string(),
            message: "`hooks` must be an array of tables (`[[hooks]]`)".to_string(),
        });
        return hooks;
    };

    for (index, value) in hooks_array.iter().enumerate() {
        let name_hint = value
            .get("name")
            .and_then(toml::Value::as_str)
            .unwrap_or("?");
        let scope = format!("hooks[{index}] ({name_hint})");
        match value.clone().try_into::<HookDef>() {
            Ok(hook) => match validate::validate_hook(&hook) {
                Ok(()) => hooks.push(hook),
                Err(message) => diagnostics.push(ConfigDiagnostic {
                    source,
                    path: path.to_path_buf(),
                    scope,
                    message,
                }),
            },
            Err(err) => diagnostics.push(ConfigDiagnostic {
                source,
                path: path.to_path_buf(),
                scope,
                message: err.to_string(),
            }),
        }
    }
    hooks
}
