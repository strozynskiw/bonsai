//! One `session/prompt` = one controller message turn, streamed as
//! `session/update` notifications until the run's terminal lifecycle event.

use std::sync::atomic::Ordering;
use std::time::Duration;

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    ContentBlock, ContentChunk, EmbeddedResourceResource, PromptResponse, RequestPermissionRequest,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall, ToolCallContent,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, Error, ErrorCode};
use tokio::sync::broadcast::error::RecvError;

use super::projection::{Carrier, Projector, carrier};
use super::session::{Handle, internal, invalid};
use crate::interaction::InteractionRequest;
use crate::server::sink::SessionEvent;

type Connection = ConnectionTo<Client>;

/// Flatten ACP prompt blocks into the user turn bonsai runs. Only text and
/// embedded/linked context are advertised; anything else is refused.
pub(crate) fn prompt_text(blocks: &[ContentBlock]) -> Result<String, Error> {
    let mut text = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text(part) => text.push_str(&part.text),
            ContentBlock::ResourceLink(link) => {
                text.push_str(&format!(" @{} ", link.uri));
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(file) => {
                    let ticks = "`".repeat(3.max(longest_run(&file.text) + 1));
                    text.push_str(&format!(
                        "\n\n{ticks} {}\n{}\n{ticks}\n",
                        file.uri,
                        file.text.trim_end_matches('\n')
                    ));
                }
                _ => return Err(invalid("binary embedded resources are not supported")),
            },
            _ => {
                return Err(invalid(
                    "bonsai accepts text, resource links and embedded text resources only",
                ));
            }
        }
    }
    if text.trim().is_empty() {
        return Err(invalid("prompt is empty"));
    }
    Ok(text)
}

fn longest_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

pub(crate) async fn run(
    handle: Handle,
    connection: Connection,
    prompt: String,
) -> Result<PromptResponse, Error> {
    handle.cancelled.store(false, Ordering::Release);
    // Subscribe before admitting the message so no event of this run is missed.
    let (_, mut events) = handle.controller.subscribe().await.map_err(internal)?;
    let request = uuid::Uuid::now_v7().to_string();
    let command = admit(&handle, &request, &prompt).await?;
    if command.state != "accepted" {
        return Err(internal(anyhow::anyhow!(
            "Message was not accepted ({})",
            command.state
        )));
    }
    let run = command.run_id;
    let mut projector = Projector::new(&handle.display_root);
    let state = loop {
        let event = match events.recv().await {
            Ok(ordered) => ordered.event,
            Err(RecvError::Lagged(_)) => SessionEvent::ResyncRequired,
            Err(RecvError::Closed) => break "failed".to_string(),
        };
        match carrier(&event) {
            Carrier::Update(_) => {
                if let Some(update) = projector.update(&event) {
                    notify(&connection, &handle, update)?;
                }
            }
            Carrier::RequestPermission => {
                if let SessionEvent::Interaction(request) = event {
                    let tool = tool_for(&projector, &request);
                    tokio::spawn(ask(
                        connection.clone(),
                        handle.clone(),
                        run.clone(),
                        request,
                        tool,
                    ));
                }
            }
            Carrier::PromptResponse => {
                if let SessionEvent::Lifecycle {
                    run_id: Some(ended),
                    state,
                } = event
                    && ended == run
                    && state != "running"
                {
                    break state;
                }
            }
            Carrier::Reconcile => reconcile(&connection, &handle, &mut projector).await?,
            Carrier::NotCarried => {}
        }
    };
    reconcile(&connection, &handle, &mut projector).await?;
    let cancelled = handle.cancelled.load(Ordering::Acquire);
    // Whatever is still open was cut off by cancellation or failure.
    for id in projector.in_flight_ids() {
        let mut fields = ToolCallUpdateFields::new().status(ToolCallStatus::Failed);
        if cancelled {
            fields = fields.content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new("Interrupted"),
            ))]);
        }
        notify(
            &connection,
            &handle,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(id.as_str()),
                fields,
            )),
        )?;
    }
    if cancelled {
        // Clients render `cancelled` silently; say what happened in the thread.
        notify(
            &connection,
            &handle,
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("\n\n_Stopped._\n"),
            ))),
        )?;
        return Ok(PromptResponse::new(StopReason::Cancelled));
    }
    if state == "failed" {
        // Same redaction discipline as the HTTP contract's `execution_failed`.
        return Err(Error::new(
            ErrorCode::INTERNAL_ERROR,
            "Execution failed; inspect local diagnostics (`bonsai doctor`, session log)",
        ));
    }
    Ok(PromptResponse::new(StopReason::EndTurn))
}

