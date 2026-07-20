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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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

use client::{LspClient, LspServerError};
use protocol::{path_to_file_uri, uri_to_path};

const TOOL_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const DIAGNOSTIC_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const PUSH_DIAGNOSTIC_WAIT: Duration = Duration::from_millis(700);

/// Shared handle used by LSP-backed tools and the agent diagnostics hook.
#[derive(Debug)]
pub(crate) struct LspHub {
    project_root: PathBuf,
    registry: LanguageServerRegistry,
    clients: Mutex<HashMap<ClientKey, Arc<LspClient>>>,
}

impl LspHub {
    pub(crate) fn new(project_root: PathBuf) -> Self {
        Self::with_registry(project_root, LanguageServerRegistry::builtin())
    }

    pub(crate) fn with_registry(project_root: PathBuf, registry: LanguageServerRegistry) -> Self {
        Self {
            project_root,
            registry,
            clients: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub(crate) async fn definition(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<LspLocation>, LspError> {
        let file = self.resolve_existing_file(path, "look up definition")?;
        let client = self.client_for_file(&file).await?;
        client.sync_document(&file).await?;
        let result = client
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": path_to_file_uri(&file)? },
                    "position": tool_position(line, character)?,
                }),
                TOOL_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(protocol::parse_locations(result)?)
    }

    pub(crate) async fn references(
        &self,
        path: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Vec<LspLocation>, LspError> {
        let file = self.resolve_existing_file(path, "look up references")?;
        let client = self.client_for_file(&file).await?;
        client.sync_document(&file).await?;
        let result = client
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": path_to_file_uri(&file)? },
                    "position": tool_position(line, character)?,
                    "context": { "includeDeclaration": include_declaration },
                }),
                TOOL_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(protocol::parse_locations(result)?)
    }

