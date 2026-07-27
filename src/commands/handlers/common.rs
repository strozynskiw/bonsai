use super::super::*;
use crate::agent::CompactionSummaryPolicy;
use crate::commands::CommandRuntimeContext;
use crate::commands::autonomy::{
    AutonomyCommandRequest, autonomy_set_message, autonomy_status_message, parse_autonomy_command,
};
use crate::commands::config_cmd::{
    ConfigCommandRequest, config_edit_message, format_config_diagnostics, format_config_view,
    parse_config_command,
};
use crate::commands::context_window_downgrade_warning;
use crate::commands::handlers::headless::unsupported_tui_feature;
use crate::commands::hooks_cmd::{apply_hooks_command, parse_hooks_command};
use crate::commands::mcp_cmd::parse_mcp_command;
use crate::commands::memory::{
    MemoryCommandRequest, format_memory_list, format_memory_view, parse_memory_command,
    parse_remember_command, unknown_memory_entry_message,
};
use crate::commands::metadata::{CommandSurface, command_surface};
use crate::commands::parse_command;
use crate::commands::providers::{
    ModelPickerOutcome, authorization_input_for_provider, authorize_provider_choices,
    open_model_picker, resolve_model_selection, resolve_provider, unauthorize_provider_choices,
    unknown_provider_error,
};
use crate::commands::sandbox::{sandbox_net_message, sandbox_set_message};
use crate::commands::self_review::{
    SelfReviewCommandRequest, parse_self_review_command, self_review_set_message,
    self_review_status_message,
};
use crate::commands::smol::{parse_smol_command, smol_set_message, smol_status_message};
use crate::commands::yolo::{YoloCommandRequest, parse_yolo_command};
use crate::memory::entry::{MemoryEntryType, MemoryTier};
use crate::model_catalog::ModelCatalog;
use crate::model_resolution::context_window_for_current_model_with_catalog;
use crate::model_role::{ModelShortcutKey, resolve_model_shortcut};
use crate::resource::agent::AgentRegistry;
use crate::resource::skill::SkillRegistry;
use crate::tool::ApprovalLevel;

