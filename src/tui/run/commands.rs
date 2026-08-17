use super::*;

/// Returns the argument of a `/theme` invocation, or None when the input is
/// some other command. An empty argument means "open the picker".
pub(in crate::tui::run) fn theme_command_arg(input: &str) -> Option<&str> {
    let rest = input.trim().strip_prefix("/theme")?;
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

/// A slash command the idle Submit arm handles inline (rather than deferring
/// to the generic background command task). Parsed up front so the Submit arm
/// echoes the command into the transcript in exactly one place and dispatches
/// through one match, instead of a copy-pasted
/// `if input == "/foo" { echo; handle; continue }` block per command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui::run) enum IdleSlashCommand<'a> {
    /// `/theme [name]` — an empty argument opens the picker.
    Theme(&'a str),
    Start,
    Continue,
    Test,
    Build,
    Retry,
    Commit,
    /// `/init` generates concise, evidence-based project steering.
    Init,
    /// `/init` accepts no arguments.
    InitWithArgs,
    PullRequest,
    Review,
    SecurityReview,
    Mode,
    Settings,
    Wizard,
    /// `/providers [subcommand]` — the provider manager; the argument grammar
    /// lives in `runtime_actions::handle_providers_command`.
    Providers(&'a str),
    Skills,
    AgentBrowser,
    AgentComposer,
    Model,
    Ctx,
    /// `/episodes` — opens the Episodes modal (TUI-only), reusing the cached
    /// context-report snapshot.
    Episodes,
    Autonomy,
    SelfReview,
    Pure,
    Smol,
    Serenity,
    Sandbox,
    Persistence(PersistenceCommand<'a>),
}

impl IdleSlashCommand<'_> {
    pub(in crate::tui::run) const fn dispatches_model_work(self) -> bool {
        matches!(
            self,
            Self::Start
                | Self::Continue
                | Self::Test
                | Self::Build
                | Self::Retry
                | Self::Commit
                | Self::Init
                | Self::PullRequest
                | Self::Review
                | Self::SecurityReview
        )
    }
}

pub(in crate::tui::run) fn idle_slash_command(input: &str) -> Option<IdleSlashCommand<'_>> {
    if let Some(arg) = theme_command_arg(input) {
        return Some(IdleSlashCommand::Theme(arg));
    }
    // Exact-form commands: given arguments they fall through to the generic
    // command task instead (e.g. `/model sonnet` switches models there; only
    // a bare `/model` opens the picker inline).
    match input {
        "/start" => return Some(IdleSlashCommand::Start),
        "/continue" => return Some(IdleSlashCommand::Continue),
        "/test" => return Some(IdleSlashCommand::Test),
        "/build" => return Some(IdleSlashCommand::Build),
        "/commit" => return Some(IdleSlashCommand::Commit),
        "/init" => return Some(IdleSlashCommand::Init),
        "/pr" => return Some(IdleSlashCommand::PullRequest),
        "/review" => return Some(IdleSlashCommand::Review),
        "/security-review" => return Some(IdleSlashCommand::SecurityReview),
        "/mode" => return Some(IdleSlashCommand::Mode),
        "/settings" => return Some(IdleSlashCommand::Settings),
        "/skills" => return Some(IdleSlashCommand::Skills),
        "/agents" => return Some(IdleSlashCommand::AgentBrowser),
        "/agents new" => return Some(IdleSlashCommand::AgentComposer),
        "/model" => return Some(IdleSlashCommand::Model),
        "/ctx" => return Some(IdleSlashCommand::Ctx),
        "/episodes" => return Some(IdleSlashCommand::Episodes),
        _ => {}
    }
    if input == "/wizard" || input.starts_with("/wizard ") {
        return Some(IdleSlashCommand::Wizard);
    }
    if input == "/providers" {
        return Some(IdleSlashCommand::Providers(""));
    }
    if let Some(rest) = input.strip_prefix("/retry")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        return Some(IdleSlashCommand::Retry);
    }
    // `/init` is deliberately bare-only: reject every prefixed form here so a
    // typo such as `/initfoo` cannot become an ordinary coding-agent prompt.
    if input.starts_with("/init") {
        return Some(IdleSlashCommand::InitWithArgs);
    }
    if let Some(rest) = input.strip_prefix("/providers ") {
        return Some(IdleSlashCommand::Providers(rest.trim()));
    }
    // Token-form commands: their argument grammar lives in their `apply_*`
    // handler's parser, so any argument list routes to the handler.
    match input.split_whitespace().next() {
        Some("/autonomy") | Some("/yolo") => return Some(IdleSlashCommand::Autonomy),
        Some("/self-review") => return Some(IdleSlashCommand::SelfReview),
        Some("/pure") => return Some(IdleSlashCommand::Pure),
        Some("/smol") => return Some(IdleSlashCommand::Smol),
        Some("/serenity") => return Some(IdleSlashCommand::Serenity),
        Some("/sandbox") => return Some(IdleSlashCommand::Sandbox),
        _ => {}
    }
    persistence_command(input).map(IdleSlashCommand::Persistence)
}

/// `/update`: force a signed-release check + install in a detached background
/// task and report the outcome to the transcript when it finishes. Works in
/// any task state — the updater never touches the agent or the conversation,
/// and concurrent bonsai processes serialize on the update file lock.
pub(in crate::tui::run) fn spawn_update_command(
    app: &mut AppState,
    sender: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    bonsai_home: std::path::PathBuf,
    config: crate::config::UpdateConfig,
) {
    push_transient_notice(app, "Checking for the newest release…");
    tokio::spawn(async move {
        let outcome = crate::update::run_forced_update(&bonsai_home, &config, false).await;
        tracing::debug!(?outcome, "/update finished");
        let (message, is_error) = crate::update::forced_outcome_message(&outcome);
        let _ = sender.send(RuntimeEvent::UpdateCommandFinished {
            message,
            kind: if is_error {
                CommandOutputKind::Error
            } else {
                CommandOutputKind::Status
            },
            // Reuse the startup hint mapping so a staged (or install-blocked)
            // update also arms the persistent "restart to apply" meta line.
            staged_notice: crate::update::startup_notice(&outcome),
        });
    });
}

pub(in crate::tui) fn open_theme_picker(app: &mut AppState) {
    rescan_themes_and_report(app);
    let original_theme = crate::tui::theme::current_theme_name().to_string();
    app.reduce(AppAction::OpenModal(ModalKind::Picker(
        crate::tui::event::PickerModal::ThemePicker {
            cursor: crate::tui::theme::current_theme_index(),
            original_theme,
        },
    )));
}

/// Rescans custom theme files (picking up newly added or edited ones) and
/// reports any load errors as a single transcript row. Run before opening the
/// picker or applying `/theme <name>` — this is the user-visible moment a
/// malformed theme file's errors surface (startup only logs them).
fn rescan_themes_and_report(app: &mut AppState) {
    let errors = crate::tui::theme::rescan_themes();
    if !errors.is_empty() {
        app.reduce(AppAction::CommandOutput {
            kind: CommandOutputKind::Error,
            text: errors.join("\n"),
        });
    }
}

pub(in crate::tui) async fn persist_current_theme(
    session_store: Arc<Mutex<SessionStore>>,
) -> anyhow::Result<()> {
    let session_snapshot = {
        let mut guard = session_store.lock().await;
        guard.theme = crate::tui::theme::current_theme_name().to_string();
        guard.clone()
    };
    session_snapshot.save_async().await
}

/// Switches the UI theme directly and persists the choice in the session store.
/// Rescans custom theme files first so `/theme <name>` can select a file the user
/// just dropped in; empty/unknown names are routed to the picker by the caller.
/// `/theme export <name>` writes the active palette to a starter TOML file.
pub(in crate::tui::run) async fn apply_theme_command(
    arg: &str,
    app: &mut AppState,
    project_root: &std::path::Path,
    session_store: Arc<Mutex<SessionStore>>,
) {
    // `export` is a reserved subcommand (whitespace-delimited so a theme named
    // e.g. `exported` still resolves as a normal `/theme <name>` switch).
    if let Some(rest) = arg.strip_prefix("export")
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
    {
        export_theme(rest.trim(), app, project_root).await;
        return;
    }

    rescan_themes_and_report(app);
    let result = if crate::tui::theme::set_theme(arg) {
        Ok(())
    } else {
        let names = crate::tui::theme::theme_names();
        Err(format!(
            "Unknown theme '{arg}'. Available: {}.",
            names.join(", ")
        ))
    };

    if result.is_ok()
        && let Err(err) = persist_current_theme(session_store).await
    {
        app.reduce(AppAction::CommandOutput {
            kind: CommandOutputKind::Error,
            text: format!("Theme applied but could not be saved: {err:#}"),
        });
    }
    if let Err(text) = result {
        app.reduce(AppAction::CommandOutput {
            kind: CommandOutputKind::Error,
            text,
        });
    }
}

/// Writes the active theme's (pre-adaptation) palette to
/// `.bonsai/themes/<name>.toml` as a fully-commented starter file. Refuses to
/// overwrite an existing file; the validated name can't traverse out of the dir.
async fn export_theme(name: &str, app: &mut AppState, project_root: &std::path::Path) {
    if name.is_empty() {
        push_command_message(
            app,
            CommandOutputKind::Status,
            "Usage: /theme export <name>",
        );
        return;
    }
    let Some(name) = crate::tui::theme::spec::normalize_theme_name(name) else {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Invalid theme name '{name}' (allowed: a-z, 0-9, '_', '-')."),
        );
        return;
    };
    let dir = project_root.join(".bonsai").join("themes");
    let path = dir.join(format!("{name}.toml"));
    if path.exists() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!(
                "Theme file already exists: {} (refusing to overwrite).",
                path.display()
            ),
        );
        return;
    }
    let contents =
        crate::tui::theme::spec::render_theme_toml(crate::tui::theme::current_original());
    if let Err(err) = tokio::fs::create_dir_all(&dir).await {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Could not create {}: {err}", dir.display()),
        );
        return;
    }
    if let Err(err) = tokio::fs::write(&path, contents).await {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Could not write {}: {err}", path.display()),
        );
        return;
    }
    push_transient_notice(
        app,
        &format!(
            "Exported current theme to {}. Edit it, then run /theme {name}.",
            path.display()
        ),
    );
}

