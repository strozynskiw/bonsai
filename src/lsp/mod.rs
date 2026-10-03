//! Managed language-server support for code intelligence tools.
//!
//! Extension contract:
//! - Add a [`LanguageServerSpec`] to [`LanguageServerRegistry::builtin`] with
//!   file extensions, root markers, command, environment, and initialization
//!   options.
//! - Add fake-server tests for descriptor matching, initialization, document
//!   sync, diagnostics, navigation, symbols, and rename edits.
//! - Keep model-facing tools generic. Normal definition/references/hover/
//!   workspace-symbol/rename support should not require a new tool name.
//! - Add an adapter only when a server has a protocol quirk that cannot be
//!   represented in the descriptor or normalized DTO parsing.

pub(crate) mod client;
mod edit;
mod protocol;
mod spec;
#[cfg(test)]
pub(crate) mod test_utils;

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use thiserror::Error;
use tokio::sync::Mutex;

pub(crate) use edit::EditedFile;
pub(crate) use edit::plan_workspace_edit;
pub(crate) use protocol::{
    LspDiagnostic, LspDiagnosticSeverity, LspHover, LspLocation, LspPosition, LspSymbol,
    LspWorkspaceEdit,
};
pub(crate) use spec::{LanguageServerRegistry, LanguageServerSpec};

use crate::tool::{PathEvidence, ProjectPathResolver, ToolPathError};
use client::{LspClient, LspLifecycleFailure, LspServerError};
use protocol::{path_to_file_uri, uri_to_path};

const TOOL_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const DIAGNOSTIC_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const PUSH_DIAGNOSTIC_WAIT: Duration = Duration::from_millis(700);
const LSP_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const LSP_RESTART_BACKOFF: Duration = Duration::from_millis(250);
const LSP_RECOVERY_COOLDOWN: Duration = Duration::from_secs(30);

#[async_trait::async_trait]
trait LspSpawner: Debug + Send + Sync {
    async fn spawn(
        &self,
        spec: LanguageServerSpec,
        root: PathBuf,
        generation: u64,
    ) -> Result<LspClient, LspError>;
}

#[derive(Debug)]
struct ProcessLspSpawner;

#[async_trait::async_trait]
impl LspSpawner for ProcessLspSpawner {
    async fn spawn(
        &self,
        spec: LanguageServerSpec,
        root: PathBuf,
        generation: u64,
    ) -> Result<LspClient, LspError> {
        LspClient::spawn(spec, root, generation).await
    }
}

/// Shared handle used by LSP-backed tools and the agent diagnostics hook.
#[derive(Debug)]
pub(crate) struct LspHub {
    project_root: PathBuf,
    registry: LanguageServerRegistry,
    spawner: Arc<dyn LspSpawner>,
    path_evidence: Option<PathEvidence>,
    clients: Mutex<HashMap<ClientKey, Arc<ClientSlot>>>,
}

impl LspHub {
    pub(crate) fn new(project_root: PathBuf) -> Self {
        Self::with_registry(project_root, LanguageServerRegistry::builtin())
    }

    pub(crate) fn with_registry(project_root: PathBuf, registry: LanguageServerRegistry) -> Self {
        Self::with_spawner(project_root, registry, Arc::new(ProcessLspSpawner))
    }

