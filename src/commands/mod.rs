use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, CompactionReport, CompactionRequest, CompactionSummarySource};
use crate::provider::ProviderRegistry;
use crate::session::SessionStore;
use crate::tui::app::TranscriptItem;
use crate::tui::pickers::ProviderOption;

mod handlers {
    pub(super) mod common;
    pub(super) mod headless;
}
mod authorize;
mod autonomy;
pub(crate) mod bug;
mod config_cmd;
mod hooks_cmd;
mod mcp_cmd;
mod memory;
mod metadata;
pub(crate) mod model_switch;
mod permissions;
mod providers;
mod providers_manage;
mod retry;
mod sandbox;
mod self_review;
mod serenity;
mod smol;
#[cfg(test)]
mod tests;
mod yolo;

pub(crate) use authorize::{
    ProviderAuthorizationState, authorize_provider_with_input_as_current, unauthorize_provider,
};
pub(crate) use autonomy::{
    AutonomyCommandRequest, autonomy_status_message, parse_autonomy_command,
};
pub(crate) use config_cmd::{
    ConfigCommandRequest, config_edit_message, format_config_diagnostics, format_config_view,
    parse_config_command,
};
pub(crate) use handlers::common::handle_command_with_catalog;
pub(crate) use hooks_cmd::{apply_hooks_command, parse_hooks_command};
pub(crate) use mcp_cmd::{McpCommandRequest, apply_mcp_command, parse_mcp_command};
pub(crate) use memory::{
    MEMORY_USAGE, MemoryCommandRequest, parse_memory_command, parse_remember_command,
    unknown_memory_entry_message,
};
#[cfg(test)]
pub(crate) use metadata::canonical_command_name;
pub use metadata::complete_command;
pub(crate) use metadata::{
    BusyCommandBehavior, COMMANDS, CommandMetadata, CommandSurface, busy_behavior_for,
    command_completion_preview, command_description, command_surface, command_usage_hint,
    complete_from_matches,
};
pub(crate) use model_switch::{
    apply_model_selection, context_window_downgrade_warning, rebuild_agent_provider,
    record_active_mode_model,
};
pub(crate) use permissions::{
    PermissionsCommandRequest, format_permission_rules, parse_permissions_command,
};
pub(crate) use providers::resolve_model_selection;
pub(crate) use providers::{
    RefreshSourceState, RefreshSourceStatus, ResolvedModelSelection, refresh_all_provider_models,
    refresh_models_dev_source, refresh_provider_source, refresh_stale_authorized_provider_models,
    refresh_stale_model_cache,
};
pub(crate) use retry::parse_retry_command;
pub(crate) use sandbox::{
    SandboxCommandRequest, parse_sandbox_command, sandbox_net_message, sandbox_set_message,
    sandbox_unavailable_message,
};
pub(crate) use self_review::{
    SelfReviewCommandRequest, parse_self_review_command, self_review_set_message,
    self_review_status_message,
};
pub(crate) use serenity::{
    SerenityCommandRequest, parse_serenity_command, serenity_set_message, serenity_status_message,
};
pub(crate) use smol::{
    SmolCommandRequest, parse_smol_command, smol_set_message, smol_status_message,
};
pub(crate) use yolo::{YoloCommandRequest, parse_yolo_command};

/// Optional runtime resources used by slash commands that inspect persisted or
/// catalog state in addition to the live agent/session pair.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CommandRuntimeContext<'a> {
    pub(crate) catalog: Option<&'a crate::model_catalog::ModelCatalog>,
    pub(crate) storage: Option<&'a crate::storage::Storage>,
    /// The built-in working mode active when the command was dispatched, so
    /// `/model` records the selection against that mode's per-mode entry.
    /// `None` (headless, custom personas) records nothing.
    pub(crate) active_mode: Option<crate::agent::AgentMode>,
}