/// Set the session's approval level: update the shared holder and mirror it
/// into view state.
async fn set_level(
    app: &mut AppState,
    yolo_mode: &YoloMode,
    storage: &crate::storage::Storage,
    level: crate::tool::ApprovalLevel,
) {
    yolo_mode.set_level(level);
    app.reduce(AppAction::SetApprovalLevel(level));
    if let Err(error) = storage.set_approval_level(level).await {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Failed to save autonomy setting: {error:#}"),
        );
    }
}

/// Apply an `/autonomy` or `/yolo` invocation. `/yolo` is a shortcut onto the
/// same axis (`on` → `yolo`, `off` → `ask`). Shared by the running and idle
/// dispatch paths; caller echoes the command into the transcript first.
pub(in crate::tui::run) async fn apply_autonomy_command(
    input: &str,
    app: &mut AppState,
    yolo_mode: &YoloMode,
    storage: &crate::storage::Storage,
) {
    use crate::commands::{AutonomyCommandRequest, YoloCommandRequest};
    use crate::tool::ApprovalLevel;

    let status_now = |app: &mut AppState| {
        push_transient_notice(
            app,
            &crate::commands::autonomy_status_message(app.approval_level),
        );
    };

    match input.split_whitespace().next() {
        Some("/autonomy") => match crate::commands::parse_autonomy_command(input) {
            Ok(AutonomyCommandRequest::Set(level)) => {
                set_level(app, yolo_mode, storage, level).await
            }
            Ok(AutonomyCommandRequest::Status) => status_now(app),
            Err(message) => push_command_message(app, CommandOutputKind::Error, &message),
        },
        Some("/yolo") => match crate::commands::parse_yolo_command(input) {
            Ok(YoloCommandRequest::Set(true)) => {
                set_level(app, yolo_mode, storage, ApprovalLevel::Yolo).await
            }
            Ok(YoloCommandRequest::Set(false)) => {
                set_level(app, yolo_mode, storage, ApprovalLevel::Ask).await
            }
            Ok(YoloCommandRequest::Toggle) => {
                let target = if app.approval_level == ApprovalLevel::Yolo {
                    ApprovalLevel::Ask
                } else {
                    ApprovalLevel::Yolo
                };
                set_level(app, yolo_mode, storage, target).await;
            }
            Ok(YoloCommandRequest::Status) => status_now(app),
            Err(message) => push_command_message(app, CommandOutputKind::Error, &message),
        },
        _ => {}
    }
}

/// Apply `/sandbox` against the shared sandbox handle mirrored into `AppState`.
/// Shared by running and idle dispatch; caller echoes the command first.
pub(in crate::tui::run) async fn apply_sandbox_command(
    input: &str,
    app: &mut AppState,
    storage: &crate::storage::Storage,
) {
    use crate::commands::SandboxCommandRequest;

    let Some(sandbox) = app.sandbox.clone() else {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Sandbox state is not available in this TUI session.",
        );
        return;
    };

    match crate::commands::parse_sandbox_command(input) {
        Ok(SandboxCommandRequest::Set(on)) => {
            if on && !sandbox.backend().is_available() {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &crate::commands::sandbox_unavailable_message(),
                );
                return;
            }
            sandbox.set_enabled(on);
            if let Err(error) = storage.set_sandbox_enabled(on).await {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save sandbox setting: {error:#}"),
                );
            }
        }
        Ok(SandboxCommandRequest::SetNet(on)) => {
            sandbox.set_deny_network(on);
            if let Err(error) = storage.set_sandbox_deny_network(on).await {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save sandbox network setting: {error:#}"),
                );
            }
        }
        Ok(SandboxCommandRequest::Status) => {
            app.reduce(AppAction::OpenModal(ModalKind::Manager(
                crate::tui::event::ManagerModal::SandboxStatus { cursor: 0 },
            )));
        }
        Err(message) => push_command_message(app, CommandOutputKind::Error, &message),
    }
}

/// Apply a `/self-review` invocation against the live agent and mirror it into
/// view state so reopening `/mode` shows the latest policy.
#[allow(clippy::too_many_arguments)]
pub(in crate::tui::run) async fn apply_self_review_command(
    input: &str,
    app: &mut AppState,
    agent: Arc<Mutex<Agent>>,
    session_store: &Arc<Mutex<SessionStore>>,
    registry: &Arc<crate::provider::ProviderRegistry>,
    model_catalog: &Arc<crate::model_catalog::ModelCatalog>,
    storage: &crate::storage::Storage,
    sync_agent: bool,
) {
    use crate::commands::SelfReviewCommandRequest;

    match crate::commands::parse_self_review_command(input) {
        Ok(SelfReviewCommandRequest::Set(mode)) => {
            if sync_agent {
                agent.lock().await.set_self_review_mode(mode);
            }
            app.self_review_mode = mode;
            push_transient_notice(
                app,
                &crate::commands::self_review_set_message(mode, app.approval_level),
            );
        }
        Ok(SelfReviewCommandRequest::Status) => {
            let mode = app.self_review_mode;
            push_transient_notice(
                app,
                &crate::commands::self_review_status_message(mode, app.approval_level),
            );
        }
        Ok(SelfReviewCommandRequest::OpenModelPicker) => {
            let entries = {
                let session = session_store.lock().await;
                app.sync_cached_model_choices(&session, registry, Some(model_catalog));
                app.cached_model_choices.clone()
            };
            if entries.is_empty() {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    "Authorize a provider before setting the self-review model. Try `/authorize <provider>`.",
                );
                return;
            }
            app.pending_self_review_model = true;
            app.reduce(AppAction::OpenModal(ModalKind::Picker(
                crate::tui::event::PickerModal::ModelPicker { entries },
            )));
        }
        Ok(SelfReviewCommandRequest::ClearModel) => {
            let settings = crate::subagent::BuiltinSubagentSettings::default();
            if let Err(err) = storage
                .upsert_builtin_subagent_settings(
                    crate::subagent::BuiltinSubagentId::SelfReview,
                    &settings,
                )
                .await
            {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to clear self-review model: {err:#}"),
                );
                return;
            }
            app.builtin_subagents
                .upsert(crate::subagent::BuiltinSubagentId::SelfReview, settings);
            push_transient_notice(
                app,
                "Self-review model cleared — the reviewer uses the parent model.",
            );
        }
        Err(message) => push_command_message(app, CommandOutputKind::Error, &message),
    }
}

pub(in crate::tui::run) async fn sync_agent_self_review_mode(
    mode: crate::self_review::SelfReviewMode,
    agent: &Arc<Mutex<Agent>>,
) {
    agent.lock().await.set_self_review_mode(mode);
}

pub(in crate::tui::run) async fn apply_smol_command(
    input: &str,
    app: &mut AppState,
    agent: Arc<Mutex<Agent>>,
    storage: &crate::storage::Storage,
    sync_agent: bool,
) {
    let request = match crate::commands::parse_smol_command(input) {
        Ok(request) => request,
        Err(message) => {
            push_command_message(app, CommandOutputKind::Error, &message);
            return;
        }
    };
    // `sync_agent: false` — the caller is mid-run and the running turn holds
    // the agent lock, so nothing here may `agent.lock().await` inline. The
    // set still applies for real: the preference is persisted and mirrored
    // into app state now, and a background task pushes it onto the agent the
    // moment the run releases the lock (the tokio mutex is FIFO, so a later
    // idle `/smol` can't be overtaken by this deferred sync).
    if !sync_agent {
        let mirrored_preference = if app.smol_mode {
            crate::smol::SmolPreference::On
        } else {
            crate::smol::SmolPreference::Off
        };
        // Toggle resolution only needs the effective on/off state, which app
        // state mirrors; the context-window tokens are irrelevant here.
        let mirror = crate::smol::SmolProfile::resolve(mirrored_preference, 0);
        match request.target_preference(mirror) {
            Some(preference) => {
                if let Err(error) = storage.set_smol_preference(preference).await {
                    push_command_message(
                        app,
                        CommandOutputKind::Error,
                        &format!("Failed to save SMOL preference: {error:#}"),
                    );
                    return;
                }
                app.reduce(AppAction::SetSmolMode(preference.is_effective()));
                if preference.is_effective() {
                    app.reduce(AppAction::SetPureMode(false));
                }
                let agent = agent.clone();
                tokio::spawn(async move {
                    agent.lock().await.set_smol_preference(preference);
                });
                push_transient_notice(
                    app,
                    &format!(
                        "SMOL preference set to {}; the running agent picks it up when the current run finishes.",
                        preference.as_str()
                    ),
                );
            }
            None => push_transient_notice(
                app,
                if app.smol_mode {
                    "SMOL effective profile: on. Preference details are available while idle."
                } else {
                    "SMOL effective profile: off. Preference details are available while idle."
                },
            ),
        }
        return;
    }
    let current = agent.lock().await.smol_profile();
    match request.target_preference(current) {
        Some(preference) => {
            if let Err(error) = storage.set_smol_preference(preference).await {
                push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("Failed to save SMOL preference: {error:#}"),
                );
                return;
            }
            let (profile, context_report) = {
                let mut agent = agent.lock().await;
                agent.set_smol_preference(preference);
                (agent.smol_profile(), agent.context_report())
            };
            app.latest_context_report = Some(context_report);
            app.reduce(AppAction::SetSmolMode(profile.enabled));
            if profile.enabled {
                app.reduce(AppAction::SetPureMode(false));
            }
            push_transient_notice(app, &crate::commands::smol_set_message(profile));
        }
        None => {
            let profile = agent.lock().await.smol_profile();
            push_transient_notice(app, &crate::commands::smol_status_message(profile));
        }
    }
}

