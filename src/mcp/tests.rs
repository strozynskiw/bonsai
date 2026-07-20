//! Fake-server suite (M5.2 acceptance gate, ROADMAP.md:696-697): an in-process
//! `rmcp` server over a `tokio::io::duplex` pair drives the *real* client path
//! — [`McpConnection`], [`McpTool`], [`ExtensionGate`] — with no real process
//! or network involved. See [`super::test_support`] for the fake server.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::test_support::{fake_server_config, gate_with, gate_with_level, register_fake_server};
use super::*;
use crate::config::schema::{BatchingPolicy, Capability, DeclaredCapabilities};
use crate::config::{ConfigSource, McpServerConfig, McpTransportConfig};
use crate::extension::status::ExtensionState;
use crate::interaction::{
    InteractionOutcome, InteractionRequest, InteractionService, PermissionDecision,
};
use crate::permissions::{Permission, PermissionManager};
use crate::tool::{ApprovalLevel, ParallelPolicy, Tool as BonsaiTool, ToolOutput, ToolRegistry};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn discovery_registers_namespaced_tools_appended_last() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let gate = gate_with(
        PermissionManager::memory_only_mcp(),
        Arc::new(InteractionService::noninteractive()),
    );
    let tools = register_fake_server(
        DeclaredCapabilities::default(),
        Vec::new(),
        gate,
        extensions.clone(),
    )
    .await;

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FakeNamed("todowrite")) as Arc<dyn BonsaiTool>);
    registry.register(Arc::new(FakeNamed("webfetch")) as Arc<dyn BonsaiTool>);
    register_mcp_tools(&mut registry, &tools, &extensions);

    let names: Vec<&str> = registry.names().collect();
    assert_eq!(
        names,
        vec![
            "todowrite",
            "webfetch",
            "mcp__fake__do_write",
            "mcp__fake__echo",
            "mcp__fake__read_note"
        ],
        "builtins unchanged, mcp tools appended after webfetch in sorted order"
    );
}

#[tokio::test]
async fn discovery_records_tool_inventory_in_status() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let gate = gate_with(
        PermissionManager::memory_only_mcp(),
        Arc::new(InteractionService::noninteractive()),
    );
    register_fake_server(
        DeclaredCapabilities::default(),
        Vec::new(),
        gate,
        extensions.clone(),
    )
    .await;

    let snapshot = extensions.snapshot();
    let status = snapshot
        .iter()
        .find(|status| status.id == ExtensionId::McpServer("fake".to_string()))
        .expect("fake server has a status entry");
    let names: Vec<&str> = status.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["do_write", "echo", "read_note"]);
    let echo = status
        .tools
        .iter()
        .find(|t| t.name == "echo")
        .expect("echo is in the inventory");
    assert_eq!(echo.description, "Echo the input back");
}

#[tokio::test]
async fn tool_name_collision_marks_server_degraded_and_keeps_builtin() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let gate = gate_with(
        PermissionManager::memory_only_mcp(),
        Arc::new(InteractionService::noninteractive()),
    );
    let tools = register_fake_server(
        DeclaredCapabilities::default(),
        Vec::new(),
        gate,
        extensions.clone(),
    )
    .await;

    let mut registry = ToolRegistry::new();
    // A builtin whose name collides with one discovered mcp tool's wire name.
    registry.register(Arc::new(FakeNamed("mcp__fake__echo")) as Arc<dyn BonsaiTool>);
    register_mcp_tools(&mut registry, &tools, &extensions);

    // The builtin is untouched; the other two mcp tools still registered.
    let names: Vec<&str> = registry.names().collect();
    assert_eq!(
        names,
        vec![
            "mcp__fake__echo",
            "mcp__fake__do_write",
            "mcp__fake__read_note"
        ]
    );
    let echo_tool = registry.get("mcp__fake__echo").unwrap();
    assert_eq!(
        echo_tool.description(),
        "fake builtin",
        "the builtin wins the name, not the mcp tool"
    );

    let snapshot = extensions.snapshot();
    let fake_status = snapshot
        .iter()
        .find(|status| status.id == ExtensionId::McpServer("fake".to_string()))
        .expect("fake server has a status entry");
    assert!(
        matches!(fake_status.state, ExtensionState::Degraded { .. }),
        "{:?}",
        fake_status.state
    );
    // The skipped tool is dropped from the inventory `/mcp tools` reads, so it
    // never lists a tool that dispatches to the shadowing builtin instead.
    let names: Vec<&str> = fake_status.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["do_write", "read_note"]);
}