pub(crate) async fn handle_command_with_catalog(
    input: &str,
    agent: &mut Agent,
    session_store: &mut SessionStore,
    project_root: &Path,
    _transcript: &[TranscriptItem],
    registry: Arc<ProviderRegistry>,
    runtime: CommandRuntimeContext<'_>,
) -> Result<CommandOutcome> {
    let catalog = runtime.catalog;
    let storage = runtime.storage;
    let parsed = parse_command(input);
    let parts = parsed.parts;
    let arg = parsed.arg;
    let mut outcome = CommandOutcome::default();

    match parts.first().copied() {
        Some("/help") => {
            outcome.modal = Some(CommandModalRequest::CommandHelp);
        }
        Some("/keys") => {
            outcome.modal = Some(CommandModalRequest::Keys);
        }
        Some("/ctx") => {
            agent.refresh_read_evidence_freshness().await;
            outcome.modal = Some(CommandModalRequest::ContextReport(Box::new(
                agent.context_report(),
            )));
        }
        Some("/perf") | Some("/cost") => {
            outcome.messages.push(status(agent.perf_report_text()));
        }
        Some("/episodes") => {
            agent.refresh_read_evidence_freshness().await;
            outcome.modal = Some(CommandModalRequest::Episodes(Box::new(
                agent.context_report(),
            )));
            // Headless callers ignore modal requests; keep the text report as
            // their command result. The TUI suppresses this duplicate status
            // when it opens the richer Episodes modal.
            outcome
                .messages
                .push(status(agent.episodes_command_report()));
        }
        Some("/compact") => match parse_compaction_request(&parts) {
            Ok(request) => match agent.compact_context(request).await {
                Ok(report) => {
                    outcome
                        .messages
                        .push(status(format_compaction_report(&report)));
                }
                Err(err) => outcome
                    .messages
                    .push(error(format!("Context compaction failed: {err:#}"))),
            },
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/autonomy") => match parse_autonomy_command(input) {
            Ok(AutonomyCommandRequest::Set(level)) => {
                agent.set_approval_level(level);
                outcome.messages.push(status(autonomy_set_message(level)));
            }
            Ok(AutonomyCommandRequest::Status) => {
                outcome
                    .messages
                    .push(status(autonomy_status_message(agent.approval_level())));
            }
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/self-review") => match parse_self_review_command(input) {
            Ok(SelfReviewCommandRequest::Set(mode)) => {
                agent.set_self_review_mode(mode);
                outcome.messages.push(status(self_review_set_message(
                    mode,
                    agent.approval_level(),
                )));
            }
            Ok(SelfReviewCommandRequest::Status) => {
                outcome.messages.push(status(self_review_status_message(
                    agent.self_review_mode(),
                    agent.approval_level(),
                )));
            }
            // The reviewer-model picker is a TUI modal; this shared/headless
            // path has no modal or catalog handles. Point the user at the TUI.
            Ok(
                SelfReviewCommandRequest::OpenModelPicker | SelfReviewCommandRequest::ClearModel,
            ) => {
                outcome.messages.push(error(
                    "`/self-review model` is available in the TUI.".to_string(),
                ));
            }
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/pure") => match parse_pure_command(input) {
            Ok(request) => {
                let target = match request {
                    PureCommandRequest::Toggle => {
                        if agent.pure_mode() {
                            PureTarget::Off
                        } else {
                            PureTarget::On
                        }
                    }
                    PureCommandRequest::Set(target) => target,
                    PureCommandRequest::Status => {
                        outcome
                            .messages
                            .push(status(pure_status_message(agent.pure_mode())));
                        return Ok(outcome);
                    }
                };
                if agent.set_pure_mode(matches!(target, PureTarget::On)) {
                    outcome.messages.push(status(pure_set_message(target)));
                }
            }
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/smol") => match parse_smol_command(input) {
            Ok(request) => match request.target_preference(agent.smol_profile()) {
                Some(preference) => {
                    agent.set_smol_preference(preference);
                    outcome
                        .messages
                        .push(status(smol_set_message(agent.smol_profile())));
                }
                None => {
                    outcome
                        .messages
                        .push(status(smol_status_message(agent.smol_profile())));
                }
            },
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/remember") => match parse_remember_command(input) {
            Ok(request) => match agent.memory().cloned() {
                Some(memory) => {
                    // `/remember` is deliberate: the fact is both description
                    // (index line) and body. Tier picks the matching type.
                    let entry_type = match request.tier {
                        MemoryTier::User => MemoryEntryType::Preference,
                        MemoryTier::Project => MemoryEntryType::Project,
                    };
                    match memory.store().write(
                        request.tier,
                        entry_type,
                        None,
                        &request.fact,
                        &request.fact,
                        None,
                    ) {
                        Ok(written) => outcome.messages.push(status(format!(
                            "remembered: {} ({}) — indexed next session",
                            written.entry.description,
                            written.entry.path.display()
                        ))),
                        Err(err) => outcome
                            .messages
                            .push(error(format!("Could not save memory: {err:#}"))),
                    }
                }
                None => outcome
                    .messages
                    .push(error("Memory is unavailable in this session.")),
            },
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/memory") => match parse_memory_command(input) {
            Ok(request) => match agent.memory().cloned() {
                Some(memory) => match request {
                    MemoryCommandRequest::List => {
                        let store = memory.store();
                        outcome.messages.push(status(format_memory_list(
                            &store.entries(),
                            store.dir(MemoryTier::User),
                            store.dir(MemoryTier::Project),
                        )));
                    }
                    MemoryCommandRequest::View { name } => match memory.store().get(&name) {
                        Some(entry) => outcome.messages.push(status(format_memory_view(&entry))),
                        None => outcome.messages.push(error(unknown_memory_entry_message(
                            &name,
                            &memory.store().entries(),
                        ))),
                    },
                    MemoryCommandRequest::Forget { name } => {
                        if memory.store().get(&name).is_none() {
                            outcome.messages.push(error(unknown_memory_entry_message(
                                &name,
                                &memory.store().entries(),
                            )));
                        } else {
                            match memory.forget(&name).await {
                                Ok(path) => outcome.messages.push(status(format!(
                                    "Forgot memory entry {name} ({})",
                                    path.display()
                                ))),
                                Err(err) => outcome
                                    .messages
                                    .push(error(format!("Could not forget {name}: {err:#}"))),
                            }
                        }
                    }
                },
                None => outcome
                    .messages
                    .push(error("Memory is unavailable in this session.")),
            },
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/yolo") => match parse_yolo_command(input) {
            Ok(YoloCommandRequest::Set(on)) => {
                let level = if on {
                    ApprovalLevel::Yolo
                } else {
                    ApprovalLevel::Ask
                };
                agent.set_approval_level(level);
                outcome.messages.push(status(autonomy_set_message(level)));
            }
            Ok(YoloCommandRequest::Toggle) => {
                let level = if agent.approval_level() == ApprovalLevel::Yolo {
                    ApprovalLevel::Ask
                } else {
                    ApprovalLevel::Yolo
                };
                agent.set_approval_level(level);
                outcome.messages.push(status(autonomy_set_message(level)));
            }
            Ok(YoloCommandRequest::Status) => {
                outcome
                    .messages
                    .push(status(autonomy_status_message(agent.approval_level())));
            }
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/sandbox") => match parse_sandbox_command(input) {
            Ok(SandboxCommandRequest::Set(on)) => {
                let sandbox = agent.sandbox();
                if on && !sandbox.backend().is_available() {
                    outcome.messages.push(error(sandbox_unavailable_message()));
                } else {
                    sandbox.set_enabled(on);
                    outcome
                        .messages
                        .push(status(sandbox_set_message(on, &sandbox)));
                }
            }
            Ok(SandboxCommandRequest::SetNet(on)) => {
                let sandbox = agent.sandbox();
                sandbox.set_deny_network(on);
                outcome
                    .messages
                    .push(status(sandbox_net_message(on, &sandbox)));
            }
            Ok(SandboxCommandRequest::Status) => {
                outcome.modal = Some(CommandModalRequest::SandboxStatus);
            }
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/config") => match parse_config_command(input) {
            Ok(ConfigCommandRequest::View) => {
                outcome
                    .messages
                    .push(status(format_config_view(agent.config())));
            }
            Ok(ConfigCommandRequest::Validate) => {
                let layers = &agent.config().layers;
                let reloaded = crate::config::load(&layers.project_root, &layers.bonsai_home);
                outcome
                    .messages
                    .push(status(format_config_diagnostics(&reloaded.diagnostics)));
            }
            Ok(ConfigCommandRequest::Edit(scope)) => {
                outcome
                    .messages
                    .push(status(config_edit_message(agent.config(), scope)));
            }
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/bug") => {
            let description = arg.unwrap_or_default().trim().to_string();
            if description.is_empty() {
                outcome.messages.push(error(
                    "Usage: /bug <description of the problem>".to_string(),
                ));
            } else {
                match crate::commands::bug::run_bug_flow(
                    &description,
                    agent,
                    storage,
                    catalog,
                    session_store,
                    &registry,
                    project_root,
                )
                .await
                {
                    Ok(message) => outcome.messages.push(status(message)),
                    Err(message) => outcome.messages.push(error(message)),
                }
            }
        }
        Some("/doctor") => {
            match crate::doctor::DoctorRequest::parse_slash_args(arg.unwrap_or_default()) {
                Ok(request) => {
                    let sandbox = agent.sandbox();
                    let (mcp_reachability, release_status) =
                        if request.network == crate::doctor::DoctorNetworkMode::Online {
                            let release = crate::release::check();
                            match agent.mcp_hub().cloned() {
                                Some(hub) => {
                                    let (mcp, release) =
                                        tokio::join!(hub.probe_reachability(), release);
                                    (Some(mcp), Some(release))
                                }
                                None => (None, Some(release.await)),
                            }
                        } else {
                            (None, None)
                        };
                    let release_status = match release_status {
                        Some(status) => Some(
                            crate::update::refine_release_status(
                                status,
                                &agent.config().layers.bonsai_home,
                            )
                            .await,
                        ),
                        None => None,
                    };
                    let report = crate::doctor::collect(crate::doctor::DoctorContext {
                        project_root,
                        storage,
                        catalog,
                        catalog_loaded_cleanly: catalog.is_some(),
                        config: agent.config(),
                        session_store: Some(session_store),
                        registry: Some(&registry),
                        sandbox: &sandbox,
                        network_mode: request.network,
                        mcp_reachability: mcp_reachability.as_ref(),
                        release_status: release_status.as_ref(),
                    })
                    .await;
                    // Text mode opens the summary modal; `json` stays a
                    // transcript dump for the machine-readable support form.
                    match request.format {
                        crate::doctor::DoctorOutputFormat::Json => {
                            match report.render(crate::doctor::DoctorOutputFormat::Json) {
                                Ok(report) => outcome.messages.push(status(report)),
                                Err(_) => outcome.messages.push(error(
                                    "Doctor report could not be serialized. Rerun `/doctor` without `json`."
                                        .to_string(),
                                )),
                            }
                        }
                        crate::doctor::DoctorOutputFormat::Text => {
                            outcome.modal = Some(CommandModalRequest::Doctor(Box::new(report)));
                        }
                    }
                }
                Err(message) => outcome.messages.push(error(message)),
            }
        }
        Some("/mcp") => match parse_mcp_command(input) {
            Ok(McpCommandRequest::View) => {
                outcome.modal = Some(CommandModalRequest::McpServers {
                    rows: crate::tui::mcp::mcp_server_rows(agent.extensions()),
                });
            }
            Ok(request) => {
                let hub = agent.mcp_hub().map(std::sync::Arc::as_ref);
                match apply_mcp_command(request, hub, agent.extensions()).await {
                    Ok(message) => outcome.messages.push(status(message)),
                    Err(message) => outcome.messages.push(error(message)),
                }
            }
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/hooks") => match parse_hooks_command(input) {
            Ok(request) => {
                match apply_hooks_command(request, agent.hooks(), agent.extensions()).await {
                    Ok(message) => outcome.messages.push(status(message)),
                    Err(message) => outcome.messages.push(error(message)),
                }
            }
            Err(message) => outcome.messages.push(error(message)),
        },
        Some("/tasks") => match parts.get(1).copied() {
            None => {
                outcome
                    .messages
                    .push(status(agent.background_tasks_report().await));
            }
            Some("stop") => {
                let Some(task_id) = parts.get(2).copied() else {
                    outcome
                        .messages
                        .push(error("Usage: /tasks stop <id>".to_string()));
                    return Ok(outcome);
                };
                match agent.stop_background_task(task_id).await {
                    Ok(report) => outcome.messages.push(status(report)),
                    Err(err) => outcome
                        .messages
                        .push(error(format!("Failed to stop {task_id}: {err:#}"))),
                }
            }
            Some(_) => outcome
                .messages
                .push(error("Usage: /tasks or /tasks stop <id>".to_string())),
        },
        Some("/init") => {
            let path = project_root.join("AGENTS.md");
            if path.exists() {
                outcome.messages.push(status(
                    "AGENTS.md already exists — not overwriting. Edit it directly.",
                ));
            } else {
                match std::fs::write(&path, starter_agents_md(project_root)) {
                    Ok(()) => outcome.messages.push(status(
                        "Created AGENTS.md — edit it to steer the agent; it loads automatically on next launch.",
                    )),
                    Err(err) => outcome
                        .messages
                        .push(error(format!("Failed to write AGENTS.md: {err}"))),
                }
            }
        }
        Some("/skills") => match (parts.get(1).copied(), parts.get(2).copied()) {
            (None, _) => {
                outcome
                    .messages
                    .push(status(format_skills_list(&agent.skills())));
            }
            (Some(action @ ("disable" | "enable")), name) => {
                let disable = action == "disable";
                outcome.messages.push(toggle_builtin_skill_message(
                    agent,
                    project_root,
                    name.filter(|value| !value.is_empty()),
                    disable,
                ));
            }
            (Some(other), _) => {
                outcome.messages.push(error(format!(
                    "Unknown /skills subcommand '{other}'. Use /skills, /skills disable <name>, or /skills enable <name>."
                )));
            }
        },
        Some("/skill") => {
            let Some(name) = arg.filter(|value| !value.is_empty()) else {
                outcome
                    .messages
                    .push(error("Usage: /skill <name>".to_string()));
                return Ok(outcome);
            };
            // Resolve the body under an immutable borrow, then inject under a
            // mutable one.
            let loaded = agent
                .skills()
                .get(name)
                .map(|skill| (skill.name.clone(), skill.render_for_context()));
            match loaded {
                Some((skill_name, body)) => {
                    agent.push_user_context_note(&body);
                    agent.mark_skill_loaded(&skill_name);
                    outcome
                        .messages
                        .push(status(format!("Loaded skill '{skill_name}' into context.")));
                }
                None => {
                    outcome.messages.push(error(format!(
                        "Unknown skill '{name}'.{}",
                        available_skills_hint(&agent.skills())
                    )));
                }
            }
        }
        Some("/agents") => match parts.get(1).copied() {
            Some("init") => {
                let path = project_root.join(".bonsai/agents/example.md");
                if path.exists() {
                    outcome.messages.push(status(format!(
                        "{} already exists — not overwriting. Edit it directly.",
                        path.display()
                    )));
                } else {
                    let write = std::fs::create_dir_all(path.parent().unwrap())
                        .and_then(|()| std::fs::write(&path, example_agent_md()));
                    match write {
                        Ok(()) => outcome.messages.push(status(format!(
                            "Created {} — a disabled example. Edit it, set `enabled: true`, and relaunch.",
                            path.display()
                        ))),
                        Err(err) => outcome
                            .messages
                            .push(error(format!("Failed to write example agent: {err}"))),
                    }
                }
            }
            Some(other) => outcome.messages.push(error(format!(
                "Unknown /agents subcommand '{other}'. Usage: /agents or /agents init"
            ))),
            None => outcome.messages.push(status(format_agents_list(
                &agent.custom_agents(),
                &agent.builtin_subagent_settings(),
            ))),
        },
        Some("/authorize") => {
            handle_authorize(arg, agent, session_store, &registry, catalog, &mut outcome).await?;
        }
        Some("/unauthorize") => {
            handle_unauthorize(arg, agent, session_store, &registry, catalog, &mut outcome).await?;
        }
        Some("/model") => {
            handle_model(
                arg,
                agent,
                session_store,
                &registry,
                catalog,
                runtime.active_mode,
                &mut outcome,
            )
            .await?;
        }
        Some(command) if parts.len() == 1 && ModelShortcutKey::from_command(command).is_some() => {
            let key = ModelShortcutKey::from_command(command)
                .expect("guard ensures command is a valid model shortcut");
            apply_shortcut_command(
                key,
                agent,
                session_store,
                &registry,
                catalog,
                runtime.active_mode,
                &mut outcome,
            )
            .await?;
        }
        Some("/providers") => {
            let messages = crate::commands::providers_manage::providers_command_messages(
                &parts[1..],
                session_store,
                &registry,
                catalog,
            )
            .await;
            outcome.messages.extend(messages);
        }
        Some("/refresh") => {
            let Some(catalog) = catalog else {
                outcome.messages.push(error(
                    "Refresh failed: model catalog is unavailable".to_string(),
                ));
                return Ok(outcome);
            };
            match refresh_all_provider_models(&registry, session_store, catalog).await {
                Ok(report) => outcome.messages.push(status(report)),
                Err(err) => outcome
                    .messages
                    .push(error(format!("Refresh failed: {err:#}"))),
            }
        }
        Some("/clear") | Some("/new") => {
            agent.clear().await;
            outcome.clear_transcript = true;
            let message = if parts.first() == Some(&"/new") {
                "Started new session"
            } else {
                "Conversation cleared"
            };
            outcome.messages.push(status(message.to_string()));
        }
        Some("/quit") => {
            outcome.quit = true;
            outcome.messages.push(status("Goodbye!".to_string()));
        }
        None => {}
        Some(command) => match command_surface(command) {
            Some(CommandSurface::TuiOnly {
                headless_message,
                strict_args,
            }) => {
                if *strict_args && parts.len() > 1 {
                    outcome.messages.push(error(format!("Usage: {command}")));
                } else {
                    outcome
                        .messages
                        .push(unsupported_tui_feature(headless_message));
                }
            }
            _ => outcome
                .messages
                .push(error(format!("Unknown command: {command}"))),
        },
    }

    Ok(outcome)
}

