use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Mutex, oneshot};

use super::protocol::{self, LspDiagnostic, path_to_file_uri};
use super::{LanguageServerSpec, LspError};

const MAX_LSP_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub(crate) type BoxedReader = Box<dyn AsyncBufRead + Send + Unpin>;
pub(crate) type BoxedWriter = Box<dyn AsyncWrite + Send + Unpin>;
type PendingResponse = oneshot::Sender<Result<Value, PendingResponseError>>;
type SharedConnection = Arc<Mutex<ConnectionRuntime>>;

#[derive(Debug)]
enum PendingResponseError {
    Server(LspServerError),
    Lifecycle(LspLifecycleFailure),
}

/// Terminal transport failure that can be recovered by replacing the process.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("LSP transport generation {generation} failed: {reason}")]
pub(crate) struct LspLifecycleFailure {
    pub(crate) generation: u64,
    pub(crate) kind: LspLifecycleKind,
    pub(crate) reason: String,
}

/// How an LSP transport became unusable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LspLifecycleKind {
    Closed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConnectionState {
    Running,
    Terminal(LspLifecycleFailure),
}

impl ConnectionState {
    fn terminal_failure(&self) -> Option<&LspLifecycleFailure> {
        match self {
            Self::Running => None,
            Self::Terminal(failure) => Some(failure),
        }
    }
}

#[derive(Debug)]
struct ConnectionRuntime {
    state: ConnectionState,
    pending: HashMap<u64, PendingResponse>,
}

#[derive(Debug)]
struct PendingRequestGuard {
    connection: SharedConnection,
    id: u64,
    armed: bool,
}

impl PendingRequestGuard {
    fn new(connection: SharedConnection, id: u64) -> Self {
        Self {
            connection,
            id,
            armed: true,
        }
    }