pub(in crate::tui::run) async fn apply_pure_command(
    input: &str,
    app: &mut AppState,
    agent: std::sync::Arc<tokio::sync::Mutex<crate::agent::Agent>>,
    sync_agent: bool,
) {
    let request = match crate::commands::parse_pure_command(input) {
        Ok(request) => request,
        Err(message) => {
            push_command_message(app, CommandOutputKind::Error, &message);
            return;
        }
    };
    let is_pure = app.pure_mode;
    let target = match request {
        crate::commands::PureCommandRequest::Toggle => {
            if is_pure {
                crate::commands::PureTarget::Off
            } else {
                crate::commands::PureTarget::On
            }
        }
        crate::commands::PureCommandRequest::Set(target) => target,
        crate::commands::PureCommandRequest::Status => {
            push_transient_notice(app, &crate::commands::pure_status_message(is_pure));
            return;
        }
    };
    let enabled = matches!(target, crate::commands::PureTarget::On);
    // Pure mode is intentionally ephemeral — scoped to this session, not
    // persisted across restarts (unlike smol which is a global preference).
    app.reduce(AppAction::SetPureMode(enabled));
    if enabled {
        app.reduce(AppAction::SetSmolMode(false));
    }
    if !sync_agent {
        let agent = agent.clone();
        tokio::spawn(async move {
            agent.lock().await.set_pure_mode(enabled);
        });
    } else {
        let context_report = {
            let mut agent = agent.lock().await;
            agent.set_pure_mode(enabled);
            agent.context_report()
        };
        app.latest_context_report = Some(context_report);
    }
    push_transient_notice(app, &crate::commands::pure_set_message(target));
}

pub(in crate::tui::run) async fn apply_serenity_command(
    input: &str,
    app: &mut AppState,
    storage: &crate::storage::Storage,
) {
    match crate::commands::serenity::parse_serenity_command(input) {
        Ok(request) => match request.target_state(app.serenity_mode) {
            Some(on) => {
                app.reduce(AppAction::SetSerenityMode(on));
                // Global preference like the theme: the toggle survives
                // restarts. `status` requests never reach this branch.
                if let Err(error) = storage.set_serenity_mode(on).await {
                    push_command_message(
                        app,
                        CommandOutputKind::Error,
                        &format!("Failed to save serenity preference: {error:#}"),
                    );
                }
                push_transient_notice(app, crate::commands::serenity::serenity_set_message(on));
            }
            None => {
                push_transient_notice(
                    app,
                    crate::commands::serenity::serenity_status_message(app.serenity_mode),
                );
            }
        },
        Err(message) => push_command_message(app, CommandOutputKind::Error, &message),
    }
}

/// Handle `/permissions [list|remove <id>]` against the live permission
/// manager. Works in any task state; the caller echoes the command first.
pub(in crate::tui::run) async fn apply_permissions_command(
    input: &str,
    app: &mut AppState,
    permissions: &crate::permissions::PermissionManager,
    domain_permissions: &crate::permissions::PermissionManager,
    storage: &crate::storage::Storage,
    session_id: Option<crate::storage::SessionId>,
) {
    use crate::commands::permissions::PermissionsCommandRequest;
    match crate::commands::permissions::parse_permissions_command(input) {
        Ok(PermissionsCommandRequest::Manage) => {
            // Bare `/permissions` opens the interactive manager built from both
            // rule sets; `list`/`remove` below stay as the scriptable text path.
            crate::tui::runtime_actions::open_permissions_manager(
                app,
                permissions,
                domain_permissions,
                String::new(),
                0,
            );
        }
        Ok(PermissionsCommandRequest::List) => {
            let (deny_floor, defaults) = permissions.builtin_counts();
            let rules = permissions.user_rules();
            let domain_rules = domain_permissions.user_rules();
            let decisions = match session_id {
                Some(session_id) => {
                    match storage.recent_authorization_decisions(session_id, 10).await {
                        Ok(decisions) => decisions,
                        Err(error) => {
                            tracing::warn!(%error, "failed to load authorization decisions");
                            Vec::new()
                        }
                    }
                }
                None => Vec::new(),
            };
            push_command_message(
                app,
                CommandOutputKind::Status,
                &crate::commands::permissions::format_permission_rules(
                    &rules,
                    deny_floor,
                    defaults,
                    &domain_rules,
                    &decisions,
                ),
            );
        }
        // `remove` deletes by id and is kind-agnostic at the storage layer, so a
        // domain rule can be pruned through the bash manager; refresh the domain
        // manager afterward so its cache drops the deleted row too.
        Ok(PermissionsCommandRequest::Remove(id)) => match permissions.remove(id).await {
            Ok(true) => {
                if let Err(err) = domain_permissions.refresh().await {
                    tracing::warn!(%err, "failed to refresh domain rules after remove");
                }
                push_command_message(
                    app,
                    CommandOutputKind::Status,
                    &format!("Removed permission rule #{id}."),
                );
            }
            Ok(false) => push_command_message(
                app,
                CommandOutputKind::Status,
                &format!("No persisted permission rule #{id}."),
            ),
            Err(err) => push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("Failed to remove rule #{id}: {err:#}"),
            ),
        },
        Err(message) => push_command_message(app, CommandOutputKind::Error, &message),
    }
}

/// Dispatch a slash command typed while the agent is running. Routing comes
/// from the single declared table (`crate::commands::busy_behavior_for`), and
/// the executors below are shared verbatim with the busy-command modal's
/// "Open read-only view" action (`tui::runtime_actions::submit_busy_command`),
/// so a command behaves identically no matter which non-idle path catches it.
pub(in crate::tui::run) async fn handle_running_slash_command(
    input: &str,
    app: &mut AppState,
    deps: PersistenceCommandDeps<'_>,
    yolo_mode: &YoloMode,
    state: &mut PersistenceCommandState<'_>,
) -> bool {
    if slash_command_name(input).is_none() {
        return false;
    }

    match crate::commands::busy_behavior_for(input) {
        crate::commands::BusyCommandBehavior::ReadOnlyNow => {
            submit_running_command(app, input);
            apply_non_idle_read_only_command(input, app, deps, state).await;
        }
        crate::commands::BusyCommandBehavior::RunNow => {
            submit_running_command(app, input);
            apply_non_idle_immediate_command(input, app, deps, yolo_mode, state).await;
        }
        crate::commands::BusyCommandBehavior::DeferUntilIdle
        | crate::commands::BusyCommandBehavior::Block => {
            submit_running_command(app, input);
            open_busy_command_modal(app, input, crate::commands::busy_behavior_for(input));
        }
    }

    true
}

/// Serve a `ReadOnlyNow`-classified command from cached / lock-free state.
/// Shared by the running composer path and the busy-command modal path — the
/// arms here must never take the agent lock (held for a run's whole duration).
pub(in crate::tui) async fn apply_non_idle_read_only_command(
    input: &str,
    app: &mut AppState,
    deps: PersistenceCommandDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
) {
    let command_name = slash_command_name(input).unwrap_or_default();
    match command_name {
        // `/ctx` and `/episodes` reuse the last emitted report mid-run: the
        // fresh preview needs the agent lock, so only the idle dispatcher
        // regenerates it (`open_context_modal_with_preview`).
        "/ctx" => {
            if app.latest_context_report.is_some() {
                app.reduce(AppAction::OpenContextModal);
            } else {
                push_command_message(app, CommandOutputKind::Status, CTX_UNAVAILABLE_WHILE_BUSY);
            }
        }
        "/episodes" => {
            if app.latest_context_report.is_some() {
                app.reduce(AppAction::OpenEpisodesModal);
            } else {
                push_command_message(app, CommandOutputKind::Status, CTX_UNAVAILABLE_WHILE_BUSY);
            }
        }
        "/keys" => app.reduce(AppAction::OpenModal(ModalKind::Detail(
            crate::tui::event::DetailModal::Help,
        ))),
        "/help" => app.reduce(AppAction::OpenModal(ModalKind::Detail(
            crate::tui::event::DetailModal::CommandHelp,
        ))),
        "/perf" | "/cost" => open_cached_usage_report_modal(app),
        "/model" => {
            open_cached_model_picker(
                app,
                deps.session_store.clone(),
                deps.registry.clone(),
                deps.model_catalog.clone(),
            )
            .await;
        }
        "/memory" => match deps.memory.as_deref() {
            // The manager works off the file store directly — no agent lock,
            // which a running turn holds until it finishes.
            Some(memory) => {
                if let Some(modal) = crate::tui::task::memory_command_modal(input, memory) {
                    app.reduce(AppAction::OpenModal(modal));
                } else {
                    push_command_message(
                        app,
                        CommandOutputKind::Error,
                        crate::commands::memory::MEMORY_USAGE,
                    );
                }
            }
            None => push_command_message(
                app,
                CommandOutputKind::Status,
                "Memory is unavailable in this session.",
            ),
        },
        "/sandbox" => apply_sandbox_command(input, app, deps.storage).await,
        "/self-review" => {
            apply_self_review_command(
                input,
                app,
                deps.agent.clone(),
                &deps.session_store,
                &deps.registry,
                &deps.model_catalog,
                deps.storage,
                false,
            )
            .await
        }
        "/smol" => apply_smol_command(input, app, deps.agent.clone(), deps.storage, false).await,
        "/pure" => apply_pure_command(input, app, deps.agent.clone(), false).await,
        "/serenity" => apply_serenity_command(input, app, deps.storage).await,
        "/sessions" => {
            if let Some(command) = persistence_command(input)
                && let Err(err) = apply_persistence_command(command, app, deps, state).await
            {
                push_command_message(app, CommandOutputKind::Error, &format!("{err:#}"));
            }
        }
        "/tasks" => {}
        // Defense-in-depth: these are normally caught by the any-task-state
        // block in event_loop.rs before reaching here, but handle them in
        // case the dispatch order ever changes.
        "/agents" => {
            let rows = crate::tui::agent_composer::browser_rows_with_settings(
                &crate::resource::agent::snapshot(&app.custom_agents),
                &app.builtin_subagents.snapshot(),
            );
            app.reduce(AppAction::OpenModal(ModalKind::Manager(
                crate::tui::event::ManagerModal::AgentBrowser { rows, cursor: 0 },
            )));
        }
        "/skills" => {
            let rows = crate::tui::skill_manager::skill_rows(
                &crate::resource::skill::snapshot(&app.skills),
                &app.loaded_skills,
            );
            app.reduce(AppAction::OpenModal(ModalKind::Manager(
                crate::tui::event::ManagerModal::SkillManager { rows, cursor: 0 },
            )));
        }
        "/providers" => {
            // Defense-in-depth: `/providers` is normally caught by the
            // any-task-state block in event_loop.rs, which opens the real
            // provider manager with full RuntimeActionDeps.
            // DEFERRED: this path only carries PersistenceCommandDeps — the
            // manager also needs InteractionService, the runtime sender, and
            // the background-task registry, none of which are threaded here.
            // Until they are, fall back to the cached model picker and say so.
            push_command_message(
                app,
                CommandOutputKind::Status,
                "The full provider manager is unavailable on this path; showing the cached model picker instead.",
            );
            open_cached_model_picker(
                app,
                deps.session_store.clone(),
                deps.registry.clone(),
                deps.model_catalog.clone(),
            )
            .await;
        }
        _ => open_busy_command_modal(app, input, crate::commands::BusyCommandBehavior::Block),
    }
}

