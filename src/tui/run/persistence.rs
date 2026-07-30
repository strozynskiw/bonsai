use super::*;

// IMPORTANT SNAPSHOT ORDERING INVARIANT: every TUI snapshot path must enter
// both this write lock and the per-session generation gate. Do not bypass,
// remove, or reorder them: a superseded background flush could otherwise land
// after `/new`, `/clear`, `/resume`, or final persistence and overwrite newer
// state while also acknowledging the wrong peer-delivery receipts.
static SNAPSHOT_WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static SNAPSHOT_GENERATIONS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<(std::path::PathBuf, i64), u64>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn snapshot_generation_key(storage: &Storage, session_id: SessionId) -> (std::path::PathBuf, i64) {
    (storage.db_path().to_path_buf(), session_id.as_i64())
}

fn begin_snapshot_generation(storage: &Storage, session_id: SessionId) -> u64 {
    let mut generations = SNAPSHOT_GENERATIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let generation = generations
        .entry(snapshot_generation_key(storage, session_id))
        .or_default();
    *generation = generation.saturating_add(1);
    *generation
}

fn snapshot_generation_is_current(
    storage: &Storage,
    session_id: SessionId,
    generation: u64,
) -> bool {
    snapshot_generation_key_is_current(&snapshot_generation_key(storage, session_id), generation)
}

fn snapshot_generation_key_is_current(key: &(std::path::PathBuf, i64), generation: u64) -> bool {
    SNAPSHOT_GENERATIONS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(key)
        .is_some_and(|current| *current == generation)
}

pub(in crate::tui::run) fn session_end_usage_summary(
    session_id: SessionId,
    provider: &str,
    model: &str,
    report: &crate::agent::ContextReport,
) -> Option<String> {
    let has_usage = report.session_prompt_tokens > 0
        || report.session_completion_tokens > 0
        || report.session_cost_micros.is_some();
    if !has_usage {
        return None;
    }

    let prompt_tokens = format_token_count(report.session_prompt_tokens);
    let completion_tokens = format_token_count(report.session_completion_tokens);
    let cost = crate::tui::widgets::sidebar::optional_cost_micros(report.session_cost_micros);
    let saved = match report.session_savings_micros {
        Some(micros) if micros > 0 => format!(
            " · saved {}",
            crate::tui::widgets::sidebar::format_cost_micros(micros)
        ),
        _ => String::new(),
    };
    let provider_model = provider_model_label(provider, model);
    Some(format!(
        "bonsai session #{session_id} complete: {provider_model} · sent {prompt_tokens} tok · received {completion_tokens} tok · cost {cost}{saved}"
    ))
}

/// Derive a short, human-readable session title from the user's first prompt.
/// Used to seed the `/sessions` list with something more useful than the project
/// name when the agent never calls `set_session_title` itself. Whitespace
/// (including newlines) is collapsed and the result is capped, cutting on a word
/// boundary when possible. Returns `None` for blank input.
pub(in crate::tui::run) fn derive_session_title(input: &str) -> Option<String> {
    crate::session_persist::derive_session_title(input)
}

/// One-line reminder, printed to the terminal after the TUI tears down, telling
/// the user how to bring the session that just closed back. `/resume <id>` works
/// inside bonsai; `bonsai -c` relaunches straight into the latest prior session
/// (which is this one immediately after quitting).
pub(in crate::tui::run) fn format_resume_hint(session_id: SessionId) -> String {
    format!(
        "Resume this session: /resume {session_id}  (or relaunch with `bonsai -c {session_id}`)"
    )
}

fn provider_model_label(provider: &str, model: &str) -> String {
    if model.contains('/') {
        model.to_string()
    } else {
        format!("{provider}/{model}")
    }
}

fn format_token_count(tokens: u64) -> String {
    let digits = tokens.to_string();
    let mut formatted = String::with_capacity(digits.len() + digits.len() / 3);
    for (idx, ch) in digits.chars().enumerate() {
        if idx > 0 && (digits.len() - idx).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(ch);
    }
    formatted
}

fn usage_snapshot_from_context_report(
    report: &crate::agent::ContextReport,
) -> Option<crate::session_persist::UsageStateSnapshot> {
    let has_usage = report.session_prompt_tokens > 0
        || report.session_completion_tokens > 0
        || report.session_cost_micros.is_some()
        || !report.usage_turns.is_empty();
    if !has_usage {
        return None;
    }
    let turn_no_cache_cost = report.session_cost_micros.and_then(|session_cost| {
        let mut saw_priced_turn = false;
        let mut actual = 0u64;
        let mut no_cache = 0u64;
        for turn in &report.usage_turns {
            match (turn.turn_cost_micros, turn.no_cache_cost_micros) {
                (Some(turn_actual), Some(turn_no_cache)) => {
                    saw_priced_turn = true;
                    actual = actual.saturating_add(turn_actual);
                    no_cache = no_cache.saturating_add(turn_no_cache);
                }
                (Some(_), None) => return None,
                (None, _) => {}
            }
        }
        (saw_priced_turn && actual == session_cost).then_some(no_cache)
    });
    let usage = crate::agent::UsageTotals {
        prompt_tokens: report.session_prompt_tokens,
        completion_tokens: report.session_completion_tokens,
        cost_micros: report.session_cost_micros,
        // IMPORTANT ACCOUNTING INVARIANT: prefer per-turn pricing only when it
        // completely covers the restored actual-cost total. Do not use a plain
        // filter/sum here; one unpriced turn would persist a misleading partial
        // no-cache baseline during a busy-agent fallback.
        no_cache_cost_micros: turn_no_cache_cost.or_else(|| {
            report
                .session_cost_micros
                .zip(report.session_savings_micros)
                .map(|(cost, savings)| cost.saturating_add(savings))
        }),
        input_cache: report.session_input_cache,
    };
    let turns = report
        .usage_turns
        .iter()
        .map(crate::context_view::UsageTurnReport::to_usage_turn)
        .collect::<Vec<_>>();
    Some(crate::session_persist::UsageStateSnapshot { usage, turns })
}