    async fn remove(&mut self) {
        if self.armed {
            self.connection.lock().await.pending.remove(&self.id);
            self.armed = false;
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let connection = self.connection.clone();
        let id = self.id;
        tokio::spawn(async move {
            connection.lock().await.pending.remove(&id);
        });
    }
}

#[derive(Debug, Error, Clone)]
#[error("{message}")]
pub(crate) struct LspServerError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

#[derive(Debug, Clone)]
struct SyncedDocument {
    version: i32,
    text: String,
}

type SharedDocumentState = Arc<Mutex<Option<SyncedDocument>>>;

/// One initialized stdio LSP client.
pub(crate) struct LspClient {
    spec: LanguageServerSpec,
    root: PathBuf,
    writer: Arc<Mutex<BoxedWriter>>,
    connection: SharedConnection,
    diagnostics: Arc<Mutex<HashMap<PathBuf, Vec<LspDiagnostic>>>>,
    documents: Mutex<HashMap<PathBuf, SharedDocumentState>>,
    next_id: AtomicU64,
    generation: u64,
    reader_task: tokio::task::JoinHandle<()>,
    child: Mutex<Option<tokio::process::Child>>,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClient")
            .field("spec", &self.spec.id)
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl LspClient {
    pub(crate) async fn spawn(
        spec: LanguageServerSpec,
        root: PathBuf,
        generation: u64,
    ) -> Result<Self, LspError> {
        let command = spec.command();
        let mut process = Command::new(&command);
        process.args(&spec.args).current_dir(&root);
        for (key, value) in &spec.env {
            process.env(key, value);
        }
        process
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = process
            .spawn()
            .map_err(|source| LspError::ServerUnavailable {
                language: spec.display_name.clone(),
                command,
                install_hint: spec.install_hint.clone(),
                source,
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            LspError::Protocol("language server stdin was not available".to_string())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            LspError::Protocol("language server stdout was not available".to_string())
        })?;
        let reader: BoxedReader = Box::new(tokio::io::BufReader::new(stdout));
        let writer: BoxedWriter = Box::new(stdin);
        let client = Self::new_with_io(spec, root, generation, reader, writer, Some(child)).await?;
        client.initialize().await?;
        Ok(client)
    }

    #[cfg(test)]
    pub(crate) async fn connect_for_test(
        spec: LanguageServerSpec,
        root: PathBuf,
        reader: BoxedReader,
        writer: BoxedWriter,
    ) -> Result<Self, LspError> {
        Self::connect_for_test_generation(spec, root, 0, reader, writer).await
    }

    #[cfg(test)]
    pub(crate) async fn connect_for_test_generation(
        spec: LanguageServerSpec,
        root: PathBuf,
        generation: u64,
        reader: BoxedReader,
        writer: BoxedWriter,
    ) -> Result<Self, LspError> {
        let client = Self::new_with_io(spec, root, generation, reader, writer, None).await?;
        client.initialize().await?;
        Ok(client)
    }

    async fn new_with_io(
        spec: LanguageServerSpec,
        root: PathBuf,
        generation: u64,
        reader: BoxedReader,
        writer: BoxedWriter,
        child: Option<tokio::process::Child>,
    ) -> Result<Self, LspError> {
        let writer = Arc::new(Mutex::new(writer));
        let connection = Arc::new(Mutex::new(ConnectionRuntime {
            state: ConnectionState::Running,
            pending: HashMap::new(),
        }));
        let diagnostics = Arc::new(Mutex::new(HashMap::new()));
        let reader_task = spawn_reader(
            reader,
            writer.clone(),
            connection.clone(),
            diagnostics.clone(),
            generation,
        );
        Ok(Self {
            spec,
            root,
            writer,
            connection,
            diagnostics,
            documents: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            generation,
            reader_task,
            child: Mutex::new(child),
        })
    }

    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            let mut connection = self.connection.lock().await;
            if let Some(failure) = connection.state.terminal_failure() {
                return Err(LspError::Lifecycle(failure.clone()));
            }
            connection.pending.insert(id, tx);
        }
        let mut pending_guard = PendingRequestGuard::new(self.connection.clone(), id);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(err) = self.send_message(&message).await {
            pending_guard.remove().await;
            return Err(err);
        }
        let response = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => {
                pending_guard.disarm();
                response
            }
            Ok(Err(_)) => {
                pending_guard.remove().await;
                return Err(LspError::Protocol(format!(
                    "LSP request '{method}' was cancelled"
                )));
            }
            Err(_) => {
                pending_guard.remove().await;
                return Err(LspError::Protocol(format!(
                    "LSP request '{method}' timed out"
                )));
            }
        };
        response.map_err(|error| match error {
            PendingResponseError::Server(error) => LspError::Server(error),
            PendingResponseError::Lifecycle(failure) => LspError::Lifecycle(failure),
        })
    }

    pub(crate) async fn notification(&self, method: &str, params: Value) -> Result<(), LspError> {
        self.send_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    pub(crate) async fn sync_document(&self, file: &Path) -> Result<(), LspError> {
        let language_id =
            self.spec
                .language_id_for_path(file)
                .ok_or_else(|| LspError::NoServerForPath {
                    path: file.to_path_buf(),
                })?;
        let state = {
            let mut documents = self.documents.lock().await;
            documents
                .entry(file.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };
        // Serialize one document without retaining the global document-map
        // lock across pipe I/O. The committed state changes only after the
        // notification succeeds, so cancellation or write failure is safely
        // retryable with the same coherent version.
        let mut committed = state.lock().await;
        let text = tokio::fs::read_to_string(file)
            .await
            .map_err(|source| LspError::Io {
                action: format!("read {}", file.display()),
                source,
            })?;
        let uri = path_to_file_uri(file)?;
        let next = match committed.as_ref() {
            Some(document) if document.text == text => return Ok(()),
            Some(document) => SyncedDocument {
                version: document.version.saturating_add(1),
                text,
            },
            None => SyncedDocument { version: 1, text },
        };
        if committed.is_some() {
            self.notification(
                "textDocument/didChange",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "version": next.version,
                    },
                    "contentChanges": [{ "text": next.text }],
                }),
            )
            .await?;
        } else {
            self.notification(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": next.version,
                        "text": next.text,
                    }
                }),
            )
            .await?;
        }
        *committed = Some(next);
        Ok(())
    }

    pub(crate) async fn diagnostics_for_file(&self, file: &Path) -> Vec<LspDiagnostic> {
        self.diagnostics
            .lock()
            .await
            .get(file)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) async fn set_diagnostics(&self, file: &Path, diagnostics: Vec<LspDiagnostic>) {
        self.diagnostics
            .lock()
            .await
            .insert(file.to_path_buf(), diagnostics);
    }

    async fn initialize(&self) -> Result<(), LspError> {
        let root_uri = path_to_file_uri(&self.root)?;
        self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{
                    "uri": root_uri,
                    "name": self.root.file_name().and_then(|name| name.to_str()).unwrap_or("workspace"),
                }],
                "capabilities": {
                    "workspace": {
                        "configuration": true,
                        "workspaceFolders": true,
                        "symbol": { "dynamicRegistration": false },
                    },
                    "textDocument": {
                        "definition": { "dynamicRegistration": false, "linkSupport": true },
                        "references": { "dynamicRegistration": false },
                        "hover": {
                            "dynamicRegistration": false,
                            "contentFormat": ["markdown", "plaintext"]
                        },
                        "rename": {
                            "dynamicRegistration": false,
                            "prepareSupport": true
                        },
                        "publishDiagnostics": {
                            "relatedInformation": true,
                            "versionSupport": true,
                            "codeDescriptionSupport": true,
                            "dataSupport": true
                        },
                        "diagnostic": { "dynamicRegistration": false },
                        "synchronization": {
                            "didSave": false,
                            "willSave": false,
                            "willSaveWaitUntil": false,
                            "dynamicRegistration": false
                        }
                    },
                    "window": { "workDoneProgress": true },
                },
                "initializationOptions": self.spec.initialization_options,
            }),
            Duration::from_millis(self.spec.request_timeout_ms),
        )
        .await?;
        self.notification("initialized", json!({})).await?;
        Ok(())
    }

    async fn send_message(&self, message: &Value) -> Result<(), LspError> {
        if let Some(failure) = self
            .connection
            .lock()
            .await
            .state
            .terminal_failure()
            .cloned()
        {
            return Err(LspError::Lifecycle(failure));
        }
        let result = {
            let mut writer = self.writer.lock().await;
            write_lsp_message(&mut **writer, message).await
        };
        if let Err(source) = result {
            let kind = source.kind();
            if matches!(
                kind,
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::UnexpectedEof
            ) {
                let failure = LspLifecycleFailure {
                    generation: self.generation,
                    kind: LspLifecycleKind::Failed,
                    reason: format!("language server transport write failed: {source}"),
                };
                transition_to_terminal(&self.connection, failure.clone()).await;
                return Err(LspError::Lifecycle(failure));
            }
            return Err(LspError::Io {
                action: "write LSP message".to_string(),
                source,
            });
        }
        Ok(())
    }

    pub(crate) async fn retire(&self) {
        transition_to_terminal(
            &self.connection,
            LspLifecycleFailure {
                generation: self.generation,
                kind: LspLifecycleKind::Closed,
                reason: "language server transport was retired".to_string(),
            },
        )
        .await;
        self.reader_task.abort();
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.reader_task.abort();
        if let Ok(mut child) = self.child.try_lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

fn spawn_reader(
    mut reader: BoxedReader,
    writer: Arc<Mutex<BoxedWriter>>,
    connection: SharedConnection,
    diagnostics: Arc<Mutex<HashMap<PathBuf, Vec<LspDiagnostic>>>>,
    generation: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let terminal_state = loop {
            let message = match read_lsp_message(&mut *reader).await {
                Ok(Some(message)) => message,
                Ok(None) => {
                    break LspLifecycleFailure {
                        generation,
                        kind: LspLifecycleKind::Closed,
                        reason: "language server transport closed its output".to_string(),
                    };
                }
                Err(err) => {
                    break LspLifecycleFailure {
                        generation,
                        kind: LspLifecycleKind::Failed,
                        reason: format!("language server transport failed: {err}"),
                    };
                }
            };
            if let Some(id) = message.get("id").and_then(Value::as_u64)
                && (message.get("result").is_some() || message.get("error").is_some())
            {
                let result = match message.get("error") {
                    Some(error) => Err(PendingResponseError::Server(parse_server_error(error))),
                    None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                };
                if let Some(tx) = connection.lock().await.pending.remove(&id)
                    && let Err(_err) = tx.send(result)
                {
                    tracing::debug!("LSP pending request waiter dropped");
                }
                continue;
            }

            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if method == "textDocument/publishDiagnostics" {
                    if let Some(params) = message.get("params")
                        && let Ok((path, items)) =
                            protocol::parse_publish_diagnostics(params.clone())
                    {
                        diagnostics.lock().await.insert(path, items);
                    }
                    continue;
                }
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let result = response_for_server_request(method);
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result,
                    });
                    let mut writer = writer.lock().await;
                    let _ = write_lsp_message(&mut **writer, &response).await;
                }
            }
        };
        transition_to_terminal(&connection, terminal_state).await;
    })
}