async fn handle_authorize(
    arg: Option<&str>,
    agent: &mut Agent,
    session_store: &mut SessionStore,
    registry: &Arc<ProviderRegistry>,
    catalog: Option<&ModelCatalog>,
    outcome: &mut CommandOutcome,
) -> Result<()> {
    if arg.is_none_or(|value| value.is_empty()) {
        outcome.modal = Some(CommandModalRequest::AuthorizeProviderPicker {
            providers: authorize_provider_choices(registry, session_store),
        });
        return Ok(());
    }

    let target_id = arg
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| session_store.current_kind_id().to_string());

    let Some(factory) = resolve_provider(registry, &target_id) else {
        outcome
            .messages
            .push(unknown_provider_error(&target_id, registry));
        return Ok(());
    };

    let target_id = factory.metadata().id.to_string();
    if factory.metadata().auth_requirement.uses_codex_cache() {
        let result = crate::commands::authorize_provider_with_input_as_current(
            registry,
            &target_id,
            authorization_input_for_provider(factory.metadata()),
            session_store,
            catalog,
        )
        .await;
        match result {
            Ok(mutation) => {
                debug_assert_eq!(
                    mutation.committed_state,
                    crate::commands::ProviderAuthorizationState::Authorized
                );
                rebuild_agent_provider(agent, registry, session_store, catalog);
                outcome.messages.push(status(format!(
                    "Authorized {}",
                    factory.metadata().display_name
                )));
                outcome.messages.extend(
                    mutation
                        .warnings
                        .into_iter()
                        .map(|warning| status(warning.to_string())),
                );
            }
            Err(err) => {
                let detail = format!("{:#}", err);
                if detail.to_lowercase().contains("codex login")
                    || detail.to_lowercase().contains("auth.json")
                {
                    outcome.messages.push(error(format!(
                        "Codex login required. Run `codex login` in another terminal, then `/authorize codex` again.\n{detail}"
                    )));
                } else {
                    outcome
                        .messages
                        .push(error(format!("Authorization failed: {detail}")));
                }
            }
        }
    } else {
        outcome.modal = Some(CommandModalRequest::ApiKeyPrompt {
            provider_id: target_id,
        });
        let prompt = if factory.metadata().auth_requirement.uses_endpoint_setup() {
            format!(
                "Opening endpoint setup for {}...",
                factory.metadata().display_name
            )
        } else {
            format!(
                "Opening API key prompt for {}...",
                factory.metadata().display_name
            )
        };
        outcome.messages.push(status(prompt));
    }
    Ok(())
}