/// Flush only the app-derived snapshots (transcript, plan, todo) for
/// `session_id`. Use this — instead of [`persist_changed_snapshots`] — at a site
/// where the agent is being cleared/replaced as part of the same operation
/// (`/clear`·`/new`): re-reading the live agent there would persist its *cleared*
/// context/usage onto the outgoing session, wiping it. The agent's context for
/// the outgoing session is already current from the periodic flush.
pub(in crate::tui::run) async fn persist_changed_app_snapshots(
    storage: &Storage,
    session_id: SessionId,
    app: &AppState,
    signatures: &mut PersistedSnapshotSignatures,
) {
    let generation = begin_snapshot_generation(storage, session_id);
    let _write_guard = SNAPSHOT_WRITE_LOCK.lock().await;
    if !snapshot_generation_is_current(storage, session_id, generation) {
        return;
    }
    let writer = crate::session_persist::SessionSnapshotWriter::new(storage, session_id);
    let snapshot = crate::session_persist::SessionSnapshotData {
        transcript: app.transcript.as_slice(),
        plan: &app.plan,
        todos: &app.todo,
        agent: None,
        fallback_usage: None,
        ui_peer_delivery_receipts: &[],
        agent_peer_delivery_receipts: &[],
    };
    match writer.persist(snapshot, *signatures).await {
        Ok(outcome) => *signatures = outcome.signatures,
        Err(error) => tracing::warn!(
            session_id = %session_id,
            error = %error,
            "failed to persist app snapshot batch"
        ),
    }
}

#[derive(Debug)]
struct OwnedSessionSnapshot {
    transcript: Vec<TranscriptItem>,
    plan: crate::plan::PlanDoc,
    todos: Vec<crate::todo::TodoItem>,
    agent: Option<crate::session_persist::AgentStateSnapshot>,
    fallback_usage: Option<crate::session_persist::UsageStateSnapshot>,
    ui_peer_delivery_receipts: Vec<crate::storage::PeerDeliveryReceipt>,
    agent_peer_delivery_receipts: Vec<crate::storage::PeerDeliveryReceipt>,
}

#[derive(Debug)]
pub(in crate::tui::run) struct PersistenceFlushResult {
    session_id: SessionId,
    generation_key: (std::path::PathBuf, i64),
    generation: u64,
    result: Option<Result<crate::session_persist::SnapshotWriteOutcome>>,
    ui_peer_delivery_receipts: Vec<crate::storage::PeerDeliveryReceipt>,
    agent_peer_delivery_receipts: Vec<crate::storage::PeerDeliveryReceipt>,
}

#[cfg(test)]
pub(in crate::tui::run) fn failed_persistence_flush_for_tests(
    storage: &Storage,
    session_id: SessionId,
    error: anyhow::Error,
) -> PersistenceFlushResult {
    let generation = begin_snapshot_generation(storage, session_id);
    PersistenceFlushResult {
        session_id,
        generation_key: snapshot_generation_key(storage, session_id),
        generation,
        result: Some(Err(error)),
        ui_peer_delivery_receipts: Vec::new(),
        agent_peer_delivery_receipts: Vec::new(),
    }
}

fn capture_session_snapshot(app: &AppState, agent: &Arc<Mutex<Agent>>) -> OwnedSessionSnapshot {
    let agent_snapshot = agent
        .try_lock()
        .ok()
        .map(|agent| crate::session_persist::AgentStateSnapshot::capture(&agent));
    let agent_peer_delivery_receipts = agent_snapshot
        .as_ref()
        .map(|snapshot| snapshot.peer_delivery_receipts().to_vec())
        .unwrap_or_default();
    let report_usage_snapshot = if agent_snapshot.is_none() {
        app.latest_context_report
            .as_ref()
            .and_then(usage_snapshot_from_context_report)
    } else {
        None
    };

    OwnedSessionSnapshot {
        transcript: app.transcript.as_slice().to_vec(),
        plan: app.plan.clone(),
        todos: app.todo.clone(),
        agent: agent_snapshot,
        fallback_usage: report_usage_snapshot,
        ui_peer_delivery_receipts: app.pending_peer_delivery_receipts(),
        agent_peer_delivery_receipts,
    }
}

async fn persist_owned_session_snapshot(
    storage: Storage,
    session_id: SessionId,
    snapshot: OwnedSessionSnapshot,
    signatures: PersistedSnapshotSignatures,
    generation: u64,
) -> PersistenceFlushResult {
    let generation_key = snapshot_generation_key(&storage, session_id);
    let _write_guard = SNAPSHOT_WRITE_LOCK.lock().await;
    if !snapshot_generation_is_current(&storage, session_id, generation) {
        return PersistenceFlushResult {
            session_id,
            generation_key,
            generation,
            result: None,
            ui_peer_delivery_receipts: snapshot.ui_peer_delivery_receipts,
            agent_peer_delivery_receipts: snapshot.agent_peer_delivery_receipts,
        };
    }
    let writer = crate::session_persist::SessionSnapshotWriter::new(&storage, session_id);
    let data = crate::session_persist::SessionSnapshotData {
        transcript: snapshot.transcript.as_slice(),
        plan: &snapshot.plan,
        todos: &snapshot.todos,
        agent: snapshot.agent.as_ref(),
        fallback_usage: snapshot.fallback_usage.as_ref(),
        ui_peer_delivery_receipts: &snapshot.ui_peer_delivery_receipts,
        agent_peer_delivery_receipts: &snapshot.agent_peer_delivery_receipts,
    };
    let result = writer.persist(data, signatures).await;
    PersistenceFlushResult {
        session_id,
        generation_key,
        generation,
        result: Some(result),
        ui_peer_delivery_receipts: snapshot.ui_peer_delivery_receipts,
        agent_peer_delivery_receipts: snapshot.agent_peer_delivery_receipts,
    }
}