/// Shared non-idle message for `/ctx` and `/episodes` when no report has been
/// cached yet: explains the declared idle-only capability instead of a bare
/// apology.
const CTX_UNAVAILABLE_WHILE_BUSY: &str =
    "Context report is not available yet — the live context preview opens when the agent is idle.";

/// Apply a `RunNow`-classified command mid-run. Shared by the running composer
/// path and (transitively) the busy-command modal; every arm must stay
/// lock-free with respect to the agent mutex. Commands classified `RunNow` in
/// `crate::commands::busy_behavior_for` must have an arm here (or be handled
/// by the any-task-state block in event_loop.rs / the persistence fallback);
/// the final arm reports instead of silently dropping input, so a
/// classification/executor mismatch is user-visible rather than a no-op.
async fn apply_non_idle_immediate_command(
    input: &str,
    app: &mut AppState,
    deps: PersistenceCommandDeps<'_>,
    yolo_mode: &YoloMode,
    state: &mut PersistenceCommandState<'_>,
) {
    let command_name = slash_command_name(input).unwrap_or_default();
    match command_name {
        "/yolo" | "/autonomy" => {
            apply_autonomy_command(input, app, yolo_mode, deps.storage).await;
        }
        "/serenity" => apply_serenity_command(input, app, deps.storage).await,
        // Setting toggles mutate lock-free holders (sandbox handle, storage,
        // app mirrors), so they apply for real mid-run — answering
        // `/sandbox off` with a status modal was the audited drift.
        "/sandbox" => apply_sandbox_command(input, app, deps.storage).await,
        // `sync_agent: false`: the running turn holds the agent lock; the app
        // mirror is authoritative and every run start pushes it to the agent
        // via `sync_agent_self_review_mode`, so the set still sticks.
        "/self-review" => {
            apply_self_review_command(
                input,
                app,
                deps.agent.clone(),
                &deps.session_store,
                &deps.registry,
                &deps.model_catalog,
                deps.storage,
                false,
            )
            .await
        }
        "/smol" => apply_smol_command(input, app, deps.agent.clone(), deps.storage, false).await,
        "/pure" => apply_pure_command(input, app, deps.agent.clone(), false).await,
        // Memory writes go to the file store, never the live conversation, so
        // they are declared safe mid-run — but they need an arm here (the
        // generic command task can't start while a run holds the task slot).
        "/remember" => apply_remember_command(input, app, deps.memory.as_deref()).await,
        "/memory" => apply_memory_forget_command(input, app, deps.memory.as_deref()).await,
        "/mode" => {
            app.reduce(AppAction::OpenModal(ModalKind::Picker(
                crate::tui::event::PickerModal::ModePicker {
                    rows: Vec::new(),
                    cursor: 0,
                },
            )));
        }
        "/theme" => {
            let arg = theme_command_arg(input).unwrap_or("");
            if arg.is_empty() {
                open_theme_picker(app);
            } else {
                apply_theme_command(arg, app, deps.project_root, deps.session_store.clone()).await;
            }
        }
        "/review" => {
            app.reduce(AppAction::OpenModal(ModalKind::Picker(
                crate::tui::event::PickerModal::ReviewScopePicker { cursor: 0 },
            )));
        }
        "/authorize" => {
            push_command_message(
                app,
                CommandOutputKind::Status,
                "Use /authorize when the agent is idle to open the provider picker.",
            );
        }
        _ => match persistence_command(input) {
            Some(command) => {
                if let Err(err) = apply_persistence_command(command, app, deps, state).await {
                    push_command_message(app, CommandOutputKind::Error, &format!("{err:#}"));
                }
            }
            // Declared `RunNow` but no executor arm: report it instead of
            // silently swallowing the input (the pre-H1 `/settings` failure
            // mode). Reaching this line is a table/executor mismatch bug.
            None => push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("{command_name} is not wired to run while the agent is busy."),
            ),
        },
    }
}

/// Mid-run `/remember`: writes through the memory service directly (the file
/// store, agent-lock-free), mirroring the headless handler in
/// `commands::handlers::common`.
async fn apply_remember_command(
    input: &str,
    app: &mut AppState,
    memory: Option<&crate::memory::MemoryService>,
) {
    let request = match crate::commands::parse_remember_command(input) {
        Ok(request) => request,
        Err(message) => {
            push_command_message(app, CommandOutputKind::Error, &message);
            return;
        }
    };
    let Some(memory) = memory else {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Memory is unavailable in this session.",
        );
        return;
    };
    let entry_type = match request.tier {
        crate::memory::entry::MemoryTier::User => crate::memory::entry::MemoryEntryType::Preference,
        crate::memory::entry::MemoryTier::Project => crate::memory::entry::MemoryEntryType::Project,
    };
    match memory
        .write(
            request.tier,
            entry_type,
            None,
            &request.fact,
            &request.fact,
            None,
        )
        .await
    {
        Ok(written) => push_command_message(
            app,
            CommandOutputKind::Status,
            &format!(
                "remembered: {} ({}) — indexed next session",
                written.entry.description,
                written.entry.path.display()
            ),
        ),
        Err(err) => push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Could not save memory: {err:#}"),
        ),
    }
}

/// Mid-run `/memory forget <name>`: deletes through the memory service
/// (agent-lock-free). List/view forms are classified `ReadOnlyNow` and open
/// the manager modal instead.
async fn apply_memory_forget_command(
    input: &str,
    app: &mut AppState,
    memory: Option<&crate::memory::MemoryService>,
) {
    let request = match crate::commands::parse_memory_command(input) {
        Ok(request) => request,
        Err(message) => {
            push_command_message(app, CommandOutputKind::Error, &message);
            return;
        }
    };
    let Some(memory) = memory else {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Memory is unavailable in this session.",
        );
        return;
    };
    let crate::commands::MemoryCommandRequest::Forget { name } = request else {
        // List/view are ReadOnlyNow; getting here means the classification
        // and this executor disagree — surface it.
        push_command_message(
            app,
            CommandOutputKind::Error,
            crate::commands::memory::MEMORY_USAGE,
        );
        return;
    };
    if memory.store().get(&name).is_none() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &crate::commands::unknown_memory_entry_message(&name, &memory.store().entries()),
        );
        return;
    }
    match memory.forget(&name).await {
        Ok(path) => push_command_message(
            app,
            CommandOutputKind::Status,
            &format!("Forgot memory entry {name} ({})", path.display()),
        ),
        Err(err) => push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Could not forget {name}: {err:#}"),
        ),
    }
}

pub(in crate::tui) fn cached_usage_report_text(
    report: Option<&crate::agent::ContextReport>,
) -> String {
    let Some(report) = report else {
        return "Usage: session\nNo usage data is available yet.".to_string();
    };
    let mut lines = vec!["Usage: session".to_string()];
    lines.extend(
        crate::context_view::telemetry::CostTelemetry::from_report(report).usage_lines(report),
    );
    lines.join("\n")
}

fn open_perf_report_modal(app: &mut AppState, title: &str, text: &str) {
    app.reduce(AppAction::OpenModal(ModalKind::perf_report(title, text)));
}

pub(in crate::tui) fn open_cached_usage_report_modal(app: &mut AppState) {
    let text = cached_usage_report_text(app.latest_context_report.as_ref());
    open_perf_report_modal(app, "Usage", &text);
}

fn open_busy_command_modal(
    app: &mut AppState,
    input: &str,
    behavior: crate::commands::BusyCommandBehavior,
) {
    let rows = busy_command_rows(behavior);
    app.reduce(AppAction::OpenModal(ModalKind::Detail(
        crate::tui::event::DetailModal::BusyCommand {
            input: input.trim().to_string(),
            rows,
            cursor: 0,
        },
    )));
}

pub(in crate::tui::run) fn busy_command_rows(
    behavior: crate::commands::BusyCommandBehavior,
) -> Vec<crate::tui::event::BusyCommandRow> {
    use crate::commands::BusyCommandBehavior;
    use crate::tui::event::BusyCommandRow;

    match behavior {
        BusyCommandBehavior::ReadOnlyNow => vec![
            BusyCommandRow::open_read_only(),
            BusyCommandRow::cancel_current_run(),
            BusyCommandRow::dismiss(),
        ],
        BusyCommandBehavior::DeferUntilIdle => vec![
            BusyCommandRow::queue(),
            BusyCommandRow::cancel_current_run(),
            BusyCommandRow::dismiss(),
        ],
        BusyCommandBehavior::RunNow => vec![BusyCommandRow::dismiss()],
        BusyCommandBehavior::Block => vec![
            BusyCommandRow::cancel_current_run(),
            BusyCommandRow::dismiss(),
        ],
    }
}

fn slash_command_name(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    trimmed.split_whitespace().next()
}

pub(in crate::tui) async fn open_context_modal_with_preview(
    app: &mut AppState,
    agent: Arc<Mutex<Agent>>,
    agent_busy: bool,
    include_draft: bool,
) {
    if !agent_busy {
        let preview = context_preview_input(app, include_draft);
        let mut agent = agent.lock().await;
        agent.refresh_read_evidence_freshness().await;
        let report = agent.context_report_with_preview(preview).await;
        app.reduce(AppAction::OpenContextPreviewModal(report));
        return;
    }
    app.reduce(AppAction::OpenContextModal);
}