async fn handle_unauthorize(
    arg: Option<&str>,
    agent: &mut Agent,
    session_store: &mut SessionStore,
    registry: &Arc<ProviderRegistry>,
    catalog: Option<&ModelCatalog>,
    outcome: &mut CommandOutcome,
) -> Result<()> {
    if arg.is_none_or(|value| value.is_empty()) {
        outcome.modal = Some(CommandModalRequest::UnauthorizeProviderPicker {
            providers: unauthorize_provider_choices(registry, session_store),
        });
        return Ok(());
    }

    let target_id = arg
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(|| session_store.current_kind_id().to_string());

    let Some(factory) = resolve_provider(registry, &target_id) else {
        outcome
            .messages
            .push(unknown_provider_error(&target_id, registry));
        return Ok(());
    };
    let target_id = factory.metadata().id.to_string();

    let was_current = target_id == session_store.current_kind_id();
    let mutation = match unauthorize_provider(registry, &target_id, session_store, catalog).await {
        Ok(mutation) => mutation,
        Err(err) => {
            outcome
                .messages
                .push(error(format!("Unauthorize failed: {:#}", err)));
            return Ok(());
        }
    };
    debug_assert_eq!(
        mutation.committed_state,
        crate::commands::ProviderAuthorizationState::Unauthorized
    );

    if was_current {
        rebuild_agent_provider(agent, registry, session_store, catalog);
    }

    let display = registry
        .lookup(&target_id)
        .map(|factory| factory.metadata().display_name.as_ref())
        .unwrap_or(target_id.as_str());
    outcome
        .messages
        .push(status(format!("Cleared Bonsai auth for {}", display)));
    outcome.messages.extend(
        mutation
            .warnings
            .into_iter()
            .map(|warning| status(warning.to_string())),
    );
    if factory.metadata().auth_requirement.uses_codex_cache() {
        outcome.messages.push(status(
            "Codex auth cache was not removed from ~/.codex/auth.json".to_string(),
        ));
    }
    Ok(())
}