pub(in crate::tui::run) fn apply_persistence_flush_result(
    flush: PersistenceFlushResult,
    current_session_id: SessionId,
    app: &mut AppState,
    agent: &Arc<Mutex<Agent>>,
    signatures: &mut PersistedSnapshotSignatures,
) {
    if flush.session_id != current_session_id
        || !snapshot_generation_key_is_current(&flush.generation_key, flush.generation)
    {
        return;
    }
    match flush.result {
        None => {}
        Some(Ok(outcome)) => {
            if outcome.changed {
                app.clear_peer_delivery_receipts(&flush.ui_peer_delivery_receipts);
                if !flush.agent_peer_delivery_receipts.is_empty()
                    && let Ok(mut agent) = agent.try_lock()
                {
                    agent.clear_peer_delivery_receipts(&flush.agent_peer_delivery_receipts);
                }
            }
            *signatures = outcome.signatures;
        }
        Some(Err(err)) => {
            tracing::warn!(
                session_id = %flush.session_id,
                error = %err,
                "failed to persist session snapshot batch"
            );
            app.set_session_toast(
                "Session changes could not be saved. Free disk space, then run /doctor.",
            );
        }
    }
}

pub(in crate::tui::run) fn spawn_changed_snapshot_flush(
    storage: &Storage,
    session_id: SessionId,
    app: &AppState,
    agent: &Arc<Mutex<Agent>>,
    signatures: PersistedSnapshotSignatures,
) -> tokio::task::JoinHandle<PersistenceFlushResult> {
    let generation = begin_snapshot_generation(storage, session_id);
    let snapshot = capture_session_snapshot(app, agent);
    let storage = storage.clone();
    // IMPORTANT UI RESPONSIVENESS INVARIANT: SQLite snapshot writes must not be
    // awaited inside the input frame. Sessions 94+ showed >500 ms flushes, long
    // enough to starve keyboard polling and make Ctrl+C appear broken.
    tokio::spawn(async move {
        persist_owned_session_snapshot(storage, session_id, snapshot, signatures, generation).await
    })
}

pub(in crate::tui::run) async fn persist_changed_snapshots(
    storage: &Storage,
    session_id: SessionId,
    app: &mut AppState,
    agent: Arc<Mutex<Agent>>,
    signatures: &mut PersistedSnapshotSignatures,
) {
    let generation = begin_snapshot_generation(storage, session_id);
    let snapshot = capture_session_snapshot(app, &agent);
    let flush = persist_owned_session_snapshot(
        storage.clone(),
        session_id,
        snapshot,
        *signatures,
        generation,
    )
    .await;
    apply_persistence_flush_result(flush, session_id, app, &agent, signatures);
}

pub(in crate::tui) type PersistedSnapshotSignatures =
    crate::session_persist::SessionSnapshotSignatures;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui::run) enum PersistenceCommand<'a> {
    SavePlan,
    DiscardPlan,
    /// `/new-plan` — reset the plan canvas to a fresh `PlanDoc` but keep the
    /// conversation transcript and context window (unlike `/new`). Clears the
    /// canvas and unbinds any saved-plan association; never deletes the saved
    /// library record.
    NewPlan,
    ExportPlan(&'a str),
    Plans,
    Sessions,
    Search(&'a str),
    Forget(&'a str),
    Resume(&'a str),
}

pub(in crate::tui) struct PersistenceCommandDeps<'a> {
    pub(in crate::tui) storage: &'a Storage,
    pub(in crate::tui) agent: Arc<Mutex<Agent>>,
    /// Direct memory-service handle for surfaces that must not lock the agent:
    /// the agent mutex is held for a run's entire duration, so mid-run memory
    /// UI (busy-path `/memory`, manager toggles) would freeze the event loop
    /// behind it. The service is created once at startup and never replaced,
    /// so a startup clone stays valid for the whole session.
    pub(in crate::tui) memory: Option<Arc<crate::memory::MemoryService>>,
    pub(in crate::tui) session_store: Arc<Mutex<SessionStore>>,
    pub(in crate::tui) registry: Arc<ProviderRegistry>,
    pub(in crate::tui) model_catalog: Arc<crate::model_catalog::ModelCatalog>,
    pub(in crate::tui) todo_store: SharedTodoStore,
    pub(in crate::tui) plan_store: SharedPlanStore,
    pub(in crate::tui) project_root: &'a Path,
    pub(in crate::tui) active_session_id: SharedActiveSessionId,
}

pub(in crate::tui) struct PersistenceCommandState<'a> {
    pub(in crate::tui) current_session_id: &'a mut SessionId,
    pub(in crate::tui) signatures: &'a mut PersistedSnapshotSignatures,
}

pub(in crate::tui::run) fn persistence_command(input: &str) -> Option<PersistenceCommand<'_>> {
    let trimmed = input.trim();
    let (command, arg) = trimmed
        .split_once(char::is_whitespace)
        .map(|(command, arg)| (command, arg.trim()))
        .unwrap_or((trimmed, ""));
    match command {
        "/save" => Some(PersistenceCommand::SavePlan),
        "/discard" => Some(PersistenceCommand::DiscardPlan),
        "/new-plan" => Some(PersistenceCommand::NewPlan),
        "/export" => Some(PersistenceCommand::ExportPlan(arg)),
        "/plans" => Some(PersistenceCommand::Plans),
        "/sessions" => Some(PersistenceCommand::Sessions),
        "/search" => Some(PersistenceCommand::Search(arg)),
        "/forget" => Some(PersistenceCommand::Forget(arg)),
        "/resume" => Some(PersistenceCommand::Resume(arg)),
        _ => None,
    }
}

