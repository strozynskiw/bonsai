use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::{Mutex, mpsc};

use crate::agent::{
    Agent, AgentMode, ContextControlAction, ContextPreviewInput, ContextPreviewUserInput,
    QueuedUserMessage,
};
use crate::background::{
    BackgroundTaskEvent, BackgroundTaskRegistry, BackgroundTaskSnapshot, BackgroundTaskStatus,
};
use crate::diff::FileDiff;
use crate::interaction::{InteractionRequest, InteractionService};
use crate::model_catalog::ModelCatalog;
use crate::model_resolution::{
    build_provider, context_window_for_current_model_with_catalog,
    prompt_estimator_for_current_model_with_catalog, resolved_model_for_provider_model,
};
use crate::output::{OutputSink, SharedSink, ToolCallStart};
use crate::plan::SharedPlanStore;
use crate::provider::ProviderRegistry;
use crate::runtime::RuntimeCore;
use crate::session::SessionStore;
use crate::storage::{
    ForgetSessionOutcome, ResumeSessionOutcome, SavedPlanId, SessionId, SessionStatus, Storage,
};
use crate::terminal::{TerminalEvent, TerminalRegistry, TerminalSnapshot, TerminalStatus};
use crate::todo::SharedTodoStore;
use crate::tool::SharedActiveSessionId;
use crate::tui::app::{
    AppState, FollowUpDelivery, PhaseAdvance, PlanExecution, ToolActivity, ToolStatus,
    TranscriptItem, TranscriptModel, clamped_scroll,
};
use crate::tui::event::{
    AppAction, CommandOutcomeEvent, CommandOutputEvent, CommandOutputKind, Focus, ModalKind,
    PlanOpenMode, ProviderRunSelection, RuntimeEvent, TaskState, UiError, UiEvent, View,
};
use crate::tui::keymap::{KeyIntent, map_key};
use crate::tui::mouse::map_mouse;
use crate::tui::pickers::{ModelOption, ModelSelection};
use crate::tui::runtime_actions::{RuntimeActionDeps, RuntimeActionResult, handle_runtime_action};
use crate::tui::task::{CommandTaskDeps, TaskController};
use crate::yolo::YoloMode;

mod commands;
mod ctrl_c;
mod event_loop;
mod persistence;
mod setup;
mod terminal_input;

use commands::*;
use persistence::*;
use setup::{TerminalSession, TuiSink, git_branch};

pub(super) use commands::{
    apply_context_control_action, apply_non_idle_read_only_command, commit_changes,
    create_pull_request, implement_plan, implement_plan_with_context, mark_started_saved_plan,
    non_empty_trimmed, open_cached_model_picker, open_context_modal_with_preview,
    open_start_plan_choice, open_theme_picker, persist_current_theme, push_command_message,
    push_transient_notice, review_changes, run_build_profile, run_test_profile,
    switch_model_selection,
};
pub(super) use event_loop::{open_background_task_list, start_delete_selected_background_task};
pub(super) use persistence::{
    PersistenceCommandDeps, PersistenceCommandState, ReviewCanvasPreflightDeps, clear_canvas_plan,
    delete_saved_plan_from_picker, open_saved_plan, protect_canvas_before_new_plan,
    protect_canvas_before_review, resume_session,
};

/// Project paths and optional recovery identity for an interactive run.
#[derive(Debug, Clone)]
pub(crate) struct TuiRunContext {
    workspace_root: PathBuf,
    session_project_root: PathBuf,
    recovery_id: Option<crate::storage::RecoveryId>,
}

impl TuiRunContext {
    /// Build a normal interactive context rooted at one project directory.
    pub(crate) fn for_project(project_root: PathBuf) -> Self {
        Self {
            workspace_root: project_root.clone(),
            session_project_root: project_root,
            recovery_id: None,
        }
    }

    /// Build an isolated recovery context backed by its source project.
    pub(crate) fn for_recovery(
        workspace_root: PathBuf,
        session_project_root: PathBuf,
        recovery_id: crate::storage::RecoveryId,
    ) -> Self {
        Self {
            workspace_root,
            session_project_root,
            recovery_id: Some(recovery_id),
        }
    }

    /// Filesystem root used by tools during this run.
    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Stable source-project root used for persisted session identity.
    pub(crate) fn session_project_root(&self) -> &Path {
        &self.session_project_root
    }
}