/// Switch the live model + reasoning to the model bound to `key`, persisting
/// the change and rebuilding the agent's provider.
async fn apply_shortcut_command(
    key: ModelShortcutKey,
    agent: &mut Agent,
    session_store: &mut SessionStore,
    registry: &Arc<ProviderRegistry>,
    catalog: Option<&ModelCatalog>,
    active_mode: Option<crate::agent::AgentMode>,
    outcome: &mut CommandOutcome,
) -> Result<()> {
    let Some(shortcut_selection) = resolve_model_shortcut(registry, session_store, catalog, key)
    else {
        outcome.messages.push(error(format!(
            "No model shortcut assigned to {}. Open /model, focus the Reasoning pane, and press {} on a model to assign it.",
            key.as_command(),
            key.as_char()
        )));
        return Ok(());
    };
    let selection = ResolvedModelSelection::from(&shortcut_selection);
    let old_context_window =
        context_window_for_current_model_with_catalog(registry, session_store, catalog) as usize;
    let previous_selection = session_store.current_model_selection_input();
    apply_model_selection(
        registry,
        session_store,
        catalog,
        &selection,
        shortcut_selection.reasoning,
    );
    record_active_mode_model(session_store, active_mode, previous_selection);
    let new_context_window =
        context_window_for_current_model_with_catalog(registry, session_store, catalog) as usize;
    session_store.save_async().await?;
    rebuild_agent_provider(agent, registry, session_store, catalog);
    outcome.messages.push(status(format!(
        "Model set to {} ({}, reasoning: {})",
        selection.model,
        key.as_command(),
        shortcut_selection.reasoning
    )));
    if let Some(warning) = context_window_downgrade_warning(old_context_window, new_context_window)
    {
        outcome.messages.push(status(warning));
    }
    Ok(())
}