pub(in crate::tui) async fn apply_context_control_action(
    app: &mut AppState,
    agent: Arc<Mutex<Agent>>,
    action: ContextControlAction,
) {
    if !app.context_state.view_mode.is_ledger() {
        return;
    }
    let Some(node_id) = app.selected_context_node_id() else {
        return;
    };
    let preview = context_preview_input(app, true);
    let mut agent = agent.lock().await;
    if !agent.apply_context_control_action(&node_id, action) {
        return;
    }
    agent.refresh_read_evidence_freshness().await;
    let report = agent.context_report_with_preview(preview).await;
    app.reduce(AppAction::RefreshContextModal(report));
}

fn context_preview_input(app: &AppState, include_draft: bool) -> ContextPreviewInput {
    let composer_draft = include_draft
        .then(|| app.input().trim().to_string())
        .filter(|text| !text.is_empty() && !text.starts_with('/'));
    let target_mode = context_preview_target_mode(app);
    let queued_inputs = app
        .queued_inputs
        .iter()
        .map(|queued| ContextPreviewUserInput {
            id: Some(queued.id),
            text: queued.text.clone(),
            mode: app.active_mode(),
        })
        .collect();
    ContextPreviewInput {
        composer_draft,
        queued_inputs,
        plan_markdown: (!app.plan.is_empty()).then(|| app.plan.to_markdown()),
        todo_markdown: (!app.todo.is_empty()).then(|| todo_markdown(&app.todo)),
        target_mode,
    }
}

/// Map a [`View`] to the [`AgentMode`] it defaults to. Retained for tests;
/// the runtime source of truth is `app.active_mode()` (synced by the reducer's
/// `default_mode_for_view`).
#[cfg(test)]
pub(in crate::tui::run) fn agent_mode_for_view(view: View) -> AgentMode {
    match view {
        View::Agent => AgentMode::Coding,
        View::Plan => AgentMode::Planning,
    }
}

/// Sync the agent to `app.active_mode()` and regenerate the persisted context
/// report so the tool-schema count (and persona) reflect the current mode.
/// Skipped while the agent is busy to avoid contending for the agent lock;
/// the running task emits a fresh report on completion.
pub(in crate::tui::run) async fn sync_agent_mode_and_refresh(
    app: &mut AppState,
    agent: Arc<Mutex<Agent>>,
) {
    if app.task_state.is_busy() {
        return;
    }
    let mut guard = agent.lock().await;
    // Only reapply on a real change: `set_persona` on a same-name custom
    // persona would re-inject a "switched from X to X" transition message
    // (built-in same-mode is already a no-op in `set_mode`). This runs both on
    // persona-change frames and unconditionally after every run finishes, so
    // it must be idempotent.
    if guard.active_persona() != &app.active_persona {
        guard.set_persona(app.active_persona.clone());
    }
    app.refresh_agent_mirrors(&guard);
    let report = guard.context_report();
    drop(guard);
    app.reduce(AppAction::RefreshContextModal(report));
}

/// The persona `/ctx` should preview: the active persona's built-in mode (a
/// custom persona maps to a neutral built-in for the preview).
fn context_preview_target_mode(app: &AppState) -> Option<AgentMode> {
    Some(app.active_mode())
}

fn todo_markdown(todos: &[crate::todo::TodoItem]) -> String {
    let mut markdown = String::from("Todo list:\n");
    for todo in todos {
        markdown.push_str(&format!("- [{}] {}\n", todo.status.label(), todo.content));
    }
    markdown
}

fn submit_running_command(app: &mut AppState, input: &str) {
    app.reduce(AppAction::ScrollBottom);
    app.reduce(AppAction::SubmitCommandInput(input.trim().to_string()));
}

pub(in crate::tui::run) fn enqueue_running_follow_up(
    app: &mut AppState,
    tasks: &TaskController,
    delivery: FollowUpDelivery,
    registry: &crate::provider::ProviderRegistry,
    catalog: Option<&crate::model_catalog::ModelCatalog>,
) -> bool {
    // Snapshot the composer: `display` is what the transcript shows and what the
    // slash-command guards test; the retained content rebuilds the expanded
    // model payload when the pending foreground turn starts.
    let submission = app.composer.submission();
    let display = submission.display_text.trim().to_string();

    if display.is_empty() {
        return false;
    }
    if display.starts_with('/') {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Slash commands cannot be sent as a busy-state follow-up.",
        );
        return false;
    }
    // Vision gate against the running model, so a queued image can't reach a
    // model that can't see it.
    if submission.input.has_images()
        && !crate::model_resolution::model_supports_vision(
            registry,
            catalog,
            &app.provider,
            &app.model,
        )
    {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "This model can't see images — remove the [Image N] chip or switch models.",
        );
        return false;
    }

    let content = app.composer.content();
    let id = app.next_queued_input_id();
    let mode = tasks
        .active_agent_mode()
        .unwrap_or_else(|| app.active_mode());
    let action = match delivery {
        FollowUpDelivery::Steer => AppAction::SteerInput {
            id,
            text: display,
            content,
            mode,
        },
        FollowUpDelivery::Queue => AppAction::QueueNextInput {
            id,
            text: display,
            content,
            mode,
        },
    };
    app.reduce(action);
    true
}

pub(in crate::tui) fn push_command_message(
    app: &mut AppState,
    kind: CommandOutputKind,
    text: &str,
) {
    app.reduce(AppAction::CommandOutput {
        kind,
        text: text.to_string(),
    });
}

pub(in crate::tui) fn push_transient_notice(app: &mut AppState, text: &str) {
    app.set_session_toast(text);
}

pub(in crate::tui) fn non_empty_trimmed(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

pub(in crate::tui) async fn open_cached_model_picker(
    app: &mut AppState,
    session_store: Arc<Mutex<SessionStore>>,
    registry: Arc<ProviderRegistry>,
    model_catalog: Arc<ModelCatalog>,
) {
    let entries = {
        let session = session_store.lock().await;
        cached_model_picker_entries(&registry, &session, Some(&model_catalog))
    };
    if entries.is_empty() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "Authorize a provider before running /model. Try `/authorize <provider>`.",
        );
        return;
    }

    app.reduce(AppAction::OpenModal(ModalKind::Picker(
        crate::tui::event::PickerModal::ModelPicker { entries },
    )));
}

fn cached_model_picker_entries(
    registry: &ProviderRegistry,
    session: &SessionStore,
    catalog: Option<&ModelCatalog>,
) -> Vec<ModelOption> {
    registry
        .all()
        .iter()
        .flat_map(|factory| {
            let metadata = factory.metadata();
            let provider_id = metadata.id.as_ref();
            let Some(provider_session) = session.providers.get(provider_id) else {
                return Vec::new();
            };
            if !factory.is_authorized(provider_session) {
                return Vec::new();
            }
            let provider_label = metadata.display_name.to_string();
            crate::model_catalog::available_model_ids_for_provider(
                catalog,
                provider_id,
                metadata,
                &provider_session.model,
            )
            .into_iter()
            .map(|model| {
                ModelOption::from_provider_model(
                    catalog,
                    provider_id,
                    &provider_label,
                    session,
                    metadata,
                    model,
                )
            })
            .collect::<Vec<_>>()
        })
        .collect()
}

/// Build the focused workflow prompt used by `/commit`.
pub(in crate::tui) fn commit_workflow_prompt() -> String {
    "Create a git commit for the current repository changes.\n\n\
Workflow:\n\
1. Inspect the worktree using the `git` read tool: `status`, staged `diff`, and unstaged `diff`. For untracked files shown by status, read them as needed to understand what would be committed.\n\
2. If the staged diff has changes, commit only the staged index. Do not run `git add` or stage extra files; preserve the user's partial-commit workflow.\n\
3. If nothing is staged but worktree changes or untracked files exist, run `git add -A` through `bash`, then inspect the staged diff again with the `git` tool.\n\
4. If the repository is clean, stop and say there is nothing to commit.\n\
5. Generate a Conventional Commit message from the committed changes: `type(scope): summary`, <=70 chars; add a body only when useful.\n\
6. Create the commit with `git commit` through `bash` only. Do not edit files, change branches, reset/revert, or amend existing commits.\n\
7. After committing, report the commit hash and message. If git refuses to commit, stop and show the error."
        .to_string()
}

/// Build the focused workflow prompt used by `/init`.
pub(in crate::tui) fn init_workflow_prompt() -> String {
    "Create concise, project-aware root `AGENTS.md` guidance through the normal read/write tools and permission flow. Do not use direct filesystem APIs or invent unverified commands.\n\n\
Workflow:\n\
1. Inspect a bounded set of authoritative project sources: relevant manifests, README or contributor documentation, CI and task-runner configuration, and the shallow source layout. Read further only when one of those sources points to a specific project rule.\n\
2. Check whether root `AGENTS.md` exists. If it exists, use the `question` tool with exactly two choices: leave it unchanged, or review and improve it while preserving useful verified project rules. If the user chooses leave it unchanged, make no mutation and report that choice.\n\
3. When creating or improving the file, include only verified, actionable facts. Keep it compact and adaptive: include a one- or two-line purpose and primary stack, exact everyday setup/run/build/test/format/lint commands only when verified, a selective map of key entry points or directories for common changes, and material project-specific conventions or hazards such as test placement or generated-file boundaries. Omit sections that have no evidence.\n\
4. Do not add generic coding etiquette, TODO placeholders, exhaustive dependency or file lists, volatile machine state, secrets, prose already obvious from standard manifests, a redundant `# AGENTS.md` title, or copied detailed documentation. Prefer a pointer to detailed docs when useful.\n\
5. Write only root `AGENTS.md` with the normal file tools after reading its current contents when it exists. Preserve useful verified rules during an improvement; do not overwrite it when the user chose leave unchanged.\n\
6. Finish by summarizing the evidence used and the concise guidance written. State that newly written steering is loaded on the next launch."
        .to_string()
}

