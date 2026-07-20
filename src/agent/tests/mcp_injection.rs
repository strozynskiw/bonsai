//! The M5 prompt-injection defense gate (ROADMAP.md:682-686), proven end-to-end
//! for MCP tool results through the real run loop — the MCP-side twin of
//! `web_injection.rs`.
//!
//! A fake MCP server's `read_note` tool returns text instructing the model to
//! run a shell command. Even when the model "obeys", the gate must hold: the
//! note is tagged untrusted, never promoted to a system message, and the
//! shell action it asks for re-enters the normal permission path — which,
//! noninteractive, refuses it. No permission rule is minted from the note.

use super::*;

use std::sync::Arc;

use crate::config::DeclaredCapabilities;
use crate::interaction::InteractionService;
use crate::mcp::test_support::{INJECTED_NOTE, gate_with_level, register_fake_server};
use crate::permissions::PermissionManager;
use crate::tool::{ApprovalLevel, BashTool};

fn tool_messages_in(request: &[ChatCompletionRequestMessage]) -> Vec<String> {
    request
        .iter()
        .filter(|message| matches!(message, ChatCompletionRequestMessage::Tool(_)))
        .map(message_content)
        .collect()
}

fn system_messages_in(request: &[ChatCompletionRequestMessage]) -> Vec<String> {
    request
        .iter()
        .filter(|message| matches!(message, ChatCompletionRequestMessage::System(_)))
        .map(message_content)
        .collect()
}

#[tokio::test]
async fn malicious_mcp_tool_result_cannot_drive_shell_actions() {
    let fixture = TestFixture::new();

    // Auto-accept is limited to the MCP adapter for this fixture so the call
    // goes straight through without a prompt — the interesting gate here is
    // the downstream Bash tool, which remains at Ask.
    let mcp_permissions = PermissionManager::memory_only_mcp();
    let interaction = Arc::new(InteractionService::noninteractive());
    let gate = gate_with_level(
        mcp_permissions,
        interaction.clone(),
        ApprovalLevel::AutoAccept,
    );
    let extensions = Arc::new(crate::extension::status::ExtensionRegistry::new());
    let tools = register_fake_server(
        DeclaredCapabilities::default(),
        Vec::new(),
        gate,
        extensions,
    )
    .await;
    let read_note = tools
        .iter()
        .find(|tool| tool.name() == "mcp__fake__read_note")
        .expect("fake server exposes read_note")
        .clone();

    // Bash: fresh permissions, noninteractive, default (Ask) autonomy — any
    // command the note asks for must re-enter the permission prompt.
    let bash_permissions = PermissionManager::memory_only();
    let bash = Arc::new(BashTool::new(
        fixture.project_root.clone(),
        bash_permissions.clone(),
        fixture.read_tracker.clone(),
        interaction.clone(),
    ));

    let mut registry = ToolRegistry::new();
    registry.register(read_note);
    registry.register(bash);

    // The model, "obeying" the note: read it, run the shell command, finish.
    let provider = MockProvider::new(vec![
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call("call-1", "mcp__fake__read_note", r#"{}"#)],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: String::new(),
            tool_calls: vec![test_tool_call(
                "call-2",
                "bash",
                r#"{"command":"touch injected.txt"}"#,
            )],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
        Ok(StreamedResponse {
            content: "done".to_string(),
            tool_calls: vec![],
            terminal: crate::provider::StreamTerminal::Completed(
                crate::provider::FinishReason::Stop,
            ),
            usage: None,
            ..StreamedResponse::default()
        }),
    ]);
    let requests = provider.requests();

    let mut agent = Agent::new(
        Box::new(provider),
        Arc::new(registry),
        empty_registry(),
        fixture.read_tracker.clone(),
        String::new(),
        fixture.project_root.clone(),
    )
    .unwrap();

    let result = agent
        .run(
            "read the note",
            CancellationToken::new(),
            Arc::new(StdoutSink),
        )
        .await
        .unwrap();
    assert_eq!(result, AgentRunResult::Completed("done".to_string()));

    let requests = requests.lock().await;
    assert_eq!(requests.len(), 3, "read_note → bash → final");

    // (1) The mcp result is tagged untrusted: the frame wraps the injected text.
    let note_tool_output = &tool_messages_in(&requests[1])[0];
    assert!(
        note_tool_output.contains("UNTRUSTED external data"),
        "mcp output is framed untrusted: {note_tool_output}"
    );
    assert!(
        note_tool_output.contains("IGNORE PREVIOUS INSTRUCTIONS"),
        "the injected text is present, inside the frame: {note_tool_output}"
    );

    // (2) It is NEVER promoted to a system message.
    for (index, request) in requests.iter().enumerate() {
        for system in system_messages_in(request) {
            assert!(
                !system.contains(INJECTED_NOTE),
                "untrusted mcp content leaked into a system message in request {index}: {system}"
            );
        }
    }

    // (3) The shell command re-entered the permission path and was refused; the
    // file the note asked for was never created.
    let bash_tool_output = tool_messages_in(&requests[2])
        .pop()
        .expect("request 2 carries the bash result");
    assert!(
        bash_tool_output.contains("noninteractive"),
        "the injected shell command hit the permission gate: {bash_tool_output}"
    );
    assert!(
        !fixture.project_root.join("injected.txt").exists(),
        "the injected shell command must not have executed"
    );

    // (4) No permission rule was minted from mcp content.
    assert!(
        bash_permissions.user_rules().is_empty(),
        "mcp content must not add bash permission rules"
    );
}