async fn handle_model(
    arg: Option<&str>,
    agent: &mut Agent,
    session_store: &mut SessionStore,
    registry: &Arc<ProviderRegistry>,
    catalog: Option<&ModelCatalog>,
    active_mode: Option<crate::agent::AgentMode>,
    outcome: &mut CommandOutcome,
) -> Result<()> {
    let Some(input) = arg.filter(|value| !value.is_empty()) else {
        if let Some(notice) = catalog.and_then(ModelCatalog::models_dev_refresh_notice) {
            outcome.messages.push(status(notice));
        }
        match open_model_picker(registry, session_store, agent, None, catalog).await? {
            ModelPickerOutcome::Open => outcome.modal = Some(CommandModalRequest::ModelPicker),
            ModelPickerOutcome::Rejected(message) => outcome.messages.push(message),
        }
        return Ok(());
    };

    if let Ok(key) = input.parse::<ModelShortcutKey>() {
        return apply_shortcut_command(
            key,
            agent,
            session_store,
            registry,
            catalog,
            active_mode,
            outcome,
        )
        .await;
    }

    let Some(selection) = resolve_model_selection(registry, session_store, catalog, input) else {
        outcome.messages.push(error(format!(
            "Unknown model '{input}' for provider '{}'. Run /model to browse, /refresh to update provider lists, or add local models through /providers. Trusted workspaces can also define `.bonsai/models`.",
            session_store.current_kind_id(),
        )));
        return Ok(());
    };

    // The `/model` text command carries no reasoning argument, so derive it from
    // prior session state; `apply_model_selection` then normalizes and stores it
    // against the new model.
    let requested_reasoning = crate::commands::model_switch::remembered_reasoning_for(
        registry,
        session_store,
        catalog,
        &selection,
    );
    let old_context_window =
        context_window_for_current_model_with_catalog(registry, session_store, catalog) as usize;
    let previous_selection = session_store.current_model_selection_input();
    apply_model_selection(
        registry,
        session_store,
        catalog,
        &selection,
        requested_reasoning,
    );
    record_active_mode_model(session_store, active_mode, previous_selection);
    let new_context_window =
        context_window_for_current_model_with_catalog(registry, session_store, catalog) as usize;
    session_store.save_async().await?;
    rebuild_agent_provider(agent, registry, session_store, catalog);
    outcome
        .messages
        .push(status(format!("Model set to {}", selection.model)));
    if let Some(warning) = context_window_downgrade_warning(old_context_window, new_context_window)
    {
        outcome.messages.push(status(warning));
    }
    Ok(())
}

fn parse_compaction_request(parts: &[&str]) -> std::result::Result<CompactionRequest, String> {
    let mut preview = false;
    let mut target_tokens = None;
    let mut summary_policy = CompactionSummaryPolicy::ProviderThenDeterministic;

    for arg in parts.iter().skip(1).copied() {
        match arg {
            "preview" => preview = true,
            "fallback" | "auto" => {
                summary_policy = CompactionSummaryPolicy::ProviderThenDeterministic;
            }
            "provider" | "strict" => {
                summary_policy = CompactionSummaryPolicy::ProviderOnly;
            }
            "deterministic" | "offline" => {
                summary_policy = CompactionSummaryPolicy::DeterministicOnly;
            }
            _ => {
                let Some(value) = arg.strip_prefix("target=") else {
                    return Err(compaction_usage());
                };
                let Some(tokens) = parse_token_count(value) else {
                    return Err("Usage: /compact [preview] [target=<tokens>] [fallback|provider|deterministic]".to_string());
                };
                target_tokens = Some(tokens);
            }
        }
    }

    let mut request = CompactionRequest::manual(preview, CancellationToken::new())
        .with_summary_policy(summary_policy);
    if let Some(target_tokens) = target_tokens {
        request = request.with_target_tokens(target_tokens);
    }
    Ok(request)
}

fn parse_token_count(value: &str) -> Option<usize> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(thousands) = value.strip_suffix('k').or_else(|| value.strip_suffix('K')) {
        return thousands
            .parse::<usize>()
            .ok()
            .and_then(|tokens| tokens.checked_mul(1_000));
    }
    value.parse::<usize>().ok()
}

fn compaction_usage() -> String {
    "Usage: /compact [preview] [target=<tokens>] [fallback|provider|deterministic]".to_string()
}