pub(in crate::tui::run) async fn apply_persistence_command(
    command: PersistenceCommand<'_>,
    app: &mut AppState,
    deps: PersistenceCommandDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
) -> Result<()> {
    match command {
        PersistenceCommand::SavePlan => {
            save_current_plan(app, deps, state).await?;
        }
        PersistenceCommand::DiscardPlan => {
            discard_current_plan(app, deps).await?;
        }
        PersistenceCommand::NewPlan => {
            new_plan(app, &deps.plan_store).await;
        }
        PersistenceCommand::ExportPlan(path) => {
            let workspace_root = if app.project_root.as_os_str().is_empty() {
                deps.project_root.to_path_buf()
            } else {
                app.project_root.clone()
            };
            export_current_plan(app, &workspace_root, path).await?;
        }
        PersistenceCommand::Plans => {
            let plans = deps
                .storage
                .saved_plans_for_project(deps.project_root, 100)
                .await?;
            app.reduce(AppAction::OpenModal(ModalKind::Picker(
                crate::tui::event::PickerModal::PlanPicker {
                    plans,
                    query: String::new(),
                    cursor: 0,
                },
            )));
        }
        PersistenceCommand::Sessions => {
            let sessions = deps
                .storage
                .recent_sessions_for_project(deps.project_root, 20)
                .await?
                .into_iter()
                .filter(|session| session.id != *state.current_session_id)
                .collect::<Vec<_>>();
            app.reduce(AppAction::OpenModal(ModalKind::Picker(
                crate::tui::event::PickerModal::SessionPicker {
                    sessions,
                    cursor: 0,
                },
            )));
        }
        PersistenceCommand::Search(query) => {
            if query.trim().is_empty() {
                anyhow::bail!("Usage: /search <query>");
            }
            let hits = deps.storage.search_messages(query, 20).await?;
            push_command_message(app, CommandOutputKind::Status, &format_search_hits(&hits));
        }
        PersistenceCommand::Forget(value) => {
            let session_id = parse_session_id(value, "/forget <id>")?;
            if session_id == *state.current_session_id {
                anyhow::bail!(
                    "Cannot forget the active session. Run /new, then /forget {session_id}."
                );
            }
            match deps
                .storage
                .forget_session(deps.project_root, session_id)
                .await?
            {
                ForgetSessionOutcome::Forgotten => {
                    push_command_message(
                        app,
                        CommandOutputKind::Status,
                        &format!("Forgot session #{session_id}."),
                    );
                    refresh_session_completion_choices(
                        app,
                        deps.storage,
                        deps.project_root,
                        *state.current_session_id,
                    )
                    .await;
                }
                ForgetSessionOutcome::NotFound => push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!("No session #{session_id}."),
                ),
                ForgetSessionOutcome::DifferentProject => push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!(
                        "Session #{session_id} belongs to a different project; forget it from there."
                    ),
                ),
                ForgetSessionOutcome::Live => push_command_message(
                    app,
                    CommandOutputKind::Error,
                    &format!(
                        "Session #{session_id} is live in another bonsai process and cannot be forgotten."
                    ),
                ),
            }
        }
        PersistenceCommand::Resume(value) => {
            if value.trim().is_empty() {
                resume_latest_prior_session(
                    app,
                    deps,
                    state,
                    "No prior sessions for this project.",
                )
                .await?;
            } else {
                let session_id = parse_session_id(value, "/resume <id>")?;
                resume_session(session_id, app, deps, state).await?;
            }
        }
    }
    Ok(())
}

async fn export_current_plan(
    app: &mut AppState,
    project_root: &Path,
    raw_path: &str,
) -> Result<()> {
    if !matches!(app.view, View::Plan) {
        anyhow::bail!("Switch to Plan view before exporting a plan.");
    }
    if app.plan.is_empty() {
        anyhow::bail!("Cannot export an empty plan. Add a title, section, or task first.");
    }

    let relative_path = validate_export_path(raw_path)?;
    let display_path = relative_path.display().to_string();
    let canonical_root = project_root
        .canonicalize()
        .with_context(|| format!("Failed to canonicalize workdir: {}", project_root.display()))?;
    let path = project_root.join(&relative_path);
    if path.exists() {
        anyhow::bail!("Export target already exists: {display_path}");
    }
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("Failed to create export directory: {}", parent.display()))?;
        let canonical_parent = parent.canonicalize().with_context(|| {
            format!(
                "Failed to canonicalize export directory: {}",
                parent.display()
            )
        })?;
        if !canonical_parent.starts_with(&canonical_root) {
            anyhow::bail!("Export path must stay inside the workdir.");
        }
    }

    let mut markdown = app.plan.to_markdown();
    markdown.push('\n');
    tokio::fs::write(&path, markdown)
        .await
        .with_context(|| format!("Failed to export plan to {display_path}"))?;
    push_command_message(
        app,
        CommandOutputKind::Status,
        &format!("Exported plan to {display_path}."),
    );
    Ok(())
}

fn validate_export_path(raw_path: &str) -> Result<std::path::PathBuf> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Usage: /export <path>");
    }

    let path = std::path::Path::new(trimmed);
    if path.is_absolute() {
        anyhow::bail!("Export path must be relative to the workdir.");
    }

    let mut clean = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => clean.push(part),
            std::path::Component::CurDir => {}
            _ => anyhow::bail!("Export path must stay inside the workdir."),
        }
    }

    if clean.as_os_str().is_empty() {
        anyhow::bail!("Usage: /export <path>");
    }
    if trimmed.ends_with(std::path::MAIN_SEPARATOR) || trimmed.ends_with('/') {
        anyhow::bail!("Export path must name a file.");
    }
    Ok(clean)
}

async fn save_current_plan(
    app: &mut AppState,
    deps: PersistenceCommandDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
) -> Result<()> {
    validate_plan_for_save(&app.plan)?;
    let saved = deps
        .storage
        .save_plan_to_library(
            *state.current_session_id,
            app.active_saved_plan_session_id,
            &app.plan,
            app.branch.as_deref(),
        )
        .await?;
    app.reduce(AppAction::SetActiveSavedPlan(Some(saved.id)));
    push_command_message(
        app,
        CommandOutputKind::Status,
        &format!("Saved plan #{}: {}.", saved.id, saved.title),
    );
    Ok(())
}

fn validate_plan_for_save(plan: &crate::plan::PlanDoc) -> Result<()> {
    if plan.title.trim().is_empty() {
        anyhow::bail!("Cannot save an untitled plan. Add a plan title first.");
    }
    if plan.sections.is_empty() && plan.tasks.is_empty() {
        anyhow::bail!("Cannot save an empty plan. Add at least one section or task first.");
    }
    Ok(())
}

