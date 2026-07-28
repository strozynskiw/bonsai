use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model_role::LegacyModelRole;
use crate::provider::{
    ModelPricing, ModelPricingSchedule, ParameterPreview, ProviderMetadata, ReasoningCodec,
    ReasoningEffort, ReasoningOption, ReasoningSelection, TokenCounterKind,
};

mod availability;
mod ids;
mod io;
mod local;
mod models_dev;
mod spec;
mod transaction;

pub(crate) use ids::*;
pub(crate) use io::*;
pub(crate) use local::*;
pub(crate) use models_dev::*;
pub(crate) use spec::*;

const MODELS_DEV_CACHE_FILE: &str = "models-dev.json";
const LIVE_MODELS_CACHE_DIR: &str = "live-models";
pub(crate) const MODELS_DEV_URL_ENV: &str = "BONSAI_MODELS_DEV_URL";
pub(crate) const MODELS_DEV_PATH_ENV: &str = "BONSAI_MODELS_DEV_PATH";
pub(crate) const DISABLE_MODELS_FETCH_ENV: &str = "BONSAI_DISABLE_MODELS_FETCH";
pub(crate) const MODELS_DEV_TTL_ENV: &str = "BONSAI_MODELS_DEV_TTL_SECS";
pub(crate) const DEFAULT_MODELS_DEV_URL: &str = "https://models.dev/api.json";

const BUILTIN_CONNECTIONS: TomlSource<'static> = TomlSource {
    name: "models/builtin/connections.toml",
    content: include_str!("../../models/builtin/connections.toml"),
};
const EXAMPLE_PROVIDER_FILE: &str = "example-local.toml";
const EXAMPLE_MODEL_FILE: &str = "example-local.toml";
const EXAMPLE_PROVIDER_TOML: &str = r#"# Example custom connector.
# Set enabled = true and edit ids, URLs, env vars, and display names.

[[connections]]
id = "local-example"
enabled = false
display_name = "Local Example"
auth = "optional-api-key"
transport = "openai-chat"
default_base_url = "http://localhost:11434/v1"
api_key_env = "LOCAL_EXAMPLE_API_KEY"
model_env = "LOCAL_EXAMPLE_MODEL"
base_url_env = "LOCAL_EXAMPLE_BASE_URL"
default_endpoint_path = "chat/completions"
default_token_counter = "heuristic"
# Send prompt-cache hints (`prompt_cache_key` on openai-chat,
# `cache_control` breakpoints on anthropic-messages). Local backends that
# cache prefixes benefit; others ignore the hint.
prompt_cache = true
"#;
const EXAMPLE_MODEL_TOML: &str = r#"# Example local/private model.
# Set enabled = true after enabling the matching connector.

[[targets]]
connection = "local-example"
enabled = false
model = "example-small"
remote_model = "example-small"
default = true
# Match the serving context size (llama.cpp --ctx-size / Ollama num_ctx),
# not the model card maximum — this drives compaction budgets.
context_window = 32768
output_limit = 4096
token_counter = "heuristic"
features = ["tool-call"]
"#;
const BUILTIN_TARGETS: &[TomlSource<'static>] = &[
    TomlSource {
        name: "models/builtin/anthropic.toml",
        content: include_str!("../../models/builtin/anthropic.toml"),
    },
    TomlSource {
        name: "models/builtin/codex.toml",
        content: include_str!("../../models/builtin/codex.toml"),
    },
    TomlSource {
        name: "models/builtin/kimi-coding-plan.toml",
        content: include_str!("../../models/builtin/kimi-coding-plan.toml"),
    },
    TomlSource {
        name: "models/builtin/minimax.toml",
        content: include_str!("../../models/builtin/minimax.toml"),
    },
    TomlSource {
        name: "models/builtin/minimax-coding-plan.toml",
        content: include_str!("../../models/builtin/minimax-coding-plan.toml"),
    },
    TomlSource {
        name: "models/builtin/mimo.toml",
        content: include_str!("../../models/builtin/mimo.toml"),
    },
    TomlSource {
        name: "models/builtin/mimo-coding-plan.toml",
        content: include_str!("../../models/builtin/mimo-coding-plan.toml"),
    },
    TomlSource {
        name: "models/builtin/moonshotai.toml",
        content: include_str!("../../models/builtin/moonshotai.toml"),
    },
    TomlSource {
        name: "models/builtin/openrouter.toml",
        content: include_str!("../../models/builtin/openrouter.toml"),
    },
    TomlSource {
        name: "models/builtin/openai.toml",
        content: include_str!("../../models/builtin/openai.toml"),
    },
    TomlSource {
        name: "models/builtin/opencode.toml",
        content: include_str!("../../models/builtin/opencode.toml"),
    },
    TomlSource {
        name: "models/builtin/opencode-zen.toml",
        content: include_str!("../../models/builtin/opencode-zen.toml"),
    },
    TomlSource {
        name: "models/builtin/zai.toml",
        content: include_str!("../../models/builtin/zai.toml"),
    },
    TomlSource {
        name: "models/builtin/zai-coding-plan.toml",
        content: include_str!("../../models/builtin/zai-coding-plan.toml"),
    },
    TomlSource {
        name: "models/builtin/deepseek.toml",
        content: include_str!("../../models/builtin/deepseek.toml"),
    },
    TomlSource {
        name: "models/builtin/qwencloud.toml",
        content: include_str!("../../models/builtin/qwencloud.toml"),
    },
    TomlSource {
        name: "models/builtin/qwencloud-token-plan.toml",
        content: include_str!("../../models/builtin/qwencloud-token-plan.toml"),
    },
    TomlSource {
        name: "models/builtin/gemini.toml",
        content: include_str!("../../models/builtin/gemini.toml"),
    },
    TomlSource {
        name: "models/builtin/xai.toml",
        content: include_str!("../../models/builtin/xai.toml"),
    },
    TomlSource {
        name: "models/builtin/mistral.toml",
        content: include_str!("../../models/builtin/mistral.toml"),
    },
    TomlSource {
        name: "models/builtin/tencent.toml",
        content: include_str!("../../models/builtin/tencent.toml"),
    },
];

#[derive(Debug, Clone, Copy)]
pub(crate) struct TomlSource<'a> {
    name: &'a str,
    content: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SourceKind {
    BuiltIn,
    User,
    Project,
}