/// Build the focused workflow prompt used by `/pr`.
pub(in crate::tui) fn pr_workflow_prompt() -> String {
    "Create or update a GitHub pull request for the current branch.\n\n\
Workflow:\n\
1. Inspect repository state using supported `git` read-tool operations: `status` for the current branch and worktree state, then `log` and `diff` against the likely base. Try common base refs such as `origin/main`, `origin/master`, `main`, and `master`; if none work, stop and say the base branch could not be determined.\n\
2. Require committed changes on a non-default feature branch. If there are uncommitted changes, stop and ask the user to commit or stash them first. If there are no commits ahead of the base, stop.\n\
3. Check GitHub CLI availability and auth with `gh` through `bash` (`gh --version`, then an auth/status command). If `gh` is missing or unauthenticated, stop with the exact blocker.\n\
4. Check for an existing PR for the current branch with `gh pr view` through `bash`. If one exists, reuse it; do not create a duplicate.\n\
5. If no PR exists, generate a concise title and body from the branch diff/log, then create the PR with `gh pr create` through `bash`. Do not push, force-push, change branches, reset/revert, amend, or edit files.\n\
6. After creating or finding the PR, read review/comments with `gh pr view --comments` through `bash` and summarize any actionable feedback.\n\
7. Report the PR URL, title, and whether it was created or already existed. If a command fails, stop and show the error."
        .to_string()
}

async fn start_focused_coding_workflow(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    prompt: String,
    completion_contract: crate::agent::TaskCompletionContract,
    status: &str,
) -> bool {
    if app.task_state.is_busy() {
        return false;
    }

    app.reduce(AppAction::SetView(View::Agent));
    app.reduce(AppAction::ScrollBottom);
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(std::time::Instant::now());
    app.reduce(AppAction::Agent(UiEvent::Thinking(status.to_string())));
    sync_agent_self_review_mode(app.self_review_mode, &agent).await;
    if let Err(err) = tasks.start_focused_coding_run(agent, prompt, completion_contract, sink) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
        return false;
    }
    true
}

/// Starts a focused coding-agent workflow that inspects the worktree and creates
/// one Conventional Commit when there are pending changes.
pub(in crate::tui) async fn commit_changes(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
) -> bool {
    start_focused_coding_workflow(
        app,
        tasks,
        agent,
        sink,
        commit_workflow_prompt(),
        crate::agent::TaskCompletionContract::action(),
        "Committing pending changes",
    )
    .await
}

/// Starts a focused coding-agent workflow that creates or improves root project
/// steering using ordinary permissioned tools.
pub(in crate::tui) async fn initialize_agents_md(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
) -> bool {
    start_focused_coding_workflow(
        app,
        tasks,
        agent,
        sink,
        init_workflow_prompt(),
        crate::agent::TaskCompletionContract::workspace_action(),
        "Preparing project-aware AGENTS.md guidance",
    )
    .await
}

async fn run_verification_profile(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    project_root: &Path,
    kind: crate::verification::VerificationKind,
) -> bool {
    let workflow = {
        let guard = agent.lock().await;
        let profile = crate::verification::VerificationProfile::resolve(
            project_root,
            &guard.config().verification,
        );
        profile
            .workflow_prompt(kind)
            .map(|prompt| crate::verification::VerificationWorkflow {
                kind,
                checks: profile.checks(kind).to_vec(),
                prompt,
            })
    };
    let workflow = match workflow {
        Ok(workflow) => workflow,
        Err(message) => {
            push_command_message(app, CommandOutputKind::Error, &message);
            return false;
        }
    };
    if app.task_state.is_busy() {
        return false;
    }
    app.reduce(AppAction::SetView(View::Agent));
    app.reduce(AppAction::ScrollBottom);
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(std::time::Instant::now());
    app.reduce(AppAction::Agent(UiEvent::Thinking(format!(
        "Running {} verification",
        kind.label()
    ))));
    sync_agent_self_review_mode(app.self_review_mode, &agent).await;
    if let Err(err) = tasks.start_verification_run(agent, workflow, sink) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
        return false;
    }
    true
}

pub(in crate::tui) async fn run_test_profile(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    project_root: &Path,
) -> bool {
    run_verification_profile(
        app,
        tasks,
        agent,
        sink,
        project_root,
        crate::verification::VerificationKind::Test,
    )
    .await
}

pub(in crate::tui) async fn run_build_profile(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    project_root: &Path,
) -> bool {
    run_verification_profile(
        app,
        tasks,
        agent,
        sink,
        project_root,
        crate::verification::VerificationKind::Build,
    )
    .await
}

pub(in crate::tui::run) struct RetryCommandDeps<'a> {
    pub(in crate::tui::run) agent: Arc<Mutex<Agent>>,
    pub(in crate::tui::run) sink: SharedSink,
    pub(in crate::tui::run) session_store: Arc<Mutex<SessionStore>>,
    pub(in crate::tui::run) registry: Arc<ProviderRegistry>,
    pub(in crate::tui::run) model_catalog: Arc<ModelCatalog>,
    pub(in crate::tui::run) storage: &'a Storage,
    pub(in crate::tui::run) session_id: SessionId,
}

pub(in crate::tui::run) async fn retry_last_turn(
    input: &str,
    app: &mut AppState,
    tasks: &mut TaskController,
    deps: RetryCommandDeps<'_>,
) -> bool {
    let request = match crate::commands::retry::parse_retry_command(input) {
        Ok(request) => request,
        Err(message) => {
            push_command_message(app, CommandOutputKind::Error, &message);
            return false;
        }
    };
    if app.task_state.is_busy() {
        return false;
    }
    if !deps.agent.lock().await.can_retry_last_turn() {
        push_command_message(
            app,
            CommandOutputKind::Error,
            "No user turn is available to retry in this conversation.",
        );
        return false;
    }

    if let Some(shortcut) = request.model_shortcut {
        let switched = switch_retry_model(shortcut, &deps).await;
        let selection = match switched {
            Ok(selection) => selection,
            Err(message) => {
                push_command_message(app, CommandOutputKind::Error, &message);
                return false;
            }
        };
        app.provider = selection.provider.clone();
        app.model = selection.model.clone();
        app.reasoning = selection.reasoning;
        app.refresh_agent_mirrors(&*deps.agent.lock().await);
        push_transient_notice(
            app,
            &format!(
                "Retry model set to {} ({}, reasoning: {}).",
                selection.model,
                shortcut.as_command(),
                selection.reasoning
            ),
        );
    }

    app.reduce(AppAction::ScrollBottom);
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(std::time::Instant::now());
    app.reduce(AppAction::Agent(UiEvent::Thinking(
        "Retrying latest turn".to_string(),
    )));
    sync_agent_self_review_mode(app.self_review_mode, &deps.agent).await;
    let persona = app.active_persona.clone();
    if let Err(err) = tasks.start_agent_retry(deps.agent, deps.sink, persona) {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
        return false;
    }
    true
}

async fn switch_retry_model(
    shortcut: crate::model_role::ModelShortcutKey,
    deps: &RetryCommandDeps<'_>,
) -> Result<ProviderRunSelection, String> {
    let (session_snapshot, selection, provider, context_window, prompt_estimator) = {
        let mut session = deps.session_store.lock().await;
        let original_session = session.clone();
        let Some(shortcut_selection) = crate::model_role::resolve_model_shortcut(
            &deps.registry,
            &session,
            Some(&deps.model_catalog),
            shortcut,
        ) else {
            return Err(format!(
                "No model shortcut is assigned to {}. Open /model, focus the Reasoning pane, and press {} on a model to assign it.",
                shortcut.as_command(),
                shortcut.as_char()
            ));
        };
        let resolved = crate::commands::ResolvedModelSelection::from(&shortcut_selection);
        let reasoning = crate::commands::apply_model_selection(
            &deps.registry,
            &mut session,
            Some(&deps.model_catalog),
            &resolved,
            shortcut_selection.reasoning,
        );
        let selection = ProviderRunSelection {
            provider: session.provider_label().to_string(),
            model: session.current_session().model.clone(),
            reasoning,
        };
        if let Err(err) = session.save_async().await {
            *session = original_session;
            return Err(format!("Failed to save retry model: {err:#}"));
        }
        (
            session.clone(),
            selection,
            build_provider(&deps.registry, &session, Some(&deps.model_catalog)),
            context_window_for_current_model_with_catalog(
                &deps.registry,
                &session,
                Some(&deps.model_catalog),
            ) as usize,
            prompt_estimator_for_current_model_with_catalog(
                &deps.registry,
                &session,
                Some(&deps.model_catalog),
            ),
        )
    };
    {
        let mut agent = deps.agent.lock().await;
        agent.set_provider(
            provider,
            context_window,
            prompt_estimator,
            crate::model_resolution::active_model_identity(&session_snapshot),
        );
        agent.set_project_info_provider(crate::model_resolution::project_info_provider_state(
            &deps.registry,
            &session_snapshot,
        ));
    }
    if let Err(err) = deps
        .storage
        .set_session_run_selection(
            deps.session_id,
            &selection.provider,
            &selection.model,
            selection.reasoning,
        )
        .await
    {
        tracing::warn!(
            error = %format!("{err:#}"),
            "failed to persist retry model on active session"
        );
    }
    Ok(selection)
}

/// Starts a focused coding-agent workflow that creates or reuses a pull request
/// for the current branch through the existing `bash`/`gh` path.
pub(in crate::tui) async fn create_pull_request(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
) -> bool {
    start_focused_coding_workflow(
        app,
        tasks,
        agent,
        sink,
        pr_workflow_prompt(),
        crate::agent::TaskCompletionContract::action(),
        "Preparing pull request",
    )
    .await
}

pub(in crate::tui) async fn implement_plan(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    todo_store: SharedTodoStore,
    plan_store: SharedPlanStore,
) -> bool {
    implement_plan_with_context(
        app,
        tasks,
        agent,
        sink,
        todo_store,
        plan_store,
        crate::agent::PlanContextMode::Clean,
    )
    .await
}