/// `/discard` — throw away the canvas plan. An unsaved canvas is cleared
/// immediately (nothing was persisted); when the plan is linked to a saved
/// library record, a confirm modal is opened first because deleting that record
/// is irreversible. Refused mid-run so it can't delete the saved record a
/// running implementation is linked to (`mark_started_saved_plan`).
async fn discard_current_plan(app: &mut AppState, deps: PersistenceCommandDeps<'_>) -> Result<()> {
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Status,
            "Finish or cancel the current run before discarding the plan.",
        );
        return Ok(());
    }
    if app.plan.is_empty() {
        push_command_message(
            app,
            CommandOutputKind::Status,
            "No plan on the canvas to discard.",
        );
        return Ok(());
    }
    if let Some(saved_plan_id) = app.active_saved_plan_session_id {
        app.reduce(AppAction::OpenModal(ModalKind::Confirm(
            crate::tui::event::ConfirmModal::PlanDiscard {
                saved_plan_id,
                title: app.plan.title.clone(),
            },
        )));
        return Ok(());
    }
    clear_canvas_plan(app, &deps.plan_store).await;
    push_command_message(app, CommandOutputKind::Status, "Plan discarded.");
    Ok(())
}

/// `/new-plan` — reset the plan canvas to a fresh, empty `PlanDoc` while
/// keeping the conversation transcript and context window intact (unlike
/// `/new`, which rotates the whole session). Always clears the canvas and
/// unbinds any saved-plan association; never deletes the saved library record,
/// so a linked plan can be re-opened from `/plans` later. Refused mid-run so a
/// live implementation can't lose its canvas out from under it.
async fn new_plan(app: &mut AppState, plan_store: &SharedPlanStore) {
    if app.task_state.is_busy() {
        push_command_message(
            app,
            CommandOutputKind::Status,
            "Finish or cancel the current run before starting a new plan.",
        );
        return;
    }
    clear_canvas_plan(app, plan_store).await;
    push_command_message(
        app,
        CommandOutputKind::Status,
        "Plan canvas cleared. Chat kept.",
    );
}

/// Clear the working plan from the canvas: empty the shared plan store, mirror it
/// into `app.plan`, and drop any saved-plan association. Mirrors the `/new` flow's
/// clear; the periodic `persist_changed_app_snapshots` re-persists the emptied
/// plan via its signature diff, so no manual signature reset is needed here.
pub(in crate::tui) async fn clear_canvas_plan(app: &mut AppState, plan_store: &SharedPlanStore) {
    {
        let mut plan = plan_store.lock().await;
        plan.edit().clear();
        app.plan = plan.clone();
    }
    app.reduce(AppAction::SetActiveSavedPlan(None));
}

pub(in crate::tui) async fn open_saved_plan(
    plan: crate::storage::SavedPlanSummary,
    mode: PlanOpenMode,
    app: &mut AppState,
    deps: PersistenceCommandDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
) -> Result<()> {
    match mode {
        PlanOpenMode::CleanContext => open_saved_plan_clean(plan.id, app, deps, state).await,
        PlanOpenMode::ResumeSourceSession => {
            let plan_id = plan.id;
            // The saved snapshot outlives its source session; without one the
            // clean-context open is the only meaningful way in.
            let Some(source_session_id) = plan.source_session_id else {
                push_command_message(
                    app,
                    CommandOutputKind::Status,
                    "The plan's source session no longer exists; opening with a clean context.",
                );
                return open_saved_plan_clean(plan_id, app, deps, state).await;
            };
            let storage = deps.storage;
            let agent = deps.agent.clone();
            resume_session(source_session_id, app, deps, state).await?;
            if app.current_session_id == Some(source_session_id) {
                storage
                    .set_session_source_plan(source_session_id, Some(plan_id))
                    .await?;
                app.reduce(AppAction::SetActiveSavedPlan(Some(plan_id)));
            }
            app.reduce(AppAction::SetView(View::Plan));
            {
                let mut agent = agent.lock().await;
                agent.set_mode(AgentMode::Planning);
                app.latest_context_report = Some(agent.context_report());
            }
            Ok(())
        }
    }
}

pub(in crate::tui::run) async fn open_saved_plan_clean(
    plan_id: SavedPlanId,
    app: &mut AppState,
    deps: PersistenceCommandDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
) -> Result<()> {
    let Some(snapshot) = deps.storage.load_saved_plan(plan_id).await? else {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("No saved plan #{plan_id}."),
        );
        return Ok(());
    };
    // Flush the outgoing session before rotating into the saved-plan session, so
    // its last unsaved transcript/plan/todo/context edits aren't stranded when
    // the signatures reset below. Signature-gated; near-no-op when clean.
    persist_changed_snapshots(
        deps.storage,
        *state.current_session_id,
        app,
        deps.agent.clone(),
        state.signatures,
    )
    .await;
    let next_id = rotate_persisted_session(
        deps.storage,
        *state.current_session_id,
        deps.project_root,
        app,
    )
    .await?;
    let conversation_cache_key = deps.storage.conversation_cache_key(next_id).await?;

    *state.current_session_id = next_id;
    crate::storage::activate_session_heartbeat(deps.storage, &deps.active_session_id, next_id)
        .await?;
    deps.storage
        .attach_recovery_workspace_session(&app.project_root, next_id)
        .await?;
    set_session_identity_or_default(app, deps.storage, next_id, deps.project_root).await;

    deps.storage
        .set_session_source_plan(next_id, Some(snapshot.summary.id))
        .await?;

    {
        let mut agent = deps.agent.lock().await;
        agent.set_conversation_cache_key(&conversation_cache_key);
        agent.clear().await;
        agent.set_mode(AgentMode::Planning);
        app.latest_context_report = Some(agent.context_report());
    }
    deps.todo_store.lock().await.clear();
    *deps.plan_store.lock().await = snapshot.plan.clone();

    app.transcript.clear();
    app.queued_inputs.clear();
    app.todo.clear();
    app.plan = snapshot.plan;
    app.task_state = TaskState::Idle;
    app.view = View::Plan;
    app.focus = Focus::Input;
    app.modal = None;
    app.modal_return_focus = None;
    app.transcript_selection = None;
    app.transcript_focus = None;
    app.active_group_tool_selection = None;
    app.expanded_execution_groups.clear();
    app.active_execution_group_id = None;
    app.reset_next_execution_group_id();
    app.active_tools.clear();
    app.current_phase = None;
    app.clear_run_timer();
    app.transcript_autoscroll = true;
    app.scroll_transcript_focus_into_view = false;
    app.plan_scroll = 0;
    app.reduce(AppAction::SetActiveSavedPlan(Some(snapshot.summary.id)));
    refresh_session_completion_choices(app, deps.storage, deps.project_root, next_id).await;

    state.signatures.reset();
    Ok(())
}