fn format_compaction_report(report: &CompactionReport) -> String {
    if !report.preview && !report.has_changes() {
        return format!(
            "Context already within target: {} tokens (target {}); nothing to compact.",
            crate::util::format::format_tokens_k(report.before_tokens),
            crate::util::format::format_tokens_k(report.target_tokens),
        );
    }
    let verb = if report.preview {
        "Context compaction preview"
    } else {
        "Context compacted"
    };
    let summary = match report.summary_source {
        CompactionSummarySource::Preview => "preview",
        CompactionSummarySource::Provider => "provider summary",
        CompactionSummarySource::Deterministic => "deterministic fallback",
        CompactionSummarySource::None => "none",
    };
    let restore = if report.summary_source_available {
        "available"
    } else {
        "not needed"
    };
    let target = if report.target_reached { "yes" } else { "no" };
    format!(
        "{verb}: {} -> {} tokens (target {}). messages omitted: {}; tool outputs stubbed: {}; summary: {}; restore: {}; target reached: {target}.",
        crate::util::format::format_tokens_k(report.before_tokens),
        crate::util::format::format_tokens_k(report.after_tokens),
        crate::util::format::format_tokens_k(report.target_tokens),
        report.messages_omitted,
        report.tool_outputs_stubbed,
        summary,
        restore
    )
}

fn format_skills_list(skills: &SkillRegistry) -> String {
    use crate::resource::skill::BuiltinSkillState;

    // Keep the state tag scannable: put it right after the name and trim the
    // (deliberately rich, for the model) description to its first clause.
    fn short_desc(description: &str) -> String {
        let head = description
            .split(" — ")
            .next()
            .unwrap_or(description)
            .trim();
        const MAX: usize = 72;
        if head.chars().count() <= MAX {
            return head.to_string();
        }
        let clipped: String = head.chars().take(MAX).collect();
        format!("{}…", clipped.trim_end())
    }

    let mut lines = Vec::new();

    let user_skills: Vec<_> = skills.user_skills().collect();
    if user_skills.is_empty() {
        lines.push(
            "No project skills. Add .bonsai/skills/<name>/SKILL.md (or .claude/skills/) to define one."
                .to_string(),
        );
    } else {
        lines.push("Project skills:".to_string());
        for skill in user_skills {
            let origin = if skill.is_global() {
                "global"
            } else {
                "project"
            };
            lines.push(format!(
                "  {} ({origin}) — {}",
                skill.name,
                short_desc(&skill.description)
            ));
        }
    }

    let builtins = skills.builtin_status();
    if !builtins.is_empty() {
        lines.push(String::new());
        lines.push("Built-in skills:".to_string());
        for status in builtins {
            let mut line = format!(
                "  {} ({}) — {}",
                status.name,
                status.state.label(),
                short_desc(&status.description)
            );
            // Inactive built-ins say how to turn them on.
            if status.state == BuiltinSkillState::Inactive && !status.activation_hint.is_empty() {
                line.push_str(&format!("  · activate with {}", status.activation_hint));
            }
            lines.push(line);
        }
    }

    lines.push(String::new());
    lines.push(
        "Load one with /skill <name>, or the agent can call the `skill` tool on demand."
            .to_string(),
    );
    lines.push(
        "Enable/disable a built-in with /skills enable|disable <name> (applied live), or edit .bonsai/skills/.disabled."
            .to_string(),
    );
    lines.join("\n")
}

/// Add or remove a built-in skill from the project's `.bonsai/skills/.disabled`
/// list, then reload the agent's registry so the change takes effect live — the
/// `skill` tool stops/starts offering it and the prompt's Skills index
/// re-renders, no relaunch needed. Validates `name` against the known built-ins
/// so a typo can't silently write a dead entry.
fn toggle_builtin_skill_message(
    agent: &mut Agent,
    project_root: &Path,
    name: Option<&str>,
    disable: bool,
) -> CommandMessage {
    let verb = if disable { "disable" } else { "enable" };
    let Some(name) = name else {
        return error(format!("Usage: /skills {verb} <built-in name>"));
    };
    let known: Vec<String> = agent
        .skills()
        .builtin_status()
        .iter()
        .map(|status| status.name.clone())
        .collect();
    if !known.iter().any(|known| known == name) {
        return error(format!(
            "'{name}' is not a built-in skill. Built-ins: {}.",
            known.join(", ")
        ));
    }

    match crate::resource::discovery::set_disabled(
        project_root,
        name,
        disable,
        crate::resource::discovery::ResourceKind::Skills,
    ) {
        Ok(true) => {
            agent.reload_skills();
            let verb = if disable { "Disabled" } else { "Re-enabled" };
            status(format!("{verb} built-in skill '{name}'."))
        }
        Ok(false) if disable => status(format!("Built-in skill '{name}' is already disabled.")),
        Ok(false) => status(format!("Built-in skill '{name}' is already enabled.")),
        Err(err) => error(format!("Could not update the disabled-skills list: {err}")),
    }
}