#[tokio::test]
async fn undeclared_capabilities_default_to_serialized_and_high_tier() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let gate = gate_with(
        PermissionManager::memory_only_mcp(),
        Arc::new(InteractionService::noninteractive()),
    );
    let tools = register_fake_server(
        DeclaredCapabilities::default(),
        Vec::new(),
        gate,
        extensions,
    )
    .await;

    for tool in &tools {
        assert_eq!(
            tool.parallel_policy(),
            ParallelPolicy::Serialized,
            "{}",
            tool.name()
        );
    }
}

#[tokio::test]
async fn declared_path_scoped_read_batches_path_scoped() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let gate = gate_with(
        PermissionManager::memory_only_mcp(),
        Arc::new(InteractionService::noninteractive()),
    );
    let declared = DeclaredCapabilities {
        capabilities: vec![Capability::Read],
        batching: BatchingPolicy::PathScoped,
    };
    let tools = register_fake_server(declared, Vec::new(), gate, extensions).await;

    for tool in &tools {
        assert_eq!(
            tool.parallel_policy(),
            ParallelPolicy::PathScoped,
            "{}",
            tool.name()
        );
    }
}

#[tokio::test]
async fn allowlist_filters_discovered_tools() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let gate = gate_with(
        PermissionManager::memory_only_mcp(),
        Arc::new(InteractionService::noninteractive()),
    );
    let tools = register_fake_server(
        DeclaredCapabilities::default(),
        vec!["echo".to_string()],
        gate,
        extensions.clone(),
    )
    .await;

    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "mcp__fake__echo");

    // The filtered set is what the status inventory reports to `/mcp tools`.
    let snapshot = extensions.snapshot();
    let status = snapshot
        .iter()
        .find(|status| status.id == ExtensionId::McpServer("fake".to_string()))
        .expect("fake server has a status entry");
    let names: Vec<&str> = status.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["echo"]);
}

#[tokio::test]
async fn server_spawn_failure_degrades_visibly() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let gate = gate_with(
        PermissionManager::memory_only_mcp(),
        Arc::new(InteractionService::noninteractive()),
    );
    // A command that cannot possibly spawn.
    let bogus = McpServerConfig {
        transport: McpTransportConfig::Stdio {
            command: "definitely-not-a-real-binary-xyz".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        },
        ..fake_server_config(DeclaredCapabilities::default(), Vec::new())
    };

    let sandbox = crate::sandbox::CommandSandbox::disabled();
    let credentials = CredentialStore::memory();
    let oauth_store = Arc::new(McpOAuthStore::new(
        credentials.clone(),
        "bogus",
        "stdio",
        CredentialPersistence::File,
    ));
    let (entry, tools) = attempt_and_register(
        "bogus".to_string(),
        bogus.clone(),
        ConfigSource::Project,
        McpRuntime {
            gate,
            extensions: extensions.clone(),
            sandbox: sandbox.clone(),
            project_root: std::path::PathBuf::new(),
            credentials,
            credential_persistence: CredentialPersistence::File,
        },
        oauth_store.clone(),
        connect_and_list(&bogus, &sandbox, std::path::Path::new("/"), &oauth_store),
    )
    .await;

    assert_eq!(entry.0, "bogus");
    assert!(entry.1.connection().await.is_none());
    assert!(tools.is_empty());
    let snapshot = extensions.snapshot();
    let status = snapshot
        .iter()
        .find(|status| status.id == ExtensionId::McpServer("bogus".to_string()))
        .expect("bogus server has a status entry");
    assert!(
        matches!(status.state, ExtensionState::Failed { .. }),
        "{:?}",
        status.state
    );
    assert!(status.tools.is_empty(), "a failed server has no inventory");
}