pub(in crate::tui) async fn delete_saved_plan_from_picker(
    app: &mut AppState,
    storage: &Storage,
    project_root: &Path,
    plan: crate::storage::SavedPlanSummary,
) -> Result<()> {
    let deleted = storage.delete_saved_plan(plan.id).await?;
    if app.active_saved_plan_session_id == Some(plan.id) {
        app.reduce(AppAction::SetActiveSavedPlan(None));
    }
    let plans = storage.saved_plans_for_project(project_root, 100).await?;
    app.reduce(AppAction::OpenModal(ModalKind::Picker(
        crate::tui::event::PickerModal::PlanPicker {
            plans,
            query: String::new(),
            cursor: 0,
        },
    )));
    if deleted {
        push_command_message(
            app,
            CommandOutputKind::Status,
            &format!("Deleted saved plan #{}.", plan.id),
        );
    } else {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("No saved plan #{}.", plan.id),
        );
    }
    Ok(())
}

pub(in crate::tui::run) async fn resume_latest_prior_session(
    app: &mut AppState,
    deps: PersistenceCommandDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
    empty_message: &str,
) -> Result<()> {
    let Some(summary) = deps
        .storage
        .latest_prior_session_for_project(deps.project_root, *state.current_session_id)
        .await?
    else {
        app.latest_context_report = Some(deps.agent.lock().await.context_report());
        push_command_message(app, CommandOutputKind::Status, empty_message);
        return Ok(());
    };
    resume_session(summary.id, app, deps, state).await
}

struct PreparedResume {
    snapshot: crate::storage::SessionSnapshot,
    conversation_cache_key: String,
    session_store: SessionStore,
    provider: Box<dyn crate::provider::Provider>,
    context_window: usize,
    prompt_estimator: crate::provider::PromptEstimator,
    model_identity: crate::agent::ActiveModelIdentity,
    project_info_provider: crate::tool::ProjectInfoProviderState,
    transcript: TranscriptModel,
    lost_terminal_ids: Vec<String>,
}

