//! `bonsai acp`: Agent Client Protocol v1 over stdio (JSON-RPC 2.0, one
//! message per line). The editor spawns bonsai as a subprocess; every request
//! drives the same `server::Controller` engine as `bonsai serve`.
//!
//! Handlers return immediately and move long work (runtime construction,
//! turns, permission round-trips) onto tokio tasks, because the SDK's
//! dispatch loop is blocked while a handler runs and `session/cancel` must
//! still get through mid-turn.

mod permission;
pub(crate) mod projection;
mod session;
mod turn;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, CancelNotification, Implementation, InitializeRequest, InitializeResponse,
    ListSessionsRequest, ListSessionsResponse, LoadSessionRequest, LoadSessionResponse,
    McpCapabilities, NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest,
    SessionCapabilities, SessionConfigGroupId, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigOptionValue, SessionConfigSelectGroup, SessionConfigSelectOption,
    SessionListCapabilities, SessionNotification, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse,
};
use agent_client_protocol::{Agent, ByteStreams};
use anyhow::{Context, Result};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::interaction::InteractionRequest;
use crate::storage::Storage;
use session::Sessions;

/// Usage text for `bonsai acp --help`.
pub(crate) const USAGE: &str = "Usage: bonsai acp\n\n\
Run bonsai as an Agent Client Protocol (v1) agent over stdio. Editors such as\n\
Zed spawn this command; it is not meant to be run interactively.\n\
See docs/acp.md for editor setup and the list of features ACP cannot carry.";

