use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, ScrollbarOrientation, ScrollbarState, Wrap};

use crate::agent::ContextReport;
pub(super) use crate::background::{BackgroundTaskSnapshot, BackgroundTaskStatus};
use crate::diff::FileDiff;
pub(super) use crate::subagent::{SubagentSnapshot, SubagentStatus};
use crate::tui::app::{
    AppState, CONTEXT_WIRE_RAW_JSON_ID, ContextViewMode, ModelPickerPane, ProviderAuthField,
    ToolActivity, ToolStatus, TranscriptItem,
};
use crate::tui::event::{ModalKind, ModeRow, PlanOpenMode, StartPlanMode, SubtaskListPane};
use crate::tui::pickers::{ModelOption, ProviderOption};
use crate::tui::theme;
use crate::tui::widgets::transcript;

const TOOL_DETAIL_DIFF_MAX_ROWS: usize = 80;
const DIFF_PREVIEW_MAX_ROWS: usize = 240;
const ARG_PREVIEW_MAX_CHARS: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuestionPromptMetrics {
    pub body_height: u16,
    pub max_scroll: u16,
    pub selected_start: u16,
    pub selected_end: u16,
}

mod agent_browser;
mod agent_composer;
mod auth;
mod busy;
mod common;
mod context;
mod context_turns;
mod detail;
mod doctor;
mod episodes;
mod help;
mod list_detail;
mod local_model;
mod mcp;
mod memory;
mod mode;
mod model;
mod onboarding;
mod peers;
mod perf;
mod picker_common;
mod prompt;
mod providers;
mod refresh;
mod sandbox;
mod saved;
mod settings;
mod skill_manager;
mod subtasks;
mod table;
mod tasks;
mod theme_picker;
mod usage;
mod usage_heatmap;
mod usage_tabs;

use agent_browser::*;
use agent_composer::*;
use auth::*;
use busy::*;
pub(crate) use common::*;
use context::*;
use context_turns::*;
use detail::*;
use doctor::*;
use episodes::*;
use help::*;
use list_detail::*;
use local_model::*;
use mcp::*;
use memory::*;
use mode::*;
pub(crate) use model::model_picker_capacities;
use model::render_model_picker;
use onboarding::*;
use peers::*;
use perf::*;
use prompt::*;
use providers::*;
use refresh::*;
use sandbox::*;
use saved::*;
use settings::*;
use skill_manager::*;
use subtasks::*;
use table::*;
use tasks::*;
use theme_picker::*;
use usage::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextModalMetrics {
    pub body_height: u16,
    pub max_scroll: u16,
    /// First visual row of the selected row's line (auto-follow anchor).
    pub selected_line: Option<u16>,
    /// One past the last visual row of the selected row's block — its sources,
    /// preview, and child rows — so an expand can reveal what it opened.
    pub selected_block_end: Option<u16>,
}