fn format_agents_list(
    custom: &AgentRegistry,
    settings: &crate::subagent::BuiltinSubagentSettingsRegistry,
) -> String {
    let mut lines = vec!["Agents and subagents:".to_string()];
    for spec in crate::tool::builtin_agents() {
        let legacy = custom.get(spec.name);
        let persisted = settings.get(&spec.id);
        let mut line = format!(
            "  {} — {} (built-in · subagent)",
            spec.name, spec.description
        );
        let enabled = persisted
            .map(|settings| settings.enabled)
            .or_else(|| legacy.map(|def| def.enabled))
            .unwrap_or(true);
        if !enabled {
            line.push_str("  (disabled)");
        }
        if let Some(settings) = persisted {
            append_model_summary(
                &mut line,
                settings.primary_model.as_deref(),
                settings.primary_effort.as_deref(),
            );
        } else if let Some(def) = legacy {
            append_agent_model_summary(&mut line, def);
        }
        lines.push(line);
    }
    for def in custom
        .iter()
        .filter(|def| !crate::tool::is_builtin_agent(&def.name))
    {
        let origin = if def.is_global() { "global" } else { "project" };
        let kind = match (def.allows_mode(), def.allows_subagent()) {
            (true, true) => "both",
            (true, false) => "agent",
            (false, true) => "subagent",
            (false, false) => "no surface",
        };
        let mut line = format!("  {} — {} ({origin} · {kind})", def.name, def.description);
        if !def.enabled {
            line.push_str("  (disabled)");
        }
        if def.allows_subagent() {
            append_agent_model_summary(&mut line, def);
            if let Some(max_turns) = def.max_turns {
                line.push_str(&format!("  [max turns: {max_turns}]"));
            }
        }
        if let Some(invalid) = invalid_custom_agent_tools(def) {
            line.push_str(&format!("  [unknown tools ignored: {invalid}]"));
        }
        lines.push(line);
    }
    lines.push(
        "Shift+Tab cycles user-facing agents; the `agent` tool delegates to subagents. A custom \
         definition may support either surface or both. Mutating subagent tools still prompt \
         under the current approval policy."
            .to_string(),
    );
    lines.join("\n")
}

fn append_agent_model_summary(line: &mut String, def: &crate::resource::agent::AgentDef) {
    append_model_summary(line, def.model.as_deref(), def.effort.as_deref());
}

fn append_model_summary(line: &mut String, model: Option<&str>, effort: Option<&str>) {
    if let Some(model) = model {
        let effort = effort
            .map(|effort| format!(", effort {effort}"))
            .unwrap_or_default();
        line.push_str(&format!("  [model: {model}{effort}]"));
    }
}

/// Comma-joined declared tools that aren't grantable to a custom agent (an
/// unknown name, or a recursion/session-internal tool like `agent`), or `None`
/// when the agent's `tools:` are all valid. Mutating tools (`write`/`edit`/
/// `bash`) are grantable now and prompt under the current policy, so they are not
/// flagged.
fn invalid_custom_agent_tools(def: &crate::resource::agent::AgentDef) -> Option<String> {
    let invalid: Vec<&str> = def
        .tools
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|name| crate::tool::canonical_agent_tool(name).is_none())
        .map(String::as_str)
        .collect();
    (!invalid.is_empty()).then(|| invalid.join(", "))
}

/// A disabled, worked-example custom agent written by `/agents init`.
fn example_agent_md() -> String {
    // Keep in sync with the read-only tool set + the agent format.
    "---\n\
name: api-explorer\n\
description: Maps HTTP routes and handlers; returns file:line\n\
# Disabled so this example never runs until you opt in.\n\
enabled: false\n\
# Invocation surface: [subagent] (default), [mode], or [mode, subagent].\n\
surface: [subagent]\n\
# Grant a subset of tools (omit for the read-only default). write/edit/bash are\n\
# grantable and prompt for approval under your current policy:\n\
# project_info, read, glob, grep, symbol_search, git, write, edit, bash\n\
tools: [read, grep, symbol_search]\n\
# Optional: run this agent on a model shortcut or full /model selector.\n\
# effort: minimal|low|medium|high|xhigh|max\n\
# model: f\n\
# model: codex:openai/gpt-5.5\n\
# effort: low\n\
# Optional: override the default 75-turn custom-subagent budget (maximum 500).\n\
# max_turns: 72\n\
---\n\
You are a read-only API exploration subagent. Locate route/handler definitions\n\
with grep and symbol_search and report them as `file:line` with the method and\n\
path. Do not modify anything; finish by stating what you found.\n"
        .to_string()
}

fn available_skills_hint(skills: &SkillRegistry) -> String {
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    if names.is_empty() {
        " No skills are defined in this project.".to_string()
    } else {
        format!(" Available: {}.", names.join(", "))
    }
}

fn starter_agents_md(root: &Path) -> String {
    let commands = if root.join("Cargo.toml").exists() {
        "```bash\ncargo build\ncargo test\ncargo clippy --all-targets --all-features -- -D warnings\ncargo fmt --all\n```"
    } else if root.join("package.json").exists() {
        "```bash\nnpm install\nnpm test\nnpm run build\n```"
    } else if root.join("go.mod").exists() {
        "```bash\ngo build ./...\ngo test ./...\n```"
    } else {
        "```bash\n# add your build / test / lint commands\n```"
    };
    format!(
        "# AGENTS.md\n\n\
         Instructions for AI coding agents working in this repository.\n\n\
         ## Commands\n\n{commands}\n\n\
         ## Conventions\n\n\
         - Describe the code style, naming, and patterns to follow.\n\
         - Note anything the agent should avoid.\n\n\
         ## Architecture\n\n\
         - Briefly describe the project layout and key modules.\n"
    )
}