pub(in crate::tui) async fn open_start_plan_choice(
    app: &mut AppState,
    plan_store: &SharedPlanStore,
) {
    if app.task_state.is_busy() {
        return;
    }
    let shared_plan = plan_store.lock().await.clone();
    let plan = if shared_plan.is_empty() {
        &app.plan
    } else {
        app.plan = shared_plan;
        &app.plan
    };
    if plan.is_empty() {
        app.reduce(AppAction::CommandOutput {
            kind: CommandOutputKind::Status,
            text: "The plan canvas is empty — press Shift+Tab to switch to the plan agent and draft it first, then type /start."
                .to_string(),
        });
        return;
    }
    if plan.is_phased() && plan.next_phase_with_pending(None).is_none() {
        app.reduce(AppAction::CommandOutput {
            kind: CommandOutputKind::Status,
            text: "Every phase is already complete — nothing to implement.".to_string(),
        });
        return;
    }
    app.reduce(AppAction::OpenModal(ModalKind::Picker(
        crate::tui::event::PickerModal::StartPlanChoice { cursor: 0 },
    )));
}

pub(in crate::tui) async fn implement_plan_with_context(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
    todo_store: SharedTodoStore,
    plan_store: SharedPlanStore,
    context_mode: crate::agent::PlanContextMode,
) -> bool {
    if app.task_state.is_busy() {
        return false;
    }

    let shared_plan = plan_store.lock().await.clone();
    let plan = if shared_plan.is_empty() {
        app.plan.clone()
    } else {
        app.plan = shared_plan.clone();
        shared_plan
    };

    if plan.is_empty() {
        app.reduce(AppAction::CommandOutput {
            kind: CommandOutputKind::Status,
            text: "The plan canvas is empty — press Shift+Tab to switch to the plan agent and draft it first, then type /start."
                .to_string(),
        });
        return false;
    }

    // Phased plans run one phase at a time and auto-advance; flat plans hand
    // off the whole checklist. Start with the first phase that still has
    // pending tasks and seed only its todos.
    let phase = if plan.is_phased() {
        match plan.next_phase_with_pending(None) {
            Some(index) => Some(index),
            None => {
                app.reduce(AppAction::CommandOutput {
                    kind: CommandOutputKind::Status,
                    text: "Every phase is already complete — nothing to implement.".to_string(),
                });
                return false;
            }
        }
    } else {
        None
    };

    // The task controller takes ownership of its plan copy so it can run
    // the seeding step inside the spawn without holding a borrow on `app`.
    let todos = match phase {
        Some(index) => plan.phase_todo_items(index),
        None => plan.tasks_as_todo_items(),
    };

    {
        let mut store = todo_store.lock().await;
        if todos.is_empty() {
            store.clear();
        } else {
            store.set_todos(todos.clone());
        }
    }
    app.todo = todos;
    app.plan_execution = phase.map(|phase_index| crate::tui::app::PlanExecution { phase_index });
    // A fresh phased run follows its own live phase; drop any resting phase left
    // over from a previous run so the card doesn't briefly show the old one.
    app.resting_todo_phase = None;
    app.phase_advance = None;

    app.reduce(AppAction::SetView(View::Agent));
    if !plan.title.trim().is_empty() {
        app.current_session_summary = plan.title.clone();
    }
    app.reduce(AppAction::ScrollBottom);
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(std::time::Instant::now());
    let thinking = match phase.and_then(|index| plan.phases.get(index)) {
        Some(plan_phase) => format!("Implementing {}", plan_phase.name),
        None => "Implementing the plan".to_string(),
    };
    app.reduce(AppAction::Agent(UiEvent::Thinking(thinking)));
    sync_agent_self_review_mode(app.self_review_mode, &agent).await;
    if let Err(err) =
        tasks.start_implement_plan_with_context(agent, plan, sink, phase, context_mode)
    {
        app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
        app.plan_execution = None;
        return false;
    }
    true
}

/// Hands the pending changes to the coding agent for review: switches to the
/// agent view, pre-seeds the conversation with the `git` diff for `scope` and
/// a review instruction, and kicks off a fresh run. With no changes in the
/// chosen scope the agent loop is skipped and a status line explains why.
pub(in crate::tui) async fn review_changes(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    scope: crate::agent::ReviewScope,
    sink: SharedSink,
) -> bool {
    review_changes_with_workflow(
        app,
        tasks,
        agent,
        scope,
        sink,
        ReviewCommandWorkflow::General,
    )
    .await
}