    fn with_spawner(
        project_root: PathBuf,
        registry: LanguageServerRegistry,
        spawner: Arc<dyn LspSpawner>,
    ) -> Self {
        Self {
            project_root,
            registry,
            spawner,
            path_evidence: None,
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn with_path_evidence(mut self, path_evidence: PathEvidence) -> Self {
        if path_evidence.is_for_root(&self.project_root) {
            self.path_evidence = Some(path_evidence);
        }
        self
    }

    pub(crate) fn project_root(&self) -> &Path {
        &self.project_root
    }

    #[cfg(test)]
    pub(crate) async fn definition(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<LspReadResult<Vec<LspLocation>>, LspError> {
        self.definition_with_recheck(path, line, character, false)
            .await
    }

    pub(crate) async fn definition_with_recheck(
        &self,
        path: &str,
        line: u32,
        character: u32,
        recheck: bool,
    ) -> Result<LspReadResult<Vec<LspLocation>>, LspError> {
        let file = self.resolve_existing_file(path, "look up definition", recheck)?;
        let client = self.client_for_file(&file).await?;
        let outcome = self
            .safe_request(
                Some(&file),
                &client,
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": path_to_file_uri(&file)? },
                    "position": tool_position(line, character)?,
                }),
                TOOL_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(LspReadResult {
            value: protocol::parse_locations(outcome.response?)?,
            recovery_notice: outcome.recovery_notice,
        })
    }

    pub(crate) async fn references_with_recheck(
        &self,
        path: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
        recheck: bool,
    ) -> Result<LspReadResult<Vec<LspLocation>>, LspError> {
        let file = self.resolve_existing_file(path, "look up references", recheck)?;
        let client = self.client_for_file(&file).await?;
        let outcome = self
            .safe_request(
                Some(&file),
                &client,
                "textDocument/references",
                json!({
                    "textDocument": { "uri": path_to_file_uri(&file)? },
                    "position": tool_position(line, character)?,
                    "context": { "includeDeclaration": include_declaration },
                }),
                TOOL_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(LspReadResult {
            value: protocol::parse_locations(outcome.response?)?,
            recovery_notice: outcome.recovery_notice,
        })
    }

    pub(crate) async fn hover_with_recheck(
        &self,
        path: &str,
        line: u32,
        character: u32,
        recheck: bool,
    ) -> Result<LspReadResult<Option<LspHover>>, LspError> {
        let file = self.resolve_existing_file(path, "look up hover", recheck)?;
        let client = self.client_for_file(&file).await?;
        let outcome = self
            .safe_request(
                Some(&file),
                &client,
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": path_to_file_uri(&file)? },
                    "position": tool_position(line, character)?,
                }),
                TOOL_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(LspReadResult {
            value: protocol::parse_hover(outcome.response?)?,
            recovery_notice: outcome.recovery_notice,
        })
    }

    pub(crate) async fn workspace_symbol_with_recheck(
        &self,
        path: &str,
        query: &str,
        recheck: bool,
    ) -> Result<LspReadResult<Vec<LspSymbol>>, LspError> {
        let anchor = self.resolve_existing_path(path, "search workspace symbols", recheck)?;
        let file = self.file_for_spec_resolution(&anchor)?;
        let client = self.client_for_file(&file).await?;
        let outcome = self
            .safe_request(
                None,
                &client,
                "workspace/symbol",
                json!({ "query": query }),
                TOOL_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(LspReadResult {
            value: protocol::parse_workspace_symbols(outcome.response?)?,
            recovery_notice: outcome.recovery_notice,
        })
    }

    pub(crate) async fn prepare_rename(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<(), LspError> {
        let file = self.resolve_existing_file(path, "prepare rename", true)?;
        let client = self.client_for_file(&file).await?;
        client.sync_document(&file).await?;
        let result = client
            .request(
                "textDocument/prepareRename",
                json!({
                    "textDocument": { "uri": path_to_file_uri(&file)? },
                    "position": tool_position(line, character)?,
                }),
                TOOL_REQUEST_TIMEOUT,
            )
            .await;
        match result {
            Ok(value) if value.is_null() => Err(LspError::Protocol(
                "language server rejected rename at this position".to_string(),
            )),
            Ok(_) => Ok(()),
            Err(LspError::Server(error)) if error.code == -32601 => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub(crate) async fn rename(
        &self,
        path: &str,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<LspWorkspaceEdit, LspError> {
        let file = self.resolve_existing_file(path, "rename symbol", true)?;
        let client = self.client_for_file(&file).await?;
        client.sync_document(&file).await?;
        let result = client
            .request(
                "textDocument/rename",
                json!({
                    "textDocument": { "uri": path_to_file_uri(&file)? },
                    "position": tool_position(line, character)?,
                    "newName": new_name,
                }),
                TOOL_REQUEST_TIMEOUT,
            )
            .await?;
        protocol::parse_workspace_edit(result)
    }

    pub(crate) async fn sync_after_files_changed(&self, files: &[PathBuf]) -> Result<(), LspError> {
        for file in files {
            if let Ok(client) = self.client_for_file(file).await {
                client.sync_document(file).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn diagnostics_for_path(
        &self,
        path: &str,
        include_warnings: bool,
    ) -> Result<LspReadResult<Vec<LspDiagnostic>>, LspError> {
        let file = self.resolve_existing_file(path, "diagnose", false)?;
        self.refresh_diagnostics_for_file(&file, include_warnings)
            .await
            .map(|(diagnostics, _, recovery_notice)| LspReadResult {
                value: diagnostics,
                recovery_notice,
            })
    }

    pub(crate) async fn error_snapshot_for_files(
        &self,
        paths: &[PathBuf],
    ) -> Result<DiagnosticSnapshot, LspError> {
        let mut diagnostics = Vec::new();
        let mut generations = HashSet::new();
        for path in paths {
            let (items, generation, _) = self.refresh_diagnostics_for_file(path, false).await?;
            diagnostics.extend(items);
            generations.insert(generation);
        }
        Ok(DiagnosticSnapshot::from_diagnostics(
            diagnostics,
            generations,
        ))
    }

    pub(crate) async fn refresh_error_snapshot_for_files(
        &self,
        paths: &[PathBuf],
    ) -> Result<(DiagnosticSnapshot, Option<String>), LspError> {
        let mut diagnostics = Vec::new();
        let mut generations = HashSet::new();
        let mut recovery_notice = None;
        for path in paths {
            let (items, generation, notice) =
                self.refresh_diagnostics_for_file(path, false).await?;
            diagnostics.extend(items);
            generations.insert(generation);
            recovery_notice = recovery_notice.or(notice);
        }
        Ok((
            DiagnosticSnapshot::from_diagnostics(diagnostics, generations),
            recovery_notice,
        ))
    }

    pub(crate) fn resolve_existing_project_file(
        &self,
        raw_path: &str,
        action: &str,
    ) -> Result<PathBuf, LspError> {
        self.resolve_existing_file(raw_path, action, false)
    }

    fn resolve_existing_file(
        &self,
        raw_path: &str,
        action: &str,
        recheck: bool,
    ) -> Result<PathBuf, LspError> {
        let path = self.resolve_existing_path(raw_path, action, recheck)?;
        if path.is_dir() {
            return Err(LspError::Path(format!(
                "Cannot {action}: path is a directory, not a file: {raw_path}"
            )));
        }
        Ok(path)
    }

    fn resolve_existing_path(
        &self,
        raw_path: &str,
        action: &str,
        recheck: bool,
    ) -> Result<PathBuf, LspError> {
        if raw_path.trim().is_empty() {
            return Err(LspError::Path("path is required".to_string()));
        }
        let mut resolver = ProjectPathResolver::new(&self.project_root)
            .action(action)
            .recheck(recheck);
        if let Some(evidence) = self.path_evidence.as_ref() {
            resolver = resolver.path_evidence(evidence);
        }
        resolver
            .resolve_existing(raw_path)
            .map(|path| path.canonical_path().to_path_buf())
            .map_err(|error| match error {
                ToolPathError::ReusedMissingPath { evidence } => {
                    LspError::ReusedMissingPath { evidence }
                }
                other => LspError::Path(other.to_string()),
            })
    }

    fn file_for_spec_resolution(&self, path: &Path) -> Result<PathBuf, LspError> {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        self.registry
            .first_supported_file(path)
            .ok_or_else(|| LspError::NoServerForPath {
                path: path.to_path_buf(),
            })
    }

    async fn slot_for_file(
        &self,
        file: &Path,
    ) -> Result<(LanguageServerSpec, PathBuf, Arc<ClientSlot>), LspError> {
        let spec = self
            .registry
            .spec_for_path(file)
            .ok_or_else(|| LspError::NoServerForPath {
                path: file.to_path_buf(),
            })?
            .clone();
        let root = spec.workspace_root(file, &self.project_root)?;
        let key = ClientKey {
            spec_id: spec.id.clone(),
            root: root.clone(),
        };
        let effective_command = spec.command();
        let slot = {
            let mut clients = self.clients.lock().await;
            clients
                .entry(key)
                .or_insert_with(|| {
                    Arc::new(ClientSlot {
                        state: Mutex::new(ClientSlotState {
                            spec: spec.clone(),
                            effective_command: effective_command.clone(),
                            epoch: 0,
                            generation: 0,
                            client: None,
                            restart_used: false,
                            cooldown_until: None,
                        }),
                        recovery: Mutex::new(()),
                    })
                })
                .clone()
        };

        let mut state = slot.state.lock().await;
        if state.spec != spec || state.effective_command != effective_command {
            let old_client = state.client.take();
            state.spec = spec.clone();
            state.effective_command = effective_command;
            state.epoch = state.epoch.saturating_add(1);
            state.generation = 0;
            state.restart_used = false;
            state.cooldown_until = None;
            drop(state);
            if let Some(client) = old_client {
                client.retire().await;
            }
        } else {
            drop(state);
        }
        Ok((spec, root, slot))
    }

    async fn client_for_file(&self, file: &Path) -> Result<ManagedClient, LspError> {
        let (spec, root, slot) = self.slot_for_file(file).await?;
        let mut state = slot.state.lock().await;
        if let Some(client) = state.client.clone() {
            let epoch = state.epoch;
            let generation = state.generation;
            drop(state);
            return Ok(ManagedClient {
                slot,
                root,
                client,
                epoch,
                generation,
            });
        }
        if state
            .cooldown_until
            .is_some_and(|until| until > Instant::now())
        {
            return Err(LspError::RecoveryUnavailable {
                reason: "language server recovery is cooling down".to_string(),
            });
        }
        state.cooldown_until = None;
        state.generation = state.generation.saturating_add(1);
        let epoch = state.epoch;
        let generation = state.generation;
        let spawned = tokio::time::timeout(
            LSP_STARTUP_TIMEOUT,
            self.spawner.spawn(spec, root.clone(), generation),
        )
        .await;
        let client = match spawned {
            Ok(Ok(client)) => Arc::new(client),
            Ok(Err(error)) => {
                state.cooldown_until = Some(Instant::now() + LSP_RECOVERY_COOLDOWN);
                return Err(LspError::RecoveryUnavailable {
                    reason: format!(
                        "language server startup failed: {error}; use symbol_search/grep meanwhile"
                    ),
                });
            }
            Err(_) => {
                state.cooldown_until = Some(Instant::now() + LSP_RECOVERY_COOLDOWN);
                return Err(LspError::RecoveryUnavailable {
                    reason: "language server startup timed out; use symbol_search/grep meanwhile"
                        .to_string(),
                });
            }
        };
        state.client = Some(client.clone());
        drop(state);
        Ok(ManagedClient {
            slot,
            root,
            client,
            epoch,
            generation,
        })
    }

    async fn recover_client(
        &self,
        managed: &ManagedClient,
        failure: &LspLifecycleFailure,
    ) -> Result<(Arc<LspClient>, Option<String>), LspError> {
        let _recovery = managed.slot.recovery.lock().await;
        let mut state = managed.slot.state.lock().await;
        if state.epoch != managed.epoch || state.generation != failure.generation {
            if let Some(client) = state.client.clone() {
                return Ok((client, None));
            }
            return Err(LspError::RecoveryUnavailable {
                reason: "language server generation changed during recovery".to_string(),
            });
        }
        if state.restart_used {
            state.client = None;
            state.cooldown_until = Some(Instant::now() + LSP_RECOVERY_COOLDOWN);
            return Err(LspError::RecoveryUnavailable {
                reason: "replacement language server transport also closed; using symbol_search/grep while LSP recovery cools down".to_string(),
            });
        }
        state.restart_used = true;
        let old_client = state.client.take();
        let spec = state.spec.clone();
        let root = managed.root.clone();
        let epoch = state.epoch;
        state.generation = state.generation.saturating_add(1);
        let generation = state.generation;
        drop(state);
        if let Some(client) = old_client {
            client.retire().await;
        }
        tokio::time::sleep(LSP_RESTART_BACKOFF).await;
        let spawned = tokio::time::timeout(
            LSP_STARTUP_TIMEOUT,
            self.spawner.spawn(spec, root, generation),
        )
        .await;
        let mut state = managed.slot.state.lock().await;
        if state.epoch != epoch || state.generation != generation {
            drop(state);
            if let Ok(Ok(client)) = spawned {
                client.retire().await;
            }
            let state = managed.slot.state.lock().await;
            return state
                .client
                .clone()
                .map(|client| (client, None))
                .ok_or_else(|| LspError::RecoveryUnavailable {
                    reason: "language server generation changed during recovery".to_string(),
                });
        }
        match spawned {
            Ok(Ok(client)) => {
                let client = Arc::new(client);
                state.client = Some(client.clone());
                Ok((
                    client,
                    Some("Language server transport closed; restarted it once and retried the read-only request.".to_string()),
                ))
            }
            Ok(Err(error)) => {
                state.cooldown_until = Some(Instant::now() + LSP_RECOVERY_COOLDOWN);
                Err(LspError::RecoveryUnavailable {
                    reason: format!(
                        "language server restart failed: {error}; use symbol_search/grep meanwhile"
                    ),
                })
            }
            Err(_) => {
                state.cooldown_until = Some(Instant::now() + LSP_RECOVERY_COOLDOWN);
                Err(LspError::RecoveryUnavailable {
                    reason: "language server restart timed out; use symbol_search/grep meanwhile"
                        .to_string(),
                })
            }
        }
    }

    pub(crate) async fn restart_for_path(&self, raw_path: &str) -> Result<String, LspError> {
        let path = self.resolve_existing_path(raw_path, "restart language server", true)?;
        let file = self.file_for_spec_resolution(&path)?;
        let (spec, root, slot) = self.slot_for_file(&file).await?;
        let _recovery = slot.recovery.lock().await;
        let mut state = slot.state.lock().await;
        let old_client = state.client.take();
        state.epoch = state.epoch.saturating_add(1);
        state.generation = state.generation.saturating_add(1);
        state.restart_used = false;
        state.cooldown_until = None;
        let epoch = state.epoch;
        let generation = state.generation;
        drop(state);
        if let Some(old_client) = old_client {
            old_client.retire().await;
        }
        let spawned = tokio::time::timeout(
            LSP_STARTUP_TIMEOUT,
            self.spawner.spawn(spec.clone(), root, generation),
        )
        .await;
        let client = match spawned {
            Ok(Ok(client)) => Arc::new(client),
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(LspError::RecoveryUnavailable {
                    reason: "explicit language server restart timed out".to_string(),
                });
            }
        };
        let mut state = slot.state.lock().await;
        if state.epoch != epoch || state.generation != generation {
            drop(state);
            client.retire().await;
            return Err(LspError::RecoveryUnavailable {
                reason: "language server configuration changed during explicit restart".to_string(),
            });
        }
        state.client = Some(client);
        Ok(format!("Restarted {} language server.", spec.display_name))
    }

    #[cfg(test)]
    pub(crate) async fn insert_client_for_test(
        &self,
        spec_id: String,
        root: PathBuf,
        client: Arc<LspClient>,
    ) {
        let spec = self
            .registry
            .specs()
            .iter()
            .find(|spec| spec.id == spec_id)
            .cloned()
            .unwrap_or_else(LanguageServerSpec::rust);
        let effective_command = spec.command();
        self.clients.lock().await.insert(
            ClientKey { spec_id, root },
            Arc::new(ClientSlot {
                state: Mutex::new(ClientSlotState {
                    spec,
                    effective_command,
                    epoch: 0,
                    generation: 0,
                    client: Some(client),
                    restart_used: false,
                    cooldown_until: None,
                }),
                recovery: Mutex::new(()),
            }),
        );
    }

    async fn safe_request(
        &self,
        file: Option<&Path>,
        managed: &ManagedClient,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<SafeRequestOutcome, LspError> {
        match execute_read_request(&managed.client, file, method, params.clone(), timeout).await {
            Ok(value) => Ok(SafeRequestOutcome {
                response: Ok(value),
                client: managed.client.clone(),
                generation: managed.generation,
                recovery_notice: None,
            }),
            Err(LspError::Lifecycle(failure)) => {
                let (replacement, notice) = self.recover_client(managed, &failure).await?;
                let response = match execute_read_request(
                    &replacement,
                    file,
                    method,
                    params,
                    timeout,
                )
                .await
                {
                    Ok(value) => Ok(value),
                    Err(LspError::Lifecycle(_)) => {
                        let mut state = managed.slot.state.lock().await;
                        state.client = None;
                        state.cooldown_until = Some(Instant::now() + LSP_RECOVERY_COOLDOWN);
                        return Err(LspError::RecoveryUnavailable {
                            reason: "replacement language server transport closed; use symbol_search/grep while LSP recovery cools down".to_string(),
                        });
                    }
                    Err(err) => Err(err),
                };
                let generation = managed.slot.state.lock().await.generation;
                Ok(SafeRequestOutcome {
                    response,
                    client: replacement,
                    generation,
                    recovery_notice: notice,
                })
            }
            Err(err) => Ok(SafeRequestOutcome {
                response: Err(err),
                client: managed.client.clone(),
                generation: managed.generation,
                recovery_notice: None,
            }),
        }
    }

    async fn refresh_diagnostics_for_file(
        &self,
        file: &Path,
        include_warnings: bool,
    ) -> Result<(Vec<LspDiagnostic>, u64, Option<String>), LspError> {
        let client = self.client_for_file(file).await?;
        let result = self
            .safe_request(
                Some(file),
                &client,
                "textDocument/diagnostic",
                json!({
                    "textDocument": { "uri": path_to_file_uri(file)? },
                }),
                DIAGNOSTIC_REQUEST_TIMEOUT,
            )
            .await;
        let outcome = result?;
        match outcome.response {
            Ok(value) => {
                let diagnostics = protocol::parse_document_diagnostic_report(file, value)?;
                outcome
                    .client
                    .set_diagnostics(file, diagnostics.clone())
                    .await;
                Ok((
                    filter_diagnostics(diagnostics, include_warnings),
                    outcome.generation,
                    outcome.recovery_notice,
                ))
            }
            Err(LspError::Server(error)) if error.code == -32601 => {
                tokio::time::sleep(PUSH_DIAGNOSTIC_WAIT).await;
                Ok((
                    filter_diagnostics(
                        outcome.client.diagnostics_for_file(file).await,
                        include_warnings,
                    ),
                    outcome.generation,
                    outcome.recovery_notice,
                ))
            }
            Err(err) => Err(err),
        }
    }
}

async fn execute_read_request(
    client: &LspClient,
    file: Option<&Path>,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<serde_json::Value, LspError> {
    if let Some(file) = file {
        client.sync_document(file).await?;
    }
    client.request(method, params, timeout).await
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    spec_id: String,
    root: PathBuf,
}

#[derive(Debug)]
struct ClientSlot {
    state: Mutex<ClientSlotState>,
    recovery: Mutex<()>,
}

#[derive(Debug)]
struct ClientSlotState {
    spec: LanguageServerSpec,
    effective_command: String,
    epoch: u64,
    generation: u64,
    client: Option<Arc<LspClient>>,
    restart_used: bool,
    cooldown_until: Option<Instant>,
}

#[derive(Debug, Clone)]
struct ManagedClient {
    slot: Arc<ClientSlot>,
    root: PathBuf,
    client: Arc<LspClient>,
    epoch: u64,
    generation: u64,
}

#[derive(Debug)]
struct SafeRequestOutcome {
    response: Result<serde_json::Value, LspError>,
    client: Arc<LspClient>,
    generation: u64,
    recovery_notice: Option<String>,
}

/// A read-only LSP result plus an optional one-time recovery notice.
#[derive(Debug)]
pub(crate) struct LspReadResult<T> {
    pub(crate) value: T,
    pub(crate) recovery_notice: Option<String>,
}

impl std::ops::Deref for ManagedClient {
    type Target = LspClient;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

#[derive(Debug, Error)]
pub(crate) enum LspError {
    #[error(
        "no LSP server available for {language}: command '{command}' could not be started: {source}. {install_hint}"
    )]
    ServerUnavailable {
        language: String,
        command: String,
        install_hint: String,
        #[source]
        source: std::io::Error,
    },
    #[error("no LSP server registered for path: {path}")]
    NoServerForPath { path: PathBuf },
    #[error("{0}")]
    Path(String),
    #[error("{evidence}")]
    ReusedMissingPath {
        evidence: crate::tool::MissingPathEvidence,
    },
    #[error("LSP protocol error: {0}")]
    Protocol(String),
    #[error("LSP unavailable: {reason}")]
    RecoveryUnavailable { reason: String },
    #[error(transparent)]
    Lifecycle(#[from] LspLifecycleFailure),
    #[error("LSP request failed: {0}")]
    Server(#[from] LspServerError),
    #[error("failed to {action}: {source}")]
    Io {
        action: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<std::io::Error> for LspError {
    fn from(source: std::io::Error) -> Self {
        Self::Io {
            action: "run LSP operation".to_string(),
            source,
        }
    }
}

fn tool_position(line: u32, character: u32) -> Result<LspPosition, LspError> {
    if line == 0 || character == 0 {
        return Err(LspError::Protocol(
            "line and character are 1-based; both must be greater than zero".to_string(),
        ));
    }
    Ok(LspPosition {
        line: line - 1,
        character: character - 1,
    })
}

fn filter_diagnostics(
    diagnostics: Vec<LspDiagnostic>,
    include_warnings: bool,
) -> Vec<LspDiagnostic> {
    diagnostics
        .into_iter()
        .filter(|diagnostic| {
            diagnostic.severity == LspDiagnosticSeverity::Error
                || (include_warnings && diagnostic.severity == LspDiagnosticSeverity::Warning)
        })
        .collect()
}

/// Stable comparable view of diagnostics used for post-edit diffing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DiagnosticSnapshot {
    diagnostics: Vec<LspDiagnostic>,
    keys: HashSet<DiagnosticKey>,
    generations: HashSet<u64>,
}

impl DiagnosticSnapshot {
    pub(crate) fn from_diagnostics(
        diagnostics: Vec<LspDiagnostic>,
        generations: HashSet<u64>,
    ) -> Self {
        let diagnostics = diagnostics
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == LspDiagnosticSeverity::Error)
            .collect::<Vec<_>>();
        let keys = diagnostics.iter().map(DiagnosticKey::from).collect();
        Self {
            diagnostics,
            keys,
            generations,
        }
    }

    pub(crate) fn new_errors_since(&self, previous: &Self) -> Vec<LspDiagnostic> {
        if self.generations != previous.generations {
            return Vec::new();
        }
        self.diagnostics
            .iter()
            .filter(|diagnostic| !previous.keys.contains(&DiagnosticKey::from(*diagnostic)))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod recovery_tests {
    use std::collections::VecDeque;

    use tokio::io::BufReader;

    use super::*;
    use crate::lsp::client::{BoxedReader, BoxedWriter, read_lsp_message, write_lsp_message};

    #[derive(Debug)]
    struct FakeSpawner {
        clients: Mutex<VecDeque<LspClient>>,
    }

    #[async_trait::async_trait]
    impl LspSpawner for FakeSpawner {
        async fn spawn(
            &self,
            _spec: LanguageServerSpec,
            _root: PathBuf,
            _generation: u64,
        ) -> Result<LspClient, LspError> {
            self.clients
                .lock()
                .await
                .pop_front()
                .ok_or_else(|| LspError::RecoveryUnavailable {
                    reason: "fake spawner exhausted".to_string(),
                })
        }
    }

    async fn fake_client(root: &Path, generation: u64) -> LspClient {
        let (client_to_server_client, client_to_server_server) = tokio::io::duplex(16 * 1024);
        let (server_to_client_server, server_to_client_client) = tokio::io::duplex(16 * 1024);
        tokio::spawn(async move {
            let mut reader = BufReader::new(client_to_server_server);
            let mut writer = server_to_client_server;
            while let Ok(Some(message)) = read_lsp_message(&mut reader).await {
                let Some(id) = message.get("id").and_then(serde_json::Value::as_u64) else {
                    continue;
                };
                let method = message
                    .get("method")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let result = if method == "initialize" {
                    json!({ "capabilities": {} })
                } else {
                    serde_json::Value::Null
                };
                let response = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                let _ = write_lsp_message(&mut writer, &response).await;
            }
        });
        let reader: BoxedReader = Box::new(BufReader::new(server_to_client_client));
        let writer: BoxedWriter = Box::new(client_to_server_client);
        LspClient::connect_for_test_generation(
            LanguageServerSpec::rust(),
            root.to_path_buf(),
            generation,
            reader,
            writer,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn initial_start_failure_enters_cooldown() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let spawner = Arc::new(FakeSpawner {
            clients: Mutex::new(VecDeque::new()),
        });
        let hub = LspHub::with_spawner(
            temp.path().to_path_buf(),
            LanguageServerRegistry::new(vec![LanguageServerSpec::rust()]),
            spawner,
        );

        let first = hub
            .definition(file.to_str().unwrap(), 1, 1)
            .await
            .unwrap_err();
        let second = hub
            .definition(file.to_str().unwrap(), 1, 1)
            .await
            .unwrap_err();

        assert!(first.to_string().contains("fake spawner exhausted"));
        assert!(second.to_string().contains("cooling down"));
    }

    #[tokio::test]
    async fn explicit_restart_uses_fresh_process_and_resets_budget() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let root = temp.path().canonicalize().unwrap();
        let replacement = fake_client(&root, 1).await;
        let spawner = Arc::new(FakeSpawner {
            clients: Mutex::new(VecDeque::from([replacement])),
        });
        let hub = LspHub::with_spawner(
            temp.path().to_path_buf(),
            LanguageServerRegistry::new(vec![LanguageServerSpec::rust()]),
            spawner,
        );

        let message = hub.restart_for_path(file.to_str().unwrap()).await.unwrap();
        let outcome = hub.definition(file.to_str().unwrap(), 1, 1).await.unwrap();

        assert!(message.contains("Restarted Rust"));
        assert!(outcome.value.is_empty());
    }

    #[test]
    fn lsp_resolution_reuses_missing_path_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let evidence = PathEvidence::new(temp.path()).unwrap();
        let hub = LspHub::new(temp.path().to_path_buf()).with_path_evidence(evidence);

        let first = hub.resolve_existing_project_file("missing.rs", "look up definition");
        assert!(first.is_err());
        let second = hub.resolve_existing_project_file("./missing.rs", "look up definition");
        assert!(matches!(second, Err(LspError::ReusedMissingPath { .. })));
    }

    #[test]
    fn diagnostic_generation_change_resets_baseline() {
        let previous = DiagnosticSnapshot::from_diagnostics(Vec::new(), HashSet::from([1]));
        let fresh = DiagnosticSnapshot::from_diagnostics(Vec::new(), HashSet::from([2]));

        assert!(fresh.new_errors_since(&previous).is_empty());
    }

    #[test]
    fn recovery_unavailable_is_actionable() {
        let error = LspError::RecoveryUnavailable {
            reason: "use symbol_search/grep while LSP recovery cools down".to_string(),
        };

        assert!(error.to_string().contains("symbol_search/grep"));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DiagnosticKey {
    path: PathBuf,
    line: u32,
    character: u32,
    message: String,
    code: Option<String>,
}

impl From<&LspDiagnostic> for DiagnosticKey {
    fn from(value: &LspDiagnostic) -> Self {
        Self {
            path: value.path.clone(),
            line: value.range.start.line,
            character: value.range.start.character,
            message: value.message.clone(),
            code: value.code.clone(),
        }
    }
}

pub(crate) fn format_diagnostics(
    project_root: &Path,
    diagnostics: &[LspDiagnostic],
    heading: &str,
) -> String {
    if diagnostics.is_empty() {
        return "No diagnostics found.".to_string();
    }

    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == LspDiagnosticSeverity::Error)
        .count();
    let warnings = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == LspDiagnosticSeverity::Warning)
        .count();
    let mut out = format!("{heading}\nerrors: {errors}  warnings: {warnings}\n");
    for diagnostic in diagnostics {
        let path = relative_path(project_root, &diagnostic.path);
        let line = diagnostic.range.start.line + 1;
        let character = diagnostic.range.start.character + 1;
        let code = diagnostic
            .code
            .as_ref()
            .map(|code| format!(" [{code}]"))
            .unwrap_or_default();
        out.push_str(&format!(
            "\n{}:{}:{}: {}{}: {}",
            path,
            line,
            character,
            diagnostic.severity.label(),
            code,
            diagnostic.message
        ));
        if let Some(source) = &diagnostic.source {
            out.push_str(&format!(" ({source})"));
        }
    }
    out
}

pub(crate) fn relative_path(project_root: &Path, path: &Path) -> String {
    path.strip_prefix(project_root)
        .unwrap_or(path)
        .display()
        .to_string()
}

pub(crate) fn workspace_edit_paths(edit: &LspWorkspaceEdit) -> Result<Vec<PathBuf>, LspError> {
    let mut paths = Vec::new();
    for uri in edit.changes.keys() {
        paths.push(uri_to_path(uri)?);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}