/// Complete runtime consumed by the interactive event loop.
pub(crate) struct TuiRuntime {
    agent: Agent,
    session_store: Arc<Mutex<SessionStore>>,
    registry: Arc<ProviderRegistry>,
    todo_store: SharedTodoStore,
    plan_store: SharedPlanStore,
    interaction_rx: mpsc::UnboundedReceiver<InteractionRequest>,
    interaction: Arc<InteractionService>,
    storage: Storage,
    model_catalog: Arc<ModelCatalog>,
    resume: Option<crate::cli::ResumeTarget>,
    active_session_id: SharedActiveSessionId,
    background_tasks: Arc<BackgroundTaskRegistry>,
    terminals: Arc<TerminalRegistry>,
    background_wakes: Arc<crate::background_wake::BackgroundWakeCoordinator>,
    subagents: Arc<crate::subagent::SubagentRegistry>,
    subagent_model_overrides: crate::subagent::SubagentModelOverrides,
    permissions: crate::permissions::PermissionManager,
    domain_permissions: crate::permissions::PermissionManager,
    peer_bus: Arc<crate::peer::PeerBus>,
    project_root: PathBuf,
    session_project_root: PathBuf,
    recovery_id: Option<crate::storage::RecoveryId>,
    accessibility: crate::tui::accessibility::TuiAccessibility,
}

impl TuiRuntime {
    /// Bind the shared runtime to one interactive launch context.
    pub(crate) fn new(
        runtime: RuntimeCore,
        interaction_rx: mpsc::UnboundedReceiver<InteractionRequest>,
        resume: Option<crate::cli::ResumeTarget>,
        run_context: TuiRunContext,
        accessibility: crate::tui::accessibility::TuiAccessibility,
    ) -> Self {
        let RuntimeCore {
            agent,
            storage,
            session_store,
            registry,
            model_catalog,
            todo_store,
            plan_store,
            interaction,
            active_session_id,
            background_tasks,
            terminals,
            background_wakes,
            subagents,
            subagent_model_overrides,
            permissions,
            domain_permissions,
            peer_bus,
            ..
        } = runtime;
        let TuiRunContext {
            workspace_root: project_root,
            session_project_root,
            recovery_id,
        } = run_context;
        Self {
            agent,
            session_store,
            registry,
            todo_store,
            plan_store,
            interaction_rx,
            interaction,
            storage,
            model_catalog,
            resume,
            active_session_id,
            background_tasks,
            terminals,
            background_wakes,
            subagents,
            subagent_model_overrides,
            permissions,
            domain_permissions,
            peer_bus,
            project_root,
            session_project_root,
            recovery_id,
            accessibility,
        }
    }
}

impl std::fmt::Debug for TuiRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TuiRuntime")
            .field("project_root", &self.project_root)
            .field("session_project_root", &self.session_project_root)
            .field("has_resume", &self.resume.is_some())
            .field("recovery_id", &self.recovery_id)
            .field("accessibility", &self.accessibility)
            .finish_non_exhaustive()
    }
}

/// Run the interactive terminal surface until shutdown.
///
/// # Errors
///
/// Returns an error when terminal setup, runtime initialization, persistence,
/// or bounded shutdown fails.
pub(crate) async fn run_tui(runtime: TuiRuntime) -> Result<()> {
    event_loop::run(runtime).await
}

#[cfg(test)]
mod runtime_context_tests {
    use super::TuiRunContext;
    use std::path::PathBuf;

    #[test]
    fn project_context_uses_one_root_for_tools_and_sessions() {
        let root = PathBuf::from("/tmp/project");
        let context = TuiRunContext::for_project(root.clone());

        assert_eq!(context.workspace_root, root);
        assert_eq!(context.session_project_root, root);
        assert!(context.recovery_id.is_none());
    }

    #[test]
    fn recovery_context_keeps_execution_and_session_roots_distinct() {
        let workspace_root = PathBuf::from("/tmp/recovery-worktree");
        let session_project_root = PathBuf::from("/tmp/source-project");
        let recovery_id = crate::storage::RecoveryId::parse("recovery-1").unwrap();
        let context = TuiRunContext::for_recovery(
            workspace_root.clone(),
            session_project_root.clone(),
            recovery_id.clone(),
        );

        assert_eq!(context.workspace_root, workspace_root);
        assert_eq!(context.session_project_root, session_project_root);
        assert_eq!(context.recovery_id, Some(recovery_id));
    }
}