#[tokio::test]
async fn expired_oauth_login_degrades_only_that_server_at_startup() {
    let remote = MockServer::start().await;
    let resource_url = format!("{}/mcp", remote.uri());
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "resource": resource_url.clone(),
            "authorization_servers": [remote.uri()]
        })))
        .mount(&remote)
        .await;
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "issuer": remote.uri(),
            "authorization_endpoint": format!("{}/authorize", remote.uri()),
            "token_endpoint": format!("{}/token", remote.uri()),
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"]
        })))
        .mount(&remote)
        .await;

    let server = McpServerConfig {
        transport: McpTransportConfig::Http {
            url: resource_url.clone(),
            headers: BTreeMap::new(),
            oauth_client_id: None,
            oauth_scopes: Vec::new(),
        },
        ..fake_server_config(DeclaredCapabilities::default(), Vec::new())
    };
    let credentials = CredentialStore::memory();
    let oauth_store = Arc::new(McpOAuthStore::new(
        credentials.clone(),
        "expired",
        &resource_url,
        CredentialPersistence::File,
    ));
    let expired = serde_json::from_value(serde_json::json!({
        "client_id": "expired-client",
        "token_response": {
            "access_token": "expired-access-token",
            "token_type": "Bearer",
            "expires_in": 1
        },
        "granted_scopes": ["mcp:tools"],
        "token_received_at": 0
    }))
    .unwrap();
    rmcp::transport::auth::CredentialStore::save(oauth_store.as_ref(), expired)
        .await
        .unwrap();

    let extensions = Arc::new(ExtensionRegistry::new());
    let gate = gate_with(
        PermissionManager::memory_only_mcp(),
        Arc::new(InteractionService::noninteractive()),
    );
    let sandbox = crate::sandbox::CommandSandbox::disabled();
    let (entry, tools) = attempt_and_register(
        "expired".to_string(),
        server.clone(),
        ConfigSource::Global,
        McpRuntime {
            gate,
            extensions: extensions.clone(),
            sandbox: sandbox.clone(),
            project_root: std::path::PathBuf::new(),
            credentials,
            credential_persistence: CredentialPersistence::File,
        },
        oauth_store.clone(),
        connect_and_list(&server, &sandbox, std::path::Path::new("/"), &oauth_store),
    )
    .await;

    assert_eq!(entry.0, "expired");
    assert!(entry.1.connection().await.is_none());
    assert!(tools.is_empty());
    let status = extensions
        .snapshot()
        .into_iter()
        .find(|status| status.id == ExtensionId::McpServer("expired".to_string()))
        .unwrap();
    let ExtensionState::Failed { error } = status.state else {
        panic!("expired login must be failed, got {:?}", status.state);
    };
    assert!(error.contains("authorize again"), "{error}");
    assert!(error.contains("/mcp auth expired"), "{error}");
}

#[test]
fn http_failure_points_to_oauth_without_overriding_static_auth() {
    let mut server = fake_server_config(DeclaredCapabilities::default(), Vec::new());
    server.transport = McpTransportConfig::Http {
        url: "https://mcp.example.test/mcp".to_string(),
        headers: BTreeMap::new(),
        oauth_client_id: None,
        oauth_scopes: Vec::new(),
    };
    let message =
        connection_failure_message("remote", &server, &anyhow::anyhow!("HTTP 401 Unauthorized"));
    assert!(message.contains("/mcp auth remote"), "{message}");

    let McpTransportConfig::Http { headers, .. } = &mut server.transport else {
        panic!("test server must use HTTP");
    };
    headers.insert("Authorization".to_string(), "Bearer ${TOKEN}".to_string());
    let message =
        connection_failure_message("remote", &server, &anyhow::anyhow!("HTTP 401 Unauthorized"));
    assert!(!message.contains("/mcp auth"), "{message}");
}

#[tokio::test]
async fn loopback_callback_reader_accepts_only_the_callback_path() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let writer = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /callback?code=one&state=two HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let callback = read_callback_url(&mut stream, address).await.unwrap();
    writer.await.unwrap();
    assert_eq!(
        callback,
        format!("http://{address}/callback?code=one&state=two")
    );

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let writer = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /not-callback HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    let error = read_callback_url(&mut stream, address).await.unwrap_err();
    writer.await.unwrap();
    assert!(error.to_string().contains("callback path"), "{error:#}");
}

#[tokio::test]
async fn tool_result_is_untrusted_framed() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let permissions = PermissionManager::memory_only_mcp();
    let gate = gate_with_level(
        permissions,
        Arc::new(InteractionService::noninteractive()),
        ApprovalLevel::AutoAccept,
    );
    let tools = register_fake_server(
        DeclaredCapabilities::default(),
        Vec::new(),
        gate,
        extensions,
    )
    .await;
    let echo = tools
        .iter()
        .find(|t| t.name() == "mcp__fake__echo")
        .unwrap();

    let output = echo
        .execute(serde_json::json!({"text": "hello"}))
        .await
        .unwrap();
    assert_eq!(
        output.execution_status(),
        crate::output::ToolExecutionStatus::Succeeded
    );
    match output {
        ToolOutput::UntrustedContext { content, .. } => {
            assert!(content.contains("<<<untrusted-content"), "{content}");
            assert!(content.contains("hello"), "{content}");
        }
        other => panic!("expected untrusted context, got {other:?}"),
    }
}