pub fn render(f: &mut Frame, area: Rect, app: &AppState) {
    let Some(kind) = app.modal.as_ref() else {
        return;
    };

    // Reset the selectable-body cache every frame; the renderers below that
    // support text selection repopulate it. Without this, swapping to a modal
    // that doesn't cache (pickers, context) would leave the previous modal's
    // body behind, and clicks would resolve against invisible text.
    app.modal_body_lines.borrow_mut().clear();
    app.modal_body_rect.set(None);

    // Reading surfaces (tool detail, diff) get a large window; everything
    // else stays a compact dialog.
    let modal = modal_area(area, kind);
    f.render_widget(Clear, modal);

    match kind {
        ModalKind::Onboarding { step, cursor } => render_onboarding(f, modal, app, *step, *cursor),
        ModalKind::Help => render_help(f, modal, app),
        ModalKind::CommandHelp => render_command_help(f, modal),
        ModalKind::Context(report) => render_context(f, modal, app, report),
        ModalKind::Episodes { report, cursor } => render_episodes(f, modal, app, report, *cursor),
        ModalKind::PerfReport { title, lines } => render_perf_report(f, modal, app, title, lines),
        ModalKind::ToolDetail { tool_id } => render_tool_detail(f, modal, app, tool_id),
        ModalKind::BlockDetail { item_index } => render_block_detail(f, modal, app, *item_index),
        ModalKind::PlanFindingDetail { index } => render_plan_finding_detail(f, modal, app, *index),
        ModalKind::DiffPreview { tool_id } => render_diff_preview(f, modal, app, tool_id),
        ModalKind::ApiKeyPrompt { provider_id, .. } => {
            render_api_key_prompt(f, modal, app, provider_id)
        }
        ModalKind::AuthorizeProviderPicker {
            providers,
            query,
            cursor,
        } => render_authorize_provider_picker(f, modal, app, providers, query, *cursor),
        ModalKind::UnauthorizeProviderPicker {
            providers,
            query,
            cursor,
        } => render_unauthorize_provider_picker(f, modal, app, providers, query, *cursor),
        ModalKind::UnauthorizeConfirm { display_name, .. } => {
            render_unauthorize_confirm(f, modal, display_name)
        }
        ModalKind::ModelPicker { entries } => render_model_picker(f, modal, app, entries),
        ModalKind::SessionPicker { sessions, cursor } => {
            render_session_picker(f, modal, sessions, *cursor)
        }
        ModalKind::PlanPicker {
            plans,
            query,
            cursor,
        } => render_plan_picker(f, modal, plans, query, *cursor),
        ModalKind::PlanOpenChoice { plan, cursor } => {
            render_plan_open_choice(f, modal, plan, *cursor)
        }
        ModalKind::StartPlanChoice { cursor } => render_start_plan_choice(f, modal, *cursor),
        ModalKind::PlanDeleteConfirm { plan } => render_plan_delete_confirm(f, modal, plan),
        ModalKind::PlanDiscardConfirm {
            saved_plan_id,
            title,
        } => render_plan_discard_confirm(f, modal, *saved_plan_id, title),
        ModalKind::ReviewScopePicker { cursor } => render_review_scope_picker(f, modal, *cursor),
        ModalKind::LocalModelWizard { state } => render_local_model_wizard(f, modal, state),
        ModalKind::AgentBrowser { rows, cursor } => render_agent_browser(f, modal, rows, *cursor),
        ModalKind::ProviderManager {
            rows,
            filter,
            searching,
            cursor,
        } => render_provider_manager(f, modal, app, rows, filter, *searching, *cursor),
        ModalKind::ProviderDetail { detail } => {
            render_provider_detail(f, modal, app, detail, app.modal_scroll)
        }
        ModalKind::ProviderRemoveConfirm {
            display_name,
            disable_builtin,
            ..
        } => render_provider_remove_confirm(f, modal, display_name, *disable_builtin),
        ModalKind::SkillManager { rows, cursor } => {
            render_skill_manager(f, modal, app, rows, *cursor)
        }
        ModalKind::MemoryManager { rows, cursor } => {
            render_memory_manager(f, modal, app, rows, *cursor)
        }
        ModalKind::MemoryAddWizard { state } => render_memory_wizard(f, modal, app, state),
        ModalKind::Settings { rows, cursor } => render_settings(f, modal, rows, *cursor),
        ModalKind::AgentComposer { state } => render_agent_composer(f, modal, state),
        ModalKind::AgentDeleteConfirm { name, path } => {
            render_agent_delete_confirm(f, modal, name, path)
        }
        ModalKind::McpServers { rows, cursor } => render_mcp_servers(f, modal, app, rows, *cursor),
        ModalKind::SandboxStatus { cursor } => render_sandbox_status(f, modal, app, *cursor),
        ModalKind::Doctor { report, cursor } => render_doctor(f, modal, report, *cursor),
        ModalKind::ThemePicker { cursor, .. } => render_theme_picker(f, modal, *cursor),
        ModalKind::ModePicker { rows, cursor } => render_mode_picker(f, modal, rows, *cursor),
        ModalKind::BusyCommand {
            input,
            rows,
            cursor,
        } => render_busy_command(f, modal, input, rows, *cursor),
        ModalKind::TaskList { tasks, cursor } => render_task_list(f, modal, app, tasks, *cursor),
        ModalKind::PeerList { peers, cursor } => render_peer_list(f, modal, peers, *cursor),
        ModalKind::Refresh {
            sources, cursor, ..
        } => render_refresh(f, modal, app, sources, *cursor),
        ModalKind::UsageDashboard { dashboard, tab } => {
            render_usage_dashboard(f, modal, app, dashboard, *tab)
        }
        ModalKind::SubtaskList {
            subtasks,
            cursor,
            pane,
        } => render_subtask_list(f, modal, app, subtasks, *cursor, *pane),
        ModalKind::PermissionPrompt {
            command, origin, ..
        } => render_confirm_prompt(
            f,
            modal,
            app,
            &permission_prompt(command, origin.as_deref()),
            app.modal_scroll,
        ),
        ModalKind::SandboxEscalationPrompt {
            command, origin, ..
        } => render_confirm_prompt(
            f,
            modal,
            app,
            &sandbox_escalation_prompt(command, origin.as_deref()),
            app.modal_scroll,
        ),
        ModalKind::WebDomainPrompt {
            url,
            host,
            redirected_from,
            origin,
            ..
        } => render_confirm_prompt(
            f,
            modal,
            app,
            &web_domain_prompt(host, url, redirected_from.as_deref(), origin.as_deref()),
            app.modal_scroll,
        ),
        ModalKind::ExtensionToolPrompt {
            id,
            server,
            capabilities,
            args_preview,
            ..
        } => render_confirm_prompt(
            f,
            modal,
            app,
            &extension_tool_prompt(id, server, capabilities, args_preview),
            app.modal_scroll,
        ),
        ModalKind::HookTrustPrompt {
            name,
            event,
            action_kind,
            action_preview,
            ..
        } => render_confirm_prompt(
            f,
            modal,
            app,
            &hook_trust_prompt(name, event, action_kind, action_preview),
            app.modal_scroll,
        ),
        ModalKind::QuestionPrompt {
            prompt,
            header,
            origin,
            options,
            multiple,
            cursor,
            selected,
            ..
        } => render_question_prompt(
            f,
            modal,
            prompt,
            header.as_deref(),
            origin.as_deref(),
            options,
            *multiple,
            *cursor,
            selected,
            app.modal_scroll,
        ),
    }
}

#[cfg(test)]
mod tests;