pub(in crate::tui) async fn resume_session(
    session_id: SessionId,
    app: &mut AppState,
    deps: PersistenceCommandDeps<'_>,
    state: &mut PersistenceCommandState<'_>,
) -> Result<()> {
    if session_id == *state.current_session_id {
        push_command_message(
            app,
            CommandOutputKind::Status,
            &format!("Session #{session_id} is already active."),
        );
        return Ok(());
    }
    let outgoing_status = terminal_session_status(&app.current_session_status);
    let Some(snapshot) = deps
        .storage
        .load_runtime_session_snapshot(session_id)
        .await?
    else {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("No session #{session_id}."),
        );
        return Ok(());
    };
    // Resume is scoped to the current project. A session id from another project
    // must not be loaded into this working tree: it would run the wrong
    // conversation against the wrong files and the wrong permission rules, and
    // would flip that project's session to active. (The `-c` latest path is
    // already project-scoped in SQL; this guards the explicit `-c <id>` path.)
    let current_project = crate::storage::canonical_project_path(deps.project_root);
    if snapshot.summary.project_path != current_project {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!(
                "Session #{session_id} belongs to a different project ({}); resume it from there.",
                snapshot.summary.project_path
            ),
        );
        return Ok(());
    }
    if deps.storage.is_session_live(session_id).await? {
        push_command_message(
            app,
            CommandOutputKind::Error,
            &format!("Session #{session_id} is live in another bonsai process."),
        );
        return Ok(());
    }

    let (session_snapshot, provider, context_window, prompt_estimator) = {
        let mut session = deps.session_store.lock().await.clone();
        let summary = &snapshot.summary;
        let factory = deps
            .registry
            .lookup(&summary.provider_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown provider '{}'", summary.provider_id))?;
        let metadata = factory.metadata();
        session.ensure_provider(&summary.provider_id);
        if !factory.is_authorized(session.session(&summary.provider_id)) {
            push_command_message(
                app,
                CommandOutputKind::Error,
                &format!(
                    "Authorize {} before resuming session #{}. Try `/authorize {}`.",
                    metadata.display_name, summary.id, summary.provider_id
                ),
            );
            return Ok(());
        }
        let resolved = resolved_model_for_provider_model(
            Some(&deps.model_catalog),
            &summary.provider_id,
            &summary.model,
        );
        session.set_active_model_target(
            &summary.provider_id,
            resolved.as_ref().map(|model| model.connection_id.clone()),
            resolved.as_ref().map(|model| model.model_id.clone()),
            &summary.model,
        );
        let current_model = session.session(&summary.provider_id).model.clone();
        session
            .session_mut(&summary.provider_id)
            .set_model_reasoning(metadata, &current_model, summary.reasoning);
        let provider = build_provider(&deps.registry, &session, Some(&deps.model_catalog));
        let context_window = context_window_for_current_model_with_catalog(
            &deps.registry,
            &session,
            Some(&deps.model_catalog),
        ) as usize;
        let prompt_estimator = prompt_estimator_for_current_model_with_catalog(
            &deps.registry,
            &session,
            Some(&deps.model_catalog),
        );
        (session, provider, context_window, prompt_estimator)
    };
    let mut transcript_items = snapshot
        .transcript
        .iter()
        .filter(|item| !item.is_suppressed())
        .cloned()
        .collect::<Vec<_>>();
    let lost_terminal_ids = normalize_lost_interactive_terminals(&mut transcript_items);
    let prepared = PreparedResume {
        conversation_cache_key: deps.storage.conversation_cache_key(session_id).await?,
        model_identity: crate::model_resolution::active_model_identity(&session_snapshot),
        project_info_provider: crate::model_resolution::project_info_provider_state(
            &deps.registry,
            &session_snapshot,
        ),
        transcript: TranscriptModel::from(transcript_items),
        lost_terminal_ids,
        snapshot,
        session_store: session_snapshot,
        provider,
        context_window,
        prompt_estimator,
    };
    // Flush the outgoing session only after the complete target state has been
    // prepared. Invalid, unauthorized, or live resume requests must not cause
    // unrelated persistence side effects.
    persist_changed_snapshots(
        deps.storage,
        *state.current_session_id,
        app,
        deps.agent.clone(),
        state.signatures,
    )
    .await;
    match deps
        .storage
        .switch_active_session(
            *state.current_session_id,
            session_id,
            deps.project_root,
            outgoing_status,
        )
        .await?
    {
        ResumeSessionOutcome::Resumed => {}
        ResumeSessionOutcome::NotFound => {
            push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("No session #{session_id}."),
            );
            return Ok(());
        }
        ResumeSessionOutcome::DifferentProject => {
            push_command_message(
                app,
                CommandOutputKind::Error,
                &format!(
                    "Session #{session_id} belongs to a different project; resume it from there."
                ),
            );
            return Ok(());
        }
        ResumeSessionOutcome::Live => {
            push_command_message(
                app,
                CommandOutputKind::Error,
                &format!("Session #{session_id} is live in another bonsai process."),
            );
            return Ok(());
        }
    }
    let PreparedResume {
        snapshot,
        conversation_cache_key,
        session_store,
        provider,
        context_window,
        prompt_estimator,
        model_identity,
        project_info_provider,
        transcript,
        lost_terminal_ids,
    } = prepared;
    let summary = snapshot.summary.clone();
    let plan = snapshot.plan.clone();
    let todos = snapshot.todos.clone();

    // The durable switch above is the final fallible operation. Everything
    // below is an in-memory commit of the already prepared target state.
    *deps.session_store.lock().await = session_store;
    app.set_session_identity(summary.id, summary.name, summary.summary);

    {
        let mut agent = deps.agent.lock().await;
        agent.set_provider(provider, context_window, prompt_estimator, model_identity);
        agent.set_conversation_cache_key(&conversation_cache_key);
        agent.set_project_info_provider(project_info_provider);
        agent.set_execution_lane(crate::agent::ExecutionLane::parent(session_id.to_string()));
        crate::session_persist::restore_agent_state(&mut agent, &snapshot).await;
        if !lost_terminal_ids.is_empty() {
            agent.push_user_context_note(&crate::tool::wrap_untrusted_content(
                "terminal:resume",
                &format!(
                    "Interactive terminal{} {} {} lost because PTY processes are process-local and cannot survive a Bonsai restart. Start the command again if it is still needed.",
                    if lost_terminal_ids.len() == 1 { "" } else { "s" },
                    lost_terminal_ids.join(", "),
                    if lost_terminal_ids.len() == 1 { "was" } else { "were" }
                ),
            ));
        }
        app.latest_context_report = Some(agent.context_report());
        app.refresh_agent_mirrors(&agent);
    }

    *deps.plan_store.lock().await = plan.clone();
    deps.todo_store.lock().await.set_todos(todos.clone());

    app.transcript = transcript;
    app.reset_next_execution_group_id();
    app.plan = plan;
    app.todo = todos;
    app.provider = summary.provider_id;
    app.model = summary.model;
    app.reasoning = summary.reasoning;
    app.active_saved_plan_session_id = summary.source_plan_id;
    app.task_state = TaskState::Idle;
    app.focus = Focus::Input;
    app.modal = None;
    app.modal_return_focus = None;
    app.transcript_selection = None;
    app.transcript_focus = None;
    app.active_group_tool_selection = None;
    app.expanded_execution_groups.clear();
    app.active_execution_group_id = None;
    app.active_tools.clear();
    app.current_phase = None;
    app.clear_run_timer();
    app.transcript_autoscroll = true;
    app.scroll_transcript_focus_into_view = false;
    app.reduce(AppAction::ScrollBottom);
    // The resume confirmation is a transient toast, not a transcript row: a
    // resumed session must not accumulate a "Resumed session #N" line every time
    // it is reopened. Acting on the interrupted hint also retires it.
    app.session_hint = None;
    app.set_session_toast(format!("Resumed session #{session_id}."));

    *state.current_session_id = session_id;
    // switch_active_session already stamped the target heartbeat and attached
    // any active recovery workspace inside the same SQLite transaction.
    *deps.active_session_id.lock().await = Some(session_id);
    // Correlates this process's log file (keyed by pid + start-millis) with
    // the DB session id, so `bonsai.db` forensics and log tailing meet.
    tracing::info!(
        target: "bonsai::session",
        session_id = %session_id,
        "session attached to this process log"
    );
    refresh_session_completion_choices(app, deps.storage, deps.project_root, session_id).await;
    state.signatures.reset();
    Ok(())
}

pub(super) fn normalize_lost_interactive_terminals(items: &mut [TranscriptItem]) -> Vec<String> {
    let mut lost = Vec::new();
    for item in items {
        match item {
            TranscriptItem::ToolActivity(activity) => {
                if let Some(id) = normalize_lost_terminal(activity) {
                    lost.push(id);
                }
            }
            TranscriptItem::ExecutionGroup(group) => {
                for activity in &mut group.tools {
                    if let Some(id) = normalize_lost_terminal(activity) {
                        lost.push(id);
                    }
                }
                if group
                    .tools
                    .iter()
                    .all(|activity| !matches!(activity.status, ToolStatus::Running))
                {
                    group.finished_at.get_or_insert_with(Instant::now);
                }
            }
            _ => {}
        }
    }
    lost
}