/// Run the curated security reviewer over the uncommitted diff. It shares the
/// normal review persona's enforced read-only registry.
pub(in crate::tui) async fn security_review_changes(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    sink: SharedSink,
) -> bool {
    review_changes_with_workflow(
        app,
        tasks,
        agent,
        crate::agent::ReviewScope::Uncommitted,
        sink,
        ReviewCommandWorkflow::Security,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewCommandWorkflow {
    General,
    Security,
}

async fn review_changes_with_workflow(
    app: &mut AppState,
    tasks: &mut TaskController,
    agent: Arc<Mutex<Agent>>,
    scope: crate::agent::ReviewScope,
    sink: SharedSink,
    workflow: ReviewCommandWorkflow,
) -> bool {
    if app.task_state.is_busy() {
        return false;
    }

    app.reduce(AppAction::SetView(View::Agent));
    app.reduce(AppAction::ScrollBottom);
    app.reduce(AppAction::SetTaskState(TaskState::Running));
    app.mark_run_started(std::time::Instant::now());
    let activity = match workflow {
        ReviewCommandWorkflow::General => format!("Reviewing: {}", scope.label()),
        ReviewCommandWorkflow::Security => {
            format!("Security review: {}", scope.label())
        }
    };
    app.reduce(AppAction::Agent(UiEvent::Thinking(activity)));
    sync_agent_self_review_mode(app.self_review_mode, &agent).await;
    let started = match workflow {
        ReviewCommandWorkflow::General => tasks.start_review(agent, scope, sink).await,
        ReviewCommandWorkflow::Security => tasks.start_security_review(agent, scope, sink).await,
    };
    match started {
        Ok(true) => true,
        Ok(false) => {
            app.reduce(AppAction::SetTaskState(TaskState::Idle));
            push_command_message(
                app,
                CommandOutputKind::Status,
                &format!("No changes to review for {}.", scope.label()),
            );
            false
        }
        Err(err) => {
            app.reduce(AppAction::Runtime(RuntimeEvent::TaskPanicked(err)));
            false
        }
    }
}

pub(in crate::tui) async fn mark_started_saved_plan(
    app: &mut AppState,
    storage: &Storage,
    current_session_id: SessionId,
) {
    let Some(plan_id) = app.active_saved_plan_session_id else {
        return;
    };
    match storage.mark_plan_started(plan_id, current_session_id).await {
        Ok(true) => {}
        Ok(false) => {
            push_command_message(
                app,
                CommandOutputKind::Error,
                &format!(
                    "Saved plan #{plan_id} is no longer in the library; execution was not linked."
                ),
            );
        }
        Err(err) => {
            push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("Failed to link saved plan #{plan_id}: {err:#}"),
            );
        }
    }
}

pub(in crate::tui) fn switch_model_selection(
    selection: ModelSelection,
    app: &mut AppState,
    agent: Arc<Mutex<Agent>>,
    session_store: Arc<Mutex<SessionStore>>,
    registry: Arc<ProviderRegistry>,
    model_catalog: Arc<ModelCatalog>,
    sender: mpsc::UnboundedSender<RuntimeEvent>,
) {
    if app.task_state.is_busy() {
        return;
    }
    app.reduce(AppAction::SetTaskState(TaskState::Command));
    let generation = app.command_generation();
    // Captured before the spawn: the picker choice is recorded against the
    // mode that was active when the user made it.
    let active_mode = app.active_persona.builtin();
    let ModelSelection {
        provider_id,
        connection_id: selected_connection_id,
        model_id: selected_model_id,
        model,
        selection_input,
        reasoning,
    } = selection;
    tracing::info!(
        provider = %provider_id,
        connection = %selected_connection_id,
        model = %model,
        model_id = selected_model_id.as_deref().unwrap_or(""),
        reasoning = %reasoning,
        "switching model"
    );
    tokio::spawn(async move {
        let result: anyhow::Result<(
            ProviderRunSelection,
            Vec<String>,
            crate::agent::ContextReport,
        )> =
            async {
                let (
                    session_snapshot,
                    selection,
                    messages,
                    provider,
                    context_window,
                    prompt_estimator,
                ) = {
                    let mut session = session_store.lock().await;
                    let resolved_selection = selection_input.as_deref().and_then(|input| {
                        crate::commands::resolve_model_selection(
                            &registry,
                            &session,
                            Some(&model_catalog),
                            input,
                        )
                    });
                    let model_selection = if let Some(resolved) = resolved_selection {
                        resolved
                    } else {
                        let connection_id = selected_connection_id
                            .parse::<crate::model_catalog::ConnectionId>()
                            .ok();
                        let model_id = selected_model_id
                            .as_deref()
                            .map(str::parse::<crate::model_catalog::ModelId>)
                            .transpose()
                            .context("Invalid selected model id")?;
                        crate::commands::ResolvedModelSelection {
                            provider_id,
                            connection_id,
                            model_id,
                            model,
                        }
                    };
                    let display_name = registry
                        .lookup(&model_selection.provider_id)
                        .ok_or_else(|| {
                            anyhow::anyhow!("Unknown provider '{}'", model_selection.provider_id)
                        })?
                        .metadata()
                        .display_name
                        .to_string();
                    let old_context_window =
                        crate::model_resolution::context_window_for_current_model_with_catalog(
                            &registry,
                            &session,
                            Some(&model_catalog),
                        ) as usize;
                    let previous_selection = session.current_model_selection_input();
                    let normalized_reasoning = crate::commands::apply_model_selection(
                        &registry,
                        &mut session,
                        Some(&model_catalog),
                        &model_selection,
                        reasoning,
                    );
                    crate::commands::record_active_mode_model(
                        &mut session,
                        active_mode,
                        previous_selection,
                    );
                    let mut messages = Vec::new();
                    if let Some(fallback) = (normalized_reasoning != reasoning).then(|| {
                        format!(
                            "Reasoning {} is not supported by {} / {}; using default.",
                            reasoning, display_name, model_selection.model
                        )
                    }) {
                        messages.push(fallback);
                    }
                    let new_context_window =
                        crate::model_resolution::context_window_for_current_model_with_catalog(
                            &registry,
                            &session,
                            Some(&model_catalog),
                        ) as usize;
                    if let Some(warning) = crate::commands::context_window_downgrade_warning(
                        old_context_window,
                        new_context_window,
                    ) {
                        messages.push(warning);
                    }
                    let selection = ProviderRunSelection {
                        provider: session.provider_label().to_string(),
                        model: session.current_session().model.clone(),
                        reasoning: normalized_reasoning,
                    };
                    (
                        session.clone(),
                        selection,
                        messages,
                        build_provider(&registry, &session, Some(&model_catalog)),
                        context_window_for_current_model_with_catalog(
                            &registry,
                            &session,
                            Some(&model_catalog),
                        ) as usize,
                        prompt_estimator_for_current_model_with_catalog(
                            &registry,
                            &session,
                            Some(&model_catalog),
                        ),
                    )
                };
                session_snapshot.save_async().await?;
                let mut agent_guard = agent.lock().await;
                agent_guard.set_provider(
                    provider,
                    context_window,
                    prompt_estimator,
                    crate::model_resolution::active_model_identity(&session_snapshot),
                );
                agent_guard.set_project_info_provider(
                    crate::model_resolution::project_info_provider_state(
                        &registry,
                        &session_snapshot,
                    ),
                );
                let context_report = agent_guard.context_report();
                Ok((selection, messages, context_report))
            }
            .await;

        let event = match result {
            Ok((provider, status_messages, context_report)) => {
                let mut messages = Vec::new();
                for text in status_messages {
                    messages.push(CommandOutputEvent {
                        kind: CommandOutputKind::Status,
                        text,
                    });
                }
                RuntimeEvent::CommandFinished(Box::new(CommandOutcomeEvent::Applied {
                    generation: Some(generation),
                    clear_transcript: false,
                    messages,
                    provider: Some(Box::new(provider)),
                    context_report: Some(Box::new(context_report)),
                    quit: false,
                    open_modal: None,
                }))
            }
            Err(err) => {
                tracing::warn!(error = %err, "model switch failed");
                RuntimeEvent::CommandFinished(Box::new(CommandOutcomeEvent::Applied {
                    generation: Some(generation),
                    clear_transcript: false,
                    messages: vec![CommandOutputEvent {
                        kind: CommandOutputKind::Error,
                        text: format!("Failed to save model selection: {err:#}"),
                    }],
                    provider: None,
                    context_report: None,
                    quit: false,
                    open_modal: None,
                }))
            }
        };
        let _ = sender.send(event);
    });
}

pub(in crate::tui::run) fn summarize_input(input: &str) -> String {
    const MAX_CHARS: usize = 120;
    let text = input.trim();
    if text.chars().count() <= MAX_CHARS {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(MAX_CHARS).collect();
        format!("{}…", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::QueuedInput;

    fn app() -> AppState {
        AppState::new(
            "codex",
            "test-model".to_string(),
            "workspace".to_string(),
            None,
        )
    }

    #[test]
    fn transient_notice_does_not_create_a_transcript_row() {
        let mut app = app();

        push_transient_notice(&mut app, "SMOL preference set to on.");

        assert!(app.transcript.is_empty());
        assert_eq!(
            app.session_toast.as_ref().map(|toast| toast.text.as_str()),
            Some("SMOL preference set to on.")
        );
    }

    #[test]
    fn idle_slash_command_parses_exact_form_commands_without_args_only() {
        // Bare forms dispatch inline…
        assert_eq!(idle_slash_command("/model"), Some(IdleSlashCommand::Model));
        assert_eq!(idle_slash_command("/ctx"), Some(IdleSlashCommand::Ctx));
        assert_eq!(
            idle_slash_command("/episodes"),
            Some(IdleSlashCommand::Episodes)
        );
        assert_eq!(idle_slash_command("/start"), Some(IdleSlashCommand::Start));
        assert_eq!(idle_slash_command("/test"), Some(IdleSlashCommand::Test));
        assert_eq!(idle_slash_command("/build"), Some(IdleSlashCommand::Build));
        assert_eq!(idle_slash_command("/init"), Some(IdleSlashCommand::Init));
        assert_eq!(
            idle_slash_command("/security-review"),
            Some(IdleSlashCommand::SecurityReview)
        );
        assert_eq!(
            idle_slash_command("/retry f"),
            Some(IdleSlashCommand::Retry)
        );
        assert_eq!(
            idle_slash_command("/retry\tf"),
            Some(IdleSlashCommand::Retry)
        );
        assert_eq!(
            idle_slash_command("/settings"),
            Some(IdleSlashCommand::Settings)
        );
        // …but with arguments they must fall through to the generic command
        // task (e.g. `/model sonnet` switches models there).
        assert_eq!(idle_slash_command("/model sonnet"), None);
        assert_eq!(idle_slash_command("/ctx foo"), None);
        assert_eq!(idle_slash_command("/episodes now"), None);
        assert_eq!(idle_slash_command("/security-review now"), None);
        assert_eq!(
            idle_slash_command("/init improve"),
            Some(IdleSlashCommand::InitWithArgs)
        );
        assert_eq!(
            idle_slash_command("/initfoo"),
            Some(IdleSlashCommand::InitWithArgs),
            "a prefixed typo must show init usage rather than reach the agent"
        );
    }

    #[test]
    fn model_work_classification_covers_inline_run_commands() {
        for input in [
            "/start",
            "/continue",
            "/test",
            "/build",
            "/retry",
            "/commit",
            "/init",
            "/pr",
            "/review",
            "/security-review",
        ] {
            assert!(
                idle_slash_command(input).is_some_and(IdleSlashCommand::dispatches_model_work),
                "{input} should be gated before model work"
            );
        }
        for input in ["/ctx", "/model", "/settings", "/skills", "/theme"] {
            assert!(
                idle_slash_command(input).is_some_and(|command| !command.dispatches_model_work()),
                "{input} should remain available without a budget warning"
            );
        }
    }

    #[test]
    fn init_workflow_prompt_requires_evidence_preservation_and_next_launch_notice() {
        let prompt = init_workflow_prompt();

        for required in [
            "bounded set of authoritative project sources",
            "exactly two choices",
            "leave it unchanged",
            "preserving useful verified project rules",
            "only verified, actionable facts",
            "exact everyday setup/run/build/test/format/lint commands only when verified",
            "compact and adaptive",
            "generic coding etiquette",
            "normal file tools",
            "loaded on the next launch",
        ] {
            assert!(
                prompt.contains(required),
                "missing prompt contract: {required}"
            );
        }
        assert!(!prompt.contains("cargo build"));
        assert!(!prompt.contains("npm test"));
    }

    #[test]
    fn idle_slash_command_distinguishes_agents_browser_and_composer() {
        assert_eq!(
            idle_slash_command("/agents"),
            Some(IdleSlashCommand::AgentBrowser)
        );
        assert_eq!(
            idle_slash_command("/agents new"),
            Some(IdleSlashCommand::AgentComposer)
        );
        assert_eq!(idle_slash_command("/agents delete"), None);
    }

    #[test]
    fn idle_slash_command_routes_token_form_commands_with_any_args() {
        // Their argument grammar lives in the apply_* handlers, so any arg
        // list still routes to the handler (which reports usage errors).
        assert_eq!(
            idle_slash_command("/yolo on"),
            Some(IdleSlashCommand::Autonomy)
        );
        assert_eq!(
            idle_slash_command("/autonomy"),
            Some(IdleSlashCommand::Autonomy)
        );
        assert_eq!(
            idle_slash_command("/smol status"),
            Some(IdleSlashCommand::Smol)
        );
        assert_eq!(
            idle_slash_command("/serenity status"),
            Some(IdleSlashCommand::Serenity)
        );
        assert_eq!(
            idle_slash_command("/self-review off"),
            Some(IdleSlashCommand::SelfReview)
        );
        assert_eq!(
            idle_slash_command("/sandbox on"),
            Some(IdleSlashCommand::Sandbox)
        );
        assert_eq!(
            idle_slash_command("/wizard local"),
            Some(IdleSlashCommand::Wizard)
        );
    }

    #[test]
    fn idle_slash_command_wraps_theme_and_persistence_parsers() {
        assert_eq!(
            idle_slash_command("/theme"),
            Some(IdleSlashCommand::Theme(""))
        );
        assert_eq!(
            idle_slash_command("/theme dark"),
            Some(IdleSlashCommand::Theme("dark"))
        );
        assert_eq!(
            idle_slash_command("/resume abc"),
            Some(IdleSlashCommand::Persistence(PersistenceCommand::Resume(
                "abc"
            )))
        );
        assert_eq!(
            idle_slash_command("/sessions"),
            Some(IdleSlashCommand::Persistence(PersistenceCommand::Sessions))
        );
    }

    #[test]
    fn idle_slash_command_rejects_other_input() {
        // Unknown slash commands and plain prompts go to the generic paths.
        assert_eq!(idle_slash_command("/cost"), None);
        assert_eq!(idle_slash_command("hello world"), None);
        assert_eq!(idle_slash_command("/themepark"), None);
    }

    #[test]
    fn ctx_persona_follows_active_mode_not_queue_or_last_run() {
        let mut app = app();

        // Agent view → coding persona, even with a planning message queued
        // (the queue head's mode only drives dispatch, not the preview).
        app.view = View::Agent;
        app.active_persona = crate::agent::ActivePersona::Builtin(AgentMode::Coding);
        app.queued_inputs.push(QueuedInput {
            id: 1,
            text: "draft a plan".to_string(),
            content: crate::tui::app::ComposerContent::default(),
            mode: AgentMode::Planning,
            delivery: FollowUpDelivery::Queue,
        });
        assert_eq!(
            context_preview_target_mode(&app),
            Some(AgentMode::Coding),
            "Agent view must preview the coding persona regardless of the queue"
        );

        // Switching to Plan view flips the persona to planning.
        app.view = View::Plan;
        app.active_persona = crate::agent::ActivePersona::Builtin(AgentMode::Planning);
        assert_eq!(context_preview_target_mode(&app), Some(AgentMode::Planning));
    }
}