/// Split a slash-command line into the whitespace-delimited argument tokens that
/// follow the command word, validating the leading token. Returns the args slice
/// on a match, or `Err(usage)` when `input` isn't `command`. Lets each command's
/// parser keep only its distinctive arms instead of repeating the token guard.
fn command_args<'a>(
    input: &'a str,
    command: &str,
    usage: &str,
) -> std::result::Result<Vec<&'a str>, String> {
    let mut parts = input.split_whitespace();
    if parts.next() != Some(command) {
        return Err(usage.to_string());
    }
    Ok(parts.collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandMessageKind {
    Status,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandMessage {
    pub kind: CommandMessageKind,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct CommandOutcome {
    pub quit: bool,
    pub clear_transcript: bool,
    pub messages: Vec<CommandMessage>,
    /// The single modal a command asks the TUI to open, if any. These are
    /// mutually exclusive — a command opens at most one — so they are one enum
    /// rather than a bag of independent flags.
    pub modal: Option<CommandModalRequest>,
}

/// A modal surface a command asks the TUI to open. Headless callers ignore these.
#[derive(Debug)]
pub enum CommandModalRequest {
    /// `/help` — the command-help popup.
    CommandHelp,
    /// `/keys` — the keyboard-shortcut popup.
    Keys,
    /// `/ctx` — a snapshot of the context window to render as a modal.
    ContextReport(Box<crate::agent::ContextReport>),
    /// `/episodes` — task-scoped context lifecycle from the same report snapshot.
    Episodes(Box<crate::agent::ContextReport>),
    /// `/model` — the model picker.
    ModelPicker,
    /// `/authorize` (no argument) — the provider picker.
    AuthorizeProviderPicker { providers: Vec<ProviderOption> },
    /// `/authorize <provider>` for API-key providers — the key entry prompt.
    ApiKeyPrompt { provider_id: String },
    /// `/mcp` — inspect detected MCP servers and discovered tools.
    McpServers {
        rows: Vec<crate::tui::mcp::McpServerRow>,
    },
    /// `/sandbox status` — open the interactive sandbox modal.
    SandboxStatus,
    /// `/doctor` (text mode) — open the read-only diagnostics summary modal.
    /// `/doctor json` bypasses this and prints the machine-readable report.
    Doctor(Box<crate::doctor::DoctorReport>),
}

#[cfg(test)]
impl CommandOutcome {
    fn opens_command_help(&self) -> bool {
        matches!(self.modal, Some(CommandModalRequest::CommandHelp))
    }

    fn opens_context_report(&self) -> bool {
        matches!(self.modal, Some(CommandModalRequest::ContextReport(_)))
    }

    fn opens_episodes(&self) -> bool {
        matches!(self.modal, Some(CommandModalRequest::Episodes(_)))
    }

    fn opens_model_picker(&self) -> bool {
        matches!(self.modal, Some(CommandModalRequest::ModelPicker))
    }

    fn api_key_prompt_provider(&self) -> Option<&str> {
        match &self.modal {
            Some(CommandModalRequest::ApiKeyPrompt { provider_id }) => Some(provider_id),
            _ => None,
        }
    }

    fn authorize_provider_picker(&self) -> Option<&[ProviderOption]> {
        match &self.modal {
            Some(CommandModalRequest::AuthorizeProviderPicker { providers }) => Some(providers),
            _ => None,
        }
    }

    fn opens_sandbox_status(&self) -> bool {
        matches!(self.modal, Some(CommandModalRequest::SandboxStatus))
    }

    fn mcp_server_rows(&self) -> Option<&[crate::tui::mcp::McpServerRow]> {
        match &self.modal {
            Some(CommandModalRequest::McpServers { rows }) => Some(rows),
            _ => None,
        }
    }
}

fn status(text: impl Into<String>) -> CommandMessage {
    CommandMessage {
        kind: CommandMessageKind::Status,
        text: text.into(),
    }
}

fn error(text: impl Into<String>) -> CommandMessage {
    CommandMessage {
        kind: CommandMessageKind::Error,
        text: text.into(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::commands) struct ParsedCommand<'a> {
    pub(in crate::commands) parts: Vec<&'a str>,
    pub(in crate::commands) arg: Option<&'a str>,
}

pub(in crate::commands) fn parse_command(input: &str) -> ParsedCommand<'_> {
    ParsedCommand {
        parts: input.split_whitespace().collect(),
        arg: input
            .trim()
            .split_once(char::is_whitespace)
            .map(|(_, rest)| rest.trim()),
    }
}