#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    #[error("failed to parse {source_name}: {source}")]
    Toml {
        source_name: String,
        source: toml::de::Error,
    },
    #[error("failed to serialize catalog file `{source_name}`: {source}")]
    TomlSerialize {
        source_name: String,
        source: toml::ser::Error,
    },
    #[error("failed to parse Models.dev catalog `{source_name}`: {source}")]
    ModelsDevJson {
        source_name: String,
        source: serde_json::Error,
    },
    #[error("failed to serialize live model availability `{connection_id}`: {source}")]
    LiveAvailabilitySerialize {
        connection_id: ConnectionId,
        source: serde_json::Error,
    },
    #[error("failed to fetch Models.dev catalog `{url}`: {source}")]
    ModelsDevFetch { url: String, source: reqwest::Error },
    #[error("Models.dev catalog `{url}` returned HTTP {status}")]
    ModelsDevHttpStatus { url: String, status: u16 },
    #[error(
        "Models.dev catalog `{source_name}` provider `{provider_id}` has invalid model id `{model_id}`: {source}"
    )]
    InvalidModelsDevModelId {
        source_name: String,
        provider_id: String,
        model_id: String,
        source: IdError,
    },
    #[error("failed to read catalog directory `{path}`: {source}")]
    ReadDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to read catalog file `{path}`: {source}")]
    ReadFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to create catalog directory `{path}`: {source}")]
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to write catalog file `{path}`: {source}")]
    WriteFile {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to replace catalog file `{path}` with `{temp_path}`: {source}")]
    RenameFile {
        path: PathBuf,
        temp_path: PathBuf,
        source: std::io::Error,
    },
    #[error("failed to lock local catalog `{path}`: {source}")]
    CatalogLock {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("local catalog recovery journal `{path}` is invalid: {message}")]
    InvalidLocalCatalogJournal { path: PathBuf, message: String },
    #[error("local catalog entry is invalid: {message}")]
    InvalidLocalCatalogInput { message: String },
    #[error("local catalog file already exists: `{path}`")]
    LocalCatalogFileExists { path: PathBuf },
    #[error("provider `{connection_id}` has no user catalog file to remove")]
    NotUserManagedConnection { connection_id: ConnectionId },
    #[error(
        "catalog file `{path}` defines entries beyond `{connection_id}`; edit the file manually"
    )]
    SharedCatalogFile {
        path: PathBuf,
        connection_id: ConnectionId,
    },
    #[error("catalog file `{source_name}` connection `{id}` is missing required field `{field}`")]
    MissingConnectionField {
        source_name: String,
        id: ConnectionId,
        field: &'static str,
    },
    #[error("duplicate connection id `{id}`")]
    DuplicateConnection { id: ConnectionId },
    #[error("duplicate target `{connection_id}:{model_id}`")]
    DuplicateTarget {
        connection_id: ConnectionId,
        model_id: ModelId,
    },
    #[error("target `{connection_id}:{model_id}` references unknown connection")]
    UnknownTargetConnection {
        connection_id: ConnectionId,
        model_id: ModelId,
    },
    #[error("connection `{connection_id}` has multiple default targets: `{first}` and `{second}`")]
    DuplicateDefaultTarget {
        connection_id: ConnectionId,
        first: ModelId,
        second: ModelId,
    },
    #[error("target `{connection_id}:{model_id}` has invalid pricing tiers: {message}")]
    InvalidPricingTiers {
        connection_id: ConnectionId,
        model_id: ModelId,
        message: String,
    },
    #[error("unknown connection `{id}`")]
    UnknownConnection { id: ConnectionId },
    #[error("unknown target `{connection_id}:{model_id}`")]
    UnknownTarget {
        connection_id: ConnectionId,
        model_id: ModelId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ModelFeature {
    ToolCall,
    Reasoning,
    StructuredOutput,
    Temperature,
    Attachment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogSpec {
    pub connections: Vec<ConnectionSpec>,
    pub targets: Vec<TargetSpec>,
    pub models_dev: ModelsDevCatalog,
    pub connection_sources: HashMap<ConnectionId, SourceKind>,
}

impl CatalogSpec {
    fn new(
        connections: Vec<ConnectionSpec>,
        targets: Vec<TargetSpec>,
    ) -> Result<Self, CatalogError> {
        validate_catalog(&connections, &targets)?;
        Ok(Self {
            connections,
            targets,
            models_dev: ModelsDevCatalog::default(),
            connection_sources: HashMap::new(),
        })
    }

    fn with_models_dev(mut self, models_dev: ModelsDevCatalog) -> Self {
        self.models_dev = models_dev;
        self
    }
}

#[derive(Debug, Default)]
struct CatalogBuilder {
    connections: HashMap<ConnectionId, (ConnectionSpec, SourceKind)>,
    connection_order: Vec<ConnectionId>,
    targets: HashMap<(ConnectionId, ModelId), (TargetSpec, SourceKind)>,
    target_order: Vec<(ConnectionId, ModelId)>,
}

impl CatalogBuilder {
    fn add_connection_patch(
        &mut self,
        source_name: &str,
        source: SourceKind,
        patch: ConnectionSpecPatch,
    ) -> Result<(), CatalogError> {
        let id = patch.id.clone();
        match self.connections.get_mut(&id) {
            Some((connection, existing_source)) if source > *existing_source => {
                connection.apply_patch(patch);
                *existing_source = source;
                Ok(())
            }
            Some((_connection, _existing_source)) => Err(CatalogError::DuplicateConnection { id }),
            None => {
                let connection = patch.into_complete(source_name)?;
                self.connection_order.push(id.clone());
                self.connections.insert(id, (connection, source));
                Ok(())
            }
        }
    }

    fn add_target_patch(
        &mut self,
        source: SourceKind,
        patch: TargetSpecPatch,
    ) -> Result<(), CatalogError> {
        let key = (patch.connection.clone(), patch.model.clone());
        match self.targets.get_mut(&key) {
            Some((target, existing_source)) if source > *existing_source => {
                target.apply_patch(patch);
                *existing_source = source;
                Ok(())
            }
            Some((_target, _existing_source)) => Err(CatalogError::DuplicateTarget {
                connection_id: key.0,
                model_id: key.1,
            }),
            None => {
                self.target_order.push(key.clone());
                self.targets.insert(key, (patch.into_complete(), source));
                Ok(())
            }
        }
    }

    fn finish(self) -> Result<CatalogSpec, CatalogError> {
        let connection_sources = self
            .connections
            .iter()
            .map(|(id, (_connection, source))| (id.clone(), *source))
            .collect();
        let connections = self
            .connection_order
            .iter()
            .filter_map(|id| {
                self.connections
                    .get(id)
                    .map(|(connection, _source)| connection.clone())
            })
            .collect::<Vec<_>>();
        let targets = self
            .target_order
            .iter()
            .filter_map(|key| {
                self.targets
                    .get(key)
                    .map(|(target, _source)| target.clone())
            })
            .collect::<Vec<_>>();
        let mut spec = CatalogSpec::new(connections, targets)?;
        spec.connection_sources = connection_sources;
        Ok(spec)
    }
}

#[derive(Debug)]
pub(crate) struct ModelCatalog {
    models_dev: RwLock<ModelsDevCatalog>,
    models_dev_revision: AtomicU64,
    models_dev_refresh_notice: RwLock<Option<String>>,
    connections: HashMap<ConnectionId, ConnectionSpec>,
    connection_sources: HashMap<ConnectionId, SourceKind>,
    connection_order: Vec<ConnectionId>,
    targets: HashMap<(ConnectionId, ModelId), TargetSpec>,
    target_order: Vec<(ConnectionId, ModelId)>,
    live_models_dir: Option<PathBuf>,
    catalog_home_dir: Option<PathBuf>,
    trusted_project_root: Option<PathBuf>,
    live_availability: RwLock<HashMap<ConnectionId, LiveModelAvailability>>,
}

impl ModelCatalog {
    pub(crate) fn from_spec(spec: CatalogSpec) -> Self {
        Self::from_spec_with_live_availability(spec, None, None, None, HashMap::new())
    }

    fn from_spec_with_live_availability(
        spec: CatalogSpec,
        live_models_dir: Option<PathBuf>,
        catalog_home_dir: Option<PathBuf>,
        trusted_project_root: Option<PathBuf>,
        live_availability: HashMap<ConnectionId, LiveModelAvailability>,
    ) -> Self {
        let CatalogSpec {
            connections,
            targets: catalog_targets,
            models_dev,
            connection_sources,
        } = spec;
        let connection_order = connections
            .iter()
            .filter(|connection| connection.enabled)
            .map(|connection| connection.id.clone())
            .collect::<Vec<_>>();
        let connections = connections
            .into_iter()
            .filter(|connection| connection.enabled)
            .map(|connection| (connection.id.clone(), connection))
            .collect::<HashMap<_, _>>();
        let mut targets = HashMap::new();
        let mut target_order = Vec::new();
        for target in catalog_targets
            .into_iter()
            .filter(|target| target.enabled && connections.contains_key(&target.connection))
        {
            let key = (target.connection.clone(), target.model.clone());
            target_order.push(key.clone());
            targets.insert(key, target);
        }
        log_models_dev_drift(&targets, &models_dev);

        Self {
            models_dev: RwLock::new(models_dev),
            models_dev_revision: AtomicU64::new(0),
            models_dev_refresh_notice: RwLock::new(None),
            connections,
            connection_sources,
            connection_order,
            targets,
            target_order,
            live_models_dir,
            catalog_home_dir,
            trusted_project_root,
            live_availability: RwLock::new(live_availability),
        }
    }

    pub(crate) fn load_builtin() -> Result<Self, CatalogError> {
        load_builtin_catalog().map(Self::from_spec)
    }

    pub(crate) fn connections(&self) -> Vec<&ConnectionSpec> {
        self.connection_order
            .iter()
            .filter_map(|id| self.connections.get(id))
            .collect()
    }

    /// Project root whose catalog files were admitted after workspace trust.
    pub(crate) fn trusted_project_root(&self) -> Option<&Path> {
        self.trusted_project_root.as_deref()
    }

    pub(crate) fn connection(&self, connection_id: &ConnectionId) -> Option<&ConnectionSpec> {
        self.connections.get(connection_id)
    }

    pub(crate) fn connection_source(&self, connection_id: &ConnectionId) -> SourceKind {
        self.connection_sources
            .get(connection_id)
            .copied()
            .unwrap_or(SourceKind::BuiltIn)
    }

    /// The connection's enabled targets in catalog order. Used to prefill the
    /// local-model wizard when editing an existing provider.
    pub(crate) fn targets_for_connection(&self, connection_id: &ConnectionId) -> Vec<&TargetSpec> {
        self.target_order
            .iter()
            .filter(|(target_connection_id, _model_id)| target_connection_id == connection_id)
            .filter_map(|key| self.targets.get(key))
            .collect()
    }

    pub(crate) fn resolve(
        &self,
        connection_id: &ConnectionId,
        model_id: &ModelId,
    ) -> Result<ResolvedModel, CatalogError> {
        let connection =
            self.connections
                .get(connection_id)
                .ok_or_else(|| CatalogError::UnknownConnection {
                    id: connection_id.clone(),
                })?;
        let target = self
            .targets
            .get(&(connection_id.clone(), model_id.clone()))
            .ok_or_else(|| CatalogError::UnknownTarget {
                connection_id: connection_id.clone(),
                model_id: model_id.clone(),
            })?;
        let models_dev_id = target.metadata_model.as_ref().unwrap_or(model_id);
        let models_dev = self.models_dev_read().model(models_dev_id).cloned();
        let remote_model_id = target
            .remote_model
            .clone()
            .unwrap_or_else(|| target.model.model().into());
        let live = self.live_model_for_connection_model(connection_id, &remote_model_id);

        Ok(resolve_target(
            connection,
            target,
            models_dev.as_ref(),
            live.as_ref(),
            ModelSource::BuiltIn,
            false,
        ))
    }

    pub(crate) fn resolve_connection_model(
        &self,
        connection_id: &ConnectionId,
        model: &str,
    ) -> Option<ResolvedModel> {
        if let Ok(model_id) = model.parse::<ModelId>()
            && let Ok(resolved) = self.resolve(connection_id, &model_id)
        {
            return Some(resolved);
        }

        self.target_order
            .iter()
            .filter(|(target_connection_id, _model_id)| target_connection_id == connection_id)
            .filter_map(|(target_connection_id, model_id)| {
                self.targets
                    .get(&(target_connection_id.clone(), model_id.clone()))
                    .map(|target| (target_connection_id, model_id, target))
            })
            .find_map(|(target_connection_id, model_id, target)| {
                let target_model = target.model.model();
                let remote_model = target.remote_model.as_deref().unwrap_or(target_model);
                (target_model == model
                    || remote_model == model
                    || target.aliases.iter().any(|alias| alias.as_ref() == model))
                .then(|| self.resolve(target_connection_id, model_id).ok())
                .flatten()
            })
            .or_else(|| self.resolve_connection_display_name(connection_id, model))
            .or_else(|| self.resolve_shadow_target(connection_id, model))
    }

    /// Resolve a model that has no static catalog target by synthesizing a
    /// minimal "shadow" target from live discovery and/or models.dev metadata.
    ///
    /// This closes the class of bug where a provider begins offering a new
    /// model (e.g. `kimi-k3`) that no `[[targets]]` block lists: without a
    /// target, [`resolve`] returns `None`, the estimator falls back to
    /// `from_metadata` (pricing always `None`, no features), so the model runs
    /// unpriced and stripped of capabilities like vision. Grounding the shadow
    /// in live availability (the provider genuinely lists it) or a models.dev
    /// row (the catalog genuinely knows it) means we only synthesize for models
    /// that really exist — a typo'd name matches neither and stays `None`.
    fn resolve_shadow_target(
        &self,
        connection_id: &ConnectionId,
        model: &str,
    ) -> Option<ResolvedModel> {
        let connection = self.connections.get(connection_id)?;
        let live = self.live_model_for_connection_model(connection_id, model);
        // models.dev keys models as `provider/model`; a bare remote id (how
        // live discovery stores them) is namespaced under the connection's
        // explicit models.dev provider when configured, or its own id for
        // direct providers.
        let canonical: ModelId = model
            .parse()
            .or_else(|_err| format!("{connection_id}/{model}").parse())
            .ok()?;
        let metadata_model = connection
            .models_dev_provider
            .as_ref()
            .and_then(|provider| format!("{provider}/{}", canonical.model()).parse().ok())
            .unwrap_or_else(|| canonical.clone());
        let models_dev = self.models_dev_read().model(&metadata_model).cloned();
        let metadata_model_override =
            (metadata_model != canonical).then_some(metadata_model.clone());
        // Only synthesize when something real backs the model. Neither signal
        // present → keep the historical `None` so we never fabricate a target.
        if models_dev.is_none() && live.is_none() {
            return None;
        }
        let remote_model = live
            .as_ref()
            .map(|available| available.remote_model_id.clone())
            .unwrap_or_else(|| canonical.model().into());
        // Leave features empty when models.dev has the row so `resolve_target`
        // pulls its (more complete) capability set — live `/models` listings
        // routinely under-report vision. Use live features only as a fallback
        // when models.dev is silent about this model.
        let features = if models_dev.is_some() {
            Vec::new()
        } else {
            live.as_ref()
                .map(|available| available.features.clone())
                .unwrap_or_default()
        };
        let shadow = TargetSpec {
            connection: connection_id.clone(),
            enabled: true,
            model: canonical,
            display_name: live
                .as_ref()
                .and_then(|available| available.display_name.clone()),
            metadata_model: metadata_model_override,
            remote_model: Some(remote_model),
            aliases: Vec::new(),
            recommended: false,
            recommended_effort: None,
            discouraged_efforts: Vec::new(),
            is_default: false,
            transport: None,
            prompt_cache_policy: None,
            endpoint_path: None,
            context_window: None,
            output_limit: None,
            token_counter: None,
            max_tokens: None,
            reasoning_codec: None,
            reasoning_options: None,
            features,
            pricing: None,
            pricing_tiers: Vec::new(),
            roles: Vec::new(),
            pinned: false,
            pinned_fields: Vec::new(),
        };
        Some(resolve_target(
            connection,
            &shadow,
            models_dev.as_ref(),
            live.as_ref(),
            ModelSource::Discovered,
            models_dev.is_none(),
        ))
    }

    pub(crate) fn resolve_connection_display_name(
        &self,
        connection_id: &ConnectionId,
        display_name: &str,
    ) -> Option<ResolvedModel> {
        self.target_order
            .iter()
            .filter(|(target_connection_id, _model_id)| target_connection_id == connection_id)
            .filter_map(|(target_connection_id, model_id)| {
                self.resolve(target_connection_id, model_id).ok()
            })
            .find(|resolved| resolved.display_name.eq_ignore_ascii_case(display_name))
    }

    #[cfg(test)]
    pub(crate) fn list_resolved_models(&self) -> Result<Vec<ResolvedModel>, CatalogError> {
        self.target_order
            .iter()
            .map(|(connection_id, model_id)| self.resolve(connection_id, model_id))
            .collect()
    }

    pub(crate) fn available_models_for_connection(
        &self,
        connection_id: &ConnectionId,
        fallback_models: Vec<String>,
    ) -> Vec<String> {
        if !self.connections.contains_key(connection_id) {
            return Vec::new();
        }

        let live_models = self
            .live_availability_read()
            .get(connection_id)
            .map(LiveModelAvailability::remote_model_ids)
            .unwrap_or_default();
        let has_live_models = !live_models.is_empty();
        let target_models = self
            .target_order
            .iter()
            .filter(|(target_connection_id, _model_id)| target_connection_id == connection_id)
            .filter_map(|(target_connection_id, model_id)| {
                self.targets
                    .get(&(target_connection_id.clone(), model_id.clone()))
                    .map(|target| {
                        target
                            .remote_model
                            .as_deref()
                            .unwrap_or_else(|| target.model.model())
                            .to_string()
                    })
            })
            .collect::<Vec<_>>();
        if target_models.is_empty() {
            return if has_live_models {
                live_models
            } else {
                fallback_models
            };
        }

        // Provider listings contain wire ids, while one wire id can back
        // multiple catalog targets (OpenAI's short- and long-context price
        // bands). Keep the provider's live order, collapse aliases that resolve
        // to the same canonical target, then append canonical selectors for
        // any additional target sharing an available wire model. Without this,
        // `/refresh` made synthetic price-band targets disappear from `/model`.
        let source_selectors = if has_live_models {
            live_models
        } else {
            target_models
        };
        let mut selectors = Vec::new();
        let mut seen_model_ids = HashSet::new();
        let mut seen_unresolved = HashSet::new();
        for selector in source_selectors {
            let resolved = self.resolve_connection_model(connection_id, &selector);
            let available_remote = resolved
                .as_ref()
                .map(|model| model.remote_model_id.as_ref())
                .unwrap_or(selector.as_str());
            let keep = resolved.as_ref().map_or_else(
                || seen_unresolved.insert(selector.clone()),
                |model| seen_model_ids.insert(model.model_id.clone()),
            );
            if !keep {
                continue;
            }

            // Place price-band variants immediately after their base model so
            // a provider with several tiered models stays readable.
            let mut variants = Vec::new();
            for (target_connection_id, model_id) in &self.target_order {
                if target_connection_id != connection_id || seen_model_ids.contains(model_id) {
                    continue;
                }
                let Some(target) = self
                    .targets
                    .get(&(target_connection_id.clone(), model_id.clone()))
                else {
                    continue;
                };
                let remote_model = target
                    .remote_model
                    .as_deref()
                    .unwrap_or_else(|| target.model.model());
                if remote_model == available_remote {
                    seen_model_ids.insert(model_id.clone());
                    variants.push(model_id.to_string());
                }
            }
            selectors.push(selector);
            selectors.extend(variants);
        }
        selectors
    }

    pub(crate) fn target_remote_models_for_connection(
        &self,
        connection_id: &ConnectionId,
    ) -> Vec<String> {
        if !self.connections.contains_key(connection_id) {
            return Vec::new();
        }

        self.target_order
            .iter()
            .filter(|(target_connection_id, _model_id)| target_connection_id == connection_id)
            .filter_map(|(target_connection_id, model_id)| {
                self.targets
                    .get(&(target_connection_id.clone(), model_id.clone()))
                    .map(|target| {
                        target
                            .remote_model
                            .as_deref()
                            .unwrap_or_else(|| target.model.model())
                            .to_string()
                    })
            })
            .collect()
    }

    fn live_availability_read(
        &self,
    ) -> RwLockReadGuard<'_, HashMap<ConnectionId, LiveModelAvailability>> {
        match self.live_availability.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn models_dev_read(&self) -> RwLockReadGuard<'_, ModelsDevCatalog> {
        match self.models_dev.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(crate) fn replace_models_dev_metadata(&self, models_dev: ModelsDevCatalog) {
        log_models_dev_drift(&self.targets, &models_dev);
        match self.models_dev.write() {
            Ok(mut guard) => *guard = models_dev,
            Err(poisoned) => *poisoned.into_inner() = models_dev,
        }
        self.models_dev_revision.fetch_add(1, Ordering::Relaxed);
        self.set_models_dev_refresh_notice(None);
    }

    /// Monotonic signal used by read-only picker caches to notice a metadata
    /// refresh even when the provider's live model-id list did not change.
    pub(crate) fn models_dev_revision(&self) -> u64 {
        self.models_dev_revision.load(Ordering::Relaxed)
    }

    /// Refresh shared Models.dev metadata when this catalog came from a Bonsai
    /// home directory. In-memory and built-in-only catalogs have no cache path
    /// and return `Ok(None)` without touching the network.
    pub(crate) async fn refresh_models_dev_metadata(&self) -> Result<Option<usize>, CatalogError> {
        let Some(home_dir) = self.catalog_home_dir.as_deref() else {
            return Ok(None);
        };
        match force_refresh_models_dev_cache_from_home(home_dir).await {
            Ok(models_dev) => {
                let model_count = models_dev.len();
                self.replace_models_dev_metadata(models_dev);
                Ok(Some(model_count))
            }
            Err(error) => {
                self.record_models_dev_refresh_failure(&error);
                Err(error)
            }
        }
    }

    /// Record a bounded, secret-free explanation for the cached-metadata
    /// fallback. The detailed error remains in tracing logs.
    pub(crate) fn record_models_dev_refresh_failure(&self, error: &CatalogError) {
        let reason = match error {
            CatalogError::ModelsDevFetch { .. } => "network request failed".to_string(),
            CatalogError::ModelsDevHttpStatus { status, .. } => {
                format!("server returned HTTP {status}")
            }
            CatalogError::ModelsDevJson { .. } | CatalogError::InvalidModelsDevModelId { .. } => {
                "downloaded metadata was invalid".to_string()
            }
            CatalogError::ReadFile { .. } => "cached metadata could not be read".to_string(),
            _ => "metadata cache could not be updated".to_string(),
        };
        self.set_models_dev_refresh_notice(Some(format!(
            "Models.dev refresh failed ({reason}); using cached model metadata. Check network access and BONSAI_MODELS_DEV_* settings, then run /refresh."
        )));
    }

    pub(crate) fn models_dev_refresh_notice(&self) -> Option<String> {
        match self.models_dev_refresh_notice.read() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Whether this catalog has a Bonsai home directory (and thus a
    /// models.dev cache that can be refreshed). In-memory/built-in-only
    /// catalogs return `false`; their `refresh_models_dev_metadata` is a
    /// no-op `Ok(None)`.
    pub(crate) fn has_catalog_home_dir(&self) -> bool {
        self.catalog_home_dir.is_some()
    }

    fn set_models_dev_refresh_notice(&self, notice: Option<String>) {
        match self.models_dev_refresh_notice.write() {
            Ok(mut guard) => *guard = notice,
            Err(poisoned) => *poisoned.into_inner() = notice,
        }
    }

    fn live_availability_write(
        &self,
    ) -> RwLockWriteGuard<'_, HashMap<ConnectionId, LiveModelAvailability>> {
        match self.live_availability.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

const LIVE_MODEL_CACHE_SCHEMA_VERSION: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct LiveModelAvailability {
    #[serde(default)]
    pub(crate) schema_version: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at_unix_secs: Option<u64>,
    #[serde(default)]
    pub models: Vec<AvailableModel>,
}

impl Default for LiveModelAvailability {
    fn default() -> Self {
        Self {
            schema_version: LIVE_MODEL_CACHE_SCHEMA_VERSION,
            fetched_at_unix_secs: None,
            models: Vec::new(),
        }
    }
}

impl LiveModelAvailability {
    pub(crate) fn from_remote_ids(models: impl IntoIterator<Item = String>) -> Self {
        Self {
            schema_version: LIVE_MODEL_CACHE_SCHEMA_VERSION,
            fetched_at_unix_secs: None,
            models: dedup_preserving_order(models.into_iter().collect())
                .into_iter()
                .map(AvailableModel::remote)
                .collect(),
        }
    }

    pub(crate) fn with_fallback_context_window(mut self, context_window: Option<u32>) -> Self {
        let Some(context_window) = context_window.filter(|value| *value > 0) else {
            return self;
        };
        for model in &mut self.models {
            if model.context_window.is_none() {
                model.context_window = Some(context_window);
            }
        }
        self
    }

    pub(crate) fn remote_model_ids(&self) -> Vec<String> {
        dedup_preserving_order(
            self.models
                .iter()
                .map(|model| model.remote_model_id.to_string())
                .collect(),
        )
    }

    fn mark_refreshed(&mut self) {
        self.fetched_at_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs());
    }

    fn is_fresh(&self, ttl: Duration) -> bool {
        if self.schema_version != LIVE_MODEL_CACHE_SCHEMA_VERSION
            || ttl.is_zero()
            || self.models.is_empty()
        {
            return false;
        }
        let Some(fetched_at) = self.fetched_at_unix_secs else {
            return false;
        };
        let Some(now) = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs())
        else {
            return false;
        };
        now.saturating_sub(fetched_at) <= ttl.as_secs()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) struct AvailableModel {
    pub remote_model_id: Box<str>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<ModelId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    /// Maximum output tokens reported by the provider's model listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_limit: Option<u32>,
    /// Human-readable name reported by the server (LM Studio native API);
    /// `None` when the listing only carries ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<Box<str>>,
    /// Capabilities reported by the server (tool use, vision, reasoning).
    /// Empty means "unreported", not "unsupported" — merge accordingly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<ModelFeature>,
    /// Exact reasoning choices advertised by the provider. Empty means the
    /// endpoint did not report them, so static catalog metadata remains active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_reasoning: Vec<ReasoningSelection>,
    /// Provider-recommended effort when the user has no saved model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended_reasoning: Option<ReasoningSelection>,
    /// Provider-reported wire codec for reasoning. This is essential for
    /// newly discovered Anthropic models whose adaptive-thinking request shape
    /// differs from the classic budget-tokens shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_codec: Option<ReasoningCodec>,
    /// Codex backend routing contract for models that use Responses Lite.
    #[serde(default, skip_serializing_if = "is_false")]
    pub use_responses_lite: bool,
    /// Live per-token pricing the provider publishes in its own model listing.
    /// Only aggregator gateways (OpenRouter) expose this; direct providers omit
    /// it, leaving `None`. When present it is the authoritative *billed* price
    /// for that route, so resolution ranks it above the models.dev estimate but
    /// below a hand-pinned catalog `pricing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl AvailableModel {
    pub(crate) fn remote(remote_model_id: impl Into<String>) -> Self {
        Self::with_metadata(remote_model_id, None, None, Vec::new())
    }

    #[cfg(test)]
    pub(crate) fn remote_with_context_window(
        remote_model_id: impl Into<String>,
        context_window: Option<u32>,
    ) -> Self {
        Self::with_metadata(remote_model_id, context_window, None, Vec::new())
    }

    pub(crate) fn with_metadata(
        remote_model_id: impl Into<String>,
        context_window: Option<u32>,
        display_name: Option<String>,
        features: Vec<ModelFeature>,
    ) -> Self {
        Self {
            remote_model_id: remote_model_id.into().into_boxed_str(),
            model_id: None,
            context_window,
            output_limit: None,
            display_name: display_name.map(String::into_boxed_str),
            features,
            supported_reasoning: Vec::new(),
            recommended_reasoning: None,
            reasoning_codec: None,
            use_responses_lite: false,
            pricing: None,
        }
    }

    pub(crate) fn with_output_limit(mut self, output_limit: Option<u32>) -> Self {
        self.output_limit = output_limit.filter(|value| *value > 0);
        self
    }

    pub(crate) fn with_reasoning_codec(mut self, reasoning_codec: ReasoningCodec) -> Self {
        self.reasoning_codec = Some(reasoning_codec);
        self
    }

    /// Attach provider-published live pricing (gateway listings only).
    pub(crate) fn with_pricing(mut self, pricing: Option<ModelPricing>) -> Self {
        self.pricing = pricing;
        self
    }

    pub(crate) fn with_reasoning(
        mut self,
        supported: Vec<ReasoningSelection>,
        recommended: Option<ReasoningSelection>,
    ) -> Self {
        self.supported_reasoning = dedup_reasoning(supported);
        self.recommended_reasoning = recommended.filter(|selection| {
            *selection == ReasoningSelection::Default
                || self.supported_reasoning.contains(selection)
        });
        self
    }

    pub(crate) fn normalize_reasoning(
        &self,
        selection: ReasoningSelection,
    ) -> Option<ReasoningSelection> {
        (!self.supported_reasoning.is_empty()).then(|| {
            if selection == ReasoningSelection::Default
                || self.supported_reasoning.contains(&selection)
            {
                selection
            } else {
                ReasoningSelection::Default
            }
        })
    }
}

pub(crate) fn available_model_ids_for_provider(
    catalog: Option<&ModelCatalog>,
    provider_id: &str,
    metadata: &ProviderMetadata,
    configured_model: &str,
) -> Vec<String> {
    let fallback_models = fallback_model_ids(metadata, configured_model);
    match catalog {
        Some(catalog) => match provider_id.parse::<ConnectionId>() {
            Ok(connection_id) => {
                catalog.available_models_for_connection(&connection_id, fallback_models)
            }
            Err(_err) => fallback_models,
        },
        None => fallback_models,
    }
}

pub(crate) fn connection_id_for_provider_id(provider_id: &str) -> Option<ConnectionId> {
    provider_id.parse().ok()
}

fn fallback_model_ids(metadata: &ProviderMetadata, configured_model: &str) -> Vec<String> {
    if metadata.seed_models.is_empty() && !configured_model.trim().is_empty() {
        vec![configured_model.to_string()]
    } else {
        metadata.seed_model_list()
    }
}

fn dedup_preserving_order(models: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    models
        .into_iter()
        .filter(|model| seen.insert(model.clone()))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedModel {
    pub connection_id: ConnectionId,
    pub model_id: ModelId,
    pub remote_model_id: Box<str>,
    pub default_base_url: Box<str>,
    pub display_name: Box<str>,
    pub transport: TransportProtocol,
    pub prompt_cache_policy: PromptCachePolicy,
    pub reasoning_codec: ReasoningCodec,
    pub endpoint_path: Option<Box<str>>,
    pub context_window: Option<u32>,
    pub output_limit: Option<u32>,
    pub token_counter: Option<TokenCounterKind>,
    pub pricing: Option<ModelPricing>,
    pub pricing_schedule: Option<ModelPricingSchedule>,
    pub reasoning_options: Vec<ReasoningOption>,
    pub parameter_preview: Vec<ParameterPreview>,
    pub features: Vec<ModelFeature>,
    pub recommended: bool,
    pub recommended_effort: Option<ReasoningSelection>,
    pub discouraged_efforts: Vec<ReasoningSelection>,
    pub roles: Vec<LegacyModelRole>,
    pub source: ModelSource,
    /// Discovered live model with no bundled target or models.dev row.
    pub unverified: bool,
    /// Per-field provenance for metadata shown by `/models` and used at
    /// runtime after the current merge.
    pub metadata_sources: ResolvedModelMetadataSources,
    /// Explicit unpinned catalog values that disagree with current models.dev.
    /// The refreshed value is used, while the mismatch remains visible so the
    /// bundled offline fallback can be maintained.
    pub catalog_drift: Vec<String>,
}

impl ResolvedModel {
    pub(crate) fn run_target(
        &self,
        base_url: Box<str>,
        reasoning: ReasoningSelection,
    ) -> RunTarget {
        let reasoning = self.normalize_reasoning(reasoning);
        let reasoning_escalation = reasoning.next_higher_supported(&self.reasoning_selections());
        RunTarget {
            connection_id: self.connection_id.clone(),
            model_id: self.model_id.clone(),
            remote_model_id: self.remote_model_id.clone(),
            base_url,
            transport: self.transport,
            prompt_cache_policy: self.prompt_cache_policy,
            reasoning_codec: self.reasoning_codec,
            endpoint_path: self.endpoint_path.clone(),
            context_window: self.context_window,
            output_limit: self.output_limit,
            reasoning,
            reasoning_escalation,
            supports_vision: self.features.contains(&ModelFeature::Attachment),
            use_responses_lite: false,
        }
    }

    pub(crate) fn reasoning_selections(&self) -> Vec<ReasoningSelection> {
        reasoning_selections_from_options(&self.reasoning_options)
    }

    pub(crate) fn supports_reasoning(&self, reasoning: ReasoningSelection) -> bool {
        reasoning == ReasoningSelection::Default || self.reasoning_selections().contains(&reasoning)
    }

    pub(crate) fn normalize_reasoning(
        &self,
        reasoning: crate::provider::ReasoningSelection,
    ) -> crate::provider::ReasoningSelection {
        if self.supports_reasoning(reasoning) {
            reasoning
        } else {
            crate::provider::ReasoningSelection::default()
        }
    }

    pub(crate) fn parameter_preview_label(&self) -> String {
        if self.parameter_preview.is_empty() {
            "default parameters".to_string()
        } else {
            self.parameter_preview
                .iter()
                .map(|preview| preview.label())
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

/// Warn once per catalog load about every explicit TOML value that disagrees
/// with the models.dev row it shadows. Refreshed values outrank unpinned
/// fallbacks, but the warning keeps the offline catalog from drifting silently.
/// Targets marked `pinned = true` remain authoritative and are skipped.
/// `pinned_fields` suppresses only the corresponding field warning.
fn log_models_dev_drift(
    targets: &HashMap<(ConnectionId, ModelId), TargetSpec>,
    models_dev: &ModelsDevCatalog,
) {
    for target in targets.values() {
        let metadata_id = target.metadata_model.as_ref().unwrap_or(&target.model);
        let Some(models_dev_model) = models_dev.model(metadata_id) else {
            continue;
        };
        let mismatches = models_dev_drift_lines(target, models_dev_model);
        if !mismatches.is_empty() {
            tracing::warn!(
                target = %target.model,
                models_dev = %metadata_id,
                drift = %mismatches.join("; "),
                "catalog target drifts from models.dev; update it or pin the deliberate field"
            );
        }
    }
}

/// The per-field drift descriptions for one target; empty when the target is
/// pinned, silent on a field, or in agreement.
fn models_dev_drift_lines(target: &TargetSpec, models_dev_model: &ModelsDevModel) -> Vec<String> {
    if target.pinned {
        return Vec::new();
    }
    let mut mismatches = Vec::new();
    if !target.pins(ModelMetadataField::ContextWindow)
        && let (Some(toml), Some(live)) = (target.context_window, models_dev_model.context_window)
        && toml != live
    {
        mismatches.push(format!("context_window {toml} vs models.dev {live}"));
    }
    if !target.pins(ModelMetadataField::OutputLimit)
        && let (Some(toml), Some(live)) = (target.output_limit, models_dev_model.output_limit)
        && toml != live
    {
        mismatches.push(format!("output_limit {toml} vs models.dev {live}"));
    }
    if !target.pins(ModelMetadataField::Pricing) {
        let toml_schedule = target
            .pricing
            .map(|base| ModelPricingSchedule::new(base, target.pricing_tiers.clone()));
        let live_schedule = models_dev_model.pricing_schedule();
        let differs = match (toml_schedule, live_schedule) {
            (Some(toml), Some(live)) if target.pricing_tiers.is_empty() => {
                toml.pricing_for_context_window(target.context_window)
                    != live.pricing_for_context_window(target.context_window)
            }
            (Some(toml), Some(live)) => toml != live,
            _ => false,
        };
        if differs {
            mismatches.push("pricing differs from models.dev".to_string());
        }
    }
    mismatches
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ModelSource {
    BuiltIn,
    /// Resolved from a synthesized "shadow" target — a model the provider
    /// offers (or models.dev knows) that has no hand-written `[[targets]]`
    /// block. Its metadata comes entirely from live discovery + models.dev.
    Discovered,
}

/// Source selected for one resolved model-metadata field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ModelMetadataSource {
    /// Bundled or user/project TOML.
    Catalog,
    /// Current models.dev metadata.
    ModelsDev,
    /// Metadata published by the provider's live model endpoint.
    Provider,
}

impl ModelMetadataSource {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Catalog => "catalog",
            Self::ModelsDev => "models.dev",
            Self::Provider => "provider",
        }
    }
}

/// Provenance of the independently merged fields on a resolved model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ResolvedModelMetadataSources {
    pub display_name: Option<ModelMetadataSource>,
    pub context_window: Option<ModelMetadataSource>,
    pub output_limit: Option<ModelMetadataSource>,
    pub pricing: Option<ModelMetadataSource>,
    pub features: Option<ModelMetadataSource>,
    pub reasoning: Option<ModelMetadataSource>,
}

impl ResolvedModelMetadataSources {
    /// Compact source attribution for the model-picker detail row.
    pub(crate) fn compact_label(&self) -> String {
        let fields = [
            ("ctx", self.context_window),
            ("out", self.output_limit),
            ("price", self.pricing),
            ("caps", self.features),
            ("reason", self.reasoning),
        ];
        fields
            .into_iter()
            .filter_map(|(field, source)| {
                source.map(|source| format!("{field}:{}", source.label()))
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn choose_refreshed_metadata<T>(
    pinned: bool,
    catalog: Option<T>,
    provider: Option<T>,
    models_dev: Option<T>,
) -> (Option<T>, Option<ModelMetadataSource>) {
    let candidates = if pinned {
        [
            (catalog, ModelMetadataSource::Catalog),
            (provider, ModelMetadataSource::Provider),
            (models_dev, ModelMetadataSource::ModelsDev),
        ]
    } else {
        [
            (provider, ModelMetadataSource::Provider),
            (models_dev, ModelMetadataSource::ModelsDev),
            (catalog, ModelMetadataSource::Catalog),
        ]
    };
    candidates
        .into_iter()
        .find_map(|(value, source)| value.map(|value| (Some(value), Some(source))))
        .unwrap_or((None, None))
}

fn live_reasoning_options(
    model: &AvailableModel,
    transport: TransportProtocol,
) -> Option<Vec<ReasoningOption>> {
    if model.supported_reasoning.is_empty() {
        return None;
    }

    let mut options = Vec::new();
    if model
        .supported_reasoning
        .iter()
        .any(|selection| matches!(selection, ReasoningSelection::Off | ReasoningSelection::On))
    {
        options.push(ReasoningOption::Toggle);
    }
    let efforts = model
        .supported_reasoning
        .iter()
        .filter_map(|selection| match selection {
            ReasoningSelection::Minimal => Some(ReasoningEffort::Minimal),
            ReasoningSelection::Low => Some(ReasoningEffort::Low),
            ReasoningSelection::Medium => Some(ReasoningEffort::Medium),
            ReasoningSelection::High => Some(ReasoningEffort::High),
            ReasoningSelection::XHigh => Some(ReasoningEffort::XHigh),
            ReasoningSelection::Max => Some(ReasoningEffort::Max),
            ReasoningSelection::Ultra => Some(ReasoningEffort::Ultra),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !efforts.is_empty() {
        options.push(ReasoningOption::Effort(efforts));
    }
    options.extend(model.supported_reasoning.iter().filter_map(|selection| {
        let ReasoningSelection::BudgetTokens(default) = selection else {
            return None;
        };
        Some(ReasoningOption::BudgetTokens {
            min: None,
            max: None,
            default: *default,
        })
    }));
    Some(reasoning_options_for_transport(&options, transport))
}

fn resolve_target(
    connection: &ConnectionSpec,
    target: &TargetSpec,
    models_dev: Option<&ModelsDevModel>,
    live: Option<&AvailableModel>,
    source: ModelSource,
    unverified: bool,
) -> ResolvedModel {
    let remote_model_id = target
        .remote_model
        .clone()
        .unwrap_or_else(|| target.model.model().into());
    let (mut output_limit, mut output_limit_source) = choose_refreshed_metadata(
        target.pins(ModelMetadataField::OutputLimit),
        target.output_limit,
        live.and_then(|model| model.output_limit),
        models_dev.and_then(|model| model.output_limit),
    );
    if output_limit.is_none() {
        output_limit = target.max_tokens;
        output_limit_source = target.max_tokens.map(|_| ModelMetadataSource::Catalog);
    }
    let transport = target.transport.unwrap_or(connection.transport);
    let prompt_cache_policy = target
        .prompt_cache_policy
        .unwrap_or(connection.prompt_cache_policy);
    let reasoning_codec = target
        .reasoning_codec
        .or_else(|| live.and_then(|model| model.reasoning_codec))
        .or(connection.reasoning_codec)
        .unwrap_or_else(|| ReasoningCodec::default_for_transport(transport));
    let parameter_preview = target
        .max_tokens
        .or(output_limit)
        .map(ParameterPreview::MaxTokens)
        .into_iter()
        .collect::<Vec<_>>();
    let live_features = live
        .filter(|model| !model.features.is_empty())
        .map(|model| model.features.clone());
    let models_dev_features = models_dev.map(ModelsDevModel::features);
    let catalog_features = (!target.features.is_empty()).then(|| target.features.clone());
    let (features, features_source) = choose_refreshed_metadata(
        target.pins(ModelMetadataField::Features),
        catalog_features,
        live_features,
        models_dev_features,
    );
    let features = features.unwrap_or_default();
    let catalog_reasoning = target
        .reasoning_options
        .as_deref()
        .map(|options| reasoning_options_for_transport(options, transport));
    let live_reasoning = live.and_then(|model| live_reasoning_options(model, transport));
    let models_dev_reasoning =
        models_dev.map(|model| model.reasoning_options_for_transport(transport));
    let (reasoning_options, reasoning_source) = choose_refreshed_metadata(
        target.pins(ModelMetadataField::Reasoning),
        catalog_reasoning,
        live_reasoning,
        models_dev_reasoning,
    );
    let reasoning_options = reasoning_options.unwrap_or_default();

    let (context_window, context_window_source) = choose_refreshed_metadata(
        target.pins(ModelMetadataField::ContextWindow),
        target.context_window,
        live.and_then(|model| model.context_window),
        models_dev.and_then(|model| model.context_window),
    );
    let catalog_pricing = target
        .pricing
        .map(|base| ModelPricingSchedule::new(base, target.pricing_tiers.clone()));
    let live_pricing = live
        .and_then(|model| model.pricing)
        .map(ModelPricingSchedule::flat);
    let models_dev_pricing = models_dev.and_then(ModelsDevModel::pricing_schedule);
    let (pricing_schedule, pricing_source) = choose_refreshed_metadata(
        target.pins(ModelMetadataField::Pricing),
        catalog_pricing,
        live_pricing,
        models_dev_pricing,
    );
    let pricing = pricing_schedule
        .as_ref()
        .map(|schedule| schedule.pricing_for_context_window(context_window));
    let (display_name, display_name_source) = choose_refreshed_metadata(
        target.pins(ModelMetadataField::DisplayName),
        target.display_name.clone(),
        live.and_then(|model| model.display_name.clone()),
        models_dev.map(|model| model.display_name.clone()),
    );
    let catalog_drift = models_dev
        .map(|model| models_dev_drift_lines(target, model))
        .unwrap_or_default();

    ResolvedModel {
        connection_id: connection.id.clone(),
        model_id: target.model.clone(),
        remote_model_id,
        default_base_url: connection.default_base_url.clone(),
        display_name: display_name.unwrap_or_else(|| target.model.model().into()),
        transport,
        prompt_cache_policy,
        reasoning_codec,
        endpoint_path: target
            .endpoint_path
            .clone()
            .or_else(|| connection.default_endpoint_path.clone()),
        context_window,
        output_limit,
        token_counter: target.token_counter.or(connection.default_token_counter),
        // A hand-pinned catalog price is a deliberate override. Otherwise a
        // fresh provider-published billed price wins, then current models.dev
        // metadata, with the bundled value retained only as the offline
        // fallback. This makes `/refresh` update prices without sacrificing a
        // usable catalog when every remote source is unavailable.
        pricing,
        pricing_schedule,
        reasoning_options,
        parameter_preview,
        features,
        recommended: target.recommended,
        recommended_effort: if target.pins(ModelMetadataField::Reasoning) {
            target
                .recommended_effort
                .or_else(|| live.and_then(|model| model.recommended_reasoning))
        } else {
            live.and_then(|model| model.recommended_reasoning)
                .or(target.recommended_effort)
        },
        discouraged_efforts: target.discouraged_efforts.clone(),
        roles: target.roles.clone(),
        source,
        unverified,
        metadata_sources: ResolvedModelMetadataSources {
            display_name: display_name_source,
            context_window: context_window_source,
            output_limit: output_limit_source,
            pricing: pricing_source,
            features: features_source,
            reasoning: reasoning_source,
        },
        catalog_drift,
    }
}

fn validate_catalog(
    connections: &[ConnectionSpec],
    targets: &[TargetSpec],
) -> Result<(), CatalogError> {
    let mut connection_ids = HashSet::new();
    for connection in connections {
        if !connection_ids.insert(connection.id.clone()) {
            return Err(CatalogError::DuplicateConnection {
                id: connection.id.clone(),
            });
        }
    }

    let mut target_keys = HashSet::new();
    let mut defaults_by_connection: HashMap<ConnectionId, ModelId> = HashMap::new();
    for target in targets {
        if !connection_ids.contains(&target.connection) {
            return Err(CatalogError::UnknownTargetConnection {
                connection_id: target.connection.clone(),
                model_id: target.model.clone(),
            });
        }

        let key = (target.connection.clone(), target.model.clone());
        if !target_keys.insert(key) {
            return Err(CatalogError::DuplicateTarget {
                connection_id: target.connection.clone(),
                model_id: target.model.clone(),
            });
        }

        if !target.pricing_tiers.is_empty() {
            if target.pricing.is_none() {
                return Err(CatalogError::InvalidPricingTiers {
                    connection_id: target.connection.clone(),
                    model_id: target.model.clone(),
                    message: "tiered rates require base pricing".to_string(),
                });
            }
            let mut thresholds = HashSet::new();
            for tier in &target.pricing_tiers {
                if tier.above_input_tokens == 0 {
                    return Err(CatalogError::InvalidPricingTiers {
                        connection_id: target.connection.clone(),
                        model_id: target.model.clone(),
                        message: "thresholds must be greater than zero".to_string(),
                    });
                }
                if !thresholds.insert(tier.above_input_tokens) {
                    return Err(CatalogError::InvalidPricingTiers {
                        connection_id: target.connection.clone(),
                        model_id: target.model.clone(),
                        message: format!("duplicate threshold {}", tier.above_input_tokens),
                    });
                }
            }
        }

        if target.enabled
            && target.is_default
            && let Some(first) =
                defaults_by_connection.insert(target.connection.clone(), target.model.clone())
        {
            return Err(CatalogError::DuplicateDefaultTarget {
                connection_id: target.connection.clone(),
                first,
                second: target.model.clone(),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ReasoningEffort;
    fn source(name: &'static str, content: &'static str) -> TomlSource<'static> {
        TomlSource { name, content }
    }

    fn connection_id(value: &str) -> ConnectionId {
        value.parse().unwrap()
    }

    fn model_id(value: &str) -> ModelId {
        value.parse().unwrap()
    }

    #[test]
    fn builtin_toml_parses_and_preserves_explicit_empty_reasoning() {
        let catalog = load_builtin_catalog().unwrap();

        assert_eq!(catalog.connections.len(), 23);
        assert_eq!(catalog.targets.len(), 201);
        assert!(
            catalog
                .connections
                .iter()
                .any(|connection| connection.id == connection_id("openai-compatible"))
        );
        assert!(
            catalog
                .connections
                .iter()
                .any(|connection| connection.id == connection_id("anthropic-compatible"))
        );
        let opencode = catalog
            .connections
            .iter()
            .find(|connection| connection.id.as_str() == "opencode")
            .unwrap();
        assert_eq!(
            opencode
                .models_dev_provider
                .as_ref()
                .map(ConnectionId::as_str),
            Some("opencode-go")
        );
        assert!(
            catalog
                .targets
                .iter()
                .any(|target| target.model.as_str() == "opencode/hy3"),
            "documented OpenCode Go models must remain usable offline"
        );

        let opencode_zen = catalog
            .connections
            .iter()
            .find(|connection| connection.id.as_str() == "opencode-zen")
            .unwrap();
        assert_eq!(opencode_zen.display_name.as_ref(), "OpenCode Zen");
        assert_eq!(
            opencode_zen.default_base_url.as_ref(),
            "https://opencode.ai/zen/v1"
        );
        assert_eq!(
            opencode_zen.default_model.as_ref().map(ModelId::as_str),
            Some("opencode-zen/claude-sonnet-5")
        );
        assert_eq!(
            opencode_zen
                .models_dev_provider
                .as_ref()
                .map(ConnectionId::as_str),
            Some("opencode")
        );

        let opencode_zen_default = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "opencode-zen/claude-sonnet-5")
            .unwrap();
        assert_eq!(
            opencode_zen_default
                .metadata_model
                .as_ref()
                .map(ModelId::as_str),
            Some("opencode/claude-sonnet-5")
        );
        assert!(opencode_zen_default.is_default);
        assert!(
            !catalog
                .targets
                .iter()
                .find(|target| target.model.as_str() == "opencode-zen/grok-code")
                .unwrap()
                .is_default,
            "the deprecated and unavailable grok-code target must not be the default"
        );
        assert_eq!(
            catalog
                .targets
                .iter()
                .find(|target| target.model.as_str() == "opencode-zen/gpt-5-nano")
                .unwrap()
                .roles,
            vec![LegacyModelRole::Cheap],
            "Zen keeps a current low-cost role after retiring grok-code"
        );
        for model in [
            "opencode-zen/claude-opus-5",
            "opencode-zen/gemini-3.5-flash-lite",
            "opencode-zen/gemini-3.6-flash",
            "opencode-zen/laguna-s-2.1-free",
            "opencode-zen/ling-3.0-flash-free",
        ] {
            assert!(
                catalog
                    .targets
                    .iter()
                    .any(|target| target.model.as_str() == model),
                "current live Zen model `{model}` must remain available offline"
            );
        }

        let deepseek_v4_pro = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "opencode-zen/deepseek-v4-pro")
            .unwrap();
        assert!(deepseek_v4_pro.pinned);
        assert_eq!(
            deepseek_v4_pro.pricing.unwrap().output_micros_per_million,
            3_480_000,
            "the official Zen gateway price overrides inherited metadata"
        );
        for (model, context_window) in [
            ("opencode-zen/claude-sonnet-4-5", 200_000),
            ("opencode-zen/gemini-3.1-pro", 200_000),
            ("opencode-zen/gpt-5.4", 272_000),
            ("opencode-zen/gpt-5.5", 272_000),
            ("opencode-zen/gpt-5.6-sol", 272_000),
            ("opencode-zen/gpt-5.6-terra", 272_000),
            ("opencode-zen/gpt-5.6-luna", 272_000),
            ("opencode-zen/grok-4.5", 200_000),
        ] {
            let target = catalog
                .targets
                .iter()
                .find(|target| target.model.as_str() == model)
                .unwrap();
            assert!(target.pinned, "{model} has tier-specific Zen pricing");
            assert_eq!(target.context_window, Some(context_window), "{model}");
        }

        let glm = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "opencode/glm-5.2")
            .unwrap();
        assert_eq!(glm.recommended_effort, Some(ReasoningSelection::Off));
        assert_eq!(
            glm.discouraged_efforts,
            vec![
                ReasoningSelection::Default,
                ReasoningSelection::High,
                ReasoningSelection::XHigh,
                ReasoningSelection::Max,
            ]
        );

        let openrouter = catalog
            .connections
            .iter()
            .find(|connection| connection.id.as_str() == "openrouter")
            .unwrap();
        assert_eq!(openrouter.display_name.as_ref(), "OpenRouter");
        assert_eq!(
            openrouter.default_base_url.as_ref(),
            "https://openrouter.ai/api/v1"
        );
        assert_eq!(
            openrouter.default_model.as_ref().map(ModelId::as_str),
            Some("openrouter/gpt-5.2")
        );

        let openrouter_default = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "openrouter/gpt-5.2")
            .unwrap();
        assert_eq!(
            openrouter_default
                .metadata_model
                .as_ref()
                .map(ModelId::as_str),
            Some("openai/gpt-5.2")
        );
        assert_eq!(
            openrouter_default.remote_model.as_deref(),
            Some("openai/gpt-5.2")
        );
        assert!(openrouter_default.is_default);

        let openrouter_haiku = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "openrouter/claude-haiku-4.5")
            .unwrap();
        assert_eq!(
            openrouter_haiku.prompt_cache_policy,
            Some(PromptCachePolicy::OpenRouterAnthropic)
        );

        let openai = catalog
            .connections
            .iter()
            .find(|connection| connection.id.as_str() == "openai")
            .unwrap();
        assert_eq!(openai.display_name.as_ref(), "OpenAI API");
        assert_eq!(
            openai.default_base_url.as_ref(),
            "https://api.openai.com/v1"
        );
        assert_eq!(openai.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
        assert_eq!(
            openai.default_model.as_ref().map(ModelId::as_str),
            Some("openai/gpt-5.6-sol")
        );
        assert_eq!(
            openai.reasoning_codec,
            Some(ReasoningCodec::OpenAiChatCompletions)
        );
        assert!(openai.prompt_cache);

        let openai_default = catalog
            .targets
            .iter()
            .find(|target| {
                target.connection.as_str() == "openai"
                    && target.model.as_str() == "openai/gpt-5.6-sol"
            })
            .unwrap();
        assert!(openai_default.is_default);
        assert!(openai_default.recommended);
        assert_eq!(openai_default.context_window, Some(272_000));

        let hosted_connections = [
            (
                "minimax",
                "MiniMax API",
                "https://api.minimax.io/anthropic",
                "MINIMAX_API_KEY",
                "minimax/MiniMax-M3",
            ),
            (
                "zai",
                "Z.AI API",
                "https://api.z.ai/api/paas/v4",
                "ZAI_API_KEY",
                "zai/glm-5.2",
            ),
            (
                "zai-coding-plan",
                "Z.AI Coding Plan",
                "https://api.z.ai/api/coding/paas/v4",
                "ZAI_CODING_PLAN_API_KEY",
                "zai-coding-plan/glm-5.2",
            ),
            (
                "moonshotai",
                "Moonshot AI API",
                "https://api.moonshot.ai/v1",
                "MOONSHOT_API_KEY",
                "moonshotai/kimi-k2.7-code",
            ),
            (
                "kimi-coding-plan",
                "Kimi Coding Plan",
                "https://api.kimi.com/coding/v1",
                "KIMI_CODING_PLAN_API_KEY",
                "kimi-coding-plan/kimi-for-coding",
            ),
        ];
        for (id, display_name, base_url, api_key_env, default_model) in hosted_connections {
            let connection = catalog
                .connections
                .iter()
                .find(|connection| connection.id.as_str() == id)
                .unwrap_or_else(|| panic!("missing builtin connection {id}"));
            assert_eq!(connection.display_name.as_ref(), display_name);
            assert_eq!(connection.default_base_url.as_ref(), base_url);
            assert_eq!(connection.api_key_env.as_deref(), Some(api_key_env));
            assert_eq!(
                connection.default_model.as_ref().map(ModelId::as_str),
                Some(default_model)
            );
        }

        let minimax_api = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "minimax/MiniMax-M3")
            .unwrap();
        let minimax_plan = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "minimax-coding-plan/MiniMax-M3")
            .unwrap();
        assert_eq!(minimax_api.context_window, Some(512_000));
        assert_eq!(minimax_plan.context_window, Some(1_000_000));
        assert_eq!(
            minimax_api
                .pricing
                .map(|pricing| pricing.input_micros_per_million),
            Some(300_000)
        );
        assert_eq!(
            minimax_plan
                .pricing
                .map(|pricing| pricing.input_micros_per_million),
            Some(0)
        );

        let zai_api = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "zai/glm-5.2")
            .unwrap();
        let zai_plan = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "zai-coding-plan/glm-5.2")
            .unwrap();
        assert_eq!(
            zai_api
                .pricing
                .map(|pricing| pricing.input_micros_per_million),
            Some(1_400_000)
        );
        assert_eq!(
            zai_plan.metadata_model.as_ref().map(ModelId::as_str),
            Some("zai-coding-plan/glm-5.2")
        );
        assert_eq!(
            zai_plan
                .pricing
                .map(|pricing| pricing.input_micros_per_million),
            Some(0)
        );

        let moonshot_api = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "moonshotai/kimi-k2.7-code")
            .unwrap();
        let kimi_plan = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "kimi-coding-plan/kimi-for-coding")
            .unwrap();
        assert_eq!(
            moonshot_api
                .pricing
                .map(|pricing| pricing.input_micros_per_million),
            Some(950_000)
        );
        assert_eq!(
            kimi_plan
                .pricing
                .map(|pricing| pricing.input_micros_per_million),
            Some(0)
        );
        assert_eq!(
            openai_default.recommended_effort,
            Some(ReasoningSelection::Medium)
        );

        let anthropic = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "anthropic/claude-sonnet-4-5")
            .unwrap();
        let expected_anthropic_budget = vec![ReasoningOption::BudgetTokens {
            min: Some(1024),
            max: None,
            default: 4096,
        }];
        assert_eq!(
            anthropic.reasoning_options.as_ref().unwrap(),
            &expected_anthropic_budget
        );
        assert_eq!(anthropic.max_tokens, Some(16_000));

        let anthropic_opus = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "anthropic/claude-opus-4-1")
            .unwrap();
        assert_eq!(
            anthropic_opus.reasoning_options.as_ref().unwrap(),
            &expected_anthropic_budget
        );

        let codex = catalog
            .targets
            .iter()
            .find(|target| target.model.as_str() == "openai/gpt-5.5")
            .unwrap();
        assert_eq!(
            codex.reasoning_options.as_ref().unwrap(),
            &vec![ReasoningOption::Effort(vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::XHigh,
            ])]
        );
        assert_eq!(
            codex.pricing,
            Some(ModelPricing::new(5_000_000, 30_000_000).with_cache_rates(Some(500_000), None))
        );
    }

    #[test]
    fn home_loader_creates_disabled_local_examples_without_registering_them() {
        let home = tempfile::TempDir::new().unwrap();
        let catalog = load_catalog_from_home(home.path()).unwrap();

        let provider_example = home.path().join("providers/example-local.toml");
        let model_example = home.path().join("models/example-local.toml");
        assert!(provider_example.exists());
        assert!(model_example.exists());
        assert!(
            std::fs::read_to_string(&provider_example)
                .unwrap()
                .contains("enabled = false")
        );
        assert!(
            std::fs::read_to_string(&model_example)
                .unwrap()
                .contains("context_window = 32768")
        );

        assert!(
            catalog
                .connections()
                .iter()
                .all(|connection| connection.id.as_str() != "local-example")
        );
        assert!(
            catalog
                .resolve(
                    &connection_id("local-example"),
                    &model_id("local-example/example-small")
                )
                .is_err()
        );
    }

    #[test]
    fn trusted_project_catalog_overrides_user_catalog() {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(home.path().join("providers")).unwrap();
        std::fs::create_dir_all(home.path().join("models")).unwrap();
        std::fs::create_dir_all(project.path().join(".bonsai/providers")).unwrap();
        std::fs::create_dir_all(project.path().join(".bonsai/models")).unwrap();
        std::fs::write(
            home.path().join("providers/openai.toml"),
            r#"[[connections]]
id = "openai"
default_base_url = "https://user.example/v1"
"#,
        )
        .unwrap();
        std::fs::write(
            home.path().join("models/openai.toml"),
            r#"[[targets]]
connection = "openai"
model = "openai/gpt-5.6"
context_window = 111000
"#,
        )
        .unwrap();
        std::fs::write(
            project.path().join(".bonsai/providers/openai.toml"),
            r#"[[connections]]
id = "openai"
default_base_url = "https://project.example/v1"
"#,
        )
        .unwrap();
        std::fs::write(
            project.path().join(".bonsai/models/openai.toml"),
            r#"[[targets]]
connection = "openai"
model = "openai/gpt-5.6"
context_window = 222000
"#,
        )
        .unwrap();

        let catalog =
            load_catalog_from_home_and_project(home.path(), Some(project.path())).unwrap();
        let resolved = catalog
            .resolve(&connection_id("openai"), &model_id("openai/gpt-5.6"))
            .unwrap();

        assert_eq!(
            resolved.default_base_url.as_ref(),
            "https://project.example/v1"
        );
        assert_eq!(resolved.context_window, Some(222_000));
        assert_eq!(catalog.trusted_project_root(), Some(project.path()));
    }

    #[test]
    fn restricted_catalog_loader_keeps_project_files_inert() {
        let home = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join(".bonsai/providers")).unwrap();
        std::fs::write(
            project.path().join(".bonsai/providers/project-only.toml"),
            r#"[[connections]]
id = "project-only"
display_name = "Project Only"
auth = "optional-api-key"
transport = "openai-chat"
default_base_url = "http://localhost:11434/v1"
"#,
        )
        .unwrap();

        let catalog = load_catalog_from_home_and_project(home.path(), None).unwrap();

        assert!(catalog.connection(&connection_id("project-only")).is_none());
        assert_eq!(catalog.trusted_project_root(), None);
    }

    #[test]
    fn live_availability_cache_roundtrips_and_records_canonical_mapping() {
        let home = tempfile::TempDir::new().unwrap();
        let catalog = load_catalog_from_home(home.path()).unwrap();
        let opencode = connection_id("opencode");

        catalog
            .write_live_availability(
                &opencode,
                LiveModelAvailability::from_remote_ids([
                    "qwen3.7-max".to_string(),
                    "unknown-live".to_string(),
                ]),
            )
            .unwrap();

        let path = home.path().join("cache/live-models/opencode.json");
        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["models"][0]["remote_model_id"], "qwen3.7-max");
        assert_eq!(value["models"][0]["model_id"], "opencode/qwen3.7-max");
        assert_eq!(value["models"][1]["remote_model_id"], "unknown-live");
        assert!(value["models"][1].get("model_id").is_none());

        let reloaded = load_catalog_from_home(home.path()).unwrap();
        assert_eq!(
            reloaded.available_models_for_connection(&opencode, Vec::new()),
            vec!["qwen3.7-max".to_string(), "unknown-live".to_string()]
        );
    }

    #[test]
    fn live_availability_cache_json_round_trips_across_formats() {
        // Pre-metadata cache files carry only remote_model_id (+ optional
        // model_id/context_window); they must keep deserializing unchanged.
        let legacy: LiveModelAvailability = serde_json::from_str(
            r#"{"models": [{"remote_model_id": "old-entry", "context_window": 4096}]}"#,
        )
        .unwrap();
        assert_eq!(legacy.models[0].remote_model_id.as_ref(), "old-entry");
        assert_eq!(legacy.models[0].display_name, None);
        assert_eq!(legacy.models[0].output_limit, None);
        assert!(legacy.models[0].features.is_empty());
        assert!(legacy.models[0].supported_reasoning.is_empty());
        assert_eq!(legacy.models[0].recommended_reasoning, None);
        assert_eq!(legacy.models[0].reasoning_codec, None);
        assert!(!legacy.models[0].use_responses_lite);

        let enriched = LiveModelAvailability {
            models: vec![{
                let mut model = AvailableModel::with_metadata(
                    "new-entry",
                    Some(131_072),
                    Some("New Entry".to_string()),
                    vec![ModelFeature::ToolCall],
                )
                .with_output_limit(Some(65_536))
                .with_reasoning(
                    vec![ReasoningSelection::Low, ReasoningSelection::Ultra],
                    Some(ReasoningSelection::Ultra),
                )
                .with_reasoning_codec(ReasoningCodec::AnthropicAdaptive);
                model.use_responses_lite = true;
                model
            }],
            ..LiveModelAvailability::default()
        };
        let json = serde_json::to_string(&enriched).unwrap();
        let reloaded: LiveModelAvailability = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded, enriched);
    }

    #[test]
    fn live_availability_fallback_context_fills_only_missing_models() {
        let availability = LiveModelAvailability {
            models: vec![
                AvailableModel::remote_with_context_window("api-sized", Some(65_536)),
                AvailableModel::remote("fallback-sized"),
            ],
            ..LiveModelAvailability::default()
        }
        .with_fallback_context_window(Some(32_768));

        assert_eq!(availability.models[0].context_window, Some(65_536));
        assert_eq!(availability.models[1].context_window, Some(32_768));
    }

    #[test]
    fn with_reasoning_dedups_and_filters_unsupported_recommendation() {
        let model = AvailableModel::remote("m").with_reasoning(
            vec![
                ReasoningSelection::Low,
                ReasoningSelection::Low,
                ReasoningSelection::High,
            ],
            Some(ReasoningSelection::Max),
        );
        assert_eq!(
            model.supported_reasoning,
            vec![ReasoningSelection::Low, ReasoningSelection::High]
        );
        // Max is not in the supported set, so the recommendation is dropped.
        assert_eq!(model.recommended_reasoning, None);

        let model = AvailableModel::remote("m").with_reasoning(
            vec![ReasoningSelection::Low],
            Some(ReasoningSelection::Default),
        );
        assert_eq!(
            model.recommended_reasoning,
            Some(ReasoningSelection::Default)
        );
    }

    #[test]
    fn normalize_reasoning_defers_to_static_metadata_without_live_list() {
        let unreported = AvailableModel::remote("m");
        assert_eq!(
            unreported.normalize_reasoning(ReasoningSelection::XHigh),
            None
        );

        let reported =
            AvailableModel::remote("m").with_reasoning(vec![ReasoningSelection::Low], None);
        assert_eq!(
            reported.normalize_reasoning(ReasoningSelection::Low),
            Some(ReasoningSelection::Low)
        );
        assert_eq!(
            reported.normalize_reasoning(ReasoningSelection::XHigh),
            Some(ReasoningSelection::Default)
        );
    }

    #[test]
    fn target_reasoning_efforts_accept_ultra() {
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "openai-compatible"
                    display_name = "OpenAI Compatible"
                    auth = "optional-api-key"
                    transport = "openai-chat"
                "#,
            )],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "openai-compatible"
                    model = "openai/apex"

                    [[targets.reasoning_options]]
                    type = "effort"
                    values = ["high", "ultra"]
                "#,
            )],
        )
        .unwrap();
        let catalog = ModelCatalog::from_spec(spec);
        let resolved = catalog
            .resolve(
                &connection_id("openai-compatible"),
                &model_id("openai/apex"),
            )
            .unwrap();
        assert!(
            resolved
                .reasoning_selections()
                .contains(&ReasoningSelection::Ultra)
        );
    }

    #[test]
    fn models_dev_drift_lines_flag_mismatches_and_respect_pinned() {
        let models_dev = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "openai": {
                "models": {
                  "gpt-5": {
                    "id": "gpt-5",
                    "limit": { "context": 400000, "output": 128000 },
                    "cost": { "input": 1.25, "output": 10 }
                  }
                }
              }
            }
            "#,
        )
        .unwrap();
        let models_dev_model = models_dev.model(&model_id("openai/gpt-5")).unwrap();
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "openai-compatible"
                    display_name = "OpenAI Compatible"
                    auth = "optional-api-key"
                    transport = "openai-chat"
                "#,
            )],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "openai-compatible"
                    model = "openai/gpt-5"
                    context_window = 200000
                    output_limit = 128000
                    pricing = { input_micros_per_million = 1250000, output_micros_per_million = 10000000 }

                    [[targets]]
                    connection = "openai-compatible"
                    model = "openai/pinned-gpt-5"
                    metadata_model = "openai/gpt-5"
                    pinned = true
                    context_window = 200000
                "#,
            )],
        )
        .unwrap();

        let drifting = spec
            .targets
            .iter()
            .find(|target| target.model.model() == "gpt-5")
            .unwrap();
        let lines = models_dev_drift_lines(drifting, models_dev_model);
        // Context drifts; output and pricing agree.
        assert_eq!(lines, vec!["context_window 200000 vs models.dev 400000"]);

        let pinned = spec
            .targets
            .iter()
            .find(|target| target.model.model() == "pinned-gpt-5")
            .unwrap();
        assert!(models_dev_drift_lines(pinned, models_dev_model).is_empty());
    }

    #[test]
    fn models_dev_refresh_notice_is_actionable_and_clears_on_success() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        catalog.record_models_dev_refresh_failure(&CatalogError::ModelsDevHttpStatus {
            url: "https://models.example/catalog.json".to_string(),
            status: 503,
        });

        let notice = catalog
            .models_dev_refresh_notice()
            .expect("refresh failure should be visible");
        assert!(notice.contains("HTTP 503"));
        assert!(notice.contains("using cached model metadata"));
        assert!(notice.contains("/refresh"));
        assert!(!notice.contains("models.example"));

        catalog.replace_models_dev_metadata(ModelsDevCatalog::default());
        assert!(catalog.models_dev_refresh_notice().is_none());
    }

    #[test]
    fn builtin_resolver_resolves_exact_targets() {
        struct EquivalenceCase {
            connection_id: &'static str,
            model_id: &'static str,
            remote_model: &'static str,
        }

        let catalog = ModelCatalog::load_builtin().unwrap();
        assert_eq!(catalog.list_resolved_models().unwrap().len(), 201);

        let cases = [
            EquivalenceCase {
                connection_id: "anthropic",
                model_id: "anthropic/claude-sonnet-4-5",
                remote_model: "claude-sonnet-4-5",
            },
            EquivalenceCase {
                connection_id: "anthropic",
                model_id: "anthropic/claude-opus-4-1",
                remote_model: "claude-opus-4-1",
            },
            EquivalenceCase {
                connection_id: "anthropic",
                model_id: "anthropic/claude-opus-5",
                remote_model: "claude-opus-5",
            },
            EquivalenceCase {
                connection_id: "anthropic",
                model_id: "anthropic/claude-haiku-4-5",
                remote_model: "claude-haiku-4-5",
            },
            EquivalenceCase {
                connection_id: "codex",
                model_id: "openai/gpt-5.5",
                remote_model: "gpt-5.5",
            },
            EquivalenceCase {
                connection_id: "codex",
                model_id: "openai/gpt-5.4",
                remote_model: "gpt-5.4",
            },
            EquivalenceCase {
                connection_id: "codex",
                model_id: "openai/gpt-5.4-mini",
                remote_model: "gpt-5.4-mini",
            },
            EquivalenceCase {
                connection_id: "minimax-coding-plan",
                model_id: "minimax-coding-plan/MiniMax-M3",
                remote_model: "MiniMax-M3",
            },
            EquivalenceCase {
                connection_id: "minimax-coding-plan",
                model_id: "minimax-coding-plan/MiniMax-M2.5",
                remote_model: "MiniMax-M2.5",
            },
            EquivalenceCase {
                connection_id: "minimax-coding-plan",
                model_id: "minimax-coding-plan/MiniMax-M2.5-highspeed",
                remote_model: "MiniMax-M2.5-highspeed",
            },
            EquivalenceCase {
                connection_id: "minimax-coding-plan",
                model_id: "minimax-coding-plan/MiniMax-M2.7",
                remote_model: "MiniMax-M2.7",
            },
            EquivalenceCase {
                connection_id: "minimax-coding-plan",
                model_id: "minimax-coding-plan/MiniMax-M2",
                remote_model: "MiniMax-M2",
            },
            EquivalenceCase {
                connection_id: "minimax-coding-plan",
                model_id: "minimax-coding-plan/MiniMax-M2.7-highspeed",
                remote_model: "MiniMax-M2.7-highspeed",
            },
            EquivalenceCase {
                connection_id: "minimax-coding-plan",
                model_id: "minimax-coding-plan/MiniMax-M2.1",
                remote_model: "MiniMax-M2.1",
            },
            EquivalenceCase {
                connection_id: "minimax",
                model_id: "minimax/MiniMax-M3",
                remote_model: "MiniMax-M3",
            },
            EquivalenceCase {
                connection_id: "zai",
                model_id: "zai/glm-5.2",
                remote_model: "glm-5.2",
            },
            EquivalenceCase {
                connection_id: "zai-coding-plan",
                model_id: "zai-coding-plan/glm-5.2",
                remote_model: "glm-5.2",
            },
            EquivalenceCase {
                connection_id: "moonshotai",
                model_id: "moonshotai/kimi-k2.7-code",
                remote_model: "kimi-k2.7-code",
            },
            EquivalenceCase {
                connection_id: "kimi-coding-plan",
                model_id: "kimi-coding-plan/kimi-for-coding",
                remote_model: "kimi-for-coding",
            },
            EquivalenceCase {
                connection_id: "openrouter",
                model_id: "openrouter/gpt-5.2",
                remote_model: "openai/gpt-5.2",
            },
            EquivalenceCase {
                connection_id: "opencode",
                model_id: "opencode/qwen3.7-max",
                remote_model: "qwen3.7-max",
            },
            EquivalenceCase {
                connection_id: "opencode",
                model_id: "opencode/glm-5.2",
                remote_model: "glm-5.2",
            },
            EquivalenceCase {
                connection_id: "opencode",
                model_id: "opencode/minimax-m3",
                remote_model: "minimax-m3",
            },
            EquivalenceCase {
                connection_id: "opencode",
                model_id: "opencode/deepseek-v4-flash",
                remote_model: "deepseek-v4-flash",
            },
        ];

        for case in cases {
            let resolved = catalog
                .resolve(&connection_id(case.connection_id), &model_id(case.model_id))
                .unwrap();
            let label = format!("{}:{}", case.connection_id, case.model_id);

            assert_eq!(
                resolved.remote_model_id.as_ref(),
                case.remote_model,
                "{label}"
            );
        }
    }

    #[test]
    fn resolver_applies_connection_defaults_and_remote_model_fallback() {
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "local-openai"
                    display_name = "Local OpenAI"
                    auth = "optional-api-key"
                    transport = "openai-chat"
                    default_endpoint_path = "chat/completions"
                    default_token_counter = "tiktoken"
                "#,
            )],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "local-openai"
                    model = "local/qwen3-coder"
                "#,
            )],
        )
        .unwrap();
        let catalog = ModelCatalog::from_spec(spec);

        let resolved = catalog
            .resolve(
                &connection_id("local-openai"),
                &model_id("local/qwen3-coder"),
            )
            .unwrap();

        assert_eq!(resolved.remote_model_id.as_ref(), "qwen3-coder");
        assert_eq!(resolved.transport, TransportProtocol::OpenAiChat);
        assert_eq!(resolved.reasoning_codec, ReasoningCodec::OpenAiCompatible);
        assert_eq!(resolved.endpoint_path.as_deref(), Some("chat/completions"));
        assert_eq!(resolved.context_window, None);
        assert_eq!(resolved.token_counter, Some(TokenCounterKind::Tiktoken));
        assert!(resolved.reasoning_options.is_empty());
        assert_eq!(resolved.source, ModelSource::BuiltIn);
    }

    #[test]
    fn builtin_catalog_resolves_qwen3_token_counters() {
        let catalog = ModelCatalog::load_builtin().unwrap();

        let opencode = catalog
            .resolve(
                &connection_id("opencode"),
                &model_id("opencode/qwen3.7-max"),
            )
            .unwrap();
        assert_eq!(opencode.token_counter, Some(TokenCounterKind::Qwen3));
        assert_eq!(
            opencode.prompt_cache_policy,
            PromptCachePolicy::RollingHistory,
            "IMPORTANT: OpenCode tool loops require volatile state after cached history"
        );

        let glm = catalog
            .resolve(&connection_id("opencode"), &model_id("opencode/glm-5.2"))
            .unwrap();
        assert_eq!(glm.prompt_cache_policy, PromptCachePolicy::RollingHistory);
        assert_eq!(glm.reasoning_codec, ReasoningCodec::ZaiReasoningEffort);
        assert_eq!(glm.output_limit, Some(12_000));
        assert_eq!(glm.recommended_effort, Some(ReasoningSelection::Off));
        assert!(
            glm.discouraged_efforts
                .contains(&ReasoningSelection::Default)
        );
        assert!(glm.discouraged_efforts.contains(&ReasoningSelection::Max));
        assert_eq!(
            glm.reasoning_options,
            vec![
                ReasoningOption::Toggle,
                ReasoningOption::Effort(vec![
                    ReasoningEffort::Minimal,
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::XHigh,
                    ReasoningEffort::Max,
                ]),
            ]
        );

        let zen = catalog
            .resolve(
                &connection_id("opencode-zen"),
                &model_id("opencode-zen/qwen3-coder"),
            )
            .unwrap();
        assert_eq!(zen.token_counter, Some(TokenCounterKind::Qwen3));
    }

    #[test]
    fn bare_target_model_name_is_display_alias_for_remote_model() {
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "openai-compatible"
                    display_name = "OpenAI Compatible"
                    auth = "optional-api-key"
                    transport = "openai-chat"
                    default_base_url = "http://localhost:1234/v1"
                    default_endpoint_path = "chat/completions"
                    default_token_counter = "heuristic"
                "#,
            )],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "openai-compatible"
                    model = "Qwen"
                    remote_model = "qwen/qwen3.6-35b-a3b"
                    context_window = 131072
                "#,
            )],
        )
        .unwrap();
        let catalog = ModelCatalog::from_spec(spec);
        let connection_id = connection_id("openai-compatible");

        let resolved = catalog
            .resolve(&connection_id, &model_id("openai-compatible/Qwen"))
            .unwrap();

        assert_eq!(resolved.model_id.as_str(), "openai-compatible/Qwen");
        assert_eq!(resolved.display_name.as_ref(), "Qwen");
        assert_eq!(resolved.remote_model_id.as_ref(), "qwen/qwen3.6-35b-a3b");
        assert_eq!(resolved.context_window, Some(131_072));
        assert_eq!(
            catalog.available_models_for_connection(&connection_id, Vec::new()),
            vec!["qwen/qwen3.6-35b-a3b".to_string()]
        );
        assert_eq!(
            catalog
                .resolve_connection_model(&connection_id, "Qwen")
                .map(|model| model.remote_model_id.to_string())
                .as_deref(),
            Some("qwen/qwen3.6-35b-a3b")
        );
    }

    #[test]
    fn available_models_preserve_multiple_targets_for_one_live_wire_model() {
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "openai"
                    display_name = "OpenAI"
                    auth = "api-key"
                    transport = "openai-chat"
                "#,
            )],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "openai"
                    model = "openai/gpt-current"
                    remote_model = "gpt-current"
                    aliases = ["gpt-current-alias"]
                    context_window = 272000

                    [[targets]]
                    connection = "openai"
                    model = "openai/gpt-current-1m"
                    metadata_model = "openai/gpt-current"
                    remote_model = "gpt-current"
                    context_window = 1050000
                "#,
            )],
        )
        .unwrap();
        let catalog = ModelCatalog::from_spec(spec);
        let connection_id = connection_id("openai");

        assert_eq!(
            catalog.available_models_for_connection(&connection_id, Vec::new()),
            vec![
                "gpt-current".to_string(),
                "openai/gpt-current-1m".to_string()
            ]
        );

        catalog
            .write_live_availability(
                &connection_id,
                LiveModelAvailability::from_remote_ids([
                    "gpt-current-alias".to_string(),
                    "gpt-current".to_string(),
                    "gpt-next".to_string(),
                ]),
            )
            .unwrap();

        assert_eq!(
            catalog.available_models_for_connection(&connection_id, Vec::new()),
            vec![
                "gpt-current-alias".to_string(),
                "openai/gpt-current-1m".to_string(),
                "gpt-next".to_string()
            ]
        );
    }

    #[test]
    fn discovered_model_without_target_resolves_via_shadow_from_models_dev() {
        // Session-23 shape: a connection that offers `kimi-k3`, no `[[targets]]`
        // block for it, but models.dev knows the model. Before shadow targets
        // this resolved to `None` → unpriced (frozen "spent") and vision-less.
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "moonshotai"
                    display_name = "Moonshot"
                    auth = "api-key"
                    transport = "openai-chat"
                    default_base_url = "https://api.moonshot.ai/v1"
                    default_endpoint_path = "chat/completions"
                    default_token_counter = "heuristic"
                "#,
            )],
            &[],
        )
        .unwrap()
        .with_models_dev(
            parse_models_dev_catalog(
                "test",
                r#"{
                    "moonshotai": {
                        "models": {
                            "kimi-k3": {
                                "id": "kimi-k3",
                                "name": "Kimi K3",
                                "tool_call": true,
                                "reasoning": true,
                                "attachment": true,
                                "modalities": { "input": ["text", "image"], "output": ["text"] },
                                "cost": { "input": 3, "output": 15, "cache_read": 0.3 },
                                "limit": { "context": 1048576, "output": 131072 }
                            }
                        }
                    }
                }"#,
            )
            .unwrap(),
        );
        let catalog = ModelCatalog::from_spec(spec);
        let connection = connection_id("moonshotai");

        let resolved = catalog
            .resolve_connection_model(&connection, "kimi-k3")
            .expect("discovered model resolves through a shadow target");

        assert_eq!(resolved.source, ModelSource::Discovered);
        assert_eq!(resolved.model_id.as_str(), "moonshotai/kimi-k3");
        assert_eq!(resolved.remote_model_id.as_ref(), "kimi-k3");
        // Pricing now flows from models.dev — the fix for the frozen counter.
        assert!(
            resolved.pricing.is_some(),
            "shadow target must carry models.dev pricing"
        );
        // Vision now flows from models.dev modalities — the fix for lost images.
        assert!(
            resolved.features.contains(&ModelFeature::Attachment),
            "shadow target must carry the model's vision capability"
        );
        assert_eq!(resolved.context_window, Some(1_048_576));

        // A model neither offered live nor known to models.dev stays `None` —
        // shadow resolution never fabricates a target for a typo'd name.
        assert!(
            catalog
                .resolve_connection_model(&connection, "kimi-k9-typo")
                .is_none()
        );
    }

    #[test]
    fn discovered_model_uses_connection_models_dev_namespace() {
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "opencode"
                    display_name = "OpenCode Go"
                    auth = "api-key"
                    transport = "openai-chat"
                    models_dev_provider = "opencode-go"
                    default_base_url = "https://opencode.ai/zen/go/v1"
                    default_endpoint_path = "chat/completions"
                    default_token_counter = "qwen3"

                    [[connections]]
                    id = "opencode-zen"
                    display_name = "OpenCode Zen"
                    auth = "api-key"
                    transport = "openai-chat"
                    models_dev_provider = "opencode"
                    default_base_url = "https://opencode.ai/zen/v1"
                    default_endpoint_path = "chat/completions"
                    default_token_counter = "tiktoken"
                "#,
            )],
            &[],
        )
        .unwrap()
        .with_models_dev(
            parse_models_dev_catalog(
                "test",
                r#"{
                    "opencode-go": {
                        "models": {
                            "hy3": {
                                "id": "hy3",
                                "name": "Hy3",
                                "tool_call": true,
                                "reasoning": true,
                                "structured_output": true,
                                "cost": {
                                    "input": 0.14,
                                    "output": 0.58,
                                    "cache_read": 0.035
                                },
                                "limit": { "context": 262144, "output": 65536 }
                            }
                        }
                    },
                    "opencode": {
                        "models": {
                            "future-zen": {
                                "id": "future-zen",
                                "name": "Future Zen",
                                "tool_call": true,
                                "reasoning": true,
                                "attachment": true,
                                "cost": {
                                    "input": 0.3,
                                    "output": 2.5,
                                    "cache_read": 0.03
                                },
                                "limit": { "context": 1048576, "output": 65536 }
                            }
                        }
                    }
                }"#,
            )
            .unwrap(),
        );
        let catalog = ModelCatalog::from_spec(spec);
        let connection = connection_id("opencode");
        catalog
            .write_live_availability(
                &connection,
                LiveModelAvailability::from_remote_ids([
                    "hy3".to_string(),
                    "hy3-preview".to_string(),
                ]),
            )
            .unwrap();

        let hy3 = catalog
            .resolve_connection_model(&connection, "hy3")
            .expect("remapped models.dev shadow resolves");
        assert_eq!(hy3.source, ModelSource::Discovered);
        assert!(!hy3.unverified);
        assert_eq!(hy3.model_id.as_str(), "opencode/hy3");
        assert_eq!(hy3.context_window, Some(262_144));
        assert_eq!(hy3.output_limit, Some(65_536));
        assert_eq!(
            hy3.pricing
                .expect("models.dev pricing")
                .input_micros_per_million,
            140_000
        );
        assert!(hy3.features.contains(&ModelFeature::ToolCall));
        assert!(hy3.features.contains(&ModelFeature::StructuredOutput));

        let preview = catalog
            .resolve_connection_model(&connection, "hy3-preview")
            .expect("live-only model remains selectable as an unverified shadow");
        assert_eq!(preview.source, ModelSource::Discovered);
        assert!(preview.unverified);
        assert!(preview.pricing.is_none());
        assert!(preview.context_window.is_none());

        let zen = connection_id("opencode-zen");
        catalog
            .write_live_availability(
                &zen,
                LiveModelAvailability::from_remote_ids(["future-zen".to_string()]),
            )
            .unwrap();
        let future_zen = catalog
            .resolve_connection_model(&zen, "future-zen")
            .expect("new Zen models resolve against the opencode namespace");
        assert!(!future_zen.unverified);
        assert_eq!(future_zen.model_id.as_str(), "opencode-zen/future-zen");
        assert_eq!(future_zen.context_window, Some(1_048_576));
        assert_eq!(future_zen.output_limit, Some(65_536));
        assert_eq!(
            future_zen.pricing.unwrap().input_micros_per_million,
            300_000
        );
        assert!(future_zen.features.contains(&ModelFeature::Attachment));
    }

    #[test]
    fn refreshed_metadata_overrides_unpinned_offline_fallbacks() {
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "opencode"
                    display_name = "OpenCode Go"
                    auth = "api-key"
                    transport = "openai-chat"
                "#,
            )],
            &[source(
                "opencode.toml",
                r#"
                    [[targets]]
                    connection = "opencode"
                    model = "opencode/qwen"
                    metadata_model = "opencode-go/qwen"
                    remote_model = "qwen"
                    display_name = "Bundled Qwen"
                    context_window = 100000
                    output_limit = 10000
                    features = ["tool-call"]
                    pricing = {
                        input_micros_per_million = 1000000,
                        output_micros_per_million = 2000000
                    }

                    [[targets.reasoning_options]]
                    type = "effort"
                    values = ["low"]
                "#,
            )],
        )
        .unwrap()
        .with_models_dev(
            parse_models_dev_catalog(
                "old",
                r#"{
                    "opencode-go": {
                        "models": {
                            "qwen": {
                                "id": "qwen",
                                "name": "Current Qwen",
                                "reasoning": true,
                                "reasoning_options": [
                                    { "type": "effort", "values": ["medium", "high"] }
                                ],
                                "cost": { "input": 1.5, "output": 3 },
                                "limit": { "context": 200000, "output": 20000 }
                            }
                        }
                    }
                }"#,
            )
            .unwrap(),
        );
        let catalog = ModelCatalog::from_spec(spec);
        let connection = connection_id("opencode");
        let model = model_id("opencode/qwen");

        let old = catalog.resolve(&connection, &model).unwrap();
        assert_eq!(old.context_window, Some(200_000));
        assert_eq!(old.output_limit, Some(20_000));
        assert_eq!(old.display_name.as_ref(), "Current Qwen");
        assert!(!old.features.contains(&ModelFeature::ToolCall));
        assert!(old.features.contains(&ModelFeature::Reasoning));
        assert_eq!(
            old.reasoning_options,
            vec![ReasoningOption::Effort(vec![
                ReasoningEffort::Medium,
                ReasoningEffort::High
            ])]
        );
        assert_eq!(
            old.pricing.as_ref().unwrap().input_micros_per_million,
            1_500_000
        );
        assert_eq!(
            old.metadata_sources.context_window,
            Some(ModelMetadataSource::ModelsDev)
        );
        assert_eq!(
            old.metadata_sources.pricing,
            Some(ModelMetadataSource::ModelsDev)
        );
        assert_eq!(
            old.metadata_sources.display_name,
            Some(ModelMetadataSource::ModelsDev)
        );
        assert_eq!(
            old.metadata_sources.reasoning,
            Some(ModelMetadataSource::ModelsDev)
        );
        assert_eq!(
            old.metadata_sources.features,
            Some(ModelMetadataSource::ModelsDev)
        );
        assert!(
            !old.catalog_drift.is_empty(),
            "the refreshed value wins, but offline-fallback drift remains visible"
        );

        catalog.replace_models_dev_metadata(
            parse_models_dev_catalog(
                "new",
                r#"{
                    "opencode-go": {
                        "models": {
                            "qwen": {
                                "id": "qwen",
                                "name": "Refreshed Qwen",
                                "reasoning": true,
                                "reasoning_options": [
                                    { "type": "effort", "values": ["high", "max"] }
                                ],
                                "cost": { "input": 2.5, "output": 5 },
                                "limit": { "context": 300000, "output": 30000 }
                            }
                        }
                    }
                }"#,
            )
            .unwrap(),
        );
        let refreshed = catalog.resolve(&connection, &model).unwrap();
        assert_eq!(refreshed.context_window, Some(300_000));
        assert_eq!(refreshed.output_limit, Some(30_000));
        assert_eq!(refreshed.display_name.as_ref(), "Refreshed Qwen");
        assert_eq!(
            refreshed.reasoning_options,
            vec![ReasoningOption::Effort(vec![
                ReasoningEffort::High,
                ReasoningEffort::Max
            ])]
        );
        assert_eq!(
            refreshed.pricing.as_ref().unwrap().input_micros_per_million,
            2_500_000
        );
        assert_eq!(
            refreshed.metadata_sources.pricing,
            Some(ModelMetadataSource::ModelsDev)
        );
    }

    #[test]
    fn pinned_metadata_remains_authoritative_over_refresh_sources() {
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "opencode"
                    display_name = "OpenCode Go"
                    auth = "api-key"
                    transport = "openai-chat"
                "#,
            )],
            &[source(
                "opencode.toml",
                r#"
                    [[targets]]
                    connection = "opencode"
                    model = "opencode/pinned"
                    metadata_model = "opencode-go/pinned"
                    remote_model = "pinned"
                    display_name = "Pinned"
                    pinned = true
                    context_window = 100000
                    output_limit = 10000
                    pricing = {
                        input_micros_per_million = 1000000,
                        output_micros_per_million = 2000000
                    }

                    [[targets.reasoning_options]]
                    type = "effort"
                    values = ["low"]
                "#,
            )],
        )
        .unwrap()
        .with_models_dev(
            parse_models_dev_catalog(
                "models-dev",
                r#"{
                    "opencode-go": {
                        "models": {
                            "pinned": {
                                "id": "pinned",
                                "name": "Models.dev",
                                "reasoning": true,
                                "reasoning_options": [
                                    { "type": "effort", "values": ["high"] }
                                ],
                                "cost": { "input": 3, "output": 4 },
                                "limit": { "context": 300000, "output": 30000 }
                            }
                        }
                    }
                }"#,
            )
            .unwrap(),
        );
        let catalog = ModelCatalog::from_spec(spec);
        let connection = connection_id("opencode");
        catalog
            .write_live_availability(
                &connection,
                LiveModelAvailability {
                    models: vec![
                        AvailableModel::with_metadata(
                            "pinned",
                            Some(400_000),
                            Some("Provider".to_string()),
                            vec![ModelFeature::Attachment],
                        )
                        .with_pricing(Some(ModelPricing::new(5_000_000, 6_000_000)))
                        .with_reasoning(
                            vec![ReasoningSelection::Max],
                            Some(ReasoningSelection::Max),
                        ),
                    ],
                    ..LiveModelAvailability::default()
                },
            )
            .unwrap();

        let resolved = catalog
            .resolve(&connection, &model_id("opencode/pinned"))
            .unwrap();
        assert_eq!(resolved.display_name.as_ref(), "Pinned");
        assert_eq!(resolved.context_window, Some(100_000));
        assert_eq!(resolved.output_limit, Some(10_000));
        assert_eq!(
            resolved.pricing.unwrap().input_micros_per_million,
            1_000_000
        );
        assert_eq!(
            resolved.reasoning_options,
            vec![ReasoningOption::Effort(vec![ReasoningEffort::Low])]
        );
        assert_eq!(
            resolved.metadata_sources.context_window,
            Some(ModelMetadataSource::Catalog)
        );
        assert_eq!(
            resolved.metadata_sources.reasoning,
            Some(ModelMetadataSource::Catalog)
        );
        assert!(resolved.catalog_drift.is_empty());
    }

    #[test]
    fn field_pins_preserve_caps_without_freezing_refreshed_prices() {
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "codex"
                    display_name = "Codex"
                    auth = "codex-cache"
                    transport = "codex-responses"
                "#,
            )],
            &[source(
                "codex.toml",
                r#"
                    [[targets]]
                    connection = "codex"
                    model = "openai/gpt-test"
                    remote_model = "gpt-test"
                    pinned_fields = ["context-window"]
                    context_window = 272000
                    output_limit = 10000
                    pricing = {
                        input_micros_per_million = 1000000,
                        output_micros_per_million = 2000000
                    }
                "#,
            )],
        )
        .unwrap()
        .with_models_dev(
            parse_models_dev_catalog(
                "models-dev",
                r#"{
                    "openai": {
                        "models": {
                            "gpt-test": {
                                "id": "gpt-test",
                                "cost": { "input": 3, "output": 4 },
                                "limit": { "context": 1050000, "output": 30000 }
                            }
                        }
                    }
                }"#,
            )
            .unwrap(),
        );
        let catalog = ModelCatalog::from_spec(spec);
        let connection = connection_id("codex");
        catalog
            .write_live_availability(
                &connection,
                LiveModelAvailability {
                    models: vec![AvailableModel::with_metadata(
                        "gpt-test",
                        Some(400_000),
                        None,
                        Vec::new(),
                    )],
                    ..LiveModelAvailability::default()
                },
            )
            .unwrap();

        let resolved = catalog
            .resolve(&connection, &model_id("openai/gpt-test"))
            .unwrap();
        assert_eq!(resolved.context_window, Some(272_000));
        assert_eq!(resolved.output_limit, Some(30_000));
        assert_eq!(
            resolved.pricing.unwrap().input_micros_per_million,
            3_000_000
        );
        assert_eq!(
            resolved.metadata_sources.context_window,
            Some(ModelMetadataSource::Catalog)
        );
        assert_eq!(
            resolved.metadata_sources.pricing,
            Some(ModelMetadataSource::ModelsDev)
        );
        assert_eq!(
            resolved.catalog_drift,
            vec![
                "output_limit 10000 vs models.dev 30000",
                "pricing differs from models.dev"
            ]
        );
    }

    #[test]
    fn shadow_target_uses_live_gateway_pricing_when_models_dev_is_silent() {
        // Gateway (OpenRouter-class) model with no target and no models.dev row,
        // but the live listing published a price — step 5's path.
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "openrouter"
                    display_name = "OpenRouter"
                    auth = "api-key"
                    transport = "openai-chat"
                    default_base_url = "https://openrouter.ai/api/v1"
                    default_endpoint_path = "chat/completions"
                    default_token_counter = "heuristic"
                "#,
            )],
            &[],
        )
        .unwrap();
        let catalog = ModelCatalog::from_spec(spec);
        let connection = connection_id("openrouter");
        catalog
            .write_live_availability(
                &connection,
                LiveModelAvailability {
                    models: vec![
                        AvailableModel::with_metadata(
                            "poolside/laguna-m.1",
                            None,
                            None,
                            vec![ModelFeature::ToolCall],
                        )
                        .with_pricing(Some(ModelPricing::new(1_000_000, 2_000_000))),
                    ],
                    ..LiveModelAvailability::default()
                },
            )
            .unwrap();

        let resolved = catalog
            .resolve_connection_model(&connection, "poolside/laguna-m.1")
            .expect("discovered gateway model resolves via shadow");

        assert_eq!(resolved.source, ModelSource::Discovered);
        let pricing = resolved
            .pricing
            .expect("live gateway pricing flows through the shadow target");
        assert_eq!(pricing.input_micros_per_million, 1_000_000);
        assert_eq!(pricing.output_micros_per_million, 2_000_000);
        assert_eq!(
            resolved.metadata_sources.pricing,
            Some(ModelMetadataSource::Provider)
        );
    }

    #[test]
    fn shadow_target_uses_live_output_limit_and_reasoning_codec() {
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "anthropic"
                    display_name = "Anthropic API"
                    auth = "api-key"
                    transport = "anthropic-messages"
                    default_base_url = "https://api.anthropic.com"
                    default_endpoint_path = "v1/messages"
                    default_token_counter = "anthropic-count-tokens"
                "#,
            )],
            &[],
        )
        .unwrap();
        let catalog = ModelCatalog::from_spec(spec);
        let connection = connection_id("anthropic");
        catalog
            .write_live_availability(
                &connection,
                LiveModelAvailability {
                    models: vec![
                        AvailableModel::with_metadata(
                            "claude-future",
                            Some(1_000_000),
                            Some("Claude Future".to_string()),
                            vec![ModelFeature::ToolCall, ModelFeature::Reasoning],
                        )
                        .with_output_limit(Some(128_000))
                        .with_reasoning(
                            vec![
                                ReasoningSelection::Off,
                                ReasoningSelection::Low,
                                ReasoningSelection::High,
                            ],
                            Some(ReasoningSelection::High),
                        )
                        .with_reasoning_codec(ReasoningCodec::AnthropicAdaptive),
                    ],
                    ..LiveModelAvailability::default()
                },
            )
            .unwrap();

        let resolved = catalog
            .resolve_connection_model(&connection, "claude-future")
            .expect("live Anthropic model resolves through a shadow target");

        assert_eq!(resolved.source, ModelSource::Discovered);
        assert_eq!(resolved.context_window, Some(1_000_000));
        assert_eq!(resolved.output_limit, Some(128_000));
        assert_eq!(resolved.reasoning_codec, ReasoningCodec::AnthropicAdaptive);
        assert_eq!(
            resolved.reasoning_selections(),
            vec![
                ReasoningSelection::Default,
                ReasoningSelection::Off,
                ReasoningSelection::Low,
                ReasoningSelection::High,
            ]
        );
        assert_eq!(resolved.recommended_effort, Some(ReasoningSelection::High));
        assert_eq!(
            resolved.metadata_sources.output_limit,
            Some(ModelMetadataSource::Provider)
        );
    }

    #[test]
    fn resolver_reports_missing_connection_or_target() {
        let catalog = ModelCatalog::load_builtin().unwrap();

        let missing_connection =
            catalog.resolve(&connection_id("missing"), &model_id("openai/gpt"));
        assert!(matches!(
            missing_connection,
            Err(CatalogError::UnknownConnection { id }) if id.as_str() == "missing"
        ));

        let missing_target =
            catalog.resolve(&connection_id("codex"), &model_id("openai/not-configured"));
        assert!(matches!(
            missing_target,
            Err(CatalogError::UnknownTarget {
                connection_id,
                model_id,
            }) if connection_id.as_str() == "codex" && model_id.as_str() == "openai/not-configured"
        ));
    }

    #[test]
    fn resolver_maps_connection_model_pairs_to_catalog_targets() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let opencode_id = connection_id("opencode");
        let minimax_id = connection_id("minimax-coding-plan");
        let openrouter_id = connection_id("openrouter");
        let codex_id = connection_id("codex");

        let opencode = catalog
            .resolve_connection_model(&opencode_id, "qwen3.7-max")
            .unwrap();
        assert_eq!(opencode.connection_id.as_str(), "opencode");
        assert_eq!(opencode.model_id.as_str(), "opencode/qwen3.7-max");
        assert_eq!(opencode.remote_model_id.as_ref(), "qwen3.7-max");

        let minimax = catalog
            .resolve_connection_model(&minimax_id, "MiniMax-M3")
            .unwrap();
        assert_eq!(minimax.connection_id.as_str(), "minimax-coding-plan");
        assert_eq!(minimax.model_id.as_str(), "minimax-coding-plan/MiniMax-M3");

        let openrouter = catalog
            .resolve_connection_model(&openrouter_id, "openai/gpt-5.2")
            .unwrap();
        assert_eq!(openrouter.connection_id.as_str(), "openrouter");
        assert_eq!(openrouter.model_id.as_str(), "openrouter/gpt-5.2");
        assert_eq!(openrouter.remote_model_id.as_ref(), "openai/gpt-5.2");

        let openrouter_haiku = catalog
            .resolve_connection_model(&openrouter_id, "anthropic/claude-haiku-4.5")
            .unwrap();
        assert_eq!(
            openrouter_haiku.model_id.as_str(),
            "openrouter/claude-haiku-4.5"
        );
        assert_eq!(
            openrouter_haiku.prompt_cache_policy,
            PromptCachePolicy::OpenRouterAnthropic
        );

        let codex = catalog
            .resolve_connection_model(&codex_id, "openai/gpt-5.5")
            .unwrap();
        assert_eq!(codex.connection_id.as_str(), "codex");
        assert_eq!(codex.model_id.as_str(), "openai/gpt-5.5");
    }

    #[test]
    fn prompt_cache_policy_matrix_preserves_provider_boundaries() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let cases = [
            ("opencode", "qwen3.7-max", PromptCachePolicy::RollingHistory),
            (
                "opencode",
                "deepseek-v4-pro",
                PromptCachePolicy::RollingHistory,
            ),
            ("opencode", "glm-5.2", PromptCachePolicy::RollingHistory),
            (
                "openrouter",
                "anthropic/claude-haiku-4.5",
                PromptCachePolicy::OpenRouterAnthropic,
            ),
            (
                "openrouter",
                "openai/gpt-5.2",
                PromptCachePolicy::TransportDefault,
            ),
            (
                "anthropic",
                "claude-haiku-4-5",
                PromptCachePolicy::TransportDefault,
            ),
            (
                "minimax-coding-plan",
                "MiniMax-M3",
                PromptCachePolicy::TransportDefault,
            ),
            (
                "openai",
                "openai/gpt-5.6-sol",
                PromptCachePolicy::TransportDefault,
            ),
            (
                "codex",
                "openai/gpt-5.6-sol",
                PromptCachePolicy::TransportDefault,
            ),
        ];

        for (connection, model, expected) in cases {
            let resolved = catalog
                .resolve_connection_model(&connection_id(connection), model)
                .unwrap_or_else(|| panic!("{connection}/{model} must resolve"));
            assert_eq!(
                resolved.prompt_cache_policy, expected,
                "IMPORTANT: cache policy drifted for {connection}/{model}"
            );
        }
    }

    #[test]
    fn openai_builtin_matches_current_chat_completions_lineup() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let openai_id = connection_id("openai");
        let connection = catalog.connection(&openai_id).unwrap();
        assert_eq!(
            connection.default_model.as_ref().map(ModelId::as_str),
            Some("openai/gpt-5.6-sol")
        );
        assert_eq!(
            catalog.available_models_for_connection(&openai_id, Vec::new()),
            vec![
                "gpt-5.6-sol",
                "openai/gpt-5.6-1m",
                "gpt-5.6-terra",
                "openai/gpt-5.6-terra-1m",
                "gpt-5.6-luna",
                "openai/gpt-5.6-luna-1m",
                "gpt-5.5",
                "openai/gpt-5.5-1m",
                "gpt-5.4",
                "openai/gpt-5.4-1m",
                "gpt-5.4-mini",
                "gpt-5.4-nano",
            ]
        );

        let cases = [
            ("openai/gpt-5.6-sol", 272_000, 5_000_000, 30_000_000),
            ("openai/gpt-5.6-1m", 1_050_000, 10_000_000, 45_000_000),
            ("openai/gpt-5.6-terra", 272_000, 2_500_000, 15_000_000),
            ("openai/gpt-5.6-terra-1m", 1_050_000, 5_000_000, 22_500_000),
            ("openai/gpt-5.6-luna", 272_000, 1_000_000, 6_000_000),
            ("openai/gpt-5.6-luna-1m", 1_050_000, 2_000_000, 9_000_000),
            ("openai/gpt-5.5", 272_000, 5_000_000, 30_000_000),
            ("openai/gpt-5.5-1m", 1_050_000, 10_000_000, 45_000_000),
            ("openai/gpt-5.4", 272_000, 2_500_000, 15_000_000),
            ("openai/gpt-5.4-1m", 1_050_000, 5_000_000, 22_500_000),
            ("openai/gpt-5.4-mini", 400_000, 750_000, 4_500_000),
            ("openai/gpt-5.4-nano", 400_000, 200_000, 1_250_000),
        ];
        for (model, context, input, output) in cases {
            let resolved = catalog
                .resolve(&openai_id, &model_id(model))
                .unwrap_or_else(|_| panic!("{model} must resolve"));
            assert_eq!(resolved.context_window, Some(context), "{model}");
            assert_eq!(resolved.output_limit, Some(128_000), "{model}");
            assert_eq!(
                resolved.pricing.map(|pricing| (
                    pricing.input_micros_per_million,
                    pricing.output_micros_per_million
                )),
                Some((input, output)),
                "{model}"
            );
            for feature in [
                ModelFeature::ToolCall,
                ModelFeature::Reasoning,
                ModelFeature::StructuredOutput,
                ModelFeature::Attachment,
            ] {
                assert!(resolved.features.contains(&feature), "{model}: {feature:?}");
            }
            assert!(
                resolved
                    .reasoning_selections()
                    .contains(&ReasoningSelection::Off),
                "{model}"
            );
        }

        let legacy = catalog
            .resolve_connection_model(&openai_id, "openai/gpt-5.6")
            .expect("the previous default selector must keep working");
        assert_eq!(legacy.model_id.as_str(), "openai/gpt-5.6-sol");
        assert_eq!(legacy.remote_model_id.as_ref(), "gpt-5.6-sol");
    }

    #[test]
    fn gemini_builtin_exposes_current_lineup_context_profiles_and_tier_pricing() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let gemini_id = connection_id("gemini");
        let connection = catalog.connection(&gemini_id).unwrap();
        assert_eq!(connection.discovery, DiscoveryKind::Gemini);
        assert!(!connection.prompt_cache);
        assert_eq!(
            connection.default_model.as_ref().map(ModelId::as_str),
            Some("gemini/gemini-3.1-pro-preview-customtools")
        );
        assert_eq!(
            catalog.available_models_for_connection(&gemini_id, Vec::new()),
            vec![
                "gemini-3.1-pro-preview-customtools",
                "gemini/gemini-3.1-pro-preview-customtools-1m",
                "gemini-3.1-pro-preview",
                "gemini/gemini-3.1-pro-preview-1m",
                "gemini-3.6-flash",
                "gemini-3.5-flash",
                "gemini-3.5-flash-lite",
                "gemini-3.1-flash-lite",
                "gemini-2.5-pro",
                "gemini/gemini-2.5-pro-1m",
                "gemini-2.5-flash",
            ]
        );

        let short = catalog
            .resolve(
                &gemini_id,
                &model_id("gemini/gemini-3.1-pro-preview-customtools"),
            )
            .unwrap();
        let long = catalog
            .resolve(
                &gemini_id,
                &model_id("gemini/gemini-3.1-pro-preview-customtools-1m"),
            )
            .unwrap();
        assert_eq!(short.remote_model_id, long.remote_model_id);
        assert_eq!(short.context_window, Some(200_000));
        assert_eq!(long.context_window, Some(1_048_576));
        assert_eq!(
            short
                .pricing
                .map(|pricing| pricing.input_micros_per_million),
            Some(2_000_000)
        );
        assert_eq!(
            long.pricing.map(|pricing| pricing.input_micros_per_million),
            Some(4_000_000)
        );
        let schedule = long.pricing_schedule.as_ref().expect("tier pricing");
        assert_eq!(
            schedule
                .pricing_for_prompt_tokens(200_000)
                .input_micros_per_million,
            2_000_000
        );
        assert_eq!(
            schedule
                .pricing_for_prompt_tokens(200_001)
                .input_micros_per_million,
            4_000_000
        );
        assert!(
            !long
                .reasoning_selections()
                .contains(&ReasoningSelection::Off)
        );

        let flash = catalog
            .resolve(&gemini_id, &model_id("gemini/gemini-3.6-flash"))
            .unwrap();
        assert!(
            flash
                .reasoning_selections()
                .contains(&ReasoningSelection::Minimal)
        );
        for feature in [
            ModelFeature::ToolCall,
            ModelFeature::Reasoning,
            ModelFeature::StructuredOutput,
            ModelFeature::Attachment,
        ] {
            assert!(flash.features.contains(&feature), "{feature:?}");
        }

        let legacy_flash = catalog
            .resolve(&gemini_id, &model_id("gemini/gemini-2.5-flash"))
            .unwrap();
        assert!(
            legacy_flash
                .reasoning_selections()
                .contains(&ReasoningSelection::Off)
        );
    }

    #[test]
    fn deepseek_builtin_refreshes_metadata_without_losing_safety_caps() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let deepseek_id = connection_id("deepseek");
        let connection = catalog.connection(&deepseek_id).unwrap();
        assert_eq!(
            connection.default_model.as_ref().map(ModelId::as_str),
            Some("deepseek/deepseek-v4-flash")
        );
        assert_eq!(
            catalog.available_models_for_connection(&deepseek_id, Vec::new()),
            vec!["deepseek-v4-flash", "deepseek-v4-pro"]
        );

        for (model, input, output, cache_read) in [
            ("deepseek/deepseek-v4-flash", 140_000, 280_000, 2_800),
            ("deepseek/deepseek-v4-pro", 435_000, 870_000, 3_625),
        ] {
            let resolved = catalog
                .resolve(&deepseek_id, &model_id(model))
                .unwrap_or_else(|_| panic!("{model} must resolve"));
            assert_eq!(resolved.context_window, Some(1_000_000), "{model}");
            assert_eq!(resolved.output_limit, Some(32_000), "{model}");
            assert_eq!(
                resolved.reasoning_codec,
                ReasoningCodec::ZaiThinking,
                "{model}"
            );
            assert_eq!(
                resolved.pricing,
                Some(ModelPricing {
                    input_micros_per_million: input,
                    output_micros_per_million: output,
                    cache_read_micros_per_million: Some(cache_read),
                    cache_write_micros_per_million: None,
                }),
                "{model}"
            );
            assert_eq!(
                resolved.reasoning_selections(),
                vec![
                    ReasoningSelection::Default,
                    ReasoningSelection::Off,
                    ReasoningSelection::High,
                    ReasoningSelection::Max,
                ],
                "{model}"
            );
            for feature in [
                ModelFeature::ToolCall,
                ModelFeature::Reasoning,
                ModelFeature::StructuredOutput,
            ] {
                assert!(resolved.features.contains(&feature), "{model}: {feature:?}");
            }
            assert_eq!(
                resolved.prompt_cache_policy,
                PromptCachePolicy::RollingHistory
            );
        }

        catalog
            .write_live_availability(
                &deepseek_id,
                LiveModelAvailability::from_remote_ids([
                    "deepseek-v4-flash".to_string(),
                    "deepseek-v4-turbo".to_string(),
                ]),
            )
            .unwrap();
        assert_eq!(
            catalog.available_models_for_connection(&deepseek_id, Vec::new()),
            vec!["deepseek-v4-flash", "deepseek-v4-turbo"],
            "live refresh must add unseen models and retire absent ones"
        );

        catalog.replace_models_dev_metadata(
            parse_models_dev_catalog(
                "deepseek-refresh.json",
                r#"{
                    "deepseek": {
                        "models": {
                            "deepseek-v4-flash": {
                                "id": "deepseek-v4-flash",
                                "name": "DeepSeek V4 Flash refreshed",
                                "reasoning": true,
                                "reasoning_options": [
                                    { "type": "effort", "values": ["minimal", "xhigh"] }
                                ],
                                "tool_call": true,
                                "structured_output": true,
                                "attachment": true,
                                "cost": {
                                    "input": 0.15,
                                    "output": 0.30,
                                    "cache_read": 0.003
                                },
                                "limit": { "context": 1100000, "output": 384000 }
                            }
                        }
                    }
                }"#,
            )
            .unwrap(),
        );
        let refreshed = catalog
            .resolve(&deepseek_id, &model_id("deepseek/deepseek-v4-flash"))
            .unwrap();

        assert_eq!(refreshed.context_window, Some(1_100_000));
        assert_eq!(
            refreshed.output_limit,
            Some(32_000),
            "the deliberate rumination cap is pinned"
        );
        assert_eq!(
            refreshed.pricing,
            Some(ModelPricing {
                input_micros_per_million: 150_000,
                output_micros_per_million: 300_000,
                cache_read_micros_per_million: Some(3_000),
                cache_write_micros_per_million: None,
            }),
            "unlike the safety cap, prices must update through /refresh"
        );
        assert!(
            refreshed.features.contains(&ModelFeature::Attachment),
            "capabilities remain refreshable"
        );
        assert_eq!(
            refreshed.reasoning_selections(),
            vec![
                ReasoningSelection::Default,
                ReasoningSelection::Off,
                ReasoningSelection::High,
                ReasoningSelection::Max,
            ],
            "provider aliases must not invent extra effective effort levels"
        );
        assert_eq!(
            refreshed.metadata_sources.pricing,
            Some(ModelMetadataSource::ModelsDev)
        );
    }

    #[test]
    fn codex_builtin_matches_current_live_working_lineup() {
        let catalog = ModelCatalog::load_builtin().unwrap();
        let codex_id = connection_id("codex");
        let connection = catalog.connection(&codex_id).unwrap();
        assert_eq!(
            connection.default_model.as_ref().map(ModelId::as_str),
            Some("openai/gpt-5.6-sol")
        );
        assert_eq!(
            catalog.target_remote_models_for_connection(&codex_id),
            vec![
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna",
                "gpt-5.5",
                "gpt-5.4",
                "gpt-5.4-mini",
            ]
        );
        for remote_model in [
            "gpt-5.6-sol",
            "gpt-5.6-terra",
            "gpt-5.6-luna",
            "gpt-5.5",
            "gpt-5.4",
            "gpt-5.4-mini",
        ] {
            let resolved = catalog
                .resolve_connection_model(&codex_id, remote_model)
                .unwrap_or_else(|| panic!("{remote_model} must resolve"));
            assert_eq!(resolved.context_window, Some(272_000));
            assert!(
                !resolved
                    .reasoning_selections()
                    .contains(&ReasoningSelection::Off)
            );
        }
        let legacy = catalog
            .resolve_connection_model(&codex_id, "openai/gpt-5.5-1m")
            .expect("legacy 1M selector must route existing sessions safely");
        assert_eq!(legacy.model_id.as_str(), "openai/gpt-5.5");
        assert_eq!(legacy.remote_model_id.as_ref(), "gpt-5.5");
    }

    #[test]
    fn user_dirs_add_connection_and_model_in_filename_order() {
        let temp = tempfile::TempDir::new().unwrap();
        let provider_dir = temp.path().join("providers");
        let model_dir = temp.path().join("models");
        std::fs::create_dir_all(&provider_dir).unwrap();
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(
            provider_dir.join("z-local.toml"),
            r#"
                [[connections]]
                id = "local-openai"
                display_name = "Local OpenAI"
                auth = "optional-api-key"
                transport = "openai-chat"
                default_base_url = "http://localhost:11434/v1"
                default_endpoint_path = "chat/completions"
                default_token_counter = "tiktoken"
            "#,
        )
        .unwrap();
        std::fs::write(
            model_dir.join("b-target.toml"),
            r#"
                [[targets]]
                connection = "local-openai"
                model = "local/qwen3-coder"
                remote_model = "qwen3-coder"
                context_window = 262144
                features = ["tool-call", "reasoning"]

                [[targets.reasoning_options]]
                type = "toggle"
            "#,
        )
        .unwrap();

        let spec = load_catalog_with_user_dirs(&provider_dir, &model_dir).unwrap();
        let catalog = ModelCatalog::from_spec(spec);
        let resolved = catalog
            .resolve(
                &connection_id("local-openai"),
                &model_id("local/qwen3-coder"),
            )
            .unwrap();

        assert_eq!(resolved.remote_model_id.as_ref(), "qwen3-coder");
        assert_eq!(resolved.context_window, Some(262_144));
        assert_eq!(
            resolved.reasoning_selections(),
            vec![
                ReasoningSelection::Default,
                ReasoningSelection::Off,
                ReasoningSelection::On
            ]
        );
        assert_eq!(
            resolved.features,
            vec![ModelFeature::ToolCall, ModelFeature::Reasoning]
        );
        assert_eq!(
            catalog
                .list_resolved_models()
                .unwrap()
                .last()
                .map(|model| model.model_id.as_str().to_string())
                .as_deref(),
            Some("local/qwen3-coder")
        );
    }

    #[test]
    fn user_target_patch_overrides_only_declared_fields() {
        let temp = tempfile::TempDir::new().unwrap();
        let provider_dir = temp.path().join("providers");
        let model_dir = temp.path().join("models");
        std::fs::create_dir_all(&provider_dir).unwrap();
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(
            model_dir.join("override.toml"),
            r#"
                [[targets]]
                connection = "codex"
                model = "openai/gpt-5.5"
                reasoning_options = []
                context_window = 256000
            "#,
        )
        .unwrap();

        let models_dev = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "openai": {
                "models": {
                  "gpt-5.5": {
                    "id": "gpt-5.5",
                    "name": "GPT-5.5",
                    "limit": { "context": 1050000, "output": 128000 },
                    "cost": { "input": 5, "output": 30 }
                  }
                }
              }
            }
            "#,
        )
        .unwrap();
        let spec = load_catalog_with_user_dirs(&provider_dir, &model_dir)
            .unwrap()
            .with_models_dev(models_dev);
        let catalog = ModelCatalog::from_spec(spec);
        let resolved = catalog
            .resolve(&connection_id("codex"), &model_id("openai/gpt-5.5"))
            .unwrap();

        assert_eq!(resolved.context_window, Some(256_000));
        assert!(resolved.reasoning_options.is_empty());
        assert_eq!(
            resolved.pricing,
            Some(ModelPricing::new(5_000_000, 30_000_000)),
            "omitted pricing should refresh from models.dev"
        );
    }

    #[test]
    fn resolver_applies_refreshed_price_tier_for_long_context_target() {
        let models_dev = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "openai": {
                "models": {
                  "gpt-current": {
                    "id": "gpt-current",
                    "cost": {
                      "input": 5,
                      "output": 30,
                      "tiers": [{
                        "input": 10,
                        "output": 45,
                        "tier": { "type": "context", "size": 272000 }
                      }]
                    }
                  }
                }
              }
            }
            "#,
        )
        .unwrap();
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "openai"
                    display_name = "OpenAI"
                    auth = "api-key"
                    transport = "openai-chat"
                "#,
            )],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "openai"
                    model = "openai/gpt-current"
                    pinned_fields = ["context-window"]
                    context_window = 272000
                    pricing = { input_micros_per_million = 1, output_micros_per_million = 1 }

                    [[targets]]
                    connection = "openai"
                    model = "openai/gpt-current-1m"
                    metadata_model = "openai/gpt-current"
                    remote_model = "gpt-current"
                    pinned_fields = ["context-window"]
                    context_window = 1050000
                    pricing = { input_micros_per_million = 1, output_micros_per_million = 1 }
                "#,
            )],
        )
        .unwrap()
        .with_models_dev(models_dev);
        let catalog = ModelCatalog::from_spec(spec);

        let short = catalog
            .resolve(&connection_id("openai"), &model_id("openai/gpt-current"))
            .unwrap();
        let long = catalog
            .resolve(&connection_id("openai"), &model_id("openai/gpt-current-1m"))
            .unwrap();

        assert_eq!(
            short.pricing,
            Some(ModelPricing::new(5_000_000, 30_000_000))
        );
        assert_eq!(
            long.pricing,
            Some(ModelPricing::new(10_000_000, 45_000_000))
        );
        assert_eq!(
            short.metadata_sources.pricing,
            Some(ModelMetadataSource::ModelsDev)
        );
        assert_eq!(
            long.metadata_sources.pricing,
            Some(ModelMetadataSource::ModelsDev)
        );
    }

    #[test]
    fn user_connection_patch_overrides_only_declared_fields() {
        let temp = tempfile::TempDir::new().unwrap();
        let provider_dir = temp.path().join("providers");
        let model_dir = temp.path().join("models");
        std::fs::create_dir_all(&provider_dir).unwrap();
        std::fs::create_dir_all(&model_dir).unwrap();
        std::fs::write(
            provider_dir.join("codex.toml"),
            r#"
                [[connections]]
                id = "codex"
                display_name = "Codex Work"
                default_base_url = "https://example.test/codex"
            "#,
        )
        .unwrap();

        let spec = load_catalog_with_user_dirs(&provider_dir, &model_dir).unwrap();
        let codex = spec
            .connections
            .iter()
            .find(|connection| connection.id.as_str() == "codex")
            .unwrap();

        assert_eq!(codex.display_name.as_ref(), "Codex Work");
        assert_eq!(
            codex.default_base_url.as_ref(),
            "https://example.test/codex"
        );
        assert_eq!(codex.auth, ConnectionAuth::CodexCache);
        assert_eq!(codex.transport, TransportProtocol::CodexResponses);
    }

    #[test]
    fn duplicate_user_target_overrides_are_rejected() {
        let temp = tempfile::TempDir::new().unwrap();
        let provider_dir = temp.path().join("providers");
        let model_dir = temp.path().join("models");
        std::fs::create_dir_all(&provider_dir).unwrap();
        std::fs::create_dir_all(&model_dir).unwrap();
        for file in ["a.toml", "b.toml"] {
            std::fs::write(
                model_dir.join(file),
                r#"
                    [[targets]]
                    connection = "codex"
                    model = "openai/gpt-5.5"
                "#,
            )
            .unwrap();
        }

        let result = load_catalog_with_user_dirs(&provider_dir, &model_dir);

        assert!(matches!(
            result,
            Err(CatalogError::DuplicateTarget {
                connection_id,
                model_id,
            }) if connection_id.as_str() == "codex" && model_id.as_str() == "openai/gpt-5.5"
        ));
    }

    #[test]
    fn resolver_fills_missing_metadata_from_models_dev_cache() {
        let models_dev = parse_models_dev_catalog(
            "models-dev.json",
            r#"
            {
              "openai": {
                "models": {
                  "gpt-5": {
                    "id": "gpt-5",
                    "name": "GPT-5",
                    "reasoning": true,
                    "tool_call": true,
                    "limit": { "context": 400000, "output": 128000 },
                    "cost": { "input": 1.25, "output": 10 }
                  }
                }
              }
            }
            "#,
        )
        .unwrap();
        let spec = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "openai-compatible"
                    display_name = "OpenAI Compatible"
                    auth = "optional-api-key"
                    transport = "openai-chat"
                    default_endpoint_path = "chat/completions"
                "#,
            )],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "openai-compatible"
                    model = "openai/gpt-5"
                "#,
            )],
        )
        .unwrap()
        .with_models_dev(models_dev);
        let catalog = ModelCatalog::from_spec(spec);

        let resolved = catalog
            .resolve(
                &connection_id("openai-compatible"),
                &model_id("openai/gpt-5"),
            )
            .unwrap();

        assert_eq!(resolved.display_name.as_ref(), "GPT-5");
        assert_eq!(resolved.context_window, Some(400_000));
        assert_eq!(resolved.output_limit, Some(128_000));
        assert_eq!(
            resolved.pricing,
            Some(ModelPricing::new(1_250_000, 10_000_000))
        );
        assert_eq!(
            resolved.features,
            vec![ModelFeature::ToolCall, ModelFeature::Reasoning]
        );
    }

    #[test]
    fn duplicate_connection_ids_fail_validation() {
        let result = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "codex"
                    display_name = "Codex"
                    auth = "codex-cache"
                    transport = "codex-responses"

                    [[connections]]
                    id = "codex"
                    display_name = "Other Codex"
                    auth = "codex-cache"
                    transport = "codex-responses"
                "#,
            )],
            &[],
        );

        assert!(
            matches!(result, Err(CatalogError::DuplicateConnection { id }) if id.as_str() == "codex")
        );
    }

    #[test]
    fn duplicate_target_keys_fail_validation() {
        let result = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "codex"
                    display_name = "Codex"
                    auth = "codex-cache"
                    transport = "codex-responses"
                "#,
            )],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "codex"
                    model = "openai/gpt-5.5"

                    [[targets]]
                    connection = "codex"
                    model = "openai/gpt-5.5"
                "#,
            )],
        );

        assert!(matches!(
            result,
            Err(CatalogError::DuplicateTarget {
                connection_id,
                model_id,
            }) if connection_id.as_str() == "codex" && model_id.as_str() == "openai/gpt-5.5"
        ));
    }

    #[test]
    fn multiple_default_targets_for_same_connection_fail_validation() {
        let result = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "codex"
                    display_name = "Codex"
                    auth = "codex-cache"
                    transport = "codex-responses"
                "#,
            )],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "codex"
                    model = "openai/gpt-5.5"
                    default = true

                    [[targets]]
                    connection = "codex"
                    model = "openai/gpt-5.4"
                    default = true
                "#,
            )],
        );

        assert!(matches!(
            result,
            Err(CatalogError::DuplicateDefaultTarget { connection_id, .. })
                if connection_id.as_str() == "codex"
        ));
    }

    #[test]
    fn target_referencing_unknown_connection_fails_validation() {
        let result = load_catalog_sources(
            &[],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "codex"
                    model = "openai/gpt-5.5"
                "#,
            )],
        );

        assert!(matches!(
            result,
            Err(CatalogError::UnknownTargetConnection { connection_id, .. })
                if connection_id.as_str() == "codex"
        ));
    }

    #[test]
    fn unknown_reasoning_are_rejected_instead_of_normalized() {
        let result = load_catalog_sources(
            &[source(
                "connections.toml",
                r#"
                    [[connections]]
                    id = "codex"
                    display_name = "Codex"
                    auth = "codex-cache"
                    transport = "codex-responses"
                "#,
            )],
            &[source(
                "targets.toml",
                r#"
                    [[targets]]
                    connection = "codex"
                    model = "openai/gpt-5.5"

                    [[targets.reasoning_options]]
                    type = "effort"
                    values = ["surprise"]
                "#,
            )],
        );

        assert!(matches!(result, Err(CatalogError::Toml { .. })));
    }
}
