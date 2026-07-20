use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::BufReader;

use super::client::{BoxedReader, BoxedWriter, LspClient, read_lsp_message, write_lsp_message};
use super::{LanguageServerRegistry, LanguageServerSpec, LspHub};

pub(crate) async fn hub_with_diagnostic_sequence(
    project_root: PathBuf,
    diagnostics: Vec<Vec<Value>>,
) -> Arc<LspHub> {
    let canonical_root = project_root.canonicalize().unwrap_or(project_root.clone());
    let spec = LanguageServerSpec::rust();
    let registry = LanguageServerRegistry::new(vec![spec.clone()]);
    let hub = Arc::new(LspHub::with_registry(project_root.clone(), registry));
    let server = spawn_diagnostic_server(diagnostics);
    let client = LspClient::connect_for_test(
        spec.clone(),
        canonical_root.clone(),
        server.client_reader,
        server.client_writer,
    )
    .await
    .unwrap();
    hub.insert_client_for_test(spec.id, canonical_root, Arc::new(client))
        .await;
    hub
}

struct FakeServer {
    client_reader: BoxedReader,
    client_writer: BoxedWriter,
}

fn spawn_diagnostic_server(diagnostics: Vec<Vec<Value>>) -> FakeServer {
    let (client_to_server_client, client_to_server_server) = tokio::io::duplex(16 * 1024);
    let (server_to_client_server, server_to_client_client) = tokio::io::duplex(16 * 1024);
    let client_reader: BoxedReader = Box::new(BufReader::new(server_to_client_client));
    let client_writer: BoxedWriter = Box::new(client_to_server_client);
    let mut server_reader = BufReader::new(client_to_server_server);
    let mut server_writer = server_to_client_server;
    tokio::spawn(async move {
        let mut diagnostics = diagnostics.into_iter();
        loop {
            let Ok(Some(message)) = read_lsp_message(&mut server_reader).await else {
                break;
            };
            let Some(method) = message.get("method").and_then(Value::as_str) else {
                continue;
            };
            let Some(id) = message.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let result = match method {
                "initialize" => json!({
                    "capabilities": {
                        "textDocumentSync": 1,
                        "diagnosticProvider": {
                            "interFileDependencies": false,
                            "workspaceDiagnostics": false
                        }
                    }
                }),
                "textDocument/diagnostic" => {
                    let items = diagnostics.next().unwrap_or_default();
                    json!({ "kind": "full", "items": items })
                }
                _ => Value::Null,
            };
            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            });
            let _ = write_lsp_message(&mut server_writer, &response).await;
        }
    });
    FakeServer {
        client_reader,
        client_writer,
    }
}