fn normalize_lost_terminal(activity: &mut ToolActivity) -> Option<String> {
    if !matches!(activity.status, ToolStatus::Running)
        || activity.name != "bash"
        || !serde_json::from_str::<serde_json::Value>(&activity.arguments)
            .ok()
            .and_then(|args| args.get("interactive").and_then(serde_json::Value::as_bool))
            .unwrap_or(false)
    {
        return None;
    }
    let terminal_id = activity
        .result
        .as_deref()
        .and_then(|result| {
            result
                .split_whitespace()
                .find(|word| word.starts_with("pty-"))
        })
        .map(|word| word.trim_end_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-'))
        .unwrap_or("interactive terminal")
        .to_string();
    activity.status = ToolStatus::Failed;
    activity.finished_at = Some(Instant::now());
    activity.result = Some(
        "Interactive terminal lost: PTY processes are process-local and cannot be reattached after Bonsai restarts. Start the command again if it is still needed."
            .to_string(),
    );
    Some(terminal_id)
}

pub(in crate::tui::run) async fn rotate_persisted_session(
    storage: &Storage,
    current_session_id: SessionId,
    project_root: &std::path::Path,
    app: &AppState,
) -> Result<SessionId> {
    storage
        .mark_session_termination(
            current_session_id,
            terminal_session_status(&app.current_session_status),
            app.current_terminal_reason,
        )
        .await?;
    storage
        .start_session(project_root, &app.provider, &app.model, app.reasoning)
        .await
}

pub(in crate::tui::run) fn terminal_session_status(status: &SessionStatus) -> SessionStatus {
    match status {
        SessionStatus::Active => SessionStatus::Completed,
        status => status.clone(),
    }
}

pub(in crate::tui::run) async fn refresh_session_completion_choices(
    app: &mut AppState,
    storage: &Storage,
    project_root: &Path,
    current_session_id: SessionId,
) {
    match storage.recent_sessions_for_project(project_root, 20).await {
        Ok(sessions) => {
            app.session_choices = sessions
                .into_iter()
                .filter(|session| session.id != current_session_id)
                .collect();
            app.recompute_completion();
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to refresh session completions");
        }
    }
}

pub(in crate::tui::run) async fn set_session_identity_or_default(
    app: &mut AppState,
    storage: &Storage,
    session_id: SessionId,
    project_root: &Path,
) {
    match storage.session_summary(session_id).await {
        Ok(Some(summary)) => {
            app.set_session_identity(session_id, summary.name, summary.summary);
        }
        Ok(None) => {
            app.set_session_identity(
                session_id,
                project_session_name(project_root),
                String::new(),
            );
        }
        Err(err) => {
            tracing::warn!(
                session_id = %session_id,
                error = %err,
                "failed to load session summary for identity"
            );
            app.set_session_identity(
                session_id,
                project_session_name(project_root),
                String::new(),
            );
        }
    }
}

fn project_session_name(project_root: &Path) -> String {
    project_root
        .file_name()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| project_root.to_string_lossy().to_string())
}

fn parse_session_id(value: &str, usage: &str) -> Result<SessionId> {
    let id = value
        .trim()
        .parse::<i64>()
        .with_context(|| usage.to_string())?;
    if id <= 0 {
        anyhow::bail!("{usage}");
    }
    Ok(SessionId::from_raw(id))
}

pub(in crate::tui::run) fn format_interrupted_sessions(
    sessions: &[crate::storage::SessionSummary],
) -> String {
    // Single line: this renders in the composer meta row / header pill, where a
    // newline would split the banner.
    if sessions.len() == 1 {
        "Interrupted session found. Use /resume to bring it back.".to_string()
    } else {
        "Interrupted sessions found. Use /resume to bring back the latest one.".to_string()
    }
}

fn format_search_hits(hits: &[crate::storage::SearchHit]) -> String {
    if hits.is_empty() {
        return "No matching messages.".to_string();
    }
    let mut text = String::from("Search results:\n");
    for hit in hits {
        text.push_str(&format!(
            "  #{:<4} {:<9} {}  {}\n",
            hit.session_id, hit.role, hit.project_path, hit.content
        ));
    }
    text.trim_end().to_string()
}

pub(in crate::tui::run) fn cached_model_signature(
    session: &SessionStore,
    catalog: &crate::model_catalog::ModelCatalog,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    catalog.live_availability_snapshot().hash(&mut hasher);
    catalog.models_dev_revision().hash(&mut hasher);
    session.providers.len().hash(&mut hasher);
    let mut provider_ids = session.providers.keys().collect::<Vec<_>>();
    provider_ids.sort();
    for provider_id in provider_ids {
        provider_id.hash(&mut hasher);
        if let Some(provider) = session.providers.get(provider_id) {
            provider.api_key.trim().is_empty().hash(&mut hasher);
            provider.account_id.trim().is_empty().hash(&mut hasher);
            provider.model.hash(&mut hasher);
            provider.reasoning.hash(&mut hasher);
            let mut model_reasoning = provider.model_reasoning.iter().collect::<Vec<_>>();
            model_reasoning.sort_by_key(|(model, _)| *model);
            for (model, reasoning) in model_reasoning {
                model.hash(&mut hasher);
                reasoning.hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

pub(in crate::tui::run) fn transcript_signature(transcript: &[TranscriptItem]) -> u64 {
    crate::session_persist::transcript_signature(transcript)
}

pub(in crate::tui::run) fn plan_signature(plan: &crate::plan::PlanDoc) -> u64 {
    crate::session_persist::plan_signature(plan)
}

pub(in crate::tui::run) fn todo_signature(todos: &[crate::todo::TodoItem]) -> u64 {
    crate::session_persist::todo_signature(todos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_cache_signature_changes_after_models_dev_refresh() {
        let catalog = crate::model_catalog::ModelCatalog::load_builtin().unwrap();
        let session = SessionStore::default();
        let before = cached_model_signature(&session, &catalog);

        catalog.replace_models_dev_metadata(Default::default());

        assert_ne!(cached_model_signature(&session, &catalog), before);
    }
}