/// The previous run publishes its terminal lifecycle a moment before its
/// worker task finishes; tolerate that window instead of failing the prompt.
async fn admit(
    handle: &Handle,
    request: &str,
    prompt: &str,
) -> Result<crate::storage::ServerCommand, Error> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match handle.controller.message(handle.id, request, prompt).await {
            Ok(command) => return Ok(command),
            Err(error)
                if error.to_string() == "Session is busy"
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) if error.to_string() == "Session is busy" => {
                return Err(Error::new(
                    ErrorCode::INVALID_REQUEST,
                    "A prompt is already running in this session",
                ));
            }
            Err(error) => return Err(internal(error)),
        }
    }
}

fn notify(connection: &Connection, handle: &Handle, update: SessionUpdate) -> Result<(), Error> {
    connection.send_notification(SessionNotification::new(handle.acp_id.clone(), update))
}

/// Settle tool calls whose results were lost to a lagged broadcast receiver.
async fn reconcile(
    connection: &Connection,
    handle: &Handle,
    projector: &mut Projector,
) -> Result<(), Error> {
    if projector.in_flight_ids().is_empty() {
        return Ok(());
    }
    let Ok((snapshot, _)) = handle.controller.subscribe().await else {
        return Ok(());
    };
    for item in &snapshot.transcript {
        let activities: Vec<&crate::protocol::ToolActivity> = match item {
            crate::protocol::TranscriptItem::ToolActivity(activity) => vec![activity],
            crate::protocol::TranscriptItem::ExecutionGroup(group) => group.tools.iter().collect(),
            _ => continue,
        };
        for activity in activities {
            if let Some(update) = projector.reconcile(activity) {
                notify(connection, handle, update)?;
            }
        }
    }
    Ok(())
}

/// The in-flight tool call a prompt belongs to, by the tool that raises it.
/// Prompts that name their tool (`edit: path [workspace-write]`) attach to it;
/// shell prompts attach to the latest shell-like call.
fn tool_for(projector: &Projector, request: &InteractionRequest) -> Option<String> {
    let shell = |name: &str| matches!(name, "bash" | "terminal" | "tasks" | "git");
    match request {
        InteractionRequest::Permission { command, .. }
        | InteractionRequest::SandboxEscalation { command, .. } => projector
            .find(|name| command.starts_with(&format!("{name}:")))
            .or_else(|| projector.find(shell)),
        InteractionRequest::WebDomain { .. } => {
            projector.find(|name| matches!(name, "webfetch" | "websearch"))
        }
        InteractionRequest::Question { .. } => projector.find(|name| name == "question"),
        InteractionRequest::ExtensionTool { id, .. } => projector
            .find(|name| name == id)
            .or_else(|| projector.current_tool().map(|(id, _)| id.clone())),
        InteractionRequest::HookTrust { .. } => None,
    }
}

/// Ask the client, then hand only its decision to the controller. Any failure
/// on the way (transport, cancellation, unknown option) is a denial.
async fn ask(
    connection: Connection,
    handle: Handle,
    run: String,
    request: InteractionRequest,
    tool: Option<String>,
) {
    let request_id = super::request_id(&request);
    let Some(options) = super::permission::options(&request) else {
        let _ = handle
            .controller
            .respond(
                handle.id,
                &run,
                request_id,
                super::permission::deny(&request),
            )
            .await;
        return;
    };
    let mut fields = super::permission::describe(&request);
    let (tool_call_id, synthetic) = match tool {
        Some(id) => (id, false),
        None => (format!("permission-{request_id}"), true),
    };
    if !synthetic {
        // Keep the real call's kind; the prompt only retitles and pauses it.
        fields.kind = None;
    }
    if synthetic {
        let mut card = ToolCall::new(ToolCallId::new(tool_call_id.as_str()), "Permission");
        card.update(fields.clone());
        let _ = notify(&connection, &handle, SessionUpdate::ToolCall(card));
    }
    let answer = connection
        .send_request(RequestPermissionRequest::new(
            handle.acp_id.clone(),
            ToolCallUpdate::new(ToolCallId::new(tool_call_id.as_str()), fields),
            options,
        ))
        .block_task()
        .await;
    let outcome = super::permission::outcome(
        &request,
        answer.as_ref().ok().map(|response| &response.outcome),
    );
    let granted = !matches!(
        outcome,
        crate::interaction::InteractionOutcome::Permission(
            crate::interaction::PermissionDecision::Deny
        ) | crate::interaction::InteractionOutcome::SandboxEscalation(
            crate::interaction::EscalationDecision::Deny
        ) | crate::interaction::InteractionOutcome::Question(None)
    );
    if let Err(error) = handle
        .controller
        .respond(handle.id, &run, request_id, outcome)
        .await
    {
        tracing::debug!(%error, "ACP permission answer was stale");
    }
    if synthetic {
        let status = if granted {
            ToolCallStatus::Completed
        } else {
            ToolCallStatus::Failed
        };
        let _ = notify(
            &connection,
            &handle,
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(tool_call_id.as_str()),
                ToolCallUpdateFields::new().status(status),
            )),
        );
    }
}