    pub(crate) async fn hover(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<Option<LspHover>, LspError> {
        let file = self.resolve_existing_file(path, "look up hover")?;
        let client = self.client_for_file(&file).await?;
        client.sync_document(&file).await?;
        let result = client
            .request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": path_to_file_uri(&file)? },
                    "position": tool_position(line, character)?,
                }),
                TOOL_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(protocol::parse_hover(result)?)
    }

    pub(crate) async fn workspace_symbol(
        &self,
        path: &str,
        query: &str,
    ) -> Result<Vec<LspSymbol>, LspError> {
        let anchor = self.resolve_existing_path(path, "search workspace symbols")?;
        let file = self.file_for_spec_resolution(&anchor)?;
        let client = self.client_for_file(&file).await?;
        let result = client
            .request(
                "workspace/symbol",
                json!({ "query": query }),
                TOOL_REQUEST_TIMEOUT,
            )
            .await?;
        Ok(protocol::parse_workspace_symbols(result)?)
    }

    pub(crate) async fn prepare_rename(
        &self,
        path: &str,
        line: u32,
        character: u32,
    ) -> Result<(), LspError> {
        let file = self.resolve_existing_file(path, "prepare rename")?;
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
        let file = self.resolve_existing_file(path, "rename symbol")?;
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
        Ok(protocol::parse_workspace_edit(result)?)
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
    ) -> Result<Vec<LspDiagnostic>, LspError> {
        let file = self.resolve_existing_file(path, "diagnose")?;
        self.refresh_diagnostics_for_file(&file, include_warnings)
            .await
    }

    pub(crate) async fn error_snapshot_for_files(
        &self,
        paths: &[PathBuf],
    ) -> Result<DiagnosticSnapshot, LspError> {
        let mut diagnostics = Vec::new();
        for path in paths {
            diagnostics.extend(self.refresh_diagnostics_for_file(path, false).await?);
        }
        Ok(DiagnosticSnapshot::from_diagnostics(diagnostics))
    }

    pub(crate) async fn refresh_error_snapshot_for_files(
        &self,
        paths: &[PathBuf],
    ) -> Result<DiagnosticSnapshot, LspError> {
        self.error_snapshot_for_files(paths).await
    }

    pub(crate) fn resolve_existing_project_file(
        &self,
        raw_path: &str,
        action: &str,
    ) -> Result<PathBuf, LspError> {
        self.resolve_existing_file(raw_path, action)
    }

    fn resolve_existing_file(&self, raw_path: &str, action: &str) -> Result<PathBuf, LspError> {
        let path = self.resolve_existing_path(raw_path, action)?;
        if path.is_dir() {
            return Err(LspError::Path(format!(
                "Cannot {action}: path is a directory, not a file: {raw_path}"
            )));
        }
        Ok(path)
    }

    fn resolve_existing_path(&self, raw_path: &str, action: &str) -> Result<PathBuf, LspError> {
        if raw_path.trim().is_empty() {
            return Err(LspError::Path("path is required".to_string()));
        }
        let input = Path::new(raw_path);
        let candidate = if input.is_absolute() {
            input.to_path_buf()
        } else {
            self.project_root.join(input)
        };
        let canonical_root = self
            .project_root
            .canonicalize()
            .map_err(|source| LspError::Io {
                action: "canonicalize project root".to_string(),
                source,
            })?;
        let canonical = candidate.canonicalize().map_err(|source| LspError::Io {
            action: format!("{action}: {}", candidate.display()),
            source,
        })?;
        if !canonical.starts_with(&canonical_root) {
            return Err(LspError::Path(format!(
                "Cannot {action} files outside project root"
            )));
        }
        Ok(canonical)
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

    async fn client_for_file(&self, file: &Path) -> Result<Arc<LspClient>, LspError> {
        let spec = self
            .registry
            .spec_for_path(file)
            .ok_or_else(|| LspError::NoServerForPath {
                path: file.to_path_buf(),
            })?;
        let root = spec.workspace_root(file, &self.project_root)?;
        let key = ClientKey {
            spec_id: spec.id.clone(),
            root: root.clone(),
        };
        if let Some(client) = self.clients.lock().await.get(&key).cloned() {
            return Ok(client);
        }

        let client = Arc::new(LspClient::spawn(spec.clone(), root).await?);
        self.clients.lock().await.insert(key, client.clone());
        Ok(client)
    }

    #[cfg(test)]
    pub(crate) async fn insert_client_for_test(
        &self,
        spec_id: String,
        root: PathBuf,
        client: Arc<LspClient>,
    ) {
        self.clients
            .lock()
            .await
            .insert(ClientKey { spec_id, root }, client);
    }

    async fn refresh_diagnostics_for_file(
        &self,
        file: &Path,
        include_warnings: bool,
    ) -> Result<Vec<LspDiagnostic>, LspError> {
        let client = self.client_for_file(file).await?;
        client.sync_document(file).await?;
        let result = client
            .request(
                "textDocument/diagnostic",
                json!({
                    "textDocument": { "uri": path_to_file_uri(file)? },
                }),
                DIAGNOSTIC_REQUEST_TIMEOUT,
            )
            .await;
        match result {
            Ok(value) => {
                let diagnostics = protocol::parse_document_diagnostic_report(file, value)?;
                client.set_diagnostics(file, diagnostics.clone()).await;
                Ok(filter_diagnostics(diagnostics, include_warnings))
            }
            Err(LspError::Server(error)) if error.code == -32601 => {
                tokio::time::sleep(PUSH_DIAGNOSTIC_WAIT).await;
                Ok(filter_diagnostics(
                    client.diagnostics_for_file(file).await,
                    include_warnings,
                ))
            }
            Err(err) => Err(err),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClientKey {
    spec_id: String,
    root: PathBuf,
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
    #[error("LSP protocol error: {0}")]
    Protocol(String),
    #[error("LSP request failed: {0}")]
    Server(#[from] LspServerError),
    #[error("failed to {action}: {source}")]
    Io {
        action: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<protocol::ProtocolError> for LspError {
    fn from(value: protocol::ProtocolError) -> Self {
        Self::Protocol(value.to_string())
    }
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
}

impl DiagnosticSnapshot {
    pub(crate) fn from_diagnostics(diagnostics: Vec<LspDiagnostic>) -> Self {
        let diagnostics = diagnostics
            .into_iter()
            .filter(|diagnostic| diagnostic.severity == LspDiagnosticSeverity::Error)
            .collect::<Vec<_>>();
        let keys = diagnostics.iter().map(DiagnosticKey::from).collect();
        Self { diagnostics, keys }
    }

    pub(crate) fn new_errors_since(&self, previous: &Self) -> Vec<LspDiagnostic> {
        self.diagnostics
            .iter()
            .filter(|diagnostic| !previous.keys.contains(&DiagnosticKey::from(*diagnostic)))
            .cloned()
            .collect()
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