#[tokio::test]
async fn is_error_result_is_untrusted_framed() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let permissions = PermissionManager::memory_only_mcp();
    let gate = gate_with_level(
        permissions.clone(),
        Arc::new(InteractionService::noninteractive()),
        ApprovalLevel::AutoAccept,
    );
    let tools = register_fake_server(
        DeclaredCapabilities::default(),
        Vec::new(),
        gate,
        extensions,
    )
    .await;
    let do_write = tools
        .iter()
        .find(|t| t.name() == "mcp__fake__do_write")
        .unwrap();

    let output = do_write.execute(serde_json::json!({})).await.unwrap();
    assert_eq!(
        output.execution_status(),
        crate::output::ToolExecutionStatus::Failed
    );
    match output {
        ToolOutput::UntrustedContext { content, .. } => {
            assert!(content.contains("<<<untrusted-content"), "{content}");
            assert!(content.contains("write not permitted"), "{content}");
        }
        other => panic!("expected untrusted context, got {other:?}"),
    }
}

#[tokio::test]
async fn permission_prompt_renders_extension_request_and_deny_errors() {
    let extensions = Arc::new(ExtensionRegistry::new());
    let (interaction, mut rx) = InteractionService::new();
    let interaction = Arc::new(interaction);
    let gate = gate_with(PermissionManager::memory_only_mcp(), interaction.clone());
    let declared = DeclaredCapabilities {
        capabilities: vec![Capability::Write],
        batching: BatchingPolicy::Serialized,
    };
    let tools = register_fake_server(declared, Vec::new(), gate, extensions).await;
    let echo = tools
        .iter()
        .find(|t| t.name() == "mcp__fake__echo")
        .unwrap()
        .clone();

    let responder = tokio::spawn(async move {
        let request = rx.recv().await.expect("a permission request");
        match request {
            InteractionRequest::ExtensionTool {
                request_id,
                id,
                server,
                capabilities,
                ..
            } => {
                assert_eq!(id, "mcp.fake.echo");
                assert_eq!(server, "fake");
                assert_eq!(capabilities, vec!["write".to_string()]);
                interaction
                    .respond(
                        request_id,
                        InteractionOutcome::Permission(PermissionDecision::Deny),
                    )
                    .await
                    .unwrap();
            }
            other => panic!("expected an extension tool request, got {other:?}"),
        }
    });

    let err = echo
        .execute(serde_json::json!({"text": "hi"}))
        .await
        .expect_err("denied call errors");
    assert!(err.to_string().contains("Permission denied"), "{err}");
    responder.await.unwrap();
}

#[tokio::test]
async fn allow_for_project_persists_an_mcp_kind_rule() {
    let ts = crate::storage::test_utils::TestStorage::new().await;
    let project_id = ts.storage.ensure_project(ts.project_path()).await.unwrap();
    let mcp_permissions = PermissionManager::load_mcp(ts.storage.clone(), project_id)
        .await
        .unwrap();

    let extensions = Arc::new(ExtensionRegistry::new());
    let (interaction, mut rx) = InteractionService::new();
    let interaction = Arc::new(interaction);
    let gate = gate_with(mcp_permissions.clone(), interaction.clone());
    let tools = register_fake_server(
        DeclaredCapabilities::default(),
        Vec::new(),
        gate,
        extensions,
    )
    .await;
    let echo = tools
        .iter()
        .find(|t| t.name() == "mcp__fake__echo")
        .unwrap()
        .clone();

    let responder = tokio::spawn(async move {
        let request = rx.recv().await.expect("a permission request");
        let InteractionRequest::ExtensionTool { request_id, .. } = request else {
            panic!("expected an extension tool request");
        };
        interaction
            .respond(
                request_id,
                InteractionOutcome::Permission(PermissionDecision::AllowForProject),
            )
            .await
            .unwrap();
    });

    echo.execute(serde_json::json!({"text": "hi"}))
        .await
        .unwrap();
    responder.await.unwrap();

    let rules = mcp_permissions.user_rules();
    assert_eq!(rules.len(), 1, "{rules:?}");
    assert!(
        rules[0].pattern.starts_with("mcp.fake.echo@"),
        "grant must be bound to the tool declaration: {:?}",
        rules[0]
    );
    assert_eq!(rules[0].permission, Permission::Allow);
}

/// A minimal builtin stand-in with a fixed name, for collision tests.
struct FakeNamed(&'static str);

#[test]
fn reachability_report_is_stable_and_secret_free() {
    let report = reachability_report(vec![
        ("zeta".to_string(), false),
        ("alpha".to_string(), true),
        ("beta".to_string(), false),
    ]);

    assert_eq!(report.enabled, 3);
    assert_eq!(report.reachable, 1);
    assert_eq!(report.failed_servers, ["beta", "zeta"]);
}

#[async_trait::async_trait]
impl BonsaiTool for FakeNamed {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "fake builtin"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::Text(String::new()))
    }
}