async fn transition_to_terminal(connection: &SharedConnection, failure: LspLifecycleFailure) {
    let pending = {
        let mut connection = connection.lock().await;
        if connection.state.terminal_failure().is_some() {
            return;
        }
        connection.state = ConnectionState::Terminal(failure.clone());
        std::mem::take(&mut connection.pending)
    };
    for (_, responder) in pending {
        if let Err(_err) = responder.send(Err(PendingResponseError::Lifecycle(failure.clone()))) {
            tracing::debug!("LSP drain responder dropped on transport death");
        }
    }
}

fn response_for_server_request(method: &str) -> Value {
    match method {
        "workspace/configuration" => Value::Array(Vec::new()),
        "workspace/workspaceFolders" => Value::Null,
        "window/workDoneProgress/create" | "client/registerCapability" => Value::Null,
        "window/showMessageRequest" => Value::Null,
        _ => Value::Null,
    }
}

fn parse_server_error(value: &Value) -> LspServerError {
    LspServerError {
        code: value.get("code").and_then(Value::as_i64).unwrap_or(-32000),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("language server request failed")
            .to_string(),
    }
}

pub(crate) async fn read_lsp_message(
    reader: &mut (dyn AsyncBufRead + Send + Unpin),
) -> std::io::Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let bytes = reader.read_line(&mut line).await?;
        if bytes == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.strip_prefix("Content-Length:") {
            let length = value.trim().parse::<usize>().map_err(|err| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid Content-Length: {err}"),
                )
            })?;
            content_length = Some(length);
        }
    }
    let length = content_length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    if length > MAX_LSP_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("LSP frame length {length} exceeds {MAX_LSP_FRAME_BYTES}-byte limit"),
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    let value = serde_json::from_slice(&body)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    Ok(Some(value))
}