pub(crate) async fn run() -> Result<()> {
    let launch_root = std::env::current_dir()?
        .canonicalize()
        .context("Cannot resolve the launch directory")?;
    let storage = Storage::open().await?;
    let sessions = Arc::new(Sessions::new(launch_root, storage));
    let transport = ByteStreams::new(
        tokio::io::stdout().compat_write(),
        tokio::io::stdin().compat(),
    );
    let connection = Agent
        .v1()
        .name("bonsai")
        .on_receive_request(
            async |request: InitializeRequest, responder, _cx| {
                responder.respond(initialize(&request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |request: NewSessionRequest, responder, _cx| {
                    let sessions = sessions.clone();
                    tokio::spawn(async move {
                        let created = sessions
                            .create(&request.cwd, &request.mcp_servers)
                            .await
                            .map(|handle| {
                                NewSessionResponse::new(handle.acp_id.clone())
                                    .config_options(config_options(&handle))
                            });
                        let _ = responder.respond_with_result(created);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |request: LoadSessionRequest, responder, cx| {
                    let sessions = sessions.clone();
                    tokio::spawn(async move {
                        let loaded = async {
                            let handle = sessions
                                .load(&request.session_id, &request.cwd, &request.mcp_servers)
                                .await?;
                            // ACP requires the full history as updates before the
                            // response; bonsai replays the durable transcript.
                            let (snapshot, _) = handle
                                .controller
                                .subscribe()
                                .await
                                .map_err(session::internal)?;
                            for update in
                                projection::replay(&handle.display_root, &snapshot.transcript)
                            {
                                cx.send_notification(SessionNotification::new(
                                    handle.acp_id.clone(),
                                    update,
                                ))?;
                            }
                            Ok(LoadSessionResponse::new().config_options(config_options(&handle)))
                        }
                        .await;
                        let _ = responder.respond_with_result(loaded);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |request: PromptRequest, responder, cx| {
                    let sessions = sessions.clone();
                    tokio::spawn(async move {
                        let result = async {
                            let text = turn::prompt_text(&request.prompt)?;
                            let handle = sessions.handle(&request.session_id).await?;
                            turn::run(handle, cx, text).await
                        }
                        .await;
                        let _ = responder.respond_with_result(result);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |request: SetSessionConfigOptionRequest, responder, _cx| {
                    let sessions = sessions.clone();
                    tokio::spawn(async move {
                        let result = async {
                            let SessionConfigOptionValue::ValueId { value } = &request.value else {
                                return Err(session::invalid("expected a select value"));
                            };
                            let handle = match &*request.config_id.0 {
                                MODEL_CONFIG_ID => {
                                    sessions.set_model(&request.session_id, &value.0).await?
                                }
                                AUTONOMY_CONFIG_ID => {
                                    sessions.set_autonomy(&request.session_id, &value.0).await?
                                }
                                _ => {
                                    return Err(session::invalid(
                                        "bonsai offers only the `model` and `autonomy` options",
                                    ));
                                }
                            };
                            Ok(SetSessionConfigOptionResponse::new(config_options(&handle)))
                        }
                        .await;
                        let _ = responder.respond_with_result(result);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |request: ListSessionsRequest, responder, _cx| {
                    let sessions = sessions.clone();
                    tokio::spawn(async move {
                        // One page only: a cursor never issued means nothing more.
                        let listed = if request.cursor.is_some() {
                            Ok(Vec::new())
                        } else {
                            sessions.list(request.cwd.as_deref()).await
                        };
                        let _ =
                            responder.respond_with_result(listed.map(ListSessionsResponse::new));
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let sessions = sessions.clone();
                async move |notification: CancelNotification, _cx| {
                    if let Ok(handle) = sessions.handle(&notification.session_id).await {
                        handle.cancelled.store(true, Ordering::Release);
                        if let Err(error) = handle.controller.interrupt(handle.id).await {
                            tracing::debug!(%error, "ACP cancel found no running turn");
                        }
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await;
    let drained = sessions.shutdown().await;
    connection.map_err(|error| anyhow::anyhow!("ACP connection failed: {error}"))?;
    drained
}

fn initialize(request: &InitializeRequest) -> InitializeResponse {
    // v1 is the only version bonsai speaks; a client asking for a newer one
    // receives v1 and decides whether to continue.
    let version = if request.protocol_version < ProtocolVersion::V1 {
        request.protocol_version
    } else {
        ProtocolVersion::V1
    };
    InitializeResponse::new(version)
        .agent_capabilities(
            AgentCapabilities::new()
                .load_session(true)
                .prompt_capabilities(
                    PromptCapabilities::new()
                        .image(false)
                        .audio(false)
                        .embedded_context(true),
                )
                .mcp_capabilities(McpCapabilities::new().http(false).sse(false))
                .session_capabilities(
                    SessionCapabilities::new().list(SessionListCapabilities::new()),
                ),
        )
        .agent_info(Implementation::new("bonsai", env!("CARGO_PKG_VERSION")).title("bonsai"))
}

const MODEL_CONFIG_ID: &str = "model";
const AUTONOMY_CONFIG_ID: &str = "autonomy";

/// The model picker: every model of every authorized connection, grouped by
/// connection, exactly the TUI `/model` list. Values are `connection::model`.
fn config_options(handle: &session::Handle) -> Vec<SessionConfigOption> {
    let value = |provider: &str, model: &str| {
        format!("{provider}{}{model}", session::MODEL_VALUE_SEPARATOR)
    };
    let current = handle.current_selection();
    let mut groups: Vec<SessionConfigSelectGroup> = handle
        .models
        .iter()
        .map(|group| {
            SessionConfigSelectGroup::new(
                SessionConfigGroupId::new(group.provider_id.as_str()),
                group.label.as_str(),
                group
                    .models
                    .iter()
                    .map(|model| {
                        SessionConfigSelectOption::new(
                            value(&group.provider_id, model),
                            model.as_str(),
                        )
                    })
                    .collect(),
            )
        })
        .collect();
    let listed = handle
        .models
        .iter()
        .any(|g| g.provider_id == current.provider_id && g.models.contains(&current.model));
    if !listed {
        // Never hide the model the session actually runs on.
        groups.insert(
            0,
            SessionConfigSelectGroup::new(
                SessionConfigGroupId::new(current.provider_id.as_str()),
                current.provider_id.as_str(),
                vec![SessionConfigSelectOption::new(
                    value(&current.provider_id, &current.model),
                    current.model.as_str(),
                )],
            ),
        );
    }
    vec![
        SessionConfigOption::select(
            MODEL_CONFIG_ID,
            "Model",
            value(&current.provider_id, &current.model),
            groups,
        )
        .category(SessionConfigOptionCategory::Model)
        .description("Models of the connections authorized in bonsai"),
        autonomy_option(handle),
    ]
}

/// Approval levels from `ask` up to the operator's saved TUI level. Sessions
/// start at `ask`; the editor can never select above the saved level.
fn autonomy_option(handle: &session::Handle) -> SessionConfigOption {
    let levels: Vec<SessionConfigSelectOption> = crate::tool::ApprovalLevel::ALL
        .into_iter()
        .filter(|level| *level <= handle.autonomy_ceiling)
        .map(|level| {
            SessionConfigSelectOption::new(level.label(), level.label())
                .description(level.summary())
        })
        .collect();
    SessionConfigOption::select(
        AUTONOMY_CONFIG_ID,
        "Autonomy",
        handle.autonomy.level().label(),
        levels,
    )
    .category(SessionConfigOptionCategory::Mode)
    .description("Which actions run without asking; capped at the level saved in bonsai")
}

fn request_id(request: &InteractionRequest) -> u64 {
    match request {
        InteractionRequest::Permission { request_id, .. }
        | InteractionRequest::SandboxEscalation { request_id, .. }
        | InteractionRequest::WebDomain { request_id, .. }
        | InteractionRequest::Question { request_id, .. }
        | InteractionRequest::ExtensionTool { request_id, .. }
        | InteractionRequest::HookTrust { request_id, .. } => *request_id,
    }
}