pub(crate) async fn write_lsp_message(
    writer: &mut (dyn AsyncWrite + Send + Unpin),
    message: &Value,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(message)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::pin::Pin;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::AtomicBool;
    use std::task::{Context, Poll};
    use tokio::io::BufReader;

    #[derive(Clone, Default)]
    struct FailOnceWriter {
        bytes: Arc<StdMutex<Vec<u8>>>,
        fail_next: Arc<AtomicBool>,
    }

    impl FailOnceWriter {
        fn failing() -> Self {
            Self {
                fail_next: Arc::new(AtomicBool::new(true)),
                ..Self::default()
            }
        }

        fn arm_failure(&self) {
            self.fail_next.store(true, Ordering::SeqCst);
        }

        fn take_text(&self) -> String {
            let mut bytes = self.bytes.lock().unwrap();
            String::from_utf8(std::mem::take(&mut *bytes)).unwrap()
        }
    }

    impl AsyncWrite for FailOnceWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.fail_next.swap(false, Ordering::SeqCst) {
                return Poll::Ready(Err(std::io::Error::other("injected LSP writer failure")));
            }
            self.bytes.lock().unwrap().extend_from_slice(buffer);
            Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    async fn client_with_writer(root: &Path, writer: FailOnceWriter) -> LspClient {
        let (reader_peer, reader) = tokio::io::duplex(1024);
        tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(reader_peer);
        });
        LspClient::new_with_io(
            LanguageServerSpec::rust(),
            root.to_path_buf(),
            0,
            Box::new(BufReader::new(reader)),
            Box::new(writer),
            None,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn json_rpc_framing_round_trips() {
        let (client, server) = tokio::io::duplex(1024);
        let (_client_read, mut client_write) = tokio::io::split(client);
        let (server_read, _server_write) = tokio::io::split(server);
        let mut reader = BufReader::new(server_read);
        let write = tokio::spawn(async move {
            write_lsp_message(&mut client_write, &json!({"jsonrpc":"2.0","method":"ping"}))
                .await
                .unwrap();
        });
        let message = read_lsp_message(&mut reader).await.unwrap().unwrap();
        write.await.unwrap();
        assert_eq!(message["method"], "ping");
    }

    #[tokio::test]
    async fn client_initializes_syncs_and_requests_diagnostics() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("src/lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let file_uri = protocol::path_to_file_uri(&file).unwrap();
        let server = spawn_fake_server(file_uri.clone());
        let spec = LanguageServerSpec::rust();
        let client = LspClient::connect_for_test(
            spec,
            temp.path().to_path_buf(),
            server.client_reader,
            server.client_writer,
        )
        .await
        .unwrap();

        client.sync_document(&file).await.unwrap();
        let definition = client
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": file_uri },
                    "position": { "line": 0, "character": 3 },
                }),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        let locations = protocol::parse_locations(definition).unwrap();
        assert_eq!(locations.len(), 1);

        let diagnostics = client
            .request(
                "textDocument/diagnostic",
                json!({ "textDocument": { "uri": protocol::path_to_file_uri(&file).unwrap() } }),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        let parsed = protocol::parse_document_diagnostic_report(&file, diagnostics).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed[0].severity,
            super::super::LspDiagnosticSeverity::Error
        );
        assert!(parsed[0].message.contains("fake error"));

        server.task.abort();
    }

    #[tokio::test]
    async fn request_timeout_removes_pending_response() {
        let temp = tempfile::TempDir::new().unwrap();
        let (client_to_server_client, _client_to_server_server) = tokio::io::duplex(8192);
        let (_server_to_client_server, server_to_client_client) = tokio::io::duplex(8192);
        let client_reader: BoxedReader = Box::new(BufReader::new(server_to_client_client));
        let client_writer: BoxedWriter = Box::new(client_to_server_client);
        let client = LspClient::new_with_io(
            LanguageServerSpec::rust(),
            temp.path().to_path_buf(),
            0,
            client_reader,
            client_writer,
            None,
        )
        .await
        .unwrap();

        let err = client
            .request(
                "textDocument/definition",
                json!({ "textDocument": { "uri": "file:///never.rs" } }),
                Duration::from_millis(1),
            )
            .await
            .unwrap_err();

        assert!(
            err.to_string().contains("timed out"),
            "unexpected error: {err}"
        );
        assert!(
            client.connection.lock().await.pending.is_empty(),
            "timed-out request left a pending responder"
        );
    }

    #[tokio::test]
    async fn oversized_lsp_frame_is_rejected_before_reading_a_body() {
        let header = format!("Content-Length: {}\r\n\r\n", MAX_LSP_FRAME_BYTES + 1);
        let mut reader = BufReader::new(header.as_bytes());

        let error = read_lsp_message(&mut reader).await.unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("exceeds"), "{error}");
    }

    #[tokio::test]
    async fn malformed_response_fails_pending_and_future_requests_immediately() {
        let temp = tempfile::TempDir::new().unwrap();
        let (client_to_server_client, client_to_server_server) = tokio::io::duplex(8192);
        let (mut server_to_client_server, server_to_client_client) = tokio::io::duplex(8192);
        let client = LspClient::new_with_io(
            LanguageServerSpec::rust(),
            temp.path().to_path_buf(),
            0,
            Box::new(BufReader::new(server_to_client_client)),
            Box::new(client_to_server_client),
            None,
        )
        .await
        .unwrap();
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(client_to_server_server);
            read_lsp_message(&mut reader).await.unwrap().unwrap();
            server_to_client_server
                .write_all(b"Content-Length: 5\r\n\r\nnope!")
                .await
                .unwrap();
        });

        let first = tokio::time::timeout(
            Duration::from_millis(250),
            client.request(
                "textDocument/definition",
                json!({ "textDocument": { "uri": "file:///broken.rs" } }),
                Duration::from_secs(5),
            ),
        )
        .await
        .expect("reader failure should resolve the pending request")
        .unwrap_err();
        server.await.unwrap();
        let first_message = first.to_string();
        assert!(
            first_message.contains("transport failed"),
            "{first_message}"
        );
        assert!(first_message.contains("line 1 column"), "{first_message}");
        assert!(matches!(
            first,
            LspError::Lifecycle(LspLifecycleFailure {
                kind: LspLifecycleKind::Failed,
                ..
            })
        ));
        {
            let connection = client.connection.lock().await;
            assert!(connection.pending.is_empty());
            assert!(matches!(connection.state, ConnectionState::Terminal(_)));
        }

        let second = tokio::time::timeout(
            Duration::from_millis(100),
            client.request(
                "textDocument/definition",
                json!({ "textDocument": { "uri": "file:///broken.rs" } }),
                Duration::from_secs(5),
            ),
        )
        .await
        .expect("terminal reader state should reject a new request")
        .unwrap_err();
        assert_eq!(second.to_string(), first_message);
    }

    #[tokio::test]
    async fn reader_eof_fails_a_pending_request_immediately() {
        let temp = tempfile::TempDir::new().unwrap();
        let (client_to_server_client, client_to_server_server) = tokio::io::duplex(8192);
        let (server_to_client_server, server_to_client_client) = tokio::io::duplex(8192);
        let client = LspClient::new_with_io(
            LanguageServerSpec::rust(),
            temp.path().to_path_buf(),
            0,
            Box::new(BufReader::new(server_to_client_client)),
            Box::new(client_to_server_client),
            None,
        )
        .await
        .unwrap();
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(client_to_server_server);
            read_lsp_message(&mut reader).await.unwrap().unwrap();
            drop(server_to_client_server);
        });

        let error = tokio::time::timeout(
            Duration::from_millis(250),
            client.request(
                "textDocument/definition",
                json!({ "textDocument": { "uri": "file:///closed.rs" } }),
                Duration::from_secs(5),
            ),
        )
        .await
        .expect("reader EOF should resolve the pending request")
        .unwrap_err();
        server.await.unwrap();

        assert!(error.to_string().contains("closed its output"), "{error}");
        assert!(matches!(
            error,
            LspError::Lifecycle(LspLifecycleFailure {
                kind: LspLifecycleKind::Closed,
                ..
            })
        ));
        let connection = client.connection.lock().await;
        assert!(connection.pending.is_empty());
        assert!(matches!(connection.state, ConnectionState::Terminal(_)));
    }

    #[tokio::test]
    async fn failed_did_open_remains_uncommitted_and_retries() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let writer = FailOnceWriter::failing();
        let client = client_with_writer(temp.path(), writer.clone()).await;

        assert!(client.sync_document(&file).await.is_err());
        let state = client.documents.lock().await.get(&file).unwrap().clone();
        assert!(state.lock().await.is_none());

        client.sync_document(&file).await.unwrap();

        let committed = state.lock().await.clone().unwrap();
        assert_eq!(committed.version, 1);
        assert_eq!(committed.text, "fn main() {}\n");
        let messages = writer.take_text();
        assert_eq!(messages.matches("textDocument/didOpen").count(), 1);
    }

    #[tokio::test]
    async fn failed_did_change_retains_prior_version_and_retries_coherently() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("main.rs");
        std::fs::write(&file, "fn before() {}\n").unwrap();
        let writer = FailOnceWriter::default();
        let client = client_with_writer(temp.path(), writer.clone()).await;
        client.sync_document(&file).await.unwrap();
        let _ = writer.take_text();
        std::fs::write(&file, "fn after() {}\n").unwrap();
        writer.arm_failure();

        assert!(client.sync_document(&file).await.is_err());
        let state = client.documents.lock().await.get(&file).unwrap().clone();
        let committed = state.lock().await.clone().unwrap();
        assert_eq!(committed.version, 1);
        assert_eq!(committed.text, "fn before() {}\n");

        client.sync_document(&file).await.unwrap();

        let committed = state.lock().await.clone().unwrap();
        assert_eq!(committed.version, 2);
        assert_eq!(committed.text, "fn after() {}\n");
        let messages = writer.take_text();
        assert_eq!(messages.matches("textDocument/didChange").count(), 1);
        assert!(messages.contains(r#""version":2"#), "{messages}");
    }

    #[tokio::test]
    async fn concurrent_syncs_for_one_file_emit_one_coherent_change() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("main.rs");
        std::fs::write(&file, "fn before() {}\n").unwrap();
        let writer = FailOnceWriter::default();
        let client = Arc::new(client_with_writer(temp.path(), writer.clone()).await);
        client.sync_document(&file).await.unwrap();
        let _ = writer.take_text();
        std::fs::write(&file, "fn after() {}\n").unwrap();

        let mut syncs = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let client = client.clone();
            let file = file.clone();
            syncs.spawn(async move { client.sync_document(&file).await });
        }
        while let Some(result) = syncs.join_next().await {
            result.unwrap().unwrap();
        }

        let messages = writer.take_text();
        assert_eq!(messages.matches("textDocument/didChange").count(), 1);
        assert!(messages.contains(r#""version":2"#), "{messages}");
    }

    #[tokio::test]
    async fn sync_uses_extension_specific_language_ids() {
        assert_did_open_language_id(
            LanguageServerSpec::typescript_javascript(),
            "view.tsx",
            "typescriptreact",
        )
        .await;
        assert_did_open_language_id(
            LanguageServerSpec::typescript_javascript(),
            "runtime.js",
            "javascript",
        )
        .await;
        assert_did_open_language_id(LanguageServerSpec::python(), "types.pyi", "python").await;
        assert_did_open_language_id(LanguageServerSpec::go(), "main.go", "go").await;
    }

    #[tokio::test]
    async fn unavailable_server_error_includes_install_and_fallback_hint() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut spec = LanguageServerSpec::python();
        spec.command = temp
            .path()
            .join("definitely-missing-pyright-langserver")
            .display()
            .to_string();

        let error = LspClient::spawn(spec, temp.path().to_path_buf(), 0)
            .await
            .unwrap_err();
        let message = error.to_string();

        assert!(message.contains("npm install -g pyright"), "{message}");
        assert!(message.contains("symbol_search/grep"), "{message}");
    }

    async fn assert_did_open_language_id(
        spec: LanguageServerSpec,
        file_name: &str,
        expected: &str,
    ) {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join(file_name);
        std::fs::write(&file, "").unwrap();
        let file_uri = protocol::path_to_file_uri(&file).unwrap();
        let mut server = spawn_fake_server(file_uri);
        let client = LspClient::connect_for_test(
            spec,
            temp.path().to_path_buf(),
            server.client_reader,
            server.client_writer,
        )
        .await
        .unwrap();

        client.sync_document(&file).await.unwrap();
        let did_open = loop {
            let message = tokio::time::timeout(Duration::from_secs(1), server.messages.recv())
                .await
                .unwrap()
                .unwrap();
            if message.get("method").and_then(Value::as_str) == Some("textDocument/didOpen") {
                break message;
            }
        };

        assert_eq!(did_open["params"]["textDocument"]["languageId"], expected);
        server.task.abort();
    }

    struct FakeServer {
        client_reader: BoxedReader,
        client_writer: BoxedWriter,
        messages: tokio::sync::mpsc::UnboundedReceiver<Value>,
        task: tokio::task::JoinHandle<()>,
    }

    fn spawn_fake_server(file_uri: String) -> FakeServer {
        let (client_to_server_client, client_to_server_server) = tokio::io::duplex(8192);
        let (server_to_client_server, server_to_client_client) = tokio::io::duplex(8192);
        let client_reader: BoxedReader = Box::new(BufReader::new(server_to_client_client));
        let client_writer: BoxedWriter = Box::new(client_to_server_client);
        let mut server_reader = BufReader::new(client_to_server_server);
        let mut server_writer = server_to_client_server;
        let (message_tx, messages) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let Ok(Some(message)) = read_lsp_message(&mut server_reader).await else {
                    break;
                };
                let _ = message_tx.send(message.clone());
                let Some(method) = message.get("method").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                if let Some(id) = message.get("id").and_then(serde_json::Value::as_u64) {
                    let result = fake_result(method, &file_uri);
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": result,
                    });
                    write_lsp_message(&mut server_writer, &response)
                        .await
                        .unwrap();
                }
            }
        });
        FakeServer {
            client_reader,
            client_writer,
            messages,
            task,
        }
    }

    fn fake_result(method: &str, file_uri: &str) -> serde_json::Value {
        match method {
            "initialize" => json!({
                "capabilities": {
                    "definitionProvider": true,
                    "diagnosticProvider": { "interFileDependencies": false, "workspaceDiagnostics": false },
                }
            }),
            "textDocument/definition" => json!([{
                "uri": file_uri,
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 2 }
                }
            }]),
            "textDocument/diagnostic" => json!({
                "kind": "full",
                "items": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 2 }
                    },
                    "severity": 1,
                    "code": "fake",
                    "source": "fake-lsp",
                    "message": "fake error"
                }]
            }),
            _ => serde_json::Value::Null,
        }
    }
}
